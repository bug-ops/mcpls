//! The answer to a request the protocol layer could not decode.
//!
//! A request whose params do not decode into its typed form reaches the
//! server as a [`CustomRequest`], whose default answer is "method not found"
//! with the method name. That is wrong for a method mcpls serves (the client
//! sent a malformed request, not an unknown method) and echoes a client string
//! at full length. [`answer`] fixes both.
//!
//! Only the methods mcpls advertises and implements count as served. The SDK
//! also answers `completion/complete` and `prompts/list` with empty results,
//! but mcpls advertises neither capability, so a malformed request for them
//! stays "method not found".

use std::borrow::Cow;

use rmcp::ErrorData as McpError;
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, ConstString as _, CustomRequest,
    DiscoverRequestMethod, DiscoverRequestParams, ErrorCode, InitializeRequestParams,
    InitializeResultMethod, JsonObject, ListResourceTemplatesRequestMethod,
    ListResourcesRequestMethod, ListToolsRequestMethod, PaginatedRequestParams, PingRequestMethod,
    ReadResourceRequestMethod, ReadResourceRequestParams, SubscribeRequestMethod,
    SubscribeRequestParams, SubscriptionsListenRequestMethod, SubscriptionsListenRequestParams,
    UnsubscribeRequestMethod, UnsubscribeRequestParams,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::util::{MAX_ECHOED_NAME_BYTES, MAX_ERROR_MESSAGE_CALLER_BYTES, escape_bounded};

/// A method mcpls serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Served {
    Initialize,
    Ping,
    Discover,
    ToolsList,
    ToolsCall,
    ResourcesList,
    ResourcesTemplatesList,
    ResourcesRead,
    ResourcesSubscribe,
    ResourcesUnsubscribe,
    SubscriptionsListen,
}

/// The sessions a served method is available on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Any,
    /// Sessions opened with `initialize`.
    LegacyOnly,
    /// Sessions of protocol version 2026-07-28 and later.
    ModernOnly,
}

impl Served {
    const ALL: [Self; 11] = [
        Self::Initialize,
        Self::Ping,
        Self::Discover,
        Self::ToolsList,
        Self::ToolsCall,
        Self::ResourcesList,
        Self::ResourcesTemplatesList,
        Self::ResourcesRead,
        Self::ResourcesSubscribe,
        Self::ResourcesUnsubscribe,
        Self::SubscriptionsListen,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Initialize => InitializeResultMethod::VALUE,
            Self::Ping => PingRequestMethod::VALUE,
            Self::Discover => DiscoverRequestMethod::VALUE,
            Self::ToolsList => ListToolsRequestMethod::VALUE,
            Self::ToolsCall => CallToolRequestMethod::VALUE,
            Self::ResourcesList => ListResourcesRequestMethod::VALUE,
            Self::ResourcesTemplatesList => ListResourceTemplatesRequestMethod::VALUE,
            Self::ResourcesRead => ReadResourceRequestMethod::VALUE,
            Self::ResourcesSubscribe => SubscribeRequestMethod::VALUE,
            Self::ResourcesUnsubscribe => UnsubscribeRequestMethod::VALUE,
            Self::SubscriptionsListen => SubscriptionsListenRequestMethod::VALUE,
        }
    }

    fn of(method: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|served| served.name() == method)
    }

    const fn lifecycle(self) -> Lifecycle {
        match self {
            Self::Ping | Self::ResourcesSubscribe | Self::ResourcesUnsubscribe => {
                Lifecycle::LegacyOnly
            }
            Self::SubscriptionsListen => Lifecycle::ModernOnly,
            Self::Initialize
            | Self::Discover
            | Self::ToolsList
            | Self::ToolsCall
            | Self::ResourcesList
            | Self::ResourcesTemplatesList
            | Self::ResourcesRead => Lifecycle::Any,
        }
    }

    const fn available(self, legacy: bool) -> bool {
        match self.lifecycle() {
            Lifecycle::Any => true,
            Lifecycle::LegacyOnly => legacy,
            Lifecycle::ModernOnly => !legacy,
        }
    }

    /// Whether `params` decode into the method's typed params.
    fn decode(self, params: Option<&Value>) -> Result<(), serde_json::Error> {
        fn check<P: DeserializeOwned>(params: Option<&Value>) -> Result<(), serde_json::Error> {
            P::deserialize(params.unwrap_or(&Value::Null)).map(drop)
        }
        match self {
            Self::Initialize => check::<InitializeRequestParams>(params),
            Self::Ping => check::<Option<JsonObject>>(params),
            Self::Discover => check::<DiscoverRequestParams>(params),
            Self::ToolsList | Self::ResourcesList | Self::ResourcesTemplatesList => {
                check::<Option<PaginatedRequestParams>>(params)
            }
            Self::ToolsCall => check::<CallToolRequestParams>(params),
            Self::ResourcesRead => check::<ReadResourceRequestParams>(params),
            Self::ResourcesSubscribe => check::<SubscribeRequestParams>(params),
            Self::ResourcesUnsubscribe => check::<UnsubscribeRequestParams>(params),
            Self::SubscriptionsListen => check::<SubscriptionsListenRequestParams>(params),
        }
    }
}

fn method_not_found(method: &str) -> McpError {
    McpError::new(
        ErrorCode::METHOD_NOT_FOUND,
        Cow::Owned(escape_bounded(method, MAX_ECHOED_NAME_BYTES)),
        None,
    )
}

