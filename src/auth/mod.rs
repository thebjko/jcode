pub mod account_store;
pub mod antigravity;
pub mod azure;
pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod external;
pub mod gemini;
pub mod google;
pub mod login_flows;
pub mod oauth;
pub mod refresh_state;
pub mod validation;

use crate::provider_catalog::LoginProviderAuthStateKey;
use crate::provider_catalog::LoginProviderDescriptor;
use crate::provider_catalog::openrouter_like_api_key_sources;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Instant;

static AUTH_STATUS_CACHE: std::sync::LazyLock<RwLock<Option<(AuthStatus, Instant)>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));
static AUTH_STATUS_FAST_CACHE: std::sync::LazyLock<RwLock<Option<(AuthStatus, Instant)>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

const AUTH_STATUS_CACHE_TTL_SECS: u64 = 30;
const AUTH_STATUS_FAST_CACHE_TTL_SECS: u64 = 5;

/// Per-process cache for command existence lookups.
/// CLI tools don't get installed/uninstalled while jcode is running, so caching
/// indefinitely per process is correct and avoids repeated PATH scans.
static COMMAND_EXISTS_CACHE: std::sync::LazyLock<Mutex<HashMap<String, bool>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn browser_suppressed(cli_no_browser: bool) -> bool {
    cli_no_browser || env_truthy("NO_BROWSER") || env_truthy("JCODE_NO_BROWSER")
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

fn auth_timing_logging_enabled() -> bool {
    env_truthy("JCODE_AUTH_TIMING")
}

/// Authentication status for all supported providers
#[derive(Debug, Clone, Default)]
pub struct AuthStatus {
    /// Jcode subscription router credentials
    pub jcode: AuthState,
    /// Anthropic provider (Claude models) - via OAuth or API key
    pub anthropic: ProviderAuth,
    /// OpenRouter provider - via API key
    pub openrouter: AuthState,
    /// Azure OpenAI provider - via Entra ID or API key
    pub azure: AuthState,
    /// OpenAI provider - via OAuth or API key
    pub openai: AuthState,
    /// OpenAI has OAuth credentials
    pub openai_has_oauth: bool,
    /// OpenAI has API key available
    pub openai_has_api_key: bool,
    /// Azure OpenAI has API key available
    pub azure_has_api_key: bool,
    /// Azure OpenAI is configured for Entra ID authentication
    pub azure_uses_entra: bool,
    /// Copilot API available (GitHub OAuth token found)
    pub copilot: AuthState,
    /// Copilot has API token (from hosts.json/apps.json/GITHUB_TOKEN)
    pub copilot_has_api_token: bool,
    /// Antigravity OAuth configured
    pub antigravity: AuthState,
    /// Gemini CLI available
    pub gemini: AuthState,
    /// Cursor provider configured via Cursor Agent plus API key or CLI session
    pub cursor: AuthState,
    /// Google/Gmail OAuth configured
    pub google: AuthState,
    /// Google Gmail has send capability (Full tier)
    pub google_can_send: bool,
}

/// Auth state for Anthropic which has multiple auth methods
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderAuth {
    /// Overall state (best of available methods)
    pub state: AuthState,
    /// Has OAuth credentials
    pub has_oauth: bool,
    /// Has API key
    pub has_api_key: bool,
}

/// State of a single auth credential
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthState {
    /// Credential is available and valid
    Available,
    /// Partial configuration exists (or OAuth may be expired)
    Expired,
    /// Credential is not configured
    #[default]
    NotConfigured,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthCredentialSource {
    #[default]
    None,
    EnvironmentVariable,
    AppConfigFile,
    JcodeManagedFile,
    TrustedExternalFile,
    TrustedExternalAppState,
    LocalCliSession,
    AzureDefaultCredential,
    Mixed,
}

impl AuthCredentialSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::EnvironmentVariable => "environment variable",
            Self::AppConfigFile => "app config file",
            Self::JcodeManagedFile => "jcode-managed file",
            Self::TrustedExternalFile => "trusted external file",
            Self::TrustedExternalAppState => "trusted external app state",
            Self::LocalCliSession => "local CLI session",
            Self::AzureDefaultCredential => "Azure DefaultAzureCredential",
            Self::Mixed => "mixed",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthExpiryConfidence {
    #[default]
    Unknown,
    Exact,
    PresenceOnly,
    ConfigurationOnly,
    NotApplicable,
}

impl AuthExpiryConfidence {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Exact => "exact timestamp",
            Self::PresenceOnly => "presence only",
            Self::ConfigurationOnly => "configuration only",
            Self::NotApplicable => "not applicable",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthRefreshSupport {
    #[default]
    Unknown,
    Automatic,
    Conditional,
    ManualRelogin,
    ExternalManaged,
    NotApplicable,
}

impl AuthRefreshSupport {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Automatic => "automatic",
            Self::Conditional => "conditional",
            Self::ManualRelogin => "manual re-login",
            Self::ExternalManaged => "external/manual",
            Self::NotApplicable => "not applicable",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthValidationMethod {
    #[default]
    Unknown,
    PresenceCheck,
    TimestampCheck,
    ConfigurationCheck,
    TrustedImportScan,
    CommandProbe,
    CompositeProbe,
}

impl AuthValidationMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::PresenceCheck => "presence check",
            Self::TimestampCheck => "timestamp check",
            Self::ConfigurationCheck => "configuration check",
            Self::TrustedImportScan => "trusted import scan",
            Self::CommandProbe => "command probe",
            Self::CompositeProbe => "composite probe",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderAuthAssessment {
    pub state: AuthState,
    pub method_detail: String,
    pub credential_source: AuthCredentialSource,
    pub credential_source_detail: String,
    pub expiry_confidence: AuthExpiryConfidence,
    pub refresh_support: AuthRefreshSupport,
    pub validation_method: AuthValidationMethod,
    pub last_validation: Option<crate::auth::validation::ProviderValidationRecord>,
    pub last_refresh: Option<crate::auth::refresh_state::ProviderRefreshRecord>,
}

impl ProviderAuthAssessment {
    pub fn health_summary(&self) -> String {
        let mut parts = vec![
            format!("source: {}", self.credential_source_detail),
            format!("expiry: {}", self.expiry_confidence.label()),
            format!("refresh: {}", self.refresh_support.label()),
            format!("probe: {}", self.validation_method.label()),
        ];

        if let Some(record) = self.last_refresh.as_ref() {
            parts.push(format!(
                "last refresh: {}",
                crate::auth::refresh_state::format_record_label(record)
            ));
        }

        parts.join(" · ")
    }
}

impl AuthStatus {
    /// Check all authentication sources and return their status.
    /// Results are cached for 30 seconds to avoid expensive PATH scanning on every frame.
    pub fn check() -> Self {
        if let Ok(cache) = AUTH_STATUS_CACHE.read()
            && let Some((ref status, ref when)) = *cache
            && when.elapsed().as_secs() < AUTH_STATUS_CACHE_TTL_SECS
        {
            return status.clone();
        }

        let status = Self::check_uncached();

        if let Ok(mut cache) = AUTH_STATUS_CACHE.write() {
            *cache = Some((status.clone(), Instant::now()));
        }
        if let Ok(mut cache) = AUTH_STATUS_FAST_CACHE.write() {
            *cache = Some((status.clone(), Instant::now()));
        }

        status
    }

    /// Fast auth snapshot for interactive UI surfaces like `/account`.
    ///
    /// Prefers any previously cached full probe (even if stale), and otherwise
    /// falls back to a cheap local-files/env-only probe that avoids subprocesses
    /// such as `cursor-agent status` or `sqlite3` lookups.
    pub fn check_fast() -> Self {
        if let Ok(cache) = AUTH_STATUS_CACHE.read()
            && let Some((ref status, _)) = *cache
        {
            return status.clone();
        }

        if let Ok(cache) = AUTH_STATUS_FAST_CACHE.read()
            && let Some((ref status, ref when)) = *cache
            && when.elapsed().as_secs() < AUTH_STATUS_FAST_CACHE_TTL_SECS
        {
            return status.clone();
        }

        let status = Self::check_uncached_fast();
        if let Ok(mut cache) = AUTH_STATUS_FAST_CACHE.write() {
            *cache = Some((status.clone(), Instant::now()));
        }

        status
    }

    /// Returns true if at least one provider has usable credentials.
    pub fn has_any_available(&self) -> bool {
        self.anthropic.state == AuthState::Available
            || self.jcode == AuthState::Available
            || self.openai == AuthState::Available
            || self.openrouter == AuthState::Available
            || self.azure == AuthState::Available
            || self.copilot == AuthState::Available
            || self.antigravity == AuthState::Available
            || self.gemini == AuthState::Available
            || self.cursor == AuthState::Available
    }

    pub fn has_any_untrusted_external_auth() -> bool {
        crate::auth::codex::has_unconsented_legacy_credentials()
            || crate::auth::claude::has_unconsented_external_auth().is_some()
            || crate::auth::external::has_any_unconsented_external_auth()
            || crate::auth::gemini::has_unconsented_cli_auth()
            || crate::auth::copilot::has_unconsented_external_auth().is_some()
            || crate::auth::cursor::has_unconsented_external_auth().is_some()
    }

    pub fn state_for_key(&self, key: LoginProviderAuthStateKey) -> AuthState {
        match key {
            LoginProviderAuthStateKey::ExternalImport => {
                if Self::has_any_untrusted_external_auth() {
                    AuthState::Available
                } else {
                    AuthState::NotConfigured
                }
            }
            LoginProviderAuthStateKey::Jcode => self.jcode,
            LoginProviderAuthStateKey::Anthropic => self.anthropic.state,
            LoginProviderAuthStateKey::OpenAi => self.openai,
            LoginProviderAuthStateKey::Azure => self.azure,
            LoginProviderAuthStateKey::OpenRouterLike => self.openrouter,
            LoginProviderAuthStateKey::Copilot => self.copilot,
            LoginProviderAuthStateKey::Antigravity => self.antigravity,
            LoginProviderAuthStateKey::Gemini => self.gemini,
            LoginProviderAuthStateKey::Cursor => self.cursor,
            LoginProviderAuthStateKey::Google => self.google,
        }
    }

    pub fn state_for_provider(&self, provider: LoginProviderDescriptor) -> AuthState {
        match provider.target {
            crate::provider_catalog::LoginProviderTarget::AutoImport => {
                if Self::has_any_untrusted_external_auth() {
                    AuthState::Available
                } else {
                    AuthState::NotConfigured
                }
            }
            crate::provider_catalog::LoginProviderTarget::Jcode => {
                if crate::subscription_catalog::has_credentials() {
                    AuthState::Available
                } else {
                    AuthState::NotConfigured
                }
            }
            crate::provider_catalog::LoginProviderTarget::OpenRouter => {
                if api_key_available("OPENROUTER_API_KEY", "openrouter.env") {
                    AuthState::Available
                } else {
                    AuthState::NotConfigured
                }
            }
            crate::provider_catalog::LoginProviderTarget::Azure => {
                if crate::auth::azure::has_configuration() {
                    AuthState::Available
                } else {
                    AuthState::NotConfigured
                }
            }
            crate::provider_catalog::LoginProviderTarget::OpenAiCompatible(profile) => {
                if crate::provider_catalog::openai_compatible_profile_is_configured(profile) {
                    AuthState::Available
                } else {
                    AuthState::NotConfigured
                }
            }
            _ => self.state_for_key(provider.auth_state_key),
        }
    }

    pub fn method_detail_for_provider(&self, provider: LoginProviderDescriptor) -> String {
        match provider.target {
            crate::provider_catalog::LoginProviderTarget::AutoImport => {
                if Self::has_any_untrusted_external_auth() {
                    "Existing external logins detected".to_string()
                } else {
                    "No importable external logins found".to_string()
                }
            }
            crate::provider_catalog::LoginProviderTarget::Jcode => {
                if self.state_for_provider(provider) == AuthState::Available {
                    if crate::subscription_catalog::has_router_base() {
                        format!(
                            "API key (`{}`) + router base",
                            crate::subscription_catalog::JCODE_API_KEY_ENV
                        )
                    } else {
                        format!(
                            "API key (`{}`), router base pending",
                            crate::subscription_catalog::JCODE_API_KEY_ENV
                        )
                    }
                } else {
                    "not configured".to_string()
                }
            }
            crate::provider_catalog::LoginProviderTarget::OpenRouter => {
                if self.state_for_provider(provider) == AuthState::Available {
                    "API key (`OPENROUTER_API_KEY`)".to_string()
                } else {
                    "not configured".to_string()
                }
            }
            crate::provider_catalog::LoginProviderTarget::Azure => {
                if self.state_for_provider(provider) == AuthState::Available {
                    crate::auth::azure::method_detail()
                } else {
                    "not configured".to_string()
                }
            }
            crate::provider_catalog::LoginProviderTarget::OpenAiCompatible(profile) => {
                let resolved = crate::provider_catalog::resolve_openai_compatible_profile(profile);
                if self.state_for_provider(provider) == AuthState::Available {
                    if resolved.requires_api_key {
                        format!("API key (`{}`)", resolved.api_key_env)
                    } else if crate::provider_catalog::load_api_key_from_env_or_config(
                        &resolved.api_key_env,
                        &resolved.env_file,
                    )
                    .is_some()
                    {
                        format!(
                            "local endpoint (`{}`) + optional API key (`{}`)",
                            resolved.api_base, resolved.api_key_env
                        )
                    } else {
                        format!("local endpoint (`{}`)", resolved.api_base)
                    }
                } else {
                    "not configured".to_string()
                }
            }
            _ => match provider.auth_state_key {
                LoginProviderAuthStateKey::Anthropic => {
                    let detail = if self.anthropic.has_oauth && self.anthropic.has_api_key {
                        "OAuth + API key"
                    } else if self.anthropic.has_oauth {
                        "OAuth"
                    } else if self.anthropic.has_api_key {
                        "API key"
                    } else {
                        "not configured"
                    };

                    let accounts = crate::auth::claude::list_accounts().unwrap_or_default();
                    if accounts.len() > 1 {
                        let active = crate::auth::claude::active_account_label()
                            .unwrap_or_else(|| "?".to_string());
                        format!(
                            "{detail} ({} accounts, active: `{}`)",
                            accounts.len(),
                            active
                        )
                    } else if accounts.len() == 1 {
                        format!("{detail} (account: `{}`)", accounts[0].label)
                    } else {
                        detail.to_string()
                    }
                }
                LoginProviderAuthStateKey::OpenAi => {
                    let detail = if self.openai_has_oauth && self.openai_has_api_key {
                        "OAuth + API key"
                    } else if self.openai_has_oauth {
                        "OAuth"
                    } else if self.openai_has_api_key {
                        "API key"
                    } else {
                        "not configured"
                    };

                    let accounts = crate::auth::codex::list_accounts().unwrap_or_default();
                    if accounts.len() > 1 {
                        let active = crate::auth::codex::active_account_label()
                            .unwrap_or_else(|| "?".to_string());
                        format!(
                            "{detail} ({} accounts, active: `{}`)",
                            accounts.len(),
                            active
                        )
                    } else if accounts.len() == 1 {
                        format!("{detail} (account: `{}`)", accounts[0].label)
                    } else {
                        detail.to_string()
                    }
                }
                _ => provider.auth_status_method.to_string(),
            },
        }
    }

    pub fn assessment_for_provider(
        &self,
        provider: LoginProviderDescriptor,
    ) -> ProviderAuthAssessment {
        let state = self.state_for_provider(provider);
        let method_detail = self.method_detail_for_provider(provider);
        let last_validation = crate::auth::validation::get(provider.id);
        let last_refresh = crate::auth::refresh_state::get(provider.id);

        let (
            credential_source,
            credential_source_detail,
            expiry_confidence,
            refresh_support,
            validation_method,
        ) = match provider.target {
            crate::provider_catalog::LoginProviderTarget::AutoImport => (
                if Self::has_any_untrusted_external_auth() {
                    AuthCredentialSource::TrustedExternalFile
                } else {
                    AuthCredentialSource::None
                },
                if Self::has_any_untrusted_external_auth() {
                    "untrusted external auth sources detected".to_string()
                } else {
                    "none detected".to_string()
                },
                AuthExpiryConfidence::Unknown,
                AuthRefreshSupport::ExternalManaged,
                AuthValidationMethod::TrustedImportScan,
            ),
            crate::provider_catalog::LoginProviderTarget::Jcode => {
                let (source, detail) = summarize_sources(vec![
                    env_source(crate::subscription_catalog::JCODE_API_KEY_ENV),
                    config_source(
                        crate::subscription_catalog::JCODE_API_KEY_ENV,
                        crate::subscription_catalog::JCODE_ENV_FILE,
                        "~/.config/jcode/jcode-subscription.env",
                    ),
                ]);
                (
                    source,
                    detail,
                    AuthExpiryConfidence::NotApplicable,
                    AuthRefreshSupport::NotApplicable,
                    AuthValidationMethod::PresenceCheck,
                )
            }
            crate::provider_catalog::LoginProviderTarget::OpenRouter => {
                let (source, detail) = summarize_sources(vec![
                    env_source("OPENROUTER_API_KEY"),
                    config_source(
                        "OPENROUTER_API_KEY",
                        "openrouter.env",
                        "~/.config/jcode/openrouter.env",
                    ),
                    external_api_key_source("OPENROUTER_API_KEY"),
                ]);
                (
                    source,
                    detail,
                    AuthExpiryConfidence::NotApplicable,
                    AuthRefreshSupport::NotApplicable,
                    AuthValidationMethod::PresenceCheck,
                )
            }
            crate::provider_catalog::LoginProviderTarget::Azure => {
                let (source, detail) = summarize_sources(vec![
                    azure_entra_source(),
                    env_source(crate::auth::azure::API_KEY_ENV),
                    config_source(
                        crate::auth::azure::API_KEY_ENV,
                        crate::auth::azure::ENV_FILE,
                        "~/.config/jcode/azure-openai.env",
                    ),
                ]);
                (
                    source,
                    detail,
                    AuthExpiryConfidence::ConfigurationOnly,
                    if crate::auth::azure::uses_entra_id() {
                        AuthRefreshSupport::Automatic
                    } else {
                        AuthRefreshSupport::NotApplicable
                    },
                    AuthValidationMethod::ConfigurationCheck,
                )
            }
            crate::provider_catalog::LoginProviderTarget::OpenAiCompatible(profile) => {
                let resolved = crate::provider_catalog::resolve_openai_compatible_profile(profile);
                let (source, detail) = summarize_sources(vec![
                    env_source(&resolved.api_key_env),
                    config_source(
                        &resolved.api_key_env,
                        &resolved.env_file,
                        format!("~/.config/jcode/{}", resolved.env_file),
                    ),
                    external_api_key_source(&resolved.api_key_env),
                ]);
                (
                    source,
                    detail,
                    AuthExpiryConfidence::NotApplicable,
                    AuthRefreshSupport::NotApplicable,
                    AuthValidationMethod::PresenceCheck,
                )
            }
            _ => assessment_for_key(self, provider.auth_state_key, state),
        };

        ProviderAuthAssessment {
            state,
            method_detail,
            credential_source,
            credential_source_detail,
            expiry_confidence,
            refresh_support,
            validation_method,
            last_validation,
            last_refresh,
        }
    }

    /// Invalidate the cached auth status so the next `check()` does a fresh probe.
    pub fn invalidate_cache() {
        if let Ok(mut cache) = AUTH_STATUS_CACHE.write() {
            *cache = None;
        }
        if let Ok(mut cache) = AUTH_STATUS_FAST_CACHE.write() {
            *cache = None;
        }
        crate::auth::copilot::invalidate_github_token_cache();
    }

    fn check_uncached() -> Self {
        let mut status = Self::default();

        if crate::subscription_catalog::has_credentials() {
            status.jcode = AuthState::Available;
        }

        // Check Anthropic (OAuth or API key)
        let mut anthropic = ProviderAuth::default();

        // Check OAuth
        if let Ok(creds) = claude::load_credentials() {
            let now_ms = chrono::Utc::now().timestamp_millis();
            anthropic.has_oauth = true;
            if creds.expires_at > now_ms {
                anthropic.state = AuthState::Available;
            } else {
                anthropic.state = AuthState::Expired;
            }
        }

        // Check API key (overrides expired OAuth)
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            anthropic.has_api_key = true;
            anthropic.state = AuthState::Available;
        }

        status.anthropic = anthropic;

        // Check OpenRouter/OpenAI-compatible API keys (env var or config file)
        let openrouter_like_keys = openrouter_like_api_key_sources();
        let openrouter_available = openrouter_like_keys
            .iter()
            .any(|(env_key, file_name)| api_key_available(env_key, file_name));

        if openrouter_available {
            status.openrouter = AuthState::Available;
        }

        status.azure_has_api_key = crate::auth::azure::has_api_key();
        status.azure_uses_entra = crate::auth::azure::uses_entra_id();
        if crate::auth::azure::has_configuration() {
            status.azure = AuthState::Available;
        }

        // Check OpenAI (Codex OAuth or API key)
        if let Ok(creds) = codex::load_credentials() {
            // Check if we have OAuth tokens (not just API key fallback)
            if !creds.refresh_token.is_empty() {
                status.openai_has_oauth = true;
                // Has OAuth - check expiry if available
                if let Some(expires_at) = creds.expires_at {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    if expires_at > now_ms {
                        status.openai = AuthState::Available;
                    } else {
                        status.openai = AuthState::Expired;
                    }
                } else {
                    // No expiry info, assume available
                    status.openai = AuthState::Available;
                }
            } else if !creds.access_token.is_empty() {
                // API key fallback
                status.openai_has_api_key = true;
                status.openai = AuthState::Available;
            }
        }

        // Fall back to env var (or combine with OAuth)
        if std::env::var("OPENAI_API_KEY")
            .ok()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            status.openai_has_api_key = true;
            status.openai = AuthState::Available;
        }

        // Check external/CLI auth providers (presence of installed CLI tooling)
        status.copilot = if copilot::has_copilot_credentials_fast() {
            status.copilot_has_api_token = true;
            AuthState::Available
        } else {
            AuthState::NotConfigured
        };

        status.antigravity = match antigravity::load_tokens() {
            Ok(tokens) => {
                if tokens.is_expired() {
                    AuthState::Expired
                } else {
                    AuthState::Available
                }
            }
            Err(_) => AuthState::NotConfigured,
        };

        status.gemini = match gemini::load_tokens() {
            Ok(tokens) => {
                if tokens.is_expired() {
                    AuthState::Expired
                } else {
                    AuthState::Available
                }
            }
            Err(_) => AuthState::NotConfigured,
        };

        let cursor_has_cli = cursor::has_cursor_agent_cli();
        let cursor_has_api_key = cursor::has_cursor_api_key();
        let cursor_has_native_auth = cursor::has_cursor_native_auth();
        let cursor_has_cli_auth = if cursor_has_cli {
            cursor::has_cursor_agent_auth()
        } else {
            false
        };

        status.cursor = if cursor_has_native_auth || (cursor_has_cli && cursor_has_cli_auth) {
            AuthState::Available
        } else if cursor_has_cli || cursor_has_api_key {
            AuthState::Expired
        } else {
            AuthState::NotConfigured
        };

        // Check Google/Gmail OAuth
        match google::load_tokens() {
            Ok(tokens) => {
                if tokens.is_expired() {
                    status.google = AuthState::Expired;
                } else {
                    status.google = AuthState::Available;
                }
                status.google_can_send = tokens.tier.can_send();
            }
            Err(_) => {
                status.google = AuthState::NotConfigured;
            }
        }

        status
    }

    fn check_uncached_fast() -> Self {
        let total_start = Instant::now();
        let mut status = Self::default();
        let mut timings = Vec::new();

        let step_start = Instant::now();
        if crate::subscription_catalog::has_credentials() {
            status.jcode = AuthState::Available;
        }
        timings.push(("jcode", step_start.elapsed().as_millis()));

        let step_start = Instant::now();
        let mut anthropic = ProviderAuth::default();
        if let Ok(creds) = claude::load_credentials() {
            let now_ms = chrono::Utc::now().timestamp_millis();
            anthropic.has_oauth = true;
            if creds.expires_at > now_ms {
                anthropic.state = AuthState::Available;
            } else {
                anthropic.state = AuthState::Expired;
            }
        }
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            anthropic.has_api_key = true;
            anthropic.state = AuthState::Available;
        }
        status.anthropic = anthropic;
        timings.push(("anthropic", step_start.elapsed().as_millis()));

        let step_start = Instant::now();
        let openrouter_like_keys = openrouter_like_api_key_sources();
        let openrouter_available = openrouter_like_keys
            .iter()
            .any(|(env_key, file_name)| api_key_available(env_key, file_name));
        if openrouter_available {
            status.openrouter = AuthState::Available;
        }
        timings.push(("openrouter", step_start.elapsed().as_millis()));

        let step_start = Instant::now();
        status.azure_has_api_key = crate::auth::azure::has_api_key();
        status.azure_uses_entra = crate::auth::azure::uses_entra_id();
        if crate::auth::azure::has_configuration() {
            status.azure = AuthState::Available;
        }
        timings.push(("azure", step_start.elapsed().as_millis()));

        let step_start = Instant::now();
        if let Ok(creds) = codex::load_credentials() {
            if !creds.refresh_token.is_empty() {
                status.openai_has_oauth = true;
                if let Some(expires_at) = creds.expires_at {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    if expires_at > now_ms {
                        status.openai = AuthState::Available;
                    } else {
                        status.openai = AuthState::Expired;
                    }
                } else {
                    status.openai = AuthState::Available;
                }
            } else if !creds.access_token.is_empty() {
                status.openai_has_api_key = true;
                status.openai = AuthState::Available;
            }
        }
        if std::env::var("OPENAI_API_KEY")
            .ok()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            status.openai_has_api_key = true;
            status.openai = AuthState::Available;
        }
        timings.push(("openai", step_start.elapsed().as_millis()));

        let step_start = Instant::now();
        status.copilot = if copilot::has_copilot_credentials() {
            status.copilot_has_api_token = true;
            AuthState::Available
        } else {
            AuthState::NotConfigured
        };
        timings.push(("copilot", step_start.elapsed().as_millis()));

        let step_start = Instant::now();
        status.antigravity = match antigravity::load_tokens() {
            Ok(tokens) => {
                if tokens.is_expired() {
                    AuthState::Expired
                } else {
                    AuthState::Available
                }
            }
            Err(_) => AuthState::NotConfigured,
        };
        timings.push(("antigravity", step_start.elapsed().as_millis()));

        let step_start = Instant::now();
        status.gemini = match gemini::load_tokens() {
            Ok(tokens) => {
                if tokens.is_expired() {
                    AuthState::Expired
                } else {
                    AuthState::Available
                }
            }
            Err(_) => AuthState::NotConfigured,
        };
        timings.push(("gemini", step_start.elapsed().as_millis()));

        let step_start = Instant::now();
        let cursor_has_cli = cursor::has_cursor_agent_cli();
        let cursor_has_api_key = cursor::has_cursor_api_key();
        let cursor_has_file_or_env_auth = cursor::load_access_token_from_env_or_file().is_ok();

        status.cursor = if cursor_has_file_or_env_auth || cursor_has_api_key {
            AuthState::Available
        } else if cursor_has_cli {
            AuthState::Expired
        } else {
            AuthState::NotConfigured
        };
        timings.push(("cursor", step_start.elapsed().as_millis()));

        let step_start = Instant::now();
        match google::load_tokens() {
            Ok(tokens) => {
                if tokens.is_expired() {
                    status.google = AuthState::Expired;
                } else {
                    status.google = AuthState::Available;
                }
                status.google_can_send = tokens.tier.can_send();
            }
            Err(_) => {
                status.google = AuthState::NotConfigured;
            }
        }

        timings.push(("google", step_start.elapsed().as_millis()));

        let nonzero: Vec<String> = timings
            .iter()
            .filter(|(_, ms)| *ms > 0)
            .map(|(name, ms)| format!("{name}={ms}ms"))
            .collect();
        if auth_timing_logging_enabled() {
            crate::logging::info(&format!(
                "[TIMING] auth_check_fast: total={}ms, nonzero=[{}]",
                total_start.elapsed().as_millis(),
                nonzero.join(", ")
            ));
        }

        status
    }
}

