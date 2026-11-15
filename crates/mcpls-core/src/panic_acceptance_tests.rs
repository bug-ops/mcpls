//! Acceptance tests for #918: a task panic payload is escaped and bounded once,
//! where `TaskPanicked` is built, so every log site that prints it is covered.

use std::time::Duration;

use tracing_subscriber::prelude::*;

use crate::util::{
    MAX_LOG_STRING_BYTES, RestartBackoff, RestartDelays, TRUNCATION_MARKER, catch_panic,
    escape_bounded,
};

#[tokio::test]
async fn test_task_panicked_message_escapes_control_and_bidi_chars() {
    let panicked = catch_panic(async { panic!("a\nfile:///x\u{202E}") })
        .await
        .unwrap_err();

    assert_eq!(panicked.message(), "a\\nfile:///x\\u{202e}");
}

#[tokio::test]
async fn test_task_panicked_message_escapes_a_formatted_string_payload() {
    let detail = "b\r\nERROR forged";
    let panicked = catch_panic(async move { panic!("{detail}") })
        .await
        .unwrap_err();

    assert_eq!(panicked.message(), "b\\r\\nERROR forged");
}

#[tokio::test]
async fn test_task_panicked_message_is_bounded_with_exactly_one_marker() {
    let raw = "x".repeat(1024);
    let expected = escape_bounded(&raw, MAX_LOG_STRING_BYTES);
    let panicked = catch_panic(async move { panic!("{raw}") })
        .await
        .unwrap_err();

    assert_eq!(panicked.message(), expected);
    assert_eq!(
        panicked.message().len(),
        MAX_LOG_STRING_BYTES + TRUNCATION_MARKER.len()
    );
    assert_eq!(panicked.message().matches(TRUNCATION_MARKER).count(), 1);
    assert!(panicked.message().ends_with(TRUNCATION_MARKER));
}

#[tokio::test]
async fn test_task_panicked_message_names_a_non_string_payload() {
    let panicked = catch_panic(async { std::panic::panic_any(7_u32) })
        .await
        .unwrap_err();

    assert_eq!(panicked.message(), "non-string panic payload");
}

// regression for #918 (M1): a second cut in `after_panic` would append a second marker.
#[tokio::test(start_paused = true)]
async fn test_after_panic_logs_a_long_payload_with_a_single_marker() {
    let logs = crate::test_lsp::CapturedLogs::default();
    let subscriber = tracing_subscriber::registry().with(logs.clone());
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let raw = "y".repeat(1024);
    let panicked = catch_panic(async move { panic!("{raw}") })
        .await
        .unwrap_err();
    let mut backoff = RestartBackoff::new(RestartDelays {
        first: Duration::from_secs(1),
        max: Duration::from_secs(8),
    });
    let _guard = tracing::subscriber::set_default(subscriber);

    let flow = backoff
        .after_panic(
            crate::error::BackgroundTask::SessionDelivery,
            &panicked,
            tokio::time::Instant::now(),
            &cancel,
        )
        .await;

    assert!(flow.is_break());
    let fields = logs.fields_at(tracing::Level::ERROR);
    let error = fields
        .iter()
        .find(|f| f.starts_with("panic="))
        .expect("panic field");
    assert_eq!(error.matches(TRUNCATION_MARKER).count(), 1, "{error}");
}

#[test]
fn test_panic_message_escapes_bounds_and_names_non_string_payloads() {
    let long: Box<dyn std::any::Any + Send> = Box::new("a\n".repeat(500));
    let message = crate::PanicMessage::from_payload(long.as_ref());
    assert!(!message.as_str().contains('\n'));
    assert!(message.as_str().len() <= MAX_LOG_STRING_BYTES + TRUNCATION_MARKER.len());
    assert!(message.as_str().ends_with(TRUNCATION_MARKER));

    let other: Box<dyn std::any::Any + Send> = Box::new(7_u32);
    assert_eq!(
        crate::PanicMessage::from_payload(other.as_ref()).to_string(),
        "non-string panic payload"
    );
}

#[tokio::test]
async fn test_panic_message_from_join_error_is_some_only_for_a_panic() {
    let panicked = tokio::spawn(async { panic!("a\nforged") })
        .await
        .unwrap_err();
    let message = crate::PanicMessage::from_join_error(panicked).expect("a panic");
    assert_eq!(message.as_str(), "a\\nforged");

    let handle = tokio::spawn(std::future::pending::<()>());
    handle.abort();
    let cancelled = handle.await.unwrap_err();
    assert!(crate::PanicMessage::from_join_error(cancelled).is_none());
}
