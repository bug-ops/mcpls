//! The `spelled_enum!` utility: closed enums whose spellings are written once.

/// A closed set of spellings that can be looked up from text.
///
/// `spelled_enum!` implements it, so a spelling is written once and the
/// reverse lookup is not hand-kept beside it.
pub trait Spelled: Copy + 'static {
    /// Every variant, in declaration order.
    const VARIANTS: &'static [Self];

    /// The spelling of this variant.
    fn as_str(self) -> &'static str;

    /// The variant whose spelling is exactly `text`.
    fn from_spelling(text: &str) -> Option<Self> {
        Self::VARIANTS
            .iter()
            .copied()
            .find(|variant| variant.as_str() == text)
    }
}

/// Declares a closed enum of spellings: the variants, `ALL`, `as_str`,
/// `Display` and a [`Spelled`] impl, so a spelling is written once.
macro_rules! spelled_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $spelling:expr),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[non_exhaustive]
        pub enum $name {
            $($(#[$variant_meta])* $variant),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: [Self; [$(Self::$variant),+].len()] = [$(Self::$variant),+];

            /// The spelling as written on the wire or a command line.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $spelling),+
                }
            }
        }

        impl $crate::spelling::Spelled for $name {
            const VARIANTS: &'static [Self] = &Self::ALL;

            fn as_str(self) -> &'static str {
                <$name>::as_str(self)
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

pub(crate) use spelled_enum;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::Spelled;

    spelled_enum! {
        /// A sample closed set with extra derives.
        #[derive(Default, Hash)]
        Sample {
            /// The default.
            #[default]
            First => "first",
            /// A spelling unlike its variant name.
            Second => "second-two",
            /// A spelling that is not an identifier.
            Third => "3",
        }
    }

    #[test]
    fn all_lists_the_variants_in_declaration_order() {
        assert_eq!(Sample::ALL, [Sample::First, Sample::Second, Sample::Third]);
    }

    #[test]
    fn every_variant_displays_as_its_spelling_and_the_spellings_are_distinct() {
        let spellings: Vec<_> = Sample::ALL.iter().map(|v| v.as_str()).collect();
        assert_eq!(spellings, ["first", "second-two", "3"]);
        for variant in Sample::ALL {
            assert_eq!(variant.to_string(), variant.as_str());
        }
        assert_eq!(
            spellings.iter().collect::<HashSet<_>>().len(),
            Sample::ALL.len()
        );
    }

    #[test]
    fn a_spelling_finds_its_variant_back() {
        for variant in Sample::ALL {
            assert_eq!(Sample::from_spelling(variant.as_str()), Some(variant));
        }
        assert_eq!(Sample::from_spelling("First"), None);
        assert_eq!(Sample::from_spelling(""), None);
    }

    #[test]
    fn extra_derives_and_variant_attributes_pass_through() {
        assert_eq!(Sample::default(), Sample::First);
        let set: HashSet<Sample> = Sample::ALL.into_iter().collect();
        assert_eq!(set.len(), 3);
    }
}