fn assessment_for_key(
    status: &AuthStatus,
    key: LoginProviderAuthStateKey,
    state: AuthState,
) -> (
    AuthCredentialSource,
    String,
    AuthExpiryConfidence,
    AuthRefreshSupport,
    AuthValidationMethod,
) {
    match key {
        LoginProviderAuthStateKey::Anthropic => {
            let (source, detail) = summarize_sources(vec![
                anthropic_oauth_source(status),
                env_source("ANTHROPIC_API_KEY"),
            ]);
            (
                source,
                detail,
                if status.anthropic.has_oauth {
                    AuthExpiryConfidence::Exact
                } else if status.anthropic.has_api_key {
                    AuthExpiryConfidence::NotApplicable
                } else {
                    AuthExpiryConfidence::Unknown
                },
                if status.anthropic.has_oauth {
                    AuthRefreshSupport::Automatic
                } else if status.anthropic.has_api_key {
                    AuthRefreshSupport::NotApplicable
                } else {
                    AuthRefreshSupport::Unknown
                },
                if status.anthropic.has_oauth {
                    AuthValidationMethod::TimestampCheck
                } else {
                    AuthValidationMethod::PresenceCheck
                },
            )
        }
        LoginProviderAuthStateKey::OpenAi => {
            let (source, detail) = summarize_sources(vec![
                openai_oauth_source(status),
                openai_api_key_source(status),
            ]);
            (
                source,
                detail,
                if status.openai_has_oauth {
                    AuthExpiryConfidence::Exact
                } else if status.openai_has_api_key {
                    AuthExpiryConfidence::NotApplicable
                } else {
                    AuthExpiryConfidence::Unknown
                },
                if status.openai_has_oauth {
                    AuthRefreshSupport::Automatic
                } else if status.openai_has_api_key {
                    AuthRefreshSupport::NotApplicable
                } else {
                    AuthRefreshSupport::Unknown
                },
                if status.openai_has_oauth {
                    AuthValidationMethod::TimestampCheck
                } else {
                    AuthValidationMethod::PresenceCheck
                },
            )
        }
        LoginProviderAuthStateKey::Copilot => {
            let (source, detail) = summarize_sources(vec![copilot_source()]);
            (
                source,
                detail,
                if state == AuthState::Available {
                    AuthExpiryConfidence::PresenceOnly
                } else {
                    AuthExpiryConfidence::Unknown
                },
                AuthRefreshSupport::ManualRelogin,
                AuthValidationMethod::CompositeProbe,
            )
        }
        LoginProviderAuthStateKey::Antigravity => {
            let (source, detail) = summarize_sources(vec![antigravity_source()]);
            (
                source,
                detail,
                if state == AuthState::NotConfigured {
                    AuthExpiryConfidence::Unknown
                } else {
                    AuthExpiryConfidence::Exact
                },
                AuthRefreshSupport::Automatic,
                AuthValidationMethod::TimestampCheck,
            )
        }
        LoginProviderAuthStateKey::Gemini => {
            let (source, detail) = summarize_sources(vec![gemini_source()]);
            (
                source,
                detail,
                if state == AuthState::NotConfigured {
                    AuthExpiryConfidence::Unknown
                } else {
                    AuthExpiryConfidence::Exact
                },
                AuthRefreshSupport::Automatic,
                AuthValidationMethod::TimestampCheck,
            )
        }
        LoginProviderAuthStateKey::Cursor => {
            let (source, detail) = summarize_sources(vec![cursor_source()]);
            (
                source,
                detail,
                if state == AuthState::Available {
                    AuthExpiryConfidence::PresenceOnly
                } else {
                    AuthExpiryConfidence::Unknown
                },
                AuthRefreshSupport::Conditional,
                AuthValidationMethod::CompositeProbe,
            )
        }
        LoginProviderAuthStateKey::Google => {
            let (source, detail) = summarize_sources(vec![google_source()]);
            (
                source,
                detail,
                if state == AuthState::NotConfigured {
                    AuthExpiryConfidence::Unknown
                } else {
                    AuthExpiryConfidence::Exact
                },
                AuthRefreshSupport::Automatic,
                AuthValidationMethod::TimestampCheck,
            )
        }
        LoginProviderAuthStateKey::Jcode
        | LoginProviderAuthStateKey::Azure
        | LoginProviderAuthStateKey::OpenRouterLike
        | LoginProviderAuthStateKey::ExternalImport => (
            AuthCredentialSource::None,
            "not configured".to_string(),
            AuthExpiryConfidence::Unknown,
            AuthRefreshSupport::Unknown,
            AuthValidationMethod::Unknown,
        ),
    }
}

