//! pi-tui: Component(render→lines) widgets on inkferro-core / inkferro-rt.
//!
//! Port of packages/tui.

#![allow(clippy::type_complexity)]

pub mod autocomplete;
pub mod clipboard;
pub mod component;
pub mod components;
pub mod fuzzy;
pub mod keybindings;
pub mod keys;
pub mod kill_ring;
pub mod line;
pub mod terminal;
pub mod terminal_image;
pub mod tui;
pub mod undo_stack;
pub mod util;
pub mod word_navigation;

pub use component::{Component, ComponentBox, Container, Focusable, RenderStatus};
pub use line::{Line, CURSOR_MARKER};
pub use tui::{Tui, VirtualTerminal};
