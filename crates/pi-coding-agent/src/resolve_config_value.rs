use std::collections::HashMap;
use std::process::Command;
use std::sync::LazyLock;
use parking_lot::Mutex;

static COMMAND_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Clear the command execution cache.
pub fn clear_config_value_cache() {
    COMMAND_CACHE.lock().clear();
}

/// Execute command with caching.
fn execute_command(command: &str) -> Option<String> {
    let command = command.trim();
    if let Some(cached) = COMMAND_CACHE.lock().get(command) {
        return cached.clone();
    }

    let result = execute_command_uncached(command);

    COMMAND_CACHE.lock().insert(command.to_string(), result.clone());
    result
}

/// Execute command without caching.
fn execute_command_uncached(command: &str) -> Option<String> {
    let command = command.trim();
    let cmd = if let Some(stripped) = command.strip_prefix('!') {
        stripped
    } else {
        command
    };

    let output = if cfg!(target_os = "windows") {
        Command::new("cmd")
            .args(["/C", cmd])
            .output()
    } else {
        Command::new("/bin/sh")
            .args(["-c", cmd])
            .output()
    };

    match output {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout);
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        _ => None,
    }
}

fn is_valid_env_var_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_env_var_char(c: char, is_first: bool) -> bool {
    if is_first {
        c.is_ascii_alphabetic() || c == '_'
    } else {
        c.is_ascii_alphanumeric() || c == '_'
    }
}

fn get_env_var(name: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    if let Some(val) = env.and_then(|env_map| env_map.get(name)) {
        return Some(val.clone());
    }
    std::env::var(name).ok()
}

/// Resolves a single config value template.
/// Returns None if any referenced env variable is missing/undefined.
pub fn resolve_template(config: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    let mut resolved = String::new();
    let chars: Vec<char> = config.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' {
            if i + 1 < chars.len() {
                let next_char = chars[i + 1];
                if next_char == '$' || next_char == '!' {
                    resolved.push(next_char);
                    i += 2;
                    continue;
                }
                if next_char == '{' {
                    // Find matching '}'
                    let mut end = i + 2;
                    while end < chars.len() && chars[end] != '}' {
                        end += 1;
                    }
                    if end >= chars.len() {
                        resolved.push('$');
                        i += 1;
                        continue;
                    }
                    let name: String = chars[i + 2..end].iter().collect();
                    if is_valid_env_var_name(&name) {
                        let val = get_env_var(&name, env)?;
                        resolved.push_str(&val);
                    } else {
                        // Keep literal
                        let literal: String = chars[i..=end].iter().collect();
                        resolved.push_str(&literal);
                    }
                    i = end + 1;
                    continue;
                }
                // Try matching plain variable name prefix
                let mut end = i + 1;
                while end < chars.len() && is_env_var_char(chars[end], end == i + 1) {
                    end += 1;
                }
                if end > i + 1 {
                    let name: String = chars[i + 1..end].iter().collect();
                    let val = get_env_var(&name, env)?;
                    resolved.push_str(&val);
                    i = end;
                    continue;
                }
            }
            resolved.push('$');
            i += 1;
        } else {
            resolved.push(chars[i]);
            i += 1;
        }
    }

    Some(resolved)
}

/// Resolve a configuration value (API key, header, etc.).
pub fn resolve_config_value(config: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    if config.starts_with('!') {
        return execute_command(config);
    }
    resolve_template(config, env)
}

/// Resolve a configuration value without caching.
pub fn resolve_config_value_uncached(config: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    if config.starts_with('!') {
        return execute_command_uncached(config);
    }
    resolve_template(config, env)
}

/// Resolve config value or return an error.
pub fn resolve_config_value_or_throw(
    config: &str,
    description: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<String, String> {
    if let Some(resolved_value) = resolve_config_value_uncached(config, env) {
        return Ok(resolved_value);
    }

    if let Some(stripped) = config.strip_prefix('!') {
        return Err(format!("Failed to resolve {} from shell command: {}", description, stripped));
    }

    let missing = get_missing_config_value_env_var_names(config, env);
    if missing.len() == 1 {
        return Err(format!("Failed to resolve {} from environment variable: {}", description, missing[0]));
    }
    if missing.len() > 1 {
        return Err(format!("Failed to resolve {} from environment variables: {}", description, missing.join(", ")));
    }

    Err(format!("Failed to resolve {}", description))
}

/// Is the config value a command?
pub fn is_command_config_value(config: &str) -> bool {
    config.starts_with('!')
}

/// Get all environment variable names referenced in the template.
pub fn get_config_value_env_var_names(config: &str) -> Vec<String> {
    if config.starts_with('!') {
        return Vec::new();
    }
    let mut names = Vec::new();
    let chars: Vec<char> = config.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' {
            if i + 1 < chars.len() {
                let next_char = chars[i + 1];
                if next_char == '$' || next_char == '!' {
                    i += 2;
                    continue;
                }
                if next_char == '{' {
                    let mut end = i + 2;
                    while end < chars.len() && chars[end] != '}' {
                        end += 1;
                    }
                    if end >= chars.len() {
                        i += 1;
                        continue;
                    }
                    let name: String = chars[i + 2..end].iter().collect();
                    if is_valid_env_var_name(&name) && !names.contains(&name) {
                        names.push(name);
                    }
                    i = end + 1;
                    continue;
                }
                let mut end = i + 1;
                while end < chars.len() && is_env_var_char(chars[end], end == i + 1) {
                    end += 1;
                }
                if end > i + 1 {
                    let name: String = chars[i + 1..end].iter().collect();
                    if !names.contains(&name) {
                        names.push(name);
                    }
                    i = end;
                    continue;
                }
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    names
}

/// Get missing referenced environment variable names.
pub fn get_missing_config_value_env_var_names(config: &str, env: Option<&HashMap<String, String>>) -> Vec<String> {
    get_config_value_env_var_names(config)
        .into_iter()
        .filter(|name| get_env_var(name, env).is_none())
        .collect()
}

/// Check if the configuration value is ready (all env variables exist).
pub fn is_config_value_configured(config: &str, env: Option<&HashMap<String, String>>) -> bool {
    get_missing_config_value_env_var_names(config, env).is_empty()
}

/// Resolve all header values in a map.
pub fn resolve_headers(
    headers: Option<&HashMap<String, String>>,
    env: Option<&HashMap<String, String>>,
) -> Option<HashMap<String, String>> {
    let headers = headers?;
    let mut resolved = HashMap::new();
    for (k, v) in headers {
        if let Some(resolved_val) = resolve_config_value(v, env).filter(|val| !val.is_empty()) {
            resolved.insert(k.clone(), resolved_val);
        }
    }
    if resolved.is_empty() {
        None
    } else {
        Some(resolved)
    }
}

/// Resolve all header values or return an error description.
pub fn resolve_headers_or_throw(
    headers: Option<&HashMap<String, String>>,
    description: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<Option<HashMap<String, String>>, String> {
    let Some(headers) = headers else {
        return Ok(None);
    };
    let mut resolved = HashMap::new();
    for (k, v) in headers {
        let resolved_val = resolve_config_value_or_throw(v, &format!("{} header \"{}\"", description, k), env)?;
        resolved.insert(k.clone(), resolved_val);
    }
    if resolved.is_empty() {
        Ok(None)
    } else {
        Ok(Some(resolved))
    }
}