fn summarize_sources(
    sources: Vec<Option<(AuthCredentialSource, String)>>,
) -> (AuthCredentialSource, String) {
    let mut collected = Vec::new();
    for source in sources.into_iter().flatten() {
        if !collected.iter().any(|(_, detail)| detail == &source.1) {
            collected.push(source);
        }
    }
    match collected.len() {
        0 => (AuthCredentialSource::None, "not configured".to_string()),
        1 => {
            let mut iter = collected.into_iter();
            if let Some(only) = iter.next() {
                only
            } else {
                unreachable!("collected.len() == 1 but no source was present")
            }
        }
        _ => (
            AuthCredentialSource::Mixed,
            collected
                .into_iter()
                .map(|(_, detail)| detail)
                .collect::<Vec<_>>()
                .join(" + "),
        ),
    }
}

fn env_source(env_key: &str) -> Option<(AuthCredentialSource, String)> {
    env_var_nonempty(env_key).then(|| {
        (
            AuthCredentialSource::EnvironmentVariable,
            format!("{env_key} environment variable"),
        )
    })
}

fn config_source(
    env_key: &str,
    file_name: &str,
    path_label: impl Into<String>,
) -> Option<(AuthCredentialSource, String)> {
    config_file_has_key(file_name, env_key).then(|| {
        (
            AuthCredentialSource::AppConfigFile,
            format!("{} ({env_key})", path_label.into()),
        )
    })
}

