use std::fs::File;
use std::io::Read;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr;
use std::slice;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

use crate::utils::{cvt, instant_to_duration};

fn set_timefd(fd: libc::c_int, timerspec: libc::itimerspec, absolute: bool) {
    cvt(unsafe {
        libc::timerfd_settime(
            fd,
            if absolute { libc::TFD_TIMER_ABSTIME } else { 0 },
            &raw const timerspec,
            ptr::null_mut(),
        )
    })
    .expect("failed to set up timerfd");
}

fn make_timerfd(timerspec: libc::itimerspec, absolute: bool) -> AsyncFd<File> {
    let fd = cvt(unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
        )
    })
    .expect("failed to create timerfd");

    set_timefd(fd, timerspec, absolute);

    let file = unsafe { File::from_raw_fd(fd) };
    AsyncFd::with_interest(file, Interest::READABLE).unwrap()
}

fn duration_to_timespec(t: Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: t.as_secs().try_into().unwrap(),
        #[expect(clippy::cast_lossless)]
        tv_nsec: t.subsec_nanos() as _,
    }
}

fn instant_to_timespec(t: Instant) -> libc::timespec {
    duration_to_timespec(instant_to_duration(t))
}

fn make_timerspec(deadline: Option<Instant>, interval: Option<Duration>) -> libc::itimerspec {
    libc::itimerspec {
        it_value: instant_to_timespec(
            deadline.unwrap_or_else(|| Instant::now() + interval.unwrap_or_default()),
        ),
        it_interval: duration_to_timespec(interval.unwrap_or_default()),
    }
}

pub struct Timer(AsyncFd<File>);

impl Timer {
    pub fn new(deadline: Option<Instant>, interval: Option<Duration>) -> Timer {
        Timer(make_timerfd(make_timerspec(deadline, interval), true))
    }

    pub fn reset(&mut self, deadline: Option<Instant>, interval: Option<Duration>) {
        set_timefd(self.0.as_raw_fd(), make_timerspec(deadline, interval), true);
    }

    pub fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<u64> {
        match self.0.poll_read_ready(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(r) => {
                let mut guard = r.expect("failed to poll timerfd");
                guard.clear_ready();
                let mut expirations = mem::MaybeUninit::<u64>::uninit();
                match guard.get_inner().read(unsafe {
                    slice::from_raw_parts_mut(expirations.as_mut_ptr().cast::<u8>(), 8)
                }) {
                    Ok(size) => {
                        assert_eq!(size, 8);
                        Poll::Ready(unsafe { expirations.assume_init() })
                    }
                    Err(ref e) if e.raw_os_error() == Some(libc::EAGAIN) => Poll::Pending,
                    Err(e) => panic!("failed to read from timerfd: {e:?}"),
                }
            }
        }
    }
}
