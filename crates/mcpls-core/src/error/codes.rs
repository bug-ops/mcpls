//! Bespoke JSON-RPC error codes and the classification of an [`Error`] for the MCP layer.

use serde::Serialize;

#[cfg(doc)]
use super::Error;
use super::{RewrittenServerError, SymbolResolutionData};
use crate::config::ServerId;

/// Bespoke JSON-RPC code for [`Error::WorkspaceIndexing`].
///
/// Picked clear of rmcp's `-32002`/`-32020..-32022`; the range is convention, not a registry.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::WORKSPACE_INDEXING_ERROR_CODE;
///
/// assert_eq!(WORKSPACE_INDEXING_ERROR_CODE, -32050);
/// ```
pub const WORKSPACE_INDEXING_ERROR_CODE: i32 = -32050;

/// Bespoke JSON-RPC code for [`Error::ServerInitializing`].
///
/// Distinct from [`WORKSPACE_INDEXING_ERROR_CODE`] so a client can tell "the
/// server hasn't registered yet" apart from "the server registered but is
/// still indexing" -- both retryable, but for different reasons.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{SERVER_INITIALIZING_ERROR_CODE, WORKSPACE_INDEXING_ERROR_CODE};
///
/// assert_eq!(SERVER_INITIALIZING_ERROR_CODE, -32051);
/// assert_ne!(SERVER_INITIALIZING_ERROR_CODE, WORKSPACE_INDEXING_ERROR_CODE);
/// ```
pub const SERVER_INITIALIZING_ERROR_CODE: i32 = -32051;

/// Bespoke JSON-RPC code for a resource subscription request rejected
/// because it was served over rmcp's stateless per-request HTTP path (#482).
///
/// Same convention range as [`WORKSPACE_INDEXING_ERROR_CODE`]/
/// [`SERVER_INITIALIZING_ERROR_CODE`], next unused slot.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{
///     SERVER_INITIALIZING_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE,
///     WORKSPACE_INDEXING_ERROR_CODE,
/// };
///
/// assert_eq!(STATELESS_SUBSCRIPTION_ERROR_CODE, -32052);
/// assert_ne!(STATELESS_SUBSCRIPTION_ERROR_CODE, WORKSPACE_INDEXING_ERROR_CODE);
/// assert_ne!(STATELESS_SUBSCRIPTION_ERROR_CODE, SERVER_INITIALIZING_ERROR_CODE);
/// ```
pub const STATELESS_SUBSCRIPTION_ERROR_CODE: i32 = -32052;

/// Bespoke JSON-RPC code for [`Error::ListenStreamsExhausted`].
///
/// Same convention range as [`WORKSPACE_INDEXING_ERROR_CODE`], next unused
/// slot after [`STATELESS_SUBSCRIPTION_ERROR_CODE`].
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{
///     LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE,
/// };
///
/// assert_eq!(LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, -32053);
/// assert_ne!(LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE);
/// ```
pub const LISTEN_STREAMS_EXHAUSTED_ERROR_CODE: i32 = -32053;

/// Bespoke JSON-RPC code for [`Error::ServerRestarted`].
///
/// Same convention range as [`WORKSPACE_INDEXING_ERROR_CODE`], next unused
/// slot after [`LISTEN_STREAMS_EXHAUSTED_ERROR_CODE`].
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, SERVER_RESTARTED_ERROR_CODE};
///
/// assert_eq!(SERVER_RESTARTED_ERROR_CODE, -32054);
/// assert_ne!(SERVER_RESTARTED_ERROR_CODE, LISTEN_STREAMS_EXHAUSTED_ERROR_CODE);
/// ```
pub const SERVER_RESTARTED_ERROR_CODE: i32 = -32054;

