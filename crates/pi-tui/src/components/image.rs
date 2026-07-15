//! Image component — port of `packages/tui/src/components/image.ts`.
//!
//! Renders via [`crate::terminal_image::render_image`], caches by width,
//! returns [`RenderStatus::Unchanged`] on cache hit.

use std::sync::Arc;

use crate::component::{Component, RenderStatus};
use crate::line::Line;
use crate::terminal_image::{
    ImageDimensions, ImageProtocol, ImageRenderOptions, allocate_image_id, get_capabilities,
    get_cell_dimensions, get_image_dimensions, image_fallback, render_image,
};

/// Theme for image fallback text.
#[derive(Clone)]
pub struct ImageTheme {
    pub fallback_color: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl Default for ImageTheme {
    fn default() -> Self {
        Self {
            fallback_color: Arc::new(|s: &str| format!("\x1b[2m{s}\x1b[22m")),
        }
    }
}

/// Options controlling image layout / Kitty reuse.
#[derive(Debug, Clone, Default)]
pub struct ImageOptions {
    pub max_width_cells: Option<u32>,
    pub max_height_cells: Option<u32>,
    pub filename: Option<String>,
    /// Kitty image ID. If provided, reuses this ID (for animations/updates).
    pub image_id: Option<u32>,
}

/// Terminal image widget.
pub struct Image {
    base64_data: String,
    mime_type: String,
    dimensions: ImageDimensions,
    theme: ImageTheme,
    options: ImageOptions,
    image_id: Option<u32>,
    cached_lines: Option<Vec<Line>>,
    cached_width: Option<u16>,
    last_status: RenderStatus,
}

impl Image {
    #[must_use]
    pub fn new(
        base64_data: impl Into<String>,
        mime_type: impl Into<String>,
        theme: ImageTheme,
        options: ImageOptions,
        dimensions: Option<ImageDimensions>,
    ) -> Self {
        let base64_data = base64_data.into();
        let mime_type = mime_type.into();
        let image_id = options.image_id;
        let dimensions = dimensions
            .or_else(|| get_image_dimensions(&base64_data, &mime_type))
            .unwrap_or(ImageDimensions {
                width_px: 800,
                height_px: 600,
            });
        Self {
            base64_data,
            mime_type,
            dimensions,
            theme,
            options,
            image_id,
            cached_lines: None,
            cached_width: None,
            last_status: RenderStatus::Changed,
        }
    }

    /// Kitty image ID used by this image (if any).
    #[must_use]
    pub fn image_id(&self) -> Option<u32> {
        self.image_id
    }

    fn invalidate_cache(&mut self) {
        self.cached_lines = None;
        self.cached_width = None;
    }

    fn rebuild(&mut self, width: u16) {
        let max_width = (width as u32)
            .saturating_sub(2)
            .max(1)
            .min(self.options.max_width_cells.unwrap_or(60));
        let cell = get_cell_dimensions();
        let default_max_height = (max_width * cell.width_px)
            .div_ceil(cell.height_px.max(1))
            .max(1);
        let max_height = self.options.max_height_cells.unwrap_or(default_max_height);

        let caps = get_capabilities();
        let lines: Vec<Line> = if caps.images != ImageProtocol::None {
            if caps.images == ImageProtocol::Kitty && self.image_id.is_none() {
                self.image_id = Some(allocate_image_id());
            }
            match render_image(
                &self.base64_data,
                self.dimensions,
                ImageRenderOptions {
                    max_width_cells: Some(max_width),
                    max_height_cells: Some(max_height),
                    preserve_aspect_ratio: None,
                    image_id: self.image_id,
                    move_cursor: Some(false),
                },
            ) {
                Some(result) => {
                    if let Some(id) = result.image_id {
                        self.image_id = Some(id);
                    }
                    let rows = result.rows.max(1) as usize;
                    match caps.images {
                        ImageProtocol::Kitty => {
                            // C=1 prevents cursor movement; pad with empty rows for height.
                            let mut out = vec![Line::image(result.sequence)];
                            for _ in 0..rows.saturating_sub(1) {
                                out.push(Line::plain(""));
                            }
                            out
                        }
                        ImageProtocol::ITerm2 => {
                            // First (rows-1) empty; last line moves up, draws, TUI accounts height.
                            let mut out = Vec::with_capacity(rows);
                            for _ in 0..rows.saturating_sub(1) {
                                out.push(Line::plain(""));
                            }
                            let row_offset = rows.saturating_sub(1);
                            let move_up = if row_offset > 0 {
                                format!("\x1b[{row_offset}A")
                            } else {
                                String::new()
                            };
                            out.push(Line::image(format!("{move_up}{}", result.sequence)));
                            out
                        }
                        ImageProtocol::None => unreachable!(),
                    }
                }
                None => {
                    let fallback = image_fallback(
                        &self.mime_type,
                        Some(self.dimensions),
                        self.options.filename.as_deref(),
                    );
                    vec![Line::from_ansi(&(self.theme.fallback_color)(&fallback))]
                }
            }
        } else {
            let fallback = image_fallback(
                &self.mime_type,
                Some(self.dimensions),
                self.options.filename.as_deref(),
            );
            vec![Line::from_ansi(&(self.theme.fallback_color)(&fallback))]
        };

        self.cached_lines = Some(lines);
        self.cached_width = Some(width);
        self.last_status = RenderStatus::Changed;
    }
}

impl Component for Image {
    fn render(&mut self, width: u16) -> &[Line] {
        if self.cached_width == Some(width) && self.cached_lines.is_some() {
            self.last_status = RenderStatus::Unchanged;
        } else {
            self.rebuild(width);
            self.last_status = RenderStatus::Changed;
        }
        self.cached_lines.as_deref().unwrap_or(&[])
    }

    fn invalidate(&mut self) {
        self.invalidate_cache();
    }

    fn last_render_status(&self) -> RenderStatus {
        self.last_status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_image::{TerminalCapabilities, set_capabilities};

    #[test]
    fn fallback_when_images_unsupported_and_caches() {
        set_capabilities(TerminalCapabilities {
            images: ImageProtocol::None,
            true_color: false,
            hyperlinks: false,
        });
        let mut img = Image::new(
            "",
            "image/png",
            ImageTheme::default(),
            ImageOptions {
                filename: Some("pic.png".into()),
                ..Default::default()
            },
            Some(ImageDimensions {
                width_px: 10,
                height_px: 10,
            }),
        );
        let lines = img.render(40);
        assert_eq!(lines.len(), 1);
        let plain = lines[0].plain_text();
        assert!(
            plain.contains("Image") || plain.contains("image/png") || plain.contains("pic"),
            "{plain}"
        );
        assert_eq!(img.last_render_status(), RenderStatus::Changed);
        let _ = img.render(40);
        assert_eq!(img.last_render_status(), RenderStatus::Unchanged);
        let _ = img.render(20);
        assert_eq!(img.last_render_status(), RenderStatus::Changed);
    }
}
