//! React language-ID variant table (#165).
//!
//! Single source of truth for the base-language + extension -> React-variant
//! mapping, consumed by both `config::language_id_for_pattern_extension`
//! (deriving a server's effective extension map from `file_patterns`) and
//! `bridge::translator`'s candidate-language fallback (routing `.tsx`/`.jsx`
//! requests to a plain `typescript`/`javascript` server when no dedicated
//! `typescriptreact`/`javascriptreact` server is configured).

struct ReactVariant {
    base: &'static str,
    extension: &'static str,
    variant: &'static str,
}

const REACT_LANGUAGE_VARIANTS: &[ReactVariant] = &[
    ReactVariant {
        base: "javascript",
        extension: "jsx",
        variant: "javascriptreact",
    },
    ReactVariant {
        base: "typescript",
        extension: "tsx",
        variant: "typescriptreact",
    },
];

/// Map a server's base language id and a file extension to a more specific
/// React variant language id, if the pair is a known React extension.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::react_variant_language_id;
///
/// assert_eq!(
///     react_variant_language_id("typescript", "tsx"),
///     Some("typescriptreact")
/// );
/// assert_eq!(react_variant_language_id("typescript", "ts"), None);
/// ```
#[must_use]
pub fn react_variant_language_id(base: &str, extension: &str) -> Option<&'static str> {
    REACT_LANGUAGE_VARIANTS
        .iter()
        .find(|v| v.base == base && v.extension == extension)
        .map(|v| v.variant)
}

/// Inverse of [`react_variant_language_id`]: map a React variant language id
/// back to its base language id.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::base_language_id;
///
/// assert_eq!(base_language_id("typescriptreact"), Some("typescript"));
/// assert_eq!(base_language_id("typescript"), None);
/// ```
#[must_use]
pub fn base_language_id(variant: &str) -> Option<&'static str> {
    REACT_LANGUAGE_VARIANTS
        .iter()
        .find(|v| v.variant == variant)
        .map(|v| v.base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_forward_and_inverse_agree_for_every_row() {
        for variant in REACT_LANGUAGE_VARIANTS {
            assert_eq!(
                react_variant_language_id(variant.base, variant.extension),
                Some(variant.variant)
            );
            assert_eq!(base_language_id(variant.variant), Some(variant.base));
        }
    }

    #[test]
    fn test_forward_unknown_pair_returns_none() {
        assert_eq!(react_variant_language_id("python", "py"), None);
        assert_eq!(react_variant_language_id("typescript", "ts"), None);
    }

    #[test]
    fn test_inverse_unknown_variant_returns_none() {
        assert_eq!(base_language_id("python"), None);
        assert_eq!(base_language_id("javascript"), None);
    }
}
