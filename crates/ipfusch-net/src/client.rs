use crate::{PacketEvent, StreamRunSpec, monotonic_ns, pacing_delay};
use crate::{
    server::quic_client_config, server::read_control_frame_async, server::write_control_frame_async,
};
use anyhow::{Context, Result, anyhow};
use ipfusch_core::config::Transport;
use ipfusch_core::protocol::{
    ControlMessage, DATA_HEADER_SIZE, DataHeader, PROTOCOL_VERSION, hash_token,
};
use std::net::SocketAddr;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;

const CTRL_PREFIX: &[u8] = b"IPFUSCH_CTRL";

pub async fn run_stream(spec: StreamRunSpec, tx: mpsc::UnboundedSender<PacketEvent>) -> Result<()> {
    match spec.transport {
        Transport::Tcp => run_stream_tcp(spec, tx).await,
        Transport::Udp => run_stream_udp(spec, tx).await,
        Transport::Quic => run_stream_quic(spec, tx).await,
    }
}

async fn run_stream_tcp(spec: StreamRunSpec, tx: mpsc::UnboundedSender<PacketEvent>) -> Result<()> {
    let mut stream = TcpStream::connect(spec.host)
        .await
        .with_context(|| format!("tcp connect failed: {}", spec.host))?;
    stream.set_nodelay(true)?;

    control_handshake_tcp(&mut stream, spec.session_id, spec.stream_id, &spec).await?;

    let payload = vec![0u8; spec.payload_bytes];
    let pacing = pacing_delay(spec.payload_bytes, spec.rate_bps);
    let start = Instant::now();
    let warmup_until = start + spec.warmup;
    let end = warmup_until + spec.duration;

    let mut seq = 0u64;
    let mut next_send = Instant::now();

    while Instant::now() < end {
        if let Some(delay) = pacing {
            let now = Instant::now();
            if now < next_send {
                tokio::time::sleep_until(tokio::time::Instant::from_std(next_send)).await;
            }
            next_send += delay;
        }

        seq += 1;
        let send_mono_ns = monotonic_ns();

        let header = DataHeader::new(
            spec.session_id,
            spec.stream_id,
            seq,
            send_mono_ns,
            spec.payload_bytes as u32,
        );
        let mut frame = Vec::with_capacity(DATA_HEADER_SIZE + payload.len());
        frame.extend_from_slice(&header.encode());
        frame.extend_from_slice(&payload);
        stream
            .write_all(&frame)
            .await
            .context("tcp packet write failed")?;

        let measured = Instant::now() >= warmup_until;
        let mut ack = [0u8; DATA_HEADER_SIZE];
        let recv_mono_ns =
            match tokio::time::timeout(spec.timeout, stream.read_exact(&mut ack)).await {
                Ok(Ok(_)) => {
                    let ack_header = DataHeader::decode(&ack).context("decode tcp ack header")?;
                    if ack_header.seq == seq {
                        Some(monotonic_ns())
                    } else {
                        None
                    }
                }
                _ => None,
            };

        tx.send(PacketEvent {
            stream_id: spec.stream_id,
            seq,
            bytes: spec.payload_bytes as u64,
            send_mono_ns,
            recv_mono_ns,
            measured,
        })
        .ok();
    }

    Ok(())
}

