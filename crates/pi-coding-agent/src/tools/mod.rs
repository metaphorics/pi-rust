//! Built-in filesystem and shell tools, ported from pi 0.80.7.

use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    future::Future,
    io::Write,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock, Weak},
    time::Instant,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use parking_lot::Mutex;
use pi_agent::{AgentToolResult, ToolDefinition};
use pi_ai::{Content, ImageContent, TextContent};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex as AsyncMutex;

const MAX_LINES: usize = 2_000;
const MAX_BYTES: usize = 50 * 1024; // 50KB
const GREP_MAX_LINE_LENGTH: usize = 500;

// =========================================================================
// Path Resolution & Normalization
// =========================================================================

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                let mut popped = false;
                if let Some(last) = normalized.components().next_back() {
                    match last {
                        Component::Normal(_) => {
                            normalized.pop();
                            popped = true;
                        }
                        Component::RootDir | Component::Prefix(_) => {
                            popped = true;
                        }
                        _ => {}
                    }
                }
                if !popped {
                    normalized.push(Component::ParentDir);
                }
            }
            Component::CurDir => {}
            Component::Normal(c) => {
                normalized.push(c);
            }
            c => {
                normalized.push(c.as_os_str());
            }
        }
    }
    normalized
}

fn resolve(cwd: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    normalize_path(&joined)
}

// =========================================================================
// Global File Mutation Locking (per path, avoiding memory leaks)
// =========================================================================

static FILE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

async fn with_file_lock<T, F, Fut>(path: PathBuf, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let arc_lock = {
        let mut map = FILE_LOCKS.lock();
        map.retain(|_, v| v.strong_count() > 0);

        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());

        if let Some(weak) = map.get(&canonical) {
            if let Some(arc) = weak.upgrade() {
                arc
            } else {
                let arc = Arc::new(AsyncMutex::new(()));
                map.insert(canonical, Arc::downgrade(&arc));
                arc
            }
        } else {
            let arc = Arc::new(AsyncMutex::new(()));
            map.insert(canonical, Arc::downgrade(&arc));
            arc
        }
    };

    let _guard = arc_lock.lock().await;
    f().await
}

// =========================================================================
// Truncation Primitives & Size Formatting
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let first_line_bytes = if !lines.is_empty() { lines[0].len() } else { 0 };
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut output_lines_arr = Vec::new();
    let mut output_bytes_count = 0;
    let mut truncated_by = TruncatedBy::Lines;

    for (i, line) in lines.iter().enumerate() {
        if i >= max_lines {
            truncated_by = TruncatedBy::Lines;
            break;
        }
        let line_bytes = line.len() + if i > 0 { 1 } else { 0 };
        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output_lines_arr.push(*line);
        output_bytes_count += line_bytes;
    }

    if output_lines_arr.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_content = output_lines_arr.join("\n");
    let final_output_bytes = output_content.len();

    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: output_lines_arr.len(),
        output_bytes: final_output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let mut output_lines_arr = VecDeque::new();
    let mut output_bytes_count = 0;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;

    for i in (0..lines.len()).rev() {
        if output_lines_arr.len() >= max_lines {
            truncated_by = TruncatedBy::Lines;
            break;
        }
        let line = lines[i];
        let line_bytes = line.len() + if !output_lines_arr.is_empty() { 1 } else { 0 };

        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if output_lines_arr.is_empty() {
                let truncated_line = truncate_string_to_bytes_from_end(line, max_bytes);
                output_bytes_count = truncated_line.len();
                output_lines_arr.push_front(truncated_line);
                last_line_partial = true;
            }
            break;
        }

        output_lines_arr.push_front(line.to_string());
        output_bytes_count += line_bytes;
    }

    if output_lines_arr.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_lines_len = output_lines_arr.len();
    let output_content = output_lines_arr.into_iter().collect::<Vec<_>>().join("\n");
    let final_output_bytes = output_content.len();

    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: output_lines_len,
        output_bytes: final_output_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

fn truncate_string_to_bytes_from_end(str: &str, max_bytes: usize) -> String {
    let bytes = str.as_bytes();
    if bytes.len() <= max_bytes {
        return str.to_string();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    if start < bytes.len() {
        std::str::from_utf8(&bytes[start..]).map(|s| s.to_string()).unwrap_or_default()
    } else {
        String::new()
    }
}

fn truncate_line(line: &str, max_utf16_units: usize) -> (String, bool) {
    let mut utf16_count = 0;
    let mut char_idx_boundary = 0;
    let mut was_truncated = false;

    for c in line.chars() {
        let len_u16 = c.len_utf16();
        if utf16_count + len_u16 > max_utf16_units {
            was_truncated = true;
            break;
        }
        utf16_count += len_u16;
        char_idx_boundary += c.len_utf8();
    }

    if was_truncated {
        (format!("{}... [truncated]", &line[..char_idx_boundary]), true)
    } else {
        (line.to_string(), false)
    }
}

// =========================================================================
// Streaming UTF-8 Decoder
// =========================================================================

pub struct StreamingUtf8Decoder {
    leftover: Vec<u8>,
}

impl Default for StreamingUtf8Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingUtf8Decoder {
    pub fn new() -> Self {
        Self { leftover: Vec::new() }
    }

    pub fn decode(&mut self, chunk: &[u8], is_finished: bool) -> String {
        let mut buf = std::mem::take(&mut self.leftover);
        buf.extend_from_slice(chunk);

        if is_finished {
            return String::from_utf8_lossy(&buf).into_owned();
        }

        let mut decoded = String::new();
        let mut input = &buf[..];

        loop {
            match std::str::from_utf8(input) {
                Ok(s) => {
                    decoded.push_str(s);
                    break;
                }
                Err(e) => {
                    let valid_len = e.valid_up_to();
                    if valid_len > 0 {
                        decoded.push_str(std::str::from_utf8(&input[..valid_len]).unwrap());
                        input = &input[valid_len..];
                    }
                    if let Some(error_len) = e.error_len() {
                        decoded.push('\u{FFFD}');
                        input = &input[error_len..];
                    } else {
                        self.leftover = input.to_vec();
                        break;
                    }
                }
            }
        }
        decoded
    }
}

// =========================================================================
// Incremental Output Accumulator
// =========================================================================

pub struct OutputSnapshot {
    pub content: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<PathBuf>,
}

pub struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    max_rolling_bytes: usize,
    temp_file_prefix: String,
    raw_chunks: Vec<Vec<u8>>,
    tail_text: String,
    tail_bytes: usize,
    tail_starts_at_line_boundary: bool,
    total_raw_bytes: usize,
    total_decoded_bytes: usize,
    completed_lines: usize,
    total_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,
    finished: bool,
    temp_file_path: Option<PathBuf>,
    temp_file: Option<File>,
}

impl OutputAccumulator {
    pub fn new(max_lines: Option<usize>, max_bytes: Option<usize>, temp_file_prefix: Option<&str>) -> Self {
        let max_lines = max_lines.unwrap_or(2000);
        let max_bytes = max_bytes.unwrap_or(50 * 1024);
        let max_rolling_bytes = std::cmp::max(max_bytes * 2, 1);
        let temp_file_prefix = temp_file_prefix.unwrap_or("pi-output").to_string();
        Self {
            max_lines,
            max_bytes,
            max_rolling_bytes,
            temp_file_prefix,
            raw_chunks: Vec::new(),
            tail_text: String::new(),
            tail_bytes: 0,
            tail_starts_at_line_boundary: true,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            total_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            finished: false,
            temp_file_path: None,
            temp_file: None,
        }
    }

    fn ensure_temp_file(&mut self) -> std::io::Result<()> {
        if self.temp_file_path.is_some() {
            return Ok(());
        }
        let named_temp_file = tempfile::Builder::new()
            .prefix(&format!("{}-", self.temp_file_prefix))
            .suffix(".log")
            .tempfile()?;

        let (file, path) = named_temp_file.keep().map_err(|e| e.error)?;
        let mut file_handle = file;
        for chunk in &self.raw_chunks {
            file_handle.write_all(chunk)?;
        }
        self.raw_chunks.clear();
        self.temp_file_path = Some(path);
        self.temp_file = Some(file_handle);
        Ok(())
    }

