//! Redaction of secrets from server output that reaches logs and MCP clients.
//!
//! A language server's stderr is shown to the MCP client when startup fails.
//! A server that dumps its environment or echoes a rejected argument could
//! otherwise hand over credentials, so values that look secret are replaced
//! with `[redacted:NAME]` first. Matching is by exact value plus its JSON and
//! `Debug` escaped spellings, so a secret is also found inside a logged wire
//! frame or a `{:?}` rendering. Other encodings (URL, base64) are not found,
//! and neither is a JSON spelling written by a foreign encoder that escapes
//! non-ASCII as `\uXXXX` or `/` as `\/`: such a secret can slip past trace
//! redaction.

use std::borrow::Cow;

use crate::config::LspServerConfig;

/// Shortest value redacted; shorter ones would match unrelated text.
const MIN_SECRET_BYTES: usize = 8;

/// Shortest fragment of a secret hidden when an elision cut splits it.
const MIN_FRAGMENT_BYTES: usize = 4;

/// Longest label kept in a replacement marker.
const MAX_LABEL_CHARS: usize = 64;

/// Upper-case substrings that make an environment variable, flag or JSON key
/// name denote a secret.
const SECRET_NAME_PATTERNS: [&str; 6] = ["TOKEN", "KEY", "SECRET", "PASSW", "CRED", "AUTH"];

/// Whether `name` (an environment variable, a flag without its dashes, or a
/// JSON key) denotes a secret, ignoring case.
pub fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_NAME_PATTERNS
        .iter()
        .any(|pattern| upper.contains(pattern))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Secret {
    label: String,
    value: String,
    /// JSON- and `Debug`-escaped spellings of `value` that differ from it.
    escaped: Vec<String>,
}

impl Secret {
    fn new(label: String, value: String) -> Self {
        let json = serde_json::Value::String(value.clone()).to_string();
        let debug = format!("{value:?}");
        let mut escaped: Vec<String> = [json, debug]
            .into_iter()
            .filter_map(|quoted| {
                quoted
                    .strip_prefix('"')
                    .and_then(|rest| rest.strip_suffix('"'))
                    .map(str::to_owned)
            })
            .filter(|spelling| *spelling != value)
            .collect();
        escaped.sort();
        escaped.dedup();
        Self {
            label,
            value,
            escaped,
        }
    }

    fn marker(&self) -> String {
        format!("[redacted:{}]", self.label)
    }

    fn spellings(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.value.as_str()).chain(self.escaped.iter().map(String::as_str))
    }
}

/// The secret values to hide from a server's output, longest first so a value
/// that contains another is replaced whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redactions(Vec<Secret>);

impl Redactions {
    /// Builds the set from `(label, value)` candidates, dropping values under
    /// [`MIN_SECRET_BYTES`] and duplicates. Labels are reduced to
    /// `[A-Za-z0-9_.-]`.
    pub(crate) fn new(candidates: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut secrets: Vec<Secret> = candidates
            .into_iter()
            .filter(|(_, value)| value.len() >= MIN_SECRET_BYTES)
            .map(|(label, value)| Secret::new(sanitize_label(&label), value))
            .collect();
        secrets.sort_by(|a, b| {
            b.value
                .len()
                .cmp(&a.value.len())
                .then_with(|| a.value.cmp(&b.value))
                .then_with(|| a.label.cmp(&b.label))
        });
        secrets.dedup_by(|a, b| a.value == b.value);
        Self(secrets)
    }

    /// The secrets a launched server can leak: values of `config.env` and of
    /// the `inherited` environment whose names look secret, the value of a
    /// secret-named `--flag=value` or `--flag value` argument, and string
    /// leaves of `initialization_options` under a secret-named key.
    pub(crate) fn for_server(
        config: &LspServerConfig,
        inherited: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let mut candidates: Vec<(String, String)> = config
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .chain(inherited)
            .filter(|(name, _)| is_secret_name(name))
            .collect();
        collect_secret_args(&config.args, &mut candidates);
        if let Some(options) = &config.initialization_options {
            collect_secret_json(options, None, &mut candidates);
        }
        Self::new(candidates)
    }

