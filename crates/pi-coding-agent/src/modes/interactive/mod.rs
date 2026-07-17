//! Interactive TUI mode.
//!
//! Port of `packages/coding-agent/src/modes/interactive/`: theme engine,
//! component set, and the InteractiveMode loop on pi-tui.

pub mod app_keybindings;
pub mod components;
pub mod dispatch;
pub mod extension_ui;
pub mod interactive_mode;
pub mod shared;
#[cfg(test)]
mod shared_cache_tests;
pub mod theme;
pub mod trust_store;