    fn should_use_temp_file(&self) -> bool {
        self.total_raw_bytes > self.max_bytes
            || self.total_decoded_bytes > self.max_bytes
            || self.total_lines > self.max_lines
    }

    pub fn append(&mut self, data: &[u8], decoder: &mut StreamingUtf8Decoder) -> std::io::Result<()> {
        if self.finished {
            return Err(std::io::Error::other(
                "Cannot append to a finished output accumulator",
            ));
        }

        self.total_raw_bytes += data.len();
        let decoded = decoder.decode(data, false);
        self.append_decoded_text(&decoded);

        if self.temp_file.is_some() || self.should_use_temp_file() {
            self.ensure_temp_file()?;
            if let Some(file) = &mut self.temp_file {
                file.write_all(data)?;
            }
        } else if !data.is_empty() {
            self.raw_chunks.push(data.to_vec());
        }
        Ok(())
    }

    pub fn finish(&mut self, decoder: &mut StreamingUtf8Decoder) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let decoded = decoder.decode(&[], true);
        self.append_decoded_text(&decoded);
        if self.should_use_temp_file() {
            let _ = self.ensure_temp_file();
        }
        Ok(())
    }

    pub fn close_temp_file(&mut self) {
        if let Some(mut file) = self.temp_file.take() {
            let _ = file.flush();
        }
    }

    pub fn get_last_line_bytes(&self) -> usize {
        self.current_line_bytes
    }

    pub fn snapshot(&mut self, persist_if_truncated: bool) -> OutputSnapshot {
        let mut tail_truncation = truncate_tail(self.get_snapshot_text(), self.max_lines, self.max_bytes);
        let truncated = self.total_lines > self.max_lines || self.total_decoded_bytes > self.max_bytes;
        let truncated_by = if truncated {
            Some(tail_truncation.truncated_by.unwrap_or(
                if self.total_decoded_bytes > self.max_bytes {
                    TruncatedBy::Bytes
                } else {
                    TruncatedBy::Lines
                }
            ))
        } else {
            None
        };

        tail_truncation.truncated = truncated;
        tail_truncation.truncated_by = truncated_by;
        tail_truncation.total_lines = self.total_lines;
        tail_truncation.total_bytes = self.total_decoded_bytes;
        tail_truncation.max_lines = self.max_lines;
        tail_truncation.max_bytes = self.max_bytes;

        if persist_if_truncated && tail_truncation.truncated {
            let _ = self.ensure_temp_file();
        }

        OutputSnapshot {
            content: tail_truncation.content.clone(),
            truncation: tail_truncation,
            full_output_path: self.temp_file_path.clone(),
        }
    }

    fn append_decoded_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        let bytes = text.len();
        self.total_decoded_bytes += bytes;
        self.tail_text.push_str(text);
        self.tail_bytes += bytes;
        if self.tail_bytes > self.max_rolling_bytes * 2 {
            self.trim_tail();
        }

        let mut newlines = 0;
        let mut last_newline = None;
        for (i, c) in text.char_indices() {
            if c == '\n' {
                newlines += 1;
                last_newline = Some(i);
            }
        }

        if newlines == 0 {
            self.current_line_bytes += bytes;
            self.has_open_line = true;
        } else {
            self.completed_lines += newlines;
            let tail = &text[last_newline.unwrap() + 1..];
            self.current_line_bytes = tail.len();
            self.has_open_line = !tail.is_empty();
        }
        self.total_lines = self.completed_lines + if self.has_open_line { 1 } else { 0 };
    }

    fn trim_tail(&mut self) {
        let buffer = self.tail_text.as_bytes();
        if buffer.len() <= self.max_rolling_bytes {
            self.tail_bytes = buffer.len();
            return;
        }

        let mut start = buffer.len() - self.max_rolling_bytes;
        while start < buffer.len() && (buffer[start] & 0xC0) == 0x80 {
            start += 1;
        }

        if start > 0 {
            self.tail_starts_at_line_boundary = buffer[start - 1] == b'\n';
        }

        self.tail_text = if start < buffer.len() {
            std::str::from_utf8(&buffer[start..]).unwrap_or("").to_string()
        } else {
            String::new()
        };
        self.tail_bytes = self.tail_text.len();
    }

    fn get_snapshot_text(&self) -> &str {
        if self.tail_starts_at_line_boundary {
            return &self.tail_text;
        }

        if let Some(first_newline) = self.tail_text.find('\n') {
            &self.tail_text[first_newline + 1..]
        } else {
            &self.tail_text
        }
    }
}

// =========================================================================
// JSDiff unified diff / patch simulation
// =========================================================================

struct Part<'a> {
    value: Vec<&'a str>,
    added: bool,
    removed: bool,
}

pub struct DiffStringResult {
    pub diff: String,
    pub first_changed_line: Option<usize>,
}

