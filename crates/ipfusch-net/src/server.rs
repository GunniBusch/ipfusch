use crate::token_matches;
use anyhow::{Context, Result, anyhow};
use ipfusch_core::config::{ServerConfig, ServerTransport};
use ipfusch_core::protocol::{
    ControlMessage, DATA_HEADER_SIZE, DataHeader, PROTOCOL_VERSION, hash_token,
};
use quinn::{Endpoint, ServerConfig as QuinnServerConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::collections::HashSet;
use std::io::BufReader;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

const CTRL_PREFIX: &[u8] = b"IPFUSCH_CTRL";
const DEV_CERT_PEM: &str = include_str!("../certs/dev-cert.pem");
const DEV_KEY_PEM: &str = include_str!("../certs/dev-key.pem");

pub async fn run_server(config: ServerConfig) -> Result<()> {
    match config.transport {
        ServerTransport::Tcp => run_tcp_server(config.bind, config.token, config.max_clients).await,
        ServerTransport::Udp => run_udp_server(config.bind, config.token).await,
        ServerTransport::Quic => run_quic_server(config.bind, config.token).await,
        ServerTransport::All => {
            let tcp_bind = config.bind;
            let udp_bind = SocketAddr::new(config.bind.ip(), config.bind.port().saturating_add(1));
            let quic_bind = SocketAddr::new(config.bind.ip(), config.bind.port().saturating_add(2));

            let tcp = run_tcp_server(tcp_bind, config.token.clone(), config.max_clients);
            let udp = run_udp_server(udp_bind, config.token.clone());
            let quic = run_quic_server(quic_bind, config.token);

            tokio::try_join!(tcp, udp, quic)?;
            Ok(())
        }
    }
}

async fn run_tcp_server(bind: SocketAddr, token: Option<String>, max_clients: usize) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("tcp bind failed on {bind}"))?;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_clients));

    loop {
        let (socket, _) = listener.accept().await.context("tcp accept failed")?;
        let token = token.clone();
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .context("failed to acquire client permit")?;
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(err) = handle_tcp_connection(socket, token).await {
                eprintln!("tcp client connection ended with error: {err:#}");
            }
        });
    }
}

async fn handle_tcp_connection(mut socket: TcpStream, token: Option<String>) -> Result<()> {
    socket.set_nodelay(true)?;
    let hello = read_control_frame_async(&mut socket)
        .await
        .context("read hello")?;
    let start = read_control_frame_async(&mut socket)
        .await
        .context("read start session")?;

    let (session_id, accepted, reason) = match (&hello, &start) {
        (
            ControlMessage::Hello {
                version,
                token_hash,
                ..
            },
            ControlMessage::StartSession { session_id, .. },
        ) => {
            if *version != PROTOCOL_VERSION {
                (
                    *session_id,
                    false,
                    Some(format!("protocol version mismatch: {version}")),
                )
            } else if !token_matches(token.as_deref(), *token_hash) {
                (*session_id, false, Some("token rejected".to_string()))
            } else {
                (*session_id, true, None)
            }
        }
        _ => (0, false, Some("invalid control flow".to_string())),
    };

    write_control_frame_async(
        &mut socket,
        &ControlMessage::Ack {
            session_id,
            accepted,
            reason: reason.clone(),
        },
    )
    .await
    .context("send ack")?;

    if !accepted {
        return Err(anyhow!(
            reason.unwrap_or_else(|| "session rejected".to_string())
        ));
    }

    let mut header_buf = [0u8; DATA_HEADER_SIZE];
    let mut payload_buf = Vec::<u8>::new();
    loop {
        if !read_exact_or_eof(&mut socket, &mut header_buf).await? {
            break;
        }
        let header = DataHeader::decode(&header_buf)?;

        if header.payload_len > 0 {
            let required = header.payload_len as usize;
            if payload_buf.len() < required {
                payload_buf.resize(required, 0);
            }
            socket
                .read_exact(&mut payload_buf[..required])
                .await
                .context("read tcp payload")?;
        }

        socket
            .write_all(&header_buf)
            .await
            .context("write tcp echo header")?;
    }

    Ok(())
}

