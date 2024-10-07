//! Hires timers for tokio.
//!
//! This is a drop-in replacement of [`tokio::time`](https://docs.rs/tokio/1.40.0/tokio/time/index.html).
//! The API is a 1:1 replication. Please refer to the doc there.
//!
//! Timers with the highest possible resolution from the operating system
//! are used. The feature `time` of tokio is not used and is not required.
//! Sub-millisecond granularity is achieved with:
//! - `timerfd` in Linux (and Android).
//! - `kqueue` with `EVFILT_TIMER` in *BSD and Apple's Darwin;
//!   specifically, `NOTE_MACHTIME` is used in Darwin to obtain the similar resolution to GCD.
//! - `CreateWaitableTimerEx` with `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` in Windows.

#[doc(no_inline)]
pub use std::time::{Duration, Instant};

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;

mod utils;

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "linux", target_os = "android"))] {
        mod timerfd;
        use timerfd::*;
    } else if #[cfg(any(target_os = "freebsd", target_os = "netbsd", target_os = "openbsd", target_os = "dragonfly", target_vendor = "apple"))] {
        mod kqueue;
        use kqueue::*;
    } else if #[cfg(windows)] {
        mod waitable;
        use waitable::*;
    } else {
        compile_error!("unsupported platform");
    }
}

pub struct Sleep {
    timer: Timer,
    deadline: Instant,
    elapsed: bool,
}

pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep {
        timer: Timer::new(deadline, None),
        deadline,
        elapsed: false,
    }
}