fn external_api_key_source(env_key: &str) -> Option<(AuthCredentialSource, String)> {
    crate::auth::external::load_api_key_for_env(env_key).map(|_| {
        (
            AuthCredentialSource::TrustedExternalFile,
            format!("trusted external auth import ({env_key})"),
        )
    })
}

fn azure_entra_source() -> Option<(AuthCredentialSource, String)> {
    crate::auth::azure::uses_entra_id().then(|| {
        (
            AuthCredentialSource::AzureDefaultCredential,
            "Azure DefaultAzureCredential".to_string(),
        )
    })
}

fn anthropic_oauth_source(status: &AuthStatus) -> Option<(AuthCredentialSource, String)> {
    if !status.anthropic.has_oauth {
        return None;
    }
    if !crate::auth::claude::list_accounts()
        .unwrap_or_default()
        .is_empty()
    {
        return Some((
            AuthCredentialSource::JcodeManagedFile,
            "~/.jcode/auth.json".to_string(),
        ));
    }
    if let Some(source) = crate::auth::claude::preferred_external_auth_source()
        && let Ok(path) = source.path()
        && crate::config::Config::external_auth_source_allowed_for_path(source.source_id(), &path)
    {
        return Some((
            AuthCredentialSource::TrustedExternalFile,
            format!("trusted external file ({})", path.display()),
        ));
    }
    if crate::auth::external::load_anthropic_oauth_tokens().is_some() {
        return Some((
            AuthCredentialSource::TrustedExternalFile,
            "trusted external auth import".to_string(),
        ));
    }
    None
}

