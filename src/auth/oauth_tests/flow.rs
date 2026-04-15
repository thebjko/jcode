use super::*;

#[test]
fn claude_exchange_request_uses_form_urlencoded() {
    let (_url, content_type, _body) =
        build_claude_exchange_request("code123", "verifier456", claude::REDIRECT_URI, None);
    assert_eq!(content_type, "application/x-www-form-urlencoded");
    assert_ne!(content_type, "application/json");
}

#[test]
fn claude_exchange_request_body_is_not_json() {
    let (_url, _ct, body) =
        build_claude_exchange_request("code123", "verifier456", claude::REDIRECT_URI, None);
    let body_str = String::from_utf8(body).unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&body_str).is_err(),
        "Body must NOT be valid JSON (should be form-urlencoded)"
    );
}

#[test]
fn claude_refresh_request_uses_form_urlencoded() {
    let (_url, content_type, _body) = build_claude_refresh_request("rt_test");
    assert_eq!(content_type, "application/x-www-form-urlencoded");
    assert_ne!(content_type, "application/json");
}

#[test]
fn claude_refresh_request_body_is_not_json() {
    let (_url, _ct, body) = build_claude_refresh_request("rt_test");
    let body_str = String::from_utf8(body).unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&body_str).is_err(),
        "Body must NOT be valid JSON (should be form-urlencoded)"
    );
}

// ========================
// Claude exchange request body validation
// ========================

#[test]
fn claude_exchange_request_contains_required_fields() {
    let (_url, _ct, body) = build_claude_exchange_request(
        "auth_code_xyz",
        "verifier_abc",
        "https://example.com/callback",
        None,
    );
    let body_str = String::from_utf8(body).unwrap();
    let pairs: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body_str.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(pairs.get("grant_type").unwrap(), "authorization_code");
    assert_eq!(pairs.get("client_id").unwrap(), claude::CLIENT_ID);
    assert_eq!(pairs.get("code").unwrap(), "auth_code_xyz");
    assert_eq!(pairs.get("code_verifier").unwrap(), "verifier_abc");
    assert_eq!(
        pairs.get("redirect_uri").unwrap(),
        "https://example.com/callback"
    );
    assert_eq!(pairs.get("state").unwrap(), "verifier_abc");
}

#[test]
fn claude_exchange_request_includes_state_when_present() {
    let (_url, _ct, body) = build_claude_exchange_request(
        "code",
        "verifier",
        claude::REDIRECT_URI,
        Some("state_value"),
    );
    let body_str = String::from_utf8(body).unwrap();
    let pairs: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body_str.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(pairs.get("state").unwrap(), "state_value");
}

#[test]
fn claude_exchange_request_targets_correct_url() {
    let (url, _ct, _body) = build_claude_exchange_request("c", "v", claude::REDIRECT_URI, None);
    assert_eq!(url, "https://console.anthropic.com/v1/oauth/token");
}

// ========================
// Claude refresh request body validation
// ========================

#[test]
fn claude_refresh_request_contains_required_fields() {
    let (_url, _ct, body) = build_claude_refresh_request("rt_refresh_token_value");
    let body_str = String::from_utf8(body).unwrap();
    let pairs: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body_str.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(pairs.get("grant_type").unwrap(), "refresh_token");
    assert_eq!(
        pairs.get("refresh_token").unwrap(),
        "rt_refresh_token_value"
    );
    assert_eq!(pairs.get("client_id").unwrap(), claude::CLIENT_ID);
}

#[test]
fn claude_refresh_request_targets_correct_url() {
    let (url, _ct, _body) = build_claude_refresh_request("rt");
    assert_eq!(url, "https://console.anthropic.com/v1/oauth/token");
}

// ========================
// OpenAI exchange request validation
// ========================

#[test]
fn openai_exchange_request_uses_form_urlencoded() {
    let (_url, content_type, _body) =
        build_openai_exchange_request("code", "verifier", "http://localhost:1455/auth/callback");
    assert_eq!(content_type, "application/x-www-form-urlencoded");
}

