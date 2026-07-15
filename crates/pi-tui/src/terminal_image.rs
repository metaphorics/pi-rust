//! Terminal image protocol helpers — port of `packages/tui/src/terminal-image.ts`.
//!
//! Capability detection from env, Kitty/iTerm2 encode, cell-size math, and
//! optional decode/resize via the `image` crate.

use std::env;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::imageops::FilterType;
use image::{GenericImageView, ImageFormat, ImageReader};

/// Graphics protocol supported by the current terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    ITerm2,
    None,
}

impl ImageProtocol {
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            ImageProtocol::Kitty => Some("kitty"),
            ImageProtocol::ITerm2 => Some("iterm2"),
            ImageProtocol::None => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub images: ImageProtocol,
    pub true_color: bool,
    pub hyperlinks: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

impl Default for CellDimensions {
    fn default() -> Self {
        Self {
            width_px: 9,
            height_px: 18,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImageRenderOptions {
    pub max_width_cells: Option<u32>,
    pub max_height_cells: Option<u32>,
    pub preserve_aspect_ratio: Option<bool>,
    /// Kitty image ID. If provided, reuses/replaces existing image with this ID.
    pub image_id: Option<u32>,
    /// Whether Kitty should apply its default cursor movement after placement.
    pub move_cursor: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCellSize {
    pub columns: u32,
    pub rows: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedImage {
    pub sequence: String,
    pub rows: u32,
    pub image_id: Option<u32>,
}

const KITTY_PREFIX: &str = "\x1b_G";
const ITERM2_PREFIX: &str = "\x1b]1337;File=";

static CAPABILITIES: Mutex<Option<TerminalCapabilities>> = Mutex::new(None);
static CELL_DIMENSIONS: Mutex<CellDimensions> = Mutex::new(CellDimensions {
    width_px: 9,
    height_px: 18,
});
static NEXT_IMAGE_ID: AtomicU32 = AtomicU32::new(1);

fn env_lower(key: &str) -> String {
    env::var(key).unwrap_or_default().to_ascii_lowercase()
}

/// Probe whether the attached tmux client forwards OSC 8 hyperlinks.
/// Defaults to `false` on any error (matches TS `probeTmuxHyperlinks`).
pub fn probe_tmux_hyperlinks() -> bool {
    use std::process::Command;
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout);
            s.split(',').map(str::trim).any(|f| f == "hyperlinks")
        }
        _ => false,
    }
}

/// Detect image/true-color/hyperlink capabilities from environment variables.
pub fn detect_capabilities(tmux_forwards_hyperlink: impl FnOnce() -> bool) -> TerminalCapabilities {
    let term_program = env_lower("TERM_PROGRAM");
    let terminal_emulator = env_lower("TERMINAL_EMULATOR");
    let term = env_lower("TERM");
    let color_term = env_lower("COLORTERM");
    let has_true_color_hint = color_term == "truecolor" || color_term == "24bit";

    // tmux: image protocols unreliable; OSC 8 only when client confirms.
    if env::var_os("TMUX").is_some() || term.starts_with("tmux") {
        return TerminalCapabilities {
            images: ImageProtocol::None,
            true_color: has_true_color_hint,
            hyperlinks: tmux_forwards_hyperlink(),
        };
    }

    if term.starts_with("screen") {
        return TerminalCapabilities {
            images: ImageProtocol::None,
            true_color: has_true_color_hint,
            hyperlinks: false,
        };
    }

    if env::var_os("KITTY_WINDOW_ID").is_some() || term_program == "kitty" {
        return TerminalCapabilities {
            images: ImageProtocol::Kitty,
            true_color: true,
            hyperlinks: true,
        };
    }

    if term_program == "ghostty"
        || term.contains("ghostty")
        || env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return TerminalCapabilities {
            images: ImageProtocol::Kitty,
            true_color: true,
            hyperlinks: true,
        };
    }

    if env::var_os("WEZTERM_PANE").is_some() || term_program == "wezterm" {
        return TerminalCapabilities {
            images: ImageProtocol::Kitty,
            true_color: true,
            hyperlinks: true,
        };
    }

    if term_program == "warpterminal"
        || env::var_os("WARP_SESSION_ID").is_some()
        || env::var_os("WARP_TERMINAL_SESSION_UUID").is_some()
    {
        return TerminalCapabilities {
            images: ImageProtocol::Kitty,
            true_color: true,
            hyperlinks: true,
        };
    }

    if env::var_os("ITERM_SESSION_ID").is_some() || term_program == "iterm.app" {
        return TerminalCapabilities {
            images: ImageProtocol::ITerm2,
            true_color: true,
            hyperlinks: true,
        };
    }

    if env::var_os("WT_SESSION").is_some() {
        return TerminalCapabilities {
            images: ImageProtocol::None,
            true_color: true,
            hyperlinks: true,
        };
    }

    if term_program == "vscode" {
        return TerminalCapabilities {
            images: ImageProtocol::None,
            true_color: true,
            hyperlinks: true,
        };
    }

    if term_program == "alacritty" {
        return TerminalCapabilities {
            images: ImageProtocol::None,
            true_color: true,
            hyperlinks: true,
        };
    }

    if terminal_emulator == "jetbrains-jediterm" {
        return TerminalCapabilities {
            images: ImageProtocol::None,
            true_color: true,
            hyperlinks: false,
        };
    }

    // Unknown terminal: conservative defaults.
    TerminalCapabilities {
        images: ImageProtocol::None,
        true_color: has_true_color_hint,
        hyperlinks: false,
    }
}

pub fn get_capabilities() -> TerminalCapabilities {
    let mut guard = CAPABILITIES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(caps) = *guard {
        return caps;
    }
    let caps = detect_capabilities(probe_tmux_hyperlinks);
    *guard = Some(caps);
    caps
}

pub fn reset_capabilities_cache() {
    let mut guard = CAPABILITIES.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// Override the cached capabilities (tests).
pub fn set_capabilities(caps: TerminalCapabilities) {
    let mut guard = CAPABILITIES.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(caps);
}

pub fn get_cell_dimensions() -> CellDimensions {
    *CELL_DIMENSIONS.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn set_cell_dimensions(dims: CellDimensions) {
    *CELL_DIMENSIONS.lock().unwrap_or_else(|e| e.into_inner()) = dims;
}

/// True when a rendered line embeds Kitty or iTerm2 graphics sequences.
pub fn is_image_line(line: &str) -> bool {
    if line.starts_with(KITTY_PREFIX) || line.starts_with(ITERM2_PREFIX) {
        return true;
    }
    line.contains(KITTY_PREFIX) || line.contains(ITERM2_PREFIX)
}

/// Random image ID in `[1, 0xffff_fffe]` for Kitty graphics protocol.
pub fn allocate_image_id() -> u32 {
    // Prefer a monotonic id for determinism in tests; still non-zero and unique
    // within a process. Fall back wrap-around stays in range.
    let id = NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 || id == u32::MAX {
        NEXT_IMAGE_ID.store(1, Ordering::Relaxed);
        1
    } else {
        id
    }
}

/// Encode base64 image data as Kitty graphics protocol sequence(s).
pub fn encode_kitty(
    base64_data: &str,
    columns: Option<u32>,
    rows: Option<u32>,
    image_id: Option<u32>,
    move_cursor: Option<bool>,
) -> String {
    const CHUNK_SIZE: usize = 4096;

    let mut params = vec!["a=T".to_string(), "f=100".to_string(), "q=2".to_string()];
    if move_cursor == Some(false) {
        params.push("C=1".to_string());
    }
    if let Some(c) = columns {
        params.push(format!("c={c}"));
    }
    if let Some(r) = rows {
        params.push(format!("r={r}"));
    }
    if let Some(i) = image_id {
        params.push(format!("i={i}"));
    }

    if base64_data.len() <= CHUNK_SIZE {
        return format!("\x1b_G{};{base64_data}\x1b\\", params.join(","));
    }

    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut is_first = true;
    while offset < base64_data.len() {
        let end = (offset + CHUNK_SIZE).min(base64_data.len());
        let chunk = &base64_data[offset..end];
        let is_last = end >= base64_data.len();
        if is_first {
            chunks.push(format!("\x1b_G{},m=1;{chunk}\x1b\\", params.join(",")));
            is_first = false;
        } else if is_last {
            chunks.push(format!("\x1b_Gm=0;{chunk}\x1b\\"));
        } else {
            chunks.push(format!("\x1b_Gm=1;{chunk}\x1b\\"));
        }
        offset = end;
    }
    chunks.join("")
}

/// Delete a Kitty graphics image by ID (also frees image data).
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

/// Delete all visible Kitty graphics images.
pub fn delete_all_kitty_images() -> String {
    "\x1b_Ga=d,d=A,q=2\x1b\\".to_string()
}

/// Encode base64 image data as iTerm2 inline image sequence.
pub fn encode_iterm2(
    base64_data: &str,
    width: Option<String>,
    height: Option<String>,
    name: Option<&str>,
    preserve_aspect_ratio: Option<bool>,
    inline: Option<bool>,
) -> String {
    let mut params = vec![format!(
        "inline={}",
        if inline != Some(false) { 1 } else { 0 }
    )];
    if let Some(w) = width {
        params.push(format!("width={w}"));
    }
    if let Some(h) = height {
        params.push(format!("height={h}"));
    }
    if let Some(n) = name {
        let name_b64 = STANDARD.encode(n.as_bytes());
        params.push(format!("name={name_b64}"));
    }
    if preserve_aspect_ratio == Some(false) {
        params.push("preserveAspectRatio=0".to_string());
    }
    format!("\x1b]1337;File={}:{base64_data}\x07", params.join(";"))
}

pub fn calculate_image_cell_size(
    image_dimensions: ImageDimensions,
    max_width_cells: u32,
    max_height_cells: Option<u32>,
    cell_dimensions: CellDimensions,
) -> ImageCellSize {
    let max_width = max_width_cells.max(1);
    let max_height = max_height_cells.map(|h| h.max(1));
    let image_width = image_dimensions.width_px.max(1) as f64;
    let image_height = image_dimensions.height_px.max(1) as f64;
    let cell_w = cell_dimensions.width_px.max(1) as f64;
    let cell_h = cell_dimensions.height_px.max(1) as f64;

    let width_scale = (max_width as f64 * cell_w) / image_width;
    let height_scale = match max_height {
        Some(mh) => (mh as f64 * cell_h) / image_height,
        None => width_scale,
    };
    let scale = width_scale.min(height_scale);

    let scaled_w = image_width * scale;
    let scaled_h = image_height * scale;
    let columns = (scaled_w / cell_w).ceil() as u32;
    let rows = (scaled_h / cell_h).ceil() as u32;

    ImageCellSize {
        columns: columns.max(1).min(max_width),
        rows: match max_height {
            Some(mh) => rows.max(1).min(mh),
            None => rows.max(1),
        },
    }
}

pub fn calculate_image_rows(
    image_dimensions: ImageDimensions,
    target_width_cells: u32,
    cell_dimensions: CellDimensions,
) -> u32 {
    calculate_image_cell_size(image_dimensions, target_width_cells, None, cell_dimensions).rows
}

fn decode_base64(data: &str) -> Option<Vec<u8>> {
    STANDARD.decode(data.trim()).ok()
}

pub fn get_png_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buf = decode_base64(base64_data)?;
    if buf.len() < 24 {
        return None;
    }
    if buf[0] != 0x89 || buf[1] != 0x50 || buf[2] != 0x4e || buf[3] != 0x47 {
        return None;
    }
    let width = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
    let height = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]);
    Some(ImageDimensions {
        width_px: width,
        height_px: height,
    })
}

pub fn get_jpeg_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buf = decode_base64(base64_data)?;
    if buf.len() < 2 || buf[0] != 0xff || buf[1] != 0xd8 {
        return None;
    }
    let mut offset = 2usize;
    while offset + 9 < buf.len() {
        if buf[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = buf[offset + 1];
        if (0xc0..=0xc2).contains(&marker) {
            let height = u16::from_be_bytes([buf[offset + 5], buf[offset + 6]]) as u32;
            let width = u16::from_be_bytes([buf[offset + 7], buf[offset + 8]]) as u32;
            return Some(ImageDimensions {
                width_px: width,
                height_px: height,
            });
        }
        if offset + 3 >= buf.len() {
            return None;
        }
        let length = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        if length < 2 {
            return None;
        }
        offset += 2 + length;
    }
    None
}

pub fn get_gif_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buf = decode_base64(base64_data)?;
    if buf.len() < 10 {
        return None;
    }
    let sig = std::str::from_utf8(&buf[0..6]).ok()?;
    if sig != "GIF87a" && sig != "GIF89a" {
        return None;
    }
    let width = u16::from_le_bytes([buf[6], buf[7]]) as u32;
    let height = u16::from_le_bytes([buf[8], buf[9]]) as u32;
    Some(ImageDimensions {
        width_px: width,
        height_px: height,
    })
}