/// Structured `data` payload of a retryable JSON-RPC error.
///
/// Each variant pairs a bespoke error code ([`Self::code`]) with its own
/// payload, so a code can never be sent with another variant's data. Keys
/// are `snake_case`; the wire form is the variant's fields as a bare object.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ServerId;
/// use mcpls_core::error::{RetryableErrorData, WORKSPACE_INDEXING_ERROR_CODE};
///
/// let data = RetryableErrorData::WorkspaceIndexing {
///     server_id: ServerId::from_static("rust"),
///     elapsed_secs: 30,
/// };
/// assert_eq!(data.code(), WORKSPACE_INDEXING_ERROR_CODE);
/// assert_eq!(
///     serde_json::to_value(&data).unwrap(),
///     serde_json::json!({"server_id": "rust", "elapsed_secs": 30})
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum RetryableErrorData {
    /// The routed server is still indexing the workspace.
    WorkspaceIndexing {
        /// Server that is indexing.
        server_id: ServerId,
        /// Seconds spent waiting for indexing to finish before giving up.
        elapsed_secs: u64,
    },
    /// The routed server has not finished initializing.
    ServerInitializing {
        /// Server that is initializing.
        server_id: ServerId,
    },
    /// An expected server has not registered yet and no single candidate
    /// could be narrowed down, so there is no server to name. The braces
    /// keep the wire value an empty object rather than `null`.
    WorkspaceServersInitializing {},
    /// The concurrent `subscriptions/listen` stream limit is reached.
    ListenStreamsExhausted {
        /// Maximum number of concurrent listen streams.
        max_listen_streams: usize,
    },
    /// The routed server was restarted while the request was in flight.
    ServerRestarted {
        /// Server that was restarted.
        server_id: ServerId,
    },
}

impl RetryableErrorData {
    /// The bespoke JSON-RPC error code for this retryable condition.
    #[must_use]
    pub const fn code(&self) -> i32 {
        match self {
            Self::WorkspaceIndexing { .. } => WORKSPACE_INDEXING_ERROR_CODE,
            Self::ServerInitializing { .. } | Self::WorkspaceServersInitializing {} => {
                SERVER_INITIALIZING_ERROR_CODE
            }
            Self::ListenStreamsExhausted { .. } => LISTEN_STREAMS_EXHAUSTED_ERROR_CODE,
            Self::ServerRestarted { .. } => SERVER_RESTARTED_ERROR_CODE,
        }
    }
}

