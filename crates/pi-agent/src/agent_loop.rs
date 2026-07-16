//! Agent loop — port of packages/agent/src/agent-loop.ts.

use std::sync::Arc;

use futures_util::StreamExt;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, Context, Content, Message, Model, StopReason,
    ToolResultMessage,
};
use serde_json::Value;

use crate::{
    cancel::CancellationToken,
    tools::{error_tool_result, prepare_and_validate_arguments},
    types::{
        AfterToolCallContext, AgentContext, AgentEvent, AgentEventSink, AgentLoopConfig,
        AgentLoopTurnUpdate, AgentMessage, AgentThinkingLevel, AgentToolCall, AgentToolResult,
        BeforeToolCallContext, PrepareNextTurnContext, ShouldStopAfterTurnContext, StreamCallOptions,
        StreamFn, ToolDefinition, ToolExecutionMode, tool_calls_from_message,
    },
};

#[derive(Debug, thiserror::Error)]
pub enum AgentLoopError {
    #[error("Cannot continue: no messages in context")]
    EmptyContext,
    #[error("Cannot continue from message role: assistant")]
    ContinueFromAssistant,
}

/// Start an agent loop with new prompt messages.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    cancel: Option<CancellationToken>,
    stream_fn: StreamFn,
) -> Vec<AgentMessage> {
    let mut new_messages = prompts.clone();
    let mut current_context = AgentContext {
        system_prompt: context.system_prompt,
        messages: {
            let mut messages = context.messages;
            messages.extend(prompts.iter().cloned());
            messages
        },
        tools: context.tools,
    };

    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;
    for prompt in &prompts {
        emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        })
        .await;
        emit(AgentEvent::MessageEnd {
            message: prompt.clone(),
        })
        .await;
    }

    run_loop(
        &mut current_context,
        &mut new_messages,
        config,
        cancel,
        &emit,
        &stream_fn,
    )
    .await;
    new_messages
}

/// Continue an agent loop from the current context without adding a new message.
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    cancel: Option<CancellationToken>,
    stream_fn: StreamFn,
) -> Result<Vec<AgentMessage>, AgentLoopError> {
    if context.messages.is_empty() {
        return Err(AgentLoopError::EmptyContext);
    }
    if context.messages.last().is_some_and(|m| m.role() == "assistant") {
        return Err(AgentLoopError::ContinueFromAssistant);
    }

    let mut new_messages = Vec::new();
    let mut current_context = context;

    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;

    run_loop(
        &mut current_context,
        &mut new_messages,
        config,
        cancel,
        &emit,
        &stream_fn,
    )
    .await;
    Ok(new_messages)
}

/// Collecting event sink for tests and simple callers.
pub fn collecting_sink(
    events: Arc<parking_lot::Mutex<Vec<AgentEvent>>>,
) -> AgentEventSink {
    Arc::new(move |event| {
        let events = events.clone();
        Box::pin(async move {
            events.lock().push(event);
        })
    })
}

async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    cancel: Option<CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) {
    let mut first_turn = true;
    let mut pending_messages: Vec<AgentMessage> =
        get_optional_messages(config.get_steering_messages.as_ref()).await;

    loop {
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending_messages.is_empty() {
            if !first_turn {
                emit(AgentEvent::TurnStart).await;
            } else {
                first_turn = false;
            }

            if !pending_messages.is_empty() {
                for message in pending_messages.drain(..) {
                    emit(AgentEvent::MessageStart {
                        message: message.clone(),
                    })
                    .await;
                    emit(AgentEvent::MessageEnd {
                        message: message.clone(),
                    })
                    .await;
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            let message =
                stream_assistant_response(current_context, &config, cancel.as_ref(), emit, stream_fn)
                    .await;
            new_messages.push(AgentMessage::assistant(message.clone()));

            if stop_is_fatal(message.stop_reason) {
                emit(AgentEvent::TurnEnd {
                    message: AgentMessage::assistant(message.clone()),
                    tool_results: Vec::new(),
                })
                .await;
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return;
            }

            let tool_calls = tool_calls_from_message(&message);
            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;

            if !tool_calls.is_empty() {
                let executed = if message.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(&tool_calls, emit).await
                } else {
                    execute_tool_calls(current_context, &message, &config, cancel.as_ref(), emit)
                        .await
                };
                tool_results = executed.messages;
                has_more_tool_calls = !executed.terminate;

                for result in &tool_results {
                    current_context
                        .messages
                        .push(AgentMessage::tool_result(result.clone()));
                    new_messages.push(AgentMessage::tool_result(result.clone()));
                }
            }

            emit(AgentEvent::TurnEnd {
                message: AgentMessage::assistant(message.clone()),
                tool_results: tool_results.clone(),
            })
            .await;

            let next_turn_context = PrepareNextTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                context: current_context.clone(),
                new_messages: new_messages.clone(),
            };
            if let Some(prepare) = config.prepare_next_turn.as_ref()
                && let Some(snapshot) = prepare(next_turn_context).await
            {
                apply_turn_update(current_context, &mut config, snapshot);
            }

            if let Some(should_stop) = config.should_stop_after_turn.as_ref() {
                let stop_ctx = ShouldStopAfterTurnContext {
                    message: message.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                };
                // Note: pi polls steering only after shouldStop returns false.
                // The TS shouldStop test counts one steering poll even when
                // stopping — because getSteeringMessages is still called once
                // at loop start (pendingMessages). Mid-loop, shouldStop exits
                // before the post-turn steering poll.
                if should_stop(stop_ctx).await {
                    emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await;
                    return;
                }
            }

            pending_messages =
                get_optional_messages(config.get_steering_messages.as_ref()).await;
        }

        let follow_ups = get_optional_messages(config.get_follow_up_messages.as_ref()).await;
        if !follow_ups.is_empty() {
            pending_messages = follow_ups;
            continue;
        }
        break;
    }

    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await;
}