    /// Replaces every secret, in raw, JSON-escaped and `Debug`-escaped
    /// spelling, in `text` with its marker; borrows `text` when none occurs.
    pub(crate) fn apply<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.0.iter().fold(Cow::Borrowed(text), |acc, secret| {
            secret.spellings().fold(acc, |acc, spelling| {
                if acc.contains(spelling) {
                    Cow::Owned(acc.replace(spelling, &secret.marker()))
                } else {
                    acc
                }
            })
        })
    }

    /// Hides a secret cut by an elision boundary at the end of `head`: the
    /// longest trailing fragment of at least [`MIN_FRAGMENT_BYTES`] bytes that
    /// is a prefix of a secret is replaced by its marker.
    pub(crate) fn mask_cut_head(&self, head: &str) -> String {
        for secret in &self.0 {
            for len in (MIN_FRAGMENT_BYTES..secret.value.len()).rev() {
                let Some(fragment) = secret.value.get(..len) else {
                    continue;
                };
                if let Some(kept) = head.strip_suffix(fragment) {
                    return format!("{kept}{}", secret.marker());
                }
            }
        }
        head.to_owned()
    }

    /// As [`Self::mask_cut_head`], for a secret cut at the start of `tail`.
    pub(crate) fn mask_cut_tail(&self, tail: &str) -> String {
        for secret in &self.0 {
            let total = secret.value.len();
            for len in (MIN_FRAGMENT_BYTES..total).rev() {
                let Some(fragment) = secret.value.get(total.saturating_sub(len)..) else {
                    continue;
                };
                if let Some(rest) = tail.strip_prefix(fragment) {
                    return format!("{}{rest}", secret.marker());
                }
            }
        }
        tail.to_owned()
    }
}

fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .take(MAX_LABEL_CHARS)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn collect_secret_args(args: &[String], out: &mut Vec<(String, String)>) {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let Some(flag) = arg.strip_prefix('-') else {
            continue;
        };
        let flag = flag.trim_start_matches('-');
        if let Some((name, value)) = flag.split_once('=') {
            if is_secret_name(name) {
                out.push((name.to_owned(), value.to_owned()));
            }
        } else if is_secret_name(flag)
            && let Some(value) = iter.next_if(|next| !next.starts_with('-'))
        {
            out.push((flag.to_owned(), value.clone()));
        }
    }
}

