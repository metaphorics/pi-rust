//! Piped stdin + `@file` argument processing — port of `main.ts:58-75`
//! `readPipedStdin`, `cli/file-processor.ts`, and `cli/initial-message.ts`.
//!
//! This is a printer module of the `pi` binary: `eprintln!` is the oracle's
//! `console.error` boot-path contract (these paths run before any wire mode
//! starts).

use std::io::Read;
use std::path::Path;

use pi_ai::ImageContent;

use crate::config::resolve_path;

use super::args::Args;

/// Read all content from piped stdin (oracle `readPipedStdin`).
/// Returns `None` when stdin is a TTY or the trimmed content is empty.
pub fn read_piped_stdin(stdin_is_tty: bool) -> Option<String> {
    if stdin_is_tty {
        return None;
    }
    let mut data = String::new();
    std::io::stdin().read_to_string(&mut data).ok()?;
    let trimmed = data.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Result of [`process_file_arguments`].
#[derive(Debug, Default)]
pub struct ProcessedFiles {
    pub text: String,
    pub images: Vec<ImageContent>,
}

fn exit_error(message: &str) -> ! {
    eprintln!("\x1b[31m{message}\x1b[39m");
    std::process::exit(1);
}

/// Process `@file` arguments into text content and image attachments
/// (oracle cli/file-processor.ts `processFileArguments`). Missing or
/// unreadable files print the oracle error and exit 1; empty files are
/// skipped.
pub fn process_file_arguments(
    file_args: &[String],
    cwd: &Path,
    auto_resize_images: bool,
) -> ProcessedFiles {
    let mut text = String::new();
    let mut images = Vec::new();

    for file_arg in file_args {
        let absolute = resolve_path(file_arg, Some(cwd));
        let Ok(metadata) = std::fs::metadata(&absolute) else {
            exit_error(&format!("Error: File not found: {}", absolute.display()));
        };
        if metadata.len() == 0 {
            continue;
        }

        if let Some(mime_type) = crate::tools::image_mime(&absolute) {
            let data = match std::fs::read(&absolute) {
                Ok(data) => data,
                Err(error) => exit_error(&format!(
                    "Error: Could not read file {}: {error}",
                    absolute.display()
                )),
            };
            match crate::tools::process_image_attachment(&data, mime_type, auto_resize_images) {
                Ok((b64, mime, hints)) => {
                    images.push(ImageContent {
                        data: b64,
                        mime_type: mime,
                    });
                    if hints.is_empty() {
                        text.push_str(&format!("<file name=\"{}\"></file>\n", absolute.display()));
                    } else {
                        text.push_str(&format!(
                            "<file name=\"{}\">{}</file>\n",
                            absolute.display(),
                            hints.join("\n")
                        ));
                    }
                }
                Err(message) => {
                    text.push_str(&format!(
                        "<file name=\"{}\">{}</file>\n",
                        absolute.display(),
                        message
                    ));
                }
            }
        } else {
            match std::fs::read_to_string(&absolute) {
                Ok(content) => text.push_str(&format!(
                    "<file name=\"{}\">\n{}\n</file>\n",
                    absolute.display(),
                    content
                )),
                Err(error) => exit_error(&format!(
                    "Error: Could not read file {}: {error}",
                    absolute.display()
                )),
            }
        }
    }

    ProcessedFiles { text, images }
}

/// Result of [`prepare_initial_message`].
#[derive(Debug, Default)]
pub struct InitialMessage {
    pub initial_message: Option<String>,
    pub initial_images: Vec<ImageContent>,
}

/// Combine stdin content, `@file` text, and the first CLI message into a
/// single initial prompt (oracle `buildInitialMessage` +
/// `prepareInitialMessage`). Consumes `parsed.messages[0]` when present.
pub fn prepare_initial_message(
    parsed: &mut Args,
    cwd: &Path,
    auto_resize_images: bool,
    stdin_content: Option<String>,
) -> InitialMessage {
    let processed = if parsed.file_args.is_empty() {
        ProcessedFiles::default()
    } else {
        process_file_arguments(&parsed.file_args, cwd, auto_resize_images)
    };

    let mut parts: Vec<String> = Vec::new();
    if let Some(stdin_content) = stdin_content {
        parts.push(stdin_content);
    }
    if !processed.text.is_empty() {
        parts.push(processed.text);
    }
    if !parsed.messages.is_empty() {
        parts.push(parsed.messages.remove(0));
    }

    InitialMessage {
        initial_message: if parts.is_empty() {
            None
        } else {
            Some(parts.concat())
        },
        initial_images: processed.images,
    }
}
