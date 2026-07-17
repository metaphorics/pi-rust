//! CLI model selection and `--list-models` — port of `main.ts:357-453`
//! `buildSessionOptions` and `cli/list-models.ts`.
//!
//! This is a printer module of the `pi` binary: `println!` here is the
//! oracle's `console.log` stdout contract for metadata commands (which never
//! run in json/rpc wire modes).

use pi_agent::AgentThinkingLevel;
use pi_ai::models::models_are_equal;
use pi_ai::types::{Model, ModelInput, ModelThinkingLevel};
use pi_tui::fuzzy::fuzzy_filter;

use crate::model_registry::{ModelRegistry, ScopedModelEntry};
use crate::session::services::{DiagnosticLevel, RuntimeDiagnostic};
use crate::session::{ScopedModel, format_no_models_available_message};

use super::args::Args;

/// Session creation options resolved from CLI flags (oracle
/// `CreateAgentSessionOptions` subset that `main` forwards).
#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    pub model: Option<Model>,
    pub thinking_level: Option<AgentThinkingLevel>,
    pub scoped_models: Vec<ScopedModel>,
    /// `--tools`: explicit initial active tool list.
    pub tools: Option<Vec<String>>,
    /// `--exclude-tools`: denylist.
    pub exclude_tools: Option<Vec<String>>,
    /// `--no-tools`: remove every tool ("all").
    pub no_tools_all: bool,
    /// `--no-builtin-tools`: remove built-in tools only.
    pub no_builtin_tools: bool,
}