#[test]
fn openai_exchange_request_contains_required_fields() {
    let (_url, _ct, body) = build_openai_exchange_request(
        "oai_code_123",
        "oai_verifier",
        "http://localhost:1455/auth/callback",
    );
    let body_str = String::from_utf8(body).unwrap();
    assert!(body_str.contains("grant_type=authorization_code"));
    assert!(body_str.contains(&format!("client_id={}", openai::CLIENT_ID)));
    assert!(body_str.contains("code=oai_code_123"));
    assert!(body_str.contains("code_verifier=oai_verifier"));
    assert!(body_str.contains("redirect_uri="));
}

#[test]
fn openai_exchange_request_targets_correct_url() {
    let (url, _ct, _body) = build_openai_exchange_request("c", "v", "http://localhost/cb");
    assert_eq!(url, "https://auth.openai.com/oauth/token");
}

// ========================
// OpenAI refresh request validation
// ========================

#[test]
fn openai_refresh_request_uses_form_urlencoded() {
    let (_url, content_type, _body) = build_openai_refresh_request("rt_oai");
    assert_eq!(content_type, "application/x-www-form-urlencoded");
}

#[test]
fn openai_refresh_request_contains_required_fields() {
    let (_url, _ct, body) = build_openai_refresh_request("rt_oai_value");
    let body_str = String::from_utf8(body).unwrap();
    assert!(body_str.contains("grant_type=refresh_token"));
    assert!(body_str.contains(&format!("client_id={}", openai::CLIENT_ID)));
    assert!(body_str.contains("refresh_token=rt_oai_value"));
}

#[test]
fn openai_refresh_request_targets_correct_url() {
    let (url, _ct, _body) = build_openai_refresh_request("rt");
    assert_eq!(url, "https://auth.openai.com/oauth/token");
}

// ========================
// Auth URL construction
// ========================

#[test]
fn claude_auth_url_contains_required_params() {
    let (verifier, challenge) = generate_pkce();
    let auth_url = format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        claude::AUTHORIZE_URL,
        claude::CLIENT_ID,
        urlencoding::encode(claude::REDIRECT_URI),
        urlencoding::encode(claude::SCOPES),
        challenge,
        verifier,
    );
    let parsed = url::Url::parse(&auth_url).unwrap();
    let params: std::collections::HashMap<String, String> = parsed
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(params.get("code").unwrap(), "true");
    assert_eq!(params.get("client_id").unwrap(), claude::CLIENT_ID);
    assert_eq!(params.get("response_type").unwrap(), "code");
    assert_eq!(params.get("redirect_uri").unwrap(), claude::REDIRECT_URI);
    assert_eq!(params.get("scope").unwrap(), claude::SCOPES);
    assert_eq!(params.get("code_challenge").unwrap(), &challenge);
    assert_eq!(params.get("code_challenge_method").unwrap(), "S256");
    assert_eq!(params.get("state").unwrap(), &verifier);
}

#[test]
fn openai_auth_url_contains_required_params() {
    let (_verifier, challenge) = generate_pkce();
    let state = generate_state();
    let redirect_uri = openai::redirect_uri(openai::DEFAULT_PORT);
    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        openai::AUTHORIZE_URL,
        openai::CLIENT_ID,
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(openai::SCOPES),
        challenge,
        state,
    );
    let parsed = url::Url::parse(&auth_url).unwrap();
    let params: std::collections::HashMap<String, String> = parsed
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(params.get("response_type").unwrap(), "code");
    assert_eq!(params.get("client_id").unwrap(), openai::CLIENT_ID);
    assert_eq!(params.get("redirect_uri").unwrap(), &redirect_uri);
    assert_eq!(params.get("scope").unwrap(), openai::SCOPES);
    assert_eq!(params.get("code_challenge").unwrap(), &challenge);
    assert_eq!(params.get("code_challenge_method").unwrap(), "S256");
    assert_eq!(params.get("state").unwrap(), &state);
}

