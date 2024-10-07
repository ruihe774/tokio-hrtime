Hires timers for tokio.

This is a drop-in replacement of [`tokio::time`](https://docs.rs/tokio/1.40.0/tokio/time/index.html).
The API is a 1:1 replication. Please refer to the doc there.
Timers with the highest possible resolution from the operating system
are used. The feature `time` of tokio is not used and is not required.
Sub-millisecond granularity is achieved with:
- `timerfd` in Linux (and Android).
- `kqueue` with `EVFILT_TIMER` in *BSD and Apple's Darwin;
  specifically, `NOTE_MACHTIME` is used in Darwin to obtain the similar resolution to GCD.
- `CreateWaitableTimerEx` with `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` in Windows.
