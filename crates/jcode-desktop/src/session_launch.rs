use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(50);

pub fn launch_resume_session(session_id: &str, title: &str) -> Result<()> {
    let title = format!("jcode · {}", compact_title(title));
    let candidates = terminal_candidates(&title, &["--resume", session_id]);
    launch_first_available_terminal(candidates, &format!("jcode --resume {session_id}"))
}

pub fn launch_new_session() -> Result<()> {
    let candidates = terminal_candidates("jcode · new session", &["--fresh-spawn"]);
    launch_first_available_terminal(candidates, "jcode")
}

pub fn send_message_to_session(session_id: &str, _title: &str, message: &str) -> Result<()> {
    validate_resume_session_id(session_id).context("refusing to send to invalid session id")?;
    if message.trim().is_empty() {
        anyhow::bail!("empty draft message");
    }

    Command::new(jcode_bin())
        .arg("--resume")
        .arg(session_id)
        .arg("run")
        .arg(message)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn jcode run for {session_id}"))?;

    Ok(())
}

#[cfg(unix)]
pub fn start_fresh_server_session(message: &str) -> Result<String> {
    if message.trim().is_empty() {
        anyhow::bail!("empty draft message");
    }

    ensure_server_running()?;
    let stream = connect_server_with_retry(SERVER_START_TIMEOUT)?;
    let mut writer = stream
        .try_clone()
        .context("failed to clone server socket writer")?;
    let mut reader = BufReader::new(stream);

    write_json_line(
        &mut writer,
        json!({
            "type": "subscribe",
            "id": 1,
            "client_has_local_history": false,
            "allow_session_takeover": false,
        }),
    )?;

    let session_id = read_session_id(&mut reader, SERVER_START_TIMEOUT)?;

    write_json_line(
        &mut writer,
        json!({
            "type": "message",
            "id": 2,
            "content": message,
            "images": [],
        }),
    )?;

    std::thread::spawn(move || drain_session_events(reader));
    Ok(session_id)
}

#[cfg(not(unix))]
pub fn start_fresh_server_session(_message: &str) -> Result<String> {
    anyhow::bail!("desktop fresh server sessions are not implemented on this platform yet")
}

#[cfg(unix)]
fn ensure_server_running() -> Result<()> {
    if UnixStream::connect(socket_path()).is_ok() {
        return Ok(());
    }

    Command::new(jcode_bin())
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn jcode serve")?;

    connect_server_with_retry(SERVER_START_TIMEOUT).map(|_| ())
}

#[cfg(unix)]
fn connect_server_with_retry(timeout: Duration) -> Result<UnixStream> {
    let socket_path = socket_path();
    let started = Instant::now();
    let mut last_error = None;
    while started.elapsed() < timeout {
        match UnixStream::connect(&socket_path) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(SERVER_CONNECT_RETRY_DELAY);
    }

    match last_error {
        Some(error) => Err(error).with_context(|| {
            format!(
                "timed out connecting to jcode server at {}",
                socket_path.display()
            )
        }),
        None => anyhow::bail!("timed out connecting to jcode server"),
    }
}

#[cfg(unix)]
fn read_session_id(reader: &mut BufReader<UnixStream>, timeout: Duration) -> Result<String> {
    reader
        .get_ref()
        .set_read_timeout(Some(SERVER_CONNECT_RETRY_DELAY))
        .context("failed to configure server socket timeout")?;
    let started = Instant::now();
    let mut line = String::new();
    while started.elapsed() < timeout {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => anyhow::bail!("jcode server disconnected before assigning a session"),
            Ok(_) => {
                let value: Value = serde_json::from_str(line.trim())
                    .context("failed to parse jcode server event")?;
                if value.get("type").and_then(Value::as_str) == Some("session") {
                    let Some(session_id) = value.get("session_id").and_then(Value::as_str) else {
                        anyhow::bail!("jcode server sent malformed session event");
                    };
                    return Ok(session_id.to_string());
                }
                if value.get("type").and_then(Value::as_str) == Some("error") {
                    let message = value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown server error");
                    anyhow::bail!("jcode server rejected fresh session: {message}");
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error).context("failed to read jcode server event"),
        }
    }

    anyhow::bail!("timed out waiting for jcode server session id")
}