async fn run_stream_udp(spec: StreamRunSpec, tx: mpsc::UnboundedSender<PacketEvent>) -> Result<()> {
    let bind = if spec.host.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind)
        .await
        .context("udp client bind failed")?;
    socket
        .connect(spec.host)
        .await
        .with_context(|| format!("udp connect failed: {}", spec.host))?;

    control_handshake_udp(&socket, &spec).await?;

    let payload = vec![0u8; spec.payload_bytes];
    let pacing = pacing_delay(spec.payload_bytes, spec.rate_bps);
    let start = Instant::now();
    let warmup_until = start + spec.warmup;
    let end = warmup_until + spec.duration;

    let mut seq = 0u64;
    let mut next_send = Instant::now();

    while Instant::now() < end {
        if let Some(delay) = pacing {
            let now = Instant::now();
            if now < next_send {
                tokio::time::sleep_until(tokio::time::Instant::from_std(next_send)).await;
            }
            next_send += delay;
        }

        seq += 1;
        let send_mono_ns = monotonic_ns();

        let header = DataHeader::new(
            spec.session_id,
            spec.stream_id,
            seq,
            send_mono_ns,
            spec.payload_bytes as u32,
        );

        let mut frame = Vec::with_capacity(DATA_HEADER_SIZE + payload.len());
        frame.extend_from_slice(&header.encode());
        frame.extend_from_slice(&payload);
        socket
            .send(&frame)
            .await
            .context("udp packet send failed")?;

        let measured = Instant::now() >= warmup_until;
        let mut ack = [0u8; DATA_HEADER_SIZE];
        let recv_mono_ns = match tokio::time::timeout(spec.timeout, socket.recv(&mut ack)).await {
            Ok(Ok(read_len)) if read_len >= DATA_HEADER_SIZE => {
                let ack_header = DataHeader::decode(&ack).context("decode udp ack header")?;
                if ack_header.seq == seq {
                    Some(monotonic_ns())
                } else {
                    None
                }
            }
            _ => None,
        };

        tx.send(PacketEvent {
            stream_id: spec.stream_id,
            seq,
            bytes: spec.payload_bytes as u64,
            send_mono_ns,
            recv_mono_ns,
            measured,
        })
        .ok();
    }

    Ok(())
}

async fn run_stream_quic(
    spec: StreamRunSpec,
    tx: mpsc::UnboundedSender<PacketEvent>,
) -> Result<()> {
    let bind: SocketAddr = if spec.host.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()
    .expect("valid socket addr literal");

    let mut endpoint = quinn::Endpoint::client(bind).context("build quic client endpoint")?;
    endpoint.set_default_client_config(quic_client_config()?);

    let conn = endpoint
        .connect(spec.host, "ipfusch.local")
        .context("quic connect setup failed")?
        .await
        .context("quic connect failed")?;

    let (mut send, mut recv) = conn.open_bi().await.context("open quic stream failed")?;
    control_handshake_quic(&mut send, &mut recv, &spec).await?;

    let payload = vec![0u8; spec.payload_bytes];
    let pacing = pacing_delay(spec.payload_bytes, spec.rate_bps);
    let start = Instant::now();
    let warmup_until = start + spec.warmup;
    let end = warmup_until + spec.duration;

    let mut seq = 0u64;
    let mut next_send = Instant::now();

    while Instant::now() < end {
        if let Some(delay) = pacing {
            let now = Instant::now();
            if now < next_send {
                tokio::time::sleep_until(tokio::time::Instant::from_std(next_send)).await;
            }
            next_send += delay;
        }

        seq += 1;
        let send_mono_ns = monotonic_ns();

        let header = DataHeader::new(
            spec.session_id,
            spec.stream_id,
            seq,
            send_mono_ns,
            spec.payload_bytes as u32,
        );
        let mut frame = Vec::with_capacity(DATA_HEADER_SIZE + payload.len());
        frame.extend_from_slice(&header.encode());
        frame.extend_from_slice(&payload);

        send.write_all(&frame)
            .await
            .context("quic packet write failed")?;

        let measured = Instant::now() >= warmup_until;
        let mut ack = [0u8; DATA_HEADER_SIZE];
        let recv_mono_ns = match tokio::time::timeout(spec.timeout, recv.read_exact(&mut ack)).await
        {
            Ok(Ok(_)) => {
                let ack_header = DataHeader::decode(&ack).context("decode quic ack header")?;
                if ack_header.seq == seq {
                    Some(monotonic_ns())
                } else {
                    None
                }
            }
            _ => None,
        };

        tx.send(PacketEvent {
            stream_id: spec.stream_id,
            seq,
            bytes: spec.payload_bytes as u64,
            send_mono_ns,
            recv_mono_ns,
            measured,
        })
        .ok();
    }

    send.finish().ok();
    endpoint.wait_idle().await;

    Ok(())
}

