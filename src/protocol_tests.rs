use super::*;
use anyhow::{Result, anyhow};

fn parse_request_json(json: &str) -> Result<Request> {
    serde_json::from_str(json).map_err(Into::into)
}

fn parse_event_json(json: &str) -> Result<ServerEvent> {
    serde_json::from_str(json).map_err(Into::into)
}

#[test]
fn test_request_roundtrip() -> Result<()> {
    let req = Request::Message {
        id: 1,
        content: "hello".to_string(),
        images: vec![],
        system_reminder: None,
    };
    let json = serde_json::to_string(&req)?;
    let decoded = parse_request_json(&json)?;
    assert_eq!(decoded.id(), 1);
    Ok(())
}

#[test]
fn test_event_roundtrip() -> Result<()> {
    let event = ServerEvent::TextDelta {
        text: "hello".to_string(),
    };
    let json = encode_event(&event);
    let decoded = parse_event_json(json.trim())?;
    let ServerEvent::TextDelta { text } = decoded else {
        return Err(anyhow!("wrong event type"));
    };
    assert_eq!(text, "hello");
    Ok(())
}

#[test]
fn test_interrupted_event_decodes_from_json() -> Result<()> {
    let json = r#"{"type":"interrupted"}"#;
    let decoded = parse_event_json(json)?;
    let ServerEvent::Interrupted = decoded else {
        return Err(anyhow!("wrong event type"));
    };
    Ok(())
}

#[test]
fn test_connection_type_event_roundtrip() -> Result<()> {
    let event = ServerEvent::ConnectionType {
        connection: "websocket".to_string(),
    };
    let json = encode_event(&event);
    let decoded = parse_event_json(json.trim())?;
    let ServerEvent::ConnectionType { connection } = decoded else {
        return Err(anyhow!("wrong event type"));
    };
    assert_eq!(connection, "websocket");
    Ok(())
}

#[test]
fn test_status_detail_event_roundtrip() -> Result<()> {
    let event = ServerEvent::StatusDetail {
        detail: "reusing websocket".to_string(),
    };
    let json = encode_event(&event);
    let decoded = parse_event_json(json.trim())?;
    let ServerEvent::StatusDetail { detail } = decoded else {
        return Err(anyhow!("wrong event type"));
    };
    assert_eq!(detail, "reusing websocket");
    Ok(())
}

#[test]
fn test_interrupted_event_roundtrip() -> Result<()> {
    let event = ServerEvent::Interrupted;
    let json = encode_event(&event);
    assert!(json.contains("\"type\":\"interrupted\""));
    let decoded = parse_event_json(json.trim())?;
    let ServerEvent::Interrupted = decoded else {
        return Err(anyhow!("wrong event type"));
    };
    Ok(())
}

#[test]
fn test_history_event_decodes_without_compaction_mode_for_older_servers() -> Result<()> {
    let json = r#"{
            "type":"history",
            "id":1,
            "session_id":"ses_test_123",
            "messages":[],
            "provider_name":"openai",
            "provider_model":"gpt-5.4",
            "available_models":["gpt-5.4"],
            "connection_type":"websocket"
        }"#;
    let decoded = parse_event_json(json)?;
    let ServerEvent::History {
        provider_name,
        provider_model,
        available_models,
        connection_type,
        compaction_mode,
        side_panel,
        ..
    } = decoded else {
        return Err(anyhow!("wrong event type"));
    };
    assert_eq!(provider_name.as_deref(), Some("openai"));
    assert_eq!(provider_model.as_deref(), Some("gpt-5.4"));
    assert_eq!(available_models, vec!["gpt-5.4"]);
    assert_eq!(connection_type.as_deref(), Some("websocket"));
    assert_eq!(compaction_mode, crate::config::CompactionMode::Reactive);
    assert!(!side_panel.has_pages());
    Ok(())
}

