use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use tokio::time::{Duration, timeout};

const CLIENT_TITLE_PREFIXES: &[&str] = &["jcode:d:", "jcode:c:"];

#[derive(Debug, Clone)]
pub struct DictationRun {
    pub text: String,
    pub mode: crate::protocol::TranscriptMode,
}

pub async fn run_configured() -> Result<DictationRun> {
    let cfg = crate::config::config().dictation.clone();
    let command = cfg.command.trim();
    if command.is_empty() {
        anyhow::bail!(
            "Dictation is not configured. Set `[dictation].command` in `~/.jcode/config.toml`."
        );
    }

    let text = run_command(command, cfg.timeout_secs).await?;
    Ok(DictationRun {
        text,
        mode: cfg.mode,
    })
}

pub async fn run_command(command: &str, timeout_secs: u64) -> Result<String> {
    let mut child = shell_command(command);
    child.stdout(Stdio::piped()).stderr(Stdio::piped());

    let child = child
        .spawn()
        .with_context(|| format!("failed to start `{}`", command))?;

    let output = if timeout_secs == 0 {
        child
            .wait_with_output()
            .await
            .context("failed to wait for dictation command")?
    } else {
        timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
            .await
            .with_context(|| format!("dictation command timed out after {}s", timeout_secs))?
            .context("failed to wait for dictation command")?
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            anyhow::bail!("dictation command exited with {}", output.status);
        }
        anyhow::bail!(stderr);
    }

    let transcript = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\r', '\n'])
        .trim()
        .to_string();
    if transcript.is_empty() {
        anyhow::bail!("dictation command returned an empty transcript");
    }

    Ok(transcript)
}

fn last_focused_session_write_cache() -> &'static Mutex<Option<String>> {
    static CACHE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

pub fn remember_last_focused_session(session_id: &str) -> Result<()> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Ok(());
    }

    if let Ok(cache) = last_focused_session_write_cache().lock()
        && cache.as_deref() == Some(session_id)
    {
        return Ok(());
    }

    let path = last_focused_session_path()?;
    if let Some(parent) = path.parent() {
        crate::storage::ensure_dir(parent)?;
    }
    std::fs::write(&path, session_id).context("failed to persist last focused jcode session")?;

    if let Ok(mut cache) = last_focused_session_write_cache().lock() {
        *cache = Some(session_id.to_string());
    }

    Ok(())
}

