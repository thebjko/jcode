use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{self, IsTerminal, Write};

const GOOGLE_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
const GEMINI_OAUTH_CLIENT_ID: &str =
    "set-gemini-client-id-in-env.apps.googleusercontent.com";
const GEMINI_OAUTH_CLIENT_SECRET: &str = "set-gemini-client-secret-in-env";
const GEMINI_MANUAL_REDIRECT_URI: &str = "https://codeassist.google.com/authcode";
const GEMINI_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl GeminiTokens {
    pub fn is_expired(&self) -> bool {
        let now_ms = chrono::Utc::now().timestamp_millis();
        self.expires_at <= now_ms + 60_000
    }
}

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct GoogleUserInfo {
    #[allow(dead_code)]
    id: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiCliOAuthCredentials {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expiry_date: Option<i64>,
    expires_at: Option<i64>,
    expires_in: Option<i64>,
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

/// Check if native Gemini OAuth tokens are available (including imported Gemini CLI tokens).
pub fn has_cached_auth() -> bool {
    load_tokens().is_ok()
}

pub fn tokens_path() -> Result<std::path::PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("gemini_oauth.json"))
}

pub fn gemini_cli_oauth_path() -> Result<std::path::PathBuf> {
    Ok(crate::storage::user_home_path(".gemini/oauth_creds.json")?)
}

pub fn load_tokens() -> Result<GeminiTokens> {
    let native_path = tokens_path()?;
    if native_path.exists() {
        crate::storage::harden_secret_file_permissions(&native_path);
        return crate::storage::read_json(&native_path)
            .with_context(|| format!("Failed to read {}", native_path.display()));
    }

    let cli_path = gemini_cli_oauth_path()?;
    if cli_path.exists() {
        crate::storage::harden_secret_file_permissions(&cli_path);
        let raw = std::fs::read_to_string(&cli_path)
            .with_context(|| format!("Failed to read {}", cli_path.display()))?;
        let imported: GeminiCliOAuthCredentials = serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse {}", cli_path.display()))?;
        let refresh_token = imported.refresh_token.filter(|value| !value.trim().is_empty());
        let access_token = imported.access_token.filter(|value| !value.trim().is_empty());
        if let (Some(refresh_token), Some(access_token)) = (refresh_token, access_token) {
            let expires_at = imported
                .expiry_date
                .or(imported.expires_at)
                .or_else(|| {
                    imported.expires_in.map(|expires_in| {
                        chrono::Utc::now().timestamp_millis() + (expires_in * 1000)
                    })
                })
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() - 1);
            return Ok(GeminiTokens {
                access_token,
                refresh_token,
                expires_at,
                email: None,
            });
        }
    }

    anyhow::bail!("No Gemini OAuth tokens found. Run `jcode login --provider gemini`.")
}

pub fn save_tokens(tokens: &GeminiTokens) -> Result<()> {
    let path = tokens_path()?;
    crate::storage::write_json_secret(&path, tokens)?;
    Ok(())
}

pub fn clear_tokens() -> Result<()> {
    let path = tokens_path()?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub async fn load_or_refresh_tokens() -> Result<GeminiTokens> {
    let tokens = load_tokens()?;
    if tokens.is_expired() {
        refresh_tokens(&tokens).await
    } else {
        Ok(tokens)
    }
}

pub async fn refresh_tokens(tokens: &GeminiTokens) -> Result<GeminiTokens> {
    let client = reqwest::Client::new();
    let resp = client
        .post(GOOGLE_TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", GEMINI_OAUTH_CLIENT_ID),
            ("client_secret", GEMINI_OAUTH_CLIENT_SECRET),
            ("refresh_token", tokens.refresh_token.as_str()),
        ])
        .send()
        .await
        .context("Failed to refresh Gemini OAuth token")?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Gemini token refresh failed: {}", body.trim());
    }

    let token_resp: GoogleTokenResponse = resp
        .json()
        .await
        .context("Failed to parse Gemini refresh response")?;
    let refreshed = GeminiTokens {
        access_token: token_resp.access_token,
        refresh_token: token_resp
            .refresh_token
            .unwrap_or_else(|| tokens.refresh_token.clone()),
        expires_at: chrono::Utc::now().timestamp_millis() + (token_resp.expires_in * 1000),
        email: tokens.email.clone(),
    };
    save_tokens(&refreshed)?;
    Ok(refreshed)
}

