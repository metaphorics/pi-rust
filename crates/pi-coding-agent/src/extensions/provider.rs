//! Extension provider bridging (Phase 6 commit C7, plan §2 `provider/`).
//!
//! Providers registered by extensions (`pi.registerProvider`) reach the host
//! as `ProviderRegistration`s: the data config (models, baseUrl, apiKey
//! template, headers) enters the host [`ModelRegistry`] — runtime catalog
//! mutation, auth resolution, and model lookup all work natively — while a
//! provider carrying a `streamSimple` implementation (a function; never
//! crosses the wire) streams through the sidecar: `provider/stream` request,
//! `provider/event` notifications relaying pi-ai's normalized
//! [`AssistantMessageEvent`]s wire-identically (no Value loss), final
//! response carrying the authoritative [`AssistantMessage`].
//!
//! Backpressure: provider events ride the bounded incoming channel and are
//! pushed synchronously into the target [`AssistantMessageEventStream`];
//! nothing is dropped and nothing buffers unboundedly beyond the stream's
//! own consumer.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use pi_agent::StreamFn;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Model, StopReason,
    create_assistant_message_event_stream,
};
use pi_ext_protocol::{ProviderRegistration, ProviderStreamParams, Request};
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::model_registry::{ModelRegistry, ProviderConfigInput};

use super::events::ExtensionErrorSink;
use super::{ClientError, ExtensionHost};

/// Host-side view of extension-registered providers. Create it BEFORE the
/// session (its [`extension_stream_fn`] wrapper goes into the session
/// config); the binding attaches the host and applies registration
/// snapshots.
pub struct ExtensionProviders {
    host: parking_lot::Mutex<Weak<ExtensionHost>>,
    /// Model registry the streaming wrapper resolves auth against; attached
    /// with the host at bind time (the wrapper is created before the
    /// session exists).
    registry: parking_lot::Mutex<Option<Arc<RwLock<ModelRegistry>>>>,
    /// Providers whose config carries `streamSimple` — their models stream
    /// via the sidecar.
    streaming: parking_lot::Mutex<HashSet<String>>,
    /// Provider names applied from the last snapshot (unregistered when a
    /// newer snapshot no longer lists them, e.g. after `ctx.reload()`).
    applied: parking_lot::Mutex<Vec<String>>,
    /// In-flight `provider/stream` targets by stream id.
    streams: parking_lot::Mutex<HashMap<String, AssistantMessageEventStream>>,
    next_stream_id: AtomicU64,
    /// Serializes registry mutations (snapshots run off-listener; runtime
    /// register/unregister runs on the serve loop — both funnel here).
    mutate: tokio::sync::Mutex<()>,
    /// Snapshot generations: allocated synchronously in listener order,
    /// checked under `mutate` so a late-running older snapshot never
    /// clobbers a newer one.
    snapshot_gen: AtomicU64,
    applied_gen: AtomicU64,
}

impl Default for ExtensionProviders {
    fn default() -> Self {
        Self {
            host: parking_lot::Mutex::new(Weak::new()),
            registry: parking_lot::Mutex::new(None),
            streaming: parking_lot::Mutex::new(HashSet::new()),
            applied: parking_lot::Mutex::new(Vec::new()),
            streams: parking_lot::Mutex::new(HashMap::new()),
            next_stream_id: AtomicU64::new(1),
            mutate: tokio::sync::Mutex::new(()),
            snapshot_gen: AtomicU64::new(0),
            applied_gen: AtomicU64::new(0),
        }
    }
}

impl ExtensionProviders {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(super) fn attach_host(&self, host: &Arc<ExtensionHost>) {
        *self.host.lock() = Arc::downgrade(host);
    }

    pub(super) fn attach_registry(&self, registry: Arc<RwLock<ModelRegistry>>) {
        *self.registry.lock() = Some(registry);
    }

    /// Whether `provider`'s models stream through the sidecar.
    pub fn is_sidecar_streaming(&self, provider: &str) -> bool {
        self.streaming.lock().contains(provider)
    }

    /// Route one `provider/event` notification into its stream. Events for
    /// unknown ids are dropped (stream already settled/cancelled).
    pub(super) fn dispatch_event(&self, stream_id: &str, event: AssistantMessageEvent) {
        let stream = self.streams.lock().get(stream_id).cloned();
        if let Some(stream) = stream {
            stream.push(event);
        }
    }