pub fn last_focused_session() -> Result<Option<String>> {
    let path = last_focused_session_path()?;
    let session_id = match std::fs::read_to_string(path) {
        Ok(text) => text.trim().to_string(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("failed to read last focused jcode session"),
    };
    if session_id.is_empty() {
        return Ok(None);
    }

    if crate::session::active_session_ids()
        .iter()
        .any(|id| id == &session_id)
    {
        Ok(Some(session_id))
    } else {
        Ok(None)
    }
}

pub fn type_text(text: &str) -> Result<()> {
    let status = Command::new("wtype")
        .arg("--")
        .arg(text)
        .status()
        .context("failed to launch `wtype`")?;
    if !status.success() {
        anyhow::bail!("`wtype` exited with {}", status);
    }
    Ok(())
}

pub fn focused_jcode_session() -> Result<Option<String>> {
    let Some(window) = focused_window_niri()? else {
        return Ok(None);
    };
    Ok(resolve_session_for_window(&window))
}

#[derive(Debug, Deserialize)]
struct NiriFocusedWindow {
    pid: u32,
    title: Option<String>,
    #[serde(rename = "app_id")]
    _app_id: Option<String>,
}

fn focused_window_niri() -> Result<Option<NiriFocusedWindow>> {
    let output = Command::new("niri")
        .args(["msg", "-j", "focused-window"])
        .output();

    let output = match output {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(None);
    }

    let window: NiriFocusedWindow =
        serde_json::from_str(trimmed).context("failed to parse `niri msg -j focused-window`")?;
    Ok(Some(window))
}

fn resolve_session_for_window(window: &NiriFocusedWindow) -> Option<String> {
    if let Some(title) = window.title.as_deref()
        && let Some(session_id) = resolve_session_from_window_title(title)
    {
        return Some(session_id);
    }

    let children = proc_children_map().ok()?;
    let mut queue = VecDeque::from([window.pid]);
    let mut candidates = Vec::new();

    while let Some(pid) = queue.pop_front() {
        if let Some(candidate) = inspect_client_process(pid) {
            candidates.push(candidate);
        }
        if let Some(next) = children.get(&pid) {
            queue.extend(next.iter().copied());
        }
    }

    if candidates.is_empty() {
        return None;
    }

    let selected = select_candidate(&candidates, window.title.as_deref())?;
    resolve_candidate_session_id(&selected)
}

fn resolve_session_from_window_title(title: &str) -> Option<String> {
    let short_name = extract_session_short_name_from_window_title(title)?;
    let mut matching: Vec<String> = crate::session::active_session_ids()
        .into_iter()
        .filter(|session_id| {
            crate::id::extract_session_name(session_id)
                .map(|name| name.eq_ignore_ascii_case(&short_name))
                .unwrap_or(false)
        })
        .collect();
    matching.sort();
    matching.pop()
}

fn extract_session_short_name_from_window_title(title: &str) -> Option<String> {
    let (_, rest) = title
        .split_once("jcode/")
        .or_else(|| title.split_once("jcode "))?;
    let candidate = rest.split('[').next().unwrap_or(rest).trim();
    let token = candidate.split_whitespace().next_back()?;
    normalize_session_short_name(token)
}

fn normalize_session_short_name(token: &str) -> Option<String> {
    let normalized = token
        .trim()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .to_ascii_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientCandidate {
    pid: u32,
    short_name: String,
    session_id: Option<String>,
}

fn inspect_client_process(pid: u32) -> Option<ClientCandidate> {
    if let Some(session_id) = read_resumed_session_id(pid) {
        let short_name = crate::id::extract_session_name(&session_id)
            .unwrap_or(session_id.as_str())
            .to_string();
        return Some(ClientCandidate {
            pid,
            short_name,
            session_id: Some(session_id),
        });
    }

    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    let short_name = CLIENT_TITLE_PREFIXES
        .iter()
        .find_map(|prefix| comm.strip_prefix(prefix))?
        .trim()
        .to_string();
    if short_name.is_empty() {
        return None;
    }

    Some(ClientCandidate {
        pid,
        short_name,
        session_id: read_resumed_session_id(pid),
    })
}

fn read_resumed_session_id(pid: u32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect();
    for pair in args.windows(2) {
        if pair[0] == "--resume" && pair[1].starts_with("session_") {
            return Some(pair[1].clone());
        }
    }
    None
}

fn select_candidate(
    candidates: &[ClientCandidate],
    title: Option<&str>,
) -> Option<ClientCandidate> {
    if candidates.len() == 1 {
        return candidates.first().cloned();
    }

    let title = title?.to_ascii_lowercase();
    candidates
        .iter()
        .find(|candidate| title.contains(&candidate.short_name.to_ascii_lowercase()))
        .cloned()
        .or_else(|| candidates.first().cloned())
}

fn resolve_candidate_session_id(candidate: &ClientCandidate) -> Option<String> {
    if let Some(session_id) = &candidate.session_id {
        return Some(session_id.clone());
    }

    let mut matching: Vec<String> = crate::session::active_session_ids()
        .into_iter()
        .filter(|session_id| {
            crate::id::extract_session_name(session_id)
                .map(|name| name.eq_ignore_ascii_case(&candidate.short_name))
                .unwrap_or(false)
        })
        .collect();

    matching.sort();
    matching.pop()
}

fn proc_children_map() -> Result<HashMap<u32, Vec<u32>>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let proc_dir = std::fs::read_dir("/proc").context("failed to read /proc")?;

    for entry in proc_dir {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };

        let status_path = entry.path().join("status");
        let Ok(status) = std::fs::read_to_string(status_path) else {
            continue;
        };
        let Some(ppid) = parse_ppid(&status) else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
    }

    Ok(children)
}

fn parse_ppid(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        let value = line.strip_prefix("PPid:")?;
        value.trim().parse::<u32>().ok()
    })
}

fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    }

    #[cfg(not(windows))]
    {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-lc").arg(command);
        cmd
    }
}

fn last_focused_session_path() -> Result<std::path::PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("last_focused_client_session"))
}

