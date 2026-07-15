use serde_json::{Value, json};

use crate::types::{Content, Context, Message, UserContent};

fn text_blocks(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            Content::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn openai_messages(context: &Context) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system) = &context.system_prompt {
        messages.push(json!({"role":"system","content":system}));
    }
    for message in &context.messages {
        match message {
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => Value::String(text.clone()),
                    UserContent::Blocks(blocks) => Value::Array(
                        blocks
                            .iter()
                            .filter_map(|block| match block {
                                Content::Text(text) => Some(json!({"type":"text","text":text.text})),
                                Content::Image(image) => Some(json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}", image.mime_type, image.data)}})),
                                _ => None,
                            })
                            .collect(),
                    ),
                };
                messages.push(json!({"role":"user","content":content}));
            }
            Message::Assistant(assistant) => {
                let mut value =
                    json!({"role":"assistant","content":text_blocks(&assistant.content)});
                let calls: Vec<Value> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        Content::ToolCall(call) => Some(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into())}})),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    value["tool_calls"] = Value::Array(calls);
                }
                messages.push(value);
            }
            Message::ToolResult(result) => messages.push(json!({
                "role":"tool",
                "tool_call_id":result.tool_call_id,
                "name":result.tool_name,
                "content":text_blocks(&result.content),
            })),
        }
    }
    messages
}

pub fn anthropic_messages(context: &Context) -> Vec<Value> {
    let mut messages = Vec::new();
    for message in &context.messages {
        match message {
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => Value::String(text.clone()),
                    UserContent::Blocks(blocks) => Value::Array(
                        blocks
                            .iter()
                            .filter_map(|block| match block {
                                Content::Text(text) => Some(json!({"type":"text","text":text.text})),
                                Content::Image(image) => Some(json!({"type":"image","source":{"type":"base64","media_type":image.mime_type,"data":image.data}})),
                                _ => None,
                            })
                            .collect(),
                    ),
                };
                messages.push(json!({"role":"user","content":content}));
            }
            Message::Assistant(assistant) => {
                let content: Vec<Value> = assistant.content.iter().filter_map(|block| match block {
                    Content::Text(text) => Some(json!({"type":"text","text":text.text})),
                    Content::Thinking(thinking) if thinking.redacted == Some(true) => Some(json!({"type":"redacted_thinking","data":thinking.thinking_signature})),
                    Content::Thinking(thinking) => Some(json!({"type":"thinking","thinking":thinking.thinking,"signature":thinking.thinking_signature})),
                    Content::ToolCall(call) => Some(json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments})),
                    Content::Image(_) => None,
                }).collect();
                messages.push(json!({"role":"assistant","content":content}));
            }
            Message::ToolResult(result) => messages.push(json!({"role":"user","content":[{
                "type":"tool_result","tool_use_id":result.tool_call_id,"is_error":result.is_error,
                "content":text_blocks(&result.content)
            }]})),
        }
    }
    messages
}

fn response_text_id(signature: Option<&str>, fallback: String) -> String {
    let Some(signature) = signature.filter(|value| !value.is_empty()) else {
        return fallback;
    };
    let id = serde_json::from_str::<crate::types::TextSignatureV1>(signature)
        .map(|value| value.id)
        .unwrap_or_else(|_| signature.to_owned());
    if id.len() <= 64 { id } else { fallback }
}

