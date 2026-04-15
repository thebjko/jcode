use super::*;

#[test]
fn pkce_verifier_and_challenge_are_different() {
    let (verifier, challenge) = generate_pkce();
    assert_ne!(verifier, challenge);
    assert_eq!(verifier.len(), 64);
    assert!(!challenge.is_empty());
}

#[test]
fn pkce_challenge_is_base64url() {
    let (_, challenge) = generate_pkce();
    assert!(!challenge.contains('+'));
    assert!(!challenge.contains('/'));
    assert!(!challenge.contains('='));
}

#[test]
fn pkce_challenge_is_sha256_of_verifier() {
    let (verifier, challenge) = generate_pkce();
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    let expected = URL_SAFE_NO_PAD.encode(hash);
    assert_eq!(challenge, expected);
}

#[test]
fn pkce_generates_unique_values() {
    let (v1, c1) = generate_pkce();
    let (v2, c2) = generate_pkce();
    assert_ne!(v1, v2);
    assert_ne!(c1, c2);
}

#[test]
fn state_is_random_hex() {
    let state = generate_state();
    assert_eq!(state.len(), 32);
    assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn state_generates_unique_values() {
    let s1 = generate_state();
    let s2 = generate_state();
    assert_ne!(s1, s2);
}

#[test]
fn oauth_tokens_serialization_roundtrip() {
    let tokens = OAuthTokens {
        access_token: "at_abc".to_string(),
        refresh_token: "rt_def".to_string(),
        expires_at: 1234567890,
        id_token: Some("idt_ghi".to_string()),
    };
    let json = serde_json::to_string(&tokens).unwrap();
    let parsed: OAuthTokens = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.access_token, "at_abc");
    assert_eq!(parsed.refresh_token, "rt_def");
    assert_eq!(parsed.expires_at, 1234567890);
    assert_eq!(parsed.id_token, Some("idt_ghi".to_string()));
}

#[test]
fn oauth_tokens_without_id_token() {
    let tokens = OAuthTokens {
        access_token: "at".to_string(),
        refresh_token: "rt".to_string(),
        expires_at: 0,
        id_token: None,
    };
    let json = serde_json::to_string(&tokens).unwrap();
    assert!(!json.contains("id_token"));
    let parsed: OAuthTokens = serde_json::from_str(&json).unwrap();
    assert!(parsed.id_token.is_none());
}

#[test]
fn save_openai_tokens_uses_jcode_home_sandbox() {
    let _lock = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());

    let tokens = OAuthTokens {
        access_token: "at_sandbox".to_string(),
        refresh_token: "rt_sandbox".to_string(),
        expires_at: 1234567890,
        id_token: Some("id_sandbox".to_string()),
    };

    save_openai_tokens(&tokens).unwrap();

    let auth_path = temp.path().join("openai-auth.json");
    assert!(auth_path.exists(), "expected {}", auth_path.display());

    let creds = crate::auth::codex::load_credentials().unwrap();
    assert_eq!(creds.access_token, "at_sandbox");
    assert_eq!(creds.refresh_token, "rt_sandbox");
    assert_eq!(creds.id_token.as_deref(), Some("id_sandbox"));
    assert_eq!(creds.expires_at, Some(1234567890));
}

#[test]
fn claude_oauth_constants() {
    assert!(!claude::CLIENT_ID.is_empty());
    assert!(claude::AUTHORIZE_URL.starts_with("https://"));
    assert!(claude::TOKEN_URL.starts_with("https://"));
    assert!(claude::PROFILE_URL.starts_with("https://"));
    assert!(claude::REDIRECT_URI.starts_with("https://"));
    assert!(!claude::SCOPES.is_empty());
}

#[tokio::test]
async fn fetch_claude_profile_email_reads_account_email() {
    let body = serde_json::json!({
        "account": {
            "email": "user@example.com"
        }
    })
    .to_string();
    let (port, _handle) = mock_token_server(200, &body).await;

    let url = format!("http://127.0.0.1:{}/api/oauth/profile", port);
    let email = fetch_claude_profile_email_at_url("token", &url)
        .await
        .unwrap();

    assert_eq!(email, Some("user@example.com".to_string()));
}

