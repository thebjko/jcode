use super::cli_common::{build_cli_prompt, run_cli_text_command};
use super::{EventStream, Provider};
use crate::auth::gemini::gemini_cli_command;
use crate::message::{Message, ToolDefinition};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, RwLock};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const DEFAULT_MODEL: &str = "gemini-2.5-pro";
const AVAILABLE_MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.0-flash",
    "gemini-1.5-pro",
    "gemini-1.5-flash",
];

pub struct GeminiCliProvider {
    cli_command: crate::auth::gemini::GeminiCliCommand,
    model: Arc<RwLock<String>>,
}

impl GeminiCliProvider {
    pub fn new() -> Self {
        let cli_command = gemini_cli_command();
        let model = std::env::var("JCODE_GEMINI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        Self {
            cli_command,
            model: Arc::new(RwLock::new(model)),
        }
    }
}

impl Default for GeminiCliProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GeminiCliProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let prompt = build_cli_prompt(system, messages);
        let model = self.model.read().unwrap().clone();
        let cli_command = self.cli_command.clone();
        let cwd = std::env::current_dir().ok();
        let (tx, rx) = mpsc::channel::<Result<crate::message::StreamEvent>>(100);

        tokio::spawn(async move {
            if tx
                .send(Ok(crate::message::StreamEvent::ConnectionType {
                    connection: "cli subprocess".to_string(),
                }))
                .await
                .is_err()
            {
                return;
            }

            let mut cmd = Command::new(&cli_command.program);
            cmd.args(&cli_command.args)
                .arg("-p")
                .arg(prompt)
                .arg("--output-format")
                .arg("text")
                .arg("-m")
                .arg(&model);
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }

            if let Err(e) = run_cli_text_command(cmd, tx.clone(), "Gemini").await {
                let _ = tx.send(Err(e)).await;
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &'static str {
        "gemini"
    }

    fn model(&self) -> String {
        self.model.read().unwrap().clone()
    }

    fn set_model(&self, model: &str) -> Result<()> {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            anyhow::bail!("Gemini model cannot be empty");
        }
        *self.model.write().unwrap() = trimmed.to_string();
        Ok(())
    }

    fn available_models(&self) -> Vec<&'static str> {
        AVAILABLE_MODELS.to_vec()
    }

    fn handles_tools_internally(&self) -> bool {
        true
    }

    fn supports_compaction(&self) -> bool {
        false
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            cli_command: self.cli_command.clone(),
            model: Arc::new(RwLock::new(self.model())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_models_include_gemini_defaults() {
        let provider = GeminiCliProvider::new();
        let models = provider.available_models();
        assert!(models.contains(&"gemini-2.5-pro"));
        assert!(models.contains(&"gemini-2.5-flash"));
    }

    #[test]
    fn set_model_accepts_gemini_models() {
        let provider = GeminiCliProvider::new();
        provider.set_model("gemini-2.5-flash").unwrap();
        assert_eq!(provider.model(), "gemini-2.5-flash");
    }
}
