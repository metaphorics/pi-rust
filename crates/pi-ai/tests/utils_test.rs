use pi_ai::{
    Api, AssistantMessage, StopReason, Usage,
    utils::{get_overflow_patterns, is_context_overflow, is_retryable_assistant_error, sanitize_surrogates},
};

fn message(reason: StopReason, error: Option<&str>) -> AssistantMessage {
    AssistantMessage {
        content: vec![],
        api: Api::from("test"),
        provider: "test".into(),
        model: "test".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: reason,
        error_message: error.map(str::to_owned),
        timestamp: 0,
    }
}

#[test]
fn detects_provider_overflow_but_excludes_throttling() {
    assert!(is_context_overflow(
        &message(StopReason::Error, Some("prompt is too long: 200001 tokens")),
        None
    ));
    assert!(!is_context_overflow(
        &message(StopReason::Error, Some("Throttling error: too many tokens")),
        None
    ));
    assert_eq!(get_overflow_patterns().len(), 24);
}

#[test]
fn detects_silent_and_length_stop_overflow() {
    let mut silent = message(StopReason::Stop, None);
    silent.usage.input = 101;
    assert!(is_context_overflow(&silent, Some(100)));

    let mut truncated = message(StopReason::Length, None);
    truncated.usage.input = 99;
    assert!(is_context_overflow(&truncated, Some(100)));
}

#[test]
fn retry_classifier_prioritizes_permanent_limits() {
    assert!(is_retryable_assistant_error(&message(
        StopReason::Error,
        Some("503 provider returned error")
    )));
    assert!(!is_retryable_assistant_error(&message(
        StopReason::Error,
        Some("429 insufficient_quota")
    )));
}

#[test]
fn valid_unicode_is_already_sanitized_by_rust_string_invariants() {
    assert_eq!(sanitize_surrogates("Hello 🙈 World"), "Hello 🙈 World");
}
