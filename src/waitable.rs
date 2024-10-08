use std::cmp::max;
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

fn set_waitable_timer(wt: HANDLE, ts: i64) {
    unsafe { SetWaitableTimer(wt, ptr::from_ref(&ts), 0, None, None, false) }
        .expect("failed to set waitable timer");
}

unsafe extern "system" fn timer_callback(cb: *mut ffi::c_void, _: BOOLEAN) {
    let cb: &mut Box<dyn FnMut() + Send> = mem::transmute(cb);
    cb();
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

fn start_waitable_timer(wt: HANDLE, cb: &Box<dyn FnMut() + Send>, oneshot: bool) -> HANDLE {
    let mut wh = HANDLE::default();
    unsafe {
        RegisterWaitForSingleObject(
            ptr::from_mut(&mut wh),
            wt,
            Some(timer_callback),
            Some(ptr::from_ref(cb) as _),
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
    let _ = unsafe { UnregisterWait(wh) };
    let _ = unsafe { CloseHandle(wt) };
}

fn make_duetime(deadline: Instant) -> i64 {
    -i64::try_from(max(
        deadline
            .saturating_duration_since(Instant::now())
            .as_nanos()
            / 100,
        1,
    ))
    .unwrap()
}

#[allow(dead_code)]
pub struct Timer {
    wt: HANDLE,
    wh: HANDLE,
    cb: Box<Box<dyn FnMut() + Send>>,
    deadline: Box<Instant>,
    waiter: PollSemaphore,
}

impl Timer {
    pub fn new(deadline: Option<Instant>, interval: Option<Duration>) -> Timer {
        let mut deadline = Box::new(deadline.unwrap_or_else(|| Instant::now() + interval.unwrap_or_default()));
        let ts = make_duetime(*deadline);
        let notify = Arc::new(Semaphore::new(0));
        let waiter = PollSemaphore::new(notify.clone());
        let wt = create_waitable_timer();
        set_waitable_timer(wt, ts);
        let cwt = wt.0 as usize;
        let pdl = ptr::from_mut(deadline.as_mut()) as usize;
        let cb = Box::new(Box::<dyn FnMut() + Send>::from(Box::new(move || {
            notify.add_permits(1);
            if let Some(interval) = interval {
                let deadline = unsafe { &mut *(pdl as *mut Instant) };
                *deadline += interval;
                let wt = HANDLE(cwt as *mut std::ffi::c_void);
                let ts = make_duetime(*deadline);
                set_waitable_timer(wt, ts);
            }
        })));
        let wh = start_waitable_timer(wt, cb.as_ref(), interval.is_none());
        Timer { wt, wh, cb, deadline, waiter }
    }

    pub fn reset(&mut self, deadline: Option<Instant>, interval: Option<Duration>) {
        *self = Timer::new(deadline, interval);
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