pub fn generate_diff_string(old_content: &str, new_content: &str, context_lines: usize) -> DiffStringResult {
    let diff_res = diff::lines(old_content, new_content);
    let mut parts: Vec<Part> = Vec::new();
    for res in diff_res {
        match res {
            diff::Result::Left(l) => {
                if !parts.is_empty() && parts.last().unwrap().removed {
                    parts.last_mut().unwrap().value.push(l);
                } else {
                    parts.push(Part {
                        value: vec![l],
                        added: false,
                        removed: true,
                    });
                }
            }
            diff::Result::Right(r) => {
                if !parts.is_empty() && parts.last().unwrap().added {
                    parts.last_mut().unwrap().value.push(r);
                } else {
                    parts.push(Part {
                        value: vec![r],
                        added: true,
                        removed: false,
                    });
                }
            }
            diff::Result::Both(b, _) => {
                let is_common = !parts.is_empty() && !parts.last().unwrap().added && !parts.last().unwrap().removed;
                if is_common {
                    parts.last_mut().unwrap().value.push(b);
                } else {
                    parts.push(Part {
                        value: vec![b],
                        added: false,
                        removed: false,
                    });
                }
            }
        }
    }

    let old_lines = split_lines_for_counting(old_content);
    let new_lines = split_lines_for_counting(new_content);
    let max_line_num = std::cmp::max(old_lines.len(), new_lines.len());
    let line_num_width = max_line_num.to_string().len();

    let mut output: Vec<String> = Vec::new();
    let mut old_line_num = 1;
    let mut new_line_num = 1;
    let mut last_was_change = false;
    let mut first_changed_line = None;

    for i in 0..parts.len() {
        let part = &parts[i];
        let raw = &part.value;

        if part.added || part.removed {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }

            for line in raw {
                if part.added {
                    let line_num = format!("{:>width$}", new_line_num, width = line_num_width);
                    output.push(format!("+{} {}", line_num, line));
                    new_line_num += 1;
                } else {
                    let line_num = format!("{:>width$}", old_line_num, width = line_num_width);
                    output.push(format!("-{} {}", line_num, line));
                    old_line_num += 1;
                }
            }
            last_was_change = true;
        } else {
            let next_part_is_change = i < parts.len() - 1 && (parts[i + 1].added || parts[i + 1].removed);
            let has_leading_change = last_was_change;
            let has_trailing_change = next_part_is_change;

            if has_leading_change && has_trailing_change {
                if raw.len() <= context_lines * 2 {
                    for line in raw {
                        let line_num = format!("{:>width$}", old_line_num, width = line_num_width);
                        output.push(format!(" {} {}", line_num, line));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    let leading_lines = &raw[..context_lines];
                    let trailing_lines = &raw[raw.len() - context_lines..];
                    let skipped_lines = raw.len() - leading_lines.len() - trailing_lines.len();

                    for line in leading_lines {
                        let line_num = format!("{:>width$}", old_line_num, width = line_num_width);
                        output.push(format!(" {} {}", line_num, line));
                        old_line_num += 1;
                        new_line_num += 1;
                    }

                    output.push(format!(" {} ...", " ".repeat(line_num_width)));
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;

                    for line in trailing_lines {
                        let line_num = format!("{:>width$}", old_line_num, width = line_num_width);
                        output.push(format!(" {} {}", line_num, line));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            } else if has_leading_change {
                let shown_len = std::cmp::min(raw.len(), context_lines);
                let shown_lines = &raw[..shown_len];
                let skipped_lines = raw.len() - shown_len;

                for line in shown_lines {
                    let line_num = format!("{:>width$}", old_line_num, width = line_num_width);
                    output.push(format!(" {} {}", line_num, line));
                    old_line_num += 1;
                    new_line_num += 1;
                }

                if skipped_lines > 0 {
                    output.push(format!(" {} ...", " ".repeat(line_num_width)));
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;
                }
            } else if has_trailing_change {
                let skipped_lines = raw.len().saturating_sub(context_lines);
                if skipped_lines > 0 {
                    output.push(format!(" {} ...", " ".repeat(line_num_width)));
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;
                }

                for line in &raw[skipped_lines..] {
                    let line_num = format!("{:>width$}", old_line_num, width = line_num_width);
                    output.push(format!(" {} {}", line_num, line));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            } else {
                old_line_num += raw.len();
                new_line_num += raw.len();
            }

            last_was_change = false;
        }
    }

    DiffStringResult {
        diff: output.join("\n"),
        first_changed_line,
    }
}

pub fn generate_unified_patch(path: &str, old_content: &str, new_content: &str, context_lines: usize) -> String {
    let diff_res = diff::lines(old_content, new_content);
    let mut parts: Vec<Part> = Vec::new();
    for res in diff_res {
        match res {
            diff::Result::Left(l) => {
                if !parts.is_empty() && parts.last().unwrap().removed {
                    parts.last_mut().unwrap().value.push(l);
                } else {
                    parts.push(Part {
                        value: vec![l],
                        added: false,
                        removed: true,
                    });
                }
            }
            diff::Result::Right(r) => {
                if !parts.is_empty() && parts.last().unwrap().added {
                    parts.last_mut().unwrap().value.push(r);
                } else {
                    parts.push(Part {
                        value: vec![r],
                        added: true,
                        removed: false,
                    });
                }
            }
            diff::Result::Both(b, _) => {
                let is_common = !parts.is_empty() && !parts.last().unwrap().added && !parts.last().unwrap().removed;
                if is_common {
                    parts.last_mut().unwrap().value.push(b);
                } else {
                    parts.push(Part {
                        value: vec![b],
                        added: false,
                        removed: false,
                    });
                }
            }
        }
    }

    struct Hunk {
        old_start: usize,
        old_len: usize,
        new_start: usize,
        new_len: usize,
        lines: Vec<String>,
    }

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current_hunk: Option<Hunk> = None;
    let mut pre_context: VecDeque<(usize, usize, String)> = VecDeque::new();
    let mut trailing_context: Vec<(usize, usize, String)> = Vec::new();

    let mut old_line_num = 1;
    let mut new_line_num = 1;

    let is_old_empty = old_content.is_empty();

    for part in &parts {
        if part.added || part.removed {
            if let Some(hunk) = &mut current_hunk {
                for (_, _, line) in trailing_context.drain(..) {
                    hunk.lines.push(format!(" {}", line));
                    hunk.old_len += 1;
                    hunk.new_len += 1;
                }
            } else {
                let old_start = if is_old_empty {
                    0
                } else {
                    pre_context.front().map(|x| x.0).unwrap_or(old_line_num)
                };
                let new_start = pre_context.front().map(|x| x.1).unwrap_or(new_line_num);
                let mut lines = Vec::new();
                let mut old_len = 0;
                let mut new_len = 0;
                for (_, _, line) in pre_context.drain(..) {
                    lines.push(format!(" {}", line));
                    old_len += 1;
                    new_len += 1;
                }
                current_hunk = Some(Hunk {
                    old_start,
                    old_len,
                    new_start,
                    new_len,
                    lines,
                });
            }

            let hunk = current_hunk.as_mut().unwrap();
            for line in &part.value {
                if part.added {
                    hunk.lines.push(format!("+{}", line));
                    hunk.new_len += 1;
                    new_line_num += 1;
                } else {
                    hunk.lines.push(format!("-{}", line));
                    hunk.old_len += 1;
                    old_line_num += 1;
                }
            }
        } else {
            for line in &part.value {
                let item = (old_line_num, new_line_num, line.to_string());
                old_line_num += 1;
                new_line_num += 1;

                if current_hunk.is_some() {
                    trailing_context.push(item);
                    if trailing_context.len() > context_lines * 2 {
                        let mut hunk = current_hunk.take().unwrap();
                        for (_, _, l) in trailing_context.drain(..context_lines) {
                            hunk.lines.push(format!(" {}", l));
                            hunk.old_len += 1;
                            hunk.new_len += 1;
                        }
                        hunks.push(hunk);

                        pre_context.clear();
                        for (o, n, l) in trailing_context.drain(..) {
                            pre_context.push_back((o, n, l));
                        }
                    }
                } else {
                    pre_context.push_back(item);
                    if pre_context.len() > context_lines {
                        pre_context.pop_front();
                    }
                }
            }
        }
    }

    if let Some(mut hunk) = current_hunk {
        let take_len = std::cmp::min(trailing_context.len(), context_lines);
        for (_, _, l) in trailing_context.drain(..take_len) {
            hunk.lines.push(format!(" {}", l));
            hunk.old_len += 1;
            hunk.new_len += 1;
        }
        hunks.push(hunk);
    }

    let mut patch_lines = vec![
        format!("--- {}", path),
        format!("+++ {}", path),
    ];
    for hunk in hunks {
        patch_lines.push(format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
        ));
        patch_lines.extend(hunk.lines);
    }
    patch_lines.join("\n") + "\n"
}

// =========================================================================
// Helpers for argument extracting
// =========================================================================

fn string_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Invalid {name}: expected a string"))
}

fn limit_arg(args: &Value, name: &str) -> Option<f64> {
    args.get(name)
        .and_then(Value::as_f64)
}

fn image_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

// =========================================================================
// Tool Definitions
// =========================================================================

/// JSON schema copied from `core/tools/read.ts`.
pub fn read_schema() -> Value {
    json!({"type":"object","properties":{"path":{"type":"string","description":"Path to the file to read (relative or absolute)"},"offset":{"type":"number","description":"Line number to start reading from (1-indexed)"},"limit":{"type":"number","description":"Maximum number of lines to read"}},"required":["path"]})
}

pub fn create_read_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "read".into(),
        label: "read".into(),
        description: "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.".into(),
        parameters: read_schema(),
        execution_mode: None,
        prepare_arguments: None,
        renderer: None,
        execute: Arc::new(move |_, args, cancellation_token, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                    return Err("Operation aborted".to_string());
                }

                let raw = string_arg(&args, "path")?;
                let target = resolve(&cwd, raw);

                if !target.exists() {
                    return Err(format!(
                        "ENOENT: no such file or directory, access '{}'",
                        target.display()
                    ));
                }

                if let Some(mime_type) = image_mime(&target) {
                    let data = fs::read(&target).map_err(|error| error.to_string())?;
                    return Ok(AgentToolResult {
                        content: vec![
                            Content::Text(TextContent {
                                text: format!("Read image file [{mime_type}]").into(),
                                text_signature: None,
                            }),
                            Content::Image(ImageContent {
                                data: BASE64.encode(data),
                                mime_type: mime_type.to_owned(),
                            }),
                        ],
                        details: Value::Object(Default::default()),
                        added_tool_names: None,
                        terminate: None,
                    });
                }

                let contents = fs::read_to_string(&target).map_err(|error| error.to_string())?;
                // Split by \n retaining empty elements to match TS string.split("\n")
                let all_lines: Vec<&str> = contents.split('\n').collect();
                let total_file_lines = all_lines.len();

                let offset_val = limit_arg(&args, "offset").unwrap_or(1.0) as usize;
                let start_line = if offset_val > 0 { offset_val - 1 } else { 0 };
                let start_line_display = start_line + 1;

                if start_line >= all_lines.len() {
                    return Err(format!(
                        "Offset {offset_val} is beyond end of file ({total_file_lines} lines total)"
                    ));
                }

                let limit_opt = limit_arg(&args, "limit").map(|v| v as usize);
                let mut user_limited_lines = None;

                let selected_content = if let Some(limit) = limit_opt {
                    let end_line = std::cmp::min(start_line + limit, all_lines.len());
                    user_limited_lines = Some(end_line - start_line);
                    all_lines[start_line..end_line].join("\n")
                } else {
                    all_lines[start_line..].join("\n")
                };

                let truncation = truncate_head(&selected_content, 2000, 50 * 1024);
                let mut details = json!({});

                let output_text = if truncation.first_line_exceeds_limit {
                    let first_line_size = format_size(all_lines[start_line].len());
                    details["truncation"] = serde_json::to_value(&truncation).unwrap_or(Value::Null);
                    format!(
                        "[Line {} is {}, exceeds {} limit. Use bash: sed -n '{}p' {} | head -c {}]",
                        start_line_display,
                        first_line_size,
                        format_size(50 * 1024),
                        start_line_display,
                        raw,
                        50 * 1024
                    )
                } else if truncation.truncated {
                    let end_line_display = start_line_display + truncation.output_lines - 1;
                    let next_offset = end_line_display + 1;
                    details["truncation"] = serde_json::to_value(&truncation).unwrap_or(Value::Null);
                    let mut text = truncation.content.clone();
                    if truncation.truncated_by == Some(TruncatedBy::Lines) {
                        text.push_str(&format!(
                            "\n\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
                            start_line_display, end_line_display, total_file_lines, next_offset
                        ));
                    } else {
                        text.push_str(&format!(
                            "\n\n[Showing lines {}-{} of {} ({} limit). Use offset={} to continue.]",
                            start_line_display, end_line_display, total_file_lines, format_size(50 * 1024), next_offset
                        ));
                    }
                    text
                } else if let Some(user_lim) = user_limited_lines {
                    if start_line + user_lim < all_lines.len() {
                        let remaining = all_lines.len() - (start_line + user_lim);
                        let next_offset = start_line + user_lim + 1;
                        format!(
                            "{}\n\n[{} more lines in file. Use offset={} to continue.]",
                            truncation.content, remaining, next_offset
                        )
                    } else {
                        truncation.content.clone()
                    }
                } else {
                    truncation.content.clone()
                };

                Ok(AgentToolResult {
                    content: vec![Content::Text(TextContent {
                        text: output_text.into(),
                        text_signature: None,
                    })],
                    details,
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
    }
}

/// JSON schema copied from `core/tools/write.ts`.
pub fn write_schema() -> Value {
    json!({"type":"object","properties":{"path":{"type":"string","description":"Path to the file to write (relative or absolute)"},"content":{"type":"string","description":"Content to write to the file"}},"required":["path","content"]})
}

pub fn create_write_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "write".into(),
        label: "write".into(),
        description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".into(),
        parameters: write_schema(),
        execution_mode: None,
        prepare_arguments: None,
        renderer: None,
        execute: Arc::new(move |_, args, cancellation_token, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = string_arg(&args, "path")?;
                let content = string_arg(&args, "content")?;
                let target = resolve(&cwd, path);

                with_file_lock(target.clone(), || async {
                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }

                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                    }

                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }

                    fs::write(&target, content).map_err(|error| error.to_string())?;

                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }

                    let utf16_len = content.encode_utf16().count();
                    Ok(AgentToolResult {
                        content: vec![Content::Text(TextContent {
                            text: format!("Successfully wrote {utf16_len} bytes to {path}").into(),
                            text_signature: None,
                        })],
                        details: Value::Object(Default::default()),
                        added_tool_names: None,
                        terminate: None,
                    })
                })
                .await
            })
        }),
    }
}