pub fn sleep(duration: Duration) -> Sleep {
    sleep_until(Instant::now().checked_add(duration).unwrap())
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.elapsed {
            return Poll::Ready(());
        }

        self.elapsed = matches!(self.timer.poll_expired(cx), Poll::Ready(_));

        if self.elapsed {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Sleep {
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn is_elapsed(&self) -> bool {
        self.elapsed
    }

    pub fn reset(&mut self, deadline: Instant) {
        self.timer.reset(deadline, None)
    }
}

pub mod error {
    use std::error::Error;
    use std::fmt::{Display, Formatter};

    #[derive(Debug, PartialEq, Eq)]
    pub struct Elapsed(pub(crate) ());

    impl Display for Elapsed {
        fn fmt(&self, fmt: &mut Formatter<'_>) -> std::fmt::Result {
            "deadline has elapsed".fmt(fmt)
        }
    }
    impl Error for Elapsed {}
}

pin_project! {
    pub struct Timeout<F> {
        #[pin]
        future: F,
        #[pin]
        sleep: Sleep,
    }
}

pub fn timeout_at<F: IntoFuture>(deadline: Instant, future: F) -> Timeout<F::IntoFuture> {
    Timeout {
        future: future.into_future(),
        sleep: sleep_until(deadline),
    }
}

pub fn timeout<F: IntoFuture>(duration: Duration, future: F) -> Timeout<F::IntoFuture> {
    timeout_at(Instant::now().checked_add(duration).unwrap(), future)
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, error::Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.sleep.is_elapsed() {
            return Poll::Ready(Err(error::Elapsed(())));
        }

        let this = self.project();
        if let Poll::Ready(output) = this.future.poll(cx) {
            Poll::Ready(Ok(output))
        } else if let Poll::Ready(()) = this.sleep.poll(cx) {
            Poll::Ready(Err(error::Elapsed(())))
        } else {
            Poll::Pending
        }
    }
}

impl<F> Timeout<F> {
    pub fn get_ref(&self) -> &F {
        &self.future
    }

    pub fn get_mut(&mut self) -> &mut F {
        &mut self.future
    }

    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut F> {
        self.project().future
    }

    pub fn into_inner(self) -> F {
        self.future
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissedTickBehavior {
    Burst,
    Delay,
    Skip,
}

pub struct Interval {
    timer: Timer,
    period: Duration,
    expirations: u64,
    behavior: MissedTickBehavior,
}

pub fn interval_at(start: Instant, period: Duration) -> Interval {
    Interval {
        timer: Timer::new(start, Some(period)),
        period,
        expirations: 0,
        behavior: if TIMER_REMEMBER_EXPIRATIONS {
            MissedTickBehavior::Burst
        } else {
            MissedTickBehavior::Skip
        },
    }
}

pub fn interval(period: Duration) -> Interval {
    let mut interval = interval_at(Instant::now().checked_add(period).unwrap(), period);
    interval.expirations = 1;
    interval
}

struct Tick<'a>(&'a mut Interval);

impl<'a> Future for Tick<'a> {
    type Output = Instant;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_tick(cx)
    }
}

impl Interval {
    pub fn tick(&mut self) -> impl Future<Output = Instant> + '_ {
        Tick(self)
    }

    pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
        if let Poll::Ready(exp) = self.timer.poll_expired(cx) {
            self.expirations += exp;
        };
        if self.expirations != 0 {
            self.expirations = match self.behavior {
                MissedTickBehavior::Burst => self.expirations - 1,
                MissedTickBehavior::Skip => 0,
                MissedTickBehavior::Delay => unreachable!(),
            };
            Poll::Ready(Instant::now())
        } else {
            Poll::Pending
        }
    }

    pub fn reset(&mut self) {
        self.reset_at(Instant::now().checked_add(self.period).unwrap())
    }

    pub fn reset_immediately(&mut self) {
        self.reset_at(Instant::now())
    }

    pub fn reset_after(&mut self, after: Duration) {
        self.reset_at(Instant::now().checked_add(after).unwrap())
    }

    pub fn reset_at(&mut self, deadline: Instant) {
        let now = Instant::now();
        if now < deadline {
            self.expirations = 0;
            self.timer.reset(deadline, Some(self.period));
        } else {
            let past = u64::try_from((now - deadline).as_nanos()).unwrap();
            let divider = u64::try_from(self.period.as_nanos()).unwrap();
            self.expirations = past / divider + 1;
            self.timer.reset(
                (now - Duration::from_nanos(past % divider))
                    .checked_add(self.period)
                    .unwrap(),
                Some(self.period),
            )
        }
    }

    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.behavior
    }

    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        if behavior == MissedTickBehavior::Delay {
            unimplemented!("MissedTickBehavior::Delay is not implemented yet");
        }
        if !TIMER_REMEMBER_EXPIRATIONS && behavior == MissedTickBehavior::Burst {
            unimplemented!("MissedTickBehavior::Burst is not supported on this platform");
        }
        self.behavior = behavior;
    }

    pub fn period(&self) -> Duration {
        self.period
    }
}

#[cfg(test)]
mod tests {
    use crate::*;

