use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::args::BridgeCommand;

const AUTH_OK: &str = "OK";
const AUTH_ERR: &str = "ERR";
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_AUTH_LINE_LEN: usize = 4096;

pub(crate) async fn run_bridge_command(command: BridgeCommand) -> Result<()> {
    match command {
        BridgeCommand::Serve {
            listen,
            local_socket,
            token_file,
        } => {
            let token = load_token(Path::new(&token_file))?;
            run_bridge_server(&listen, Path::new(&local_socket), &token).await
        }
        BridgeCommand::Dial {
            remote,
            bind,
            token_file,
        } => {
            let token = load_token(Path::new(&token_file))?;
            run_bridge_dial(&remote, Path::new(&bind), &token).await
        }
    }
}

fn load_token(path: &Path) -> Result<String> {
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read bridge token file {}", path.display()))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("Bridge token file {} is empty", path.display());
    }
    Ok(token)
}

async fn run_bridge_server(listen: &str, local_socket: &Path, token: &str) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("Failed to bind bridge listener on {listen}"))?;
    let local_socket = local_socket.to_path_buf();
    let token = token.to_string();

    loop {
        let (tcp_stream, _) = listener.accept().await?;
        let local_socket = local_socket.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_server_connection(tcp_stream, &local_socket, &token).await {
                eprintln!("bridge serve connection failed: {err:#}");
            }
        });
    }
}

async fn run_bridge_dial(remote: &str, bind: &Path, token: &str) -> Result<()> {
    let (listener, _guard) = bind_dial_listener(bind).await?;
    let remote = remote.to_string();
    let token = token.to_string();

    loop {
        let (mut local_stream, _) = listener.accept().await?;
        let remote = remote.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_dial_connection(&mut local_stream, &remote, &token).await {
                eprintln!("bridge dial connection failed: {err:#}");
            }
        });
    }
}

async fn bind_dial_listener(path: &Path) -> Result<(crate::transport::Listener, BoundSocketGuard)> {
    if crate::transport::is_socket_path(path) {
        if crate::transport::Stream::connect(path).await.is_ok() {
            bail!(
                "Refusing to replace active server socket at {}",
                path.display()
            );
        }
        bail!(
            "Socket path already exists at {}. Remove it after confirming no jcode bridge is running.",
            path.display()
        );
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = crate::transport::Listener::bind(path)
        .with_context(|| format!("Failed to bind local bridge socket at {}", path.display()))?;
    let _ = crate::platform::set_permissions_owner_only(path);

    Ok((listener, BoundSocketGuard::new(path.to_path_buf())))
}

async fn handle_server_connection(
    mut tcp_stream: TcpStream,
    local_socket: &Path,
    token: &str,
) -> Result<()> {
    authenticate_incoming_connection(&mut tcp_stream, token).await?;
    let mut local_stream = connect_local_socket(local_socket).await?;
    relay_streams(&mut tcp_stream, &mut local_stream).await
}

async fn handle_dial_connection(
    local_stream: &mut crate::transport::Stream,
    remote: &str,
    token: &str,
) -> Result<()> {
    let mut remote_stream = TcpStream::connect(remote)
        .await
        .with_context(|| format!("Failed to connect to remote bridge at {remote}"))?;
    authenticate_outgoing_connection(&mut remote_stream, token).await?;
    relay_streams(local_stream, &mut remote_stream).await
}

async fn connect_local_socket(path: &Path) -> Result<crate::transport::Stream> {
    match crate::transport::Stream::connect(path).await {
        Ok(stream) => Ok(stream),
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused && path.exists() => {
            bail!(
                "Socket exists but refused the connection at {}. Retry, or remove it after confirming no jcode server is running.",
                path.display()
            )
        }
        Err(err) if err.raw_os_error() == Some(libc::EMFILE) => Err(anyhow::anyhow!(
            "{} ({})",
            err,
            crate::util::process_fd_diagnostic_snapshot()
        )),
        Err(err) => Err(err.into()),
    }
}

async fn authenticate_incoming_connection(stream: &mut TcpStream, token: &str) -> Result<()> {
    let presented = tokio::time::timeout(AUTH_TIMEOUT, read_auth_line(stream))
        .await
        .context("Timed out waiting for bridge authentication token")??;
    if presented != token {
        stream.write_all(format!("{AUTH_ERR}\n").as_bytes()).await?;
        bail!("Bridge authentication failed");
    }
    stream.write_all(format!("{AUTH_OK}\n").as_bytes()).await?;
    Ok(())
}

async fn authenticate_outgoing_connection(stream: &mut TcpStream, token: &str) -> Result<()> {
    tokio::time::timeout(AUTH_TIMEOUT, async {
        stream.write_all(token.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        Result::<()>::Ok(())
    })
    .await
    .context("Timed out sending bridge authentication token")??;

    let response = tokio::time::timeout(AUTH_TIMEOUT, read_auth_line(stream))
        .await
        .context("Timed out waiting for bridge authentication response")??;
    if response != AUTH_OK {
        bail!("Bridge authentication failed");
    }
    Ok(())
}

async fn read_auth_line(stream: &mut TcpStream) -> Result<String> {
    let mut line = Vec::new();

    loop {
        let mut byte = [0u8; 1];
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            bail!("Bridge peer disconnected during authentication");
        }

        if byte[0] == b'\n' {
            break;
        }
        if byte[0] != b'\r' {
            line.push(byte[0]);
            if line.len() > MAX_AUTH_LINE_LEN {
                bail!("Bridge authentication token exceeded {MAX_AUTH_LINE_LEN} bytes");
            }
        }
    }

    String::from_utf8(line).context("Bridge authentication token was not valid UTF-8")
}

async fn relay_streams<A, B>(left: &mut A, right: &mut B) -> Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    tokio::io::copy_bidirectional(left, right).await?;
    Ok(())
}

