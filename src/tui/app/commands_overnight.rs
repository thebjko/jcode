use super::{App, DisplayMessage};
use crate::overnight::{OvernightCommand, OvernightStartOptions};
use crate::provider::Provider;
use std::sync::Arc;

pub(super) fn handle_overnight_command(app: &mut App, trimmed: &str) -> bool {
    let Some(command) = crate::overnight::parse_overnight_command(trimmed) else {
        return false;
    };

    match command {
        Ok(OvernightCommand::Help) => show_overnight_help(app),
        Ok(OvernightCommand::Status) => show_overnight_status(app),
        Ok(OvernightCommand::Log) => show_overnight_log(app),
        Ok(OvernightCommand::Review) => open_overnight_review(app),
        Ok(OvernightCommand::Cancel) => cancel_overnight(app),
        Ok(OvernightCommand::Start { duration, mission }) => {
            let working_dir = app
                .session
                .working_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_dir())
                .or_else(|| std::env::current_dir().ok());
            let provider = overnight_provider_for_app(app);
            let options = OvernightStartOptions {
                duration,
                mission,
                parent_session: app.session.clone(),
                provider,
                registry: app.registry.clone(),
                working_dir,
                use_current_session: true,
            };
            match crate::overnight::start_overnight_run(options) {
                Ok(launch) => {
                    let manifest = launch.manifest;
                    app.upsert_overnight_display_card(&manifest);
                    app.set_status_notice("Overnight started");
                }
                Err(error) => app.push_display_message(DisplayMessage::error(format!(
                    "Failed to start overnight run: {}",
                    crate::util::format_error_chain(&error)
                ))),
            }
        }
        Err(error) => app.push_display_message(DisplayMessage::error(error)),
    }

    true
}

fn show_overnight_help(app: &mut App) {
    app.push_display_message(DisplayMessage::system(
        "`/overnight <hours>[h|m] [mission]`\nStart one overnight coordinator with a target wake/report time. The coordinator prioritizes verifiable, low-risk work, maintains logs, and updates a review HTML page.\n\n`/overnight status`\nShow the latest overnight run status.\n\n`/overnight log`\nShow recent overnight events.\n\n`/overnight review`\nOpen the generated review page.\n\n`/overnight cancel`\nRequest cancellation after the current coordinator turn.".to_string(),
    ));
}

fn overnight_provider_for_app(app: &mut App) -> Arc<dyn Provider> {
    if !app.is_remote {
        return app.provider.fork();
    }

    // Remote-attached TUIs intentionally use NullProvider because normal turns
    // execute in the remote backend process. `/overnight` is supervised by the
    // launching TUI process, so it needs a real local provider instead of the
    // remote placeholder. Restore the displayed session model when possible and
    // otherwise fall back to the local default provider.
    let provider: Arc<dyn Provider> = Arc::new(crate::provider::MultiProvider::new_fast());
    if let Some(model) = app
        .session
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty() && *model != "unknown")
        && let Err(error) = provider.set_model(model)
    {
        app.push_display_message(DisplayMessage::system(format!(
            "Overnight could not restore remote model `{}` locally: {}. Using local default provider `{}` instead.",
            model,
            error,
            provider.name()
        )));
    }
    provider
}

fn show_overnight_status(app: &mut App) {
    match crate::overnight::latest_manifest() {
        Ok(Some(manifest)) => {
            if !app.upsert_overnight_display_card(&manifest) {
                app.push_display_message(DisplayMessage::system(
                    crate::overnight::format_status_markdown(&manifest),
                ));
            }
            app.set_status_notice("Overnight status");
        }
        Ok(None) => app.push_display_message(DisplayMessage::system(
            "No overnight runs found.".to_string(),
        )),
        Err(error) => app.push_display_message(DisplayMessage::error(format!(
            "Failed to read overnight status: {}",
            crate::util::format_error_chain(&error)
        ))),
    }
}

fn show_overnight_log(app: &mut App) {
    match crate::overnight::latest_manifest() {
        Ok(Some(manifest)) => {
            app.push_display_message(DisplayMessage::system(
                crate::overnight::format_log_markdown(&manifest, 30),
            ));
            app.set_status_notice("Overnight log");
        }
        Ok(None) => app.push_display_message(DisplayMessage::system(
            "No overnight runs found.".to_string(),
        )),
        Err(error) => app.push_display_message(DisplayMessage::error(format!(
            "Failed to read overnight log: {}",
            crate::util::format_error_chain(&error)
        ))),
    }
}

fn open_overnight_review(app: &mut App) {
    match crate::overnight::latest_manifest() {
        Ok(Some(manifest)) => {
            if let Err(error) = crate::overnight::render_review_html(&manifest) {
                app.push_display_message(DisplayMessage::error(format!(
                    "Failed to refresh overnight review page: {}",
                    crate::util::format_error_chain(&error)
                )));
                return;
            }
            match open::that_detached(&manifest.review_path) {
                Ok(()) => {
                    app.push_display_message(DisplayMessage::system(format!(
                        "Opened overnight review page: `{}`",
                        manifest.review_path.display()
                    )));
                    app.set_status_notice("Overnight review opened");
                }
                Err(error) => app.push_display_message(DisplayMessage::error(format!(
                    "Failed to open overnight review page `{}`: {}",
                    manifest.review_path.display(),
                    error
                ))),
            }
        }
        Ok(None) => app.push_display_message(DisplayMessage::system(
            "No overnight runs found.".to_string(),
        )),
        Err(error) => app.push_display_message(DisplayMessage::error(format!(
            "Failed to read overnight review: {}",
            crate::util::format_error_chain(&error)
        ))),
    }
}

fn cancel_overnight(app: &mut App) {
    match crate::overnight::cancel_latest_run() {
        Ok(manifest) => {
            if !app.upsert_overnight_display_card(&manifest) {
                app.push_display_message(DisplayMessage::system(format!(
                    "Cancellation requested for overnight run `{}`. The coordinator will stop after the current turn reaches a safe boundary.",
                    manifest.run_id,
                )));
            }
            app.set_status_notice("Overnight cancel requested");
        }
        Err(error) => app.push_display_message(DisplayMessage::error(format!(
            "Failed to cancel overnight run: {}",
            crate::util::format_error_chain(&error)
        ))),
    }
}
