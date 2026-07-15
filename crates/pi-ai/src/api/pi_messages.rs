use std::sync::Arc;

use serde_json::{Value, json};

use crate::{event_stream::AssistantMessageEventStream, http::{ReqwestStreamHttpClient, StreamHttpClient}, sse::parse_sse_chunks, types::{AssistantMessageEvent, Context, Model, StopReason, StreamOptions}};

use super::common::{self, ApiResult, EventBuilder};

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    json!({"model":model.id,"context":context,"options":{
        "temperature":options.temperature,
        "maxTokens":options.max_tokens,
        "cacheRetention":options.cache_retention,
        "sessionId":options.session_id,
    }})
}

pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers=common::merged_headers(model,options);headers.push(("accept".into(),"text/event-stream".into()));headers.push(("content-type".into(),"application/json".into()));
    if let Some(key)=&options.api_key{headers.push(("authorization".into(),format!("Bearer {key}")));}headers
}

pub fn parse_stream_events<I,B>(chunks:I,model:&Model)->ApiResult<Vec<AssistantMessageEvent>> where I:IntoIterator<Item=B>,B:AsRef<[u8]> {
    let mut builder=EventBuilder::new(model);let mut reason=StopReason::Stop;
    for data in parse_sse_chunks(chunks){let event:Value=serde_json::from_str(&data).map_err(|error|format!("invalid pi-messages SSE JSON: {error}"))?;
        match event["type"].as_str(){
            Some("text_delta")=>builder.text_delta(event["delta"].as_str().unwrap_or("")),
            Some("thinking_delta")=>builder.thinking_delta(event["delta"].as_str().unwrap_or("")),
            Some("thinking_end")=>{if let Some(signature)=event["contentSignature"].as_str(){builder.set_thinking_signature(signature.to_owned());}},
            Some("toolcall_start")=>{let key=event["contentIndex"].as_u64().unwrap_or(0).to_string();builder.tool_call_start(&key,event["id"].as_str().unwrap_or(&key),event["toolName"].as_str().unwrap_or(""));},
            Some("toolcall_delta")=>{let key=event["contentIndex"].as_u64().unwrap_or(0).to_string();builder.tool_call_delta(&key,event["delta"].as_str().unwrap_or(""));},
            Some("done")=>{reason=common::stop_reason(event["reason"].as_str());let usage=&event["usage"];builder.set_usage(usage["input"].as_u64(),usage["output"].as_u64(),usage["cacheRead"].as_u64(),usage["cacheWrite"].as_u64(),usage["reasoning"].as_u64());builder.set_response_id(event["responseId"].as_str());},
            Some("error")=>return Err(event["errorMessage"].as_str().unwrap_or("pi-messages stream error").to_owned()),
            _=>{}
        }
    }Ok(builder.finish(reason))
}

pub fn stream_with_client(model:Model,context:Context,options:StreamOptions,client:Arc<dyn StreamHttpClient>)->AssistantMessageEventStream{let url=format!("{}/messages",model.base_url.trim_end_matches('/'));let headers=build_headers(&model,&options);let body=build_request_body(&model,&context,&options);common::spawn_stream(model,context,options,client,url,headers,body,|chunks,model|parse_stream_events(chunks,model),false)}
pub fn stream(model:Model,context:Context,options:StreamOptions)->AssistantMessageEventStream{match ReqwestStreamHttpClient::new(){Ok(client)=>stream_with_client(model,context,options,Arc::new(client)),Err(error)=>{let stream=AssistantMessageEventStream::new();let mut message=common::empty_message(&model);message.stop_reason=StopReason::Error;message.error_message=Some(error.to_string());stream.push(AssistantMessageEvent::Error{reason:StopReason::Error,error:message});stream}}}
pub fn stream_simple(model:Model,context:Context,options:StreamOptions)->AssistantMessageEventStream{stream(model,context,options)}