fn openai_oauth_source(status: &AuthStatus) -> Option<(AuthCredentialSource, String)> {
    if !status.openai_has_oauth {
        return None;
    }
    if !crate::auth::codex::list_accounts()
        .unwrap_or_default()
        .is_empty()
    {
        return Some((
            AuthCredentialSource::JcodeManagedFile,
            "~/.jcode/openai-auth.json".to_string(),
        ));
    }
    if crate::auth::codex::legacy_auth_allowed() && crate::auth::codex::legacy_auth_source_exists()
    {
        return Some((
            AuthCredentialSource::TrustedExternalFile,
            "trusted legacy Codex auth file".to_string(),
        ));
    }
    if crate::auth::external::load_openai_oauth_tokens().is_some() {
        return Some((
            AuthCredentialSource::TrustedExternalFile,
            "trusted external auth import".to_string(),
        ));
    }
    None
}

fn openai_api_key_source(status: &AuthStatus) -> Option<(AuthCredentialSource, String)> {
    if !status.openai_has_api_key {
        return None;
    }
    env_source("OPENAI_API_KEY").or_else(|| {
        (crate::auth::codex::legacy_auth_allowed()
            && crate::auth::codex::legacy_auth_source_exists())
        .then(|| {
            (
                AuthCredentialSource::TrustedExternalFile,
                "trusted legacy Codex API key".to_string(),
            )
        })
    })
}

fn gemini_source() -> Option<(AuthCredentialSource, String)> {
    if let Ok(path) = crate::auth::gemini::tokens_path()
        && path.exists()
    {
        return Some((
            AuthCredentialSource::JcodeManagedFile,
            format!("{}", path.display()),
        ));
    }
    if let Ok(path) = crate::auth::gemini::gemini_cli_oauth_path()
        && path.exists()
        && crate::config::Config::external_auth_source_allowed_for_path(
            crate::auth::gemini::GEMINI_CLI_AUTH_SOURCE_ID,
            &path,
        )
    {
        return Some((
            AuthCredentialSource::TrustedExternalFile,
            format!("trusted Gemini CLI file ({})", path.display()),
        ));
    }
    crate::auth::external::load_gemini_oauth_tokens().map(|_| {
        (
            AuthCredentialSource::TrustedExternalFile,
            "trusted external auth import".to_string(),
        )
    })
}