pub fn get_webp_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buf = decode_base64(base64_data)?;
    if buf.len() < 30 {
        return None;
    }
    if &buf[0..4] != b"RIFF" || &buf[8..12] != b"WEBP" {
        return None;
    }
    let chunk = &buf[12..16];
    if chunk == b"VP8 " {
        let width = (u16::from_le_bytes([buf[26], buf[27]]) & 0x3fff) as u32;
        let height = (u16::from_le_bytes([buf[28], buf[29]]) & 0x3fff) as u32;
        return Some(ImageDimensions {
            width_px: width,
            height_px: height,
        });
    }
    if chunk == b"VP8L" {
        if buf.len() < 25 {
            return None;
        }
        let bits = u32::from_le_bytes([buf[21], buf[22], buf[23], buf[24]]);
        let width = (bits & 0x3fff) + 1;
        let height = ((bits >> 14) & 0x3fff) + 1;
        return Some(ImageDimensions {
            width_px: width,
            height_px: height,
        });
    }
    if chunk == b"VP8X" {
        let width =
            (u32::from(buf[24]) | (u32::from(buf[25]) << 8) | (u32::from(buf[26]) << 16)) + 1;
        let height =
            (u32::from(buf[27]) | (u32::from(buf[28]) << 8) | (u32::from(buf[29]) << 16)) + 1;
        return Some(ImageDimensions {
            width_px: width,
            height_px: height,
        });
    }
    None
}

