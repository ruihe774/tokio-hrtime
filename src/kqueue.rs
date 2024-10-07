use std::collections::BTreeMap;
use std::fs::File;
use std::mem;
use std::ops::DerefMut;
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

use crate::utils::{cvt, instant_to_duration};

cfg_if::cfg_if! {
    if #[cfg(target_vendor = "apple")] {
        #[allow(deprecated)]
        fn add_timer_to_kqueue(kq: libc::c_int, id: usize, duration: i64, oneshot: bool) {
            let mut ti = mem::MaybeUninit::<libc::mach_timebase_info>::uninit();
            assert_eq!(unsafe { libc::mach_timebase_info(ti.as_mut_ptr()) }, 0);
            let ti = unsafe { ti.assume_init() };

            delete_timer_from_kqueue(kq, id);

            let changelist = [libc::kevent {
                ident: id,
                filter: libc::EVFILT_TIMER,
                flags: libc::EV_ADD,
                fflags: libc::NOTE_MACHTIME | libc::NOTE_CRITICAL | if oneshot { libc::NOTE_ABSOLUTE } else { 0 },
                data: (duration.checked_mul(ti.denom as i64).unwrap() / (ti.numer as i64)).try_into().unwrap(),
                udata: ptr::null_mut(),
            }];
            cvt(unsafe { libc::kevent(kq, changelist.as_ptr(), 1, ptr::null_mut(), 0, ptr::null()) })
                .expect("failed to add timer to kqueue");
        }
    } else {
        fn add_timer_to_kqueue(kq: libc::c_int, id: usize, duration: i64, oneshot: bool) {
            let changelist = [libc::kevent {
                ident: id,
                filter: libc::EVFILT_TIMER,
                flags: libc::EV_ADD | if oneshot { libc::EV_ONESHOT } else { 0 },
                fflags: libc::NOTE_NSECONDS,
                data: (if oneshot { duration - instant_to_nseconds(Instant::now()) } else { duration }).try_into().unwrap(),
                udata: ptr::null_mut(),
            }];
            cvt(unsafe { libc::kevent(kq, changelist.as_ptr(), 1, ptr::null_mut(), 0, ptr::null()) })
                .expect("failed to add timer to kqueue");
        }
    }
}

fn delete_timer_from_kqueue(kq: libc::c_int, id: usize) {
    let changelist = [libc::kevent {
        ident: id,
        filter: libc::EVFILT_TIMER,
        flags: libc::EV_DELETE,
        fflags: 0,
        data: 0,
        udata: ptr::null_mut(),
    }];
    let _ = unsafe { libc::kevent(kq, changelist.as_ptr(), 1, ptr::null_mut(), 0, ptr::null()) };
}

fn wait_kqueue(kq: libc::c_int) -> Vec<usize> {
    const NEVENTS: usize = 128;
    let mut eventlist = mem::MaybeUninit::<[libc::kevent; NEVENTS]>::uninit();
    let immediate = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let nsignals = cvt(unsafe {
        libc::kevent(
            kq,
            ptr::null(),
            0,
            eventlist.assume_init_mut().as_mut_ptr(),
            NEVENTS as libc::c_int,
            ptr::from_ref(&immediate),
        )
    })
    .expect("failed to wait on kqueue") as usize;
    let eventlist = &unsafe { eventlist.assume_init_ref() }[..nsignals];
    eventlist.iter().map(|ke| ke.ident).collect()
}

fn create_kqueue() -> AsyncFd<File> {
    let kq = cvt(unsafe { libc::kqueue() }).expect("failed to create kqueue");
    cvt(unsafe { libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC) }).unwrap();

    let file = unsafe { File::from_raw_fd(kq) };
    AsyncFd::with_interest(file, Interest::READABLE).unwrap()
}

fn duration_to_nseconds(duration: Duration) -> i64 {
    duration.as_nanos().try_into().unwrap()
}

fn instant_to_nseconds(instant: Instant) -> i64 {
    duration_to_nseconds(instant_to_duration(instant))
}

struct SharedState {
    kq: AsyncFd<File>,
    timers: BTreeMap<usize, (u64, Option<Waker>, Option<Duration>)>,
}

static SS: Mutex<Option<SharedState>> = Mutex::new(None);

pub struct Timer {
    ident: usize,
}

