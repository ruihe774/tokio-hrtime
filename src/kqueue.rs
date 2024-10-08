use std::collections::BTreeMap;
use std::fs::File;
use std::future::Future;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd};
use std::pin::Pin;
use std::ptr;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::Semaphore;
use tokio::task::unconstrained;
use tokio_util::sync::PollSemaphore;

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
    timers: Mutex<BTreeMap<usize, (Arc<Semaphore>, Option<Duration>)>>,
}

static SS: Mutex<Weak<SharedState>> = Mutex::new(Weak::new());

pub struct Timer {
    ss: Arc<SharedState>,
    ident: usize,
    waiter: PollSemaphore,
}

impl Timer {
    pub fn new(deadline: Instant, interval: Option<Duration>) -> Timer {
        let mut ss = SS.lock().unwrap();

        let ss = match ss.upgrade() {
            Some(ss) => ss,
            None => {
                let new_ss = Arc::new(SharedState {
                    kq: create_kqueue(),
                    timers: Mutex::new(BTreeMap::new()),
                });
                *ss = Arc::downgrade(&new_ss);
                let _ = tokio::spawn(unconstrained(Background(Arc::downgrade(&new_ss))));
                new_ss
            }
        };

        let mut timers = ss.timers.lock().unwrap();

        let mut ident = timers
            .keys()
            .last()
            .copied()
            .unwrap_or_default()
            .wrapping_add(1);
        if ident == 0 {
            ident = timers
                .keys()
                .zip(1usize..)
                .find_map(|(&ident, idx)| (ident != idx).then_some(idx))
                .expect("timer idents run out");
        }

        let notify = Arc::new(Semaphore::new(0));
        let waiter = PollSemaphore::new(notify.clone());

        assert!(timers.insert(ident, (notify, interval)).is_none());

        add_timer_to_kqueue(
            ss.kq.as_raw_fd(),
            ident,
            instant_to_nseconds(deadline),
            true,
        );

        drop(timers);

        Timer { ss, ident, waiter }
    }

    pub fn reset(&mut self, deadline: Instant, interval: Option<Duration>) {
        add_timer_to_kqueue(
            self.ss.kq.as_raw_fd(),
            self.ident,
            instant_to_nseconds(deadline),
            true,
        );
        self.ss
            .timers
            .lock()
            .unwrap()
            .get_mut(&self.ident)
            .unwrap()
            .1 = interval;
    }

    pub fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<u64> {
        self.waiter
            .poll_acquire_many(
                cx,
                self.waiter.available_permits().clamp(1, u32::MAX as usize) as u32,
            )
            .map(|permits| {
                let permits = permits.unwrap();
                let expirations = permits.num_permits();
                permits.forget();
                expirations as u64
            })
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        delete_timer_from_kqueue(self.ss.kq.as_raw_fd(), self.ident);
        debug_assert!(self
            .ss
            .timers
            .lock()
            .ok()
            .map(|mut timer| timer.remove(&self.ident))
            .flatten()
            .is_some());
    }
}

struct Background(Weak<SharedState>);

impl Future for Background {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(ss) = self.0.upgrade() else {
            return Poll::Ready(());
        };

        while let Poll::Ready(Ok(mut guard)) = ss.kq.poll_read_ready(cx) {
            guard.clear_ready();
            let events = wait_kqueue(guard.get_inner().as_raw_fd());
            let mut timers = ss.timers.lock().unwrap();
            for ident in events {
                let (notify, interval) = timers.get_mut(&ident).unwrap();
                notify.add_permits(1);
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

        Poll::Pending
    }
}