async fn control_handshake_tcp(
    stream: &mut TcpStream,
    session_id: u64,
    stream_id: u32,
    spec: &StreamRunSpec,
) -> Result<()> {
    write_control_frame_async(
        stream,
        &ControlMessage::Hello {
            version: PROTOCOL_VERSION,
            client_name: format!("ipfusch-stream-{stream_id}"),
            token_hash: spec.token.as_deref().map(hash_token),
            capabilities: vec![spec.transport.to_string(), "multistream".to_string()],
        },
    )
    .await?;

    write_control_frame_async(
        stream,
        &ControlMessage::StartSession {
            session_id,
            transport: spec.transport.to_string(),
            streams: 1,
            duration_ms: spec.duration.as_millis() as u64,
            payload_bytes: spec.payload_bytes,
            interval_ms: 100,
            rate_bps: spec.rate_bps,
        },
    )
    .await?;

    validate_ack(read_control_frame_async(stream).await?)
}

async fn control_handshake_udp(socket: &UdpSocket, spec: &StreamRunSpec) -> Result<()> {
    let hello = ControlMessage::Hello {
        version: PROTOCOL_VERSION,
        client_name: format!("ipfusch-stream-{}", spec.stream_id),
        token_hash: spec.token.as_deref().map(hash_token),
        capabilities: vec![spec.transport.to_string(), "multistream".to_string()],
    };

    let payload = bincode::serialize(&hello)?;
    let mut frame = Vec::with_capacity(CTRL_PREFIX.len() + payload.len());
    frame.extend_from_slice(CTRL_PREFIX);
    frame.extend_from_slice(&payload);

    socket.send(&frame).await.context("udp hello send failed")?;

    let mut buf = vec![0u8; 4 * 1024];
    let len = tokio::time::timeout(spec.timeout, socket.recv(&mut buf))
        .await
        .map_err(|_| anyhow!("udp hello ack timeout"))?
        .context("udp hello ack recv failed")?;

    let ack = decode_udp_control(&buf[..len]).ok_or_else(|| anyhow!("invalid udp ack frame"))?;
    validate_ack(ack)
}

async fn control_handshake_quic(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    spec: &StreamRunSpec,
) -> Result<()> {
    write_control_frame_async(
        send,
        &ControlMessage::Hello {
            version: PROTOCOL_VERSION,
            client_name: format!("ipfusch-stream-{}", spec.stream_id),
            token_hash: spec.token.as_deref().map(hash_token),
            capabilities: vec![spec.transport.to_string(), "multistream".to_string()],
        },
    )
    .await?;

    write_control_frame_async(
        send,
        &ControlMessage::StartSession {
            session_id: spec.session_id,
            transport: spec.transport.to_string(),
            streams: 1,
            duration_ms: spec.duration.as_millis() as u64,
            payload_bytes: spec.payload_bytes,
            interval_ms: 100,
            rate_bps: spec.rate_bps,
        },
    )
    .await?;

    validate_ack(read_control_frame_async(recv).await?)
}

fn validate_ack(msg: ControlMessage) -> Result<()> {
    match msg {
        ControlMessage::Ack { accepted: true, .. } => Ok(()),
        ControlMessage::Ack {
            accepted: false,
            reason,
            ..
        } => Err(anyhow!(
            reason.unwrap_or_else(|| "session rejected".to_string())
        )),
        _ => Err(anyhow!("unexpected control response")),
    }
}

fn decode_udp_control(frame: &[u8]) -> Option<ControlMessage> {
    if !frame.starts_with(CTRL_PREFIX) {
        return None;
    }
    bincode::deserialize(&frame[CTRL_PREFIX.len()..]).ok()
}