#[test]
fn test_error_event_retry_after_roundtrip() {
    let event = ServerEvent::Error {
        id: 42,
        message: "rate limited".to_string(),
        retry_after_secs: Some(17),
    };
    let json = encode_event(&event);
    let decoded: ServerEvent = serde_json::from_str(json.trim()).unwrap();
    match decoded {
        ServerEvent::Error {
            id,
            message,
            retry_after_secs,
        } => {
            assert_eq!(id, 42);
            assert_eq!(message, "rate limited");
            assert_eq!(retry_after_secs, Some(17));
        }
        _ => panic!("wrong event type"),
    }
}

#[test]
fn test_error_event_retry_after_back_compat_default() {
    let json = r#"{"type":"error","id":7,"message":"oops"}"#;
    let decoded: ServerEvent = serde_json::from_str(json).unwrap();
    match decoded {
        ServerEvent::Error {
            id,
            message,
            retry_after_secs,
        } => {
            assert_eq!(id, 7);
            assert_eq!(message, "oops");
            assert_eq!(retry_after_secs, None);
        }
        _ => panic!("wrong event type"),
    }
}

#[test]
fn test_comm_propose_plan_roundtrip() {
    let req = Request::CommProposePlan {
        id: 42,
        session_id: "sess_a".to_string(),
        items: vec![PlanItem {
            content: "Refactor parser".to_string(),
            status: "pending".to_string(),
            priority: "high".to_string(),
            id: "p1".to_string(),
            blocked_by: vec!["p0".to_string()],
            assigned_to: Some("sess_b".to_string()),
        }],
    };
    let json = serde_json::to_string(&req).unwrap();
    let decoded: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.id(), 42);
    match decoded {
        Request::CommProposePlan { items, .. } => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].id, "p1");
        }
        _ => panic!("wrong request type"),
    }
}

#[test]
fn test_stdin_response_roundtrip() {
    let req = Request::StdinResponse {
        id: 99,
        request_id: "stdin-call_abc-1".to_string(),
        input: "my_password".to_string(),
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"type\":\"stdin_response\""));
    assert!(json.contains("\"request_id\":\"stdin-call_abc-1\""));
    assert!(json.contains("\"input\":\"my_password\""));

    let decoded: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.id(), 99);
    match decoded {
        Request::StdinResponse {
            request_id, input, ..
        } => {
            assert_eq!(request_id, "stdin-call_abc-1");
            assert_eq!(input, "my_password");
        }
        _ => panic!("expected StdinResponse"),
    }
}

#[test]
fn test_stdin_response_deserialize_from_json() {
    let json = r#"{"type":"stdin_response","id":5,"request_id":"req-42","input":"hello world"}"#;
    let decoded: Request = serde_json::from_str(json).unwrap();
    assert_eq!(decoded.id(), 5);
    match decoded {
        Request::StdinResponse {
            request_id, input, ..
        } => {
            assert_eq!(request_id, "req-42");
            assert_eq!(input, "hello world");
        }
        _ => panic!("expected StdinResponse"),
    }
}

#[test]
fn test_stdin_request_event_roundtrip() {
    let event = ServerEvent::StdinRequest {
        request_id: "stdin-xyz-1".to_string(),
        prompt: "Password: ".to_string(),
        is_password: true,
        tool_call_id: "call_abc".to_string(),
    };
    let json = encode_event(&event);
    assert!(json.contains("\"type\":\"stdin_request\""));
    assert!(json.contains("\"is_password\":true"));

    let decoded: ServerEvent = serde_json::from_str(json.trim()).unwrap();
    match decoded {
        ServerEvent::StdinRequest {
            request_id,
            prompt,
            is_password,
            tool_call_id,
        } => {
            assert_eq!(request_id, "stdin-xyz-1");
            assert_eq!(prompt, "Password: ");
            assert!(is_password);
            assert_eq!(tool_call_id, "call_abc");
        }
        _ => panic!("expected StdinRequest"),
    }
}

#[test]
fn test_stdin_request_event_defaults() {
    // is_password defaults to false when not present
    let json = r#"{"type":"stdin_request","request_id":"r1","prompt":"","tool_call_id":"tc1"}"#;
    let decoded: ServerEvent = serde_json::from_str(json).unwrap();
    match decoded {
        ServerEvent::StdinRequest { is_password, .. } => {
            assert!(!is_password, "is_password should default to false");
        }
        _ => panic!("expected StdinRequest"),
    }
}

