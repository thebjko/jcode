# jcode Telemetry

jcode collects **anonymous, minimal usage statistics** to help understand how many people use jcode, what providers/models are popular, whether onboarding works, which feature families are used, how often sessions succeed, and whether performance/regressions are improving. This data helps prioritize development without collecting prompts or code.

Recent telemetry additions also include: coarse onboarding steps, explicit thumbs-up / thumbs-down feedback, build-channel / dev-mode cleanup flags, session/workflow/tool-category summaries, coarse project language buckets, and retention helpers like active days in the last 7 / 30 days.

## What We Collect

### Install Event (sent once, on first launch)

| Field | Example | Purpose |
|-------|---------|----------|
| `id` | `a1b2c3d4-...` | Random UUID, not tied to your identity |
| `event` | `"install"` | Event type |
| `version` | `"0.6.0"` | jcode version |
| `os` | `"linux"` | Operating system |
| `arch` | `"x86_64"` | CPU architecture |

### Upgrade Event

| Field | Example | Purpose |
|-------|---------|----------|
| `id` | `a1b2c3d4-...` | Same random UUID |
| `event` | `"upgrade"` | Event type |
| `version` | `"0.9.1"` | Current jcode version |
| `from_version` | `"0.8.1"` | Previously recorded jcode version |
| `os` / `arch` | `"linux"` / `"x86_64"` | Environment breakdown |

### Auth Success Event

| Field | Example | Purpose |
|-------|---------|----------|
| `id` | `a1b2c3d4-...` | Same random UUID |
| `event` | `"auth_success"` | Event type |
| `auth_provider` | `"claude"` | Which provider/account system was configured |
| `auth_method` | `"oauth"` | Coarse auth method only |
| `version` / `os` / `arch` | `"0.9.1"` / `"linux"` / `"x86_64"` | Activation funnel dimensions |

### Onboarding Step Event

| Field | Example | Purpose |
|-------|---------|----------|
| `event` | `"onboarding_step"` | Event type |
| `step` | `"first_prompt_sent"` | Coarse funnel step |
| `auth_provider` | `"openai"` | Optional provider dimension for auth steps |
| `auth_method` | `"oauth"` | Optional auth-method dimension for auth steps |
| `milestone_elapsed_ms` | `42000` | Rough time from install to milestone |

### Feedback Event

| Field | Example | Purpose |
|-------|---------|----------|
| `event` | `"feedback"` | Event type |
| `feedback_rating` | `"up"` / `"down"` | Explicit product sentiment |
| `feedback_reason` | `"slow"` | Optional coarse reason bucket |

### Session Start Event

| Field | Example | Purpose |
|-------|---------|----------|
| `id` | `a1b2c3d4-...` | Same random UUID |
| `event` | `"session_start"` | Event type |
| `version` | `"0.6.0"` | jcode version |
| `os` | `"linux"` | Operating system |
| `arch` | `"x86_64"` | CPU architecture |
| `provider_start` | `"OpenAI"` | Provider when session started |
| `model_start` | `"gpt-5.4"` | Model when session started |
| `resumed_session` | `false` | Whether this was a resumed session |

### Session End / Crash Event