/// Result of [`build_session_options`].
pub struct BuildSessionOptionsResult {
    pub options: SessionOptions,
    pub cli_thinking_from_model: bool,
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

pub(crate) fn model_to_agent_thinking(level: ModelThinkingLevel) -> AgentThinkingLevel {
    match level {
        ModelThinkingLevel::Off => AgentThinkingLevel::Off,
        ModelThinkingLevel::Minimal => AgentThinkingLevel::Minimal,
        ModelThinkingLevel::Low => AgentThinkingLevel::Low,
        ModelThinkingLevel::Medium => AgentThinkingLevel::Medium,
        ModelThinkingLevel::High => AgentThinkingLevel::High,
        ModelThinkingLevel::Xhigh => AgentThinkingLevel::Xhigh,
        ModelThinkingLevel::Max => AgentThinkingLevel::Max,
    }
}

fn cli_thinking_to_model(level: super::args::ThinkingLevel) -> ModelThinkingLevel {
    match level {
        super::args::ThinkingLevel::Off => ModelThinkingLevel::Off,
        super::args::ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        super::args::ThinkingLevel::Low => ModelThinkingLevel::Low,
        super::args::ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        super::args::ThinkingLevel::High => ModelThinkingLevel::High,
        super::args::ThinkingLevel::XHigh => ModelThinkingLevel::Xhigh,
        super::args::ThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

fn agent_to_model_thinking(level: AgentThinkingLevel) -> ModelThinkingLevel {
    match level {
        AgentThinkingLevel::Off => ModelThinkingLevel::Off,
        AgentThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        AgentThinkingLevel::Low => ModelThinkingLevel::Low,
        AgentThinkingLevel::Medium => ModelThinkingLevel::Medium,
        AgentThinkingLevel::High => ModelThinkingLevel::High,
        AgentThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        AgentThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

/// Clamp an initial thinking level to model capabilities (sdk.ts:238-243):
/// no model ⇒ off, otherwise `clampThinkingLevel`.
pub fn clamp_initial_thinking(
    model: Option<&Model>,
    level: AgentThinkingLevel,
) -> AgentThinkingLevel {
    match model {
        None => AgentThinkingLevel::Off,
        Some(model) => model_to_agent_thinking(pi_ai::models::clamp_thinking_level(
            model,
            agent_to_model_thinking(level),
        )),
    }
}
/// Parse a settings `defaultThinkingLevel` string.
pub fn model_thinking_from_str(raw: &str) -> Option<ModelThinkingLevel> {
    match raw {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

/// Build the model/thinking/scoped/tool options from parsed CLI flags
/// (oracle `buildSessionOptions`, main.ts:357-453).
pub async fn build_session_options(
    parsed: &Args,
    scoped_models: &[ScopedModelEntry],
    has_existing_session: bool,
    model_registry: &ModelRegistry,
    default_provider: Option<&str>,
    default_model: Option<&str>,
) -> BuildSessionOptionsResult {
    let mut options = SessionOptions::default();
    let mut diagnostics = Vec::new();
    let mut cli_thinking_from_model = false;

    // Model from CLI: --provider <name> --model <pattern>, or
    // --model <provider>/<pattern>[:<thinking>].
    if parsed.model.is_some() {
        let resolved = model_registry
            .resolve(
                parsed.provider.as_deref(),
                parsed.model.as_deref(),
                parsed.thinking.map(cli_thinking_to_model),
            )
            .await;
        if let Some(warning) = resolved.warning {
            diagnostics.push(RuntimeDiagnostic {
                level: DiagnosticLevel::Warning,
                message: warning,
            });
        }
        if let Some(error) = resolved.error {
            diagnostics.push(RuntimeDiagnostic {
                level: DiagnosticLevel::Error,
                message: error,
            });
        }
        if let Some(model) = resolved.model {
            options.model = Some(model);
            // "--model <pattern>:<thinking>" shorthand; explicit --thinking
            // still takes precedence (applied below).
            if parsed.thinking.is_none()
                && let Some(level) = resolved.thinking_level
            {
                options.thinking_level = Some(model_to_agent_thinking(level));
                cli_thinking_from_model = true;
            }
        }
    }

    if options.model.is_none() && !scoped_models.is_empty() && !has_existing_session {
        // Saved default within scope wins, otherwise the first scoped model.
        let saved_model = match (default_provider, default_model) {
            (Some(provider), Some(model_id)) => model_registry.find(provider, model_id).cloned(),
            _ => None,
        };
        let saved_in_scope = saved_model.as_ref().and_then(|saved| {
            scoped_models
                .iter()
                .find(|sm| models_are_equal(Some(&sm.model), Some(saved)))
        });
        let chosen = saved_in_scope.unwrap_or(&scoped_models[0]);
        options.model = Some(chosen.model.clone());
        if parsed.thinking.is_none()
            && let Some(level) = chosen.thinking_level
        {
            options.thinking_level = Some(model_to_agent_thinking(level));
        }
    }

    // Explicit --thinking takes precedence over scoped model thinking.
    if let Some(thinking) = parsed.thinking {
        options.thinking_level = Some(model_to_agent_thinking(cli_thinking_to_model(thinking)));
    }

    // Scoped models for Ctrl+P cycling; unset thinking level means
    // "inherit current session thinking level" during cycling.
    options.scoped_models = scoped_models
        .iter()
        .map(|sm| ScopedModel {
            model: sm.model.clone(),
            thinking_level: sm.thinking_level.map(model_to_agent_thinking),
        })
        .collect();

    // Tools.
    if parsed.no_tools {
        options.no_tools_all = true;
    } else if parsed.no_builtin_tools {
        options.no_builtin_tools = true;
    }
    if let Some(tools) = &parsed.tools {
        options.tools = Some(tools.clone());
    }
    if let Some(exclude) = &parsed.exclude_tools {
        options.exclude_tools = Some(exclude.clone());
    }

    BuildSessionOptionsResult {
        options,
        cli_thinking_from_model,
        diagnostics,
    }
}

/// Format a token count as human-readable (200000 -> "200K", 1000000 -> "1M").
fn format_token_count(count: u64) -> String {
    if count >= 1_000_000 {
        let millions = count as f64 / 1_000_000.0;
        if millions.fract() == 0.0 {
            format!("{}M", millions as u64)
        } else {
            format!("{millions:.1}M")
        }
    } else if count >= 1_000 {
        let thousands = count as f64 / 1_000.0;
        if thousands.fract() == 0.0 {
            format!("{}K", thousands as u64)
        } else {
            format!("{thousands:.1}K")
        }
    } else {
        count.to_string()
    }
}

/// List available models, optionally fuzzy-filtered (port of
/// cli/list-models.ts `listModels`).
pub async fn list_models(model_registry: &ModelRegistry, search_pattern: Option<&str>) {
    if let Some(load_error) = model_registry.get_error() {
        eprintln!("\x1b[33mWarning: errors loading models.json:\n{load_error}\x1b[39m");
    }

    let models = model_registry.get_available().await;
    if models.is_empty() {
        println!("{}", format_no_models_available_message());
        return;
    }

    let mut filtered: Vec<&Model> = match search_pattern {
        Some(pattern) => {
            let searchable: Vec<(String, &Model)> = models
                .iter()
                .map(|m| (format!("{} {}", m.provider, m.id), m))
                .collect();
            fuzzy_filter(&searchable, pattern, |entry| entry.0.as_str())
                .into_iter()
                .map(|entry| entry.1)
                .collect()
        }
        None => models.iter().collect(),
    };

    if filtered.is_empty() {
        println!(
            "No models matching \"{}\"",
            search_pattern.unwrap_or_default()
        );
        return;
    }

    filtered.sort_by(|a, b| a.provider.cmp(&b.provider).then_with(|| a.id.cmp(&b.id)));

    struct Row {
        provider: String,
        model: String,
        context: String,
        max_out: String,
        thinking: &'static str,
        images: &'static str,
    }
    let rows: Vec<Row> = filtered
        .iter()
        .map(|m| Row {
            provider: m.provider.to_string(),
            model: m.id.clone(),
            context: format_token_count(m.context_window),
            max_out: format_token_count(m.max_tokens),
            thinking: if m.reasoning { "yes" } else { "no" },
            images: if m.input.contains(&ModelInput::Image) {
                "yes"
            } else {
                "no"
            },
        })
        .collect();

    let headers = [
        "provider", "model", "context", "max-out", "thinking", "images",
    ];
    let mut widths: [usize; 6] = headers.map(str::len);
    for row in &rows {
        widths[0] = widths[0].max(row.provider.len());
        widths[1] = widths[1].max(row.model.len());
        widths[2] = widths[2].max(row.context.len());
        widths[3] = widths[3].max(row.max_out.len());
        widths[4] = widths[4].max(row.thinking.len());
        widths[5] = widths[5].max(row.images.len());
    }

    let format_line = |cells: [&str; 6]| {
        cells
            .iter()
            .zip(widths.iter())
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!("{}", format_line(headers));
    for row in &rows {
        println!(
            "{}",
            format_line([
                &row.provider,
                &row.model,
                &row.context,
                &row.max_out,
                row.thinking,
                row.images,
            ])
        );
    }
}
