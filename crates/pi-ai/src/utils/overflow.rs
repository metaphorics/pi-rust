use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};

use crate::types::{AssistantMessage, StopReason};

pub const OVERFLOW_PATTERN_STRINGS: &[&str] = &[
    "prompt is too long",
    "request_too_large",
    "input is too long for requested model",
    "exceeds the context window",
    "exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\\d,]+ tokens?|\\s*\\([\\d,]+\\))",
    "input token count.*exceeds the maximum",
    "maximum prompt length is \\d+",
    "reduce the length of the messages",
    "maximum context length is \\d+ tokens",
    "exceeds (?:the )?maximum allowed input length of [\\d,]+ tokens?",
    "input \\(\\d+ tokens\\) is longer than the model'?s context length \\(\\d+ tokens\\)",
    "exceeds the limit of \\d+",
    "exceeds the available context size",
    "greater than the context length",
    "context window exceeds limit",
    "exceeded model token limit",
    "too large for model with \\d+ maximum context length",
    "prompt has [\\d,]+ tokens?, but the configured context size is [\\d,]+ tokens?",
    "model_context_window_exceeded",
    "prompt too long; exceeded (?:max )?context length",
    "context[_ ]length[_ ]exceeded",
    "too many tokens",
    "token limit exceeded",
    "^4(?:00|13)\\s*(?:status code)?\\s*\\(no body\\)",
];

const NON_OVERFLOW_PATTERN_STRINGS: &[&str] = &[
    "^(Throttling error|Service unavailable):",
    "rate limit",
    "too many requests",
];

static OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> =
    LazyLock::new(|| compile_patterns(OVERFLOW_PATTERN_STRINGS));
static NON_OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> =
    LazyLock::new(|| compile_patterns(NON_OVERFLOW_PATTERN_STRINGS));

fn compile_patterns(patterns: &[&str]) -> Vec<Regex> {
    patterns
        .iter()
        .map(|pattern| {
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .expect("pi overflow regex is valid")
        })
        .collect()
}

pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    if message.stop_reason == StopReason::Error
        && let Some(error) = message.error_message.as_deref()
        && !NON_OVERFLOW_PATTERNS
            .iter()
            .any(|pattern| pattern.is_match(error))
        && OVERFLOW_PATTERNS
            .iter()
            .any(|pattern| pattern.is_match(error))
    {
        return true;
    }

    let Some(context_window) = context_window.filter(|window| *window > 0) else {
        return false;
    };
    let input_tokens = message.usage.input + message.usage.cache_read;

    if message.stop_reason == StopReason::Stop && input_tokens > context_window {
        return true;
    }

    message.stop_reason == StopReason::Length
        && message.usage.output == 0
        && input_tokens.saturating_mul(100) >= context_window.saturating_mul(99)
}

pub fn get_overflow_patterns() -> &'static [Regex] {
    &OVERFLOW_PATTERNS
}
