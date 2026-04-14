mod paths;
mod source_state;
mod storage_helpers;

pub use paths::{
    SELFDEV_CARGO_PROFILE, binary_name, binary_stem, client_update_candidate,
    current_binary_build_time_string, current_binary_built_at, find_dev_binary,
    find_repo_in_ancestors, get_repo_dir, is_jcode_repo, launcher_binary_path, launcher_dir,
    preferred_reload_candidate, release_binary_path, run_selfdev_build, selfdev_binary_path,
    selfdev_build_command, update_launcher_symlink_to_current, update_launcher_symlink_to_stable,
};
pub use source_state::{
    current_build_info, current_git_diff, current_git_hash, current_git_hash_full,
    current_source_state, ensure_source_state_matches, get_commit_message, is_working_tree_dirty,
    repo_build_version, repo_scope_key, worktree_scope_key,
};
pub use storage_helpers::{
    build_log_path, build_progress_path, builds_dir, canary_binary_path, clear_build_progress,
    clear_migration_context, current_binary_path, current_version_file, load_migration_context,
    manifest_path, migration_context_path, read_build_progress, read_current_version,
    read_stable_version, save_migration_context, stable_binary_path, stable_version_file,
    version_binary_path, write_build_progress,
};

use crate::storage;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelfDevBuildCommand {
    pub program: String,
    pub args: Vec<String>,
    pub display: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceState {
    pub repo_scope: String,
    pub worktree_scope: String,
    pub short_hash: String,
    pub full_hash: String,
    pub dirty: bool,
    pub fingerprint: String,
    pub version_label: String,
    pub changed_paths: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublishedBuild {
    pub version: String,
    pub source_fingerprint: String,
    pub versioned_path: PathBuf,
    pub current_link: PathBuf,
    pub launcher_link: PathBuf,
    pub previous_current_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingActivation {
    pub session_id: String,
    pub new_version: String,
    pub previous_current_version: Option<String>,
    pub source_fingerprint: Option<String>,
    pub requested_at: DateTime<Utc>,
}

/// Status of a canary build being tested
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CanaryStatus {
    /// Build is currently being tested
    #[serde(alias = "Testing")]
    Testing,
    /// Build passed all tests and is ready for promotion
    #[serde(alias = "Passed")]
    Passed,
    /// Build failed testing
    #[serde(alias = "Failed")]
    Failed,
}

/// Information about a specific build version
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildInfo {
    /// Git commit hash (short)
    pub hash: String,
    /// Git commit hash (full)
    pub full_hash: String,
    /// Build timestamp
    pub built_at: DateTime<Utc>,
    /// Git commit message (first line)
    pub commit_message: Option<String>,
    /// Whether build is from dirty working tree
    pub dirty: bool,
    /// Stable fingerprint of the source state used to produce the build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<String>,
    /// Immutable published version label, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_label: Option<String>,
}

/// Manifest tracking build versions and their status
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuildManifest {
    /// Current stable build hash (known good)
    pub stable: Option<String>,
    /// Current canary build hash (being tested)
    pub canary: Option<String>,
    /// Session ID testing the canary build
    pub canary_session: Option<String>,
    /// Status of canary testing
    pub canary_status: Option<CanaryStatus>,
    /// History of recent builds
    #[serde(default)]
    pub history: Vec<BuildInfo>,
    /// Last crash information (if canary crashed)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_crash: Option<CrashInfo>,
    /// Pending activation being validated across reload/resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_activation: Option<PendingActivation>,
}

/// Information about a crash during canary testing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashInfo {
    /// Build hash that crashed
    pub build_hash: String,
    /// Exit code
    pub exit_code: i32,
    /// Stderr output (truncated)
    pub stderr: String,
    /// Timestamp of crash
    pub crashed_at: DateTime<Utc>,
    /// Git diff that was being tested
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// Context saved before migrating to a canary build
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationContext {
    pub session_id: String,
    pub from_version: String,
    pub to_version: String,
    pub change_summary: Option<String>,
    pub diff: Option<String>,
    pub timestamp: DateTime<Utc>,
}

impl BuildManifest {
    /// Load manifest from disk
    pub fn load() -> Result<Self> {
        let path = manifest_path()?;
        if path.exists() {
            storage::read_json(&path)
        } else {
            Ok(Self::default())
        }
    }

    /// Save manifest to disk
    pub fn save(&self) -> Result<()> {
        let path = manifest_path()?;
        storage::write_json(&path, self)
    }

    /// Check if we should use stable or canary for a given session
    pub fn binary_for_session(&self, session_id: &str) -> BinaryChoice {
        // If this session is the canary tester, use canary
        if let Some(ref canary_session) = self.canary_session
            && canary_session == session_id
            && let Some(ref canary) = self.canary
        {
            return BinaryChoice::Canary(canary.clone());
        }
        // Otherwise use stable
        if let Some(ref stable) = self.stable {
            BinaryChoice::Stable(stable.clone())
        } else {
            BinaryChoice::Current
        }
    }

    /// Start canary testing for a session
    pub fn start_canary(&mut self, hash: &str, session_id: &str) -> Result<()> {
        self.canary = Some(hash.to_string());
        self.canary_session = Some(session_id.to_string());
        self.canary_status = Some(CanaryStatus::Testing);
        self.save()
    }

    /// Mark canary as passed
    pub fn mark_canary_passed(&mut self) -> Result<()> {
        self.canary_status = Some(CanaryStatus::Passed);
        self.save()
    }

    /// Mark canary as failed
    pub fn mark_canary_failed(&mut self) -> Result<()> {
        self.canary_status = Some(CanaryStatus::Failed);
        self.save()
    }

    /// Record a crash
    pub fn record_crash(
        &mut self,
        hash: &str,
        exit_code: i32,
        stderr: &str,
        diff: Option<String>,
    ) -> Result<()> {
        self.last_crash = Some(CrashInfo {
            build_hash: hash.to_string(),
            exit_code,
            stderr: stderr.chars().take(4096).collect(), // Truncate
            crashed_at: Utc::now(),
            diff,
        });
        self.canary_status = Some(CanaryStatus::Failed);
        self.save()
    }

    /// Clear crash info after it's been handled
    pub fn clear_crash(&mut self) -> Result<()> {
        self.last_crash = None;
        self.save()
    }

    pub fn set_pending_activation(&mut self, activation: PendingActivation) -> Result<()> {
        self.pending_activation = Some(activation);
        self.save()
    }

    pub fn clear_pending_activation(&mut self) -> Result<()> {
        self.pending_activation = None;
        self.save()
    }

    /// Add build to history
    pub fn add_to_history(&mut self, info: BuildInfo) -> Result<()> {
        // Keep last 20 builds
        self.history.insert(0, info);
        self.history.truncate(20);
        self.save()
    }
}

pub fn complete_pending_activation_for_session(session_id: &str) -> Result<Option<String>> {
    let mut manifest = BuildManifest::load()?;
    let Some(pending) = manifest.pending_activation.clone() else {
        return Ok(None);
    };
    if pending.session_id != session_id {
        return Ok(None);
    }

    manifest.canary = Some(pending.new_version.clone());
    manifest.canary_session = Some(session_id.to_string());
    manifest.canary_status = Some(CanaryStatus::Passed);
    manifest.pending_activation = None;
    manifest.last_crash = None;
    manifest.save()?;
    Ok(Some(pending.new_version))
}

pub fn rollback_pending_activation_for_session(session_id: &str) -> Result<Option<String>> {
    let mut manifest = BuildManifest::load()?;
    let Some(pending) = manifest.pending_activation.clone() else {
        return Ok(None);
    };
    if pending.session_id != session_id {
        return Ok(None);
    }

    if let Some(previous) = pending.previous_current_version.as_deref() {
        update_current_symlink(previous)?;
        update_launcher_symlink_to_current()?;
    }
    manifest.canary_status = Some(CanaryStatus::Failed);
    manifest.pending_activation = None;
    manifest.save()?;
    Ok(Some(pending.new_version))
}

/// Which binary to use
#[derive(Debug, Clone)]
pub enum BinaryChoice {
    /// Use the stable version
    Stable(String),
    /// Use the canary version (for testing)
    Canary(String),
    /// Use current running binary (no versioned builds yet)
    Current,
}

/// Install a binary at a specific immutable version path.
pub fn install_binary_at_version(source: &std::path::Path, version: &str) -> Result<PathBuf> {
    if !source.exists() {
        anyhow::bail!("Binary not found at {:?}", source);
    }

    let dest_dir = builds_dir()?.join("versions").join(version);
    storage::ensure_dir(&dest_dir)?;

    let dest = dest_dir.join(binary_name());

    // Remove existing file first to avoid ETXTBSY when replacing a running binary.
    if dest.exists() {
        std::fs::remove_file(&dest)?;
    }

    // Prefer hard link (instant, zero I/O) over copy (71MB+ binary).
    // Falls back to copy if hard link fails (e.g. cross-filesystem).
    if std::fs::hard_link(source, &dest).is_err() {
        std::fs::copy(source, &dest)?;
    }
    crate::platform::set_permissions_executable(&dest)?;

    Ok(dest)
}

pub fn smoke_test_binary(binary: &Path) -> Result<()> {
    let output = Command::new(binary)
        .args(["version", "--json"])
        .env("JCODE_NON_INTERACTIVE", "1")
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Binary smoke test failed for {} with exit code {:?}: {}",
            binary.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|err| {
        anyhow::anyhow!(
            "Binary smoke test for {} returned invalid JSON: {}",
            binary.display(),
            err
        )
    })?;
    if value.get("version").is_none() {
        anyhow::bail!(
            "Binary smoke test for {} returned JSON without a version field",
            binary.display()
        );
    }
    Ok(())
}

fn update_channel_symlink(channel: &str, version: &str) -> Result<PathBuf> {
    let channel_dir = builds_dir()?.join(channel);
    storage::ensure_dir(&channel_dir)?;

    let link_path = channel_dir.join(binary_name());
    let target = version_binary_path(version)?;
    if !target.exists() {
        anyhow::bail!("Version binary not found at {:?}", target);
    }

    let temp = channel_dir.join(format!(
        ".{}-{}-{}",
        binary_stem(),
        channel,
        std::process::id()
    ));
    crate::platform::atomic_symlink_swap(&target, &link_path, &temp)?;

    Ok(link_path)
}

/// Update stable symlink to point to a version and publish stable-version marker.
pub fn update_stable_symlink(version: &str) -> Result<PathBuf> {
    let stable_link = update_channel_symlink("stable", version)?;
    std::fs::write(stable_version_file()?, version)?;
    Ok(stable_link)
}

/// Update current symlink to point to a version and publish current-version marker.
pub fn update_current_symlink(version: &str) -> Result<PathBuf> {
    let current_link = update_channel_symlink("current", version)?;
    std::fs::write(current_version_file()?, version)?;
    Ok(current_link)
}

pub fn publish_local_current_build_for_source(
    repo_dir: &Path,
    source: &SourceState,
) -> Result<PublishedBuild> {
    let binary = find_dev_binary(repo_dir)
        .ok_or_else(|| anyhow::anyhow!("Binary not found in target/selfdev or target/release"))?;
    if !binary.exists() {
        anyhow::bail!("Binary not found at {:?}", binary);
    }

    smoke_test_binary(&binary)?;
    let previous_current_version = read_current_version()?;
    let versioned_path = install_binary_at_version(&binary, &source.version_label)?;
    smoke_test_binary(&versioned_path)?;
    let current_link = update_current_symlink(&source.version_label)?;
    let launcher_link = update_launcher_symlink_to_current()?;

    Ok(PublishedBuild {
        version: source.version_label.clone(),
        source_fingerprint: source.fingerprint.clone(),
        versioned_path,
        current_link,
        launcher_link,
        previous_current_version,
    })
}

/// Install the local release binary into immutable versions and make it the active `current`
/// build + launcher, while keeping `stable` untouched.
pub fn publish_local_current_build(repo_dir: &std::path::Path) -> Result<PathBuf> {
    let source = current_source_state(repo_dir)?;
    Ok(publish_local_current_build_for_source(repo_dir, &source)?.versioned_path)
}

/// Install release binary into immutable versions, promote it to stable, and also make it the
/// active current/launcher build.
pub fn install_local_release(repo_dir: &std::path::Path) -> Result<PathBuf> {
    let source = release_binary_path(repo_dir);
    if !source.exists() {
        anyhow::bail!("Binary not found at {:?}", source);
    }

    let version = repo_build_version(repo_dir)?;

    let versioned = install_binary_at_version(&source, &version)?;
    update_stable_symlink(&version)?;
    update_current_symlink(&version)?;
    update_launcher_symlink_to_current()?;

    Ok(versioned)
}

/// Copy binary to versioned location
pub fn install_version(repo_dir: &std::path::Path, hash: &str) -> Result<PathBuf> {
    let source = release_binary_path(repo_dir);
    install_binary_at_version(&source, hash)
}

/// Update canary symlink to point to a version
pub fn update_canary_symlink(hash: &str) -> Result<()> {
    let _ = update_channel_symlink("canary", hash)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_jcode_home<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::storage::lock_test_env();
        let temp_home = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp_home.path());
        let result = f();
        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
        result
    }

    fn create_git_repo_fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path().join(".git")).expect("create .git dir");
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"jcode\"\nversion = \"0.0.0\"\n",
        )
        .expect("write Cargo.toml");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(temp.path())
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(temp.path())
            .output()
            .expect("git config email");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(temp.path())
            .output()
            .expect("git config name");
        std::process::Command::new("git")
            .args(["add", "Cargo.toml"])
            .current_dir(temp.path())
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(temp.path())
            .output()
            .expect("git commit");
        temp
    }

    #[test]
    fn test_build_manifest_default() {
        let manifest = BuildManifest::default();
        assert!(manifest.stable.is_none());
        assert!(manifest.canary.is_none());
        assert!(manifest.history.is_empty());
    }

    #[test]
    fn test_binary_choice_for_canary_session() {
        let manifest = BuildManifest {
            canary: Some("abc123".to_string()),
            canary_session: Some("session_test".to_string()),
            ..Default::default()
        };

        // Canary session should get canary binary
        match manifest.binary_for_session("session_test") {
            BinaryChoice::Canary(hash) => assert_eq!(hash, "abc123"),
            _ => panic!("Expected canary binary"),
        }

        // Other sessions should get stable (or current if no stable)
        match manifest.binary_for_session("other_session") {
            BinaryChoice::Current => {}
            _ => panic!("Expected current binary"),
        }
    }

    #[test]
    fn test_find_repo_in_ancestors_walks_upward() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("jcode-repo");
        let nested = repo.join("a").join("b").join("c");

        std::fs::create_dir_all(repo.join(".git")).expect("create .git");
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"jcode\"\nversion = \"0.0.0\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir_all(&nested).expect("create nested dirs");

        let found = find_repo_in_ancestors(&nested).expect("repo should be found");
        assert_eq!(found, repo);
    }

    #[test]
    fn test_client_update_candidate_prefers_dev_binary_for_selfdev() {
        let _guard = crate::storage::lock_test_env();
        let temp_home = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp_home.path());

        let version = "test-current";
        let version_binary =
            install_binary_at_version(std::env::current_exe().as_ref().unwrap(), version)
                .expect("install test version");
        update_current_symlink(version).expect("update current symlink");

        let candidate = client_update_candidate(true).expect("expected selfdev candidate");
        assert_eq!(candidate.1, "current");
        assert_eq!(
            std::fs::canonicalize(candidate.0).expect("canonical candidate"),
            std::fs::canonicalize(version_binary).expect("canonical version binary")
        );

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    #[test]
    fn launcher_dir_uses_sandbox_bin_when_jcode_home_is_set() {
        with_temp_jcode_home(|| {
            let launcher_dir = launcher_dir().expect("launcher dir");
            let expected = storage::jcode_dir().expect("jcode dir").join("bin");
            assert_eq!(launcher_dir, expected);
        });
    }

    #[test]
    fn update_launcher_symlink_stays_inside_sandbox_home() {
        with_temp_jcode_home(|| {
            let version = "sandbox-current";
            let version_binary =
                install_binary_at_version(std::env::current_exe().as_ref().unwrap(), version)
                    .expect("install test version");
            update_current_symlink(version).expect("update current symlink");

            let launcher = update_launcher_symlink_to_current().expect("update launcher");
            let expected_launcher = storage::jcode_dir()
                .expect("jcode dir")
                .join("bin")
                .join(binary_name());
            assert_eq!(launcher, expected_launcher);
            assert_eq!(
                std::fs::canonicalize(&launcher).expect("canonical launcher"),
                std::fs::canonicalize(version_binary).expect("canonical version binary")
            );
        });
    }

    #[test]
    fn test_canary_status_serialization() {
        assert_eq!(
            serde_json::to_string(&CanaryStatus::Testing).unwrap(),
            "\"testing\""
        );
        assert_eq!(
            serde_json::to_string(&CanaryStatus::Passed).unwrap(),
            "\"passed\""
        );
    }

    #[test]
    fn dirty_source_state_uses_fingerprint_in_version_label() {
        let repo = create_git_repo_fixture();
        std::fs::write(repo.path().join("notes.txt"), "dirty change\n").expect("write dirty file");

        let state = current_source_state(repo.path()).expect("source state");
        assert!(state.dirty);
        assert!(
            state
                .version_label
                .starts_with(&format!("{}-dirty-", state.short_hash))
        );
        assert!(state.version_label.len() > state.short_hash.len() + 7);
    }

    #[test]
    fn pending_activation_can_complete_and_roll_back() {
        with_temp_jcode_home(|| {
            let current_version = "stable-prev";
            install_binary_at_version(std::env::current_exe().as_ref().unwrap(), current_version)
                .expect("install previous version");
            update_current_symlink(current_version).expect("publish previous current");

            let mut manifest = BuildManifest::default();
            manifest
                .set_pending_activation(PendingActivation {
                    session_id: "session-a".to_string(),
                    new_version: "canary-next".to_string(),
                    previous_current_version: Some(current_version.to_string()),
                    source_fingerprint: Some("fingerprint-a".to_string()),
                    requested_at: Utc::now(),
                })
                .expect("set pending activation");

            let completed = complete_pending_activation_for_session("session-a")
                .expect("complete activation")
                .expect("completed version");
            assert_eq!(completed, "canary-next");
            let manifest = BuildManifest::load().expect("load manifest");
            assert!(manifest.pending_activation.is_none());
            assert_eq!(manifest.canary.as_deref(), Some("canary-next"));
            assert_eq!(manifest.canary_status, Some(CanaryStatus::Passed));

            let mut manifest = BuildManifest::load().expect("reload manifest");
            manifest
                .set_pending_activation(PendingActivation {
                    session_id: "session-b".to_string(),
                    new_version: "canary-bad".to_string(),
                    previous_current_version: Some(current_version.to_string()),
                    source_fingerprint: Some("fingerprint-b".to_string()),
                    requested_at: Utc::now(),
                })
                .expect("set second pending activation");

            let rolled_back = rollback_pending_activation_for_session("session-b")
                .expect("rollback activation")
                .expect("rolled back version");
            assert_eq!(rolled_back, "canary-bad");
            let restored = read_current_version()
                .expect("read current version")
                .expect("restored current version");
            assert_eq!(restored, current_version);
        });
    }
}