#[cfg(test)]
mod tests {
    use super::{
        ClientCandidate, extract_session_short_name_from_window_title, focused_jcode_session,
        last_focused_session, normalize_session_short_name, parse_ppid, read_resumed_session_id,
        remember_last_focused_session, run_command, select_candidate,
    };
    use std::ffi::OsString;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set<K: AsRef<std::ffi::OsStr>>(key: &'static str, value: K) -> Self {
            let previous = std::env::var_os(key);
            crate::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                crate::env::set_var(self.key, previous);
            } else {
                crate::env::remove_var(self.key);
            }
        }
    }

    #[cfg(target_os = "linux")]
    struct ChildGuard(std::process::Child);

    #[cfg(target_os = "linux")]
    impl ChildGuard {
        fn spawn_named(name: &str) -> Self {
            let child = std::process::Command::new("python3")
                .args([
                    "-c",
                    "import ctypes, sys, time; libc = ctypes.CDLL(None); libc.prctl(15, sys.argv[1].encode(), 0, 0, 0); time.sleep(30)",
                    name,
                ])
                .spawn()
                .expect("spawn named helper process");
            Self(child)
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(target_os = "linux")]
    fn install_fake_niri(bin_dir: &std::path::Path, pid: u32, title: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(bin_dir).expect("create fake bin dir");
        let script = bin_dir.join("niri");
        let json = serde_json::json!({
            "pid": pid,
            "title": title,
            "app_id": "kitty"
        });
        std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", json))
            .expect("write fake niri script");
        let mut perms = std::fs::metadata(&script)
            .expect("fake niri metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod fake niri");
    }

    #[test]
    fn parse_ppid_from_proc_status() {
        let status = "Name:\tbash\nState:\tS (sleeping)\nPPid:\t1234\n";
        assert_eq!(parse_ppid(status), Some(1234));
    }

    #[tokio::test]
    async fn run_command_trims_trailing_newlines() {
        let text = run_command("printf 'hello from test\\n'", 5)
            .await
            .expect("dictation command should succeed");
        assert_eq!(text, "hello from test");
    }

    #[test]
    fn select_candidate_prefers_title_match() {
        let candidates = vec![
            ClientCandidate {
                pid: 1,
                short_name: "whale".to_string(),
                session_id: Some("session_whale_1".to_string()),
            },
            ClientCandidate {
                pid: 2,
                short_name: "crab".to_string(),
                session_id: Some("session_crab_1".to_string()),
            },
        ];

        let selected = select_candidate(&candidates, Some("🦀 jcode/sleeping Crab [self-dev]"))
            .expect("should select matching candidate");
        assert_eq!(selected.short_name, "crab");
    }

    #[test]
    fn read_resumed_session_id_from_cmdline_for_current_process() {
        let _ = read_resumed_session_id(std::process::id());
    }

    #[test]
    fn extract_session_short_name_from_jcode_window_title() {
        assert_eq!(
            extract_session_short_name_from_window_title("🦢 jcode/cliff Swan [self-dev]"),
            Some("swan".to_string())
        );
        assert_eq!(
            extract_session_short_name_from_window_title("🦊 jcode Fox"),
            Some("fox".to_string())
        );
    }

    #[test]
    fn normalize_session_short_name_strips_wrapping_punctuation() {
        assert_eq!(
            normalize_session_short_name("[Swan]"),
            Some("swan".to_string())
        );
        assert_eq!(
            normalize_session_short_name("Polar-bear"),
            Some("polar-bear".to_string())
        );
    }

    #[test]
    fn remember_and_read_last_focused_session() {
        let _guard = crate::storage::lock_test_env();
        let prev = std::env::var_os("JCODE_HOME");
        let temp = tempfile::TempDir::new().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());

        let active_dir = temp.path().join("active_pids");
        std::fs::create_dir_all(&active_dir).expect("create active_pids");
        std::fs::write(active_dir.join("session_whale_123"), "99999").expect("write active pid");

        remember_last_focused_session("session_whale_123").expect("remember session");
        assert_eq!(
            last_focused_session().expect("read session"),
            Some("session_whale_123".to_string())
        );

        if let Some(prev) = prev {
            crate::env::set_var("JCODE_HOME", prev);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn focused_jcode_session_uses_niri_window_title_when_process_name_is_generic() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let _home = EnvVarGuard::set("JCODE_HOME", temp.path());

        let active_dir = temp.path().join("active_pids");
        std::fs::create_dir_all(&active_dir).expect("create active_pids");
        std::fs::write(active_dir.join("session_swan_123"), "12345").expect("write active pid");

        let focused_process = ChildGuard::spawn_named("jcode");
        let bin_dir = temp.path().join("bin");
        install_fake_niri(
            &bin_dir,
            focused_process.pid(),
            "🦢 jcode/cliff Swan [self-dev]",
        );

        let prev_path = std::env::var_os("PATH").unwrap_or_default();
        let mut path = OsString::from(bin_dir.as_os_str());
        path.push(":");
        path.push(prev_path);
        let _path = EnvVarGuard::set("PATH", path);

        assert_eq!(
            focused_jcode_session().expect("resolve focused session"),
            Some("session_swan_123".to_string())
        );
    }
}
