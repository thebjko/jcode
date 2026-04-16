use super::*;

impl ReloadContext {
    fn sanitize_session_id(session_id: &str) -> String {
        session_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect()
    }

    pub fn path_for_session(session_id: &str) -> Result<std::path::PathBuf> {
        let sanitized = Self::sanitize_session_id(session_id);
        Ok(storage::jcode_dir()?.join(format!("reload-context-{}.json", sanitized)))
    }

    fn legacy_path() -> Result<std::path::PathBuf> {
        Ok(storage::jcode_dir()?.join("reload-context.json"))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path_for_session(&self.session_id)?;
        storage::write_json(&path, self)?;
        Ok(())
    }

    pub fn load() -> Result<Option<Self>> {
        let legacy = Self::legacy_path()?;
        if !legacy.exists() {
            return Ok(None);
        }
        let ctx: Self = storage::read_json(&legacy)?;
        let _ = std::fs::remove_file(&legacy);
        Ok(Some(ctx))
    }

    /// Peek at context for a specific session without consuming it.
    pub fn peek_for_session(session_id: &str) -> Result<Option<Self>> {
        let session_path = Self::path_for_session(session_id)?;
        if session_path.exists() {
            let ctx: Self = storage::read_json(&session_path)?;
            return Ok(Some(ctx));
        }

        let legacy = Self::legacy_path()?;
        if !legacy.exists() {
            return Ok(None);
        }

        let ctx: Self = storage::read_json(&legacy)?;
        if ctx.session_id == session_id {
            Ok(Some(ctx))
        } else {
            Ok(None)
        }
    }

    /// Load context only if it belongs to the given session; consumes on success.
    pub fn load_for_session(session_id: &str) -> Result<Option<Self>> {
        let session_path = Self::path_for_session(session_id)?;
        if session_path.exists() {
            let ctx: Self = storage::read_json(&session_path)?;
            let _ = std::fs::remove_file(&session_path);
            return Ok(Some(ctx));
        }

        let legacy = Self::legacy_path()?;
        if !legacy.exists() {
            return Ok(None);
        }

        let ctx: Self = storage::read_json(&legacy)?;
        if ctx.session_id == session_id {
            let _ = std::fs::remove_file(&legacy);
            Ok(Some(ctx))
        } else {
            Ok(None)
        }
    }
}