/// The error a request that fell through to the custom-request hook gets.
///
/// `legacy` says whether the session was opened with `initialize`; the
/// lifecycle-gated methods (`ping`, `resources/subscribe` and
/// `resources/unsubscribe` on legacy sessions, `subscriptions/listen` on later
/// ones) answer "method not found" on the other kind, as the typed path does.
///
/// - A method mcpls does not serve is "method not found" with the method name
///   escaped and bounded.
/// - A served method whose params do not decode is "invalid params", naming
///   the method and the bounded decoding error.
/// - A served method whose params decode when read as a plain value, yet whose
///   request does not (a duplicate field is the case that gets here), is
///   "invalid params" with a fixed text.
pub(super) fn answer(request: &CustomRequest, legacy: bool) -> McpError {
    let Some(served) = Served::of(&request.method) else {
        return method_not_found(&request.method);
    };
    if !served.available(legacy) {
        return method_not_found(&request.method);
    }
    let name = served.name();
    match served.decode(request.params.as_ref()) {
        Err(error) => McpError::invalid_params(
            format!(
                "invalid params for `{name}`: {}",
                escape_bounded(&error.to_string(), MAX_ERROR_MESSAGE_CALLER_BYTES)
            ),
            None,
        ),
        Ok(()) => McpError::invalid_params(
            format!("invalid params for `{name}`: the params are malformed (duplicate field)"),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::util::TRUNCATION_MARKER;

    fn request(method: &str, params: Option<Value>) -> CustomRequest {
        CustomRequest::new(method, params)
    }

    fn code(error: &McpError) -> ErrorCode {
        error.code
    }

    #[test]
    fn malformed_params_of_a_served_method_are_invalid_params() {
        for (method, params) in [
            ("tools/call", Some(json!([]))),
            ("tools/call", Some(json!({"name": 5}))),
            ("tools/call", Some(json!({}))),
            ("tools/call", None),
            ("resources/read", Some(json!({}))),
            ("resources/read", Some(json!({"uri": 5}))),
            ("resources/subscribe", Some(json!({}))),
            ("tools/list", Some(json!(5))),
            ("resources/list", Some(json!("x"))),
            ("resources/templates/list", Some(json!(5))),
            ("ping", Some(json!(5))),
            ("initialize", Some(json!({}))),
        ] {
            let error = answer(&request(method, params), true);
            assert_eq!(code(&error), ErrorCode::INVALID_PARAMS, "{method}");
            assert!(
                error
                    .message
                    .starts_with(&format!("invalid params for `{method}`: ")),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn a_method_mcpls_does_not_serve_is_not_found() {
        let error = answer(&request("foo/bar", None), true);
        assert_eq!(code(&error), ErrorCode::METHOD_NOT_FOUND);
        assert_eq!(error.message, "foo/bar");
    }

    #[test]
    fn lifecycle_gated_methods_are_not_found_on_the_other_kind_of_session() {
        let subscribe = request("resources/subscribe", Some(json!({})));
        assert_eq!(code(&answer(&subscribe, true)), ErrorCode::INVALID_PARAMS);
        assert_eq!(
            code(&answer(&subscribe, false)),
            ErrorCode::METHOD_NOT_FOUND
        );

        let listen = request("subscriptions/listen", Some(json!({})));
        assert_eq!(code(&answer(&listen, true)), ErrorCode::METHOD_NOT_FOUND);
        assert_eq!(code(&answer(&listen, false)), ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn params_that_decode_get_a_fixed_text() {
        let initialize = json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "c", "version": "1"},
        });
        for (method, params) in [
            ("tools/list", None),
            ("initialize", Some(initialize)),
            ("tools/call", Some(json!({"name": "get_hover"}))),
        ] {
            let error = answer(&request(method, params), true);
            assert_eq!(code(&error), ErrorCode::INVALID_PARAMS, "{method}");
            assert!(
                error.message.ends_with("(duplicate field)"),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn methods_the_sdk_answers_but_mcpls_does_not_advertise_stay_not_found() {
        for method in ["completion/complete", "prompts/list"] {
            let error = answer(&request(method, Some(json!({}))), true);
            assert_eq!(code(&error), ErrorCode::METHOD_NOT_FOUND, "{method}");
        }
    }

    #[test]
    fn ping_is_served_on_legacy_sessions_only() {
        let ping = request("ping", Some(json!(5)));
        assert_eq!(code(&answer(&ping, true)), ErrorCode::INVALID_PARAMS);
        assert_eq!(code(&answer(&ping, false)), ErrorCode::METHOD_NOT_FOUND);
    }

    #[test]
    fn echoed_text_is_bounded() {
        let long = "Z".repeat(1 << 20);

        let unknown = answer(&request(&format!("x/{long}"), None), true);
        assert_eq!(code(&unknown), ErrorCode::METHOD_NOT_FOUND);
        assert!(unknown.message.len() <= MAX_ECHOED_NAME_BYTES + TRUNCATION_MARKER.len());
        assert!(unknown.message.ends_with(TRUNCATION_MARKER));

        let invalid = answer(&request("tools/call", Some(json!({"name": long}))), true);
        assert!(
            invalid.message.len() <= 2 * MAX_ERROR_MESSAGE_CALLER_BYTES,
            "{}",
            invalid.message.len()
        );
        let invalid = answer(&request("tools/call", Some(json!(long))), true);
        assert_eq!(code(&invalid), ErrorCode::INVALID_PARAMS);
        assert!(
            invalid.message.len() <= 2 * MAX_ERROR_MESSAGE_CALLER_BYTES,
            "{}",
            invalid.message.len()
        );
    }
}
