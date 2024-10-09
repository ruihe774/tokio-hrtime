use std::ffi;
use std::mem;
use std::panic::catch_unwind;
use std::pin::Pin;
use std::process::abort;
use std::ptr;
use std::sync::{Arc, Mutex};
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

struct Capsule {
    wt: HANDLE,
    notify: Arc<Semaphore>,
    deadline: Instant,
    interval: Option<Duration>,
}

fn reset_timer(capsule: &Mutex<Capsule>) {
    let mut capsule = capsule.lock().unwrap();
    capsule.notify.add_permits(1);
    if let Some(interval) = capsule.interval {
        capsule.deadline += interval;
        let ts = make_duetime(capsule.deadline);
        set_waitable_timer(capsule.wt, ts);
    }
}

unsafe extern "system" fn timer_callback(capsule: *mut ffi::c_void, _: BOOLEAN) {
    if let Err(err) = catch_unwind(|| {
        let capsule: &Mutex<Capsule> = mem::transmute(capsule);
        reset_timer(capsule)
    }) {
        eprintln!("{err:?}");
        abort();
    }
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
    capsule: Pin<&Mutex<Capsule>>,
    oneshot: bool,
) -> HANDLE {
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
    waiter: PollSemaphore,
}

impl Timer {
    pub fn new(deadline: Option<Instant>, interval: Option<Duration>) -> Timer {
        let deadline = deadline.unwrap_or_else(|| Instant::now() + interval.unwrap_or_default());
        let ts = make_duetime(deadline);
        let notify = Arc::new(Semaphore::new(0));
        let waiter = PollSemaphore::new(notify.clone());
        let wt = create_waitable_timer();
        set_waitable_timer(wt, ts);
        let capsule = Box::pin(Mutex::new(Capsule {
            notify,
            wt,
            deadline,
            interval,
        }));
        let wh = unsafe { start_waitable_timer(wt, capsule.as_ref(), interval.is_none()) };
        Timer {
            wt,
            wh,
            capsule,
            waiter,
        }
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
