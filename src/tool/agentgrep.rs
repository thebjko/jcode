use super::{Tool, ToolContext, ToolOutput};
use crate::message::{ContentBlock, ToolCall};
use crate::session::Session;
use crate::storage;
use crate::{logging, util};
use ::agentgrep::cli::{FindArgs, FullRegionMode, GrepArgs, OutlineArgs, SmartArgs};
use ::agentgrep::find::{FindFile, FindResult, run_find};
use ::agentgrep::outline::{OutlineResult, run_outline};
use ::agentgrep::search::{FileMatches, GrepResult, run_grep};
use ::agentgrep::smart_dsl::{Relation, SmartQuery, parse_smart_query};
use ::agentgrep::smart_engine::{SmartFile, SmartRegion, SmartResult, run_smart};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Deserialize)]
struct AgentGrepInput {
    mode: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    terms: Option<Vec<String>>,
    #[serde(default)]
    regex: Option<bool>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(rename = "type", default)]
    file_type: Option<String>,
    #[serde(default)]
    hidden: Option<bool>,
    #[serde(default)]
    no_ignore: Option<bool>,
    #[serde(default)]
    max_files: Option<usize>,
    #[serde(default)]
    max_regions: Option<usize>,
    #[serde(default)]
    full_region: Option<String>,
    #[serde(default)]
    debug_plan: Option<bool>,
    #[serde(default)]
    debug_score: Option<bool>,
    #[serde(default)]
    paths_only: Option<bool>,
}

#[derive(Debug, Serialize, Default)]
struct AgentGrepHarnessContext {
    version: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    known_regions: Vec<AgentGrepKnownRegion>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    known_files: Vec<AgentGrepKnownFile>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    known_symbols: Vec<AgentGrepKnownSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    focus_files: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AgentGrepKnownRegion {
    path: String,
    start_line: usize,
    end_line: usize,
    body_confidence: f32,
    current_version_confidence: f32,
    prune_confidence: f32,
    source_strength: &'static str,
    reasons: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct AgentGrepKnownFile {
    path: String,
    structure_confidence: f32,
    body_confidence: f32,
    current_version_confidence: f32,
    prune_confidence: f32,
    source_strength: &'static str,
    reasons: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct AgentGrepKnownSymbol {
    path: String,
    symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    structure_confidence: f32,
    body_confidence: f32,
    current_version_confidence: f32,
    prune_confidence: f32,
    source_strength: &'static str,
    reasons: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy)]
struct RegionConfidenceProfile {
    body_confidence: f32,
    current_version_confidence: f32,
    prune_confidence: f32,
    source_strength: &'static str,
}

#[derive(Debug, Clone)]
struct PendingTraceRegion {
    path: String,
    kind: Option<&'static str>,
    start_line: usize,
    end_line: usize,
}

#[derive(Debug, Clone)]
struct ToolExposureObservation {
    tool: ToolCall,
    content: String,
    timestamp: Option<DateTime<Utc>>,
    message_index: usize,
}

#[derive(Debug, Clone, Copy)]
struct ExposureDescriptor {
    timestamp: Option<DateTime<Utc>>,
    message_index: usize,
    total_messages: usize,
    compaction_cutoff: Option<usize>,
}

pub struct AgentGrepTool;

impl AgentGrepTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for AgentGrepTool {
    fn name(&self) -> &str {
        "agentgrep"
    }

    fn description(&self) -> &str {
        "Search code."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["mode"],
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["grep", "find", "outline", "trace"],
                    "description": "Mode."
                },
                "query": {
                    "type": "string"
                },
                "file": {
                    "type": "string"
                },
                "terms": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Terms."
                },
                "regex": {
                    "type": "boolean",
                    "description": "Regex."
                },
                "path": {
                    "type": "string",
                    "description": "Root path."
                },
                "glob": {
                    "type": "string",
                    "description": "Glob."
                },
                "type": {
                    "type": "string",
                    "description": "File type."
                },
                "max_files": {
                    "type": "integer",
                    "description": "Max files."
                },
                "max_regions": {
                    "type": "integer",
                    "description": "Max regions."
                },
                "paths_only": {
                    "type": "boolean",
                    "description": "Paths only."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: AgentGrepInput = serde_json::from_value(input)?;
        let context_path = maybe_write_context_json(&params, &ctx)?;
        let request = summarize_agentgrep_request(&params, &ctx, context_path.as_deref());
        let started_at = std::time::Instant::now();
        let outcome = execute_linked_agentgrep(&params, &ctx, context_path.as_deref());
        let elapsed_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

        if let Some(path) = context_path {
            let _ = std::fs::remove_file(path);
        }

        match outcome {
            Ok(output) => {
                if elapsed_ms >= 2_000 {
                    logging::warn(&format!(
                        "agentgrep slow mode={} elapsed_ms={} request={}",
                        params.mode, elapsed_ms, request
                    ));
                }
                Ok(output)
            }
            Err(err) => {
                let detail = err.to_string();
                let detail = util::truncate_str(detail.trim(), 600);
                logging::warn(&format!(
                    "agentgrep failure mode={} elapsed_ms={} request={} error={}",
                    params.mode, elapsed_ms, request, detail
                ));
                Err(anyhow::anyhow!(
                    "agentgrep {} failed after {}ms: {}",
                    params.mode,
                    elapsed_ms,
                    err
                ))
            }
        }
    }
}

fn execute_linked_agentgrep(
    params: &AgentGrepInput,
    ctx: &ToolContext,
    context_json_path: Option<&Path>,
) -> Result<ToolOutput> {
    match params.mode.as_str() {
        "grep" => {
            let args = build_grep_args(params, ctx)?;
            let root = resolve_search_root(ctx, args.path.as_deref());
            let result = run_grep(&root, &args).map_err(anyhow::Error::msg)?;
            Ok(ToolOutput::new(render_grep_output(&result, &args)).with_title("agentgrep grep"))
        }
        "find" => {
            let args = build_find_args(params, ctx)?;
            let root = resolve_search_root(ctx, args.path.as_deref());
            let result = run_find(&root, &args);
            Ok(ToolOutput::new(render_find_output(&result, &args)).with_title("agentgrep find"))
        }
        "outline" => {
            let args = build_outline_args(params, ctx, context_json_path)?;
            let root = resolve_search_root(ctx, args.path.as_deref());
            let result = run_outline(&root, &args).map_err(anyhow::Error::msg)?;
            Ok(ToolOutput::new(render_outline_output(&result)).with_title("agentgrep outline"))
        }
        "trace" | "smart" => {
            let (args, query) = build_smart_args_and_query(params, ctx, context_json_path)?;
            let root = resolve_search_root(ctx, args.path.as_deref());
            let result = run_smart(&root, &query, &args).map_err(anyhow::Error::msg)?;
            Ok(ToolOutput::new(render_smart_output(&result, &args))
                .with_title(format!("agentgrep {}", params.mode)))
        }
        _ => Err(anyhow::anyhow!(
            "Unsupported agentgrep mode: {}. Use grep, find, outline, or trace.",
            params.mode
        )),
    }
}

fn build_grep_args(params: &AgentGrepInput, ctx: &ToolContext) -> Result<GrepArgs> {
    let query = params
        .query
        .clone()
        .ok_or_else(|| anyhow::anyhow!("agentgrep grep requires 'query'"))?;
    Ok(GrepArgs {
        query,
        regex: params.regex.unwrap_or(false),
        file_type: params.file_type.clone(),
        json: false,
        paths_only: params.paths_only.unwrap_or(false),
        hidden: params.hidden.unwrap_or(false),
        no_ignore: params.no_ignore.unwrap_or(false),
        path: resolved_root_string(ctx, params.path.as_deref()),
        glob: normalized_agentgrep_glob_owned(params.glob.as_deref()),
    })
}

fn build_find_args(params: &AgentGrepInput, ctx: &ToolContext) -> Result<FindArgs> {
    let query = params
        .query
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("agentgrep find requires 'query'"))?;
    Ok(FindArgs {
        query_parts: query.split_whitespace().map(ToOwned::to_owned).collect(),
        file_type: params.file_type.clone(),
        json: false,
        paths_only: params.paths_only.unwrap_or(false),
        debug_score: params.debug_score.unwrap_or(false),
        max_files: params.max_files.unwrap_or(10),
        hidden: params.hidden.unwrap_or(false),
        no_ignore: params.no_ignore.unwrap_or(false),
        path: resolved_root_string(ctx, params.path.as_deref()),
        glob: normalized_agentgrep_glob_owned(params.glob.as_deref()),
    })
}

