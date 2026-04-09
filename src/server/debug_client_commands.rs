#[allow(dead_code)]
pub(super) fn execute_client_debug_command(command: &str) -> String {
    use crate::tui::{markdown, mermaid, visual_debug};

    let trimmed = command.trim();

    if trimmed == "frame" || trimmed == "screen-json" {
        visual_debug::enable();
        return visual_debug::latest_frame_json().unwrap_or_else(|| {
            "No frames captured yet. Try again after some UI activity.".to_string()
        });
    }

    if trimmed == "frame-normalized" || trimmed == "screen-json-normalized" {
        visual_debug::enable();
        return visual_debug::latest_frame_json_normalized()
            .unwrap_or_else(|| "No frames captured yet.".to_string());
    }

    if trimmed == "screen" {
        visual_debug::enable();
        match visual_debug::dump_to_file() {
            Ok(path) => return format!("Frames written to: {}", path.display()),
            Err(e) => return format!("Error dumping frames: {}", e),
        }
    }

    if trimmed == "enable" || trimmed == "debug-enable" {
        visual_debug::enable();
        return "Visual debugging enabled.".to_string();
    }

    if trimmed == "disable" || trimmed == "debug-disable" {
        visual_debug::disable();
        return "Visual debugging disabled.".to_string();
    }

    if trimmed == "status" {
        let enabled = visual_debug::is_enabled();
        let overlay = visual_debug::overlay_enabled();
        return serde_json::json!({
            "visual_debug_enabled": enabled,
            "visual_debug_overlay": overlay,
        })
        .to_string();
    }

    if trimmed == "overlay" || trimmed == "overlay:status" {
        let overlay = visual_debug::overlay_enabled();
        return serde_json::json!({
            "visual_debug_overlay": overlay,
        })
        .to_string();
    }

    if trimmed == "overlay:on" || trimmed == "overlay:enable" {
        visual_debug::set_overlay(true);
        return "Visual debug overlay enabled.".to_string();
    }

    if trimmed == "overlay:off" || trimmed == "overlay:disable" {
        visual_debug::set_overlay(false);
        return "Visual debug overlay disabled.".to_string();
    }

    if trimmed == "layout" {
        visual_debug::enable();
        return visual_debug::latest_frame().map_or_else(
            || "layout: no frames captured".to_string(),
            |frame| {
                serde_json::to_string_pretty(&serde_json::json!({
                    "frame_id": frame.frame_id,
                    "terminal_size": frame.terminal_size,
                    "layout": frame.layout,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            },
        );
    }

    if trimmed == "margins" {
        visual_debug::enable();
        return visual_debug::latest_frame().map_or_else(
            || "margins: no frames captured".to_string(),
            |frame| {
                serde_json::to_string_pretty(&serde_json::json!({
                    "frame_id": frame.frame_id,
                    "margins": frame.layout.margins,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            },
        );
    }

    if trimmed == "widgets" || trimmed == "info-widgets" {
        visual_debug::enable();
        return visual_debug::latest_frame().map_or_else(
            || "widgets: no frames captured".to_string(),
            |frame| {
                serde_json::to_string_pretty(&serde_json::json!({
                    "frame_id": frame.frame_id,
                    "info_widgets": frame.info_widgets,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            },
        );
    }

    if trimmed == "render-stats" {
        visual_debug::enable();
        return visual_debug::latest_frame().map_or_else(
            || "render-stats: no frames captured".to_string(),
            |frame| {
                serde_json::to_string_pretty(&serde_json::json!({
                    "frame_id": frame.frame_id,
                    "render_timing": frame.render_timing,
                    "render_order": frame.render_order,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            },
        );
    }

    if trimmed == "render-order" {
        visual_debug::enable();
        return visual_debug::latest_frame().map_or_else(
            || "render-order: no frames captured".to_string(),
            |frame| {
                serde_json::to_string_pretty(&frame.render_order)
                    .unwrap_or_else(|_| "[]".to_string())
            },
        );
    }

    if trimmed == "anomalies" {
        visual_debug::enable();
        return visual_debug::latest_frame().map_or_else(
            || "anomalies: no frames captured".to_string(),
            |frame| {
                serde_json::to_string_pretty(&frame.anomalies).unwrap_or_else(|_| "[]".to_string())
            },
        );
    }

    if trimmed == "theme" {
        visual_debug::enable();
        return visual_debug::latest_frame().map_or_else(
            || "theme: no frames captured".to_string(),
            |frame| {
                serde_json::to_string_pretty(&frame.theme).unwrap_or_else(|_| "null".to_string())
            },
        );
    }

    if trimmed == "mermaid:stats" {
        let stats = mermaid::debug_stats();
        return serde_json::to_string_pretty(&stats).unwrap_or_else(|_| "{}".to_string());
    }

    if trimmed == "mermaid:memory" {
        let profile = mermaid::debug_memory_profile();
        return serde_json::to_string_pretty(&profile).unwrap_or_else(|_| "{}".to_string());
    }

    if trimmed == "memory" {
        let payload = serde_json::json!({
            "process": crate::process_memory::snapshot_with_source("client:memory"),
            "markdown": markdown::debug_memory_profile(),
            "mermaid": mermaid::debug_memory_profile(),
            "visual_debug": visual_debug::debug_memory_profile(),
        });
        return serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string());
    }

    if trimmed == "memory-history" {
        let payload = crate::process_memory::history(128);
        return serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "[]".to_string());
    }

    if trimmed == "mermaid:memory-bench" {
        let result = mermaid::debug_memory_benchmark(40);
        return serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
    }

    if let Some(raw_iterations) = trimmed.strip_prefix("mermaid:memory-bench ") {
        let iterations = match raw_iterations.trim().parse::<usize>() {
            Ok(v) => v,
            Err(_) => return "Invalid iterations (expected integer)".to_string(),
        };
        let result = mermaid::debug_memory_benchmark(iterations);
        return serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
    }

    if trimmed == "mermaid:cache" {
        let entries = mermaid::debug_cache();
        return serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string());
    }

    if trimmed == "mermaid:evict" || trimmed == "mermaid:clear-cache" {
        return match mermaid::clear_cache() {
            Ok(_) => "mermaid: cache cleared".to_string(),
            Err(e) => format!("mermaid: cache clear failed: {}", e),
        };
    }

    if trimmed == "mermaid:state" {
        let state = mermaid::debug_image_state();
        return serde_json::to_string_pretty(&state).unwrap_or_else(|_| "[]".to_string());
    }

    if trimmed == "mermaid:test" {
        let result = mermaid::debug_test_render();
        return serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
    }

    if trimmed == "mermaid:scroll" {
        let result = mermaid::debug_test_scroll(None);
        return serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
    }

    if let Some(content) = trimmed.strip_prefix("mermaid:render ") {
        let result = mermaid::debug_render(content);
        return serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
    }

    if let Some(hash_str) = trimmed.strip_prefix("mermaid:stability ") {
        if let Ok(hash) = u64::from_str_radix(hash_str, 16) {
            let result = mermaid::debug_test_resize_stability(hash);
            return serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
        }
        return "Invalid hash (expected hex)".to_string();
    }

    if trimmed == "mermaid:active" {
        let diagrams = mermaid::get_active_diagrams();
        let info: Vec<serde_json::Value> = diagrams
            .iter()
            .map(|diagram| {
                serde_json::json!({
                    "hash": format!("{:016x}", diagram.hash),
                    "width": diagram.width,
                    "height": diagram.height,
                    "label": diagram.label,
                })
            })
            .collect();
        return serde_json::to_string_pretty(&serde_json::json!({
            "count": diagrams.len(),
            "diagrams": info,
        }))
        .unwrap_or_else(|_| "{}".to_string());
    }

    if trimmed == "markdown:stats" {
        let stats = markdown::debug_stats();
        return serde_json::to_string_pretty(&stats).unwrap_or_else(|_| "{}".to_string());
    }

    if trimmed == "markdown:memory" {
        let profile = markdown::debug_memory_profile();
        return serde_json::to_string_pretty(&profile).unwrap_or_else(|_| "{}".to_string());
    }

    if trimmed == "help" {
        return r#"Client debug commands:
  frame / screen-json      - Get latest visual debug frame (JSON)
  frame-normalized         - Get normalized frame (for diffs)
  screen                   - Dump visual debug frames to file
  layout                   - Get latest layout JSON
  margins                  - Get layout margins JSON
  widgets                  - Get info widget summary/placements
  render-stats             - Get render timing + order JSON
  render-order             - Get render order list
  anomalies                - Get latest visual debug anomalies
  theme                    - Get palette snapshot
  mermaid:stats            - Get mermaid render/cache stats
  mermaid:memory           - Mermaid memory profile (RSS + cache estimates)
  mermaid:memory-bench [n] - Run synthetic Mermaid memory benchmark
  mermaid:cache            - List mermaid cache entries
  mermaid:state            - Get image state (resize modes, areas)
  mermaid:test             - Render test diagram, return results
  mermaid:scroll           - Run scroll simulation test
  mermaid:render <content> - Render arbitrary mermaid content
  mermaid:stability <hash> - Test resize mode stability for hash
  mermaid:active           - List active diagrams (for pinned widget)
  mermaid:evict            - Clear mermaid cache
  markdown:stats           - Get markdown render stats
  markdown:memory          - Markdown highlight cache memory estimate
  memory                   - Aggregate client memory profile
  memory-history           - Recent process memory samples
  overlay:on/off/status    - Toggle overlay boxes
  enable                   - Enable visual debug capture
  disable                  - Disable visual debug capture
  status                   - Get client debug status
  help                     - Show this help

Note: Visual debug captures TUI rendering state for debugging UI issues.
Frames are captured automatically when visual debug is enabled."#
            .to_string();
    }

    format!(
        "Unknown client command: {}. Use client:help for available commands.",
        trimmed
    )
}