    /// Allocate a snapshot generation. MUST be called synchronously in
    /// listener order; the matching [`apply_snapshot`](Self::apply_snapshot)
    /// call may then run concurrently — a stale generation is skipped.
    pub(super) fn allocate_snapshot_generation(&self) -> u64 {
        self.snapshot_gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Apply one `provider/register` (order-preserving: runs on the serve
    /// loop, matching pi's synchronous registration effects).
    pub(super) async fn register(
        &self,
        registry: &Arc<RwLock<ModelRegistry>>,
        registration: &ProviderRegistration,
        errors: &ExtensionErrorSink,
    ) {
        let _serial = self.mutate.lock().await;
        self.register_locked(registry, registration, errors).await;
    }

    async fn register_locked(
        &self,
        registry: &Arc<RwLock<ModelRegistry>>,
        registration: &ProviderRegistration,
        errors: &ExtensionErrorSink,
    ) {
        let config: ProviderConfigInput =
            match serde_json::from_value(registration.config_dto.clone()) {
                Ok(config) => config,
                Err(error) => {
                    errors(provider_error(
                        &registration.name,
                        format!("malformed provider config: {error}"),
                    ));
                    return;
                }
            };
        if let Err(error) = registry
            .write()
            .await
            .register_provider(registration.name.clone(), config)
        {
            errors(provider_error(&registration.name, error));
            return;
        }
        if registration.has_stream_simple {
            self.streaming.lock().insert(registration.name.clone());
        } else {
            self.streaming.lock().remove(&registration.name);
        }
        let mut applied = self.applied.lock();
        if !applied.contains(&registration.name) {
            applied.push(registration.name.clone());
        }
    }
    /// Apply one `provider/unregister`.
    pub(super) async fn unregister(&self, registry: &Arc<RwLock<ModelRegistry>>, name: &str) {
        let _serial = self.mutate.lock().await;
        self.unregister_locked(registry, name).await;
    }

    async fn unregister_locked(&self, registry: &Arc<RwLock<ModelRegistry>>, name: &str) {
        registry.write().await.unregister_provider(name);
        self.streaming.lock().remove(name);
        self.applied.lock().retain(|applied| applied != name);
    }
    /// Reconcile a full registration snapshot (initial load, reload
    /// re-init, respawn replay): register everything listed, unregister
    /// providers that vanished. `generation` (from
    /// [`allocate_snapshot_generation`](Self::allocate_snapshot_generation))
    /// keeps late-running older snapshots from clobbering newer ones.
    pub(super) async fn apply_snapshot(
        &self,
        generation: u64,
        registry: &Arc<RwLock<ModelRegistry>>,
        providers: &[ProviderRegistration],
        errors: &ExtensionErrorSink,
    ) {
        let _serial = self.mutate.lock().await;
        if generation <= self.applied_gen.load(Ordering::SeqCst) {
            return; // Stale snapshot; a newer one already applied.
        }
        self.applied_gen.store(generation, Ordering::SeqCst);
        let vanished: Vec<String> = {
            let applied = self.applied.lock();
            applied
                .iter()
                .filter(|name| !providers.iter().any(|p| &&p.name == name))
                .cloned()
                .collect()
        };
        for name in vanished {
            self.unregister_locked(registry, &name).await;
        }
        for registration in providers {
            self.register_locked(registry, registration, errors).await;
        }
    }
}

fn provider_error(name: &str, error: String) -> pi_ext_protocol::ExtensionError {
    pi_ext_protocol::ExtensionError {
        extension_path: format!("provider:{name}"),
        event: "registerProvider".to_string(),
        error,
        stack: None,
    }
}

/// An error-terminal event stream (mirrors the session's auth-failure
/// shape: a single `error` event carrying an errored assistant message).
fn error_stream(model: &Model, error: String) -> AssistantMessageEventStream {
    let mut message = pi_ai::models::create_empty_assistant_message(model);
    message.stop_reason = StopReason::Error;
    message.error_message = Some(error);
    let stream = create_assistant_message_event_stream();
    stream.push(AssistantMessageEvent::Error {
        reason: StopReason::Error,
        error: message,
    });
    stream
}

/// Wrap a stream fn: models of sidecar-streaming extension providers route
/// through `provider/stream`; everything else falls through to `inner`.
/// Auth (apiKey template/env interpolation + custom headers) resolves in the
/// HOST registry and rides the wire options, exactly like the native path.
pub fn extension_stream_fn(providers: Arc<ExtensionProviders>, inner: StreamFn) -> StreamFn {
    Arc::new(move |model: Model, context, options| {
        if !providers.is_sidecar_streaming(&model.provider) {
            return inner(model, context, options);
        }
        let providers = providers.clone();
        Box::pin(async move {
            let Some(host) = providers.host.lock().clone().upgrade() else {
                return error_stream(&model, "Extension sidecar is not available".to_string());
            };
            let Some(connection) = host.current_connection().await else {
                return error_stream(&model, "Extension sidecar is not running".to_string());
            };
            let Some(registry) = providers.registry.lock().clone() else {
                return error_stream(&model, "Extension sidecar is not available".to_string());
            };

            // Host-side auth resolution (same class as default_stream_fn).
            let auth = {
                let registry = registry.read().await;
                registry.get_api_key_and_headers(&model).await
            };
            if !auth.ok {
                return error_stream(
                    &model,
                    auth.error
                        .unwrap_or_else(|| "Authentication failed".to_string()),
                );
            }

            let mut wire_options = serde_json::Map::new();
            if let Some(api_key) = options.api_key.or(auth.api_key) {
                wire_options.insert("apiKey".to_string(), json!(api_key));
            }
            if let Some(headers) = auth.headers {
                wire_options.insert("headers".to_string(), json!(headers));
            }
            if let Some(temperature) = options.temperature {
                wire_options.insert("temperature".to_string(), json!(temperature));
            }
            if let Some(max_tokens) = options.max_tokens {
                wire_options.insert("maxTokens".to_string(), json!(max_tokens));
            }
            if let Some(reasoning) = options.reasoning {
                wire_options.insert("reasoning".to_string(), json!(reasoning));
            }
            if let Some(session_id) = options.session_id.clone() {
                wire_options.insert("sessionId".to_string(), json!(session_id));
            }

            let stream_id = format!(
                "ps-{}",
                providers.next_stream_id.fetch_add(1, Ordering::Relaxed)
            );
            let stream = create_assistant_message_event_stream();
            providers
                .streams
                .lock()
                .insert(stream_id.clone(), stream.clone());

            let request = Request::ProviderStream(Box::new(ProviderStreamParams {
                stream_id: stream_id.clone(),
                provider: model.provider.clone(),
                model: model.clone(),
                context,
                options: if wire_options.is_empty() {
                    None
                } else {
                    Some(Value::Object(wire_options))
                },
            }));
            let mut pending = match connection.begin_request(request).await {
                Ok(pending) => pending,
                Err(error) => {
                    providers.streams.lock().remove(&stream_id);
                    return error_stream(&model, error.to_string());
                }
            };

            // Drive the response (and cancellation) off-stream; events push
            // as `provider/event` notifications arrive on the serve loop.
            let cancel = options.cancel.clone();
            tokio::spawn({
                let providers = providers.clone();
                let stream = stream.clone();
                let model = model.clone();
                async move {
                    let outcome = match cancel {
                        Some(token) => {
                            tokio::select! {
                                outcome = &mut pending => Some(outcome),
                                _ = super::tools::wait_cancelled(token) => {
                                    // Wire cancel frame → the sidecar aborts
                                    // streamSimple's AbortSignal.
                                    pending.cancel().await;
                                    None
                                }
                            }
                        }
                        None => Some((&mut pending).await),
                    };
                    if !stream.is_complete() {
                        match outcome {
                            Some(Ok(value)) => {
                                match serde_json::from_value::<AssistantMessage>(value) {
                                    // Normal case: the done/error EVENT
                                    // already finished the stream; this is
                                    // the fallback when it did not arrive.
                                    Ok(message) => stream.push(AssistantMessageEvent::Done {
                                        reason: message.stop_reason,
                                        message,
                                    }),
                                    Err(error) => {
                                        let mut message =
                                            pi_ai::models::create_empty_assistant_message(&model);
                                        message.stop_reason = StopReason::Error;
                                        message.error_message =
                                            Some(format!("malformed provider result: {error}"));
                                        stream.push(AssistantMessageEvent::Error {
                                            reason: StopReason::Error,
                                            error: message,
                                        });
                                    }
                                }
                            }
                            Some(Err(error)) => {
                                let mut message =
                                    pi_ai::models::create_empty_assistant_message(&model);
                                message.stop_reason = StopReason::Error;
                                message.error_message = Some(match error {
                                    ClientError::Remote(remote) => remote.message,
                                    other => other.to_string(),
                                });
                                stream.push(AssistantMessageEvent::Error {
                                    reason: StopReason::Error,
                                    error: message,
                                });
                            }
                            None => {
                                let mut message =
                                    pi_ai::models::create_empty_assistant_message(&model);
                                message.stop_reason = StopReason::Aborted;
                                message.error_message = Some("Request aborted".to_string());
                                stream.push(AssistantMessageEvent::Error {
                                    reason: StopReason::Aborted,
                                    error: message,
                                });
                            }
                        }
                    }
                    providers.streams.lock().remove(&stream_id);
                }
            });

            stream
        })
    })
}