#[test]
fn test_comm_await_members_roundtrip() {
    let req = Request::CommAwaitMembers {
        id: 55,
        session_id: "sess_waiter".to_string(),
        target_status: vec!["completed".to_string(), "stopped".to_string()],
        session_ids: vec!["sess_a".to_string(), "sess_b".to_string()],
        timeout_secs: Some(120),
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"type\":\"comm_await_members\""));
    let decoded: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.id(), 55);
    match decoded {
        Request::CommAwaitMembers {
            session_id,
            target_status,
            session_ids,
            timeout_secs,
            ..
        } => {
            assert_eq!(session_id, "sess_waiter");
            assert_eq!(target_status, vec!["completed", "stopped"]);
            assert_eq!(session_ids, vec!["sess_a", "sess_b"]);
            assert_eq!(timeout_secs, Some(120));
        }
        _ => panic!("expected CommAwaitMembers"),
    }
}

#[test]
fn test_comm_await_members_defaults() {
    let json =
        r#"{"type":"comm_await_members","id":1,"session_id":"s1","target_status":["completed"]}"#;
    let decoded: Request = serde_json::from_str(json).unwrap();
    match decoded {
        Request::CommAwaitMembers {
            session_ids,
            timeout_secs,
            ..
        } => {
            assert!(
                session_ids.is_empty(),
                "session_ids should default to empty"
            );
            assert_eq!(timeout_secs, None, "timeout_secs should default to None");
        }
        _ => panic!("expected CommAwaitMembers"),
    }
}

#[test]
fn test_comm_await_members_response_roundtrip() {
    let event = ServerEvent::CommAwaitMembersResponse {
        id: 55,
        completed: true,
        members: vec![
            AwaitedMemberStatus {
                session_id: "sess_a".to_string(),
                friendly_name: Some("fox".to_string()),
                status: "completed".to_string(),
                done: true,
            },
            AwaitedMemberStatus {
                session_id: "sess_b".to_string(),
                friendly_name: Some("wolf".to_string()),
                status: "stopped".to_string(),
                done: true,
            },
        ],
        summary: "All 2 members are done: fox, wolf".to_string(),
    };
    let json = encode_event(&event);
    assert!(json.contains("\"type\":\"comm_await_members_response\""));
    let decoded: ServerEvent = serde_json::from_str(json.trim()).unwrap();
    match decoded {
        ServerEvent::CommAwaitMembersResponse {
            id,
            completed,
            members,
            summary,
        } => {
            assert_eq!(id, 55);
            assert!(completed);
            assert_eq!(members.len(), 2);
            assert_eq!(members[0].friendly_name.as_deref(), Some("fox"));
            assert!(members[0].done);
            assert_eq!(members[1].status, "stopped");
            assert!(summary.contains("fox"));
        }
        _ => panic!("expected CommAwaitMembersResponse"),
    }
}

#[test]
fn test_comm_members_roundtrip_includes_status() {
    let event = ServerEvent::CommMembers {
        id: 9,
        members: vec![AgentInfo {
            session_id: "sess-peer".to_string(),
            friendly_name: Some("bear".to_string()),
            files_touched: vec!["src/main.rs".to_string()],
            status: Some("running".to_string()),
            detail: Some("working on tests".to_string()),
            role: Some("agent".to_string()),
        }],
    };

    let json = encode_event(&event);
    assert!(json.contains("\"type\":\"comm_members\""));
    assert!(json.contains("\"status\":\"running\""));

    let decoded: ServerEvent = serde_json::from_str(json.trim()).unwrap();
    match decoded {
        ServerEvent::CommMembers { id, members } => {
            assert_eq!(id, 9);
            assert_eq!(members.len(), 1);
            assert_eq!(members[0].friendly_name.as_deref(), Some("bear"));
            assert_eq!(members[0].status.as_deref(), Some("running"));
            assert_eq!(members[0].detail.as_deref(), Some("working on tests"));
        }
        _ => panic!("expected CommMembers"),
    }
}

