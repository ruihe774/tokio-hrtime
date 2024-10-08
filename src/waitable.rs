use std::ffi;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio_util::sync::PollSemaphore;
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

unsafe fn start_waitable_timer(
    wt: HANDLE,
    cb: *mut Box<dyn FnMut() + Send>,
    oneshot: bool,
) -> HANDLE {
    let mut wh = HANDLE::default();
    RegisterWaitForSingleObject(
        ptr::from_mut(&mut wh),
        wt,
        Some(timer_callback),
        Some(cb as _),
        INFINITE,
        if oneshot {
            Threading::WT_EXECUTEONLYONCE | Threading::WT_EXECUTEINWAITTHREAD
        } else {
            Threading::WT_EXECUTEINWAITTHREAD
        },
    )
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
    cb: Box<Box<dyn FnMut() + Send>>,
    waiter: PollSemaphore,
}

impl Timer {
    pub fn new(deadline: Option<Instant>, interval: Option<Duration>) -> Timer {
        let mut deadline =
            deadline.unwrap_or_else(|| Instant::now() + interval.unwrap_or_default());
        let ts = make_duetime(deadline);
        let notify = Arc::new(Semaphore::new(0));
        let waiter = PollSemaphore::new(notify.clone());
        let wt = create_waitable_timer();
        set_waitable_timer(wt, ts);
        let cwt = wt.0 as usize;
        let mut cb = Box::new(Box::<dyn FnMut() + Send>::from(Box::new(move || {
            notify.add_permits(1);
            if let Some(interval) = interval {
                deadline += interval;
                let wt = HANDLE(cwt as *mut std::ffi::c_void);
                let ts = make_duetime(deadline);
                set_waitable_timer(wt, ts);
            }
        })));
        let wh =
            unsafe { start_waitable_timer(wt, ptr::from_mut(cb.as_mut()), interval.is_none()) };
        Timer { wt, wh, cb, waiter }
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