pub fn get_image_dimensions(base64_data: &str, mime_type: &str) -> Option<ImageDimensions> {
    match mime_type {
        "image/png" => get_png_dimensions(base64_data),
        "image/jpeg" => get_jpeg_dimensions(base64_data),
        "image/gif" => get_gif_dimensions(base64_data),
        "image/webp" => get_webp_dimensions(base64_data),
        _ => None,
    }
}

/// Decode raw image bytes (or base64) with the `image` crate and optionally
/// resize to fit `max_width_cells` × cell pixel width. Returns PNG base64 + dims.
pub fn decode_and_resize_to_png_base64(
    data: &[u8],
    max_width_px: Option<u32>,
    max_height_px: Option<u32>,
) -> Option<(String, ImageDimensions)> {
    let reader = ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;
    let (mut w, mut h) = img.dimensions();
    let mut out = img;
    if let (Some(mw), Some(mh)) = (max_width_px, max_height_px) {
        if w > mw || h > mh {
            out = out.resize(mw, mh, FilterType::Triangle);
            (w, h) = out.dimensions();
        }
    } else if let Some(mw) = max_width_px {
        if w > mw {
            out = out.resize(mw, u32::MAX, FilterType::Triangle);
            (w, h) = out.dimensions();
        }
    } else if let Some(mh) = max_height_px
        && h > mh {
            out = out.resize(u32::MAX, mh, FilterType::Triangle);
            (w, h) = out.dimensions();
        }
    let mut buf = Vec::new();
    out.write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
        .ok()?;
    Some((
        STANDARD.encode(&buf),
        ImageDimensions {
            width_px: w,
            height_px: h,
        },
    ))
}

