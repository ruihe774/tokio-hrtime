use std::mem;
use std::time::{Duration, Instant};

pub fn instant_to_duration(t: Instant) -> Duration {
    // this is dirty
    let t0 = mem::MaybeUninit::<Instant>::zeroed();
    let t0 = unsafe { t0.assume_init() };
    t - t0
}

cfg_if::cfg_if! {
    if #[cfg(unix)] {
        use std::io;

        pub trait IsMinusOne {
            fn is_minus_one(&self) -> bool;
        }

        macro_rules! impl_is_minus_one {
            ($($t:ident)*) => ($(impl IsMinusOne for $t {
                fn is_minus_one(&self) -> bool {
                    *self == -1
                }
            })*)
        }

        impl_is_minus_one! { i8 i16 i32 i64 isize }

        pub fn cvt<T: IsMinusOne>(t: T) -> io::Result<T> {
            if t.is_minus_one() {
                Err(io::Error::last_os_error())
            } else {
                Ok(t)
            }
        }
    }
}
