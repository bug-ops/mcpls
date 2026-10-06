//! Shared boilerplate for validated string newtypes.

/// Implements the read-only conversions every validated text newtype shares.
///
/// The type must be a tuple struct over `Cow<'static, str>` or `String` that
/// provides `fn new(impl Into<String>) -> Result<Self, $err>`.
///
/// The `non_blank` form also generates that constructor and `from_static`
/// for a `Cow<'static, str>` newtype whose only rule is "not blank": `new`
/// rejects blank text, `from_static` additionally requires ASCII so the
/// literal check is a subset of `new`'s. `$what` names the value in the
/// `from_static` panic message.
macro_rules! impl_text_newtype {
    ($ty:ident, $err:ident, non_blank, $what:literal) => {
        impl $ty {
            /// Builds the value from a literal, checked at compile time when
            /// evaluated in a `const` context.
            ///
            /// Accepts only ASCII, non-blank literals, a subset of what
            /// [`Self::new`] accepts.
            ///
            /// # Panics
            ///
            /// Panics if the literal is blank or not ASCII.
            ///
            /// # Examples
            ///
            #[doc = concat!("```\nuse mcpls_core::config::", stringify!($ty), ";\n")]
            #[doc = concat!("const VALUE: ", stringify!($ty), " = ", stringify!($ty), "::from_static(\"value\");")]
            #[doc = "assert_eq!(VALUE.as_str(), \"value\");\n```"]
            #[must_use]
            pub const fn from_static(text: &'static str) -> Self {
                assert!(
                    text.is_ascii() && !text.trim_ascii().is_empty(),
                    concat!($what, " must be ASCII and not blank")
                );
                Self(::std::borrow::Cow::Borrowed(text))
            }

            /// Builds the value from any string.
            ///
            /// # Errors
            ///
            #[doc = concat!("Returns [`", stringify!($err), "`] if `text` is blank.")]
            ///
            /// # Examples
            ///
            #[doc = concat!("```\nuse mcpls_core::config::", stringify!($ty), ";\n")]
            #[doc = concat!("assert!(", stringify!($ty), "::new(\"value\").is_ok());")]
            #[doc = concat!("assert!(", stringify!($ty), "::new(\"  \").is_err());\n```")]
            pub fn new(text: impl Into<String>) -> ::std::result::Result<Self, $err> {
                let text = text.into();
                if text.trim().is_empty() {
                    return ::std::result::Result::Err($err);
                }
                Ok(Self(::std::borrow::Cow::Owned(text)))
            }
        }

        impl_text_newtype!($ty, $err);
    };
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
                Self::from(value.0)
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
