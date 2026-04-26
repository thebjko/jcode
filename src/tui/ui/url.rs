use regex::Regex;
use std::sync::OnceLock;

pub(crate) fn url_regex() -> Option<&'static Regex> {
    static URL_REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    URL_REGEX
        .get_or_init(|| Regex::new(r#"(?i)(?:https?://|mailto:|file://)[^\s<>'\"]+"#).ok())
        .as_ref()
}

pub(crate) fn trim_url_candidate(candidate: &str) -> &str {
    let mut trimmed = candidate;
    loop {
        let next = if trimmed.ends_with(['.', ',', ';', ':', '!', '?'])
            || (trimmed.ends_with(')')
                && trimmed.matches(')').count() > trimmed.matches('(').count())
            || (trimmed.ends_with(']')
                && trimmed.matches(']').count() > trimmed.matches('[').count())
            || (trimmed.ends_with('}')
                && trimmed.matches('}').count() > trimmed.matches('{').count())
        {
            &trimmed[..trimmed.len() - 1]
        } else {
            trimmed
        };

        if next.len() == trimmed.len() {
            return trimmed;
        }
        trimmed = next;
    }
}

#[cfg(test)]
mod tests {
    use super::{trim_url_candidate, url_regex};

    #[test]
    fn url_regex_matches_supported_link_schemes() {
        let regex = url_regex();
        assert!(regex.is_some(), "test URL regex should initialize");
        let Some(regex) = regex else {
            return;
        };
        let text = "See https://example.com, mailto:user@example.com, and file:///tmp/a.txt";
        let matches: Vec<&str> = regex.find_iter(text).map(|mat| mat.as_str()).collect();

        assert_eq!(
            matches,
            vec![
                "https://example.com,",
                "mailto:user@example.com,",
                "file:///tmp/a.txt"
            ]
        );
    }

    #[test]
    fn trim_url_candidate_removes_trailing_sentence_punctuation() {
        assert_eq!(
            trim_url_candidate("https://example.com,"),
            "https://example.com"
        );
        assert_eq!(
            trim_url_candidate("https://example.com?!"),
            "https://example.com"
        );
        assert_eq!(
            trim_url_candidate("mailto:user@example.com."),
            "mailto:user@example.com"
        );
    }

    #[test]
    fn trim_url_candidate_preserves_balanced_closing_delimiters() {
        assert_eq!(
            trim_url_candidate("https://example.com/path_(draft)"),
            "https://example.com/path_(draft)"
        );
        assert_eq!(
            trim_url_candidate("https://example.com/path_(draft))."),
            "https://example.com/path_(draft)"
        );
        assert_eq!(
            trim_url_candidate("https://example.com/[docs]]"),
            "https://example.com/[docs]"
        );
    }
}