/// JSON schema copied from `core/tools/edit.ts`.
pub fn edit_schema() -> Value {
    json!({"type":"object","properties":{"path":{"type":"string","description":"Path to the file to edit (relative or absolute)"},"edits":{"type":"array","items":{"type":"object","properties":{"oldText":{"type":"string","description":"Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},"newText":{"type":"string","description":"Replacement text for this targeted edit."}},"required":["oldText","newText"]},"description":"One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."}},"required":["path","edits"]})
}

pub fn create_edit_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "edit".into(),
        label: "edit".into(),
        description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".into(),
        parameters: edit_schema(),
        execution_mode: None,
        prepare_arguments: None,
        renderer: None,
        execute: Arc::new(move |_, args, cancellation_token, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = string_arg(&args, "path")?;
                let edits_val = args.get("edits").and_then(Value::as_array).ok_or_else(|| "Edit tool input is invalid. edits must contain at least one replacement.".to_string())?;
                if edits_val.is_empty() {
                    return Err("Edit tool input is invalid. edits must contain at least one replacement.".into());
                }
                let target = resolve(&cwd, path);

                with_file_lock(target.clone(), || async {
                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }

                    if !target.exists() {
                        return Err(format!("Could not edit file: {path}. Error code: ENOENT."));
                    }

                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }

                    let original = fs::read_to_string(&target).map_err(|error| format!("Could not edit file: {path}. {error}."))?;

                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }

                    let bom = original.strip_prefix('\u{feff}').map_or("", |_| "\u{feff}");
                    let normalized = original.trim_start_matches('\u{feff}').replace("\r\n", "\n");
                    let mut matches = Vec::with_capacity(edits_val.len());

                    for (index, edit) in edits_val.iter().enumerate() {
                        let old = edit.get("oldText").and_then(Value::as_str).ok_or_else(|| format!("edits[{index}].oldText must not be empty in {path}."))?;
                        if old.is_empty() {
                            return Err(if edits_val.len() == 1 {
                                format!("oldText must not be empty in {path}.")
                            } else {
                                format!("edits[{index}].oldText must not be empty in {path}.")
                            });
                        }
                        let old_normalized = old.replace("\r\n", "\n");
                        let positions: Vec<_> = normalized.match_indices(&old_normalized).collect();
                        if positions.is_empty() {
                            return Err(if edits_val.len() == 1 {
                                format!("Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines.")
                            } else {
                                format!("Could not find edits[{index}] in {path}. The oldText must match exactly including all whitespace and newlines.")
                            });
                        }
                        if positions.len() > 1 {
                            return Err(if edits_val.len() == 1 {
                                format!("Found {} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique.", positions.len())
                            } else {
                                format!("Found {} occurrences of edits[{index}] in {path}. Each oldText must be unique. Please provide more context to make it unique.", positions.len())
                            });
                        }
                        let replacement = edit.get("newText").and_then(Value::as_str).ok_or_else(|| format!("Invalid edits[{index}].newText: expected a string"))?.replace("\r\n", "\n");
                        matches.push((positions[0].0, old_normalized.len(), replacement, index));
                    }

                    matches.sort_by_key(|entry| entry.0);
                    for pair in matches.windows(2) {
                        if pair[0].0 + pair[0].1 > pair[1].0 {
                            return Err(format!("edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.", pair[0].3, pair[1].3));
                        }
                    }

                    let mut changed = normalized.clone();
                    for (position, length, replacement, _) in matches.iter().rev() {
                        changed.replace_range(*position..position + length, replacement);
                    }

                    if changed == normalized {
                        return Err(if edits_val.len() == 1 {
                            format!("No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.")
                        } else {
                            format!("No changes made to {path}. The replacements produced identical content.")
                        });
                    }

                    let changed_with_endings = if original.contains("\r\n") {
                        changed.replace('\n', "\r\n")
                    } else {
                        changed.clone()
                    };

                    fs::write(&target, format!("{bom}{changed_with_endings}")).map_err(|error| error.to_string())?;

                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }

                    let diff_res = generate_diff_string(&normalized, &changed, 4);
                    let patch_str = generate_unified_patch(path, &normalized, &changed, 4);

                    Ok(AgentToolResult {
                        content: vec![Content::Text(TextContent {
                            text: format!("Successfully replaced {} block(s) in {}.", edits_val.len(), path).into(),
                            text_signature: None,
                        })],
                        details: json!({
                            "diff": diff_res.diff,
                            "patch": patch_str,
                            "firstChangedLine": diff_res.first_changed_line,
                        }),
                        added_tool_names: None,
                        terminate: None,
                    })
                })
                .await
            })
        }),
    }
}

