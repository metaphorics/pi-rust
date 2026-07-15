use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};

use crate::types::{AssistantMessage, StopReason};

const NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERNS: &[&str] = &[
    "GoUsageLimitError",
    "FreeUsageLimitError",
    "Monthly usage limit reached",
    "available balance",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
];

const RETRYABLE_PROVIDER_ERROR_PATTERNS: &[&str] = &[
    "overloaded",
    "rate.?limit",
    "too many requests",
    "429",
    "500",
    "502",
    "503",
    "504",
    "524",
    "service.?unavailable",
    "server.?error",
    "internal.?error",
    "provider.?returned.?error",
    "network.?error",
    "connection.?error",
    "connection.?refused",
    "connection.?lost",
    "other side closed",
    "fetch failed",
    "upstream.?connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed? out",
    "timeout",
    "terminated",
    "websocket.?closed",
    "websocket.?error",
    "ended without",
    "stream ended before message_stop",
    "http2 request did not get a response",
    "retry delay",
    "you can retry your request",
    "try your request again",
    "please retry your request",
    "ResourceExhausted",
];

static NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| build_provider_error_pattern(NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERNS));
static RETRYABLE_PROVIDER_ERROR_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| build_provider_error_pattern(RETRYABLE_PROVIDER_ERROR_PATTERNS));

fn build_provider_error_pattern(patterns: &[&str]) -> Regex {
    RegexBuilder::new(&patterns.join("|"))
        .case_insensitive(true)
        .build()
        .expect("pi retry regex is valid")
}

pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error) = message.error_message.as_deref() else {
        return false;
    };
    if NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN.is_match(error) {
        return false;
    }
    RETRYABLE_PROVIDER_ERROR_PATTERN.is_match(error)
}
