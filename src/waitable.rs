use std::ffi;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio_util::sync::PollSemaphore;

use windows::Win32::Foundation::{CloseHandle, BOOLEAN, HANDLE};
use windows::Win32::System::Threading::{
    self, CreateWaitableTimerExW, RegisterWaitForSingleObject, SetWaitableTimer, UnregisterWait,
    INFINITE,
};

fn set_waitable_timer(wt: HANDLE, ts: (i64, i32)) {
    unsafe { SetWaitableTimer(wt, ptr::from_ref(&ts.0), ts.1, None, None, false) }
        .expect("failed to set waitable timer");
}

unsafe extern "system" fn timer_callback(cb: *mut ffi::c_void, _: BOOLEAN) {
    let cb = mem::transmute::<*mut ffi::c_void, &Box<dyn Fn() + Send>>(cb);
    cb();
}

fn make_waitable_timer(ts: (i64, i32), cb: &Box<dyn Fn() + Send>) -> (HANDLE, HANDLE) {
    let wt = unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            Threading::CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            Threading::TIMER_ALL_ACCESS.0,
        )
    }
    .expect("failed to create waitable timer");
    set_waitable_timer(wt, ts);
    let mut wh = HANDLE::default();
    unsafe {
        RegisterWaitForSingleObject(
            ptr::from_mut(&mut wh),
            wt,
            Some(timer_callback),
            Some(ptr::from_ref(cb) as _),
            INFINITE,
            if ts.1 == 0 {
                Threading::WT_EXECUTEONLYONCE | Threading::WT_EXECUTEINWAITTHREAD
            } else {
                Threading::WT_EXECUTEINWAITTHREAD
            },
        )
    }
    .expect("failed to watch waitable timer");
    (wt, wh)
}

fn destroy_waitable_timer(wt: HANDLE, wh: HANDLE) {
    let _ = unsafe { UnregisterWait(wh) };
    let _ = unsafe { CloseHandle(wt) };
}

fn make_timerspec(deadline: Instant, interval: Option<Duration>) -> (i64, i32) {
    let duetime = -i64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_nanos()
            / 100,
    )
    .unwrap();
    let period = i32::try_from(interval.unwrap_or_default().as_millis()).unwrap();
    (duetime, period)
}

pub struct Timer {
    wt: HANDLE,
    wh: HANDLE,
    #[allow(dead_code)]
    cb: Box<Box<dyn Fn() + Send>>,
    waiter: PollSemaphore,
}

impl Timer {
    pub fn new(deadline: Instant, interval: Option<Duration>) -> Timer {
        let ts = make_timerspec(deadline, interval);
        let notify = Arc::new(Semaphore::new(0));
        let waiter = PollSemaphore::new(notify.clone());
        let cb = Box::new(Box::<dyn Fn() + Send>::from(Box::new(move || {
            notify.add_permits(1);
        })));
        let (wt, wh) = make_waitable_timer(ts, cb.as_ref());
        Timer { wt, wh, cb, waiter }
    }

    pub fn reset(&mut self, deadline: Instant, interval: Option<Duration>) {
        let _ = self.waiter.as_ref().forget_permits(usize::MAX);
        set_waitable_timer(self.wt, make_timerspec(deadline, interval));
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
        destroy_waitable_timer(self.wt, self.wh);
    }
}

pub const TIMER_REMEMBER_EXPIRATIONS: bool = true;