| Field | Example | Purpose |
|-------|---------|----------|
| `id` | `a1b2c3d4-...` | Same random UUID |
| `event` | `"session_end"` / `"session_crash"` | Event type |
| `version` | `"0.6.0"` | jcode version |
| `os` | `"linux"` | Operating system |
| `arch` | `"x86_64"` | CPU architecture |
| `provider_start` | `"OpenAI"` | Provider when session started |
| `provider_end` | `"OpenAI"` | Provider when session ended |
| `model_start` | `"gpt-5.4"` | Model when session started |
| `model_end` | `"gpt-5.4"` | Model when session ended |
| `provider_switches` | `0` | How many times you switched providers |
| `model_switches` | `1` | How many times you switched models |
| `duration_mins` | `45` | Session length in minutes |
| `duration_secs` | `2700` | Finer-grained session length |
| `turns` | `23` | Number of user prompts sent |
| `had_user_prompt` | `true` | Whether any real prompt was submitted |
| `had_assistant_response` | `true` | Whether the assistant produced a response |
| `assistant_responses` | `6` | Number of assistant responses |
| `first_assistant_response_ms` | `1200` | Time to first assistant response |
| `first_tool_call_ms` | `900` | Time to first tool invocation |
| `first_tool_success_ms` | `1500` | Time to first successful tool execution |
| `first_file_edit_ms` | `2200` | Time to first successful file edit |
| `first_test_pass_ms` | `4100` | Time to first successful test run |
| `tool_calls` | `8` | Number of tool executions |
| `tool_failures` | `1` | Number of tool execution failures |
| `executed_tool_calls` | `10` | Centralized count of actual registry tool executions |
| `executed_tool_successes` | `9` | Successful registry tool executions |
| `executed_tool_failures` | `1` | Failed registry tool executions |
| `tool_latency_total_ms` | `4200` | Aggregate tool execution latency |
| `tool_latency_max_ms` | `1800` | Slowest single tool call |
| `file_write_calls` | `2` | Count of write/edit/patch style tool uses |
| `tests_run` | `1` | Coarse count of test runs triggered |
| `tests_passed` | `1` | Coarse count of successful test runs |
| `feature_*_used` | `true/false` | Whether a feature family was used (memory, swarm, web, email, MCP, side panel, goals, selfdev, background, subagents) |
| `tool_cat_*` | `0..N` | Coarse tool category counts (read/search, write, shell, web, memory, subagent, swarm, email, side-panel, goal, MCP, other) |
| `command_*_used` | `true/false` | Whether a slash-command family was used in-session |
| `workflow_*_used` | `true/false` | Whether the session looked like coding, research, testing, background, subagent, or swarm work |
| `unique_mcp_servers` | `2` | Count of distinct MCP servers touched in-session |
| `session_success` | `true` | Coarse success proxy based on outcomes like responses, successful tools, tests, or edits |
| `abandoned_before_response` | `false` | Whether the user engaged but got no successful outcome before ending |
| `transport_https` | `2` | Number of provider requests sent over HTTPS/SSE |
| `transport_persistent_ws_fresh` | `1` | Number of fresh persistent WebSocket requests |
| `transport_persistent_ws_reuse` | `5` | Number of turns that reused an existing persistent WebSocket |
| `transport_cli_subprocess` | `0` | Number of requests sent through a CLI subprocess transport |
| `transport_native_http2` | `0` | Number of requests sent through native HTTP/2 transports |
| `transport_other` | `0` | Number of requests using any other transport |
| `project_repo_present` | `true` | Whether the working directory looked like a repo |
| `project_lang_*` | `true/false` | Coarse project-language buckets (Rust, JS/TS, Python, Go, Markdown, mixed) |
| `days_since_install` | `12` | Rough install age in days |
| `active_days_7d` / `active_days_30d` | `4` / `9` | How many distinct active days this install had recently |
| `resumed_session` | `false` | Whether this session was resumed |
| `end_reason` | `"normal_exit"` | Coarse end reason |
| `errors` | `{"provider_timeout": 0, ...}` | Count of errors by category |

### Shared Event Metadata

Most events also carry a few coarse quality / cleanup fields:

| Field | Example | Purpose |
|-------|---------|----------|
| `event_id` | `"uuid"` | Deduplication |
| `session_id` | `"uuid"` | Joins session-scoped events together |
| `schema_version` | `2` | Forward-compatible parsing |
| `build_channel` | `"release"` / `"selfdev"` / `"local_build"` | Filter out dev/test usage |
| `is_git_checkout` | `true/false` | Distinguish source-tree usage from installed usage |
| `is_ci` | `true/false` | Filter CI noise |
| `ran_from_cargo` | `true/false` | Filter local dev launches |

## What We Do NOT Collect

- No file paths, project names, or directory structures
- No code, prompts, or LLM responses
- No tool inputs or tool outputs
- No MCP server names or configurations
- No IP addresses (Cloudflare Workers don't log these by default)
- No personal information of any kind
- No error messages or stack traces in telemetry (only coarse categories and end reasons)

The UUID is randomly generated on first run and stored at `~/.jcode/telemetry_id`. It is not derived from your machine, username, email, or any identifiable information.

## How It Works

1. On first launch, jcode generates a random UUID and sends an `install` event
2. When a session begins, jcode sends a `session_start` event
3. When a session ends normally, jcode sends a `session_end` event with coarse session metrics
4. When auth succeeds, jcode sends a coarse `auth_success` event for activation-funnel analysis
5. When jcode detects a version change, it sends an `upgrade` event
6. On best-effort crash/signal handling, jcode sends a `session_crash` event
7. jcode may also send one-off onboarding milestone events and explicit feedback events when triggered
8. Requests are fire-and-forget HTTP POSTs that don't block normal usage (install/session shutdown have short bounded blocking timeouts)
9. If a request fails (offline, firewall, etc.), jcode silently continues - no retries, no queuing

The telemetry endpoint is a Cloudflare Worker that stores events in a D1 database. The source code for the worker is in [`telemetry-worker/`](./telemetry-worker/).

## How to Opt Out

Any of these methods will disable telemetry completely:

```bash
# Option 1: Environment variable
export JCODE_NO_TELEMETRY=1

# Option 2: Standard DO_NOT_TRACK (https://consoledonottrack.com/)
export DO_NOT_TRACK=1

# Option 3: File-based opt-out
touch ~/.jcode/no_telemetry
```

When opted out, zero network requests are made. The telemetry module short-circuits immediately.

## Verification

This is open source. The entire telemetry implementation is in [`src/telemetry.rs`](./src/telemetry.rs) - you can read exactly what gets sent. There are no other network calls related to telemetry anywhere in the codebase.

## Data Retention

Telemetry data is used in aggregate only (install count, active users, provider distribution, session success/crash rates, feature-level counts). Individual event records are retained for up to 12 months and then deleted.
