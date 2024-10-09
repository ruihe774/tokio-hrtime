use std::ffi;
use std::marker::PhantomPinned;
use std::mem;
use std::pin::Pin;
use std::ptr;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use windows::core::HRESULT;
use windows::Win32::Foundation::{CloseHandle, BOOLEAN, ERROR_IO_PENDING, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    self, CreateEventW, CreateWaitableTimerExW, RegisterWaitForSingleObject, SetWaitableTimer,
    UnregisterWaitEx, WaitForSingleObject, INFINITE,
};

fn set_waitable_timer(wt: HANDLE, ts: i64) {
    unsafe { SetWaitableTimer(wt, ptr::from_ref(&ts), 0, None, None, false) }
        .expect("failed to set waitable timer");
}

struct Capsule {
    wt: HANDLE,
    waker: Option<Waker>,
    expirations: u64,
    deadline: Instant,
    interval: Option<Duration>,
    _pin: PhantomPinned,
}

fn reset_timer(capsule: &Mutex<Capsule>) {
    let mut capsule = capsule.lock().unwrap();

    capsule.expirations += 1;

    if let Some(waker) = capsule.waker.take() {
        waker.wake();
    }

    if let Some(interval) = capsule.interval {
        capsule.deadline += interval;
        let ts = make_duetime(capsule.deadline);
        set_waitable_timer(capsule.wt, ts);
    }
}

unsafe extern "system" fn timer_callback(capsule: *mut ffi::c_void, _: BOOLEAN) {
    let capsule: &Mutex<Capsule> = mem::transmute(capsule);
    reset_timer(capsule);
}

fn create_waitable_timer() -> HANDLE {
    unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            Threading::CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            Threading::TIMER_ALL_ACCESS.0,
        )
    }
    .expect("failed to create waitable timer")
}

unsafe fn start_waitable_timer(wt: HANDLE, capsule: Pin<&Mutex<Capsule>>, oneshot: bool) -> HANDLE {
    let mut wh = HANDLE::default();
    unsafe {
        RegisterWaitForSingleObject(
            ptr::from_mut(&mut wh),
            wt,
            Some(timer_callback),
            Some(ptr::from_ref(capsule.get_ref()) as _),
            INFINITE,
            if oneshot {
                Threading::WT_EXECUTEONLYONCE | Threading::WT_EXECUTEINWAITTHREAD
            } else {
                Threading::WT_EXECUTEINWAITTHREAD
            },
        )
    }
    .expect("failed to watch waitable timer");
    wh
}

fn destroy_waitable_timer(wt: HANDLE, wh: HANDLE) {
    let eh = unsafe { CreateEventW(None, false, false, None) }.unwrap();
    match unsafe { UnregisterWaitEx(wh, eh) } {
        Err(e) if e.code() == HRESULT::from(ERROR_IO_PENDING) => Ok(()),
        r => r,
    }
    .unwrap();
    assert_eq!(unsafe { WaitForSingleObject(eh, INFINITE) }, WAIT_OBJECT_0);
    unsafe { CloseHandle(wt) }.unwrap();
}

fn make_duetime(deadline: Instant) -> i64 {
    -i64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_nanos()
            .div_ceil(100),
    )
    .unwrap()
}

#[allow(dead_code)]
pub struct Timer {
    wt: HANDLE,
    wh: HANDLE,
    capsule: Pin<Box<Mutex<Capsule>>>,
}

impl Timer {
    pub fn new(deadline: Option<Instant>, interval: Option<Duration>) -> Timer {
        let deadline = deadline.unwrap_or_else(|| Instant::now() + interval.unwrap_or_default());
        let ts = make_duetime(deadline);
        let wt = create_waitable_timer();
        set_waitable_timer(wt, ts);
        let capsule = Box::pin(Mutex::new(Capsule {
            wt,
            waker: None,
            expirations: 0,
            deadline,
            interval,
            _pin: PhantomPinned,
        }));
        let wh = unsafe { start_waitable_timer(wt, capsule.as_ref(), interval.is_none()) };
        Timer { wt, wh, capsule }
    }

    pub fn reset(&mut self, deadline: Option<Instant>, interval: Option<Duration>) {
        *self = Timer::new(deadline, interval);
    }

    pub fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<u64> {
        let mut capsule = self.capsule.lock().unwrap();

        let expirations = mem::replace(&mut capsule.expirations, 0);

        if let Some(waker) = capsule.waker.as_mut() {
            waker.clone_from(cx.waker());
        } else {
            capsule.waker = Some(cx.waker().clone());
        }

        if expirations != 0 {
            Poll::Ready(expirations)
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        destroy_waitable_timer(self.wt, self.wh);
    }
}
