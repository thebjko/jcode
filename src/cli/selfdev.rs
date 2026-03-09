use anyhow::Result;
use std::process::Command as ProcessCommand;

use crate::{build, id, logging, server, session, startup_profile, tui};

use super::terminal::{
    cleanup_tui_runtime, init_tui_runtime, set_current_session, spawn_session_signal_watchers,
};
use super::tui_launch::hot_rebuild;
use super::tui_launch::hot_reload;
use super::tui_launch::hot_update;

pub const EXIT_RELOAD_REQUESTED: i32 = 42;

pub const SELFDEV_SOCKET: &str = "/tmp/jcode-selfdev.sock";

pub async fn run_self_dev(should_build: bool, resume_session: Option<String>) -> Result<()> {
    startup_profile::mark("run_self_dev_enter");
    std::env::set_var("JCODE_SELFDEV_MODE", "1");

    let repo_dir =
        build::get_repo_dir().ok_or_else(|| anyhow::anyhow!("Could not find jcode repository"))?;

    startup_profile::mark("selfdev_session_create");
    let is_resume = resume_session.is_some();
    let session_id = if let Some(id) = resume_session {
        if let Ok(mut session) = session::Session::load(&id) {
            if !session.is_canary {
                session.set_canary("self-dev");
                let _ = session.save();
            }
        }
        id
    } else {
        let mut session =
            session::Session::create(None, Some("Self-development session".to_string()));
        session.set_canary("self-dev");
        session.id.clone()
    };

    let target_binary =
        build::find_dev_binary(&repo_dir).unwrap_or_else(|| build::release_binary_path(&repo_dir));

    if should_build {
        eprintln!("Building...");

        let build_status = ProcessCommand::new("cargo")
            .args(["build", "--release"])
            .current_dir(&repo_dir)
            .status()?;

        if !build_status.success() {
            anyhow::bail!("Build failed");
        }

        eprintln!("✓ Build complete");
    }

    if !target_binary.exists() {
        anyhow::bail!(
            "No binary found at {:?}\n\
             Run 'cargo build --release' first, or use 'jcode self-dev --build'.",
            target_binary
        );
    }

    let hash = build::current_git_hash(&repo_dir)?;
    startup_profile::mark("selfdev_git_hash");
    let binary_path = target_binary.clone();

    if !is_resume {
        eprintln!("Starting self-dev session with {}...", hash);
    } else {
        logging::info(&format!("Resuming self-dev session with {}...", hash));
    }

    let exe = std::env::current_exe()?;
    let cwd = std::env::current_dir()?;

    if is_resume {
        std::env::set_var("JCODE_RESUMING", "1");
    }

    let err = crate::platform::replace_process(
        ProcessCommand::new(&exe)
            .arg("canary-wrapper")
            .arg(&session_id)
            .arg(binary_path.to_string_lossy().as_ref())
            .arg(&hash)
            .current_dir(cwd),
    );

    Err(anyhow::anyhow!("Failed to exec wrapper {:?}: {}", exe, err))
}

pub async fn is_server_alive(socket_path: &str) -> bool {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    if !crate::transport::is_socket_path(std::path::Path::new(socket_path)) {
        return false;
    }

    let stream = match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::transport::Stream::connect(socket_path),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => return false,
    };

    let (reader, mut writer) = stream.into_split();
    let ping = "{\"type\":\"ping\",\"id\":0}\n";
    if writer.write_all(ping.as_bytes()).await.is_err() {
        return false;
    }

    let mut buf_reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        buf_reader.read_line(&mut line),
    )
    .await
    {
        Ok(Ok(n)) if n > 0 => true,
        _ => false,
    }
}