fn build_outline_args(
    params: &AgentGrepInput,
    ctx: &ToolContext,
    context_json_path: Option<&Path>,
) -> Result<OutlineArgs> {
    let file = outline_file_arg(params)?;
    Ok(OutlineArgs {
        file,
        json: false,
        max_items: None,
        path: resolved_root_string(ctx, params.path.as_deref()),
        context_json: context_json_path.map(|path| path.display().to_string()),
    })
}

fn build_smart_args_and_query(
    params: &AgentGrepInput,
    ctx: &ToolContext,
    context_json_path: Option<&Path>,
) -> Result<(SmartArgs, SmartQuery)> {
    let terms = trace_or_smart_terms_owned(params)?;
    let query = parse_smart_query(&terms).map_err(|err| {
        anyhow::anyhow!(
            "{}\n\ntrace queries use a small DSL. Example:\n  agentgrep trace subject:auth_status relation:rendered support:ui",
            err
        )
    })?;

    let args = SmartArgs {
        terms,
        json: false,
        max_files: params.max_files.unwrap_or(5),
        max_regions: params.max_regions.unwrap_or(6),
        full_region: parse_full_region_mode(params.full_region.as_deref())?,
        debug_plan: params.debug_plan.unwrap_or(false),
        debug_score: params.debug_score.unwrap_or(false),
        paths_only: params.paths_only.unwrap_or(false),
        path: resolved_root_string(ctx, params.path.as_deref()),
        file_type: params.file_type.clone(),
        glob: normalized_agentgrep_glob_owned(params.glob.as_deref()),
        hidden: params.hidden.unwrap_or(false),
        no_ignore: params.no_ignore.unwrap_or(false),
        context_json: context_json_path.map(|path| path.display().to_string()),
    };

    Ok((args, query))
}

fn trace_or_smart_terms_owned(params: &AgentGrepInput) -> Result<Vec<String>> {
    if let Some(terms) = params.terms.as_ref().filter(|terms| !terms.is_empty()) {
        return Ok(terms.clone());
    }

    if params.mode == "smart"
        && let Some(query) = params.query.as_deref()
    {
        let split_terms: Vec<String> = query
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if !split_terms.is_empty() {
            return Ok(split_terms);
        }
    }

    let field_hint = if params.mode == "smart" {
        "non-empty 'terms' or 'query'"
    } else {
        "non-empty 'terms'"
    };

    Err(anyhow::anyhow!(
        "agentgrep {} requires {}",
        params.mode,
        field_hint
    ))
}

fn outline_file_arg(params: &AgentGrepInput) -> Result<String> {
    params
        .file
        .clone()
        .or_else(|| params.query.clone())
        .or_else(|| {
            params
                .terms
                .as_ref()
                .and_then(|terms| terms.first().cloned())
        })
        .ok_or_else(|| {
            anyhow::anyhow!("agentgrep outline requires 'file' (or legacy 'query' / first term)")
        })
}

fn parse_full_region_mode(value: Option<&str>) -> Result<FullRegionMode> {
    match value.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(FullRegionMode::Auto),
        "always" => Ok(FullRegionMode::Always),
        "never" => Ok(FullRegionMode::Never),
        other => Err(anyhow::anyhow!(
            "agentgrep trace full_region must be one of: auto, always, never; got {other}"
        )),
    }
}

fn resolved_root_string(ctx: &ToolContext, path: Option<&str>) -> Option<String> {
    path.map(|path| resolve_path_arg(ctx, path).display().to_string())
}

