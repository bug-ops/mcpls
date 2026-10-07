//! Shared boilerplate for validated numeric newtypes.

/// Implements the conversions and the default every bounded number shares.
///
/// The type must provide a `const fn new(prim) -> Result<Self, $err>`.
///
/// - `$ty, $prim, $err, default = $default, $doc, into = $into` generates
///   `DEFAULT` (documented by `$doc`), `Default`, `TryFrom<$prim>` and
///   `From<$ty> for $prim`, where `$into` is a closure from the value to
///   `$prim`.
/// - `@default $ty, $default, $doc` generates only `DEFAULT` and `Default`.
/// - `@convert [generics] $ty, $prim, $err, $into` generates only the
///   conversions, for a generic type.
macro_rules! impl_bounded_number {
    (@default $ty:ty, $default:expr, $doc:literal) => {
        impl $ty {
            #[doc = $doc]
            pub const DEFAULT: Self = match Self::new($default) {
                Ok(value) => value,
                Err(_) => panic!(concat!(
                    "the default of ",
                    stringify!($ty),
                    " must be in range"
                )),
            };
        }

        impl ::std::default::Default for $ty {
            fn default() -> Self {
                Self::DEFAULT
            }
        }
    };
    (@convert [$($gen:tt)*] $ty:ty, $prim:ty, $err:ty, $into:expr) => {
        impl<$($gen)*> ::std::convert::TryFrom<$prim> for $ty {
            type Error = $err;

            fn try_from(value: $prim) -> ::std::result::Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl<$($gen)*> ::std::convert::From<$ty> for $prim {
            fn from(value: $ty) -> Self {
                ($into)(value)
            }
        }
    };
    ($ty:ty, $prim:ty, $err:ty, default = $default:expr, $doc:literal, into = $into:expr) => {
        impl_bounded_number!(@default $ty, $default, $doc);
        impl_bounded_number!(@convert [] $ty, $prim, $err, $into);
    };
}

pub(super) use impl_bounded_number;
