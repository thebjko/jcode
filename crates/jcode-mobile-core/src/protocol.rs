use serde::{Deserialize, Serialize};
use serde_json::Value;

fn is_false(value: &bool) -> bool {
    !*value
}
fn is_empty_images(images: &[(String, String)]) -> bool {
    images.is_empty()
}
fn default_model_direction() -> i8 {
    1
}

/// Requests sent by the mobile app to the jcode gateway.
/// Mirrors the current Swift `Request` enum in `ios/Sources/JCodeKit/Protocol.swift`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MobileRequest {
    Subscribe {
        id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
    },
    Message {
        id: u64,
        content: String,
        #[serde(default, skip_serializing_if = "is_empty_images")]
        images: Vec<(String, String)>,
    },
    Cancel {
        id: u64,
    },
    Ping {
        id: u64,
    },
    GetHistory {
        id: u64,
    },
    State {
        id: u64,
    },
    Clear {
        id: u64,
    },
    ResumeSession {
        id: u64,
        session_id: String,
    },
    CycleModel {
        id: u64,
        #[serde(default = "default_model_direction")]
        direction: i8,
    },
    SetModel {
        id: u64,
        model: String,
    },
    Compact {
        id: u64,
    },
    SoftInterrupt {
        id: u64,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        urgent: bool,
    },
    CancelSoftInterrupts {
        id: u64,
    },
    BackgroundTool {
        id: u64,
    },
    Split {
        id: u64,
    },
    StdinResponse {
        id: u64,
        request_id: String,
        input: String,
    },
}

/// Events received by the mobile app from the jcode gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MobileServerEvent {
    Ack {
        id: u64,
    },
    TextDelta {
        text: String,
    },
    TextReplace {
        text: String,
    },
    ToolStart {
        id: String,
        name: String,
    },
    ToolInput {
        delta: String,
    },
    ToolExec {
        id: String,
        name: String,
    },
    ToolDone {
        id: String,
        name: String,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "tokens")]
    TokenUsage {
        input: u64,
        output: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_input: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_creation_input: Option<u64>,
    },
    UpstreamProvider {
        provider: String,
    },
    Done {
        id: u64,
    },
    Error {
        id: u64,
        message: String,
    },
    Pong {
        id: u64,
    },
    State {
        id: u64,
        session_id: String,
        message_count: usize,
        is_processing: bool,
    },
    SessionId {
        session_id: String,
    },
    History(HistoryPayload),
    Reloading {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_socket: Option<String>,
    },
    ReloadProgress {
        step: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        success: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    ModelChanged {
        id: u64,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Notification(MobileNotification),
    SwarmStatus {
        members: Vec<SwarmMemberStatus>,
    },
    McpStatus {
        servers: Vec<String>,
    },
    SoftInterruptInjected {
        content: String,
        point: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools_skipped: Option<usize>,
    },
    Interrupted,
    MemoryInjected {
        count: usize,
        prompt: String,
        prompt_chars: usize,
        computed_age_ms: u64,
    },
    SplitResponse {
        id: u64,
        new_session_id: String,
        new_session_name: String,
    },
    CompactResult {
        id: u64,
        message: String,
        success: bool,
    },
    StdinRequest {
        request_id: String,
        prompt: String,
        is_password: bool,
        tool_call_id: String,
    },
}

/// Lossless event envelope for preserving unknown gateway events in simulator/fake-backend work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawMobileServerEvent {
    pub event_type: String,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPayload {
    pub session_id: String,
    #[serde(default)]
    pub messages: Vec<HistoryMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
    #[serde(default)]
    pub available_models: Vec<String>,
    #[serde(default)]
    pub all_sessions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_canary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was_interrupted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<TokenTotals>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessage {
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_data: Option<HistoryToolData>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryToolData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileNotification {
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmMemberStatus {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub code: String,
    pub device_id: String,
    pub device_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apns_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResponse {
    pub token: String,
    pub server_name: String,
    pub server_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairErrorBody {
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub gateway: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mobile_request_matches_gateway_json_shape() {
        let request = MobileRequest::Message {
            id: 7,
            content: "hello".to_string(),
            images: vec![("image/jpeg".to_string(), "abc".to_string())],
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({"type":"message","id":7,"content":"hello","images":[["image/jpeg","abc"]]})
        );
    }

    #[test]
    fn mobile_request_omits_empty_optional_fields() {
        let request = MobileRequest::Subscribe {
            id: 1,
            working_dir: None,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({"type":"subscribe","id":1})
        );
    }

    #[test]
    fn mobile_event_decodes_text_replace() {
        let event: MobileServerEvent =
            serde_json::from_value(json!({"type":"text_replace","text":"replacement"})).unwrap();
        assert_eq!(
            event,
            MobileServerEvent::TextReplace {
                text: "replacement".to_string()
            }
        );
    }

    #[test]
    fn history_payload_decodes_server_metadata() {
        let event: MobileServerEvent = serde_json::from_value(json!({"type":"history","session_id":"s1","server_name":"jcode","provider_model":"gpt-5","available_models":["gpt-5","claude-sonnet-4"],"all_sessions":["s1","s2"],"messages":[{"role":"assistant","content":"hi"}]})).unwrap();
        let MobileServerEvent::History(payload) = event else {
            panic!("expected history event");
        };
        assert_eq!(payload.session_id, "s1");
        assert_eq!(payload.provider_model.as_deref(), Some("gpt-5"));
        assert_eq!(payload.messages[0].content, "hi");
    }

    #[test]
    fn pairing_models_match_swift_sdk_shape() {
        let request = PairRequest {
            code: "123456".to_string(),
            device_id: "ios-test".to_string(),
            device_name: "simulator".to_string(),
            apns_token: None,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({"code":"123456","device_id":"ios-test","device_name":"simulator"})
        );
    }
}