#[cfg(unix)]
fn write_json_line(writer: &mut UnixStream, value: Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, &value).context("failed to encode server request")?;
    writer
        .write_all(b"\n")
        .context("failed to send server request")?;
    writer.flush().context("failed to flush server request")
}

#[cfg(unix)]
fn drain_session_events(mut reader: BufReader<UnixStream>) {
    let _ = reader.get_ref().set_read_timeout(None);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    match value.get("type").and_then(Value::as_str) {
                        Some("done" | "error") => break,
                        _ => {}
                    }
                }
            }
        }
    }
}

fn socket_path() -> PathBuf {
    if let Ok(custom) = std::env::var("JCODE_SOCKET") {
        return PathBuf::from(custom);
    }
    if let Ok(dir) = std::env::var("JCODE_RUNTIME_DIR") {
        return PathBuf::from(dir).join("jcode.sock");
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("jcode.sock");
    }
    std::env::temp_dir()
        .join(format!("jcode-{}", runtime_user_discriminator()))
        .join("jcode.sock")
}

#[cfg(unix)]
fn runtime_user_discriminator() -> String {
    unsafe { libc::geteuid() }.to_string()
}

#[cfg(not(unix))]
fn runtime_user_discriminator() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_string())
}

fn launch_first_available_terminal(candidates: Vec<Command>, description: &str) -> Result<()> {
    let mut failures = Vec::new();

    for mut candidate in candidates {
        match candidate.spawn() {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                failures.push(format!(
                    "{} not found",
                    candidate.get_program().to_string_lossy()
                ));
            }
            Err(error) => {
                failures.push(format!(
                    "{}: {error}",
                    candidate.get_program().to_string_lossy()
                ));
            }
        }
    }

    anyhow::bail!(
        "failed to launch a terminal for {description}: {}",
        failures.join("; ")
    )
}

fn terminal_candidates(title: &str, jcode_args: &[&str]) -> Vec<Command> {
    let mut candidates = Vec::new();

    if let Ok(program) = std::env::var("JCODE_DESKTOP_TERMINAL") {
        candidates.push(terminal_command(program, &[], jcode_args));
    }

    candidates.push(terminal_command(
        "footclient",
        &["-T", title, "--"],
        jcode_args,
    ));
    candidates.push(terminal_command("foot", &["-T", title, "--"], jcode_args));
    candidates.push(terminal_command("kitty", &["--title", title], jcode_args));
    candidates.push(terminal_command(
        "alacritty",
        &["-t", title, "-e"],
        jcode_args,
    ));
    candidates.push(terminal_command("wezterm", &["start", "--"], jcode_args));
    candidates.push(terminal_command(
        "x-terminal-emulator",
        &["-T", title, "-e"],
        jcode_args,
    ));

    candidates
}

fn terminal_command(
    program: impl AsRef<str>,
    prefix_args: &[&str],
    jcode_args: &[&str],
) -> Command {
    let mut command = Command::new(program.as_ref());
    command
        .args(prefix_args)
        .arg(jcode_bin())
        .args(jcode_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn jcode_bin() -> String {
    std::env::var("JCODE_BIN").unwrap_or_else(|_| "jcode".to_string())
}

fn compact_title(title: &str) -> String {
    let normalized = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return "session".to_string();
    }

    let mut chars = normalized.chars();
    let compact = chars.by_ref().take(48).collect::<String>();
    if chars.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}

pub fn validate_resume_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty() {
        anyhow::bail!("empty session id");
    }
    if !session_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        anyhow::bail!("session id contains unsupported characters");
    }
    Ok(())
}

pub fn launch_validated_resume_session(session_id: &str, title: &str) -> Result<()> {
    validate_resume_session_id(session_id).context("refusing to launch invalid session id")?;
    launch_resume_session(session_id, title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_safe_session_ids() -> Result<()> {
        validate_resume_session_id("session_cow_123-abc.def")?;
        assert!(validate_resume_session_id("bad/id").is_err());
        assert!(validate_resume_session_id("bad id").is_err());
        Ok(())
    }

    #[test]
    fn compact_title_shortens_long_titles() {
        let title =
            compact_title("this is a very long title that should become shorter for terminals");
        assert!(title.ends_with('…'));
        assert!(title.chars().count() <= 49);
    }
}