fn collect_secret_json(
    value: &serde_json::Value,
    secret_key: Option<&str>,
    out: &mut Vec<(String, String)>,
) {
    match value {
        serde_json::Value::String(text) => {
            if let Some(key) = secret_key {
                out.push((key.to_owned(), text.clone()));
            }
        }
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let key_for_child = if is_secret_name(key) {
                    Some(key.as_str())
                } else {
                    secret_key
                };
                collect_secret_json(child, key_for_child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_secret_json(item, secret_key, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn redactions(pairs: &[(&str, &str)]) -> Redactions {
        Redactions::new(
            pairs
                .iter()
                .map(|(label, value)| ((*label).to_owned(), (*value).to_owned())),
        )
    }

    fn server_config() -> LspServerConfig {
        let mut config = LspServerConfig::rust_analyzer();
        config.env = HashMap::new();
        config.args = Vec::new();
        config.initialization_options = None;
        config
    }

    #[test]
    fn test_apply_finds_json_and_debug_escaped_spellings() {
        let set = redactions(&[("API_TOKEN", "pa\"ss\\word-12345")]);
        let frame = serde_json::json!({"token": "pa\"ss\\word-12345"}).to_string();
        let debug = format!("{:?}", ["pa\"ss\\word-12345"]);

        for text in [frame, debug, "raw pa\"ss\\word-12345".to_owned()] {
            let cleaned = set.apply(&text);
            assert!(cleaned.contains("[redacted:API_TOKEN]"), "{cleaned}");
            assert!(!cleaned.contains("word-12345"), "{cleaned}");
        }
    }

    #[test]
    fn test_apply_borrows_text_without_secrets() {
        let set = redactions(&[("API_TOKEN", "ghp_abcdefgh")]);

        assert!(matches!(set.apply("nothing here"), Cow::Borrowed(_)));
    }

    #[test]
    fn test_secret_name_matches_case_insensitively() {
        for name in [
            "GITHUB_TOKEN",
            "api_key",
            "MySecret",
            "DB_PASSWORD",
            "AWS_CREDENTIALS",
            "Authorization",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in ["RUSTUP_TOOLCHAIN", "JAVA_HOME", "GOFLAGS", "PATH"] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[test]
    fn test_apply_labels_the_replacement_and_skips_short_values() {
        let set = redactions(&[("GITHUB_TOKEN", "ghp_abcdefgh"), ("SHORT_KEY", "abc")]);

        assert_eq!(
            set.apply("t=ghp_abcdefgh k=abc"),
            "t=[redacted:GITHUB_TOKEN] k=abc"
        );
    }

    #[test]
    fn test_apply_replaces_the_longer_overlapping_secret_whole() {
        let set = redactions(&[("A_KEY", "abcdefgh"), ("B_KEY", "abcdefghXYZ12345")]);

        let text = set.apply("v=abcdefghXYZ12345");

        assert_eq!(text, "v=[redacted:B_KEY]");
    }

    #[test]
    fn test_new_deduplicates_equal_values() {
        let set = redactions(&[("A_TOKEN", "samesecret"), ("B_TOKEN", "samesecret")]);

        assert_eq!(set.0.len(), 1);
    }

    #[test]
    fn test_label_is_reduced_to_safe_characters() {
        let set = redactions(&[("bad]\n[label", "secretvalue")]);

        assert_eq!(set.apply("secretvalue"), "[redacted:bad___label]");
    }

    #[test]
    fn test_for_server_takes_secret_named_env_only() {
        let mut config = server_config();
        config.env = HashMap::from([
            ("API_TOKEN".to_string(), "tok-1234567890".to_string()),
            (
                "RUSTUP_TOOLCHAIN".to_string(),
                "nightly-2024-01-01".to_string(),
            ),
        ]);

        let set = Redactions::for_server(&config, []);

        let text = set.apply("tok-1234567890 nightly-2024-01-01");
        assert_eq!(text, "[redacted:API_TOKEN] nightly-2024-01-01");
    }

    #[test]
    fn test_for_server_takes_inherited_secret_env() {
        let set = Redactions::for_server(
            &server_config(),
            [
                ("GITHUB_TOKEN".to_string(), "ghp_inherited123".to_string()),
                ("HOME".to_string(), "/home/someone-long".to_string()),
            ],
        );

        assert_eq!(
            set.apply("ghp_inherited123 /home/someone-long"),
            "[redacted:GITHUB_TOKEN] /home/someone-long"
        );
    }

    #[test]
    fn test_for_server_takes_secret_flag_values_in_both_forms() {
        let mut config = server_config();
        config.args = [
            "--stdio",
            "--api-key=key-value-1",
            "--auth-token",
            "tok-value-2",
            "--verbose",
            "--log-level=trace-level",
        ]
        .map(String::from)
        .to_vec();

        let set = Redactions::for_server(&config, []);

        assert_eq!(
            set.apply("key-value-1 tok-value-2 trace-level"),
            "[redacted:api-key] [redacted:auth-token] trace-level"
        );
    }

    #[test]
    fn test_for_server_takes_string_leaves_under_secret_json_keys() {
        let mut config = server_config();
        config.initialization_options = Some(serde_json::json!({
            "server": {"name": "long-server-name", "apiToken": "init-token-value"},
            "credentials": ["first-credential", {"inner": "second-credential"}],
            "retries": 3
        }));

        let set = Redactions::for_server(&config, []);

        let text =
            set.apply("long-server-name init-token-value first-credential second-credential");
        assert_eq!(
            text,
            "long-server-name [redacted:apiToken] [redacted:credentials] [redacted:credentials]"
        );
    }

    #[test]
    fn test_mask_cut_head_hides_a_secret_prefix_at_the_boundary() {
        let set = redactions(&[("A_TOKEN", "supersecretvalue")]);

        assert_eq!(
            set.mask_cut_head("token=supersec"),
            "token=[redacted:A_TOKEN]"
        );
        assert_eq!(set.mask_cut_head("token=sup"), "token=sup");
        assert_eq!(set.mask_cut_head("nothing here"), "nothing here");
    }

    #[test]
    fn test_mask_cut_tail_hides_a_secret_suffix_at_the_boundary() {
        let set = redactions(&[("A_TOKEN", "supersecretvalue")]);

        assert_eq!(
            set.mask_cut_tail("retvalue and more"),
            "[redacted:A_TOKEN] and more"
        );
        assert_eq!(set.mask_cut_tail("lue and more"), "lue and more");
    }
}
