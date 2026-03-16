use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Platform-aware runtime directory for sockets and ephemeral state.
///
/// - Linux: `$XDG_RUNTIME_DIR` (typically `/run/user/<uid>`)
/// - macOS: `$TMPDIR` (per-user, e.g. `/var/folders/xx/.../T/`)
/// - Fallback: `std::env::temp_dir()`
///
/// Can be overridden with `$JCODE_RUNTIME_DIR`.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JCODE_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(dir) = std::env::var("TMPDIR") {
            return PathBuf::from(dir);
        }
    }
    std::env::temp_dir()
}

pub fn jcode_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("JCODE_HOME") {
        return Ok(PathBuf::from(path));
    }

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory"))?;
    Ok(home.join(".jcode"))
}

pub fn logs_dir() -> Result<PathBuf> {
    Ok(jcode_dir()?.join("logs"))
}

/// Resolve a path under the user's home directory, but sandbox it under
/// `$JCODE_HOME/external/` when `JCODE_HOME` is set.
///
/// This keeps external provider auth files isolated during tests and sandboxed
/// runs without changing default on-disk locations for normal users.
pub fn user_home_path(relative: impl AsRef<Path>) -> Result<PathBuf> {
    let relative = relative.as_ref();
    if relative.is_absolute() {
        anyhow::bail!(
            "user_home_path expects a relative path, got {}",
            relative.display()
        );
    }

    if let Ok(path) = std::env::var("JCODE_HOME") {
        return Ok(PathBuf::from(path).join("external").join(relative));
    }

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory"))?;
    Ok(home.join(relative))
}

/// Best-effort startup hardening for local config dirs that may store credentials.
///
/// This intentionally ignores failures so startup does not fail on exotic
/// filesystems, but it narrows exposure on typical Unix systems.
pub fn harden_user_config_permissions() {
    if let Some(config_dir) = dirs::config_dir() {
        let jcode_config_dir = config_dir.join("jcode");
        if jcode_config_dir.exists() {
            let _ = crate::platform::set_directory_permissions_owner_only(&jcode_config_dir);
        }
    }

    if let Ok(jcode_home) = jcode_dir() {
        if jcode_home.exists() {
            let _ = crate::platform::set_directory_permissions_owner_only(&jcode_home);
        }
    }
}

/// Best-effort hardening for a secret-bearing file and its parent directory.
///
/// This is used before reading credential files so legacy permissive modes can
/// be tightened opportunistically.
pub fn harden_secret_file_permissions(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = crate::platform::set_directory_permissions_owner_only(parent);
    }
    if path.exists() {
        let _ = crate::platform::set_permissions_owner_only(path);
    }
}

#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static Mutex<()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
pub(crate) fn lock_test_env() -> MutexGuard<'static, ()> {
    test_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn harden_secret_file_permissions_sets_owner_only_modes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().expect("create temp dir");
        let secret_dir = dir.path().join("jcode");
        std::fs::create_dir_all(&secret_dir).expect("create secret dir");

        let secret_file = secret_dir.join("openrouter.env");
        std::fs::write(&secret_file, "OPENROUTER_API_KEY=sk-or-v1-test\n")
            .expect("write secret file");

        std::fs::set_permissions(&secret_dir, std::fs::Permissions::from_mode(0o755))
            .expect("set initial dir perms");
        std::fs::set_permissions(&secret_file, std::fs::Permissions::from_mode(0o644))
            .expect("set initial file perms");

        harden_secret_file_permissions(&secret_file);

        let dir_mode = std::fs::metadata(&secret_dir)
            .expect("stat dir")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(&secret_file)
            .expect("stat file")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn user_home_path_uses_external_dir_under_jcode_home() {
        let _guard = lock_test_env();
        let prev_home = std::env::var_os("JCODE_HOME");
        let temp = tempfile::TempDir::new().expect("create temp dir");
        crate::env::set_var("JCODE_HOME", temp.path());

        let resolved = user_home_path(".codex/auth.json").expect("resolve user home path");
        assert_eq!(
            resolved,
            temp.path()
                .join("external")
                .join(".codex")
                .join("auth.json")
        );

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
        crate::platform::set_directory_permissions_owner_only(path)?;
    }
    Ok(())
}

pub fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    write_json_inner(path, value, true)
}

pub fn write_json_secret<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    write_json_inner(path, value, true)?;
    if let Some(parent) = path.parent() {
        crate::platform::set_directory_permissions_owner_only(parent)?;
    }
    crate::platform::set_permissions_owner_only(path)?;
    Ok(())
}

/// Fast JSON write: atomic rename but no fsync. Good for frequent saves where
/// durability on power loss is not critical (e.g., session saves during tool execution).
/// Data is still safe against process crashes (atomic rename protects against partial writes).
pub fn write_json_fast<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    write_json_inner(path, value, false)
}

fn write_json_inner<T: Serialize + ?Sized>(path: &Path, value: &T, durable: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }

    let pid = std::process::id();
    let nonce: u64 = rand::random();
    let tmp_path = path.with_extension(format!("tmp.{}.{}", pid, nonce));

    let result = (|| -> Result<()> {
        let file = std::fs::File::create(&tmp_path)?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, value)?;
        let file = writer
            .into_inner()
            .map_err(|e| anyhow::anyhow!("flush failed: {}", e))?;

        if durable {
            file.sync_all()?;
        }

        if path.exists() {
            let bak_path = path.with_extension("bak");
            let _ = std::fs::rename(path, &bak_path);
        }

        std::fs::rename(&tmp_path, path)?;

        #[cfg(unix)]
        if durable {
            if let Some(parent) = path.parent() {
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
        }

        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }

    result
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let data = std::fs::read_to_string(path)?;
    match serde_json::from_str(&data) {
        Ok(val) => Ok(val),
        Err(e) => {
            let bak_path = path.with_extension("bak");
            if bak_path.exists() {
                crate::logging::warn(&format!(
                    "Corrupt JSON at {}, trying backup: {}",
                    path.display(),
                    e
                ));
                let bak_data = std::fs::read_to_string(&bak_path)?;
                match serde_json::from_str(&bak_data) {
                    Ok(val) => {
                        crate::logging::info(&format!(
                            "Recovered from backup: {}",
                            bak_path.display()
                        ));
                        let _ = std::fs::copy(&bak_path, path);
                        Ok(val)
                    }
                    Err(bak_err) => Err(anyhow::anyhow!(
                        "Corrupt JSON at {} ({}), backup also corrupt ({})",
                        path.display(),
                        e,
                        bak_err
                    )),
                }
            } else {
                Err(anyhow::anyhow!("Corrupt JSON at {}: {}", path.display(), e))
            }
        }
    }
}

/// Fast append of a single JSON value followed by a newline.
/// Intended for append-only journals where per-write fsync is not required.
pub fn append_json_line_fast<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}
