use crate::types::{
    Api, AssistantMessage, Model, ModelCostRates, ModelThinkingLevel, StopReason, Usage, UsageCost,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelEntry {
    pub provider: &'static str,
    pub id: &'static str,
    pub name: &'static str,
    pub api: &'static str,
    pub base_url: &'static str,
    pub reasoning: bool,
    pub context_window: u64,
    pub max_tokens: u64,
    pub raw_json: &'static str,
}

impl ModelEntry {
    pub fn to_model(self) -> Result<Model, serde_json::Error> {
        serde_json::from_str(self.raw_json)
    }
}

pub fn empty_usage() -> Usage {
    Usage::default()
}

pub fn create_empty_assistant_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: empty_usage(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: jiff::Timestamp::now().as_millisecond(),
    }
}

pub fn has_api(model: &Model, api: &str) -> bool {
    model.api == *api
}

pub fn calculate_cost(model: &Model, usage: &mut Usage) -> UsageCost {
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    let mut rates = model.cost.rates();
    let mut matched_threshold = None;
    for tier in &model.cost.tiers {
        if input_tokens > tier.input_tokens_above
            && matched_threshold.is_none_or(|threshold| tier.input_tokens_above > threshold)
        {
            rates = ModelCostRates {
                input: tier.input,
                output: tier.output,
                cache_read: tier.cache_read,
                cache_write: tier.cache_write,
            };
            matched_threshold = Some(tier.input_tokens_above);
        }
    }

    let long_write = usage.cache_write1h.unwrap_or(0);
    let short_write = usage.cache_write.saturating_sub(long_write);
    usage.cost.input = rates.input / 1_000_000.0 * usage.input as f64;
    usage.cost.output = rates.output / 1_000_000.0 * usage.output as f64;
    usage.cost.cache_read = rates.cache_read / 1_000_000.0 * usage.cache_read as f64;
    usage.cost.cache_write = (rates.cache_write * short_write as f64
        + rates.input * 2.0 * long_write as f64)
        / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    usage.cost.clone()
}

pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    ModelThinkingLevel::ALL
        .into_iter()
        .filter(|level| {
            let mapped = model
                .thinking_level_map
                .as_ref()
                .and_then(|mapping| mapping.get(level));
            if mapped.is_some_and(Option::is_none) {
                return false;
            }
            if matches!(level, ModelThinkingLevel::Xhigh | ModelThinkingLevel::Max) {
                return mapped.is_some();
            }
            true
        })
        .collect()
}

pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available = get_supported_thinking_levels(model);
    if available.contains(&level) {
        return level;
    }
    let requested = ModelThinkingLevel::ALL
        .iter()
        .position(|candidate| *candidate == level)
        .unwrap_or(0);
    ModelThinkingLevel::ALL[requested..]
        .iter()
        .chain(ModelThinkingLevel::ALL[..requested].iter().rev())
        .copied()
        .find(|candidate| available.contains(candidate))
        .or_else(|| available.first().copied())
        .unwrap_or(ModelThinkingLevel::Off)
}

pub fn models_are_equal(a: Option<&Model>, b: Option<&Model>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a.id == b.id && a.provider == b.provider)
}

pub fn api(value: impl Into<String>) -> Api {
    Api::new(value)
}
