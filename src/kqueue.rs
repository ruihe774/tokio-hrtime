use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

use crate::utils::{cvt, instant_to_duration};

cfg_if::cfg_if! {
    if #[cfg(target_vendor = "apple")] {
        #[expect(deprecated)]
        fn add_timer_to_kqueue(kq: libc::c_int, id: usize, duration: i64, oneshot: bool) {
            let mut ti = std::mem::MaybeUninit::<libc::mach_timebase_info>::uninit();
            assert_eq!(unsafe { libc::mach_timebase_info(ti.as_mut_ptr()) }, 0);
            let ti = unsafe { ti.assume_init() };

            let changelist = [libc::kevent {
                ident: id,
                filter: libc::EVFILT_TIMER,
                flags: libc::EV_DELETE,
                fflags: 0,
                data: 0,
                udata: ptr::null_mut(),
            }];
            let _ = unsafe { libc::kevent(kq, changelist.as_ptr(), 1, ptr::null_mut(), 0, ptr::null()) };

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

fn wait_kqueue(kq: libc::c_int) -> Option<isize> {
    let mut eventlist = [libc::kevent {
        ident: 0,
        filter: 0,
        flags: 0,
        fflags: 0,
        data: 0,
        udata: ptr::null_mut(),
    }];
    let immediate = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    (cvt(unsafe {
        libc::kevent(
            kq,
            ptr::null(),
            0,
            eventlist.as_mut_ptr(),
            1,
            &raw const immediate,
        )
    })
    .expect("failed to wait on kqueue")
        != 0)
        .then_some(eventlist[0].data)
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

pub struct Timer {
    kq: AsyncFd<File>,
    interval: Option<Duration>,
}

impl Timer {
    pub fn new(deadline: Option<Instant>, mut interval: Option<Duration>) -> Timer {
        let kq = create_kqueue();
        if let Some(deadline) = deadline {
            add_timer_to_kqueue(kq.as_raw_fd(), 1, instant_to_nseconds(deadline), true);
        } else if let Some(interval) = interval.take() {
            add_timer_to_kqueue(kq.as_raw_fd(), 1, duration_to_nseconds(interval), false);
        }
        Timer { kq, interval }
    }

    pub fn reset(&mut self, deadline: Option<Instant>, mut interval: Option<Duration>) {
        let kq = &self.kq;
        if let Some(deadline) = deadline {
            add_timer_to_kqueue(kq.as_raw_fd(), 1, instant_to_nseconds(deadline), true);
        } else if let Some(interval) = interval.take() {
            add_timer_to_kqueue(kq.as_raw_fd(), 1, duration_to_nseconds(interval), false);
        }
        self.interval = interval;
    }

    pub fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<u64> {
        match self.kq.poll_read_ready(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(r) => {
                let mut guard = r.expect("failed to poll kqueue");
                guard.clear_ready();
                if let Some(expirations) = wait_kqueue(guard.get_inner().as_raw_fd()) {
                    if let Some(interval) = self.interval.take() {
                        add_timer_to_kqueue(
                            self.kq.as_raw_fd(),
                            1,
                            duration_to_nseconds(interval),
                            false,
                        );
                    }
                    Poll::Ready(expirations as u64)
                } else {
                    Poll::Pending
                }
            }
        }
    }
}
