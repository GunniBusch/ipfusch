use ipfusch_core::config::{RateMode, RunConfig, ServerConfig, ServerTransport, Transport};
use ipfusch_engine::run_benchmark;
use ipfusch_net::run_server;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::time::Duration;

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("bind free port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn test_run_config(host: SocketAddr, transport: Transport, token: Option<String>) -> RunConfig {
    RunConfig {
        host,
        transport,
        streams: 1,
        duration: Duration::from_millis(500),
        warmup: Duration::from_millis(100),
        payload_bytes: 256,
        rate: RateMode::Bps(10_000_000),
        token,
        interval: Duration::from_millis(100),
    }
}

async fn start_server(
    bind: SocketAddr,
    transport: ServerTransport,
    token: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = run_server(ServerConfig {
            bind,
            transport,
            token,
            max_clients: 64,
            idle_timeout: Duration::from_secs(5),
        })
        .await;
    })
}

fn assert_report_consistent(report: &ipfusch_core::report::RunReport) {
    assert!(report.aggregate.packets_sent > 0, "must send packets");
    assert!(
        report.aggregate.packets_sent >= report.aggregate.packets_acked,
        "acked packets cannot exceed sent packets"
    );
    assert_eq!(report.config.streams as usize, report.streams.len());
    assert!(!report.intervals.is_empty(), "expected interval snapshots");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_and_udp_runs_have_consistent_metrics() {
    for transport in [Transport::Tcp, Transport::Udp] {
        let port = free_port();
        let host = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

        let server = start_server(
            host,
            match transport {
                Transport::Tcp => ServerTransport::Tcp,
                Transport::Udp => ServerTransport::Udp,
                Transport::Quic => unreachable!(),
            },
            None,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(200)).await;
        let report = run_benchmark(test_run_config(host, transport, None), None)
            .await
            .expect("benchmark run should succeed");
        assert_report_consistent(&report);

        server.abort();
        let _ = server.await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quic_supports_optional_token_auth() {
    let port = free_port();
    let host = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    let server = start_server(
        host,
        ServerTransport::Quic,
        Some("secret-token".to_string()),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let ok_report = run_benchmark(
        test_run_config(host, Transport::Quic, Some("secret-token".to_string())),
        None,
    )
    .await
    .expect("quic run with valid token should pass");
    assert_report_consistent(&ok_report);

    let bad = run_benchmark(
        test_run_config(host, Transport::Quic, Some("wrong-token".to_string())),
        None,
    )
    .await;
    assert!(bad.is_err(), "quic run with invalid token should fail");

    server.abort();
    let _ = server.await;
}