#[test]
fn claude_auth_url_with_dynamic_redirect_uri() {
    let (verifier, challenge) = generate_pkce();
    let dynamic_redirect = "http://localhost:34531/callback";
    let auth_url = format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        claude::AUTHORIZE_URL,
        claude::CLIENT_ID,
        urlencoding::encode(dynamic_redirect),
        urlencoding::encode(claude::SCOPES),
        challenge,
        verifier,
    );
    let parsed = url::Url::parse(&auth_url).unwrap();
    let params: std::collections::HashMap<String, String> = parsed
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(params.get("redirect_uri").unwrap(), dynamic_redirect);
}

// ========================
// Code parsing (plain code, URL, code#state)
// ========================

#[test]
fn parse_plain_auth_code() {
    let input = "abc123def456";
    let (raw_code, state) = parse_claude_code_input(input).unwrap();
    assert_eq!(raw_code, "abc123def456");
    assert!(state.is_none());
}

#[test]
fn parse_code_from_url() {
    let input = "https://example.com/callback?code=mycode123&state=mystate";
    let (raw_code, state) = parse_claude_code_input(input).unwrap();
    assert_eq!(raw_code, "mycode123");
    assert_eq!(state, Some("mystate".to_string()));
}

#[test]
fn parse_code_from_query_string() {
    let input = "code=mycode456&state=s";
    let (raw_code, state) = parse_claude_code_input(input).unwrap();
    assert_eq!(raw_code, "mycode456");
    assert_eq!(state, Some("s".to_string()));
}

#[test]
fn parse_code_hash_state_format() {
    let raw_code = "authcode789#statevalue";
    let (code, state) = parse_claude_code_input(raw_code).unwrap();
    assert_eq!(code, "authcode789");
    assert_eq!(state, Some("statevalue".to_string()));
}

#[test]
fn parse_code_without_hash() {
    let raw_code = "authcode_no_hash";
    let (code, state) = parse_claude_code_input(raw_code).unwrap();
    assert_eq!(code, "authcode_no_hash");
    assert!(state.is_none());
}

#[test]
fn parse_code_trims_input_whitespace() {
    let input = "   authcode_trim   ";
    let (code, state) = parse_claude_code_input(input).unwrap();
    assert_eq!(code, "authcode_trim");
    assert!(state.is_none());
}

#[test]
fn parse_code_url_with_whitespace_extracts_state() {
    let input = "   https://example.com/callback?code=mycode&state=mystate   ";
    let (code, state) = parse_claude_code_input(input).unwrap();
    assert_eq!(code, "mycode");
    assert_eq!(state, Some("mystate".to_string()));
}

#[test]
fn parse_code_rejects_empty_input() {
    let err = parse_claude_code_input("   ").expect_err("empty input should fail");
    assert!(err.to_string().contains("No authorization code provided"));
}

#[test]
fn parse_code_rejects_empty_code_query_param() {
    let err = parse_claude_code_input("code=&state=abc")
        .expect_err("empty code query parameter should fail");
    assert!(err.to_string().contains("No authorization code provided"));
}

#[test]
fn parse_callback_input_requires_state() {
    let err = parse_callback_input_with_state("just-a-code")
        .expect_err("plain code should not satisfy stateful callback parsing");
    assert!(err.to_string().contains("full callback URL"));
}

#[test]
fn parse_callback_input_extracts_code_and_state() {
    let (code, state) = parse_callback_input_with_state(
        "http://localhost:1455/auth/callback?code=mycode&state=mystate",
    )
    .unwrap();
    assert_eq!(code, "mycode");
    assert_eq!(state, "mystate");
}

#[test]
fn claude_redirect_uri_uses_manual_callback_for_console_url() {
    let selected = claude_redirect_uri_for_input(
        "https://console.anthropic.com/oauth/code/callback?code=abc&state=xyz",
        "http://localhost:9999/callback",
    );
    assert_eq!(selected, claude::REDIRECT_URI);
}

#[test]
fn claude_redirect_uri_keeps_localhost_fallback_for_raw_code() {
    let selected = claude_redirect_uri_for_input("abc123", "http://localhost:9999/callback");
    assert_eq!(selected, "http://localhost:9999/callback");
}

// ========================
// Mock server integration: Claude exchange
// ========================