/// JSON schema copied from `core/tools/bash.ts`.
pub fn bash_schema() -> Value {
    json!({"type":"object","properties":{"command":{"type":"string","description":"Bash command to execute"},"timeout":{"type":"number","description":"Timeout in seconds (optional, no default timeout)"}},"required":["command"]})
}

fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        use rustix::process::{kill_process_group, Pid, Signal};
        if let Some(pgid) = Pid::from_raw(pid as i32) {
            let _ = kill_process_group(pgid, Signal::KILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill")
            .args(&["/F", "/T", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

enum StreamData {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    StdoutEnd,
    StderrEnd,
}

pub fn create_bash_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "bash".into(),
        label: "bash".into(),
        description: "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.".into(),
        parameters: bash_schema(),
        execution_mode: None,
        prepare_arguments: None,
        renderer: None,
        execute: Arc::new(move |_, args, cancellation_token, on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let command = string_arg(&args, "command")?;
                let timeout = args.get("timeout").and_then(Value::as_f64);
                if timeout.is_some_and(|seconds| !seconds.is_finite() || seconds <= 0.0) {
                    return Err("Invalid timeout: must be a finite number of seconds".into());
                }
                if !cwd.is_dir() {
                    return Err(format!(
                        "Working directory does not exist: {}\nCannot execute bash commands.",
                        cwd.display()
                    ));
                }

                let mut cmd = tokio::process::Command::new("bash");
                cmd.arg("-c")
                    .arg(command)
                    .current_dir(&cwd)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());

                #[cfg(unix)]
                {
                    cmd.process_group(0);
                }

                let mut child = cmd.spawn().map_err(|error| error.to_string())?;
                let child_pid = child.id().unwrap_or(0);

                let stdout = child.stdout.take().unwrap();
                let stderr = child.stderr.take().unwrap();

                let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamData>(100);

                let tx_out = tx.clone();
                let mut stdout_stream = stdout;
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stdout_stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        if tx_out.send(StreamData::Stdout(buf[..n].to_vec())).await.is_err() {
                            return;
                        }
                    }
                    let _ = tx_out.send(StreamData::StdoutEnd).await;
                });

                let tx_err = tx.clone();
                let mut stderr_stream = stderr;
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stderr_stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        if tx_err.send(StreamData::Stderr(buf[..n].to_vec())).await.is_err() {
                            return;
                        }
                    }
                    let _ = tx_err.send(StreamData::StderrEnd).await;
                });

                drop(tx);

                if let Some(update_cb) = &on_update {
                    update_cb(AgentToolResult {
                        content: vec![],
                        details: Value::Null,
                        added_tool_names: None,
                        terminate: None,
                    });
                }

                let mut output = OutputAccumulator::new(Some(MAX_LINES), Some(MAX_BYTES), Some("pi-bash"));
                let mut decoder = StreamingUtf8Decoder::new();
                let mut killed = false;
                let mut timed_out = false;
                let started_at = Instant::now();
                let timeout_duration = timeout.map(std::time::Duration::from_secs_f64);

                let mut last_update_at = Instant::now();
                let mut update_dirty = false;

                let mut exited = false;
                let mut exit_code = None;
                let mut stdout_ended = false;
                let mut stderr_ended = false;
                let mut grace_deadline: Option<Instant> = None;

                loop {
                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) && !killed {
                        kill_process_tree(child_pid);
                        killed = true;
                    }

                    if timeout_duration.is_some_and(|dur| started_at.elapsed() >= dur) && !killed {
                        kill_process_tree(child_pid);
                        killed = true;
                        timed_out = true;
                    }

                    if !exited {
                        let status_opt = child.try_wait().ok().flatten();
                        if let Some(status) = status_opt {
                            exited = true;
                            exit_code = status.code();
                            grace_deadline = Some(Instant::now() + std::time::Duration::from_millis(100));
                        }
                    }

                    if exited {
                        if stdout_ended && stderr_ended {
                            break;
                        }
                        if grace_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            break;
                        }
                    }

                    let sleep_dur = std::time::Duration::from_millis(10);
                    let sleep_future = tokio::time::sleep(sleep_dur);

                    tokio::select! {
                        stream_msg = rx.recv() => {
                            match stream_msg {
                                Some(StreamData::Stdout(bytes)) => {
                                    let _ = output.append(&bytes, &mut decoder);
                                    update_dirty = true;
                                    if exited {
                                        grace_deadline = Some(Instant::now() + std::time::Duration::from_millis(100));
                                    }
                                }
                                Some(StreamData::Stderr(bytes)) => {
                                    let _ = output.append(&bytes, &mut decoder);
                                    update_dirty = true;
                                    if exited {
                                        grace_deadline = Some(Instant::now() + std::time::Duration::from_millis(100));
                                    }
                                }
                                Some(StreamData::StdoutEnd) => {
                                    stdout_ended = true;
                                }
                                Some(StreamData::StderrEnd) => {
                                    stderr_ended = true;
                                }
                                None => {
                                    stdout_ended = true;
                                    stderr_ended = true;
                                }
                            }
                        }
                        _ = sleep_future => {
                            // Ticks
                        }
                    }

                    if update_dirty && last_update_at.elapsed() >= std::time::Duration::from_millis(100) {
                        if let Some(update_cb) = &on_update {
                            let snapshot = output.snapshot(true);
                            let mut details = json!({});
                            if snapshot.truncation.truncated {
                                details["truncation"] = serde_json::to_value(&snapshot.truncation).unwrap_or(Value::Null);
                            }
                            if let Some(path) = &snapshot.full_output_path {
                                details["fullOutputPath"] = Value::String(path.to_string_lossy().to_string());
                            }
                            update_cb(AgentToolResult {
                                content: vec![Content::Text(TextContent {
                                    text: snapshot.content.clone().into(),
                                    text_signature: None,
                                })],
                                details,
                                added_tool_names: None,
                                terminate: None,
                            });
                        }
                        update_dirty = false;
                        last_update_at = Instant::now();
                    }
                }

                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        StreamData::Stdout(bytes) | StreamData::Stderr(bytes) => {
                            let _ = output.append(&bytes, &mut decoder);
                        }
                        _ => {}
                    }
                }

                let _ = output.finish(&mut decoder);
                output.close_temp_file();
                let snapshot = output.snapshot(true);

                let mut status_msg = String::new();
                let is_error = if killed {
                    if timed_out {
                        status_msg = format!("Command timed out after {} seconds", timeout.unwrap_or(0.0));
                    } else {
                        status_msg = "Command aborted".to_string();
                    }
                    true
                } else {
                    match exit_code {
                        Some(code) if code != 0 => {
                            status_msg = format!("Command exited with code {}", code);
                            true
                        }
                        _ => false,
                    }
                };

                let empty_text = if is_error { "" } else { "(no output)" };
                let mut output_text = if snapshot.content.is_empty() {
                    empty_text.to_string()
                } else {
                    snapshot.content.clone()
                };

                let mut details = json!({});
                if snapshot.truncation.truncated {
                    details["truncation"] = serde_json::to_value(&snapshot.truncation).unwrap_or(Value::Null);
                    if let Some(path) = &snapshot.full_output_path {
                        let path_str = path.to_string_lossy().to_string();
                        details["fullOutputPath"] = Value::String(path_str.clone());

                        let start_line = snapshot.truncation.total_lines - snapshot.truncation.output_lines + 1;
                        let end_line = snapshot.truncation.total_lines;
                        if snapshot.truncation.last_line_partial {
                            let last_line_size = format_size(output.get_last_line_bytes());
                            output_text.push_str(&format!(
                                "\n\n[Showing last {} of line {} (line is {}). Full output: {}]",
                                format_size(snapshot.truncation.output_bytes),
                                end_line,
                                last_line_size,
                                path_str
                            ));
                        } else if snapshot.truncation.truncated_by == Some(TruncatedBy::Lines) {
                            output_text.push_str(&format!(
                                "\n\n[Showing lines {}-{} of {}. Full output: {}]",
                                start_line, end_line, snapshot.truncation.total_lines, path_str
                            ));
                        } else {
                            output_text.push_str(&format!(
                                "\n\n[Showing lines {}-{} of {} ({} limit). Full output: {}]",
                                start_line, end_line, snapshot.truncation.total_lines, format_size(50 * 1024), path_str
                            ));
                        }
                    }
                }

                if let Some(update_cb) = &on_update {
                    on_update_flush(update_cb, &output_text, &details);
                }

                if is_error {
                    let combined = if output_text.is_empty() {
                        status_msg
                    } else {
                        format!("{}\n\n{}", output_text, status_msg)
                    };
                    return Err(combined);
                }

                Ok(AgentToolResult {
                    content: vec![Content::Text(TextContent {
                        text: output_text.into(),
                        text_signature: None,
                    })],
                    details,
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
    }
}

