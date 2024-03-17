// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License in the LICENSE file at the
// root of this repository, or online at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Cast utilities.

use num::traits::bounds::UpperBounded;
use num::Signed;
use std::error::Error;
use std::fmt;
use std::ops::Deref;

/// A trait for safe, simple, and infallible casts.
///
/// `CastFrom` is like [`std::convert::From`], but it is implemented for some
/// platform-specific casts that are missing from the standard library. For
/// example, there is no `From<u32> for usize` implementation, because Rust may
/// someday support platforms where usize is smaller than 32 bits. Since we
/// don't care about such platforms, we are happy to provide a `CastFrom<u32>
/// for usize` implementation.
///
/// `CastFrom` should be preferred to the `as` operator, since the `as` operator
/// will silently truncate if the target type is smaller than the source type.
/// When applicable, `CastFrom` should also be preferred to the
/// [`std::convert::TryFrom`] trait, as `TryFrom` will produce a runtime error,
/// while `CastFrom` will produce a compile-time error.
pub trait CastFrom<T> {
    /// Performs the cast.
    fn cast_from(from: T) -> Self;
}

macro_rules! cast_from {
    ($from:ty, $to:ty) => {
        paste::paste! {
            impl crate::cast::CastFrom<$from> for $to {
                #[allow(clippy::as_conversions)]
                #[allow(unused)]
                fn cast_from(from: $from) -> $to {
                    from as $to
                }
            }

            /// Casts [`$from`] to [`$to`].
            ///
            /// This is equivalent to the [`crate::cast::CastFrom`] implementation but is
            /// available as a `const fn`.
            #[allow(clippy::as_conversions)]
            #[allow(unused)]
            pub const fn [< $from _to_ $to >](from: $from) -> $to {
                from as $to
            }
        }
    };
}

#[cfg(target_pointer_width = "32")]
/// Safe casts for 32bit platforms
mod target32 {
    // size_of<from> < size_of<target>
    cast_from!(u8, usize);
    cast_from!(u16, usize);
    cast_from!(u8, isize);
    cast_from!(i8, isize);
    cast_from!(u16, isize);
    cast_from!(i16, isize);

    cast_from!(usize, u64);
    cast_from!(usize, i64);
    cast_from!(usize, u128);
    cast_from!(usize, i128);
    cast_from!(isize, i64);
    cast_from!(isize, i128);

    // size_of<from> == size_of<target>
    cast_from!(usize, u32);
    cast_from!(isize, i32);
    cast_from!(u32, usize);
    cast_from!(i32, isize);
}

#[cfg(target_pointer_width = "64")]
/// Safe casts for 64bit platforms
pub mod target64 {
    // size_of<from> < size_of<target>
    cast_from!(u8, usize);
    cast_from!(u16, usize);
    cast_from!(u32, usize);
    cast_from!(u8, isize);
    cast_from!(i8, isize);
    cast_from!(u16, isize);
    cast_from!(i16, isize);
    cast_from!(u32, isize);
    cast_from!(i32, isize);

    cast_from!(usize, u128);
    cast_from!(usize, i128);
    cast_from!(isize, i128);

    // size_of<from> == size_of<target>
    cast_from!(usize, u64);
    cast_from!(isize, i64);
    cast_from!(u64, usize);
    cast_from!(i64, isize);
}

// TODO(petrosagg): remove these once the std From impls become const
cast_from!(u8, u8);
cast_from!(u8, u16);
cast_from!(u8, i16);
cast_from!(u8, u32);
cast_from!(u8, i32);
cast_from!(u8, u64);
cast_from!(u8, i64);
cast_from!(u8, u128);
cast_from!(u8, i128);
cast_from!(u16, u16);
cast_from!(u16, u32);
cast_from!(u16, i32);
cast_from!(u16, u64);
cast_from!(u16, i64);
cast_from!(u16, u128);
cast_from!(u16, i128);
cast_from!(u32, u32);
cast_from!(u32, u64);
cast_from!(u32, i64);
cast_from!(u32, u128);
cast_from!(u32, i128);
cast_from!(u64, u64);
cast_from!(u64, u128);
cast_from!(u64, i128);
cast_from!(i8, i8);
cast_from!(i8, i16);
cast_from!(i8, i32);
cast_from!(i8, i64);
cast_from!(i8, i128);
cast_from!(i16, i16);
cast_from!(i16, i32);
cast_from!(i16, i64);
cast_from!(i16, i128);
cast_from!(i32, i32);
cast_from!(i32, i64);
cast_from!(i32, i128);
cast_from!(i64, i64);
cast_from!(i64, i128);