pub fn responses_input(context: &Context) -> Vec<Value> {
    let mut input = Vec::new();
    if let Some(system) = &context.system_prompt {
        input.push(json!({"role":"system","content":[{"type":"input_text","text":system}]}));
    }
    for (message_index, message) in context.messages.iter().enumerate() {
        match message {
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => vec![json!({"type":"input_text","text":text})],
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            Content::Text(text) => {
                                Some(json!({"type":"input_text","text":text.text}))
                            }
                            Content::Image(image) => Some(json!({
                                "type":"input_image",
                                "image_url":format!("data:{};base64,{}", image.mime_type, image.data)
                            })),
                            _ => None,
                        })
                        .collect(),
                };
                input.push(json!({"role":"user","content":content}));
            }
            Message::Assistant(assistant) => {
                let mut text_index = 0;
                for block in &assistant.content {
                    match block {
                        Content::Text(text) if !text.text.is_empty() => {
                            let fallback = if text_index == 0 {
                                format!("msg_pi_{message_index}")
                            } else {
                                format!("msg_pi_{message_index}_{text_index}")
                            };
                            let id = response_text_id(text.text_signature.as_deref(), fallback);
                            text_index += 1;
                            input.push(json!({
                                "type":"message",
                                "role":"assistant",
                                "status":"completed",
                                "id":id,
                                "content":[{
                                    "type":"output_text",
                                    "text":text.text,
                                    "annotations":[]
                                }]
                            }));
                        }
                        Content::ToolCall(call) => {
                            let (call_id, item_id) = call
                                .id
                                .split_once('|')
                                .map_or((call.id.as_str(), None), |(call_id, item_id)| {
                                    (call_id, Some(item_id))
                                });
                            let mut item = json!({
                                "type":"function_call",
                                "call_id":call_id,
                                "name":call.name,
                                "arguments":serde_json::to_string(&call.arguments)
                                    .unwrap_or_else(|_| "{}".into())
                            });
                            if let Some(item_id) = item_id {
                                item["id"] = Value::String(item_id.to_owned());
                            }
                            input.push(item);
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(result) => {
                let call_id = result
                    .tool_call_id
                    .split_once('|')
                    .map_or(result.tool_call_id.as_str(), |(call_id, _)| call_id);
                input.push(json!({
                    "type":"function_call_output",
                    "call_id":call_id,
                    "output":text_blocks(&result.content)
                }));
            }
        }
    }
    input
}

pub fn google_contents(context: &Context) -> Vec<Value> {
    let mut contents = Vec::new();
    for message in &context.messages {
        match message {
            Message::User(user) => {
                let parts = match &user.content {
                    UserContent::Text(text) => vec![json!({"text":text})],
                    UserContent::Blocks(blocks) => blocks.iter().filter_map(|block| match block {
                        Content::Text(text) => Some(json!({"text":text.text})),
                        Content::Image(image) => Some(json!({"inlineData":{"mimeType":image.mime_type,"data":image.data}})),
                        _ => None,
                    }).collect(),
                };
                contents.push(json!({"role":"user","parts":parts}));
            }
            Message::Assistant(assistant) => {
                let parts: Vec<Value> = assistant.content.iter().filter_map(|block| match block {
                    Content::Text(text) => Some(json!({"text":text.text})),
                    Content::Thinking(thinking) => Some(json!({"text":thinking.thinking,"thought":true,"thoughtSignature":thinking.thinking_signature})),
                    Content::ToolCall(call) => Some(json!({"functionCall":{"id":call.id,"name":call.name,"args":call.arguments},"thoughtSignature":call.thought_signature})),
                    Content::Image(_) => None,
                }).collect();
                contents.push(json!({"role":"model","parts":parts}));
            }
            Message::ToolResult(result) => contents.push(json!({"role":"user","parts":[{"functionResponse":{"id":result.tool_call_id,"name":result.tool_name,"response":{"output":text_blocks(&result.content)}}}]})),
        }
    }
    contents
}

pub fn openai_tools(context: &Context) -> Vec<Value> {
    context.tools.iter().map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.parameters}})).collect()
}

pub fn responses_tools(context: &Context) -> Vec<Value> {
    context.tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters})).collect()
}

pub fn google_tools(context: &Context) -> Vec<Value> {
    if context.tools.is_empty() {
        return Vec::new();
    }
    vec![
        json!({"functionDeclarations":context.tools.iter().map(|tool| json!({"name":tool.name,"description":tool.description,"parametersJsonSchema":tool.parameters})).collect::<Vec<_>>()}),
    ]
}
