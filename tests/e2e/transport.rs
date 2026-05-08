use crate::test_support::*;

#[tokio::test]
async fn test_websocket_transport_matches_unix_socket_for_subscribe_history_message_and_resume()
-> Result<()> {
    let _env = setup_test_env()?;
    let unix = run_unix_transport_scenario().await?;
    let websocket = run_websocket_transport_scenario().await?;

    assert!(
        unix.subscribe_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Ack { id } if *id == 1))
    );
    assert!(
        unix.subscribe_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 1))
    );
    assert!(
        websocket
            .subscribe_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Ack { id } if *id == 1))
    );
    assert!(
        websocket
            .subscribe_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 1))
    );

    let unix_history = unix
        .history_events
        .iter()
        .find_map(summarize_history_invariant)
        .ok_or_else(|| anyhow::anyhow!("missing unix history event"))?;
    let websocket_history = websocket
        .history_events
        .iter()
        .find_map(summarize_history_invariant)
        .ok_or_else(|| anyhow::anyhow!("missing websocket history event"))?;
    assert_eq!(
        unix_history, websocket_history,
        "history payload should match across transports"
    );

    let unix_resume = unix
        .resume_events
        .iter()
        .find_map(summarize_history_invariant)
        .ok_or_else(|| anyhow::anyhow!("missing unix resume history event"))?;
    let websocket_resume = websocket
        .resume_events
        .iter()
        .find_map(summarize_history_invariant)
        .ok_or_else(|| anyhow::anyhow!("missing websocket resume history event"))?;
    assert_eq!(
        unix_resume, websocket_resume,
        "resume history payload should match across transports"
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn test_bridge_transport_exposes_remote_server_as_local_socket() -> Result<()> {
    let _env = setup_test_env()?;
    let temp_root = tempfile::Builder::new()
        .prefix("jcode-bridge-e2e-")
        .tempdir()?;
    let server_socket_path = temp_root.path().join("server.sock");
    let debug_socket_path = temp_root.path().join("server-debug.sock");
    let bridge_socket_path = temp_root.path().join("bridge.sock");
    let token_path = temp_root.path().join("bridge-token");
    let serve_stderr_path = temp_root.path().join("bridge-serve.stderr");
    let dial_stderr_path = temp_root.path().join("bridge-dial.stderr");
    std::fs::write(&token_path, "secret\n")?;

    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new());
    let server_instance = server::Server::new_with_paths(
        provider,
        server_socket_path.clone(),
        debug_socket_path.clone(),
    );
    let server_handle = tokio::spawn(async move { server_instance.run().await });

    let bridge_port = reserve_tcp_port()?;
    let bridge_addr = format!("127.0.0.1:{bridge_port}");

    let mut bridge_serve = Command::new(env!("CARGO_BIN_EXE_jcode"))
        .arg("--no-update")
        .arg("bridge")
        .arg("serve")
        .arg("--listen")
        .arg(&bridge_addr)
        .arg("--local-socket")
        .arg(&server_socket_path)
        .arg("--token-file")
        .arg(&token_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&serve_stderr_path)?))
        .spawn()?;

    let mut bridge_dial = Command::new(env!("CARGO_BIN_EXE_jcode"))
        .arg("--no-update")
        .arg("bridge")
        .arg("dial")
        .arg("--remote")
        .arg(&bridge_addr)
        .arg("--bind")
        .arg(&bridge_socket_path)
        .arg("--token-file")
        .arg(&token_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&dial_stderr_path)?))
        .spawn()?;

    let result = async {
        server::wait_for_server_ready(&server_socket_path, Duration::from_secs(10)).await?;
        wait_for_socket(&bridge_socket_path).await?;

        let mut client = wait_for_server_client(&bridge_socket_path).await?;
        assert!(
            client.ping().await?,
            "bridge dial socket should proxy ping to the remote server"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    kill_child(&mut bridge_dial);
    kill_child(&mut bridge_serve);
    abort_server_and_cleanup(&server_handle, &server_socket_path, &debug_socket_path);

    if let Err(ref error) = result {
        eprintln!("bridge transport e2e error: {error:#}");
        if let Ok(stderr) = std::fs::read_to_string(&serve_stderr_path) {
            eprintln!("bridge serve stderr:\n{stderr}");
        }
        if let Ok(stderr) = std::fs::read_to_string(&dial_stderr_path) {
            eprintln!("bridge dial stderr:\n{stderr}");
        }
    }

    result
}

#[cfg(unix)]
#[tokio::test]
async fn test_bridge_transport_accepts_second_client_while_first_session_stays_open() -> Result<()>
{
    let _env = setup_test_env()?;
    let temp_root = tempfile::Builder::new()
        .prefix("jcode-bridge-multi-client-")
        .tempdir()?;
    let server_socket_path = temp_root.path().join("server.sock");
    let debug_socket_path = temp_root.path().join("server-debug.sock");
    let bridge_socket_path = temp_root.path().join("bridge.sock");
    let token_path = temp_root.path().join("bridge-token");
    let serve_stderr_path = temp_root.path().join("bridge-serve.stderr");
    let dial_stderr_path = temp_root.path().join("bridge-dial.stderr");
    std::fs::write(&token_path, "secret\n")?;

    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new());
    let server_instance = server::Server::new_with_paths(
        provider,
        server_socket_path.clone(),
        debug_socket_path.clone(),
    );
    let server_handle = tokio::spawn(async move { server_instance.run().await });

    let bridge_port = reserve_tcp_port()?;
    let bridge_addr = format!("127.0.0.1:{bridge_port}");

    let mut bridge_serve = Command::new(env!("CARGO_BIN_EXE_jcode"))
        .arg("--no-update")
        .arg("bridge")
        .arg("serve")
        .arg("--listen")
        .arg(&bridge_addr)
        .arg("--local-socket")
        .arg(&server_socket_path)
        .arg("--token-file")
        .arg(&token_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&serve_stderr_path)?))
        .spawn()?;

    let mut bridge_dial = Command::new(env!("CARGO_BIN_EXE_jcode"))
        .arg("--no-update")
        .arg("bridge")
        .arg("dial")
        .arg("--remote")
        .arg(&bridge_addr)
        .arg("--bind")
        .arg(&bridge_socket_path)
        .arg("--token-file")
        .arg(&token_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&dial_stderr_path)?))
        .spawn()?;

    let result = async {
        server::wait_for_server_ready(&server_socket_path, Duration::from_secs(10)).await?;
        wait_for_socket(&bridge_socket_path).await?;

        let mut client1 = wait_for_server_client(&bridge_socket_path).await?;
        let subscribe_id = client1.subscribe().await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let ServerEvent::Done { id } = client1.read_event().await? {
                    if id == subscribe_id {
                        return Ok::<_, anyhow::Error>(());
                    }
                }
            }
        })
        .await??;

        let mut client2 = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server::Client::connect_with_path(bridge_socket_path.clone()),
        )
        .await??;
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), client2.ping()).await??,
            "bridge should accept a second client while the first subscribed session stays open"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), client1.ping()).await??,
            "first bridged client should remain healthy after a second client connects"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    kill_child(&mut bridge_dial);
    kill_child(&mut bridge_serve);
    abort_server_and_cleanup(&server_handle, &server_socket_path, &debug_socket_path);

    if let Err(ref error) = result {
        eprintln!("bridge multi-client e2e error: {error:#}");
        if let Ok(stderr) = std::fs::read_to_string(&serve_stderr_path) {
            eprintln!("bridge serve stderr:\n{stderr}");
        }
        if let Ok(stderr) = std::fs::read_to_string(&dial_stderr_path) {
            eprintln!("bridge dial stderr:\n{stderr}");
        }
    }

    result
}