#[tokio::test]
async fn claude_exchange_mock_server_receives_form_urlencoded() {
    let success_body = serde_json::json!({
        "access_token": "at_mock",
        "refresh_token": "rt_mock",
        "expires_in": 3600,
        "id_token": "idt_mock"
    })
    .to_string();
    let (port, handle) = mock_token_server(200, &success_body).await;

    let url = format!("http://127.0.0.1:{}/v1/oauth/token", port);
    let result = exchange_code_at_url(&url, "code123", "verifier456", "https://redir", None)
        .await
        .unwrap();

    assert_eq!(result.access_token, "at_mock");
    assert_eq!(result.refresh_token, "rt_mock");
    assert_eq!(result.id_token, Some("idt_mock".to_string()));

    let (method, _path, headers, body) = handle.await.unwrap();
    assert_eq!(method, "POST");
    assert_eq!(
        headers.get("content-type").unwrap(),
        "application/x-www-form-urlencoded"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(&body).is_err(),
        "Body must be form-urlencoded, not JSON"
    );
    let pairs: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(pairs.get("grant_type").unwrap(), "authorization_code");
    assert_eq!(pairs.get("code").unwrap(), "code123");
    assert_eq!(pairs.get("code_verifier").unwrap(), "verifier456");
    assert_eq!(pairs.get("state").unwrap(), "verifier456");
}

#[tokio::test]
async fn claude_exchange_mock_server_with_state() {
    let success_body = serde_json::json!({
        "access_token": "at",
        "refresh_token": "rt",
        "expires_in": 3600
    })
    .to_string();
    let (port, handle) = mock_token_server(200, &success_body).await;

    let url = format!("http://127.0.0.1:{}/v1/oauth/token", port);
    let _ = exchange_code_at_url(&url, "c", "v", "https://r", Some("my_state"))
        .await
        .unwrap();

    let (_method, _path, _headers, body) = handle.await.unwrap();
    let pairs: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(pairs.get("state").unwrap(), "my_state");
}

#[tokio::test]
async fn claude_exchange_uses_state_from_url_query_when_present() {
    let success_body = serde_json::json!({
        "access_token": "at",
        "refresh_token": "rt",
        "expires_in": 3600
    })
    .to_string();
    let (port, handle) = mock_token_server(200, &success_body).await;

    let url = format!("http://127.0.0.1:{}/v1/oauth/token", port);
    let _ = exchange_claude_code_at_url(
        &url,
        "query_state",
        "https://example.com/callback?code=test_code&state=query_state",
        "https://r",
    )
    .await
    .unwrap();

    let (_method, _path, _headers, body) = handle.await.unwrap();
    let pairs: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(pairs.get("state").unwrap(), "query_state");
    assert_eq!(pairs.get("code").unwrap(), "test_code");
}

#[tokio::test]
async fn claude_exchange_rejects_state_mismatch() {
    let result = exchange_claude_code_at_url(
        "http://127.0.0.1:1/v1/oauth/token",
        "expected_state",
        "https://example.com/callback?code=test_code&state=wrong_state",
        "https://r",
    )
    .await;

    let err = result.expect_err("state mismatch should fail before token exchange");
    assert!(
        err.to_string().contains("OAuth state mismatch"),
        "unexpected error: {err}"
    );
}

#[test]
fn openai_docs_reference_current_callback_uri() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let expected = openai::default_redirect_uri();
    for relative in ["OAUTH.md", "README.md"] {
        let content = std::fs::read_to_string(repo_root.join(relative))
            .unwrap_or_else(|e| panic!("failed to read {relative}: {e}"));
        assert!(
            content.contains(&expected),
            "{relative} should mention current OpenAI callback URI {expected}"
        );
    }
}

