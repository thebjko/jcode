use anyhow::Result;
use jcode_agent_runtime::{SoftInterruptMessage, SoftInterruptSource};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSoftInterrupt {
    content: String,
    urgent: bool,
    source: PersistedSoftInterruptSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedSoftInterruptSource {
    User,
    System,
    BackgroundTask,
}

impl From<SoftInterruptSource> for PersistedSoftInterruptSource {
    fn from(value: SoftInterruptSource) -> Self {
        match value {
            SoftInterruptSource::User => Self::User,
            SoftInterruptSource::System => Self::System,
            SoftInterruptSource::BackgroundTask => Self::BackgroundTask,
        }
    }
}

impl From<PersistedSoftInterruptSource> for SoftInterruptSource {
    fn from(value: PersistedSoftInterruptSource) -> Self {
        match value {
            PersistedSoftInterruptSource::User => Self::User,
            PersistedSoftInterruptSource::System => Self::System,
            PersistedSoftInterruptSource::BackgroundTask => Self::BackgroundTask,
        }
    }
}

impl From<SoftInterruptMessage> for PersistedSoftInterrupt {
    fn from(value: SoftInterruptMessage) -> Self {
        Self {
            content: value.content,
            urgent: value.urgent,
            source: value.source.into(),
        }
    }
}

impl From<PersistedSoftInterrupt> for SoftInterruptMessage {
    fn from(value: PersistedSoftInterrupt) -> Self {
        Self {
            content: value.content,
            urgent: value.urgent,
            source: value.source.into(),
        }
    }
}

fn dir_path() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("pending-soft-interrupts"))
}

fn path_for_session(session_id: &str) -> Result<PathBuf> {
    Ok(dir_path()?.join(format!("{}.json", session_id)))
}

pub fn load(session_id: &str) -> Result<Vec<SoftInterruptMessage>> {
    let path = path_for_session(session_id)?;
    if !path.exists() {
        return Ok(Vec::new());
    }

    let persisted: Vec<PersistedSoftInterrupt> = crate::storage::read_json(&path)?;
    Ok(persisted
        .into_iter()
        .map(SoftInterruptMessage::from)
        .collect())
}

pub fn take(session_id: &str) -> Result<Vec<SoftInterruptMessage>> {
    let path = path_for_session(session_id)?;
    let loaded = load(session_id)?;
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    Ok(loaded)
}

pub fn overwrite(session_id: &str, interrupts: &[SoftInterruptMessage]) -> Result<()> {
    let path = path_for_session(session_id)?;
    if interrupts.is_empty() {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let persisted: Vec<PersistedSoftInterrupt> =
        interrupts.iter().cloned().map(Into::into).collect();
    crate::storage::write_json_fast(&path, &persisted)
}

pub fn append(session_id: &str, interrupt: SoftInterruptMessage) -> Result<()> {
    let mut current = load(session_id)?;
    current.push(interrupt);
    overwrite(session_id, &current)
}

pub fn clear(session_id: &str) -> Result<()> {
    overwrite(session_id, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_take_and_clear_round_trip() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let session_id = "ses_soft_interrupt_store";
        append(
            session_id,
            SoftInterruptMessage {
                content: "hello".to_string(),
                urgent: true,
                source: SoftInterruptSource::System,
            },
        )
        .expect("append first interrupt");
        append(
            session_id,
            SoftInterruptMessage {
                content: "world".to_string(),
                urgent: false,
                source: SoftInterruptSource::BackgroundTask,
            },
        )
        .expect("append second interrupt");

        let loaded = load(session_id).expect("load interrupts");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "hello");
        assert!(loaded[0].urgent);
        assert_eq!(loaded[1].content, "world");

        let taken = take(session_id).expect("take interrupts");
        assert_eq!(taken.len(), 2);
        assert!(load(session_id).expect("reload after take").is_empty());

        append(
            session_id,
            SoftInterruptMessage {
                content: "later".to_string(),
                urgent: false,
                source: SoftInterruptSource::User,
            },
        )
        .expect("append later interrupt");
        clear(session_id).expect("clear interrupts");
        assert!(load(session_id).expect("load after clear").is_empty());

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }
}
