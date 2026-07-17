//! Three-line interactive footer ported from `footer.ts`.

use std::path::{Path, PathBuf};

use pi_tui::component::{Component, RenderStatus};
use pi_tui::line::Line;
use pi_tui::util::{truncate_to_width, visible_width};

use crate::modes::interactive::theme::{ThemeColor, theme};

#[derive(Clone, Debug, Default)]
pub struct FooterStats {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    pub context_percent: Option<f64>,
    pub context_window: u64,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub reasoning: bool,
    pub thinking_level: Option<String>,
    pub using_subscription: bool,
    pub experimental: bool,
}

/// Dynamic footer data. Closures are evaluated on every render, matching the
/// oracle's read-only footer data provider and session state getters.
pub struct FooterData {
    pub cwd: Box<dyn Fn() -> String>,
    pub git_branch: Box<dyn Fn() -> Option<String>>,
    pub session_name: Box<dyn Fn() -> Option<String>>,
    pub stats: Box<dyn Fn() -> FooterStats>,
    pub extension_statuses: Box<dyn Fn() -> Vec<(String, String)>>,
    pub available_provider_count: Box<dyn Fn() -> usize>,
}

fn to_fixed(value: f64, decimals: usize) -> String {
    let factor = 10_f64.powi(decimals as i32);
    format!("{:.*}", decimals, (value * factor).round() / factor)
}

#[must_use]
pub fn format_tokens(count: u64) -> String {
    match count {
        0..=999 => count.to_string(),
        1_000..=9_999 => format!("{}k", to_fixed(count as f64 / 1_000.0, 1)),
        10_000..=999_999 => format!("{}k", (count + 500) / 1_000),
        1_000_000..=9_999_999 => format!("{}M", to_fixed(count as f64 / 1_000_000.0, 1)),
        _ => format!("{}M", (count + 500_000) / 1_000_000),
    }
}

fn resolve_lexically(path: &Path) -> PathBuf {
    let mut resolved = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_default()
    };
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

#[must_use]
pub fn format_cwd_for_footer(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return cwd.to_owned();
    };
    let cwd_path = resolve_lexically(Path::new(cwd));
    let home_path = resolve_lexically(Path::new(home));
    match cwd_path.strip_prefix(&home_path) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Ok(relative) => PathBuf::from("~").join(relative).display().to_string(),
        Err(_) => cwd.to_owned(),
    }
}

