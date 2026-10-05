//! React language-ID variant table (#165).
//!
//! Single source of truth for the base-language + extension -> React-variant
//! mapping, consumed by both `config::language_id_for_pattern_extension`
//! (deriving a server's effective extension map from `file_patterns`) and
//! `bridge::translator`'s candidate-language fallback (routing `.tsx`/`.jsx`
//! requests to a plain `typescript`/`javascript` server when no dedicated
//! `typescriptreact`/`javascriptreact` server is configured).

use super::LanguageId;

struct ReactVariant {
    base: LanguageId,
    extension: &'static str,
    variant: LanguageId,
}

const REACT_LANGUAGE_VARIANTS: &[ReactVariant] = &[
    ReactVariant {
        base: LanguageId::from_static("javascript"),
        extension: "jsx",
        variant: LanguageId::from_static("javascriptreact"),
    },
    ReactVariant {
        base: LanguageId::from_static("typescript"),
        extension: "tsx",
        variant: LanguageId::from_static("typescriptreact"),
    },
];

/// Map a server's base language id and a file extension to a more specific
/// React variant language id, if the pair is a known React extension.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::{LanguageId, react_variant_language_id};
///
/// let typescript = LanguageId::new("typescript").unwrap();
/// assert_eq!(
///     react_variant_language_id(&typescript, "tsx"),
///     Some(LanguageId::new("typescriptreact").unwrap())
/// );
/// assert_eq!(react_variant_language_id(&typescript, "ts"), None);
/// ```
#[must_use]
pub fn react_variant_language_id(base: &LanguageId, extension: &str) -> Option<LanguageId> {
    REACT_LANGUAGE_VARIANTS
        .iter()
        .find(|v| v.base == *base && v.extension == extension)
        .map(|v| v.variant.clone())
}

/// Inverse of [`react_variant_language_id`]: map a React variant language id
/// back to its base language id.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::{LanguageId, base_language_id};
///
/// let react = LanguageId::new("typescriptreact").unwrap();
/// assert_eq!(base_language_id(&react), Some(LanguageId::new("typescript").unwrap()));
/// assert_eq!(base_language_id(&LanguageId::new("typescript").unwrap()), None);
/// ```
#[must_use]
pub fn base_language_id(variant: &LanguageId) -> Option<LanguageId> {
    REACT_LANGUAGE_VARIANTS
        .iter()
        .find(|v| v.variant == *variant)
        .map(|v| v.base.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_forward_and_inverse_agree_for_every_row() {
        for variant in REACT_LANGUAGE_VARIANTS {
            assert_eq!(
                react_variant_language_id(&variant.base, variant.extension),
                Some(variant.variant.clone())
            );
            assert_eq!(
                base_language_id(&variant.variant),
                Some(variant.base.clone())
            );
        }
    }

    #[test]
    fn test_forward_unknown_pair_returns_none() {
        let id = |s: &'static str| LanguageId::from_static(s);
        assert_eq!(react_variant_language_id(&id("python"), "py"), None);
        assert_eq!(react_variant_language_id(&id("typescript"), "ts"), None);
    }

    #[test]
    fn test_inverse_unknown_variant_returns_none() {
        let id = |s: &'static str| LanguageId::from_static(s);
        assert_eq!(base_language_id(&id("python")), None);
        assert_eq!(base_language_id(&id("javascript")), None);
    }
}