/// Render image for the detected protocol. Returns `None` when images unsupported.
pub fn render_image(
    base64_data: &str,
    image_dimensions: ImageDimensions,
    options: ImageRenderOptions,
) -> Option<RenderedImage> {
    let caps = get_capabilities();
    if caps.images == ImageProtocol::None {
        return None;
    }

    let max_width = options.max_width_cells.unwrap_or(80);
    let size = calculate_image_cell_size(
        image_dimensions,
        max_width,
        options.max_height_cells,
        get_cell_dimensions(),
    );

    match caps.images {
        ImageProtocol::Kitty => {
            let sequence = encode_kitty(
                base64_data,
                Some(size.columns),
                Some(size.rows),
                options.image_id,
                options.move_cursor,
            );
            Some(RenderedImage {
                sequence,
                rows: size.rows,
                image_id: options.image_id,
            })
        }
        ImageProtocol::ITerm2 => {
            let sequence = encode_iterm2(
                base64_data,
                Some(size.columns.to_string()),
                Some("auto".to_string()),
                None,
                options.preserve_aspect_ratio.or(Some(true)),
                None,
            );
            Some(RenderedImage {
                sequence,
                rows: size.rows,
                image_id: None,
            })
        }
        ImageProtocol::None => None,
    }
}

/// Wrap text in an OSC 8 hyperlink sequence.
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

pub fn image_fallback(
    mime_type: &str,
    dimensions: Option<ImageDimensions>,
    filename: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if let Some(f) = filename {
        parts.push(f.to_string());
    }
    parts.push(format!("[{mime_type}]"));
    if let Some(d) = dimensions {
        parts.push(format!("{}x{}", d.width_px, d.height_px));
    }
    format!("[Image: {}]", parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_image_line_detects_prefixes() {
        assert!(is_image_line("\x1b_Ga=T;abc\x1b\\"));
        assert!(is_image_line("\x1b]1337;File=inline=1:abc\x07"));
        assert!(!is_image_line("plain text"));
    }

    #[test]
    fn delete_kitty_image_format() {
        assert_eq!(delete_kitty_image(42), "\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
    }

    #[test]
    fn cell_size_respects_max_width() {
        let size = calculate_image_cell_size(
            ImageDimensions {
                width_px: 900,
                height_px: 180,
            },
            10,
            None,
            CellDimensions {
                width_px: 9,
                height_px: 18,
            },
        );
        assert!(size.columns <= 10);
        assert!(size.rows >= 1);
    }
}