impl Timer {
    pub fn new(deadline: Instant, interval: Option<Duration>) -> Timer {
        let mut ss = SS.lock().unwrap();
        let ss = ss.get_or_insert_with(|| SharedState {
            kq: create_kqueue(),
            timers: BTreeMap::new(),
        });

        let mut ident = ss
            .timers
            .keys()
            .last()
            .copied()
            .unwrap_or_default()
            .wrapping_add(1);
        if ident == 0 {
            ident = ss
                .timers
                .keys()
                .zip(1usize..)
                .find_map(|(&ident, idx)| (ident != idx).then_some(idx))
                .expect("timer idents run out");
        }
        assert!(ss.timers.insert(ident, (0, None, interval)).is_none());

        add_timer_to_kqueue(
            ss.kq.as_raw_fd(),
            ident,
            instant_to_nseconds(deadline),
            true,
        );
        Timer { ident }
    }

    pub fn reset(&mut self, deadline: Instant, interval: Option<Duration>) {
        let mut ss = SS.lock().unwrap();
        let ss = ss.as_mut().unwrap();
        add_timer_to_kqueue(
            ss.kq.as_raw_fd(),
            self.ident,
            instant_to_nseconds(deadline),
            true,
        );
        ss.timers.get_mut(&self.ident).unwrap().2 = interval;
    }

    pub fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<u64> {
        let mut ss = SS.lock().unwrap();
        let ss = ss.as_mut().unwrap();

        let waker = &mut ss.timers.get_mut(&self.ident).unwrap().1;
        if let Some(waker) = waker {
            waker.clone_from(cx.waker());
        } else {
            *waker = Some(cx.waker().clone());
        }

        let waker = merge_wakers(
            ss.timers
                .values()
                .filter_map(|(_, waker, _)| waker.clone())
                .collect(),
        );
        let mut cx = Context::from_waker(&waker);

        if let Poll::Ready(Ok(mut guard)) = ss.kq.poll_read_ready(&mut cx) {
            guard.clear_ready();
            let events = wait_kqueue(guard.get_inner().as_raw_fd());
            for ident in events {
                let (expirations, _, interval) = ss.timers.get_mut(&ident).unwrap();
                *expirations += 1;
                if let Some(interval) = interval.take() {
                    add_timer_to_kqueue(
                        ss.kq.as_raw_fd(),
                        ident,
                        duration_to_nseconds(interval),
                        false,
                    );
                }
            }
        }

        let expirations = &mut ss.timers.get_mut(&self.ident).unwrap().0;

        if *expirations != 0 {
            Poll::Ready(mem::replace(expirations, 0))
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let Ok(mut ss) = SS.lock() else {
            debug_assert!(false);
            return;
        };
        let Some(ss) = ss.deref_mut() else {
            debug_assert!(false);
            return;
        };
        delete_timer_from_kqueue(ss.kq.as_raw_fd(), self.ident);
        debug_assert!(ss.timers.remove(&self.ident).is_some());
    }
}

mod waker_merger {
    use std::mem::forget;
    use std::ptr;
    use std::sync::Arc;
    use std::task::{RawWaker, RawWakerVTable, Waker};

    unsafe fn clone(data: *const ()) -> RawWaker {
        let data = data as *const Arc<Vec<Waker>>;
        let wakers = Box::new((*data).clone());
        let data = ptr::from_ref(wakers.as_ref());
        forget(wakers);
        RawWaker::new(data as _, &VTABLE)
    }

    unsafe fn wake(data: *const ()) {
        let data = data as *mut Arc<Vec<Waker>>;
        let wakers = Box::from_raw(data);
        match Arc::try_unwrap(*wakers) {
            Ok(wakers) => {
                for waker in wakers {
                    waker.wake();
                }
            }
            Err(wakers) => {
                for waker in wakers.iter() {
                    waker.wake_by_ref();
                }
            }
        }
    }

    unsafe fn wake_by_ref(data: *const ()) {
        let data = data as *const Arc<Vec<Waker>>;
        let wakers = &*data;
        for waker in wakers.iter() {
            waker.wake_by_ref();
        }
    }

    unsafe fn drop_waker(data: *const ()) {
        let data = data as *mut Arc<Vec<Waker>>;
        let _ = Box::from_raw(data);
    }

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

    pub fn merge_wakers(wakers: Vec<Waker>) -> Waker {
        let wakers = Box::new(Arc::new(wakers));
        let data = ptr::from_ref(wakers.as_ref());
        forget(wakers);
        unsafe { Waker::from_raw(RawWaker::new(data as _, &VTABLE)) }
    }
}

use waker_merger::merge_wakers;