pub async fn run_canary_wrapper(
    session_id: &str,
    initial_binary: &str,
    current_hash: &str,
) -> Result<()> {
    let initial_binary_path = std::path::PathBuf::from(initial_binary);
    let socket_path = SELFDEV_SOCKET.to_string();

    server::set_socket_path(&socket_path);
    startup_profile::mark("canary_wrapper_enter");

    let is_resuming = std::env::var("JCODE_RESUMING").is_ok();
    macro_rules! startup_msg {
        ($($arg:tt)*) => {
            if is_resuming {
                crate::logging::info(&format!($($arg)*));
            } else {
                eprintln!($($arg)*);
            }
        };
    }

    let server_alive = is_server_alive(&socket_path).await;
    startup_profile::mark("canary_server_alive_check");

    let mut server_just_spawned = false;
    if !server_alive {
        startup_msg!("Starting self-dev server...");

        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(format!("{}.hash", socket_path));
        let _ = std::fs::remove_file(server::debug_socket_path());

        let binary_path = if initial_binary_path.exists() {
            initial_binary_path.clone()
        } else {
            let canary_path = build::canary_binary_path().ok();
            let stable_path = build::stable_binary_path().ok();
            if canary_path.as_ref().map(|p| p.exists()).unwrap_or(false) {
                canary_path.unwrap()
            } else if stable_path.as_ref().map(|p| p.exists()).unwrap_or(false) {
                stable_path.unwrap()
            } else {
                anyhow::bail!("No binary found for server!");
            }
        };

        let cwd = std::env::current_dir().unwrap_or_default();
        let mut cmd = std::process::Command::new(&binary_path);
        cmd.arg("serve")
            .current_dir(&cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());

        #[cfg(unix)]
        {
            let _child = server::spawn_server_notify(&mut cmd).await?;
        }
        #[cfg(not(unix))]
        {
            cmd.spawn()?;
        }

        startup_profile::mark("canary_server_spawned");
        server_just_spawned = true;
        startup_msg!("Server spawned, starting TUI...");
    } else {
        let hash_path = format!("{}.hash", socket_path);
        let server_hash = std::fs::read_to_string(&hash_path).unwrap_or_default();

        let server_ver = if server_hash.is_empty() {
            "unknown version"
        } else {
            server_hash.trim()
        };

        if !server_hash.is_empty() && server_hash.trim() != current_hash {
            startup_msg!(
                "Connecting to existing self-dev server ({}) on {} (client built from {})",
                server_ver,
                socket_path,
                current_hash
            );
        } else {
            startup_msg!(
                "Connecting to existing self-dev server ({}) on {}...",
                server_ver,
                socket_path
            );
        }
    }
    startup_profile::mark("canary_server_resolved");

    let session_name = id::extract_session_name(session_id)
        .map(|s| s.to_string())
        .unwrap_or_else(|| session_id.to_string());

    startup_msg!("Starting TUI client...");
    startup_profile::mark("canary_tui_start");
    set_current_session(session_id);
    spawn_session_signal_watchers();

    let (terminal, tui_runtime) = init_tui_runtime()?;
    startup_profile::mark("canary_terminal_init");
    startup_profile::mark("canary_mermaid_picker");
    startup_profile::mark("canary_config_load");
    startup_profile::mark("canary_keyboard_enhance");

    let mut app = tui::App::new_for_remote(Some(session_id.to_string()));
    if server_just_spawned {
        app.set_server_spawning();
    }
    startup_profile::mark("canary_app_new");

    let icon = id::session_icon(&session_name);
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::SetTitle(format!("{} jcode {} [self-dev]", icon, session_name))
    );

    startup_profile::mark("canary_pre_run_remote");
    startup_profile::report_to_log();

    let result = app.run_remote(terminal).await;

    let run_result = match result {
        Ok(r) => r,
        Err(e) => {
            cleanup_tui_runtime(&tui_runtime, true);
            return Err(e);
        }
    };

    let will_exec = run_result.reload_session.is_some()
        || run_result.rebuild_session.is_some()
        || run_result.update_session.is_some()
        || run_result.exit_code == Some(EXIT_RELOAD_REQUESTED);

    if !will_exec {
        cleanup_tui_runtime(&tui_runtime, true);
    } else {
        cleanup_tui_runtime(&tui_runtime, false);
    }

    if let Some(ref reload_session_id) = run_result.reload_session {
        hot_reload(reload_session_id)?;
    }

    if let Some(ref rebuild_session_id) = run_result.rebuild_session {
        hot_rebuild(rebuild_session_id)?;
    }

    if let Some(ref update_session_id) = run_result.update_session {
        hot_update(update_session_id)?;
    }

    if let Some(code) = run_result.exit_code {
        if code == EXIT_RELOAD_REQUESTED {
            let binary_path = build::canary_binary_path().ok();

            let binary = binary_path
                .filter(|p| p.exists())
                .or_else(|| {
                    initial_binary_path
                        .exists()
                        .then(|| initial_binary_path.clone())
                })
                .ok_or_else(|| anyhow::anyhow!("No binary found for reload"))?;

            let cwd = std::env::current_dir()?;

            std::env::set_var("JCODE_RESUMING", "1");

            let err = crate::platform::replace_process(
                ProcessCommand::new(&binary)
                    .arg("self-dev")
                    .arg("--resume")
                    .arg(session_id)
                    .current_dir(cwd),
            );

            return Err(anyhow::anyhow!("Failed to exec {:?}: {}", binary, err));
        }
    }

    eprintln!();
    eprintln!(
        "\x1b[33mSession \x1b[1m{}\x1b[0m\x1b[33m - to resume:\x1b[0m",
        session_name
    );
    eprintln!("  jcode --resume {}", session_id);
    eprintln!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{provider, session, storage, tool};
    use std::ffi::OsString;
    use std::sync::Arc;

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        storage::lock_test_env()
    }

    struct TestEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<OsString>,
        prev_test_session: Option<OsString>,
        _temp_home: tempfile::TempDir,
    }

    impl TestEnvGuard {
        fn new() -> anyhow::Result<Self> {
            let lock = lock_env();
            let temp_home = tempfile::Builder::new()
                .prefix("jcode-selfdev-test-home-")
                .tempdir()?;
            let prev_home = std::env::var_os("JCODE_HOME");
            let prev_test_session = std::env::var_os("JCODE_TEST_SESSION");

            std::env::set_var("JCODE_HOME", temp_home.path());
            std::env::set_var("JCODE_TEST_SESSION", "1");

            Ok(Self {
                _lock: lock,
                prev_home,
                prev_test_session,
                _temp_home: temp_home,
            })
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            if let Some(prev_home) = &self.prev_home {
                std::env::set_var("JCODE_HOME", prev_home);
            } else {
                std::env::remove_var("JCODE_HOME");
            }

            if let Some(prev_test_session) = &self.prev_test_session {
                std::env::set_var("JCODE_TEST_SESSION", prev_test_session);
            } else {
                std::env::remove_var("JCODE_TEST_SESSION");
            }
        }
    }

    fn setup_test_env() -> TestEnvGuard {
        TestEnvGuard::new().expect("failed to setup isolated test environment")
    }

    struct TestProvider;

    #[async_trait::async_trait]
    impl provider::Provider for TestProvider {
        fn name(&self) -> &str {
            "test"
        }

        fn model(&self) -> String {
            "test".to_string()
        }

        fn available_models(&self) -> Vec<&'static str> {
            vec![]
        }

        fn available_models_display(&self) -> Vec<String> {
            vec![]
        }

        async fn prefetch_models(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_model(&self, _model: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn handles_tools_internally(&self) -> bool {
            false
        }

        async fn complete(
            &self,
            _messages: &[crate::message::Message],
            _tools: &[crate::message::ToolDefinition],
            _system: &str,
            _session_id: Option<&str>,
        ) -> anyhow::Result<crate::provider::EventStream> {
            unimplemented!()
        }

        fn fork(&self) -> Arc<dyn provider::Provider> {
            Arc::new(TestProvider)
        }
    }

    #[tokio::test]
    async fn test_selfdev_tool_registration() {
        let _env = setup_test_env();

        let mut session = session::Session::create(None, Some("Test".to_string()));
        session.set_canary("test");
        assert!(session.is_canary, "Session should be marked as canary");

        let provider = Arc::new(TestProvider) as Arc<dyn provider::Provider>;
        let registry = tool::Registry::new(provider).await;

        let tools_before: Vec<String> = registry.tool_names().await;
        let has_selfdev_before = tools_before.contains(&"selfdev".to_string());

        registry.register_selfdev_tools().await;

        let tools_after: Vec<String> = registry.tool_names().await;
        let has_selfdev_after = tools_after.contains(&"selfdev".to_string());

        println!(
            "Before: selfdev={}, tools={:?}",
            has_selfdev_before,
            tools_before.len()
        );
        println!(
            "After: selfdev={}, tools={:?}",
            has_selfdev_after,
            tools_after.len()
        );

        assert!(has_selfdev_after, "selfdev should be registered");
    }

    #[tokio::test]
    async fn test_selfdev_session_and_registry() {
        let _env = setup_test_env();

        let mut session = session::Session::create(None, Some("Test E2E".to_string()));
        session.set_canary("test-build");
        let session_id = session.id.clone();
        session.save().expect("Failed to save session");

        let loaded = session::Session::load(&session_id).expect("Failed to load session");
        assert!(loaded.is_canary, "Loaded session should be canary");

        let provider = Arc::new(TestProvider) as Arc<dyn provider::Provider>;
        let registry = tool::Registry::new(provider.clone()).await;

        let tools_before = registry.tool_names().await;
        assert!(
            !tools_before.contains(&"selfdev".to_string()),
            "selfdev should NOT be registered initially"
        );

        registry.register_selfdev_tools().await;

        let tools_after = registry.tool_names().await;
        assert!(
            tools_after.contains(&"selfdev".to_string()),
            "selfdev SHOULD be registered after register_selfdev_tools"
        );

        let ctx = tool::ToolContext {
            session_id: session_id.clone(),
            message_id: "test".to_string(),
            tool_call_id: "test".to_string(),
            working_dir: None,
            stdin_request_tx: None,
        };
        let result = registry
            .execute("selfdev", serde_json::json!({"action": "status"}), ctx)
            .await;

        println!("selfdev status result: {:?}", result);
        assert!(result.is_ok(), "selfdev tool should execute successfully");

        let _ = std::fs::remove_file(
            storage::jcode_dir()
                .unwrap()
                .join("sessions")
                .join(format!("{}.json", session_id)),
        );
    }
}