fn apply_turn_update(
    current_context: &mut AgentContext,
    config: &mut AgentLoopConfig,
    snapshot: AgentLoopTurnUpdate,
) {
    if let Some(context) = snapshot.context {
        *current_context = context;
    }
    if let Some(model) = snapshot.model {
        config.model = model;
    }
    if let Some(level) = snapshot.thinking_level {
        config.reasoning = match level {
            AgentThinkingLevel::Off => None,
            other => other.into(),
        };
    }
}

async fn get_optional_messages(
    getter: Option<&crate::types::GetMessagesFn>,
) -> Vec<AgentMessage> {
    match getter {
        Some(get) => get().await,
        None => Vec::new(),
    }
}

fn stop_is_fatal(reason: StopReason) -> bool {
    matches!(reason, StopReason::Error | StopReason::Aborted)
}

async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> AssistantMessage {
    let mut messages = context.messages.clone();
    if let Some(transform) = config.transform_context.as_ref() {
        messages = transform(messages, cancel.cloned()).await;
    }

    let llm_messages = (config.convert_to_llm)(messages).await;
    let llm_context = Context {
        system_prompt: if context.system_prompt.is_empty() {
            None
        } else {
            Some(context.system_prompt.clone())
        },
        messages: llm_messages,
        tools: context
            .tools
            .iter()
            .map(|tool| tool.to_llm_tool())
            .collect(),
    };

    let resolved_api_key = if let Some(get_key) = config.get_api_key.as_ref() {
        get_key(config.model.provider.clone())
            .await
            .or_else(|| config.api_key.clone())
    } else {
        config.api_key.clone()
    };

    let options = StreamCallOptions {
        temperature: config.temperature,
        max_tokens: config.max_tokens,
        api_key: resolved_api_key,
        reasoning: config.reasoning,
        cancel: cancel.cloned(),
        session_id: config.session_id.clone(),
        metadata: config.metadata.clone(),
    };

    let response = stream_fn(config.model.clone(), llm_context, options).await;

    let mut partial_message: Option<AssistantMessage> = None;
    let mut added_partial = false;

    let mut stream = response.clone();
    while let Some(event) = stream.next().await {
        match &event {
            AssistantMessageEvent::Start { partial } => {
                partial_message = Some(partial.clone());
                context
                    .messages
                    .push(AgentMessage::assistant(partial.clone()));
                added_partial = true;
                emit(AgentEvent::MessageStart {
                    message: AgentMessage::assistant(partial.clone()),
                })
                .await;
            }
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolcallStart { partial, .. }
            | AssistantMessageEvent::ToolcallDelta { partial, .. }
            | AssistantMessageEvent::ToolcallEnd { partial, .. } => {
                if partial_message.is_some() {
                    partial_message = Some(partial.clone());
                    if let Some(last) = context.messages.last_mut() {
                        *last = AgentMessage::assistant(partial.clone());
                    }
                    emit(AgentEvent::MessageUpdate {
                        message: AgentMessage::assistant(partial.clone()),
                        assistant_message_event: event.clone(),
                    })
                    .await;
                }
            }
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                let final_message = response.result().await;
                if added_partial {
                    if let Some(last) = context.messages.last_mut() {
                        *last = AgentMessage::assistant(final_message.clone());
                    }
                } else {
                    context
                        .messages
                        .push(AgentMessage::assistant(final_message.clone()));
                }
                if !added_partial {
                    emit(AgentEvent::MessageStart {
                        message: AgentMessage::assistant(final_message.clone()),
                    })
                    .await;
                }
                emit(AgentEvent::MessageEnd {
                    message: AgentMessage::assistant(final_message.clone()),
                })
                .await;
                return final_message;
            }
        }
    }

    let final_message = response.result().await;
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = AgentMessage::assistant(final_message.clone());
        }
    } else {
        context
            .messages
            .push(AgentMessage::assistant(final_message.clone()));
        emit(AgentEvent::MessageStart {
            message: AgentMessage::assistant(final_message.clone()),
        })
        .await;
    }
    emit(AgentEvent::MessageEnd {
        message: AgentMessage::assistant(final_message.clone()),
    })
    .await;
    final_message
}

