use crate::types::{Context, Model, StreamOptions, ThinkingBudgets, ThinkingLevel};

const CONTEXT_SAFETY_TOKENS: u64 = 4096;

pub fn estimate_context_tokens(context: &Context) -> u64 {
    let bytes = serde_json::to_vec(context).map_or(0, |value| value.len() as u64);
    bytes.div_ceil(4)
}

pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: u64) -> u64 {
    if model.context_window == 0 { return max_tokens.max(1); }
    let available = model.context_window.saturating_sub(estimate_context_tokens(context)).saturating_sub(CONTEXT_SAFETY_TOKENS).max(1);
    max_tokens.min(available)
}

pub fn build_base_options(model: &Model, context: &Context, options: Option<&StreamOptions>, api_key: Option<String>) -> StreamOptions {
    let mut result = options.cloned().unwrap_or_default();
    result.max_tokens = Some(clamp_max_tokens_to_context(model, context, result.max_tokens.unwrap_or(model.max_tokens)));
    if api_key.is_some() { result.api_key = api_key; }
    result
}

pub fn adjust_max_tokens_for_thinking(base_max_tokens: Option<u64>, model_max_tokens: u64, level: ThinkingLevel, custom: Option<&ThinkingBudgets>) -> (u64, u64) {
    let defaults = ThinkingBudgets { minimal: Some(1024), low: Some(2048), medium: Some(8192), high: Some(16384) };
    let budget = match level {
        ThinkingLevel::Minimal => custom.and_then(|v| v.minimal).or(defaults.minimal),
        ThinkingLevel::Low => custom.and_then(|v| v.low).or(defaults.low),
        ThinkingLevel::Medium => custom.and_then(|v| v.medium).or(defaults.medium),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => custom.and_then(|v| v.high).or(defaults.high),
    }.unwrap_or(1024);
    let max_tokens = base_max_tokens.map_or(model_max_tokens, |base| base.saturating_add(budget).min(model_max_tokens));
    (max_tokens, budget.min(max_tokens.saturating_sub(1024)))
}
