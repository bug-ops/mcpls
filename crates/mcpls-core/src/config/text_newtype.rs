//! Shared boilerplate for validated string newtypes.

/// Implements the read-only conversions every validated text newtype shares.
///
/// The type must be a tuple struct over `Cow<'static, str>` that provides
/// `fn new(impl Into<String>) -> Result<Self, $err>`.
macro_rules! impl_text_newtype {
    ($ty:ident, $err:ty) => {
        impl $ty {
            /// The validated text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl ::std::fmt::Display for $ty {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl TryFrom<String> for $ty {
            type Error = $err;

            fn try_from(text: String) -> ::std::result::Result<Self, Self::Error> {
                Self::new(text)
            }
        }

        impl From<$ty> for String {
            fn from(value: $ty) -> Self {
                value.0.into_owned()
            }
        }

        impl ::std::str::FromStr for $ty {
            type Err = $err;

            fn from_str(text: &str) -> ::std::result::Result<Self, Self::Err> {
                Self::new(text)
            }
        }

        impl AsRef<str> for $ty {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl ::std::borrow::Borrow<str> for $ty {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }

        impl PartialEq<str> for $ty {
            fn eq(&self, other: &str) -> bool {
                self.as_str() == other
            }
        }

        impl PartialEq<&str> for $ty {
            fn eq(&self, other: &&str) -> bool {
                self.as_str() == *other
            }
        }
    };
}

pub(super) use impl_text_newtype;