#[tokio::test]
async fn openai_callback_input_rejects_state_mismatch() {
    let err = exchange_openai_callback_input(
        "verifier",
        "http://localhost:1455/auth/callback?code=abc123&state=wrong_state",
        "expected_state",
        "http://localhost:1455/auth/callback",
    )
    .await
    .expect_err("state mismatch should fail before token exchange");

    assert!(
        err.to_string().contains("OAuth state mismatch"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn claude_exchange_falls_back_to_verifier_when_input_has_no_state() {
    let success_body = serde_json::json!({
        "access_token": "at",
        "refresh_token": "rt",
        "expires_in": 3600
    })
    .to_string();
    let (port, handle) = mock_token_server(200, &success_body).await;

    let url = format!("http://127.0.0.1:{}/v1/oauth/token", port);
    let _ = exchange_claude_code_at_url(&url, "verifier_only", "plain_code", "https://r")
        .await
        .unwrap();

    let (_method, _path, _headers, body) = handle.await.unwrap();
    let pairs: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(pairs.get("state").unwrap(), "verifier_only");
    assert_eq!(pairs.get("code").unwrap(), "plain_code");
}

#[tokio::test]
async fn claude_exchange_uses_verifier_when_input_state_is_empty() {
    let success_body = serde_json::json!({
        "access_token": "at",
        "refresh_token": "rt",
        "expires_in": 3600
    })
    .to_string();
    let (port, handle) = mock_token_server(200, &success_body).await;

    let url = format!("http://127.0.0.1:{}/v1/oauth/token", port);
    let _ = exchange_claude_code_at_url(&url, "verifier_only", "plain_code#", "https://r")
        .await
        .unwrap();

    let (_method, _path, _headers, body) = handle.await.unwrap();
    let pairs: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(pairs.get("state").unwrap(), "verifier_only");
}

#[tokio::test]
async fn claude_exchange_mock_server_error_propagates() {
    let error_body =
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid"}}"#;
    let (port, _handle) = mock_token_server(400, error_body).await;

    let url = format!("http://127.0.0.1:{}/v1/oauth/token", port);
    let result = exchange_code_at_url(&url, "c", "v", "https://r", None).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("Token exchange failed"));
}

// ========================
// Mock server integration: Claude refresh
// ========================

#[tokio::test]
async fn claude_refresh_mock_server_receives_form_urlencoded() {
    let success_body = serde_json::json!({
        "access_token": "at_refreshed",
        "refresh_token": "rt_refreshed",
        "expires_in": 7200
    })
    .to_string();
    let (port, handle) = mock_token_server(200, &success_body).await;

    let url = format!("http://127.0.0.1:{}/v1/oauth/token", port);
    let result = refresh_tokens_at_url(&url, "old_refresh_token")
        .await
        .unwrap();

    assert_eq!(result.access_token, "at_refreshed");
    assert_eq!(result.refresh_token, "rt_refreshed");

    let (method, _path, headers, body) = handle.await.unwrap();
    assert_eq!(method, "POST");
    assert_eq!(
        headers.get("content-type").unwrap(),
        "application/x-www-form-urlencoded"
    );
    let pairs: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(pairs.get("grant_type").unwrap(), "refresh_token");
    assert_eq!(pairs.get("refresh_token").unwrap(), "old_refresh_token");
    assert_eq!(pairs.get("client_id").unwrap(), claude::CLIENT_ID);
}

#[tokio::test]
async fn claude_refresh_mock_server_error_propagates() {
    let error_body = r#"{"error":"invalid_grant"}"#;
    let (port, _handle) = mock_token_server(400, error_body).await;

    let url = format!("http://127.0.0.1:{}/v1/oauth/token", port);
    let result = refresh_tokens_at_url(&url, "expired_token").await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Token refresh failed")
    );
}

// ========================
// Regression: JSON body must be rejected
// ========================

#[tokio::test]
async fn regression_json_body_rejected_by_strict_server() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let mut request_line = String::new();
        reader.read_line(&mut request_line).await.unwrap();

        let mut content_type = String::new();
        let mut content_length: usize = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let k = k.trim().to_lowercase();
                if k == "content-type" {
                    content_type = v.trim().to_string();
                }
                if k == "content-length" {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).await.unwrap();
        }

        if content_type.contains("application/json") {
            let error_resp = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid request format"}}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                error_resp.len(),
                error_resp
            );
            writer.write_all(response.as_bytes()).await.unwrap();
            return false;
        }

        let success = serde_json::json!({
            "access_token": "at",
            "refresh_token": "rt",
            "expires_in": 3600
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            success.len(),
            success
        );
        writer.write_all(response.as_bytes()).await.unwrap();
        true
    });

    let url = format!("http://127.0.0.1:{}/v1/oauth/token", port);
    let result = exchange_code_at_url(&url, "code", "verifier", "https://redir", None).await;

    let server_accepted = handle.await.unwrap();
    assert!(
        server_accepted,
        "Server should have accepted the form-urlencoded request"
    );
    assert!(
        result.is_ok(),
        "Exchange should succeed with form-urlencoded"
    );
}