fn on_update_flush(
    update_cb: &Arc<dyn Fn(AgentToolResult) + Send + Sync>,
    output_text: &str,
    details: &Value,
) {
    update_cb(AgentToolResult {
        content: vec![Content::Text(TextContent {
            text: output_text.to_string().into(),
            text_signature: None,
        })],
        details: details.clone(),
        added_tool_names: None,
        terminate: None,
    });
}

/// JSON schema copied from `core/tools/grep.ts`.
pub fn grep_schema() -> Value {
    json!({"type":"object","properties":{"pattern":{"type":"string","description":"Search pattern (regex or literal string)"},"path":{"type":"string","description":"Directory or file to search (default: current directory)"},"glob":{"type":"string","description":"Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"},"ignoreCase":{"type":"boolean","description":"Case-insensitive search (default: false)"},"literal":{"type":"boolean","description":"Treat pattern as literal string instead of regex (default: false)"},"context":{"type":"number","description":"Number of lines to show before and after each match (default: 0)"},"limit":{"type":"number","description":"Maximum number of matches to return (default: 100)"}},"required":["pattern"]})
}

fn file_set(root: &Path, glob: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let matcher = match glob {
        Some(pattern) => {
            let mut builder = GlobSetBuilder::new();
            builder.add(Glob::new(pattern).map_err(|error| error.to_string())?);
            Some(builder.build().map_err(|error| error.to_string())?)
        }
        None => None,
    };
    let mut files = Vec::new();
    if root.is_file() {
        files.push(root.to_path_buf());
        return Ok(files);
    }
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .build()
    {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && matcher
                .as_ref()
                .is_none_or(|set| set.is_match(entry.path()))
        {
            files.push(entry.into_path());
        }
    }
    Ok(files)
}

pub fn create_grep_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "grep".into(),
        label: "grep".into(),
        description: "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars.".into(),
        parameters: grep_schema(),
        execution_mode: None,
        prepare_arguments: None,
        renderer: None,
        execute: Arc::new(move |_, args, cancellation_token, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let pattern = string_arg(&args, "pattern")?;
                let root = resolve(&cwd, args.get("path").and_then(Value::as_str).unwrap_or("."));
                if !root.exists() {
                    return Err(format!("Path not found: {}", root.display()));
                }
                let limit = limit_arg(&args, "limit").unwrap_or(100.0).max(1.0).ceil() as usize;
                let literal = args.get("literal").and_then(Value::as_bool).unwrap_or(false);
                let pattern = if literal { regex::escape(pattern) } else { pattern.to_owned() };
                let context_value = limit_arg(&args, "context").unwrap_or(0.0) as usize;

                let regex = RegexBuilder::new(&pattern)
                    .case_insensitive(args.get("ignoreCase").and_then(Value::as_bool).unwrap_or(false))
                    .build()
                    .map_err(|error| error.to_string())?;

                let mut match_count = 0;
                let mut match_limit_reached = false;
                let mut lines_truncated = false;
                let mut output_lines = Vec::new();

                for file in file_set(&root, args.get("glob").and_then(Value::as_str))? {
                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }

                    let Ok(contents) = fs::read_to_string(&file) else { continue };
                    let lines: Vec<&str> = contents.split('\n').collect();

                    for (line_idx, line) in lines.iter().enumerate() {
                        let line_number = line_idx + 1;
                        let sanitized = line.replace('\r', "");
                        if regex.is_match(&sanitized) {
                            match_count += 1;

                            let relative = file
                                .strip_prefix(&cwd)
                                .unwrap_or(&file)
                                .display()
                                .to_string()
                                .replace('\\', "/");
                            if context_value == 0 {
                                let (truncated_text, was_truncated) = truncate_line(&sanitized, GREP_MAX_LINE_LENGTH);
                                if was_truncated {
                                    lines_truncated = true;
                                }
                                output_lines.push(format!("{relative}:{line_number}: {truncated_text}"));
                            } else {
                                let start = if line_number > context_value { line_number - context_value } else { 1 };
                                let end = std::cmp::min(lines.len(), line_number + context_value);
                                for current in start..=end {
                                    let is_match_line = current == line_number;
                                    let current_line = lines[current - 1].replace('\r', "");
                                    let (truncated_text, was_truncated) = truncate_line(&current_line, GREP_MAX_LINE_LENGTH);
                                    if was_truncated {
                                        lines_truncated = true;
                                    }
                                    if is_match_line {
                                        output_lines.push(format!("{relative}:{current}: {truncated_text}"));
                                    } else {
                                        output_lines.push(format!("{relative}-{current}- {truncated_text}"));
                                    }
                                }
                            }

                            if match_count >= limit {
                                match_limit_reached = true;
                                break;
                            }
                        }
                    }
                    if match_limit_reached {
                        break;
                    }
                }

                if output_lines.is_empty() {
                    return Ok(AgentToolResult {
                        content: vec![Content::Text(TextContent {
                            text: "No matches found".to_string().into(),
                            text_signature: None,
                        })],
                        details: Value::Object(Default::default()),
                        added_tool_names: None,
                        terminate: None,
                    });
                }

                let raw_output = output_lines.join("\n");
                let truncation = truncate_head(&raw_output, usize::MAX, 50 * 1024);
                let mut output = truncation.content.clone();

                let mut notices = Vec::new();
                let mut details = json!({});

                if match_limit_reached {
                    notices.push(format!(
                        "{limit} matches limit reached. Use limit={} for more, or refine pattern",
                        limit * 2
                    ));
                    details["matchLimitReached"] = Value::Number(serde_json::Number::from(limit));
                }
                if truncation.truncated {
                    notices.push(format!("{} limit reached", format_size(50 * 1024)));
                    details["truncation"] = serde_json::to_value(&truncation).unwrap_or(Value::Null);
                }
                if lines_truncated {
                    notices.push(format!("Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"));
                    details["linesTruncated"] = Value::Bool(true);
                }

                if !notices.is_empty() {
                    output.push_str(&format!("\n\n[{}]", notices.join(". ")));
                }

                Ok(AgentToolResult {
                    content: vec![Content::Text(TextContent {
                        text: output.into(),
                        text_signature: None,
                    })],
                    details: if details.as_object().unwrap().is_empty() {
                        Value::Object(Default::default())
                    } else {
                        details
                    },
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
    }
}

/// JSON schema copied from `core/tools/find.ts`.
pub fn find_schema() -> Value {
    json!({"type":"object","properties":{"pattern":{"type":"string","description":"Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"},"path":{"type":"string","description":"Directory to search in (default: current directory)"},"limit":{"type":"number","description":"Maximum number of results (default: 1000)"}},"required":["pattern"]})
}