struct ExecutedToolCallBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

struct PreparedToolCall {
    tool_call: AgentToolCall,
    tool: Arc<ToolDefinition>,
    args: Value,
}

enum Preparation {
    Prepared(PreparedToolCall),
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
}

struct FinalizedToolCall {
    tool_call: AgentToolCall,
    result: AgentToolResult,
    is_error: bool,
}

async fn fail_tool_calls_from_truncated_message(
    tool_calls: &[AgentToolCall],
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut messages = Vec::new();
    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: Value::Object(tool_call.arguments.clone()),
        })
        .await;
        let finalized = FinalizedToolCall {
            tool_call: tool_call.clone(),
            result: error_tool_result(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tool_call.name
            )),
            is_error: true,
        };
        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }
    ExecutedToolCallBatch {
        messages,
        terminate: false,
    }
}

async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let tool_calls = tool_calls_from_message(assistant_message);
    let has_sequential = tool_calls.iter().any(|tc| {
        current_context
            .tools
            .iter()
            .find(|t| t.name == tc.name)
            .and_then(|t| t.execution_mode)
            == Some(ToolExecutionMode::Sequential)
    });
    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential {
        execute_tool_calls_sequential(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            cancel,
            emit,
        )
        .await
    } else {
        execute_tool_calls_parallel(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            cancel,
            emit,
        )
        .await
    }
}

async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[AgentToolCall],
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut finalized_calls = Vec::new();
    let mut messages = Vec::new();

    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: Value::Object(tool_call.arguments.clone()),
        })
        .await;

        let preparation =
            prepare_tool_call(current_context, assistant_message, tool_call, config, cancel).await;
        let finalized = match preparation {
            Preparation::Immediate { result, is_error } => FinalizedToolCall {
                tool_call: tool_call.clone(),
                result,
                is_error,
            },
            Preparation::Prepared(prepared) => {
                let executed = execute_prepared_tool_call(&prepared, cancel, emit).await;
                finalize_executed_tool_call(
                    current_context,
                    assistant_message,
                    &prepared,
                    executed,
                    config,
                    cancel,
                )
                .await
            }
        };

        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        finalized_calls.push(finalized);
        messages.push(tool_result_message);

        if cancel.is_some_and(CancellationToken::is_cancelled) {
            break;
        }
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[AgentToolCall],
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    enum Entry {
        Ready(FinalizedToolCall),
        Pending {
            prepared: PreparedToolCall,
        },
    }

    let mut entries: Vec<Entry> = Vec::new();

    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: Value::Object(tool_call.arguments.clone()),
        })
        .await;

        let preparation =
            prepare_tool_call(current_context, assistant_message, tool_call, config, cancel).await;
        match preparation {
            Preparation::Immediate { result, is_error } => {
                let finalized = FinalizedToolCall {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                };
                emit_tool_execution_end(&finalized, emit).await;
                entries.push(Entry::Ready(finalized));
            }
            Preparation::Prepared(prepared) => {
                entries.push(Entry::Pending { prepared });
            }
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            break;
        }
    }

    // Run pending tools concurrently; tool_execution_end emits in completion order.
    let mut pending_futures = Vec::new();
    let mut pending_indices = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Entry::Pending { prepared } = entry {
            pending_indices.push(index);
            let prepared = PreparedToolCall {
                tool_call: prepared.tool_call.clone(),
                tool: prepared.tool.clone(),
                args: prepared.args.clone(),
            };
            let cancel = cancel.cloned();
            let emit = emit.clone();
            let current_context = current_context.clone();
            let assistant_message = assistant_message.clone();
            // Capture after_tool_call / hooks via config clone pieces we need.
            let after_tool_call = config.after_tool_call.clone();
            pending_futures.push(async move {
                let executed = execute_prepared_tool_call(&prepared, cancel.as_ref(), &emit).await;
                let finalized = finalize_executed_with_after(
                    &current_context,
                    &assistant_message,
                    &prepared,
                    executed,
                    after_tool_call.as_ref(),
                    cancel.as_ref(),
                )
                .await;
                emit_tool_execution_end(&finalized, &emit).await;
                (index, finalized)
            });
        }
    }

    let mut ordered: Vec<Option<FinalizedToolCall>> = entries
        .into_iter()
        .map(|entry| match entry {
            Entry::Ready(finalized) => Some(finalized),
            Entry::Pending { .. } => None,
        })
        .collect();

    let results = futures_util::future::join_all(pending_futures).await;
    for (index, finalized) in results {
        ordered[index] = Some(finalized);
    }

    let ordered_finalized: Vec<FinalizedToolCall> =
        ordered.into_iter().map(|item| item.expect("finalized")).collect();

    let mut messages = Vec::new();
    for finalized in &ordered_finalized {
        let tool_result_message = create_tool_result_message(finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&ordered_finalized),
    }
}

fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCall]) -> bool {
    !finalized_calls.is_empty()
        && finalized_calls
            .iter()
            .all(|finalized| finalized.result.terminate == Some(true))
}

async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &AgentToolCall,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
) -> Preparation {
    let Some(tool) = current_context
        .tools
        .iter()
        .find(|t| t.name == tool_call.name)
        .cloned()
    else {
        return Preparation::Immediate {
            result: error_tool_result(format!("Tool {} not found", tool_call.name)),
            is_error: true,
        };
    };

    match prepare_and_validate_arguments(&tool, tool_call) {
        Ok(validated_args) => {
            let args = Arc::new(parking_lot::Mutex::new(validated_args));
            if let Some(before) = config.before_tool_call.as_ref() {
                let before_result = before(
                    BeforeToolCallContext {
                        assistant_message: assistant_message.clone(),
                        tool_call: tool_call.clone(),
                        args: args.clone(),
                        context: current_context.clone(),
                    },
                    cancel.cloned(),
                )
                .await;
                if cancel.is_some_and(CancellationToken::is_cancelled) {
                    return Preparation::Immediate {
                        result: error_tool_result("Operation aborted"),
                        is_error: true,
                    };
                }
                if let Some(result) = before_result
                    && result.block
                {
                    return Preparation::Immediate {
                        result: error_tool_result(
                            result
                                .reason
                                .unwrap_or_else(|| "Tool execution was blocked".into()),
                        ),
                        is_error: true,
                    };
                }
            }
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Preparation::Immediate {
                    result: error_tool_result("Operation aborted"),
                    is_error: true,
                };
            }
            let final_args = args.lock().clone();
            Preparation::Prepared(PreparedToolCall {
                tool_call: tool_call.clone(),
                tool,
                args: final_args,
            })
        }
        Err(error) => Preparation::Immediate {
            result: error_tool_result(error),
            is_error: true,
        },
    }
}

struct ExecutedToolCallOutcome {
    result: AgentToolResult,
    is_error: bool,
}

