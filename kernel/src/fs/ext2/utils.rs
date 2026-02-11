// SPDX-License-Identifier: MPL-2.0

use core::{fmt::Debug, ops::MulAssign, time::Duration};

use crate::{prelude::warn, time::Clock};

pub trait IsPowerOf: Copy + Sized + MulAssign + PartialOrd {
    /// Returns true if and only if `self == x^k` for some `k` where `k > 0`.
    ///
    /// The `x` must be a positive value.
    fn is_power_of(&self, x: Self) -> bool {
        let mut power = x;
        while power < *self {
            power *= x;
        }

        power == *self
    }
}

macro_rules! impl_ipo_for {
    ($($ipo_ty:ty),*) => {
        $(impl IsPowerOf for $ipo_ty {})*
    };
}

impl_ipo_for!(
    u8, u16, u32, u64, u128, i8, i16, i32, i64, i128, isize, usize
);

/// Wraps a value with dirty tracking.
pub struct Dirty<T: Debug> {
    value: T,
    dirty: bool,
}

impl<T: Debug> Dirty<T> {
    /// Creates a new Dirty without setting the dirty flag.
    pub fn new(val: T) -> Dirty<T> {
        Dirty {
            value: val,
            dirty: false,
        }
    }

    /// Creates a new Dirty with setting the dirty flag.
    pub fn new_dirty(val: T) -> Dirty<T> {
        Dirty {
            value: val,
            dirty: true,
        }
    }

    /// Returns true if dirty, false otherwise.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clears the dirty flag.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

impl<T: Debug> core::ops::Deref for Dirty<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: Debug> core::ops::DerefMut for Dirty<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.dirty = true;
        &mut self.value
    }
}

impl<T: Debug> Drop for Dirty<T> {
    fn drop(&mut self) {
        if self.is_dirty() {
            warn!("[{:?}] is dirty then dropping", self.value);
        }
    }
}

impl<T: Debug> Debug for Dirty<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let tag = if self.dirty { "Dirty" } else { "Clean" };
        write!(f, "[{}] {:?}", tag, self.value)
    }
}

/// Returns the current time.
pub fn now() -> Duration {
    crate::time::clocks::RealTimeCoarseClock::get().read_time()
}