pub fn create_find_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "find".into(),
        label: "find".into(),
        description: "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first).".into(),
        parameters: find_schema(),
        execution_mode: None,
        prepare_arguments: None,
        renderer: None,
        execute: Arc::new(move |_, args, cancellation_token, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let pattern = string_arg(&args, "pattern")?;
                let root = resolve(&cwd, args.get("path").and_then(Value::as_str).unwrap_or("."));
                if !root.exists() {
                    return Err(format!("Path not found: {}", root.display()));
                }
                let limit = limit_arg(&args, "limit").unwrap_or(1000.0).max(1.0).ceil() as usize;

                let mut builder = GlobSetBuilder::new();
                builder.add(Glob::new(pattern).map_err(|error| error.to_string())?);
                let matcher = builder.build().map_err(|error| error.to_string())?;

                let mut results = Vec::new();
                let mut results_limit_reached = false;

                for entry in WalkBuilder::new(&root)
                    .hidden(false)
                    .git_ignore(true)
                    .git_global(true)
                    .build()
                {
                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }

                    let entry = entry.map_err(|error| error.to_string())?;
                    let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                    if !relative.as_os_str().is_empty() && matcher.is_match(relative) {
                        results.push(relative.display().to_string().replace('\\', "/"));
                        if results.len() >= limit {
                            results_limit_reached = true;
                            break;
                        }
                    }
                }

                results.sort();

                if results.is_empty() {
                    return Ok(AgentToolResult {
                        content: vec![Content::Text(TextContent {
                            text: "No files found matching pattern".to_string().into(),
                            text_signature: None,
                        })],
                        details: Value::Object(Default::default()),
                        added_tool_names: None,
                        terminate: None,
                    });
                }

                let raw_output = results.join("\n");
                let truncation = truncate_head(&raw_output, usize::MAX, 50 * 1024);
                let mut output = truncation.content.clone();

                let mut notices = Vec::new();
                let mut details = json!({});

                if results_limit_reached {
                    notices.push(format!(
                        "{limit} results limit reached. Use limit={} for more, or refine pattern",
                        limit * 2
                    ));
                    details["resultLimitReached"] = Value::Number(serde_json::Number::from(limit));
                }
                if truncation.truncated {
                    notices.push(format!("{} limit reached", format_size(50 * 1024)));
                    details["truncation"] = serde_json::to_value(&truncation).unwrap_or(Value::Null);
                }

                if !notices.is_empty() {
                    output.push_str(&format!("\n\n[{}]", notices.join(". ")));
                }

                Ok(AgentToolResult {
                    content: vec![Content::Text(TextContent {
                        text: output.into(),
                        text_signature: None,
                    })],
                    details: if details.as_object().unwrap().is_empty() {
                        Value::Object(Default::default())
                    } else {
                        details
                    },
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
    }
}

/// JSON schema copied from `core/tools/ls.ts`.
pub fn ls_schema() -> Value {
    json!({"type":"object","properties":{"path":{"type":"string","description":"Directory to list (default: current directory)"},"limit":{"type":"number","description":"Maximum number of entries to return (default: 500)"}}})
}

pub fn create_ls_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "ls".into(),
        label: "ls".into(),
        description: "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first).".into(),
        parameters: ls_schema(),
        execution_mode: None,
        prepare_arguments: None,
        renderer: None,
        execute: Arc::new(move |_, args, cancellation_token, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = resolve(&cwd, args.get("path").and_then(Value::as_str).unwrap_or("."));
                if !path.exists() {
                    return Err(format!("Path not found: {}", path.display()));
                }
                let metadata = fs::metadata(&path).map_err(|e| format!("Cannot read directory: {}", e))?;
                if !metadata.is_dir() {
                    return Err(format!("Not a directory: {}", path.display()));
                }
                let limit = limit_arg(&args, "limit").unwrap_or(500.0).max(1.0).ceil() as usize;

                let read_dir = fs::read_dir(&path).map_err(|e| format!("Cannot read directory: {}", e))?;
                let mut entries = Vec::new();
                for entry in read_dir {
                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }
                    let entry = entry.map_err(|e| format!("Cannot read directory: {}", e))?;
                    entries.push(entry.file_name().to_string_lossy().into_owned());
                }

                entries.sort_by_key(|a| a.to_lowercase());

                let mut results = Vec::new();
                let mut entry_limit_reached = false;
                for entry in entries {
                    if cancellation_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
                        return Err("Operation aborted".to_string());
                    }
                    if results.len() >= limit {
                        entry_limit_reached = true;
                        break;
                    }
                    let full_path = path.join(&entry);
                    let mut suffix = "";
                    if let Ok(meta) = fs::metadata(&full_path) {
                        if meta.is_dir() {
                            suffix = "/";
                        }
                    } else {
                        continue;
                    }
                    results.push(format!("{}{}", entry, suffix));
                }

                if results.is_empty() {
                    return Ok(AgentToolResult {
                        content: vec![Content::Text(TextContent {
                            text: "(empty directory)".to_string().into(),
                            text_signature: None,
                        })],
                        details: Value::Object(Default::default()),
                        added_tool_names: None,
                        terminate: None,
                    });
                }

                let raw_output = results.join("\n");
                let truncation = truncate_head(&raw_output, usize::MAX, 50 * 1024);
                let mut output = truncation.content.clone();
                let mut notices = Vec::new();
                let mut details = json!({});

                if entry_limit_reached {
                    notices.push(format!(
                        "{limit} entries limit reached. Use limit={} for more",
                        limit * 2
                    ));
                    details["entryLimitReached"] = Value::Number(serde_json::Number::from(limit));
                }
                if truncation.truncated {
                    notices.push(format!("{} limit reached", format_size(50 * 1024)));
                    details["truncation"] = serde_json::to_value(&truncation).unwrap_or(Value::Null);
                }

                if !notices.is_empty() {
                    output.push_str(&format!("\n\n[{}]", notices.join(". ")));
                }

                Ok(AgentToolResult {
                    content: vec![Content::Text(TextContent {
                        text: output.into(),
                        text_signature: None,
                    })],
                    details: if details.as_object().unwrap().is_empty() {
                        Value::Object(Default::default())
                    } else {
                        details
                    },
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
    }
}