pub async fn login() -> Result<GeminiTokens> {
    let (verifier, challenge) = super::oauth::generate_pkce_public();
    let state = super::oauth::generate_state_public();

    if !browser_suppressed() {
        if let Ok(listener) = super::oauth::bind_callback_listener(0) {
            let port = listener.local_addr()?.port();
            let redirect_uri = format!("http://127.0.0.1:{port}/oauth2callback");
            let auth_url = build_auth_url(&redirect_uri, &challenge, &state);

            eprintln!("\nOpening browser for Gemini login...\n");
            eprintln!("If the browser didn't open, visit:\n{}\n", auth_url);
            if let Some(qr) = crate::login_qr::indented_section(
                &auth_url,
                "Scan this QR on another device if this machine has no browser:",
                "    ",
            ) {
                eprintln!("{qr}\n");
            }

            let browser_opened = open::that(&auth_url).is_ok();
            if browser_opened {
                eprintln!(
                    "Waiting up to 300s for automatic callback on {}",
                    redirect_uri
                );
                match tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    super::oauth::wait_for_callback_async_on_listener(listener, &state),
                )
                .await
                {
                    Ok(Ok(code)) => {
                        let tokens = exchange_authorization_code(&code, &verifier, &redirect_uri)
                            .await
                            .context("Gemini token exchange failed")?;
                        save_tokens(&tokens)?;
                        return Ok(tokens);
                    }
                    Ok(Err(err)) => {
                        eprintln!(
                            "Automatic callback failed ({err}). Falling back to manual auth code entry."
                        );
                    }
                    Err(_) => {
                        eprintln!(
                            "Timed out waiting for callback. Falling back to manual auth code entry."
                        );
                    }
                }
            } else {
                eprintln!(
                    "Couldn't open a browser on this machine. Falling back to manual auth code entry.\n"
                );
            }
        }
    }

    manual_login(&verifier, &challenge, &state).await
}

async fn manual_login(verifier: &str, challenge: &str, state: &str) -> Result<GeminiTokens> {
    if !io::stdin().is_terminal() {
        anyhow::bail!(
            "Gemini login needs an interactive terminal for manual code entry. Re-run in an interactive terminal."
        );
    }

    let auth_url = build_auth_url(GEMINI_MANUAL_REDIRECT_URI, challenge, state);
    eprintln!("\nManual Gemini auth required.\n");
    eprintln!("Open this URL in your browser:\n\n{}\n", auth_url);
    if let Some(qr) = crate::login_qr::indented_section(
        &auth_url,
        "Scan this QR on another device if needed:",
        "    ",
    ) {
        eprintln!("{qr}\n");
    }
    if !browser_suppressed() {
        let _ = open::that(&auth_url);
    }
    eprintln!(
        "After approving access, Google will show an authorization code. Paste it below.\n"
    );
    eprint!("Authorization code: ");
    io::stdout().flush()?;
    let code = crate::cli::login::read_secret_line()?;
    if code.trim().is_empty() {
        anyhow::bail!("No authorization code provided.");
    }

    let tokens = exchange_authorization_code(&code, verifier, GEMINI_MANUAL_REDIRECT_URI)
        .await
        .context("Gemini token exchange failed")?;
    save_tokens(&tokens)?;
    Ok(tokens)
}