impl SelfDevTool {
    pub(super) async fn do_reload(
        &self,
        context: Option<String>,
        session_id: &str,
        execution_mode: ToolExecutionMode,
    ) -> Result<ToolOutput> {
        let repo_dir = build::get_repo_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not find jcode repository directory"))?;

        let target_binary = build::find_dev_binary(&repo_dir)
            .unwrap_or_else(|| build::release_binary_path(&repo_dir));
        if !target_binary.exists() {
            return Ok(ToolOutput::new(
                format!(
                    "No binary found at {}.\n\
                     Run 'jcode self-dev --build' first, or build with 'scripts/dev_cargo.sh build --profile selfdev -p jcode --bin jcode' and then try reload again.",
                    target_binary.display()
                )
                .to_string(),
            ));
        }

        let source = if SelfDevTool::is_test_session() {
            build::SourceState {
                repo_scope: "test-repo-scope".to_string(),
                worktree_scope: "test-worktree-scope".to_string(),
                short_hash: "test-reload-hash".to_string(),
                full_hash: "test-reload-hash-full".to_string(),
                dirty: true,
                fingerprint: "test-reload-fingerprint".to_string(),
                version_label: "test-reload-hash".to_string(),
                changed_paths: 0,
            }
        } else {
            build::current_source_state(&repo_dir)?
        };
        let hash = source.version_label.clone();
        let version_before = env!("JCODE_VERSION").to_string();
        let published = if SelfDevTool::is_test_session() {
            None
        } else {
            Some(build::publish_local_current_build_for_source(
                &repo_dir, &source,
            )?)
        };

        // Update manifest - track what we're testing
        let mut manifest = build::BuildManifest::load()?;
        manifest.canary = Some(hash.clone());
        manifest.canary_status = Some(build::CanaryStatus::Testing);
        manifest.set_pending_activation(build::PendingActivation {
            session_id: session_id.to_string(),
            new_version: hash.clone(),
            previous_current_version: published
                .as_ref()
                .and_then(|published| published.previous_current_version.clone()),
            source_fingerprint: Some(source.fingerprint.clone()),
            requested_at: chrono::Utc::now(),
        })?;
        manifest.save()?;

        // Save reload context for continuation after restart
        let reload_ctx = ReloadContext {
            task_context: context,
            version_before,
            version_after: hash.clone(),
            session_id: session_id.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        crate::logging::info(&format!(
            "Saving reload context to {:?}",
            ReloadContext::path_for_session(session_id)
        ));
        if let Err(e) = reload_ctx.save() {
            crate::logging::error(&format!("Failed to save reload context: {}", e));
            return Err(e);
        }
        crate::logging::info("Reload context saved successfully");

        // Write reload info for post-restart display
        let info_path = crate::storage::jcode_dir()?.join("reload-info");
        let info = format!("reload:{}", hash);
        std::fs::write(&info_path, &info)?;

        // Signal the server via in-process channel (replaces filesystem-based rebuild-signal)
        let request_id =
            server::send_reload_signal(hash.clone(), Some(session_id.to_string()), true);
        crate::logging::info(&format!(
            "selfdev reload: request={} session_id={} hash={} execution_mode={:?}",
            request_id, session_id, hash, execution_mode
        ));

        let timeout = std::time::Duration::from_secs(SelfDevTool::reload_timeout_secs());
        let ack_wait_started = std::time::Instant::now();
        let ack = server::wait_for_reload_ack(&request_id, timeout)
            .await
            .map_err(|error| {
                let _ = build::rollback_pending_activation_for_session(session_id);
                anyhow::anyhow!(
                    "Timed out waiting for the server to begin reload after {}s: {}. The reload signal may not have been picked up; check that the connected server is running a build with unified self-dev reload support and try restarting the shared server.",
                    timeout.as_secs(),
                    error
                )
            })?;

        crate::logging::info(&format!(
            "selfdev reload: acked request={} hash={} after {}ms state={}",
            ack.request_id,
            ack.hash,
            ack_wait_started.elapsed().as_millis(),
            server::reload_state_summary(std::time::Duration::from_secs(60))
        ));

        match execution_mode {
            ToolExecutionMode::Direct => {
                if SelfDevTool::is_test_session() {
                    return Ok(ToolOutput::new(format!(
                        "Reload acknowledged for build {}. Server is restarting now.",
                        ack.hash
                    )));
                }
                match server::await_reload_handoff(&server::socket_path(), timeout).await {
                    server::ReloadWaitStatus::Ready => {
                        let _ = build::complete_pending_activation_for_session(session_id);
                        Ok(ToolOutput::new(format!(
                            "Reload completed successfully for build {}. Server reported ready.",
                            ack.hash
                        )))
                    }
                    server::ReloadWaitStatus::Failed(detail) => {
                        let _ = build::rollback_pending_activation_for_session(session_id);
                        Err(anyhow::anyhow!(
                            "Reload was acknowledged for build {}, but the replacement server failed before becoming ready: {}",
                            ack.hash,
                            detail.unwrap_or_else(|| "unknown reload failure".to_string())
                        ))
                    }
                    server::ReloadWaitStatus::Idle | server::ReloadWaitStatus::Waiting { .. } => {
                        let _ = build::rollback_pending_activation_for_session(session_id);
                        Err(anyhow::anyhow!(
                            "Reload was acknowledged for build {}, but readiness could not be confirmed within {}s.",
                            ack.hash,
                            timeout.as_secs()
                        ))
                    }
                }
            }
            ToolExecutionMode::AgentTurn => {
                let sleep_forever = async {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    }
                };

                match tokio::time::timeout(timeout, sleep_forever).await {
                    Ok(_) => unreachable!("infinite wait future unexpectedly completed"),
                    Err(_) => {
                        crate::logging::warn(&format!(
                            "selfdev reload: request={} not interrupted after {}ms state={} ",
                            ack.request_id,
                            timeout.as_millis(),
                            server::reload_state_summary(std::time::Duration::from_secs(60))
                        ));
                        Err(anyhow::anyhow!(
                            "Reload was acknowledged by the server for build {}, but this tool execution was not interrupted within {}s. The server restart may be stuck; inspect logs and active sessions. Current reload state: {}",
                            ack.hash,
                            timeout.as_secs(),
                            server::reload_state_summary(std::time::Duration::from_secs(60))
                        ))
                    }
                }
            }
        }
    }
}