// ========================
// Token response parsing
// ========================

#[tokio::test]
async fn exchange_parses_optional_id_token() {
    let body_with = serde_json::json!({
        "access_token": "at",
        "refresh_token": "rt",
        "expires_in": 3600,
        "id_token": "idt_value"
    })
    .to_string();
    let (port, _handle) = mock_token_server(200, &body_with).await;
    let url = format!("http://127.0.0.1:{}/token", port);
    let result = exchange_code_at_url(&url, "c", "v", "r", None)
        .await
        .unwrap();
    assert_eq!(result.id_token, Some("idt_value".to_string()));
}

#[tokio::test]
async fn exchange_handles_missing_id_token() {
    let body_without = serde_json::json!({
        "access_token": "at",
        "refresh_token": "rt",
        "expires_in": 3600
    })
    .to_string();
    let (port, _handle) = mock_token_server(200, &body_without).await;
    let url = format!("http://127.0.0.1:{}/token", port);
    let result = exchange_code_at_url(&url, "c", "v", "r", None)
        .await
        .unwrap();
    assert!(result.id_token.is_none());
}

#[tokio::test]
async fn exchange_sets_expires_at_in_future() {
    let body = serde_json::json!({
        "access_token": "at",
        "refresh_token": "rt",
        "expires_in": 3600
    })
    .to_string();
    let (port, _handle) = mock_token_server(200, &body).await;
    let url = format!("http://127.0.0.1:{}/token", port);
    let before = chrono::Utc::now().timestamp_millis();
    let result = exchange_code_at_url(&url, "c", "v", "r", None)
        .await
        .unwrap();
    let after = chrono::Utc::now().timestamp_millis();
    assert!(result.expires_at >= before + 3600 * 1000);
    assert!(result.expires_at <= after + 3600 * 1000);
}

// ========================
// Special characters / URL encoding
// ========================

#[test]
fn claude_exchange_handles_special_chars_in_code() {
    let (_url, _ct, body) = build_claude_exchange_request(
        "code+with/special=chars&more",
        "verifier",
        claude::REDIRECT_URI,
        None,
    );
    let body_str = String::from_utf8(body).unwrap();
    let pairs: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body_str.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(pairs.get("code").unwrap(), "code+with/special=chars&more");
}

#[test]
fn openai_redirect_uri_format() {
    let uri = openai::redirect_uri(1455);
    assert_eq!(uri, "http://localhost:1455/auth/callback");
    let uri2 = openai::redirect_uri(9999);
    assert_eq!(uri2, "http://localhost:9999/auth/callback");
}

// ========================
// All providers use form-urlencoded (comprehensive check)
// ========================

#[test]
fn all_token_requests_use_form_urlencoded_not_json() {
    let checks: Vec<(&str, String)> = vec![
        (
            "claude_exchange",
            build_claude_exchange_request("c", "v", "r", None).1,
        ),
        (
            "claude_exchange_with_state",
            build_claude_exchange_request("c", "v", "r", Some("s")).1,
        ),
        ("claude_refresh", build_claude_refresh_request("rt").1),
        (
            "openai_exchange",
            build_openai_exchange_request("c", "v", "r").1,
        ),
        ("openai_refresh", build_openai_refresh_request("rt").1),
    ];
    for (name, ct) in checks {
        assert_eq!(
            ct, "application/x-www-form-urlencoded",
            "{} must use application/x-www-form-urlencoded, got {}",
            name, ct
        );
    }
}
