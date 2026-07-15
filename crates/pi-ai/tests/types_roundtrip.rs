use futures_util::StreamExt;
use pi_ai::{AssistantMessageEvent, AssistantMessageEventStream, Message};
use serde_json::Value;

#[test]
fn assistant_message_roundtrips_wire_fixture() {
    let source = include_str!("fixtures/messages/assistant_message.json");
    let expected: Value = serde_json::from_str(source).unwrap();
    let message: Message = serde_json::from_str(source).unwrap();
    assert_eq!(serde_json::to_value(&message).unwrap(), expected);
    let json = serde_json::to_string(&message).unwrap();
    assert_eq!(json.matches("\"role\"").count(), 1);
}

#[test]
fn assistant_events_roundtrip_with_snake_case_tags_and_camel_case_fields() {
    let source = include_str!("fixtures/events/sample_events.json");
    let expected: Value = serde_json::from_str(source).unwrap();
    let events: Vec<AssistantMessageEvent> = serde_json::from_str(source).unwrap();
    assert_eq!(serde_json::to_value(&events).unwrap(), expected);
    assert_eq!(events.len(), 11);
    assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
    assert!(matches!(events[1], AssistantMessageEvent::TextStart { .. }));
    assert!(matches!(events[2], AssistantMessageEvent::TextDelta { .. }));
    assert!(matches!(events[3], AssistantMessageEvent::TextEnd { .. }));
    assert!(matches!(
        events[4],
        AssistantMessageEvent::ThinkingStart { .. }
    ));
    assert!(matches!(
        events[5],
        AssistantMessageEvent::ThinkingDelta { .. }
    ));
    assert!(matches!(
        events[6],
        AssistantMessageEvent::ThinkingEnd { .. }
    ));
    assert!(matches!(
        events[7],
        AssistantMessageEvent::ToolcallStart { .. }
    ));
    assert!(matches!(
        events[8],
        AssistantMessageEvent::ToolcallDelta { .. }
    ));
    assert!(matches!(
        events[9],
        AssistantMessageEvent::ToolcallEnd { .. }
    ));
    assert!(matches!(events[10], AssistantMessageEvent::Done { .. }));
}

#[test]
fn assistant_error_event_roundtrips() {
    let source = include_str!("fixtures/events/sample_error_events.json");
    let expected: Value = serde_json::from_str(source).unwrap();
    let events: Vec<AssistantMessageEvent> = serde_json::from_str(source).unwrap();
    assert_eq!(serde_json::to_value(&events).unwrap(), expected);
    assert!(matches!(
        events.as_slice(),
        [AssistantMessageEvent::Error { .. }]
    ));
}

#[tokio::test]
async fn event_stream_yields_terminal_event_and_collects_final_message() {
    let events: Vec<AssistantMessageEvent> =
        serde_json::from_str(include_str!("fixtures/events/sample_events.json")).unwrap();
    let stream = AssistantMessageEventStream::new();
    for event in events.clone() {
        stream.push(event);
    }
    assert!(stream.is_complete());

    let final_message = stream.result().await;
    assert_eq!(final_message.stop_reason, pi_ai::StopReason::ToolUse);

    let yielded: Vec<_> = stream.collect().await;
    assert_eq!(yielded, events);
}
