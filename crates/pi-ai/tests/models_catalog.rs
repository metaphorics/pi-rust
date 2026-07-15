use pi_ai::{
    Api, Model, ModelCost, ModelInput, ModelThinkingLevel, Usage,
    models::{calculate_cost, clamp_thinking_level, get_supported_thinking_levels, models_are_equal},
    models_generated::MODELS,
};

fn model() -> Model {
    Model {
        id: "model".into(),
        name: "Model".into(),
        api: Api::from("test"),
        provider: "provider".into(),
        base_url: "https://example.test".into(),
        reasoning: true,
        thinking_level_map: Some([
            (ModelThinkingLevel::Xhigh, None),
            (ModelThinkingLevel::Max, Some("max".into())),
        ].into_iter().collect()),
        input: vec![ModelInput::Text],
        cost: ModelCost { input: 1.0, output: 2.0, cache_read: 0.5, cache_write: 1.25, tiers: vec![] },
        context_window: 1000,
        max_tokens: 100,
        headers: None,
        compat: None,
    }
}

#[test]
fn generated_catalog_is_nonempty_and_deserializes() {
    assert!(MODELS.iter().any(|model| model.provider == "anthropic"));
    assert!(MODELS.iter().any(|model| model.provider == "openai"));
    for entry in MODELS {
        let model = entry.to_model().unwrap_or_else(|error| panic!("{}/{}: {error}", entry.provider, entry.id));
        assert_eq!(model.provider, entry.provider);
        assert_eq!(model.id, entry.id);
    }
}

#[test]
fn thinking_levels_match_pi_clamping() {
    let model = model();
    assert_eq!(
        get_supported_thinking_levels(&model),
        vec![
            ModelThinkingLevel::Off,
            ModelThinkingLevel::Minimal,
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Max,
        ]
    );
    assert_eq!(clamp_thinking_level(&model, ModelThinkingLevel::Xhigh), ModelThinkingLevel::Max);
    assert!(models_are_equal(Some(&model), Some(&model)));
    assert!(!models_are_equal(Some(&model), None));
}

#[test]
fn cost_accounts_for_long_cache_writes() {
    let model = model();
    let mut usage = Usage { input: 1_000_000, output: 1_000_000, cache_read: 1_000_000, cache_write: 1_000_000, cache_write1h: Some(500_000), total_tokens: 4_000_000, ..Usage::default() };
    let cost = calculate_cost(&model, &mut usage);
    assert_eq!(cost.input, 1.0);
    assert_eq!(cost.output, 2.0);
    assert_eq!(cost.cache_read, 0.5);
    assert_eq!(cost.cache_write, 1.625);
    assert_eq!(cost.total, 5.125);
}
