use pi_ai::{
    SharedText,
    api::EventBuilder,
    types::{Api, AssistantMessageEvent, Content, Model, ModelCost, ModelInput, StopReason},
};

fn model() -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: Api::from("openai-responses"),
        provider: "test-provider".into(),
        base_url: "https://example.test/v1".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 16_384,
        max_tokens: 128,
        headers: None,
        compat: None,
    }
}

fn partial_text(event: &AssistantMessageEvent) -> Option<String> {
    let AssistantMessageEvent::TextDelta { partial, .. } = event else {
        return None;
    };
    let Content::Text(text) = &partial.content[0] else {
        panic!("text delta partial must contain text");
    };
    Some(text.text.as_string())
}

#[test]
fn text_delta_partials_keep_historical_snapshots() {
    let mut builder = EventBuilder::new(&model());
    for _ in 0..100 {
        builder.text_delta("x");
    }

    let events = builder.finish(StopReason::Stop);
    let deltas: Vec<_> = events.iter().filter_map(partial_text).collect();
    assert_eq!(deltas.len(), 100);
    assert_eq!(deltas.first().unwrap(), "x");
    assert_eq!(deltas.last().unwrap(), &"x".repeat(100));

    let first_delta = events
        .iter()
        .find(|event| matches!(event, AssistantMessageEvent::TextDelta { .. }))
        .unwrap();
    let serialized = serde_json::to_value(first_delta).unwrap();
    assert_eq!(serialized["partial"]["content"][0]["text"], "x");
}

#[test]
fn shared_text_append_chain_stores_only_new_delta_bytes() {
    let root = SharedText::from_str("a");
    let mut tip = root.clone();
    let mut sum_of_tip_lengths = tip.len();

    for _ in 0..5_000 {
        tip = tip.append("b");
        sum_of_tip_lengths += tip.len();
    }

    assert_eq!(sum_of_tip_lengths, 5_001 * 5_002 / 2);
    assert_eq!(tip.len(), 5_001);
    assert_eq!(root.as_string(), "a");
    assert_eq!(tip.as_string(), format!("a{}", "b".repeat(5_000)));
}