#[test]
fn test_transcript_request_roundtrip() {
    let req = Request::Transcript {
        id: 77,
        text: "hello from whisper".to_string(),
        mode: TranscriptMode::Send,
        session_id: Some("sess_abc".to_string()),
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"type\":\"transcript\""));
    let decoded: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.id(), 77);
    match decoded {
        Request::Transcript {
            text,
            mode,
            session_id,
            ..
        } => {
            assert_eq!(text, "hello from whisper");
            assert_eq!(mode, TranscriptMode::Send);
            assert_eq!(session_id.as_deref(), Some("sess_abc"));
        }
        _ => panic!("expected Transcript request"),
    }
}

#[test]
fn test_transcript_event_roundtrip() {
    let event = ServerEvent::Transcript {
        text: "dictated text".to_string(),
        mode: TranscriptMode::Replace,
    };
    let json = encode_event(&event);
    assert!(json.contains("\"type\":\"transcript\""));
    let decoded: ServerEvent = serde_json::from_str(json.trim()).unwrap();
    match decoded {
        ServerEvent::Transcript { text, mode } => {
            assert_eq!(text, "dictated text");
            assert_eq!(mode, TranscriptMode::Replace);
        }
        _ => panic!("expected Transcript event"),
    }
}

#[test]
fn test_memory_activity_event_roundtrip() {
    let event = ServerEvent::MemoryActivity {
        activity: MemoryActivitySnapshot {
            state: MemoryStateSnapshot::SidecarChecking { count: 3 },
            state_age_ms: 275,
            pipeline: Some(MemoryPipelineSnapshot {
                search: MemoryStepStatusSnapshot::Done,
                search_result: Some(MemoryStepResultSnapshot {
                    summary: "5 hits".to_string(),
                    latency_ms: 14,
                }),
                verify: MemoryStepStatusSnapshot::Running,
                verify_result: None,
                verify_progress: Some((1, 3)),
                inject: MemoryStepStatusSnapshot::Pending,
                inject_result: None,
                maintain: MemoryStepStatusSnapshot::Pending,
                maintain_result: None,
            }),
        },
    };

    let json = encode_event(&event);
    assert!(json.contains("\"type\":\"memory_activity\""));
    let decoded: ServerEvent = serde_json::from_str(json.trim()).unwrap();
    match decoded {
        ServerEvent::MemoryActivity { activity } => {
            assert_eq!(
                activity.state,
                MemoryStateSnapshot::SidecarChecking { count: 3 }
            );
            assert_eq!(activity.state_age_ms, 275);
            let pipeline = activity.pipeline.expect("pipeline snapshot");
            assert_eq!(pipeline.search, MemoryStepStatusSnapshot::Done);
            assert_eq!(pipeline.verify, MemoryStepStatusSnapshot::Running);
            assert_eq!(pipeline.verify_progress, Some((1, 3)));
        }
        _ => panic!("expected MemoryActivity event"),
    }
}

#[test]
fn test_input_shell_request_roundtrip() {
    let req = Request::InputShell {
        id: 88,
        command: "ls -la".to_string(),
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"type\":\"input_shell\""));
    let decoded: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.id(), 88);
    match decoded {
        Request::InputShell { id, command } => {
            assert_eq!(id, 88);
            assert_eq!(command, "ls -la");
        }
        _ => panic!("expected InputShell request"),
    }
}

#[test]
fn test_input_shell_result_event_roundtrip() {
    let event = ServerEvent::InputShellResult {
        result: crate::message::InputShellResult {
            command: "pwd".to_string(),
            cwd: Some("/tmp/project".to_string()),
            output: "/tmp/project\n".to_string(),
            exit_code: Some(0),
            duration_ms: 7,
            truncated: false,
            failed_to_start: false,
        },
    };
    let json = encode_event(&event);
    assert!(json.contains("\"type\":\"input_shell_result\""));
    let decoded: ServerEvent = serde_json::from_str(json.trim()).unwrap();
    match decoded {
        ServerEvent::InputShellResult { result } => {
            assert_eq!(result.command, "pwd");
            assert_eq!(result.cwd.as_deref(), Some("/tmp/project"));
            assert_eq!(result.exit_code, Some(0));
        }
        _ => panic!("expected InputShellResult event"),
    }
}