fn sanitize_status_text(text: &str) -> String {
    text.replace(['\r', '\n', '\t'], " ")
        .split(' ')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

fn truncate_with_indicator(text: &str, width: usize, indicator: &str) -> String {
    if visible_width(text) <= width {
        return text.to_owned();
    }
    let indicator_width = visible_width(indicator).min(width);
    let mut result = truncate_to_width(text, width.saturating_sub(indicator_width));
    result.push_str(&truncate_to_width(indicator, indicator_width));
    result
}

pub struct FooterComponent {
    data: FooterData,
    auto_compact_enabled: bool,
    cached: Vec<Line>,
}

impl FooterComponent {
    #[must_use]
    pub fn new(data: FooterData) -> Self {
        Self {
            data,
            auto_compact_enabled: true,
            cached: Vec::new(),
        }
    }

    pub fn set_auto_compact_enabled(&mut self, enabled: bool) {
        self.auto_compact_enabled = enabled;
    }

    /// Mutable data-provider access (extension statuses bind post-construction).
    pub fn data_mut(&mut self) -> &mut FooterData {
        &mut self.data
    }

    /// Read a branch or detached commit directly from `.git/HEAD`.
    #[must_use]
    pub fn read_git_branch(cwd: &Path) -> Option<String> {
        let head = std::fs::read_to_string(cwd.join(".git/HEAD")).ok()?;
        Some(
            head.strip_prefix("ref: refs/heads/")
                .unwrap_or(&head)
                .trim()
                .to_owned(),
        )
        .filter(|branch| !branch.is_empty())
    }
}

impl Component for FooterComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        let width = usize::from(width);
        let stats = (self.data.stats)();
        let mut pwd = format_cwd_for_footer(
            &(self.data.cwd)(),
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok()
                .as_deref(),
        );
        if let Some(branch) = (self.data.git_branch)() {
            pwd.push_str(&format!(" ({branch})"));
        }
        if let Some(name) = (self.data.session_name)() {
            pwd.push_str(&format!(" • {name}"));
        }

        let mut parts = Vec::new();
        if stats.input > 0 {
            parts.push(format!("↑{}", format_tokens(stats.input)));
        }
        if stats.output > 0 {
            parts.push(format!("↓{}", format_tokens(stats.output)));
        }
        if stats.cache_read > 0 {
            parts.push(format!("R{}", format_tokens(stats.cache_read)));
        }
        if stats.cache_write > 0 {
            parts.push(format!("W{}", format_tokens(stats.cache_write)));
        }
        let latest_prompt_tokens = stats.input + stats.cache_read + stats.cache_write;
        if (stats.cache_read > 0 || stats.cache_write > 0) && latest_prompt_tokens > 0 {
            parts.push(format!(
                "CH{}%",
                to_fixed(
                    stats.cache_read as f64 / latest_prompt_tokens as f64 * 100.0,
                    1,
                )
            ));
        }
        if stats.cost != 0.0 || stats.using_subscription {
            parts.push(format!(
                "${}{}",
                to_fixed(stats.cost, 3),
                if stats.using_subscription {
                    " (sub)"
                } else {
                    ""
                }
            ));
        }

        let auto = if self.auto_compact_enabled {
            " (auto)"
        } else {
            ""
        };
        let context_display = stats.context_percent.map_or_else(
            || {
                format!(
                    "?/{window}{auto}",
                    window = format_tokens(stats.context_window)
                )
            },
            |percent| {
                format!(
                    "{}%/{window}{auto}",
                    to_fixed(percent, 1),
                    window = format_tokens(stats.context_window)
                )
            },
        );
        let context_display = match stats.context_percent {
            Some(percent) if percent > 90.0 => theme().fg(ThemeColor::Error, &context_display),
            Some(percent) if percent > 70.0 => theme().fg(ThemeColor::Warning, &context_display),
            _ => context_display,
        };
        parts.push(context_display);
        if stats.experimental {
            parts.push(format!(
                "{} {}",
                theme().fg(ThemeColor::Dim, "•"),
                theme().bold(&theme().fg(ThemeColor::Warning, "xp"))
            ));
        }

        let mut stats_left = parts.join(" ");
        if visible_width(&stats_left) > width {
            stats_left = truncate_with_indicator(&stats_left, width, "...");
        }
        let stats_left_width = visible_width(&stats_left);

        let model_name = stats.model.as_deref().unwrap_or("no-model");
        let right_without_provider = if stats.reasoning {
            match stats.thinking_level.as_deref().unwrap_or("off") {
                "off" => format!("{model_name} • thinking off"),
                level => format!("{model_name} • {level}"),
            }
        } else {
            model_name.to_owned()
        };
        let mut right = right_without_provider.clone();
        if (self.data.available_provider_count)() > 1
            && stats.model.is_some()
            && let Some(provider) = stats.provider.as_deref()
        {
            let with_provider = format!("({provider}) {right_without_provider}");
            if stats_left_width + 2 + visible_width(&with_provider) <= width {
                right = with_provider;
            }
        }

        let right_width = visible_width(&right);
        let stats_line = if stats_left_width + 2 + right_width <= width {
            format!(
                "{stats_left}{}{right}",
                " ".repeat(width - stats_left_width - right_width)
            )
        } else {
            let available = width.saturating_sub(stats_left_width + 2);
            if available == 0 {
                stats_left.clone()
            } else {
                let right = truncate_to_width(&right, available);
                format!(
                    "{stats_left}{}{right}",
                    " ".repeat(width.saturating_sub(stats_left_width + visible_width(&right)))
                )
            }
        };
        let remainder = &stats_line[stats_left.len().min(stats_line.len())..];
        let stats_line = format!(
            "{}{}",
            theme().fg(ThemeColor::Dim, &stats_left),
            theme().fg(ThemeColor::Dim, remainder)
        );

        let pwd_line = truncate_with_indicator(
            &theme().fg(ThemeColor::Dim, &pwd),
            width,
            &theme().fg(ThemeColor::Dim, "..."),
        );
        let mut statuses = (self.data.extension_statuses)();
        statuses.sort_by(|a, b| a.0.cmp(&b.0));
        let status_line = statuses
            .into_iter()
            .map(|(_, status)| sanitize_status_text(&status))
            .collect::<Vec<_>>()
            .join(" ");
        let status_line =
            truncate_with_indicator(&status_line, width, &theme().fg(ThemeColor::Dim, "..."));

        self.cached = [pwd_line, stats_line, status_line]
            .into_iter()
            .map(|line| Line::from_ansi(&line))
            .collect();
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}
