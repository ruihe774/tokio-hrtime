use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

use crate::utils::cvt;

fn add_timer_to_kqueue(kq: libc::c_int, id: usize, duration: isize, oneshot: bool) {
    let changelist = [libc::kevent {
        ident: id,
        filter: libc::EVFILT_TIMER,
        flags: libc::EV_ADD | if oneshot { libc::EV_ONESHOT } else { 0 },
        fflags: libc::NOTE_NSECONDS,
        data: duration,
        udata: ptr::null_mut(),
    }];
    cvt(unsafe { libc::kevent(kq, changelist.as_ptr(), 1, ptr::null_mut(), 0, ptr::null()) })
        .expect("failed to add timer to kqueue");
}

fn create_kqueue() -> AsyncFd<File> {
    let kq = cvt(unsafe { libc::kqueue() }).expect("failed to create kqueue");
    cvt(unsafe { libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC) }).unwrap();

    let file = unsafe { File::from_raw_fd(kq) };
    AsyncFd::with_interest(file, Interest::READABLE).unwrap()
}

fn wait_kqueue(kq: libc::c_int) -> bool {
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
    cvt(unsafe {
        libc::kevent(
            kq,
            ptr::null(),
            0,
            eventlist.as_mut_ptr(),
            1,
            ptr::from_ref(&immediate),
        )
    })
    .expect("failed to wait on kqueue")
        != 0
}

fn duration_to_nseconds(duration: Duration) -> isize {
    duration.as_nanos().try_into().unwrap()
}

pub struct Timer {
    kq: AsyncFd<File>,
    interval: Option<Duration>,
}

impl Timer {
    pub fn new(deadline: Instant, interval: Option<Duration>) -> Timer {
        let kq = create_kqueue();
        add_timer_to_kqueue(
            kq.as_raw_fd(),
            1,
            duration_to_nseconds(deadline - Instant::now()),
            true,
        );
        Timer { kq, interval }
    }

    pub fn reset(&mut self, deadline: Instant, interval: Option<Duration>) {
        add_timer_to_kqueue(
            self.kq.as_raw_fd(),
            1,
            duration_to_nseconds(deadline - Instant::now()),
            true,
        );
        self.interval = interval;
    }

    pub fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<u64> {
        match self.kq.poll_read_ready(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(r) => {
                let mut guard = r.expect("failed to poll kqueue");
                guard.clear_ready();
                if wait_kqueue(guard.get_inner().as_raw_fd()) {
                    if let Some(interval) = self.interval.take() {
                        add_timer_to_kqueue(
                            self.kq.as_raw_fd(),
                            1,
                            duration_to_nseconds(interval),
                            false,
                        );
                    }
                    Poll::Ready(1)
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

pub const TIMER_REMEMBER_EXPIRATIONS: bool = false;