#[tokio::test]
async fn fetch_claude_profile_email_handles_missing_email() {
    let body = serde_json::json!({
        "account": {}
    })
    .to_string();
    let (port, _handle) = mock_token_server(200, &body).await;

    let url = format!("http://127.0.0.1:{}/api/oauth/profile", port);
    let email = fetch_claude_profile_email_at_url("token", &url)
        .await
        .unwrap();

    assert!(email.is_none());
}

#[tokio::test]
async fn fetch_claude_profile_email_propagates_http_error() {
    let body = serde_json::json!({
        "error": "bad_token"
    })
    .to_string();
    let (port, _handle) = mock_token_server(401, &body).await;

    let url = format!("http://127.0.0.1:{}/api/oauth/profile", port);
    let err = fetch_claude_profile_email_at_url("token", &url)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("Profile fetch failed"));
}

#[test]
fn openai_oauth_constants() {
    assert!(!openai::CLIENT_ID.is_empty());
    assert!(openai::AUTHORIZE_URL.starts_with("https://"));
    assert!(openai::TOKEN_URL.starts_with("https://"));
    assert!(openai::redirect_uri(openai::DEFAULT_PORT).starts_with("http"));
    assert!(!openai::SCOPES.is_empty());
}

#[tokio::test]
async fn wait_for_callback_async_parses_code() {
    let state = "test_state_abc";
    let listener = bind_callback_listener(0).unwrap();
    let port = listener.local_addr().unwrap().port();

    let state_clone = state.to_string();
    let handle =
        tokio::spawn(
            async move { wait_for_callback_async_on_listener(listener, &state_clone).await },
        );

    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    stream
        .write_all(
            format!(
                "GET /callback?code=test_code_123&state={} HTTP/1.1\r\nHost: localhost\r\n\r\n",
                state
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let result = handle.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "test_code_123");
}

#[tokio::test]
async fn wait_for_callback_async_on_prebound_listener_parses_code() {
    let state = "test_state_prebound";
    let listener = bind_callback_listener(0).unwrap();
    let port = listener.local_addr().unwrap().port();

    let state_clone = state.to_string();
    let handle =
        tokio::spawn(
            async move { wait_for_callback_async_on_listener(listener, &state_clone).await },
        );

    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    stream
        .write_all(
            format!(
                "GET /callback?code=prebound_code&state={} HTTP/1.1\r\nHost: localhost\r\n\r\n",
                state
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let result = handle.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "prebound_code");
}

#[tokio::test]
async fn wait_for_callback_async_ignores_wrong_state_until_valid_callback() {
    let listener = bind_callback_listener(0).unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        wait_for_callback_async_on_listener(listener, "expected_state").await
    });

    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    stream
        .write_all(
            b"GET /callback?code=code123&state=wrong_state HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .unwrap();
    drop(stream);

    let mut valid_stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    valid_stream
        .write_all(
            b"GET /callback?code=code123&state=expected_state HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .unwrap();

    let result = handle.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "code123");
}

#[tokio::test]
async fn wait_for_callback_async_ignores_missing_code_until_valid_callback() {
    let listener = bind_callback_listener(0).unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle =
        tokio::spawn(
            async move { wait_for_callback_async_on_listener(listener, "state123").await },
        );

    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    stream
        .write_all(b"GET /callback?state=state123 HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    drop(stream);

    let mut valid_stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    valid_stream
        .write_all(
            b"GET /callback?code=valid_code&state=state123 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .unwrap();

    let result = handle.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "valid_code");
}

#[tokio::test]
async fn wait_for_callback_async_surfaces_provider_error() {
    let listener = bind_callback_listener(0).unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        wait_for_callback_async_on_listener(listener, "expected_state").await
    });

    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    stream
            .write_all(
                b"GET /callback?error=access_denied&state=expected_state HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .await
            .unwrap();

    let result = handle.await.unwrap();
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("OAuth provider returned error")
    );
}