/// Creates the seven first-party tool definitions.
pub fn builtin_tools(cwd: impl Into<PathBuf>) -> Vec<ToolDefinition> {
    let cwd = cwd.into();
    vec![
        create_read_tool(cwd.clone()),
        create_bash_tool(cwd.clone()),
        create_edit_tool(cwd.clone()),
        create_write_tool(cwd.clone()),
        create_grep_tool(cwd.clone()),
        create_find_tool(cwd.clone()),
        create_ls_tool(cwd),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::CancellationToken;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pi-tools-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("tempdir");
        dir
    }

    async fn run(tool: &ToolDefinition, args: Value) -> Result<String, String> {
        let result = (tool.execute)("test".into(), args, None, None).await?;
        let Content::Text(content) = &result.content[0] else {
            panic!("expected text")
        };
        Ok(content.text.as_string())
    }

    #[test]
    fn test_path_normalization() {
        let cwd = Path::new("/home/alpha/exp/pi-rust");

        // Root /.. test
        assert_eq!(resolve(cwd, "/../tmp"), PathBuf::from("/tmp"));

        // Relative unresolved parent tests
        assert_eq!(resolve(cwd, "../tmp"), PathBuf::from("/home/alpha/exp/tmp"));
        assert_eq!(resolve(cwd, "../../tmp"), PathBuf::from("/home/alpha/tmp"));
        assert_eq!(resolve(cwd, "a/../x"), PathBuf::from("/home/alpha/exp/pi-rust/x"));

        // Literal /.. test
        assert_eq!(normalize_path(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(normalize_path(Path::new("/../../..")), PathBuf::from("/"));
    }

    #[test]
    fn schemas_and_descriptions_equal_pi_fixtures() {
        let read_t = create_read_tool(Path::new("."));
        let write_t = create_write_tool(Path::new("."));
        let edit_t = create_edit_tool(Path::new("."));
        let bash_t = create_bash_tool(Path::new("."));
        let grep_t = create_grep_tool(Path::new("."));
        let find_t = create_find_tool(Path::new("."));
        let ls_t = create_ls_tool(Path::new("."));

        // Load fixtures
        let read_fixture: Value = serde_json::from_str(include_str!("fixtures/read.json")).unwrap();
        let write_fixture: Value = serde_json::from_str(include_str!("fixtures/write.json")).unwrap();
        let edit_fixture: Value = serde_json::from_str(include_str!("fixtures/edit.json")).unwrap();
        let bash_fixture: Value = serde_json::from_str(include_str!("fixtures/bash.json")).unwrap();
        let grep_fixture: Value = serde_json::from_str(include_str!("fixtures/grep.json")).unwrap();
        let find_fixture: Value = serde_json::from_str(include_str!("fixtures/find.json")).unwrap();
        let ls_fixture: Value = serde_json::from_str(include_str!("fixtures/ls.json")).unwrap();

        // Schema Assertions
        assert_eq!(read_t.parameters, read_fixture);
        assert_eq!(write_t.parameters, write_fixture);
        assert_eq!(edit_t.parameters, edit_fixture);
        assert_eq!(bash_t.parameters, bash_fixture);
        assert_eq!(grep_t.parameters, grep_fixture);
        assert_eq!(find_t.parameters, find_fixture);
        assert_eq!(ls_t.parameters, ls_fixture);

        // Description Assertions
        assert_eq!(read_t.description, "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.");
        assert_eq!(write_t.description, "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.");
        assert_eq!(edit_t.description, "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.");
        assert_eq!(bash_t.description, "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.");
        assert_eq!(grep_t.description, "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars.");
        assert_eq!(find_t.description, "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first).");
        assert_eq!(ls_t.description, "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first).");
    }

    #[tokio::test]
    async fn tools_execute_on_tempdir() {
        let dir = tempdir();
        fs::write(dir.join("input.txt"), "alpha\nbeta\ngamma\n").expect("fixture");
        fs::create_dir(dir.join("nested")).expect("nested");
        fs::write(dir.join("nested/item.rs"), "fn main() {}\n").expect("fixture");

        assert_eq!(
            run(
                &create_read_tool(&dir),
                json!({"path":"input.txt","offset":2,"limit":1})
            )
            .await
            .expect("read"),
            "beta\n\n[2 more lines in file. Use offset=3 to continue.]"
        );

        fs::write(dir.join("picture.png"), [137, 80, 78, 71]).expect("image fixture");
        let image = (create_read_tool(&dir).execute)(
            "test".into(),
            json!({"path":"picture.png"}),
            None,
            None,
        )
        .await
        .expect("image read");
        assert!(matches!(
            image.content.get(1),
            Some(Content::Image(_))
        ));

        assert!(
            run(
                &create_write_tool(&dir),
                json!({"path":"made/out.txt","content":"created"})
            )
            .await
            .expect("write")
            .contains("Successfully wrote 7 bytes")
        );

        assert!(
            run(
                &create_write_tool(&dir),
                json!({"path":"made/emoji.txt","content":"🚀"})
            )
            .await
            .expect("write")
            .contains("Successfully wrote 2 bytes")
        );

        let edit_res = (create_edit_tool(&dir).execute)(
            "test".into(),
            json!({"path":"input.txt","edits":[{"oldText":"beta","newText":"delta"}]}),
            None,
            None,
        )
        .await
        .expect("edit");

        let Content::Text(txt) = &edit_res.content[0] else {
            panic!("expected text");
        };
        assert!(txt.text.as_string().contains("Successfully replaced 1"));
        assert!(edit_res.details.get("diff").is_some());
        assert!(edit_res.details.get("patch").is_some());

        // Test read limit=1.5 returns 1 line (truncation/floor)
        assert_eq!(
            run(
                &create_read_tool(&dir),
                json!({"path":"input.txt","offset":2,"limit":1.5})
            )
            .await
            .expect("read fractional"),
            "delta\n\n[2 more lines in file. Use offset=3 to continue.]"
        );

        // Test read limit=0 returns 0 lines with correct footer
        assert_eq!(
            run(
                &create_read_tool(&dir),
                json!({"path":"input.txt","offset":2,"limit":0})
            )
            .await
            .expect("read limit 0"),
            "\n\n[3 more lines in file. Use offset=2 to continue.]"
        );

        assert!(
            run(&create_bash_tool(&dir), json!({"command":"printf shell"}))
                .await
                .expect("bash")
                .contains("shell")
        );

        assert!(
            run(&create_grep_tool(&dir), json!({"pattern":"delta"}))
                .await
                .expect("grep")
                .contains("input.txt:2: delta")
        );

        // Test limit=0 defaults/clamping to 1
        let grep_zero = run(&create_grep_tool(&dir), json!({"pattern":"a","path":"input.txt","limit":0})).await.expect("grep limit 0");
        assert!(grep_zero.contains("input.txt:1: alpha"));
        assert!(!grep_zero.contains("input.txt:2: delta"));

        // Test grep limit=1.5 returns 2 matches (ceil) on input.txt
        let grep_frac = run(&create_grep_tool(&dir), json!({"pattern":"a","path":"input.txt","limit":1.5})).await.expect("grep fractional");
        assert!(grep_frac.contains("input.txt:1: alpha"));
        assert!(grep_frac.contains("input.txt:2: delta"));
        assert!(!grep_frac.contains("input.txt:3: gamma"));

        assert!(
            run(&create_find_tool(&dir), json!({"pattern":"**/*.rs"}))
                .await
                .expect("find")
                .contains("nested/item.rs")
        );

        assert!(
            run(&create_ls_tool(&dir), json!({}))
                .await
                .expect("ls")
                .contains("nested/")
        );

        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn test_cancellation_and_descendants() {
        let dir = tempdir();
        let tool = create_bash_tool(&dir);
        let ct = CancellationToken::new();
        let marker = dir.join("marker.txt");
        let marker_str = marker.to_string_lossy().to_string();

        let ct_clone = ct.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ct_clone.cancel();
        });

        let start = Instant::now();
        let res = (tool.execute)(
            "test".into(),
            json!({"command": format!("sh -c 'sleep 1 && echo hello > {}'", marker_str)}),
            Some(ct),
            None,
        )
        .await;

        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        assert!(res.is_err());
        let err_msg = res.err().unwrap();
        assert!(err_msg.contains("Command aborted"));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(!marker.exists(), "Descendant process survived and wrote marker!");

        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn test_bash_timeout() {
        let dir = tempdir();
        let tool = create_bash_tool(&dir);
        let res = (tool.execute)(
            "test".into(),
            json!({"command": "sleep 10", "timeout": 0.1}),
            None,
            None,
        )
        .await;
        assert!(res.is_err());
        assert!(res.err().unwrap().contains("Command timed out after 0.1 seconds"));
        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn test_bash_truncation_tempfile() {
        let dir = tempdir();
        let tool = create_bash_tool(&dir);
        let command = "for i in {1..2500}; do echo \"line $i\"; done";
        let res = (tool.execute)(
            "test".into(),
            json!({"command": command}),
            None,
            None,
        )
        .await
        .expect("bash execution");

        assert!(res.details.get("truncation").is_some());
        let full_output_path_val = res.details.get("fullOutputPath").expect("must have fullOutputPath");
        let path_str = full_output_path_val.as_str().expect("must be string");
        let temp_path = PathBuf::from(path_str);
        assert!(temp_path.exists());

        let full_content = fs::read_to_string(&temp_path).expect("read temp file");
        assert!(full_content.contains("line 1\n"));
        assert!(full_content.contains("line 2500\n"));

        let _ = fs::remove_file(temp_path);
        fs::remove_dir_all(dir).expect("cleanup");
    }
}