    cfg_if::cfg_if! {
        if #[cfg(feature = "test-hires")] {
            const TOLERANCE: Duration = Duration::from_millis(1);
        } else {
            const TOLERANCE: Duration = Duration::from_millis(5);
        }
    }

    #[tokio::test]
    async fn test_sleep() {
        let start = Instant::now();
        let duration = Duration::from_millis(100);
        sleep(duration).await;
        let elapsed = start.elapsed();
        assert!(elapsed.abs_diff(duration) < TOLERANCE);
    }

    #[tokio::test]
    async fn test_sleep_until() {
        let start = Instant::now();
        let duration = Duration::from_millis(100);
        sleep_until(start + duration).await;
        let elapsed = start.elapsed();
        assert!(elapsed.abs_diff(duration) < TOLERANCE);
    }

    #[tokio::test]
    async fn test_timeout() {
        let start = Instant::now();
        let duration = Duration::from_millis(100);
        let large_duration = Duration::from_secs(1);
        let small_duration = Duration::from_millis(10);
        assert!(timeout(duration, sleep(small_duration)).await.is_ok());
        let elapsed = start.elapsed();
        assert!(elapsed.abs_diff(small_duration) < TOLERANCE);

        let start = Instant::now();
        assert!(timeout(duration, sleep(large_duration)).await.is_err());
        let elapsed = start.elapsed();
        assert!(elapsed.abs_diff(duration) < TOLERANCE);
    }

    #[tokio::test]
    async fn test_timeout_at() {
        let start = Instant::now();
        let duration = Duration::from_millis(100);
        let large_duration = Duration::from_secs(1);
        let small_duration = Duration::from_millis(10);
        assert!(timeout_at(start + duration, sleep(small_duration))
            .await
            .is_ok());
        let elapsed = start.elapsed();
        assert!(elapsed.abs_diff(small_duration) < TOLERANCE);

        let start = Instant::now();
        assert!(timeout_at(start + duration, sleep(large_duration))
            .await
            .is_err());
        let elapsed = start.elapsed();
        assert!(elapsed.abs_diff(duration) < TOLERANCE);
    }

    #[tokio::test]
    async fn test_interval() {
        let start = Instant::now();
        let duration = Duration::from_millis(100);
        let mut iv = interval(duration);

        for i in 0..10 {
            let _ = iv.tick().await;
            let elapsed = start.elapsed();
            assert!(elapsed.abs_diff(duration * i) < TOLERANCE * (i + 1));
        }
    }

    #[tokio::test]
    async fn test_interval_at() {
        let start = Instant::now();
        let duration = Duration::from_millis(100);
        let mut iv = interval_at(start + duration, duration);

        for i in 1..=10 {
            let _ = iv.tick().await;
            let elapsed = start.elapsed();
            assert!(elapsed.abs_diff(duration * i) < TOLERANCE * i);
        }
    }

    #[cfg_attr(
        any(
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
            target_vendor = "apple"
        ),
        ignore
    )]
    #[tokio::test]
    async fn test_interval_burst() {
        let start = Instant::now();
        let duration = Duration::from_millis(100);
        let mut iv = interval(duration);

        sleep(duration * 3).await;
        let _ = iv.tick().await;
        let _ = iv.tick().await;
        let _ = iv.tick().await;
        let _ = iv.tick().await;
        let _ = iv.tick().await;
        let elapsed = start.elapsed();
        assert!(elapsed.abs_diff(duration * 4) < TOLERANCE * 5);
    }

    #[tokio::test]
    async fn test_interval_skip() {
        let start = Instant::now();
        let duration = Duration::from_millis(100);
        let mut iv = interval(duration);
        iv.set_missed_tick_behavior(MissedTickBehavior::Skip);

        sleep(duration * 3).await;
        let _ = iv.tick().await;
        let _ = iv.tick().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed.abs_diff(duration * 4) < TOLERANCE * 5
                || elapsed.abs_diff(duration * 5) < TOLERANCE * 5
        );
    }

    #[tokio::test]
    async fn test_interval_reset() {
        let start = Instant::now();
        let duration = Duration::from_millis(100);
        let mut iv = interval(duration);

        let _ = iv.tick().await;
        let _ = iv.tick().await;
        let _ = iv.tick().await;
        let elapsed = start.elapsed();
        assert!(elapsed.abs_diff(duration * 2) < TOLERANCE * 2);

        iv.reset_immediately();
        let _ = iv.tick().await;
        let _ = iv.tick().await;
        let _ = iv.tick().await;
        let elapsed = start.elapsed();
        assert!(elapsed.abs_diff(duration * 4) < TOLERANCE * 4);

        if TIMER_REMEMBER_EXPIRATIONS {
            iv.reset_at(start + Duration::from_millis(250));
            let _ = iv.tick().await;
            let _ = iv.tick().await;
            let _ = iv.tick().await;
            let elapsed = start.elapsed();
            assert!(elapsed.abs_diff(Duration::from_millis(450)) < TOLERANCE * 5);
        }
    }
}
