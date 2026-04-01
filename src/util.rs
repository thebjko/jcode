/// Truncate a string at a valid UTF-8 character boundary.
///
/// Returns a slice of at most `max_bytes` bytes, ending at a valid char boundary.
/// This prevents panics when truncating strings that contain multi-byte characters.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Find the largest valid char boundary at or before max_bytes
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Format an anyhow error including its full cause chain.
///
/// This preserves actionable upstream details such as HTTP status/body instead of
/// only showing the outermost context message.
pub fn format_error_chain(err: &anyhow::Error) -> String {
    let mut parts = Vec::new();
    for cause in err.chain() {
        let text = cause.to_string();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if parts.last().is_some_and(|prev: &String| prev == trimmed) {
            continue;
        }
        parts.push(trimmed.to_string());
    }

    match parts.len() {
        0 => "unknown error".to_string(),
        1 => parts.remove(0),
        _ => parts.join(": "),
    }
}

/// Extract the payload from an SSE `data:` line.
///
/// The SSE spec allows an optional single space after the colon, so both
/// `data:{...}` and `data: {...}` are valid and should parse identically.
pub fn sse_data_line(line: &str) -> Option<&str> {
    line.strip_prefix("data:")
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_error_chain_includes_nested_causes() {
        let err =
            anyhow::anyhow!("HTTP 400: invalid argument").context("Gemini generateContent failed");
        assert_eq!(
            format_error_chain(&err),
            "Gemini generateContent failed: HTTP 400: invalid argument"
        );
    }

    #[test]
    fn test_format_error_chain_deduplicates_repeated_messages() {
        let err = anyhow::anyhow!("same").context("same");
        assert_eq!(format_error_chain(&err), "same");
    }

    #[test]
    fn test_truncate_ascii() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn test_truncate_multibyte() {
        // "学" is 3 bytes (E5 AD A6)
        let s = "abc学def";
        assert_eq!(truncate_str(s, 3), "abc"); // exactly before 学
        assert_eq!(truncate_str(s, 4), "abc"); // mid-char, back up
        assert_eq!(truncate_str(s, 5), "abc"); // mid-char, back up
        assert_eq!(truncate_str(s, 6), "abc学"); // exactly after 学
    }

    #[test]
    fn test_truncate_emoji() {
        // "🦀" is 4 bytes
        let s = "hi🦀bye";
        assert_eq!(truncate_str(s, 2), "hi");
        assert_eq!(truncate_str(s, 3), "hi"); // mid-emoji
        assert_eq!(truncate_str(s, 5), "hi"); // mid-emoji
        assert_eq!(truncate_str(s, 6), "hi🦀");
    }

    #[test]
    fn test_truncate_empty() {
        assert_eq!(truncate_str("", 10), "");
        assert_eq!(truncate_str("hello", 0), "");
    }

    #[test]
    fn test_sse_data_line_accepts_optional_space() {
        assert_eq!(sse_data_line("data: {\"ok\":true}"), Some("{\"ok\":true}"));
        assert_eq!(sse_data_line("data:{\"ok\":true}"), Some("{\"ok\":true}"));
        assert_eq!(sse_data_line("event: message"), None);
    }
}