async fn execute_prepared_tool_call(
    prepared: &PreparedToolCall,
    cancel: Option<&CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallOutcome {
    let accepting = Arc::new(std::sync::atomic::AtomicBool::new(true));
    // JoinHandles for each tool_execution_update emit — awaited after execute
    // settles (mirrors TS Promise.all(updateEvents) before tool_execution_end).
    let update_handles =
        Arc::new(parking_lot::Mutex::new(Vec::<tokio::task::JoinHandle<()>>::new()));

    let on_update: crate::types::AgentToolUpdateCallback = {
        let accepting = accepting.clone();
        let emit = emit.clone();
        let tool_call_id = prepared.tool_call.id.clone();
        let tool_name = prepared.tool_call.name.clone();
        let args = Value::Object(prepared.tool_call.arguments.clone());
        let update_handles = update_handles.clone();
        Arc::new(move |partial_result: AgentToolResult| {
            if !accepting.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let emit = emit.clone();
            let tool_call_id = tool_call_id.clone();
            let tool_name = tool_name.clone();
            let args = args.clone();
            let handle = tokio::spawn(async move {
                emit(AgentEvent::ToolExecutionUpdate {
                    tool_call_id,
                    tool_name,
                    args,
                    partial_result,
                })
                .await;
            });
            update_handles.lock().push(handle);
        })
    };

    let execute_result = (prepared.tool.execute)(
        prepared.tool_call.id.clone(),
        prepared.args.clone(),
        cancel.cloned(),
        Some(on_update),
    )
    .await;

    // Stop accepting new updates (oracle: both try and catch paths).
    accepting.store(false, std::sync::atomic::Ordering::Release);
    // Promise.all-style barrier: every scheduled update emit finishes before we
    // return, so the caller emits tool_execution_end strictly after updates.
    let handles = std::mem::take(&mut *update_handles.lock());
    for handle in handles {
        let _ = handle.await;
    }

    match execute_result {
        Ok(result) => ExecutedToolCallOutcome {
            result,
            is_error: false,
        },
        Err(error) => ExecutedToolCallOutcome {
            result: error_tool_result(error),
            is_error: true,
        },
    }
}

async fn finalize_executed_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    prepared: &PreparedToolCall,
    executed: ExecutedToolCallOutcome,
    config: &AgentLoopConfig,
    cancel: Option<&CancellationToken>,
) -> FinalizedToolCall {
    finalize_executed_with_after(
        current_context,
        assistant_message,
        prepared,
        executed,
        config.after_tool_call.as_ref(),
        cancel,
    )
    .await
}

async fn finalize_executed_with_after(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    prepared: &PreparedToolCall,
    executed: ExecutedToolCallOutcome,
    after_tool_call: Option<&crate::types::AfterToolCallFn>,
    cancel: Option<&CancellationToken>,
) -> FinalizedToolCall {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after) = after_tool_call
        && let Some(after_result) = after(
            AfterToolCallContext {
                assistant_message: assistant_message.clone(),
                tool_call: prepared.tool_call.clone(),
                args: prepared.args.clone(),
                result: result.clone(),
                is_error,
                context: current_context.clone(),
            },
            cancel.cloned(),
        )
        .await
    {
        if let Some(content) = after_result.content {
            result.content = content;
        }
        if let Some(details) = after_result.details {
            result.details = details;
        }
        if let Some(terminate) = after_result.terminate {
            result.terminate = Some(terminate);
        }
        if let Some(flag) = after_result.is_error {
            is_error = flag;
        }
    }

    FinalizedToolCall {
        tool_call: prepared.tool_call.clone(),
        result,
        is_error,
    }
}

async fn emit_tool_execution_end(finalized: &FinalizedToolCall, emit: &AgentEventSink) {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.is_error,
    })
    .await;
}

fn create_tool_result_message(finalized: &FinalizedToolCall) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized.result.content.clone(),
        details: if finalized.result.details.is_null() {
            None
        } else {
            Some(finalized.result.details.clone())
        },
        added_tool_names: finalized.result.added_tool_names.clone(),
        is_error: finalized.is_error,
        timestamp: now_ms(),
    }
}

async fn emit_tool_result_message(tool_result_message: &ToolResultMessage, emit: &AgentEventSink) {
    emit(AgentEvent::MessageStart {
        message: AgentMessage::tool_result(tool_result_message.clone()),
    })
    .await;
    emit(AgentEvent::MessageEnd {
        message: AgentMessage::tool_result(tool_result_message.clone()),
    })
    .await;
}

fn now_ms() -> i64 {
    jiff_now_ms()
}

fn jiff_now_ms() -> i64 {
    // Avoid adding jiff as a direct dep; use system time millis.
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Re-export Message for stream_fn signature ergonomics in tests.
pub use pi_ai::Message as LlmMessage;

/// Build a default no-network stream_fn that always errors (safety).
pub fn unavailable_stream_fn() -> StreamFn {
    Arc::new(|model, _context, _options| {
        Box::pin(async move {
            let stream = pi_ai::create_assistant_message_event_stream();
            let message = AssistantMessage {
                content: Vec::new(),
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: Default::default(),
                stop_reason: StopReason::Error,
                error_message: Some("no stream_fn configured".into()),
                timestamp: now_ms(),
            };
            stream.push(AssistantMessageEvent::Error {
                reason: StopReason::Error,
                error: message.clone(),
            });
            stream
        })
    })
}

// Keep Model import used via StreamFn.
const _: fn(Model) = |_| {};
const _: fn(Message) = |_| {};
const _: fn(Content) = |_| {};
