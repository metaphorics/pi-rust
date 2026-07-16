//! `pi` binary entry point.
//!
//! Port of packages/coding-agent main CLI routing.

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = pi_coding_agent::cli::parse_args(&args);

    // 1. Check parser-level diagnostics first
    if !parsed.diagnostics.is_empty() {
        let has_errors = parsed.diagnostics.iter().any(|d| d.r#type == pi_coding_agent::cli::DiagnosticType::Error);
        for d in &parsed.diagnostics {
            let prefix = match d.r#type {
                pi_coding_agent::cli::DiagnosticType::Error => "Error",
                pi_coding_agent::cli::DiagnosticType::Warning => "Warning",
            };
            let color_code = match d.r#type {
                pi_coding_agent::cli::DiagnosticType::Error => "\x1b[31m",
                pi_coding_agent::cli::DiagnosticType::Warning => "\x1b[33m",
            };
            eprintln!("{color_code}{}: {}\x1b[39m", prefix, d.message);
        }
        if has_errors {
            std::process::exit(1);
        }
    }

    // 2. Check version
    if parsed.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    // 3. Run combination validation
    let combo_diagnostics = pi_coding_agent::cli::validate_arg_combinations(&parsed);
    if !combo_diagnostics.is_empty() {
        let has_errors = combo_diagnostics.iter().any(|d| d.r#type == pi_coding_agent::cli::DiagnosticType::Error);
        for d in &combo_diagnostics {
            let prefix = match d.r#type {
                pi_coding_agent::cli::DiagnosticType::Error => "Error",
                pi_coding_agent::cli::DiagnosticType::Warning => "Warning",
            };
            let color_code = match d.r#type {
                pi_coding_agent::cli::DiagnosticType::Error => "\x1b[31m",
                pi_coding_agent::cli::DiagnosticType::Warning => "\x1b[33m",
            };
            eprintln!("{color_code}{}: {}\x1b[39m", prefix, d.message);
        }
        if has_errors {
            std::process::exit(1);
        }
    }

    // 4. Check help
    if parsed.help {
        println!("{}", pi_coding_agent::cli::get_help_text(None));
        std::process::exit(0);
    }

    Ok(())
}