async fn exchange_authorization_code(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<GeminiTokens> {
    let client = reqwest::Client::new();
    let resp = client
        .post(GOOGLE_TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", GEMINI_OAUTH_CLIENT_ID),
            ("client_secret", GEMINI_OAUTH_CLIENT_SECRET),
            ("code", code.trim()),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .context("Failed to exchange Gemini authorization code")?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Gemini token exchange failed: {}", body.trim());
    }

    let token_resp: GoogleTokenResponse = resp
        .json()
        .await
        .context("Failed to parse Gemini token exchange response")?;

    let refresh_token = token_resp.refresh_token.ok_or_else(|| {
        anyhow::anyhow!(
            "No refresh token received. Revoke access at https://myaccount.google.com/permissions and try again."
        )
    })?;

    let email = fetch_email(&token_resp.access_token).await.ok();
    Ok(GeminiTokens {
        access_token: token_resp.access_token,
        refresh_token,
        expires_at: chrono::Utc::now().timestamp_millis() + (token_resp.expires_in * 1000),
        email,
    })
}

pub async fn fetch_email(access_token: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(GOOGLE_USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .await
        .context("Failed to fetch Gemini Google profile")?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Failed to fetch Gemini Google profile: {}", body.trim());
    }

    let profile: GoogleUserInfo = resp
        .json()
        .await
        .context("Failed to parse Gemini Google profile")?;
    profile
        .email
        .filter(|email| !email.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Google profile did not include an email address"))
}

fn build_auth_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    let scope = GEMINI_SCOPES.join(" ");
    format!(
        "{base}?response_type=code&client_id={client_id}&redirect_uri={redirect_uri}&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&state={state}&access_type=offline&prompt=consent",
        base = GOOGLE_AUTHORIZE_URL,
        client_id = urlencoding::encode(GEMINI_OAUTH_CLIENT_ID),
        redirect_uri = urlencoding::encode(redirect_uri),
        scope = urlencoding::encode(&scope),
        challenge = urlencoding::encode(challenge),
        state = urlencoding::encode(state),
    )
}

fn browser_suppressed() -> bool {
    env_truthy("NO_BROWSER") || env_truthy("JCODE_NO_BROWSER")
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| {
            let trimmed = value.trim();
            !trimmed.is_empty() && trimmed != "0" && !trimmed.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
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
    use crate::storage::lock_test_env;

    #[test]
    fn parses_env_command_with_args() {
        let resolved =
            resolve_gemini_cli_command_with(Some("npx @google/gemini-cli --proxy test"), |_| {
                false
            });
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

    #[test]
    fn build_auth_url_contains_expected_redirect_uri() {
        let url = build_auth_url(
            GEMINI_MANUAL_REDIRECT_URI,
            "challenge-123",
            "state-123",
        );
        assert!(url.contains("codeassist.google.com%2Fauthcode"));
        assert!(url.contains("code_challenge=challenge-123"));
        assert!(url.contains("state=state-123"));
    }

    #[test]
    fn imports_cli_oauth_tokens_when_native_tokens_missing() {
        let _guard = lock_test_env();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let prev_home = std::env::var_os("JCODE_HOME");
        std::env::set_var("JCODE_HOME", temp.path());

        let cli_path = gemini_cli_oauth_path().expect("cli path");
        std::fs::create_dir_all(cli_path.parent().unwrap()).expect("create cli dir");
        std::fs::write(
            &cli_path,
            r#"{"access_token":"at-123","refresh_token":"rt-456","expiry_date":4102444800000}"#,
        )
        .expect("write cli token file");

        let tokens = load_tokens().expect("load tokens");
        assert_eq!(tokens.access_token, "at-123");
        assert_eq!(tokens.refresh_token, "rt-456");
        assert_eq!(tokens.expires_at, 4102444800000);

        if let Some(prev_home) = prev_home {
            std::env::set_var("JCODE_HOME", prev_home);
        } else {
            std::env::remove_var("JCODE_HOME");
        }
    }
}