async fn run_udp_server(bind: SocketAddr, token: Option<String>) -> Result<()> {
    let socket = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("udp bind failed on {bind}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut authorized = HashSet::new();

    loop {
        let (len, peer) = socket
            .recv_from(&mut buf)
            .await
            .context("udp recv failed")?;
        let frame = &buf[..len];

        if let Some(msg) = decode_control_datagram(frame) {
            if let ControlMessage::Hello {
                version,
                token_hash,
                ..
            } = msg
            {
                let accepted =
                    version == PROTOCOL_VERSION && token_matches(token.as_deref(), token_hash);
                if accepted {
                    authorized.insert(peer);
                }
                let ack = ControlMessage::Ack {
                    session_id: 0,
                    accepted,
                    reason: if accepted {
                        None
                    } else {
                        Some("token or protocol rejected".to_string())
                    },
                };
                let encoded = encode_control_datagram(&ack)?;
                socket
                    .send_to(&encoded, peer)
                    .await
                    .context("udp ack send failed")?;
            }
            continue;
        }

        if token.is_some() && !authorized.contains(&peer) {
            continue;
        }

        if frame.len() < DATA_HEADER_SIZE {
            continue;
        }

        if DataHeader::decode(&frame[..DATA_HEADER_SIZE]).is_err() {
            continue;
        }

        socket
            .send_to(&frame[..DATA_HEADER_SIZE], peer)
            .await
            .context("udp echo failed")?;
    }
}

async fn run_quic_server(bind: SocketAddr, token: Option<String>) -> Result<()> {
    let server_config = build_quic_server_config()?;
    let endpoint = Endpoint::server(server_config, bind)
        .with_context(|| format!("quic bind failed on {bind}"))?;

    while let Some(connecting) = endpoint.accept().await {
        let token = token.clone();
        tokio::spawn(async move {
            let Ok(connection) = connecting.await else {
                return;
            };

            loop {
                let stream = connection.accept_bi().await;
                let (mut send, mut recv) = match stream {
                    Ok(parts) => parts,
                    Err(_) => break,
                };

                if let Err(err) = handle_quic_stream(&mut send, &mut recv, token.clone()).await {
                    eprintln!("quic stream failed: {err:#}");
                    break;
                }
            }
        });
    }

    Ok(())
}

async fn handle_quic_stream(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    token: Option<String>,
) -> Result<()> {
    let hello = read_control_frame_async(recv)
        .await
        .context("read quic hello")?;
    let start = read_control_frame_async(recv)
        .await
        .context("read quic start")?;

    let (session_id, accepted, reason) = match (&hello, &start) {
        (
            ControlMessage::Hello {
                version,
                token_hash,
                ..
            },
            ControlMessage::StartSession { session_id, .. },
        ) => {
            if *version != PROTOCOL_VERSION {
                (
                    *session_id,
                    false,
                    Some(format!("protocol version mismatch: {version}")),
                )
            } else if !token_matches(token.as_deref(), *token_hash) {
                (*session_id, false, Some("token rejected".to_string()))
            } else {
                (*session_id, true, None)
            }
        }
        _ => (0, false, Some("invalid control flow".to_string())),
    };

    write_control_frame_async(
        send,
        &ControlMessage::Ack {
            session_id,
            accepted,
            reason: reason.clone(),
        },
    )
    .await
    .context("send quic ack")?;

    if !accepted {
        return Err(anyhow!(
            reason.unwrap_or_else(|| "session rejected".to_string())
        ));
    }

    let mut header_buf = [0u8; DATA_HEADER_SIZE];
    let mut payload_buf = Vec::<u8>::new();
    loop {
        if !read_exact_or_eof(recv, &mut header_buf).await? {
            break;
        }

        let header = DataHeader::decode(&header_buf)?;
        if header.payload_len > 0 {
            let required = header.payload_len as usize;
            if payload_buf.len() < required {
                payload_buf.resize(required, 0);
            }
            recv.read_exact(&mut payload_buf[..required])
                .await
                .context("read quic payload")?;
        }

        send.write_all(&header_buf)
            .await
            .context("write quic header echo")?;
    }

    send.finish().context("finish quic stream")?;
    Ok(())
}

async fn read_exact_or_eof<R>(reader: &mut R, buf: &mut [u8]) -> std::io::Result<bool>
where
    R: AsyncRead + Unpin,
{
    match reader.read_exact(buf).await {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

pub(crate) async fn write_control_frame_async<W>(writer: &mut W, msg: &ControlMessage) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = bincode::serialize(msg)?;
    let len = u32::try_from(payload.len()).context("control frame too large")?;
    writer
        .write_all(&len.to_le_bytes())
        .await
        .context("write control len")?;
    writer
        .write_all(&payload)
        .await
        .context("write control payload")?;
    Ok(())
}

pub(crate) async fn read_control_frame_async<R>(reader: &mut R) -> Result<ControlMessage>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .context("read control len")?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("read control payload")?;
    let msg = bincode::deserialize(&payload).context("deserialize control payload")?;
    Ok(msg)
}

fn encode_control_datagram(msg: &ControlMessage) -> Result<Vec<u8>> {
    let payload = bincode::serialize(msg)?;
    let mut out = Vec::with_capacity(CTRL_PREFIX.len() + payload.len());
    out.extend_from_slice(CTRL_PREFIX);
    out.extend_from_slice(&payload);
    Ok(out)
}

fn decode_control_datagram(frame: &[u8]) -> Option<ControlMessage> {
    if !frame.starts_with(CTRL_PREFIX) {
        return None;
    }
    bincode::deserialize(&frame[CTRL_PREFIX.len()..]).ok()
}

fn build_quic_server_config() -> Result<QuinnServerConfig> {
    ensure_rustls_crypto_provider();
    let certs = load_certs(DEV_CERT_PEM)?;
    let key = load_private_key(DEV_KEY_PEM)?;
    let mut cfg =
        QuinnServerConfig::with_single_cert(certs, key).context("build quic server config")?;
    cfg.transport_config(Arc::new(quinn::TransportConfig::default()));
    Ok(cfg)
}

pub(crate) fn quic_client_config() -> Result<quinn::ClientConfig> {
    ensure_rustls_crypto_provider();
    let certs = load_certs(DEV_CERT_PEM)?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots
            .add(cert)
            .map_err(|e| anyhow!("add root cert failed: {e}"))?;
    }

    let client_crypto = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)
        .context("build quic client crypto")?;

    Ok(quinn::ClientConfig::new(Arc::new(quic_crypto)))
}

fn ensure_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn load_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(pem.as_bytes());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse certificate pem")?;
    if certs.is_empty() {
        return Err(anyhow!("no certificates in pem"));
    }
    Ok(certs)
}

fn load_private_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(pem.as_bytes());
    let mut keys = rustls_pemfile::pkcs8_private_keys(&mut reader);
    let key: PrivatePkcs8KeyDer<'static> = keys
        .next()
        .ok_or_else(|| anyhow!("no private key in pem"))
        .and_then(|k| k.map_err(|e| anyhow!(e)))?;

    Ok(PrivateKeyDer::Pkcs8(key))
}

#[allow(dead_code)]
fn _token_hash_for_docs(token: &str) -> [u8; 32] {
    hash_token(token)
}
