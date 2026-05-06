use super::{
    DEFAULT_CONTEXT_LIMIT, EventStream, ModelCatalogRefreshSummary, ModelRoute, Provider,
    RouteCheapnessEstimate, RouteCostConfidence, RouteCostSource, summarize_model_catalog_refresh,
};
use crate::message::{
    ContentBlock as JContentBlock, Message as JMessage, Role as JRole, StreamEvent, ToolDefinition,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_bedrock::Client as BedrockControlClient;
use aws_sdk_bedrockruntime::Client as BedrockRuntimeClient;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ContentBlockDelta, ContentBlockStart, ConversationRole, ConverseStreamOutput,
    ImageBlock, ImageFormat, ImageSource, InferenceConfiguration, Message,
    ReasoningContentBlockDelta, SystemContentBlock, Tool, ToolConfiguration, ToolInputSchema,
    ToolSpecification,
};
use aws_smithy_types::Blob;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const DEFAULT_MODEL: &str = "anthropic.claude-3-5-sonnet-20241022-v2:0";
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 4096;

#[derive(Debug, Clone)]
struct BedrockModelInfo {
    context_tokens: usize,
    max_output_tokens: usize,
    supports_tools: bool,
    supports_vision: bool,
    supports_reasoning: bool,
    pricing: Option<(u64, u64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedCatalog {
    models: Vec<String>,
    inference_profiles: Vec<String>,
    region: Option<String>,
    fetched_at_rfc3339: String,
}

pub struct BedrockProvider {
    model: Arc<RwLock<String>>,
    fetched_models: Arc<RwLock<Vec<String>>>,
    fetched_inference_profiles: Arc<RwLock<Vec<String>>>,
}

impl BedrockProvider {
    pub fn new() -> Self {
        let model =
            std::env::var("JCODE_BEDROCK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let provider = Self {
            model: Arc::new(RwLock::new(model)),
            fetched_models: Arc::new(RwLock::new(Vec::new())),
            fetched_inference_profiles: Arc::new(RwLock::new(Vec::new())),
        };
        provider.seed_cached_catalog();
        provider
    }

    pub fn has_credentials() -> bool {
        let explicitly_enabled = std::env::var("JCODE_BEDROCK_ENABLE")
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if explicitly_enabled {
            return true;
        }

        let has_region = std::env::var_os("JCODE_BEDROCK_REGION").is_some()
            || std::env::var_os("AWS_REGION").is_some()
            || std::env::var_os("AWS_DEFAULT_REGION").is_some();
        let has_credential_hint = std::env::var_os("AWS_BEARER_TOKEN_BEDROCK").is_some()
            || std::env::var_os("AWS_ACCESS_KEY_ID").is_some()
            || std::env::var_os("AWS_PROFILE").is_some()
            || std::env::var_os("JCODE_BEDROCK_PROFILE").is_some()
            || std::env::var_os("AWS_WEB_IDENTITY_TOKEN_FILE").is_some()
            || std::env::var_os("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
            || std::env::var_os("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
            || std::env::var_os("AWS_SHARED_CREDENTIALS_FILE").is_some()
            || std::env::var_os("AWS_CONFIG_FILE").is_some();

        has_region && has_credential_hint
    }

    async fn sdk_config() -> aws_types::SdkConfig {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Ok(region) = std::env::var("JCODE_BEDROCK_REGION")
            .or_else(|_| std::env::var("AWS_REGION"))
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        {
            loader = loader.region(aws_types::region::Region::new(region));
        }
        if let Ok(profile) =
            std::env::var("JCODE_BEDROCK_PROFILE").or_else(|_| std::env::var("AWS_PROFILE"))
        {
            loader = loader.profile_name(profile);
        }
        loader.load().await
    }

    async fn runtime_client() -> BedrockRuntimeClient {
        let config = Self::sdk_config().await;
        BedrockRuntimeClient::new(&config)
    }

    async fn control_client() -> BedrockControlClient {
        let config = Self::sdk_config().await;
        BedrockControlClient::new(&config)
    }

    async fn validate_credentials_if_requested() -> Result<()> {
        let validate = std::env::var("JCODE_BEDROCK_VALIDATE_STS")
            .ok()
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(false);
        if !validate {
            return Ok(());
        }
        let config = Self::sdk_config().await;
        let client = aws_sdk_sts::Client::new(&config);
        client
            .get_caller_identity()
            .send()
            .await
            .map(|_| ())
            .map_err(|err| anyhow::anyhow!(Self::classify_error_message(&err.to_string())))
    }

    fn configured_region() -> Option<String> {
        std::env::var("JCODE_BEDROCK_REGION")
            .or_else(|_| std::env::var("AWS_REGION"))
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    fn persisted_catalog_path() -> Result<std::path::PathBuf> {
        Ok(crate::storage::app_config_dir()?.join("bedrock_models_cache.json"))
    }

    fn load_persisted_catalog() -> Option<PersistedCatalog> {
        let path = Self::persisted_catalog_path().ok()?;
        crate::storage::read_json(&path).ok()
    }

    fn persist_catalog(models: &[String], inference_profiles: &[String]) {
        let Ok(path) = Self::persisted_catalog_path() else {
            return;
        };
        let payload = PersistedCatalog {
            models: models.to_vec(),
            inference_profiles: inference_profiles.to_vec(),
            region: Self::configured_region(),
            fetched_at_rfc3339: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(err) = crate::storage::write_json(&path, &payload) {
            crate::logging::warn(&format!(
                "Failed to persist Bedrock model catalog {}: {}",
                path.display(),
                err
            ));
        }
    }

    fn seed_cached_catalog(&self) {
        if let Some(catalog) = Self::load_persisted_catalog() {
            if let Ok(mut models) = self.fetched_models.write() {
                *models = catalog.models;
            }
            if let Ok(mut profiles) = self.fetched_inference_profiles.write() {
                *profiles = catalog.inference_profiles;
            }
        }
    }

    fn classify_error_message(raw: &str) -> String {
        let lower = raw.to_ascii_lowercase();
        let hint = if lower.contains("no credentials")
            || lower.contains("could not load credentials")
            || lower.contains("credentials") && lower.contains("not loaded")
        {
            "AWS credentials were not found. Set AWS_BEARER_TOKEN_BEDROCK, AWS_PROFILE, AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, or run `aws sso login`."
        } else if lower.contains("expired") || lower.contains("sso") && lower.contains("token") {
            "AWS SSO/session credentials look expired. Run `aws sso login --profile <profile>` and retry."
        } else if lower.contains("accessdenied")
            || lower.contains("access denied")
            || lower.contains("not authorized")
        {
            "AWS IAM denied the Bedrock request. Ensure the principal can call bedrock:InvokeModel and bedrock:InvokeModelWithResponseStream."
        } else if lower.contains("validationexception") && lower.contains("model")
            || lower.contains("model") && lower.contains("not found")
            || lower.contains("resource not found")
        {
            "Bedrock did not recognize this model in the selected region/account. Check model ID, inference profile ID, region, and model access."
        } else if lower.contains("throttl")
            || lower.contains("too many requests")
            || lower.contains("rate exceeded")
        {
            "Bedrock throttled the request. Retry later or request a quota increase."
        } else if lower.contains("region") && lower.contains("missing") {
            "AWS region is missing. Set AWS_REGION or JCODE_BEDROCK_REGION."
        } else {
            "Bedrock request failed. Check AWS credentials, region, model access, and IAM permissions."
        };
        format!("{} Original error: {}", hint, raw.trim())
    }

    fn json_to_document(value: &serde_json::Value) -> aws_smithy_types::Document {
        match value {
            serde_json::Value::Null => aws_smithy_types::Document::Null,
            serde_json::Value::Bool(v) => aws_smithy_types::Document::Bool(*v),
            serde_json::Value::Number(n) => {
                if let Some(v) = n.as_u64() {
                    aws_smithy_types::Document::from(v)
                } else if let Some(v) = n.as_i64() {
                    aws_smithy_types::Document::from(v)
                } else if let Some(v) = n.as_f64() {
                    aws_smithy_types::Document::from(v)
                } else {
                    aws_smithy_types::Document::Null
                }
            }
            serde_json::Value::String(v) => aws_smithy_types::Document::String(v.clone()),
            serde_json::Value::Array(values) => aws_smithy_types::Document::Array(
                values.iter().map(Self::json_to_document).collect(),
            ),
            serde_json::Value::Object(map) => aws_smithy_types::Document::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), Self::json_to_document(value)))
                    .collect::<HashMap<_, _>>(),
            ),
        }
    }

    fn image_format_for_media_type(media_type: &str) -> Option<ImageFormat> {
        match media_type.trim().to_ascii_lowercase().as_str() {
            "image/png" => Some(ImageFormat::Png),
            "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
            "image/gif" => Some(ImageFormat::Gif),
            "image/webp" => Some(ImageFormat::Webp),
            _ => None,
        }
    }

    fn image_block(media_type: &str, data: &str) -> Result<ImageBlock> {
        let format = Self::image_format_for_media_type(media_type).ok_or_else(|| {
            anyhow::anyhow!(
                "Bedrock image input does not support media type `{}`",
                media_type
            )
        })?;
        let bytes = BASE64.decode(data).with_context(|| {
            format!("Failed to decode {} image payload for Bedrock", media_type)
        })?;
        ImageBlock::builder()
            .format(format)
            .source(ImageSource::Bytes(Blob::new(bytes)))
            .build()
            .context("Failed to build Bedrock image block")
    }

    fn to_bedrock_messages(messages: &[JMessage], allow_images: bool) -> Result<Vec<Message>> {
        messages
            .iter()
            .filter_map(|msg| {
                let role = match msg.role {
                    JRole::User => ConversationRole::User,
                    JRole::Assistant => ConversationRole::Assistant,
                };
                let mut content = Vec::new();
                for block in &msg.content {
                    match block {
                        JContentBlock::Text { text, .. } => {
                            content.push(ContentBlock::Text(text.clone()))
                        }
                        JContentBlock::Image { media_type, data } => {
                            if !allow_images {
                                return Some(Err(anyhow::anyhow!(
                                    "Current Bedrock model does not advertise image input support"
                                )));
                            }
                            match Self::image_block(media_type, data) {
                                Ok(image) => content.push(ContentBlock::Image(image)),
                                Err(err) => return Some(Err(err)),
                            }
                        }
                        JContentBlock::ToolResult {
                            tool_use_id,
                            content: text,
                            is_error,
                        } => {
                            let status = if is_error.unwrap_or(false) {
                                aws_sdk_bedrockruntime::types::ToolResultStatus::Error
                            } else {
                                aws_sdk_bedrockruntime::types::ToolResultStatus::Success
                            };
                            let result =
                                match aws_sdk_bedrockruntime::types::ToolResultBlock::builder()
                                    .tool_use_id(tool_use_id)
                                    .status(status)
                                    .content(
                                        aws_sdk_bedrockruntime::types::ToolResultContentBlock::Text(
                                            text.clone(),
                                        ),
                                    )
                                    .build()
                                {
                                    Ok(result) => result,
                                    Err(err) => return Some(Err(anyhow::anyhow!(err))),
                                };
                            content.push(ContentBlock::ToolResult(result));
                        }
                        JContentBlock::ToolUse { id, name, input } => {
                            let tool_use =
                                match aws_sdk_bedrockruntime::types::ToolUseBlock::builder()
                                    .tool_use_id(id)
                                    .name(name)
                                    .input(Self::json_to_document(input))
                                    .build()
                                {
                                    Ok(tool_use) => tool_use,
                                    Err(err) => return Some(Err(anyhow::anyhow!(err))),
                                };
                            content.push(ContentBlock::ToolUse(tool_use));
                        }
                        _ => {}
                    }
                }
                if content.is_empty() {
                    return None;
                }
                Some(
                    Message::builder()
                        .role(role)
                        .set_content(Some(content))
                        .build()
                        .map_err(|err| anyhow::anyhow!(err)),
                )
            })
            .collect()
    }

    fn tool_config(tools: &[ToolDefinition]) -> Option<ToolConfiguration> {
        if tools.is_empty() {
            return None;
        }
        let bedrock_tools = tools
            .iter()
            .filter_map(|tool| {
                let schema = ToolInputSchema::Json(Self::json_to_document(&tool.input_schema));
                ToolSpecification::builder()
                    .name(&tool.name)
                    .description(tool.description.clone())
                    .input_schema(schema)
                    .build()
                    .ok()
                    .map(Tool::ToolSpec)
            })
            .collect::<Vec<_>>();
        if bedrock_tools.is_empty() {
            None
        } else {
            ToolConfiguration::builder()
                .set_tools(Some(bedrock_tools))
                .build()
                .ok()
        }
    }

    fn inference_config() -> Option<InferenceConfiguration> {
        let max_tokens = std::env::var("JCODE_BEDROCK_MAX_TOKENS")
            .ok()
            .and_then(|v| v.trim().parse::<i32>().ok())
            .filter(|v| *v > 0);
        let temperature = std::env::var("JCODE_BEDROCK_TEMPERATURE")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|v| (0.0..=1.0).contains(v));
        let top_p = std::env::var("JCODE_BEDROCK_TOP_P")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|v| (0.0..=1.0).contains(v));
        let stop_sequences = std::env::var("JCODE_BEDROCK_STOP_SEQUENCES")
            .ok()
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());
        if max_tokens.is_none()
            && temperature.is_none()
            && top_p.is_none()
            && stop_sequences.is_none()
        {
            return None;
        }
        Some(
            InferenceConfiguration::builder()
                .set_max_tokens(max_tokens)
                .set_temperature(temperature)
                .set_top_p(top_p)
                .set_stop_sequences(stop_sequences)
                .build(),
        )
    }

    fn normalize_model_id(model: &str) -> String {
        let mut value = model.trim().to_string();
        if let Some((_, tail)) = value.rsplit_once('/') {
            value = tail.to_string();
        }
        for prefix in ["us.", "eu.", "apac."] {
            if let Some(stripped) = value.strip_prefix(prefix) {
                value = stripped.to_string();
                break;
            }
        }
        value
    }

    fn model_info(model: &str) -> BedrockModelInfo {
        let id = Self::normalize_model_id(model).to_ascii_lowercase();
        if id.contains("claude-opus-4") || id.contains("claude-sonnet-4") {
            BedrockModelInfo {
                context_tokens: 200_000,
                max_output_tokens: 64_000,
                supports_tools: true,
                supports_vision: true,
                supports_reasoning: true,
                pricing: Some((3_000_000, 15_000_000)),
            }
        } else if id.contains("claude-3-7-sonnet") || id.contains("claude-3-5-sonnet") {
            BedrockModelInfo {
                context_tokens: 200_000,
                max_output_tokens: 8_192,
                supports_tools: true,
                supports_vision: true,
                supports_reasoning: id.contains("3-7"),
                pricing: Some((3_000_000, 15_000_000)),
            }
        } else if id.contains("claude-3-5-haiku") || id.contains("claude-3-haiku") {
            BedrockModelInfo {
                context_tokens: 200_000,
                max_output_tokens: 8_192,
                supports_tools: true,
                supports_vision: true,
                supports_reasoning: false,
                pricing: Some((800_000, 4_000_000)),
            }
        } else if id.contains("amazon.nova-pro") {
            BedrockModelInfo {
                context_tokens: 300_000,
                max_output_tokens: 5_120,
                supports_tools: true,
                supports_vision: true,
                supports_reasoning: false,
                pricing: Some((800_000, 3_200_000)),
            }
        } else if id.contains("amazon.nova-lite") {
            BedrockModelInfo {
                context_tokens: 300_000,
                max_output_tokens: 5_120,
                supports_tools: true,
                supports_vision: true,
                supports_reasoning: false,
                pricing: Some((60_000, 240_000)),
            }
        } else if id.contains("amazon.nova-micro") {
            BedrockModelInfo {
                context_tokens: 128_000,
                max_output_tokens: 5_120,
                supports_tools: true,
                supports_vision: false,
                supports_reasoning: false,
                pricing: Some((35_000, 140_000)),
            }
        } else if id.contains("llama3-1-405b") {
            BedrockModelInfo {
                context_tokens: 128_000,
                max_output_tokens: 4_096,
                supports_tools: false,
                supports_vision: false,
                supports_reasoning: false,
                pricing: Some((5_320_000, 16_000_000)),
            }
        } else if id.contains("mistral-large") {
            BedrockModelInfo {
                context_tokens: 128_000,
                max_output_tokens: 8_192,
                supports_tools: true,
                supports_vision: false,
                supports_reasoning: false,
                pricing: Some((4_000_000, 12_000_000)),
            }
        } else {
            BedrockModelInfo {
                context_tokens: DEFAULT_CONTEXT_LIMIT,
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                supports_tools: true,
                supports_vision: false,
                supports_reasoning: false,
                pricing: None,
            }
        }
    }

    fn route_pricing(model: &str) -> Option<RouteCheapnessEstimate> {
        let info = Self::model_info(model);
        info.pricing.map(|(input, output)| {
            RouteCheapnessEstimate::metered(
                RouteCostSource::Heuristic,
                RouteCostConfidence::Medium,
                input,
                output,
                None,
                Some("AWS Bedrock public on-demand pricing heuristic; verify for your region/account".to_string()),
            )
        })
    }

    fn known_models() -> Vec<&'static str> {
        vec![
            "anthropic.claude-3-5-sonnet-20241022-v2:0",
            "anthropic.claude-3-5-haiku-20241022-v1:0",
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
            "anthropic.claude-sonnet-4-20250514-v1:0",
            "anthropic.claude-opus-4-20250514-v1:0",
            "amazon.nova-pro-v1:0",
            "amazon.nova-lite-v1:0",
            "amazon.nova-micro-v1:0",
            "meta.llama3-1-405b-instruct-v1:0",
            "mistral.mistral-large-2407-v1:0",
        ]
    }

    fn all_display_models(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut models = Vec::new();
        for model in Self::known_models().into_iter().map(str::to_string) {
            if seen.insert(model.clone()) {
                models.push(model);
            }
        }
        if let Ok(fetched) = self.fetched_models.read() {
            for model in fetched.iter() {
                if seen.insert(model.clone()) {
                    models.push(model.clone());
                }
            }
        }
        if let Ok(profiles) = self.fetched_inference_profiles.read() {
            for profile in profiles.iter() {
                if seen.insert(profile.clone()) {
                    models.push(profile.clone());
                }
            }
        }
        models
    }

    async fn refresh_catalog(&self) -> Result<(Vec<String>, Vec<String>)> {
        let client = Self::control_client().await;
        let mut models = Vec::new();
        let model_resp = client
            .list_foundation_models()
            .send()
            .await
            .map_err(|err| anyhow::anyhow!(Self::classify_error_message(&err.to_string())))?;
        for summary in model_resp.model_summaries() {
            let model_id = summary.model_id();
            if !model_id.is_empty() {
                models.push(model_id.to_string());
            }
        }
        models.sort();
        models.dedup();

        let mut profiles = Vec::new();
        match client.list_inference_profiles().send().await {
            Ok(resp) => {
                for summary in resp.inference_profile_summaries() {
                    let id = summary.inference_profile_id();
                    if !id.is_empty() {
                        profiles.push(id.to_string());
                    }
                    let arn = summary.inference_profile_arn();
                    if !arn.is_empty() {
                        profiles.push(arn.to_string());
                    }
                }
                profiles.sort();
                profiles.dedup();
            }
            Err(err) => {
                crate::logging::info(&format!(
                    "Bedrock inference profile discovery skipped: {}",
                    Self::classify_error_message(&err.to_string())
                ));
            }
        }

        if let Ok(mut guard) = self.fetched_models.write() {
            *guard = models.clone();
        }
        if let Ok(mut guard) = self.fetched_inference_profiles.write() {
            *guard = profiles.clone();
        }
        Self::persist_catalog(&models, &profiles);
        Ok((models, profiles))
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    async fn complete(
        &self,
        messages: &[JMessage],
        tools: &[ToolDefinition],
        system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Self::validate_credentials_if_requested().await?;
        let model = self.model();
        let info = Self::model_info(&model);
        let request_messages = Self::to_bedrock_messages(messages, info.supports_vision)?;
        let tool_config = if info.supports_tools {
            Self::tool_config(tools)
        } else {
            None
        };
        let inference_config = Self::inference_config();
        let system_blocks = if system.trim().is_empty() {
            None
        } else {
            Some(vec![SystemContentBlock::Text(system.to_string())])
        };
        let (tx, rx) = mpsc::channel::<Result<StreamEvent>>(64);
        tokio::spawn(async move {
            let client = Self::runtime_client().await;
            let mut req = client
                .converse_stream()
                .model_id(model.clone())
                .set_messages(Some(request_messages));
            if let Some(system_blocks) = system_blocks {
                req = req.set_system(Some(system_blocks));
            }
            if let Some(tool_config) = tool_config {
                req = req.tool_config(tool_config);
            }
            if let Some(inference_config) = inference_config {
                req = req.inference_config(inference_config);
            }
            let resp = match req.send().await {
                Ok(resp) => resp,
                Err(err) => {
                    let _ = tx
                        .send(Err(anyhow::anyhow!(Self::classify_error_message(
                            &err.to_string()
                        ))))
                        .await;
                    return;
                }
            };
            let mut stream = resp.stream;
            let mut current_tool: Option<(String, String, String)> = None;
            loop {
                match stream.recv().await {
                    Ok(Some(event)) => match event {
                        ConverseStreamOutput::ContentBlockStart(start) => {
                            if let Some(ContentBlockStart::ToolUse(tool)) = start.start {
                                let id = tool.tool_use_id().to_string();
                                let name = tool.name().to_string();
                                current_tool = Some((id.clone(), name.clone(), String::new()));
                                let _ = tx.send(Ok(StreamEvent::ToolUseStart { id, name })).await;
                            }
                        }
                        ConverseStreamOutput::ContentBlockDelta(delta) => {
                            if let Some(d) = delta.delta {
                                match d {
                                    ContentBlockDelta::Text(text) => {
                                        let _ = tx.send(Ok(StreamEvent::TextDelta(text))).await;
                                    }
                                    ContentBlockDelta::ToolUse(tool_delta) => {
                                        let input = tool_delta.input();
                                        if !input.is_empty() {
                                            if let Some((_, _, buf)) = current_tool.as_mut() {
                                                buf.push_str(input);
                                            }
                                            let _ = tx
                                                .send(Ok(StreamEvent::ToolInputDelta(
                                                    input.to_string(),
                                                )))
                                                .await;
                                        }
                                    }
                                    ContentBlockDelta::ReasoningContent(reasoning) => {
                                        if let ReasoningContentBlockDelta::Text(text) = reasoning {
                                            let _ =
                                                tx.send(Ok(StreamEvent::ThinkingDelta(text))).await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        ConverseStreamOutput::ContentBlockStop(_) => {
                            if current_tool.take().is_some() {
                                let _ = tx.send(Ok(StreamEvent::ToolUseEnd)).await;
                            }
                        }
                        ConverseStreamOutput::MessageStop(stop) => {
                            let reason = Some(format!("{:?}", stop.stop_reason()));
                            let _ = tx
                                .send(Ok(StreamEvent::MessageEnd {
                                    stop_reason: reason,
                                }))
                                .await;
                        }
                        ConverseStreamOutput::Metadata(meta) => {
                            if let Some(usage) = meta.usage() {
                                let _ = tx
                                    .send(Ok(StreamEvent::TokenUsage {
                                        input_tokens: Some(usage.input_tokens() as u64),
                                        output_tokens: Some(usage.output_tokens() as u64),
                                        cache_read_input_tokens: None,
                                        cache_creation_input_tokens: None,
                                    }))
                                    .await;
                            }
                        }
                        _ => {}
                    },
                    Ok(None) => break,
                    Err(err) => {
                        let _ = tx
                            .send(Err(anyhow::anyhow!(Self::classify_error_message(
                                &err.to_string()
                            ))))
                            .await;
                        break;
                    }
                }
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx))
            as Pin<
                Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>,
            >)
    }

    fn name(&self) -> &str {
        "bedrock"
    }

    fn model(&self) -> String {
        self.model.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn supports_image_input(&self) -> bool {
        Self::model_info(&self.model()).supports_vision
    }

    fn set_model(&self, model: &str) -> Result<()> {
        *self.model.write().unwrap_or_else(|p| p.into_inner()) = model.to_string();
        Ok(())
    }

    fn available_models(&self) -> Vec<&'static str> {
        Self::known_models()
    }

    fn available_models_display(&self) -> Vec<String> {
        self.all_display_models()
    }

    fn available_models_for_switching(&self) -> Vec<String> {
        self.all_display_models()
    }

    fn model_routes(&self) -> Vec<ModelRoute> {
        self.all_display_models()
            .into_iter()
            .map(|model| {
                let info = Self::model_info(&model);
                let mut features = Vec::new();
                if info.supports_tools {
                    features.push("tools");
                }
                if info.supports_vision {
                    features.push("vision");
                }
                if info.supports_reasoning {
                    features.push("reasoning");
                }
                ModelRoute {
                    model: model.clone(),
                    provider: "AWS Bedrock".to_string(),
                    api_method: "bedrock".to_string(),
                    available: true,
                    detail: format!(
                        "ConverseStream · context ~{} tokens · max output ~{} · {}",
                        info.context_tokens,
                        info.max_output_tokens,
                        if features.is_empty() {
                            "text".to_string()
                        } else {
                            features.join(", ")
                        }
                    ),
                    cheapness: Self::route_pricing(&model),
                }
            })
            .collect()
    }

    async fn prefetch_models(&self) -> Result<()> {
        self.refresh_catalog().await.map(|_| ())
    }

    async fn refresh_model_catalog(&self) -> Result<ModelCatalogRefreshSummary> {
        let before_models = self.available_models_display();
        let before_routes = self.model_routes();
        self.refresh_catalog().await?;
        let after_models = self.available_models_display();
        let after_routes = self.model_routes();
        Ok(summarize_model_catalog_refresh(
            before_models,
            after_models,
            before_routes,
            after_routes,
        ))
    }

    fn context_window(&self) -> usize {
        Self::model_info(&self.model()).context_tokens
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn uses_jcode_compaction(&self) -> bool {
        true
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            model: Arc::new(RwLock::new(self.model())),
            fetched_models: self.fetched_models.clone(),
            fetched_inference_profiles: self.fetched_inference_profiles.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_env_credentials_requires_region_and_credential_hint() {
        let _guard = crate::storage::lock_test_env();
        crate::env::remove_var("JCODE_BEDROCK_ENABLE");
        crate::env::remove_var("AWS_PROFILE");
        crate::env::remove_var("JCODE_BEDROCK_PROFILE");
        crate::env::set_var("JCODE_BEDROCK_REGION", "us-east-1");
        assert!(!BedrockProvider::has_credentials());
        crate::env::set_var("AWS_PROFILE", "test");
        assert!(BedrockProvider::has_credentials());
        crate::env::remove_var("JCODE_BEDROCK_REGION");
        crate::env::remove_var("AWS_PROFILE");
    }

    #[test]
    fn explicit_enable_marks_configured_for_instance_metadata_credentials() {
        let _guard = crate::storage::lock_test_env();
        crate::env::set_var("JCODE_BEDROCK_ENABLE", "1");
        assert!(BedrockProvider::has_credentials());
        crate::env::remove_var("JCODE_BEDROCK_ENABLE");
    }

    #[test]
    fn switches_arbitrary_model_ids() {
        let p = BedrockProvider::new();
        p.set_model("us.anthropic.claude-3-5-sonnet-20241022-v2:0")
            .unwrap();
        assert_eq!(p.model(), "us.anthropic.claude-3-5-sonnet-20241022-v2:0");
    }

    #[test]
    fn known_context_and_vision_capabilities() {
        let p = BedrockProvider::new();
        p.set_model("anthropic.claude-3-5-sonnet-20241022-v2:0")
            .unwrap();
        assert!(p.supports_image_input());
        assert_eq!(p.context_window(), 200_000);
        p.set_model("amazon.nova-micro-v1:0").unwrap();
        assert!(!p.supports_image_input());
        assert_eq!(p.context_window(), 128_000);
    }

    #[test]
    fn error_classification_mentions_model_access() {
        let message = BedrockProvider::classify_error_message(
            "ValidationException: The provided model identifier is invalid",
        );
        assert!(message.contains("model"));
        assert!(message.contains("region"));
    }

    #[tokio::test]
    #[ignore = "requires AWS credentials and enabled Bedrock model access"]
    async fn bedrock_live_smoke_test() {
        if std::env::var("JCODE_BEDROCK_LIVE_TEST").ok().as_deref() != Some("1") {
            return;
        }
        let provider = BedrockProvider::new();
        let output = provider
            .complete_simple("say bedrock ok and nothing else", "")
            .await
            .expect("live Bedrock completion");
        assert!(output.to_ascii_lowercase().contains("bedrock ok"));
    }
}