#[derive(Debug)]
struct BoundSocketGuard {
    path: PathBuf,
}

impl BoundSocketGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for BoundSocketGuard {
    fn drop(&mut self) {
        crate::transport::remove_socket(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn load_token_trims_whitespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token_path = dir.path().join("bridge-token");
        std::fs::write(&token_path, "secret\n").expect("write token");

        assert_eq!(load_token(&token_path).expect("load token"), "secret");
    }

    #[test]
    fn load_token_rejects_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token_path = dir.path().join("bridge-token");
        std::fs::write(&token_path, "  \n").expect("write token");

        let err = load_token(&token_path).expect_err("empty token should fail");
        assert!(err.to_string().contains("is empty"));
    }

    #[tokio::test]
    async fn bind_dial_listener_refuses_live_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("bridge.sock");
        let _listener = crate::transport::Listener::bind(&socket_path).expect("bind listener");

        let err = bind_dial_listener(&socket_path)
            .await
            .expect_err("should refuse live socket");
        assert!(
            err.to_string()
                .contains("Refusing to replace active server socket"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn bind_dial_listener_refuses_stale_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("bridge.sock");
        std::fs::write(&socket_path, b"").expect("create placeholder");

        let err = bind_dial_listener(&socket_path)
            .await
            .expect_err("should refuse existing path");
        assert!(
            err.to_string()
                .contains("Remove it after confirming no jcode bridge is running"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn authenticate_connection_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            authenticate_incoming_connection(&mut stream, "secret")
                .await
                .expect("server auth");
        });

        let mut client = TcpStream::connect(addr).await.expect("connect");
        authenticate_outgoing_connection(&mut client, "secret")
            .await
            .expect("client auth");
        server.await.expect("server join");
    }

    #[tokio::test]
    async fn authenticate_connection_rejects_wrong_token() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            authenticate_incoming_connection(&mut stream, "secret")
                .await
                .expect_err("server should reject token")
                .to_string()
        });

        let mut client = TcpStream::connect(addr).await.expect("connect");
        client.write_all(b"wrong\n").await.expect("write token");
        assert_eq!(
            read_auth_line(&mut client).await.expect("read auth error"),
            AUTH_ERR
        );

        let server_error = server.await.expect("server join");
        assert!(server_error.contains("Bridge authentication failed"));
    }

    #[tokio::test]
    async fn read_auth_line_rejects_oversized_token() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            read_auth_line(&mut stream)
                .await
                .expect_err("oversized token should fail")
                .to_string()
        });

        let mut client = TcpStream::connect(addr).await.expect("connect");
        client
            .write_all(vec![b'a'; MAX_AUTH_LINE_LEN + 1].as_slice())
            .await
            .expect("write oversized token");
        client.write_all(b"\n").await.expect("write newline");

        let error = server.await.expect("server join");
        assert!(error.contains("exceeded"));
    }

    #[tokio::test]
    async fn read_auth_line_rejects_eof_before_newline() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            read_auth_line(&mut stream)
                .await
                .expect_err("missing newline should fail")
                .to_string()
        });

        let mut client = TcpStream::connect(addr).await.expect("connect");
        client
            .write_all(b"secret")
            .await
            .expect("write partial token");
        client.shutdown().await.expect("shutdown client");

        let error = server.await.expect("server join");
        assert!(error.contains("disconnected during authentication"));
    }

    #[tokio::test]
    async fn server_connection_relays_payload_to_local_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("local.sock");
        let local_listener = crate::transport::Listener::bind(&socket_path).expect("bind local");

        let echo_task = tokio::spawn(async move {
            let (mut stream, _) = local_listener.accept().await.expect("accept local");
            let mut payload = [0u8; 4];
            stream.read_exact(&mut payload).await.expect("read payload");
            assert_eq!(&payload, b"ping");
            stream.write_all(&payload).await.expect("write payload");
        });

        let bridge_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind bridge");
        let addr = bridge_listener.local_addr().expect("bridge addr");
        let socket_for_bridge = socket_path.clone();
        let bridge_task = tokio::spawn(async move {
            let (stream, _) = bridge_listener.accept().await.expect("accept bridge");
            handle_server_connection(stream, &socket_for_bridge, "secret")
                .await
                .expect("bridge relay");
        });

        let mut client = TcpStream::connect(addr).await.expect("connect bridge");
        authenticate_outgoing_connection(&mut client, "secret")
            .await
            .expect("authenticate client");
        client.write_all(b"ping").await.expect("write payload");

        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.expect("read echo");
        assert_eq!(&echoed, b"ping");
        client.shutdown().await.expect("shutdown client");

        bridge_task.await.expect("bridge join");
        echo_task.await.expect("echo join");
    }

    #[tokio::test]
    async fn dial_connection_relays_payload_to_remote_bridge() {
        let bridge_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind bridge");
        let addr = bridge_listener.local_addr().expect("bridge addr");
        let bridge_task = tokio::spawn(async move {
            let (mut stream, _) = bridge_listener.accept().await.expect("accept bridge");
            authenticate_incoming_connection(&mut stream, "secret")
                .await
                .expect("bridge auth");

            let mut payload = [0u8; 4];
            stream.read_exact(&mut payload).await.expect("read payload");
            assert_eq!(&payload, b"ping");
            stream.write_all(&payload).await.expect("write payload");
        });

        let (mut bridge_side, mut local_side) =
            crate::transport::stream_pair().expect("stream pair");
        let dial_task = tokio::spawn(async move {
            handle_dial_connection(&mut bridge_side, &addr.to_string(), "secret")
                .await
                .expect("dial relay");
        });

        local_side.write_all(b"ping").await.expect("write payload");
        let mut echoed = [0u8; 4];
        local_side.read_exact(&mut echoed).await.expect("read echo");
        assert_eq!(&echoed, b"ping");
        local_side.shutdown().await.expect("shutdown local side");

        dial_task.await.expect("dial join");
        bridge_task.await.expect("bridge join");
    }
}
