-- Telemetry health dashboard query.
-- Usage:
--   wrangler d1 execute jcode-telemetry --remote --file=health.sql

WITH counts AS (
    SELECT
        SUM(CASE WHEN event = 'install' THEN 1 ELSE 0 END) AS install_events,
        SUM(CASE WHEN event = 'session_start' THEN 1 ELSE 0 END) AS session_starts,
        SUM(CASE WHEN event = 'session_end' THEN 1 ELSE 0 END) AS session_ends,
        SUM(CASE WHEN event = 'session_crash' THEN 1 ELSE 0 END) AS session_crashes,
        COUNT(DISTINCT CASE WHEN event = 'install' THEN telemetry_id END) AS install_ids,
        COUNT(DISTINCT CASE WHEN event IN ('session_end', 'session_crash') THEN telemetry_id END) AS lifecycle_ids,
        COUNT(DISTINCT CASE WHEN event = 'session_start' THEN telemetry_id END) AS session_start_ids,
        COUNT(DISTINCT CASE WHEN event IN ('session_end', 'session_crash') AND telemetry_id NOT IN (
            SELECT telemetry_id FROM events WHERE event = 'install'
        ) THEN telemetry_id END) AS lifecycle_ids_without_install
    FROM events
),
meaningful AS (
    SELECT
        COUNT(*) AS meaningful_sessions,
        COUNT(DISTINCT telemetry_id) AS meaningful_users_30d
    FROM events
    WHERE event IN ('session_end', 'session_crash')
      AND created_at > datetime('now', '-30 days')
      AND (
        turns > 0
        OR duration_mins > 0
        OR error_provider_timeout > 0
        OR error_auth_failed > 0
        OR error_tool_error > 0
        OR error_mcp_error > 0
        OR error_rate_limited > 0
        OR provider_switches > 0
        OR model_switches > 0
        OR had_user_prompt > 0
        OR had_assistant_response > 0
        OR assistant_responses > 0
        OR tool_calls > 0
        OR tool_failures > 0
        OR executed_tool_calls > 0
        OR feature_memory_used > 0
        OR feature_swarm_used > 0
        OR feature_web_used > 0
        OR feature_email_used > 0
        OR feature_mcp_used > 0
        OR feature_side_panel_used > 0
        OR feature_goal_used > 0
        OR feature_selfdev_used > 0
        OR feature_background_used > 0
        OR feature_subagent_used > 0
      )
),
outliers AS (
    SELECT
        MAX(session_events) AS max_session_events_one_id,
        SUM(CASE WHEN rn <= 5 THEN session_events ELSE 0 END) AS top5_session_events,
        SUM(session_events) AS total_session_events
    FROM (
        SELECT telemetry_id, COUNT(*) AS session_events,
               ROW_NUMBER() OVER (ORDER BY COUNT(*) DESC) AS rn
        FROM events
        WHERE event IN ('session_end', 'session_crash')
        GROUP BY telemetry_id
    )
)
SELECT
    install_events,
    session_starts,
    session_ends,
    session_crashes,
    install_ids,
    lifecycle_ids,
    session_start_ids,
    lifecycle_ids_without_install,
    meaningful_sessions,
    meaningful_users_30d,
    max_session_events_one_id,
    top5_session_events,
    total_session_events,
    ROUND(CAST(session_ends + session_crashes AS REAL) / NULLIF(session_starts, 0), 3) AS lifecycle_completion_ratio
FROM counts, meaningful, outliers;
