//! Whether a server asked for `textDocument/didSave`, and with what content.
//!
//! The sweep sends `didSave` after it resyncs a changed file, so servers that
//! produce diagnostics only on save (`rust-analyzer`'s flycheck) re-run. The
//! notification is sent only when the server advertises it in
//! `TextDocumentSyncOptions.save`, as the specification requires.

use lsp_types::{Save, TextDocumentSync};

/// What a server's `textDocumentSync` capability says about `didSave`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SaveSupport {
    /// The server did not ask for `didSave`: nothing is sent.
    NotAdvertised,
    /// The server wants `didSave` without the document text.
    WithoutText,
    /// The server wants `didSave` with the document text.
    WithText,
}

impl SaveSupport {
    /// The support a `textDocumentSync` capability advertises.
    ///
    /// The bare `TextDocumentSyncKind` form and an absent capability carry no
    /// save option, so they advertise nothing.
    pub(super) const fn of(sync: Option<&TextDocumentSync>) -> Self {
        match sync {
            Some(TextDocumentSync::Options(options)) => match &options.save {
                Some(Save::Bool(true)) => Self::WithoutText,
                Some(Save::SaveOptions(save)) => match save.include_text {
                    Some(true) => Self::WithText,
                    Some(false) | None => Self::WithoutText,
                },
                Some(Save::Bool(false)) | None => Self::NotAdvertised,
            },
            Some(TextDocumentSync::Kind(_)) | None => Self::NotAdvertised,
        }
    }
}

#[cfg(test)]
mod tests {
    use lsp_types::{SaveOptions, TextDocumentSyncKind, TextDocumentSyncOptions};

    use super::*;

    fn with_save(save: Option<Save>) -> TextDocumentSync {
        TextDocumentSync::Options(TextDocumentSyncOptions {
            save,
            ..TextDocumentSyncOptions::default()
        })
    }

    #[test]
    fn test_only_an_options_form_with_a_save_entry_advertises_did_save() {
        let kind = TextDocumentSync::Kind(TextDocumentSyncKind::Full);
        assert_eq!(SaveSupport::of(None), SaveSupport::NotAdvertised);
        assert_eq!(SaveSupport::of(Some(&kind)), SaveSupport::NotAdvertised);
        assert_eq!(
            SaveSupport::of(Some(&with_save(None))),
            SaveSupport::NotAdvertised
        );
        assert_eq!(
            SaveSupport::of(Some(&with_save(Some(Save::Bool(false))))),
            SaveSupport::NotAdvertised
        );
    }

    #[test]
    fn test_include_text_selects_the_text_form() {
        let options =
            |include_text| with_save(Some(Save::SaveOptions(SaveOptions { include_text })));
        assert_eq!(
            SaveSupport::of(Some(&with_save(Some(Save::Bool(true))))),
            SaveSupport::WithoutText
        );
        assert_eq!(
            SaveSupport::of(Some(&options(None))),
            SaveSupport::WithoutText
        );
        assert_eq!(
            SaveSupport::of(Some(&options(Some(false)))),
            SaveSupport::WithoutText
        );
        assert_eq!(
            SaveSupport::of(Some(&options(Some(true)))),
            SaveSupport::WithText
        );
    }
}