/// JSON-RPC error-code classification for an [`Error`], returned by
/// [`Error::mcp_error_kind`].
///
/// mcpls-core has no dependency on the MCP transport crate, so this carries
/// only plain data; `crate::mcp` is responsible for turning it into the
/// actual wire-level error type.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{Error, McpErrorKind};
///
/// let err = Error::InvalidToolParams("missing `file_path`".to_string());
/// assert_eq!(err.mcp_error_kind(), McpErrorKind::InvalidParams);
/// ```
///
/// This enum is `#[non_exhaustive]`: match it with a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpErrorKind {
    /// Caller-fault: the request itself was invalid. Maps to JSON-RPC
    /// `-32602` (`INVALID_PARAMS`).
    InvalidParams,
    /// Caller-fault: the position fell outside the document and the server's
    /// raw error was rewritten for display. Maps to `-32602` with the
    /// original error as `data`.
    InvalidPosition(RewrittenServerError),
    /// Caller-fault: a symbol name did not resolve to exactly one position.
    /// Maps to `-32602` with the structured resolution outcome as `data`.
    SymbolResolution(SymbolResolutionData),
    /// A transient, retryable server-side condition, distinct from a crash.
    /// Maps to a bespoke JSON-RPC `code` with a structured `data` payload a
    /// caller can act on mechanically, rather than the generic
    /// `INTERNAL_ERROR`.
    Retryable(RetryableErrorData),
    /// An unexpected server-side failure. Maps to JSON-RPC `-32603`
    /// (`INTERNAL_ERROR`).
    Internal,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::bridge::resources::ResourceUriError;
    use crate::bridge::{Capability, InvalidClientPath};
    use crate::config::LanguageId;
    use crate::error::tests::no_server_for_language;
    use crate::error::{BackgroundTask, Error, StdioStream};
    use crate::lsp::JsonRpcError;
    use crate::util::SizeExceeded;

    /// #479: caller-fault variants must classify as `InvalidParams`, not fall
    /// through to the generic `Internal` bucket.
    #[test]
    fn test_mcp_error_kind_caller_fault_variants_are_invalid_params() {
        let caller_fault_errors = vec![
            Error::InvalidToolParams("bad params".to_string()),
            Error::PathOutsideWorkspace(PathBuf::from("/etc/passwd")),
            Error::NotARegularFile(PathBuf::from("/dev/null")),
            Error::NoResolvableListenUris,
            Error::ResourceUri(ResourceUriError::InvalidScheme("x".to_string())),
            Error::DocumentNotFound(PathBuf::from("/missing.rs")),
            Error::FileSizeLimitExceeded(SizeExceeded {
                size: 100,
                max: std::num::NonZeroU64::new(10).unwrap(),
            }),
            Error::ListenFilterTooLarge { max: 1000 },
            Error::InvalidClientPath(InvalidClientPath::ContainsNul),
        ];

        for err in caller_fault_errors {
            assert_eq!(
                err.mcp_error_kind(),
                McpErrorKind::InvalidParams,
                "expected {err:?} to classify as InvalidParams"
            );
        }
    }

    #[tokio::test]
    async fn test_mcp_error_kind_internal_failures_are_internal() {
        let join_error = tokio::spawn(async { panic!("boom") }).await.unwrap_err();
        let internal_errors = vec![
            Error::TaskFailed {
                task: BackgroundTask::McpService,
                source: join_error,
            },
            Error::StdioCapture(StdioStream::Stdin),
            Error::PathToUri(PathBuf::from("/x")),
            Error::McpServerStart("handshake".into()),
        ];

        for err in internal_errors {
            assert_eq!(
                err.mcp_error_kind(),
                McpErrorKind::Internal,
                "expected {err:?} to classify as Internal"
            );
        }
    }

    #[test]
    fn test_mcp_error_kind_workspace_indexing_is_retryable_with_dedicated_code() {
        let err = Error::WorkspaceIndexing {
            server_id: ServerId::from_static("rust"),
            elapsed_secs: 30,
        };
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected WorkspaceIndexing to classify as Retryable");
        };
        assert_eq!(data.code(), WORKSPACE_INDEXING_ERROR_CODE);
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({"server_id": "rust", "elapsed_secs": 30})
        );
    }

    #[test]
    fn test_mcp_error_kind_server_initializing_is_retryable_with_dedicated_code() {
        let err = Error::ServerInitializing {
            server_id: ServerId::from_static("python"),
        };
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected ServerInitializing to classify as Retryable");
        };
        assert_eq!(data.code(), SERVER_INITIALIZING_ERROR_CODE);
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({"server_id": "python"})
        );
        assert_ne!(
            data.code(),
            WORKSPACE_INDEXING_ERROR_CODE,
            "ServerInitializing must be distinguishable on the wire from WorkspaceIndexing"
        );
    }

    /// `WorkspaceServersInitializing` is `ServerInitializing`'s counterpart
    /// for a resolution that never narrowed down to a single server (see the
    /// variant's doc comment), so it must be retryable too -- a client that
    /// auto-retries on the bespoke retryable code must not treat this as a
    /// hard failure just because no `server_id` was available.
    #[test]
    fn test_mcp_error_kind_workspace_servers_initializing_is_retryable() {
        let err = Error::WorkspaceServersInitializing;
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected WorkspaceServersInitializing to classify as Retryable");
        };
        assert_eq!(data.code(), SERVER_INITIALIZING_ERROR_CODE);
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({}),
            "the empty struct variant must serialize as `{{}}`, not `null`"
        );
    }

    #[test]
    fn test_retryable_error_data_codes_and_snake_case_keys() {
        let cases = [
            (
                RetryableErrorData::WorkspaceIndexing {
                    server_id: ServerId::from_static("rust"),
                    elapsed_secs: 7,
                },
                WORKSPACE_INDEXING_ERROR_CODE,
                serde_json::json!({"server_id": "rust", "elapsed_secs": 7}),
            ),
            (
                RetryableErrorData::ServerInitializing {
                    server_id: ServerId::from_static("python"),
                },
                SERVER_INITIALIZING_ERROR_CODE,
                serde_json::json!({"server_id": "python"}),
            ),
            (
                RetryableErrorData::WorkspaceServersInitializing {},
                SERVER_INITIALIZING_ERROR_CODE,
                serde_json::json!({}),
            ),
            (
                RetryableErrorData::ListenStreamsExhausted {
                    max_listen_streams: 100,
                },
                LISTEN_STREAMS_EXHAUSTED_ERROR_CODE,
                serde_json::json!({"max_listen_streams": 100}),
            ),
        ];
        for (data, code, wire) in cases {
            assert_eq!(data.code(), code);
            assert_eq!(serde_json::to_value(&data).unwrap(), wire);
        }
    }

    #[test]
    fn test_mcp_error_kind_listen_streams_exhausted_is_retryable() {
        let err = Error::ListenStreamsExhausted { max: 100 };
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected ListenStreamsExhausted to classify as Retryable");
        };
        assert_eq!(data.code(), LISTEN_STREAMS_EXHAUSTED_ERROR_CODE);
        assert_eq!(
            data,
            RetryableErrorData::ListenStreamsExhausted {
                max_listen_streams: 100
            }
        );
    }

    #[test]
    fn test_mcp_error_kind_unretained_variants_stay_internal() {
        let internal_errors = vec![
            no_server_for_language("python"),
            Error::NoServerForTool {
                language_id: LanguageId::from_static("rust"),
                tool: crate::config::ToolKind::Hover,
            },
            Error::CapabilityNotSupported {
                server_id: ServerId::from_static("rust"),
                capability: Capability::Rename,
            },
            Error::NoWorkspaceRoots(PathBuf::from("/tmp")),
            Error::DocumentLimitExceeded {
                current: 150,
                max: std::num::NonZeroUsize::new(100).unwrap(),
            },
            Error::SubscriptionLimitReached { max: 1000 },
        ];

        for err in internal_errors {
            assert_eq!(
                err.mcp_error_kind(),
                McpErrorKind::Internal,
                "expected {err:?} to classify as Internal"
            );
        }
    }

    /// #479 regression: a client-supplied path that doesn't exist (the
    /// common case behind `WorkspaceRoots::validate`'s `canonicalize()`
    /// failure) must classify as caller-fault, matching `DocumentNotFound`.
    #[test]
    fn test_mcp_error_kind_file_io_not_found_is_invalid_params() {
        let err = Error::FileIo {
            path: PathBuf::from("/no/such/file.rs"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file or directory"),
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::InvalidParams);
    }

    #[test]
    fn test_position_beyond_document_is_invalid_params_naming_the_line() {
        let err = Error::PositionBeyondDocument {
            line: std::num::NonZeroU32::new(7).unwrap(),
            last_line: std::num::NonZeroU32::new(3).unwrap(),
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::InvalidParams);
        assert_eq!(
            err.to_string(),
            "line 7 is beyond the end of the document (the document ends at line 3)"
        );
    }

    /// #575: a malformed client path is caller-fault, whatever IO kind the
    /// validation hit.
    #[test]
    fn test_mcp_error_kind_malformed_path_is_invalid_params() {
        use std::io::ErrorKind;

        for kind in [
            ErrorKind::NotADirectory,
            ErrorKind::InvalidFilename,
            ErrorKind::InvalidInput,
        ] {
            let err = Error::MalformedPath {
                path: PathBuf::from("/ws/main.rs/x"),
                source: std::io::Error::new(kind, "bad path"),
            };
            assert_eq!(
                err.mcp_error_kind(),
                McpErrorKind::InvalidParams,
                "{kind:?}"
            );
        }
    }

    /// The same IO kinds from a post-validation read or open are
    /// environmental, not caller-fault.
    #[test]
    fn test_mcp_error_kind_post_validation_file_io_invalid_input_stays_internal() {
        let err = Error::FileIo {
            path: PathBuf::from("/ws/main.rs"),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad input"),
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    /// Counterpart: a non-not-found IO failure (permission denied, etc.) is
    /// a genuine server-side problem, not something the caller can fix by
    /// changing their request.
    #[test]
    fn test_mcp_error_kind_file_io_other_kind_stays_internal() {
        let err = Error::FileIo {
            path: PathBuf::from("/root/secret.rs"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied"),
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    /// #496: an `LspServerError` carrying the recognized "position out of
    /// range" shape (same substring `sanitize_lsp_server_message` rewrites
    /// for display) is caller-fault, not a generic internal failure.
    #[test]
    fn test_mcp_error_kind_lsp_server_error_invalid_offset_is_invalid_params() {
        let err = Error::LspServerError(JsonRpcError {
            code: -32603,
            message: "Invalid offset LineCol { line: 2291, col: 0 } (line index length: 100417)"
                .to_string(),
            data: None,
        });
        assert_eq!(
            err.mcp_error_kind(),
            McpErrorKind::InvalidPosition(RewrittenServerError::new(
                -32603,
                "Invalid offset LineCol { line: 2291, col: 0 } (line index length: 100417)"
            ))
        );
    }

    /// Counterpart: an `LspServerError` whose message doesn't match the
    /// recognized position-out-of-range shape is a genuine server-side
    /// problem and must stay `Internal`.
    #[test]
    fn test_mcp_error_kind_lsp_server_error_other_message_stays_internal() {
        let err = Error::LspServerError(JsonRpcError {
            code: -32603,
            message: "internal error".to_string(),
            data: None,
        });
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    /// #496: `SubscriptionLimitReached` fires on aggregate per-session
    /// tracker state, not this request's params, so it must classify the
    /// same way as `DocumentLimitExceeded` -- not `InvalidParams`.
    #[test]
    fn test_mcp_error_kind_subscription_limit_reached_stays_internal() {
        let err = Error::SubscriptionLimitReached { max: 1000 };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }
}