fn antigravity_source() -> Option<(AuthCredentialSource, String)> {
    if let Ok(path) = crate::auth::antigravity::tokens_path()
        && path.exists()
    {
        return Some((
            AuthCredentialSource::JcodeManagedFile,
            format!("{}", path.display()),
        ));
    }
    crate::auth::external::load_antigravity_oauth_tokens().map(|_| {
        (
            AuthCredentialSource::TrustedExternalFile,
            "trusted external auth import".to_string(),
        )
    })
}

fn google_source() -> Option<(AuthCredentialSource, String)> {
    if let (Ok(tokens_path), Ok(credentials_path)) = (
        crate::auth::google::tokens_path(),
        crate::auth::google::credentials_path(),
    ) && tokens_path.exists()
        && credentials_path.exists()
    {
        return Some((
            AuthCredentialSource::JcodeManagedFile,
            format!("{} + {}", credentials_path.display(), tokens_path.display()),
        ));
    }
    None
}

fn cursor_source() -> Option<(AuthCredentialSource, String)> {
    if env_var_nonempty("CURSOR_ACCESS_TOKEN") || env_var_nonempty("CURSOR_API_KEY") {
        return Some((
            AuthCredentialSource::EnvironmentVariable,
            "CURSOR_ACCESS_TOKEN / CURSOR_API_KEY environment variable".to_string(),
        ));
    }
    if let Ok(file_path) = crate::auth::cursor::cursor_auth_file_path()
        && file_path.exists()
        && crate::config::Config::external_auth_source_allowed_for_path(
            crate::auth::cursor::CURSOR_AUTH_FILE_SOURCE_ID,
            &file_path,
        )
    {
        return Some((
            AuthCredentialSource::TrustedExternalFile,
            format!("trusted Cursor auth file ({})", file_path.display()),
        ));
    }
    if let Some(source) = crate::auth::cursor::preferred_external_auth_source()
        && matches!(
            source,
            crate::auth::cursor::ExternalCursorAuthSource::CursorVscdb
        )
        && let Ok(path) = source.path()
    {
        return Some((
            AuthCredentialSource::TrustedExternalAppState,
            format!("trusted Cursor app state ({})", path.display()),
        ));
    }
    if config_source("CURSOR_API_KEY", "cursor.env", "~/.config/jcode/cursor.env").is_some() {
        return config_source("CURSOR_API_KEY", "cursor.env", "~/.config/jcode/cursor.env");
    }
    if crate::auth::cursor::has_cursor_agent_auth() {
        return Some((
            AuthCredentialSource::LocalCliSession,
            "cursor-agent authenticated session".to_string(),
        ));
    }
    None
}

fn copilot_source() -> Option<(AuthCredentialSource, String)> {
    if env_var_nonempty("COPILOT_GITHUB_TOKEN")
        || env_var_nonempty("GH_TOKEN")
        || env_var_nonempty("GITHUB_TOKEN")
    {
        return Some((
            AuthCredentialSource::EnvironmentVariable,
            "COPILOT_GITHUB_TOKEN / GH_TOKEN / GITHUB_TOKEN".to_string(),
        ));
    }

    for source in [
        crate::auth::copilot::ExternalCopilotAuthSource::ConfigJson,
        crate::auth::copilot::ExternalCopilotAuthSource::HostsJson,
        crate::auth::copilot::ExternalCopilotAuthSource::AppsJson,
    ] {
        let path = source.path();
        if path.exists()
            && crate::config::Config::external_auth_source_allowed_for_path(
                source.source_id(),
                &path,
            )
        {
            return Some((
                AuthCredentialSource::TrustedExternalFile,
                format!("trusted Copilot file ({})", path.display()),
            ));
        }
    }

    if crate::auth::external::load_copilot_oauth_token().is_some() {
        return Some((
            AuthCredentialSource::TrustedExternalFile,
            "trusted external auth import".to_string(),
        ));
    }

    crate::auth::copilot::load_github_token().ok().map(|_| {
        (
            AuthCredentialSource::LocalCliSession,
            "gh CLI token fallback".to_string(),
        )
    })
}

fn env_var_nonempty(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn config_file_has_key(file_name: &str, env_key: &str) -> bool {
    let Ok(config_dir) = crate::storage::app_config_dir() else {
        return false;
    };
    let path = config_dir.join(file_name);
    config_file_contains_assignment(&path, env_key)
}

fn config_file_contains_assignment(path: &Path, env_key: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let prefix = format!("{env_key}=");
    content.lines().any(|line| {
        line.strip_prefix(&prefix)
            .map(|value| !value.trim().trim_matches('"').trim_matches('\'').is_empty())
            .unwrap_or(false)
    })
}

fn api_key_available(env_key: &str, file_name: &str) -> bool {
    crate::provider_catalog::load_api_key_from_env_or_config(env_key, file_name).is_some()
}

pub(crate) fn command_available_from_env(env_var: &str, fallback: &str) -> bool {
    if let Ok(cmd) = std::env::var(env_var) {
        let trimmed = cmd.trim();
        if !trimmed.is_empty() && command_exists(trimmed) {
            return true;
        }
    }

    command_exists(fallback)
}

fn command_exists(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }

    // Absolute/relative path: direct stat, no caching needed
    let path = std::path::Path::new(command);
    if path.is_absolute() || contains_path_separator(command) {
        return explicit_command_exists(path);
    }

    // Check per-process cache first (O(1) on repeated calls)
    if let Ok(cache) = COMMAND_EXISTS_CACHE.lock()
        && let Some(&cached) = cache.get(command)
    {
        return cached;
    }

    let path_var = match std::env::var_os("PATH") {
        Some(p) if !p.is_empty() => p,
        _ => {
            cache_command_result(command, false);
            return false;
        }
    };

    let wsl2 = is_wsl2();
    let found = std::env::split_paths(&path_var)
        // On WSL2 skip Windows DrvFs mounts (/mnt/c, /mnt/d, …) — they are
        // accessed via the slow 9P filesystem and CLI tools are never there.
        .filter(|dir| !(wsl2 && is_wsl2_windows_path(dir)))
        .flat_map(|dir| {
            command_candidates(command)
                .into_iter()
                .map(move |c| dir.join(c))
        })
        .any(|p| p.exists());

    cache_command_result(command, found);
    found
}

fn cache_command_result(command: &str, exists: bool) {
    if let Ok(mut cache) = COMMAND_EXISTS_CACHE.lock() {
        cache.insert(command.to_string(), exists);
    }
}

/// Detect WSL2: reads `/proc/version` once and caches the result for the
/// process lifetime.  Returns false on any platform without that file.
fn is_wsl2() -> bool {
    static IS_WSL2: OnceLock<bool> = OnceLock::new();
    *IS_WSL2.get_or_init(|| {
        std::fs::read_to_string("/proc/version")
            .map(|s| s.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false)
    })
}

/// Returns true for paths like `/mnt/c`, `/mnt/d`, … that are Windows drive
/// mounts under WSL2 (DrvFs via 9P).
fn is_wsl2_windows_path(dir: &std::path::Path) -> bool {
    use std::path::Component;
    let mut it = dir.components();
    if !matches!(it.next(), Some(Component::RootDir)) {
        return false;
    }
    if !matches!(it.next(), Some(Component::Normal(s)) if s == "mnt") {
        return false;
    }
    if let Some(Component::Normal(drive)) = it.next() {
        let s = drive.to_string_lossy();
        return s.len() == 1 && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    }
    false
}

fn explicit_command_exists(path: &std::path::Path) -> bool {
    if path.exists() {
        return true;
    }

    if has_extension(path) {
        return false;
    }

    #[cfg(windows)]
    {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        for ext in pathext
            .split(';')
            .map(str::trim)
            .filter(|ext| !ext.is_empty())
        {
            let candidate = path.with_extension(ext.trim_start_matches('.'));
            if candidate.exists() {
                return true;
            }
        }
    }

    false
}

