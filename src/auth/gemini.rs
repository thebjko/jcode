use anyhow::Result;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeminiCliCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl GeminiCliCommand {
    pub fn display(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// Resolve the Gemini CLI command from the environment or a sensible default.
///
/// Preference order:
/// 1. `JCODE_GEMINI_CLI_PATH` (supports a full command like `npx @google/gemini-cli`)
/// 2. `gemini` on PATH
/// 3. `npx @google/gemini-cli`
pub fn gemini_cli_command() -> GeminiCliCommand {
    resolve_gemini_cli_command_with(
        std::env::var("JCODE_GEMINI_CLI_PATH").ok().as_deref(),
        super::command_exists,
    )
}

/// Resolve just the executable portion for legacy callers.
pub fn gemini_cli_path() -> String {
    gemini_cli_command().program
}

/// Check if a usable Gemini CLI command is available.
pub fn has_gemini_cli() -> bool {
    let resolved = gemini_cli_command();
    super::command_exists(&resolved.program)
}

/// Best-effort probe for cached Gemini CLI auth.
///
/// Gemini CLI supports `/auth` interactively, but its non-interactive auth probe
/// surface is not stable enough to rely on here. For now we treat CLI presence
/// as sufficient to expose the provider and let runtime errors prompt login.
pub fn has_cached_auth() -> bool {
    gemini_state_dir()
        .map(|path| path.exists())
        .unwrap_or(false)
}

fn gemini_state_dir() -> Result<std::path::PathBuf> {
    Ok(crate::storage::user_home_path(".gemini")?)
}

fn resolve_gemini_cli_command_with<F>(env_spec: Option<&str>, command_exists: F) -> GeminiCliCommand
where
    F: Fn(&str) -> bool,
{
    if let Some(spec) = env_spec.and_then(parse_command_spec) {
        return GeminiCliCommand {
            program: spec[0].clone(),
            args: spec[1..].to_vec(),
        };
    }

    if command_exists("gemini") {
        return GeminiCliCommand {
            program: "gemini".to_string(),
            args: Vec::new(),
        };
    }

    if command_exists("npx") {
        return GeminiCliCommand {
            program: "npx".to_string(),
            args: vec!["@google/gemini-cli".to_string()],
        };
    }

    GeminiCliCommand {
        program: "gemini".to_string(),
        args: Vec::new(),
    }
}

fn parse_command_spec(raw: &str) -> Option<Vec<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;

    for ch in raw.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }

        match ch {
            '\\' if !in_single => escape = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if escape {
        current.push('\\');
    }

    if !current.is_empty() {
        parts.push(current);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_command_with_args() {
        let resolved =
            resolve_gemini_cli_command_with(Some("npx @google/gemini-cli --proxy test"), |_| false);
        assert_eq!(
            resolved,
            GeminiCliCommand {
                program: "npx".to_string(),
                args: vec![
                    "@google/gemini-cli".to_string(),
                    "--proxy".to_string(),
                    "test".to_string(),
                ],
            }
        );
    }

    #[test]
    fn falls_back_to_gemini_binary_when_available() {
        let resolved = resolve_gemini_cli_command_with(None, |cmd| cmd == "gemini");
        assert_eq!(resolved.program, "gemini");
        assert!(resolved.args.is_empty());
    }

    #[test]
    fn falls_back_to_npx_when_gemini_binary_missing() {
        let resolved = resolve_gemini_cli_command_with(None, |cmd| cmd == "npx");
        assert_eq!(resolved.program, "npx");
        assert_eq!(resolved.args, vec!["@google/gemini-cli"]);
    }

    #[test]
    fn display_includes_args_when_present() {
        let command = GeminiCliCommand {
            program: "npx".to_string(),
            args: vec!["@google/gemini-cli".to_string()],
        };
        assert_eq!(command.display(), "npx @google/gemini-cli");
    }
}