/// A trait for reinterpreting casts.
///
/// `ReinterpretCast` is like `as`, but it allows the caller to be specific about their
/// intentions to reinterpreting the bytes from one type to another. For example, if we
/// have some `u32` that we want to use as the return value of a postgres function, and
/// we don't mind converting large unsigned numbers to negative signed numbers, then
/// we would use `ReinterpretCast<i32>`.
///
/// `ReinterpretCast` should be preferred to the `as` operator, since it explicitly
/// conveys the intention to reinterpret the type.
pub trait ReinterpretCast<T> {
    /// Performs the cast.
    fn reinterpret_cast(from: T) -> Self;
}

macro_rules! reinterpret_cast {
    ($from:ty, $to:ty) => {
        impl ReinterpretCast<$from> for $to {
            #[allow(clippy::as_conversions)]
            fn reinterpret_cast(from: $from) -> $to {
                from as $to
            }
        }
    };
}

reinterpret_cast!(u8, i8);
reinterpret_cast!(i8, u8);
reinterpret_cast!(u16, i16);
reinterpret_cast!(i16, u16);
reinterpret_cast!(u32, i32);
reinterpret_cast!(i32, u32);
reinterpret_cast!(u64, i64);
reinterpret_cast!(i64, u64);

/// A trait for attempted casts.
///
/// `TryCast` is like `as`, but returns `None` if
/// the conversion can't be round-tripped.
///
/// Note: there may be holes in the domain of `try_cast_from`,
/// which is probably why `TryFrom` wasn't implemented for floats in the
/// standard library. For example, `i64::MAX` can be converted to
/// `f64`, but `i64::MAX - 1` can't.
pub trait TryCastFrom<T>: Sized {
    /// Attempts to perform the cast
    fn try_cast_from(from: T) -> Option<Self>;
}

/// Implement `TryCastFrom` for the specified types.
/// This is only necessary for types for which `as` exists,
/// but `TryFrom` doesn't (notably floats).
macro_rules! try_cast_from {
    ($from:ty, $to:ty) => {
        impl crate::cast::TryCastFrom<$from> for $to {
            #[allow(clippy::as_conversions)]
            fn try_cast_from(from: $from) -> Option<$to> {
                let to = from as $to;
                let inverse = to as $from;
                if from == inverse {
                    Some(to)
                } else {
                    None
                }
            }
        }
    };
}

try_cast_from!(f64, i64);
try_cast_from!(i64, f64);
try_cast_from!(f64, u64);
try_cast_from!(u64, f64);

/// A wrapper type which ensures a signed number is non-negative.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
#[repr(transparent)]
pub struct NonNeg<T>(T)
where
    T: Signed + fmt::Display;

impl<T> NonNeg<T>
where
    T: Signed + fmt::Display,
{
    /// Returns the minimum value of the type.
    pub fn min() -> NonNeg<T> {
        NonNeg(T::zero())
    }

    /// Returns the maximum value of the type.
    pub fn max() -> NonNeg<T>
    where
        T: UpperBounded,
    {
        NonNeg(T::max_value())
    }

    /// Attempts to construct a `NonNeg` from its underlying type.
    ///
    /// Returns an error if `n` is negative.
    pub fn try_from(n: T) -> Result<NonNeg<T>, NonNegError> {
        match n.is_negative() {
            false => Ok(NonNeg(n)),
            true => Err(NonNegError),
        }
    }
}

impl<T> fmt::Display for NonNeg<T>
where
    T: Signed + fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> Deref for NonNeg<T>
where
    T: Signed + fmt::Display,
{
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl From<NonNeg<i64>> for u64 {
    fn from(n: NonNeg<i64>) -> u64 {
        u64::try_from(*n).expect("non-negative")
    }
}

#[cfg(target_pointer_width = "64")]
impl CastFrom<NonNeg<i64>> for usize {
    #[allow(clippy::as_conversions)]
    fn cast_from(from: NonNeg<i64>) -> usize {
        usize::cast_from(u64::from(from))
    }
}

/// An error indicating the attempted construction of a `NonNeg` with a negative
/// number.
#[derive(Debug, Clone)]
pub struct NonNegError;

impl fmt::Display for NonNegError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("cannot construct NonNeg from negative number")
    }
}

impl Error for NonNegError {}