fn command_candidates(command: &str) -> Vec<std::ffi::OsString> {
    let path = std::path::Path::new(command);
    let file_name = match path.file_name() {
        Some(name) => name.to_os_string(),
        None => return Vec::new(),
    };

    if has_extension(path) {
        return vec![file_name];
    }

    #[allow(unused_mut)]
    let mut candidates = vec![file_name.clone()];

    #[cfg(windows)]
    {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let exts: Vec<&str> = pathext
            .split(';')
            .map(str::trim)
            .filter(|ext| !ext.is_empty())
            .collect();

        for ext in exts {
            let ext_no_dot = ext.trim_start_matches('.');
            if ext_no_dot.is_empty() {
                continue;
            }
            let mut candidate = path.to_path_buf();
            candidate.set_extension(ext_no_dot);
            if let Some(cand_name) = candidate.file_name() {
                candidates.push(cand_name.to_os_string());
            }
        }
    }

    dedup_preserve_order(candidates)
}

fn contains_path_separator(command: &str) -> bool {
    command.contains('/')
        || command.contains('\\')
        || std::path::Path::new(command).components().count() > 1
}

fn has_extension(path: &std::path::Path) -> bool {
    path.extension().is_some()
}

fn dedup_preserve_order(mut values: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut out = Vec::new();
    for value in values.drain(..) {
        if !out.iter().any(|v| v == &value) {
            out.push(value);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn restore_env_var(key: &str, previous: Option<OsString>) {
        if let Some(previous) = previous {
            crate::env::set_var(key, previous);
        } else {
            crate::env::remove_var(key);
        }
    }

    #[cfg(unix)]
    fn write_mock_cursor_agent(dir: &std::path::Path, script_body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("cursor-agent-mock");
        std::fs::write(&path, script_body).expect("write mock cursor agent");
        let mut permissions = std::fs::metadata(&path)
            .expect("stat mock cursor agent")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).expect("chmod mock cursor agent");
        path
    }

    #[test]
    fn command_candidates_adds_extension_on_windows() {
        crate::env::set_var("PATHEXT", ".EXE;.BAT");
        let candidates = command_candidates("testcmd");
        if cfg!(windows) {
            let normalized: Vec<String> = candidates
                .iter()
                .map(|c| c.to_string_lossy().to_ascii_lowercase())
                .collect();
            assert!(normalized.iter().any(|c| c == "testcmd"));
            assert!(normalized.iter().any(|c| c == "testcmd.exe"));
            assert!(normalized.iter().any(|c| c == "testcmd.bat"));
        } else {
            assert_eq!(candidates.len(), 1);
            assert!(candidates.iter().any(|c| c == "testcmd"));
        }
    }

    #[test]
    fn auth_state_default_is_not_configured() {
        let state = AuthState::default();
        assert_eq!(state, AuthState::NotConfigured);
    }

    #[test]
    fn auth_status_default_all_not_configured() {
        let status = AuthStatus::default();
        assert_eq!(status.anthropic.state, AuthState::NotConfigured);
        assert_eq!(status.openrouter, AuthState::NotConfigured);
        assert_eq!(status.openai, AuthState::NotConfigured);
        assert_eq!(status.copilot, AuthState::NotConfigured);
        assert_eq!(status.cursor, AuthState::NotConfigured);
        assert_eq!(status.antigravity, AuthState::NotConfigured);
        assert!(!status.openai_has_oauth);
        assert!(!status.openai_has_api_key);
        assert!(!status.copilot_has_api_token);
        assert!(!status.anthropic.has_oauth);
        assert!(!status.anthropic.has_api_key);
    }

    #[test]
    fn provider_auth_default() {
        let auth = ProviderAuth::default();
        assert_eq!(auth.state, AuthState::NotConfigured);
        assert!(!auth.has_oauth);
        assert!(!auth.has_api_key);
    }

    #[test]
    fn command_exists_for_known_binary() {
        if cfg!(windows) {
            assert!(command_exists("cmd") || command_exists("cmd.exe"));
        } else {
            assert!(command_exists("ls"));
        }
    }

    #[test]
    fn command_exists_empty_string() {
        assert!(!command_exists(""));
        assert!(!command_exists("   "));
    }

    #[test]
    fn command_exists_nonexistent() {
        assert!(!command_exists("surely_this_binary_does_not_exist_xyz"));
    }

    #[test]
    fn command_exists_absolute_path() {
        if cfg!(windows) {
            assert!(command_exists(r"C:\Windows\System32\cmd.exe"));
        } else {
            assert!(command_exists("/bin/ls") || command_exists("/usr/bin/ls"));
        }
    }

    #[test]
    fn command_exists_absolute_nonexistent() {
        assert!(!command_exists("/nonexistent/path/to/binary"));
    }

    #[test]
    fn contains_path_separator_detection() {
        assert!(contains_path_separator("/usr/bin/test"));
        assert!(contains_path_separator("./test"));
        assert!(!contains_path_separator("test"));
    }

    #[test]
    fn has_extension_detection() {
        assert!(has_extension(std::path::Path::new("test.exe")));
        assert!(!has_extension(std::path::Path::new("test")));
        assert!(has_extension(std::path::Path::new("test.sh")));
    }

    #[test]
    fn dedup_preserves_order() {
        let input = vec![
            std::ffi::OsString::from("a"),
            std::ffi::OsString::from("b"),
            std::ffi::OsString::from("a"),
            std::ffi::OsString::from("c"),
        ];
        let result = dedup_preserve_order(input);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], "a");
        assert_eq!(result[1], "b");
        assert_eq!(result[2], "c");
    }

    #[test]
    fn auth_state_equality() {
        assert_eq!(AuthState::Available, AuthState::Available);
        assert_eq!(AuthState::Expired, AuthState::Expired);
        assert_eq!(AuthState::NotConfigured, AuthState::NotConfigured);
        assert_ne!(AuthState::Available, AuthState::Expired);
        assert_ne!(AuthState::Available, AuthState::NotConfigured);
    }

    #[test]
    fn is_wsl2_windows_path_matches_drive_mounts() {
        assert!(is_wsl2_windows_path(std::path::Path::new("/mnt/c")));
        assert!(is_wsl2_windows_path(std::path::Path::new("/mnt/d")));
        assert!(is_wsl2_windows_path(std::path::Path::new("/mnt/z")));
        assert!(is_wsl2_windows_path(std::path::Path::new(
            "/mnt/c/Windows/System32"
        )));
    }

    #[test]
    fn is_wsl2_windows_path_rejects_non_drives() {
        // /mnt/wsl is a WSL-internal mount, not a Windows drive
        assert!(!is_wsl2_windows_path(std::path::Path::new("/mnt/wsl")));
        // /usr/bin is a plain Linux directory
        assert!(!is_wsl2_windows_path(std::path::Path::new("/usr/bin")));
        // /mnt alone is not a drive
        assert!(!is_wsl2_windows_path(std::path::Path::new("/mnt")));
        // empty
        assert!(!is_wsl2_windows_path(std::path::Path::new("")));
    }

    #[test]
    fn command_exists_cached_on_second_call() {
        // Clear cache first to isolate this test
        if let Ok(mut cache) = COMMAND_EXISTS_CACHE.lock() {
            cache.remove("surely_this_binary_does_not_exist_xyz_cache_test");
        }
        // First call populates the cache
        let result1 = command_exists("surely_this_binary_does_not_exist_xyz_cache_test");
        assert!(!result1);
        // Second call must return same result (from cache)
        let result2 = command_exists("surely_this_binary_does_not_exist_xyz_cache_test");
        assert_eq!(result1, result2);
    }

    #[test]
    fn auth_status_check_returns_valid_struct() {
        let status = AuthStatus::check();
        // Just verify it runs without panicking and has coherent state
        match status.anthropic.state {
            AuthState::Available | AuthState::Expired | AuthState::NotConfigured => {}
        }
        match status.openai {
            AuthState::Available | AuthState::Expired | AuthState::NotConfigured => {}
        }
        // If copilot has api token, state should be Available
        if status.copilot_has_api_token {
            assert_eq!(status.copilot, AuthState::Available);
        }
    }

    #[test]
    fn openrouter_like_status_is_provider_specific() {
        let _lock = crate::storage::lock_test_env();
        let prev_chutes = std::env::var_os("CHUTES_API_KEY");
        let prev_opencode = std::env::var_os("OPENCODE_API_KEY");

        crate::env::set_var("CHUTES_API_KEY", "chutes-test-key");
        crate::env::remove_var("OPENCODE_API_KEY");
        AuthStatus::invalidate_cache();

        let status = AuthStatus::check();
        assert_eq!(
            status.state_for_provider(crate::provider_catalog::CHUTES_LOGIN_PROVIDER),
            AuthState::Available
        );
        assert_eq!(
            status.state_for_provider(crate::provider_catalog::OPENCODE_LOGIN_PROVIDER),
            AuthState::NotConfigured
        );
        assert_eq!(
            status.method_detail_for_provider(crate::provider_catalog::CHUTES_LOGIN_PROVIDER),
            "API key (`CHUTES_API_KEY`)".to_string()
        );

        restore_env_var("CHUTES_API_KEY", prev_chutes);
        restore_env_var("OPENCODE_API_KEY", prev_opencode);
        AuthStatus::invalidate_cache();
    }

    #[cfg(unix)]
    #[test]
    fn cursor_status_is_available_when_api_key_exists_without_cli() {
        let _lock = crate::storage::lock_test_env();
        let prev_access_token = std::env::var_os("CURSOR_ACCESS_TOKEN");
        let prev_refresh_token = std::env::var_os("CURSOR_REFRESH_TOKEN");
        let prev_api_key = std::env::var_os("CURSOR_API_KEY");
        let prev_cli_path = std::env::var_os("JCODE_CURSOR_CLI_PATH");
        let temp = tempfile::TempDir::new().expect("create temp dir");

        crate::env::remove_var("CURSOR_ACCESS_TOKEN");
        crate::env::remove_var("CURSOR_REFRESH_TOKEN");
        crate::env::set_var("CURSOR_API_KEY", "cursor-test-key");
        crate::env::set_var(
            "JCODE_CURSOR_CLI_PATH",
            temp.path().join("missing-cursor-agent"),
        );
        AuthStatus::invalidate_cache();

        let status = AuthStatus::check();
        assert_eq!(status.cursor, AuthState::Available);

        restore_env_var("CURSOR_ACCESS_TOKEN", prev_access_token);
        restore_env_var("CURSOR_REFRESH_TOKEN", prev_refresh_token);
        restore_env_var("CURSOR_API_KEY", prev_api_key);
        restore_env_var("JCODE_CURSOR_CLI_PATH", prev_cli_path);
        AuthStatus::invalidate_cache();
    }

    #[cfg(unix)]
    #[test]
    fn cursor_status_is_available_for_native_auth_without_cli() {
        let _lock = crate::storage::lock_test_env();
        let prev_access_token = std::env::var_os("CURSOR_ACCESS_TOKEN");
        let prev_refresh_token = std::env::var_os("CURSOR_REFRESH_TOKEN");
        let prev_api_key = std::env::var_os("CURSOR_API_KEY");
        let prev_cli_path = std::env::var_os("JCODE_CURSOR_CLI_PATH");
        let temp = tempfile::TempDir::new().expect("create temp dir");

        crate::env::set_var(
            "CURSOR_ACCESS_TOKEN",
            "eyJhbGciOiJub25lIn0.eyJleHAiIjo0MTAyNDQ0ODAwfQ.",
        );
        crate::env::remove_var("CURSOR_REFRESH_TOKEN");
        crate::env::remove_var("CURSOR_API_KEY");
        crate::env::set_var(
            "JCODE_CURSOR_CLI_PATH",
            temp.path().join("missing-cursor-agent"),
        );
        AuthStatus::invalidate_cache();

        let status = AuthStatus::check();
        assert_eq!(status.cursor, AuthState::Available);

        restore_env_var("CURSOR_ACCESS_TOKEN", prev_access_token);
        restore_env_var("CURSOR_REFRESH_TOKEN", prev_refresh_token);
        restore_env_var("CURSOR_API_KEY", prev_api_key);
        restore_env_var("JCODE_CURSOR_CLI_PATH", prev_cli_path);
        AuthStatus::invalidate_cache();
    }

    #[cfg(unix)]
    #[test]
    fn cursor_status_is_available_for_authenticated_cli_session() {
        let _lock = crate::storage::lock_test_env();
        let prev_api_key = std::env::var_os("CURSOR_API_KEY");
        let prev_cli_path = std::env::var_os("JCODE_CURSOR_CLI_PATH");
        let temp = tempfile::TempDir::new().expect("create temp dir");
        let mock_cli = write_mock_cursor_agent(
            temp.path(),
            "#!/bin/sh\nif [ \"$1\" = \"status\" ]; then\n  echo \"Authenticated\\nAccount: test@example.com\"\n  exit 0\nfi\nexit 1\n",
        );

        crate::env::remove_var("CURSOR_API_KEY");
        crate::env::set_var("JCODE_CURSOR_CLI_PATH", &mock_cli);
        AuthStatus::invalidate_cache();

        let status = AuthStatus::check();
        assert_eq!(status.cursor, AuthState::Available);

        restore_env_var("CURSOR_API_KEY", prev_api_key);
        restore_env_var("JCODE_CURSOR_CLI_PATH", prev_cli_path);
        AuthStatus::invalidate_cache();
    }

    #[test]
    fn configured_api_key_source_uses_valid_overrides() {
        let _lock = crate::storage::lock_test_env();
        let key_var = "JCODE_OPENAI_COMPAT_API_KEY_NAME";
        let file_var = "JCODE_OPENAI_COMPAT_ENV_FILE";
        let prev_key = std::env::var(key_var).ok();
        let prev_file = std::env::var(file_var).ok();

        crate::env::set_var(key_var, "GROQ_API_KEY");
        crate::env::set_var(file_var, "groq.env");

        let source = crate::provider_catalog::configured_api_key_source(
            key_var,
            file_var,
            "OPENAI_COMPAT_API_KEY",
            "compat.env",
        );
        assert_eq!(
            source,
            Some(("GROQ_API_KEY".to_string(), "groq.env".to_string()))
        );

        if let Some(v) = prev_key {
            crate::env::set_var(key_var, v);
        } else {
            crate::env::remove_var(key_var);
        }
        if let Some(v) = prev_file {
            crate::env::set_var(file_var, v);
        } else {
            crate::env::remove_var(file_var);
        }
    }

    #[test]
    fn configured_api_key_source_rejects_invalid_values() {
        let _lock = crate::storage::lock_test_env();
        let key_var = "JCODE_OPENAI_COMPAT_API_KEY_NAME";
        let file_var = "JCODE_OPENAI_COMPAT_ENV_FILE";
        let prev_key = std::env::var(key_var).ok();
        let prev_file = std::env::var(file_var).ok();

        crate::env::set_var(key_var, "bad-key");
        crate::env::set_var(file_var, "../bad.env");

        let source = crate::provider_catalog::configured_api_key_source(
            key_var,
            file_var,
            "OPENAI_COMPAT_API_KEY",
            "compat.env",
        );
        assert!(source.is_none());

        if let Some(v) = prev_key {
            crate::env::set_var(key_var, v);
        } else {
            crate::env::remove_var(key_var);
        }
        if let Some(v) = prev_file {
            crate::env::set_var(file_var, v);
        } else {
            crate::env::remove_var(file_var);
        }
    }
}