fn resolve_search_root(ctx: &ToolContext, path: Option<&str>) -> PathBuf {
    path.map(PathBuf::from)
        .or_else(|| ctx.working_dir.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn summarize_agentgrep_request(
    params: &AgentGrepInput,
    ctx: &ToolContext,
    context_json_path: Option<&Path>,
) -> String {
    let mut parts = vec![format!("mode={}", params.mode)];
    if let Some(query) = params.query.as_deref() {
        parts.push(format!("query={}", util::truncate_str(query, 80)));
    }
    if let Some(file) = params.file.as_deref() {
        parts.push(format!("file={file}"));
    }
    if let Some(terms) = params.terms.as_ref() {
        parts.push(format!(
            "terms={}",
            util::truncate_str(&terms.join(" "), 80)
        ));
    }
    if let Some(path) = resolved_root_string(ctx, params.path.as_deref()) {
        parts.push(format!("root={path}"));
    }
    if let Some(glob) = normalized_agentgrep_glob(params.glob.as_deref()) {
        parts.push(format!("glob={glob}"));
    }
    if let Some(file_type) = params.file_type.as_deref() {
        parts.push(format!("type={file_type}"));
    }
    if params.paths_only.unwrap_or(false) {
        parts.push("paths_only=true".to_string());
    }
    if context_json_path.is_some() {
        parts.push("context_json=true".to_string());
    }
    parts.join(" ")
}

fn render_grep_output(result: &GrepResult, args: &GrepArgs) -> String {
    if args.paths_only {
        return result
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut lines = vec![
        format!("query: {}", result.query),
        format!(
            "matches: {} in {} files",
            result.total_matches, result.total_files
        ),
    ];

    for file in &result.files {
        render_grep_file(file, &mut lines);
    }

    lines.join("\n")
}

fn render_grep_file(file: &FileMatches, lines: &mut Vec<String>) {
    lines.push(String::new());
    lines.push(file.path.clone());
    if file.total_symbols > 0 {
        lines.push(format!(
            "  symbols: {} total, {} matched, {} other",
            file.total_symbols,
            file.matched_symbol_count,
            file.total_symbols.saturating_sub(file.matched_symbol_count)
        ));
    } else {
        lines.push("  symbols: no structural items detected".to_string());
    }
    for group in &file.groups {
        match (group.start_line, group.end_line) {
            (Some(start_line), Some(end_line)) => lines.push(format!(
                "    - {} {} @ {}-{}",
                group.kind, group.label, start_line, end_line
            )),
            _ => lines.push(format!("    - {}", group.label)),
        }
        for line_match in group.resolved_matches(&file.matches) {
            lines.push(format!(
                "      - @ {} {}",
                line_match.line_number, line_match.line_text
            ));
        }
    }
    if !file.other_symbols.is_empty() {
        let mut summary = file
            .other_symbols
            .iter()
            .map(|item| {
                format!(
                    "{} {} @ {}-{}",
                    item.kind, item.label, item.start_line, item.end_line
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        if file.other_symbols_omitted_count > 0 {
            if !summary.is_empty() {
                summary.push_str("; ");
            }
            summary.push_str(&format!("... {} more", file.other_symbols_omitted_count));
        }
        lines.push(format!("    - other: {summary}"));
    }
}

fn render_find_output(result: &FindResult, args: &FindArgs) -> String {
    if args.paths_only {
        return result
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut lines = vec![
        format!("query: {}", result.query),
        format!("top files: {}", result.files.len()),
    ];

    for (idx, file) in result.files.iter().enumerate() {
        render_find_file(idx, file, args, &mut lines);
    }

    lines.join("\n")
}

fn render_find_file(idx: usize, file: &FindFile, args: &FindArgs, lines: &mut Vec<String>) {
    lines.push(String::new());
    lines.push(format!("{}. {}", idx + 1, file.path));
    lines.push(format!("   role: {}", file.role));
    lines.push("   why:".to_string());
    for reason in &file.why {
        lines.push(format!("     - {reason}"));
    }
    if args.debug_score {
        lines.push(format!("   score: {}", file.score));
    }
    lines.push("   structure:".to_string());
    for item in &file.structure.items {
        lines.push(format!(
            "     - {} {} @ {}-{} ({} lines)",
            item.kind, item.label, item.start_line, item.end_line, item.line_count
        ));
    }
    if file.structure.omitted_count > 0 {
        lines.push(format!(
            "     ... {} more symbols",
            file.structure.omitted_count
        ));
    }
}

fn render_outline_output(result: &OutlineResult) -> String {
    let mut lines = vec![
        format!("file: {}", result.path),
        format!("language: {}", result.language),
        format!("role: {}", result.role),
        format!("lines: {}", result.total_lines),
        format!(
            "symbols: {}",
            result.structure.items.len() + result.structure.omitted_count
        ),
        String::new(),
        "structure:".to_string(),
    ];

    if result.structure.items.is_empty() {
        lines.push("  (no structural items detected)".to_string());
    } else {
        for item in &result.structure.items {
            lines.push(format!(
                "  - {} {} @ {}-{} ({} lines)",
                item.kind, item.label, item.start_line, item.end_line, item.line_count
            ));
        }
        if result.structure.omitted_count > 0 {
            lines.push(format!(
                "  ... {} more symbols",
                result.structure.omitted_count
            ));
        }
    }
    if let Some(note) = &result.context_applied {
        lines.push(String::new());
        lines.push(format!("context: {note}"));
    }

    lines.join("\n")
}

fn render_smart_output(result: &SmartResult, args: &SmartArgs) -> String {
    if args.paths_only {
        return result
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut lines = Vec::new();
    if args.debug_plan {
        lines.extend(render_debug_plan(result));
        lines.push(String::new());
    }
    lines.push("query parameters:".to_string());
    lines.push(format!("  subject: {}", result.query.subject));
    lines.push(format!("  relation: {}", result.query.relation.as_str()));
    if !result.query.support.is_empty() {
        lines.push(format!("  support: {}", result.query.support.join(", ")));
    }
    if let Some(kind) = &result.query.kind {
        lines.push(format!("  kind: {kind}"));
    }
    if let Some(path_hint) = &result.query.path_hint {
        lines.push(format!("  path_hint: {path_hint}"));
    }
    lines.push(String::new());
    lines.push(format!(
        "top results: {} files, {} regions",
        result.summary.total_files, result.summary.total_regions
    ));
    if result.files.is_empty() {
        lines.push("no results found for the current trace query and scope".to_string());
    }
    if let Some(best_file) = &result.summary.best_file {
        lines.push(format!("best answer likely in {best_file}"));
    }
    for (idx, file) in result.files.iter().enumerate() {
        render_smart_file(idx, file, args, &mut lines);
    }

    lines.join("\n")
}

fn render_debug_plan(result: &SmartResult) -> Vec<String> {
    let relation_terms = match result.query.relation {
        Relation::Rendered => "render, draw, ui, widget, view",
        Relation::CalledFrom => "call, invoke, dispatch",
        Relation::TriggeredFrom => "trigger, dispatch, schedule",
        Relation::Populated => "set, assign, insert, push, build",
        Relation::ComesFrom => "source, load, parse, read, fetch",
        Relation::Handled => "handle, handler, event, dispatch",
        Relation::Defined => "fn, struct, enum, class, def",
        Relation::Implementation => "impl, register, wire, tool",
        _ => result.query.relation.as_str(),
    };
    let mut lines = vec![
        "debug plan:".to_string(),
        "  mode: trace".to_string(),
        format!("  subject: {}", result.query.subject),
        format!("  relation: {}", result.query.relation.as_str()),
        format!("  relation_terms: {relation_terms}"),
    ];
    if let Some(kind) = &result.query.kind {
        lines.push(format!("  kind filter: {kind}"));
    }
    if let Some(path_hint) = &result.query.path_hint {
        lines.push(format!("  path hint: {path_hint}"));
    }
    if !result.query.support.is_empty() {
        lines.push(format!(
            "  support terms: {}",
            result.query.support.join(", ")
        ));
    }
    lines
}

fn render_smart_file(idx: usize, file: &SmartFile, args: &SmartArgs, lines: &mut Vec<String>) {
    lines.push(String::new());
    lines.push(format!("{}. {}", idx + 1, file.path));
    lines.push(format!("   role: {}", file.role));
    lines.push("   why:".to_string());
    for reason in &file.why {
        lines.push(format!("     - {reason}"));
    }
    if args.debug_score {
        lines.push(format!("   score: {}", file.score));
    }
    lines.push("   structure:".to_string());
    for item in &file.structure.items {
        lines.push(format!(
            "     - {} {} @ {}-{} ({} lines)",
            item.kind, item.label, item.start_line, item.end_line, item.line_count
        ));
    }
    if file.structure.omitted_count > 0 {
        lines.push(format!(
            "     ... {} more symbols",
            file.structure.omitted_count
        ));
    }
    if let Some(note) = &file.context_applied {
        lines.push(format!("   context: {note}"));
    }
    lines.push("   regions:".to_string());
    for region in &file.regions {
        render_smart_region(region, args.debug_score, lines);
    }
}

fn render_smart_region(region: &SmartRegion, debug_score: bool, lines: &mut Vec<String>) {
    lines.push(format!(
        "     - {} @ {}-{} ({} lines)",
        region.label, region.start_line, region.end_line, region.line_count
    ));
    lines.push(format!("       kind: {}", region.kind));
    if debug_score {
        lines.push(format!("       score: {}", region.score));
    }
    if region.full_region {
        lines.push("       full region:".to_string());
    } else {
        lines.push("       snippet:".to_string());
    }
    for line in region.body.lines() {
        lines.push(format!("         {line}"));
    }
    lines.push("       why:".to_string());
    for reason in &region.why {
        lines.push(format!("         - {reason}"));
    }
    if let Some(note) = &region.context_applied {
        lines.push(format!("       context: {note}"));
    }
}

fn maybe_write_context_json(params: &AgentGrepInput, ctx: &ToolContext) -> Result<Option<PathBuf>> {
    if !matches!(params.mode.as_str(), "trace" | "smart" | "outline") {
        return Ok(None);
    }

    let context = build_harness_context(params, ctx);
    let Some(context) = context else {
        return Ok(None);
    };

    let mut path = storage::runtime_dir();
    path.push(format!(
        "jcode-agentgrep-context-{}-{}.json",
        ctx.session_id, ctx.tool_call_id
    ));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, serde_json::to_vec(&context)?)?;
    Ok(Some(path))
}

fn build_harness_context(
    params: &AgentGrepInput,
    ctx: &ToolContext,
) -> Option<AgentGrepHarnessContext> {
    let session = Session::load(&ctx.session_id).ok()?;
    let observations = collect_tool_exposures(&session);
    let search_root = params
        .path
        .as_deref()
        .map(|path| resolve_path_arg(ctx, path))
        .or_else(|| ctx.working_dir.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let total_messages = session.messages.len().max(1);
    let compaction_cutoff = session
        .compaction
        .as_ref()
        .map(|state| state.covers_up_to_turn.min(total_messages));
    let mut file_mtime_cache = HashMap::new();

    let mut context = AgentGrepHarnessContext {
        version: 1,
        ..Default::default()
    };
    let mut focus = HashSet::new();

    for observation in observations {
        let exposure = ExposureDescriptor {
            timestamp: observation.timestamp,
            message_index: observation.message_index,
            total_messages,
            compaction_cutoff,
        };
        match observation.tool.name.as_str() {
            "read" => collect_read_exposure(
                &observation.tool,
                &search_root,
                ctx,
                &mut context,
                &mut focus,
                exposure,
                &mut file_mtime_cache,
            ),
            "agentgrep" => collect_agentgrep_exposure(
                &observation.tool,
                &observation.content,
                &search_root,
                ctx,
                &mut context,
                &mut focus,
                exposure,
                &mut file_mtime_cache,
            ),
            "bash" => collect_bash_exposure(
                &observation.tool,
                &observation.content,
                &search_root,
                ctx,
                &mut context,
                &mut focus,
                exposure,
                &mut file_mtime_cache,
            ),
            _ => {}
        }
    }

    let mut focus_files = focus.into_iter().collect::<Vec<_>>();
    focus_files.sort();
    context.focus_files = focus_files;
    if context.known_regions.is_empty()
        && context.known_files.is_empty()
        && context.known_symbols.is_empty()
        && context.focus_files.is_empty()
    {
        None
    } else {
        Some(context)
    }
}

fn collect_tool_exposures(session: &Session) -> Vec<ToolExposureObservation> {
    let mut observations = Vec::new();
    let mut tool_map: HashMap<String, ToolCall> = HashMap::new();

    for (message_index, msg) in session.messages.iter().enumerate() {
        for block in &msg.content {
            match block {
                ContentBlock::ToolUse { id, name, input } => {
                    tool_map.insert(
                        id.clone(),
                        ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                            intent: None,
                        },
                    );
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    let tool = tool_map
                        .get(tool_use_id)
                        .cloned()
                        .unwrap_or_else(|| ToolCall {
                            id: tool_use_id.clone(),
                            name: "tool".to_string(),
                            input: Value::Null,
                            intent: None,
                        });
                    observations.push(ToolExposureObservation {
                        tool,
                        content: content.clone(),
                        timestamp: msg.timestamp,
                        message_index,
                    });
                }
                _ => {}
            }
        }
    }

    observations
}

fn collect_read_exposure(
    tool: &ToolCall,
    search_root: &Path,
    ctx: &ToolContext,
    context: &mut AgentGrepHarnessContext,
    focus: &mut HashSet<String>,
    exposure: ExposureDescriptor,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) {
    let Some(file_path) = tool.input.get("file_path").and_then(|value| value.as_str()) else {
        return;
    };
    let Some(path) = normalize_context_path(file_path, search_root, ctx) else {
        return;
    };
    let (start_line, end_line) = normalize_read_range_from_tool_input(&tool.input);
    focus.insert(path.clone());
    let region = tune_known_region(
        AgentGrepKnownRegion {
            path: path.clone(),
            start_line,
            end_line,
            body_confidence: 0.85,
            current_version_confidence: 0.88,
            prune_confidence: 0.78,
            source_strength: "full_region",
            reasons: vec!["read_tool_exposure", "session_local_history"],
        },
        exposure,
        search_root,
        ctx,
        file_mtime_cache,
    );
    push_known_region(context, region);
    let file = tune_known_file(
        AgentGrepKnownFile {
            path,
            structure_confidence: 0.55,
            body_confidence: 0.45,
            current_version_confidence: 0.88,
            prune_confidence: 0.4,
            source_strength: "snippet",
            reasons: vec!["read_tool_exposure"],
        },
        exposure,
        search_root,
        ctx,
        file_mtime_cache,
    );
    push_known_file(context, file);
}

#[expect(
    clippy::too_many_arguments,
    reason = "agentgrep exposure collection needs tool payload, content, search root, context, focus set, exposure metadata, and mtime cache"
)]
fn collect_agentgrep_exposure(
    tool: &ToolCall,
    content: &str,
    search_root: &Path,
    ctx: &ToolContext,
    context: &mut AgentGrepHarnessContext,
    focus: &mut HashSet<String>,
    exposure: ExposureDescriptor,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) {
    let Some(mode) = tool.input.get("mode").and_then(|value| value.as_str()) else {
        return;
    };
    match mode {
        "outline" => {
            let file = tool
                .input
                .get("file")
                .and_then(|value| value.as_str())
                .or_else(|| tool.input.get("query").and_then(|value| value.as_str()));
            let Some(file) = file else {
                return;
            };
            let Some(path) = normalize_context_path(file, search_root, ctx) else {
                return;
            };
            focus.insert(path.clone());
            let known = tune_known_file(
                AgentGrepKnownFile {
                    path: path.clone(),
                    structure_confidence: 0.95,
                    body_confidence: 0.15,
                    current_version_confidence: 0.82,
                    prune_confidence: 0.86,
                    source_strength: "outline_only",
                    reasons: vec!["agentgrep_outline_result"],
                },
                exposure,
                search_root,
                ctx,
                file_mtime_cache,
            );
            push_known_file(context, known);
            collect_outline_symbols(
                content,
                &path,
                context,
                exposure,
                search_root,
                ctx,
                file_mtime_cache,
            );
        }
        "trace" | "smart" => {
            if let Some(path_hint) = tool.input.get("path").and_then(|value| value.as_str())
                && let Some(path) = normalize_context_path(path_hint, search_root, ctx)
            {
                focus.insert(path);
            }
            collect_trace_exposure(
                content,
                search_root,
                ctx,
                context,
                focus,
                exposure,
                file_mtime_cache,
            );
        }
        _ => {}
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "bash exposure collection needs tool payload, output content, search root, context, focus set, exposure metadata, and mtime cache"
)]
fn collect_bash_exposure(
    tool: &ToolCall,
    content: &str,
    search_root: &Path,
    ctx: &ToolContext,
    context: &mut AgentGrepHarnessContext,
    focus: &mut HashSet<String>,
    exposure: ExposureDescriptor,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) {
    let Some(command) = tool.input.get("command").and_then(|value| value.as_str()) else {
        return;
    };

    if let Some(path) = parse_sed_file_range(command).and_then(|(path, start_line, end_line)| {
        normalize_context_path(&path, search_root, ctx)
            .map(|normalized| (normalized, start_line, end_line))
    }) {
        let (path, start_line, end_line) = path;
        focus.insert(path.clone());
        let region = tune_known_region(
            AgentGrepKnownRegion {
                path,
                start_line,
                end_line,
                body_confidence: 0.78,
                current_version_confidence: 0.7,
                prune_confidence: 0.7,
                source_strength: "snippet",
                reasons: vec!["bash_sed_exposure"],
            },
            exposure,
            search_root,
            ctx,
            file_mtime_cache,
        );
        push_known_region(context, region);
    }

    for candidate in parse_cat_files(command)
        .into_iter()
        .chain(parse_git_show_files(command).into_iter())
        .chain(parse_git_diff_files(command).into_iter())
    {
        let Some(path) = normalize_context_path(&candidate, search_root, ctx) else {
            continue;
        };
        focus.insert(path.clone());
        let known = tune_known_file(
            AgentGrepKnownFile {
                path,
                structure_confidence: 0.5,
                body_confidence: 0.72,
                current_version_confidence: 0.72,
                prune_confidence: 0.55,
                source_strength: "full_file",
                reasons: vec!["bash_file_exposure"],
            },
            exposure,
            search_root,
            ctx,
            file_mtime_cache,
        );
        push_known_file(context, known);
    }

    collect_shell_output_path_exposure(
        content,
        search_root,
        ctx,
        context,
        focus,
        exposure,
        file_mtime_cache,
    );
}

fn normalize_context_path(path: &str, search_root: &Path, ctx: &ToolContext) -> Option<String> {
    let path = path.trim().trim_matches('"').trim_matches('\'');
    let path = path.strip_prefix("./").unwrap_or(path);
    let resolved = ctx.resolve_path(Path::new(path));
    if let Ok(relative) = resolved.strip_prefix(search_root) {
        return Some(relative.display().to_string());
    }
    if Path::new(path).is_relative() {
        return Some(path.to_string());
    }
    None
}

fn normalize_read_range_from_tool_input(input: &Value) -> (usize, usize) {
    if let Some(start_line) = input.get("start_line").and_then(|value| value.as_u64()) {
        let start_line = start_line as usize;
        let end_line = input
            .get("end_line")
            .and_then(|value| value.as_u64())
            .map(|value| value as usize)
            .unwrap_or(
                start_line
                    .saturating_add(
                        input
                            .get("limit")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(200) as usize,
                    )
                    .saturating_sub(1),
            );
        return (start_line.max(1), end_line.max(start_line.max(1)));
    }
    let offset = input
        .get("offset")
        .and_then(|value| value.as_u64())
        .unwrap_or(0) as usize;
    let limit = input
        .get("limit")
        .and_then(|value| value.as_u64())
        .unwrap_or(200) as usize;
    let start_line = offset + 1;
    let end_line = start_line + limit.saturating_sub(1);
    (start_line, end_line)
}

fn collect_outline_symbols(
    content: &str,
    path: &str,
    context: &mut AgentGrepHarnessContext,
    exposure: ExposureDescriptor,
    search_root: &Path,
    ctx: &ToolContext,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) {
    for (kind, label, _start_line, _end_line) in parse_structure_items(content) {
        let symbol = tune_known_symbol(
            AgentGrepKnownSymbol {
                path: path.to_string(),
                symbol: label,
                kind: Some(kind),
                structure_confidence: 0.92,
                body_confidence: 0.1,
                current_version_confidence: 0.82,
                prune_confidence: 0.8,
                source_strength: "outline_only",
                reasons: vec!["agentgrep_outline_structure"],
            },
            exposure,
            search_root,
            ctx,
            file_mtime_cache,
        );
        push_known_symbol(context, symbol);
    }
}

fn collect_trace_exposure(
    content: &str,
    search_root: &Path,
    ctx: &ToolContext,
    context: &mut AgentGrepHarnessContext,
    focus: &mut HashSet<String>,
    exposure: ExposureDescriptor,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) {
    let mut current_file: Option<String> = None;
    let mut section: Option<&str> = None;
    let mut pending_region: Option<PendingTraceRegion> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(path) = parse_ranked_file_header(trimmed) {
            current_file = Some(path.clone());
            focus.insert(path.clone());
            let known = tune_known_file(
                AgentGrepKnownFile {
                    path,
                    structure_confidence: 0.72,
                    body_confidence: 0.2,
                    current_version_confidence: 0.78,
                    prune_confidence: 0.62,
                    source_strength: "trace_summary",
                    reasons: vec!["agentgrep_trace_file"],
                },
                exposure,
                search_root,
                ctx,
                file_mtime_cache,
            );
            push_known_file(context, known);
            section = None;
            pending_region = None;
            continue;
        }
        if let Some(best_file) = trimmed.strip_prefix("best answer likely in ") {
            if let Some(path) = normalize_context_path(best_file.trim(), search_root, ctx) {
                focus.insert(path);
            }
            continue;
        }
        match trimmed {
            "structure:" => {
                section = Some("structure");
                pending_region = None;
                continue;
            }
            "regions:" => {
                section = Some("regions");
                pending_region = None;
                continue;
            }
            _ => {}
        }

        let Some(file_path) = current_file.clone() else {
            continue;
        };

        if section == Some("structure") {
            if let Some((kind, label, _start_line, _end_line)) = parse_structure_item_line(trimmed)
            {
                let symbol = tune_known_symbol(
                    AgentGrepKnownSymbol {
                        path: file_path,
                        symbol: label,
                        kind: Some(kind),
                        structure_confidence: 0.82,
                        body_confidence: 0.12,
                        current_version_confidence: 0.78,
                        prune_confidence: 0.66,
                        source_strength: "trace_structure",
                        reasons: vec!["agentgrep_trace_structure"],
                    },
                    exposure,
                    search_root,
                    ctx,
                    file_mtime_cache,
                );
                push_known_symbol(context, symbol);
            }
            continue;
        }

        if section == Some("regions") {
            if let Some((label, start_line, end_line)) = parse_region_header_line(trimmed) {
                pending_region = Some(PendingTraceRegion {
                    path: file_path.clone(),
                    kind: None,
                    start_line,
                    end_line,
                });
                let symbol = tune_known_symbol(
                    AgentGrepKnownSymbol {
                        path: file_path,
                        symbol: label,
                        kind: None,
                        structure_confidence: 0.86,
                        body_confidence: 0.28,
                        current_version_confidence: 0.8,
                        prune_confidence: 0.68,
                        source_strength: "trace_region",
                        reasons: vec!["agentgrep_trace_region_header"],
                    },
                    exposure,
                    search_root,
                    ctx,
                    file_mtime_cache,
                );
                push_known_symbol(context, symbol);
                continue;
            }
            if let Some(kind) = trimmed.strip_prefix("kind: ") {
                if let Some(region) = pending_region.as_mut() {
                    region.kind = Some(leak_str(kind.trim().to_string()));
                }
                continue;
            }
            if (trimmed == "full region:" || trimmed == "snippet:")
                && let Some(region) = pending_region.take()
            {
                let profile = if trimmed == "full region:" {
                    RegionConfidenceProfile {
                        body_confidence: 0.9,
                        current_version_confidence: 0.72,
                        prune_confidence: 0.82,
                        source_strength: "full_region",
                    }
                } else {
                    RegionConfidenceProfile {
                        body_confidence: 0.48,
                        current_version_confidence: 0.72,
                        prune_confidence: 0.52,
                        source_strength: "snippet",
                    }
                };
                let region = tune_known_region(
                    AgentGrepKnownRegion {
                        path: region.path,
                        start_line: region.start_line,
                        end_line: region.end_line,
                        body_confidence: profile.body_confidence,
                        current_version_confidence: profile.current_version_confidence,
                        prune_confidence: profile.prune_confidence,
                        source_strength: profile.source_strength,
                        reasons: vec!["agentgrep_trace_region_body"],
                    },
                    exposure,
                    search_root,
                    ctx,
                    file_mtime_cache,
                );
                push_known_region(context, region);
            }
        }
    }
}

fn collect_shell_output_path_exposure(
    content: &str,
    search_root: &Path,
    ctx: &ToolContext,
    context: &mut AgentGrepHarnessContext,
    focus: &mut HashSet<String>,
    exposure: ExposureDescriptor,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) {
    for (path, line_number) in parse_path_line_hits(content) {
        let Some(path) = normalize_context_path(&path, search_root, ctx) else {
            continue;
        };
        focus.insert(path.clone());
        let file = tune_known_file(
            AgentGrepKnownFile {
                path: path.clone(),
                structure_confidence: 0.28,
                body_confidence: 0.22,
                current_version_confidence: 0.68,
                prune_confidence: 0.18,
                source_strength: "match_line_only",
                reasons: vec!["bash_output_file_hit"],
            },
            exposure,
            search_root,
            ctx,
            file_mtime_cache,
        );
        push_known_file(context, file);
        let region = tune_known_region(
            AgentGrepKnownRegion {
                path,
                start_line: line_number,
                end_line: line_number,
                body_confidence: 0.26,
                current_version_confidence: 0.68,
                prune_confidence: 0.2,
                source_strength: "match_line_only",
                reasons: vec!["bash_output_line_hit"],
            },
            exposure,
            search_root,
            ctx,
            file_mtime_cache,
        );
        push_known_region(context, region);
    }
}

fn tune_known_file(
    mut known: AgentGrepKnownFile,
    exposure: ExposureDescriptor,
    search_root: &Path,
    ctx: &ToolContext,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) -> AgentGrepKnownFile {
    apply_exposure_tuning(
        Some(&mut known.structure_confidence),
        &mut known.body_confidence,
        &mut known.current_version_confidence,
        &mut known.prune_confidence,
        &mut known.reasons,
        &known.path,
        exposure,
        search_root,
        ctx,
        file_mtime_cache,
    );
    known
}

fn tune_known_region(
    mut known: AgentGrepKnownRegion,
    exposure: ExposureDescriptor,
    search_root: &Path,
    ctx: &ToolContext,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) -> AgentGrepKnownRegion {
    apply_exposure_tuning(
        None,
        &mut known.body_confidence,
        &mut known.current_version_confidence,
        &mut known.prune_confidence,
        &mut known.reasons,
        &known.path,
        exposure,
        search_root,
        ctx,
        file_mtime_cache,
    );
    known
}

fn tune_known_symbol(
    mut known: AgentGrepKnownSymbol,
    exposure: ExposureDescriptor,
    search_root: &Path,
    ctx: &ToolContext,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) -> AgentGrepKnownSymbol {
    apply_exposure_tuning(
        Some(&mut known.structure_confidence),
        &mut known.body_confidence,
        &mut known.current_version_confidence,
        &mut known.prune_confidence,
        &mut known.reasons,
        &known.path,
        exposure,
        search_root,
        ctx,
        file_mtime_cache,
    );
    known
}

#[expect(
    clippy::too_many_arguments,
    reason = "exposure tuning uses several confidence outputs plus exposure metadata and file freshness cache"
)]
fn apply_exposure_tuning(
    structure_confidence: Option<&mut f32>,
    body_confidence: &mut f32,
    current_version_confidence: &mut f32,
    prune_confidence: &mut f32,
    reasons: &mut Vec<&'static str>,
    path: &str,
    exposure: ExposureDescriptor,
    search_root: &Path,
    ctx: &ToolContext,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) {
    let position_ratio = if exposure.total_messages <= 1 {
        1.0
    } else {
        (exposure.message_index + 1) as f32 / exposure.total_messages as f32
    };
    let memory_multiplier = if exposure
        .compaction_cutoff
        .is_some_and(|cutoff| exposure.message_index < cutoff)
    {
        merge_reasons(reasons, vec!["compacted_history"]);
        0.42
    } else if position_ratio >= 0.85 {
        merge_reasons(reasons, vec!["active_context_tail"]);
        1.0
    } else if position_ratio >= 0.6 {
        merge_reasons(reasons, vec!["recent_context"]);
        0.88
    } else {
        merge_reasons(reasons, vec!["older_context"]);
        0.72
    };

    if let Some(structure_confidence) = structure_confidence {
        *structure_confidence =
            (*structure_confidence * (0.75 + 0.25 * memory_multiplier)).clamp(0.0, 1.0);
    }
    *body_confidence = (*body_confidence * memory_multiplier).clamp(0.0, 1.0);
    *prune_confidence = (*prune_confidence * memory_multiplier).clamp(0.0, 1.0);

    let freshness_multiplier =
        file_freshness_multiplier(path, exposure.timestamp, search_root, ctx, file_mtime_cache);
    if freshness_multiplier < 0.999 {
        merge_reasons(reasons, vec!["file_changed_since_seen"]);
    } else if exposure.timestamp.is_some() {
        merge_reasons(reasons, vec!["file_unchanged_since_seen"]);
    }
    *current_version_confidence =
        (*current_version_confidence * freshness_multiplier).clamp(0.0, 1.0);
}

fn file_freshness_multiplier(
    path: &str,
    exposure_time: Option<DateTime<Utc>>,
    search_root: &Path,
    ctx: &ToolContext,
    file_mtime_cache: &mut HashMap<String, Option<DateTime<Utc>>>,
) -> f32 {
    let Some(exposure_time) = exposure_time else {
        return 0.7;
    };

    let modified_at = file_mtime_cache
        .entry(path.to_string())
        .or_insert_with(|| file_modified_at(path, search_root, ctx))
        .to_owned();
    let Some(modified_at) = modified_at else {
        return 0.72;
    };
    if modified_at <= exposure_time {
        return 1.0;
    }

    let delta = modified_at.signed_duration_since(exposure_time);
    if delta.num_seconds() <= 5 {
        0.92
    } else if delta.num_minutes() <= 10 {
        0.68
    } else if delta.num_hours() <= 6 {
        0.45
    } else {
        0.25
    }
}

fn file_modified_at(path: &str, search_root: &Path, ctx: &ToolContext) -> Option<DateTime<Utc>> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        let resolved = ctx.resolve_path(Path::new(path));
        if resolved.starts_with(search_root) {
            resolved
        } else {
            search_root.join(path)
        }
    };
    let modified = std::fs::metadata(candidate).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

fn push_known_file(context: &mut AgentGrepHarnessContext, known: AgentGrepKnownFile) {
    if let Some(existing) = context
        .known_files
        .iter_mut()
        .find(|entry| entry.path == known.path)
    {
        existing.structure_confidence = existing
            .structure_confidence
            .max(known.structure_confidence);
        existing.body_confidence = existing.body_confidence.max(known.body_confidence);
        existing.current_version_confidence = existing
            .current_version_confidence
            .max(known.current_version_confidence);
        existing.prune_confidence = existing.prune_confidence.max(known.prune_confidence);
        merge_reasons(&mut existing.reasons, known.reasons);
        return;
    }
    context.known_files.push(known);
}

fn push_known_region(context: &mut AgentGrepHarnessContext, known: AgentGrepKnownRegion) {
    if let Some(existing) = context.known_regions.iter_mut().find(|entry| {
        entry.path == known.path
            && entry.start_line == known.start_line
            && entry.end_line == known.end_line
    }) {
        existing.body_confidence = existing.body_confidence.max(known.body_confidence);
        existing.current_version_confidence = existing
            .current_version_confidence
            .max(known.current_version_confidence);
        existing.prune_confidence = existing.prune_confidence.max(known.prune_confidence);
        merge_reasons(&mut existing.reasons, known.reasons);
        return;
    }
    context.known_regions.push(known);
}

fn push_known_symbol(context: &mut AgentGrepHarnessContext, known: AgentGrepKnownSymbol) {
    if let Some(existing) = context.known_symbols.iter_mut().find(|entry| {
        entry.path == known.path && entry.symbol == known.symbol && entry.kind == known.kind
    }) {
        existing.structure_confidence = existing
            .structure_confidence
            .max(known.structure_confidence);
        existing.body_confidence = existing.body_confidence.max(known.body_confidence);
        existing.current_version_confidence = existing
            .current_version_confidence
            .max(known.current_version_confidence);
        existing.prune_confidence = existing.prune_confidence.max(known.prune_confidence);
        merge_reasons(&mut existing.reasons, known.reasons);
        return;
    }
    context.known_symbols.push(known);
}

fn merge_reasons(existing: &mut Vec<&'static str>, new_reasons: Vec<&'static str>) {
    for reason in new_reasons {
        if !existing.contains(&reason) {
            existing.push(reason);
        }
    }
}

fn parse_structure_items(content: &str) -> Vec<(&'static str, String, usize, usize)> {
    content
        .lines()
        .filter_map(|line| parse_structure_item_line(line.trim()))
        .collect()
}

fn parse_structure_item_line(line: &str) -> Option<(&'static str, String, usize, usize)> {
    static STRUCTURE_ITEM_RE: OnceLock<Regex> = OnceLock::new();
    let captures = STRUCTURE_ITEM_RE
        .get_or_init(|| {
            Regex::new(r"^-\s+([A-Za-z0-9_-]+)\s+(.+?)\s+@\s*(\d+)-(\d+)")
                .expect("valid structure item regex")
        })
        .captures(line)?;
    let kind = captures.get(1)?.as_str();
    let label = captures.get(2)?.as_str().trim().to_string();
    let start_line = captures.get(3)?.as_str().parse().ok()?;
    let end_line = captures.get(4)?.as_str().parse().ok()?;
    Some((leak_str(kind.to_string()), label, start_line, end_line))
}

fn parse_ranked_file_header(line: &str) -> Option<String> {
    static FILE_HEADER_RE: OnceLock<Regex> = OnceLock::new();
    FILE_HEADER_RE
        .get_or_init(|| Regex::new(r"^\d+\.\s+(.+)$").expect("valid ranked file regex"))
        .captures(line)
        .and_then(|captures| {
            captures
                .get(1)
                .map(|value| value.as_str().trim().to_string())
        })
}

fn parse_region_header_line(line: &str) -> Option<(String, usize, usize)> {
    static REGION_HEADER_RE: OnceLock<Regex> = OnceLock::new();
    let captures = REGION_HEADER_RE
        .get_or_init(|| Regex::new(r"^-\s+(.+?)\s+@\s*(\d+)-(\d+)").expect("valid region regex"))
        .captures(line)?;
    let label = captures.get(1)?.as_str().trim().to_string();
    let start_line = captures.get(2)?.as_str().parse().ok()?;
    let end_line = captures.get(3)?.as_str().parse().ok()?;
    Some((label, start_line, end_line))
}

fn parse_sed_file_range(command: &str) -> Option<(String, usize, usize)> {
    static SED_RE: OnceLock<Regex> = OnceLock::new();
    let captures = SED_RE
        .get_or_init(|| {
            Regex::new(r#"sed\s+-n\s+['"]?(\d+),(\d+)p['"]?\s+(?:"([^"]+)"|'([^']+)'|([^\s|;]+))"#)
                .expect("valid sed range regex")
        })
        .captures(command)?;
    let start_line = captures.get(1)?.as_str().parse().ok()?;
    let end_line = captures.get(2)?.as_str().parse().ok()?;
    let path = captures
        .get(3)
        .or_else(|| captures.get(4))
        .or_else(|| captures.get(5))?
        .as_str()
        .to_string();
    Some((path, start_line, end_line))
}

fn parse_cat_files(command: &str) -> Vec<String> {
    static CAT_RE: OnceLock<Regex> = OnceLock::new();
    CAT_RE
        .get_or_init(|| {
            Regex::new(r#"(?:^|[;&|]\s*)cat\s+(?:"([^"]+)"|'([^']+)'|([^\s|;]+))"#)
                .expect("valid cat regex")
        })
        .captures_iter(command)
        .filter_map(|captures| {
            captures
                .get(1)
                .or_else(|| captures.get(2))
                .or_else(|| captures.get(3))
                .map(|value| value.as_str().to_string())
        })
        .collect()
}

fn parse_git_show_files(command: &str) -> Vec<String> {
    static GIT_SHOW_RE: OnceLock<Regex> = OnceLock::new();
    GIT_SHOW_RE
        .get_or_init(|| {
            Regex::new(r#"git\s+show\s+[^:\s]+:(?:"([^"]+)"|'([^']+)'|([^\s|;]+))"#)
                .expect("valid git show regex")
        })
        .captures_iter(command)
        .filter_map(|captures| {
            captures
                .get(1)
                .or_else(|| captures.get(2))
                .or_else(|| captures.get(3))
                .map(|value| value.as_str().to_string())
        })
        .collect()
}

fn parse_git_diff_files(command: &str) -> Vec<String> {
    static GIT_DIFF_RE: OnceLock<Regex> = OnceLock::new();
    GIT_DIFF_RE
        .get_or_init(|| {
            Regex::new(r#"git\s+diff(?:\s+[^\n]*)?\s+--\s+(?:"([^"]+)"|'([^']+)'|([^\s|;]+))"#)
                .expect("valid git diff regex")
        })
        .captures_iter(command)
        .filter_map(|captures| {
            captures
                .get(1)
                .or_else(|| captures.get(2))
                .or_else(|| captures.get(3))
                .map(|value| value.as_str().to_string())
        })
        .collect()
}

fn parse_path_line_hits(content: &str) -> Vec<(String, usize)> {
    static PATH_LINE_RE: OnceLock<Regex> = OnceLock::new();
    PATH_LINE_RE
        .get_or_init(|| Regex::new(r"(?m)^([^:\n]+):(\d+):").expect("valid path line regex"))
        .captures_iter(content)
        .filter_map(|captures| {
            let path = captures.get(1)?.as_str().trim().to_string();
            let line_number = captures.get(2)?.as_str().parse().ok()?;
            Some((path, line_number))
        })
        .collect()
}

fn leak_str(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn resolve_path_arg(ctx: &ToolContext, path: &str) -> PathBuf {
    ctx.resolve_path(Path::new(path))
}

fn normalized_agentgrep_glob(glob: Option<&str>) -> Option<&str> {
    let glob = glob?.trim();
    if glob.is_empty() {
        return None;
    }

    if is_match_all_glob(glob) {
        return None;
    }

    Some(glob)
}

fn normalized_agentgrep_glob_owned(glob: Option<&str>) -> Option<String> {
    normalized_agentgrep_glob(glob).map(ToOwned::to_owned)
}

fn is_match_all_glob(glob: &str) -> bool {
    matches!(glob, "*" | "**" | "**/*" | "./*" | "./**" | "./**/*")
}

#[cfg(test)]
#[path = "agentgrep_tests.rs"]
mod tests;
