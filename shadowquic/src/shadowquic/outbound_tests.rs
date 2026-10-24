use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream, UdpSocket},
};

use crate::{
    Outbound, ProxyRequest, TcpSession, UserContext,
    config::{CongestionControl, ShadowQuicClientCfg},
    dns::ResolverManager,
    error::SError,
    shadowquic::outbound::ShadowQuicClient,
    squic::CONN_ACQUIRE_TIMEOUT,
};

/// A UDP socket that nothing ever reads: packets are delivered, but no reply and no
/// ICMP error comes back, so the QUIC handshake keeps retransmitting exactly like it
/// does against a server that is powered off.
async fn blackholed_server() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").await.unwrap()
}

fn client(server: SocketAddr) -> Arc<ShadowQuicClient> {
    Arc::new(ShadowQuicClient::new(
        ShadowQuicClientCfg {
            addr: server.to_string(),
            username: "user".into(),
            password: "password".into(),
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            zero_rtt: true,
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            ..Default::default()
        },
        Arc::new(ResolverManager::new()),
    ))
}

/// Returns the peer side of a session plus the request itself, so a test can tell
/// whether the session's stream was closed while its request was waiting.
async fn tcp_request() -> (TcpStream, ProxyRequest) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    (
        peer,
        ProxyRequest::Tcp(TcpSession {
            stream: Box::new(stream),
            dst: "127.0.0.1:80".parse::<SocketAddr>().unwrap().into(),
            src_addr: None,
            user_context: UserContext::default(),
        }),
    )
}

/// True when the peer of the session sees the stream closed instead of hanging.
async fn peer_sees_close(peer: &mut TcpStream) -> bool {
    let mut buf = [0u8; 1];
    match tokio::time::timeout(Duration::from_secs(1), peer.read(&mut buf)).await {
        Ok(Ok(read)) => read == 0,
        Ok(Err(..)) => true,
        Err(..) => false,
    }
}

#[tokio::test(start_paused = true)]
async fn unreachable_server_fails_the_request_within_the_bound() {
    let server = blackholed_server().await;
    let client = client(server.local_addr().unwrap());
    let (mut peer, req) = tcp_request().await;

    let started = tokio::time::Instant::now();
    let error = client.handle(req).await.unwrap_err();
    let elapsed = started.elapsed();

    assert!(
        matches!(error, SError::OutboundUnavailable),
        "expected the acquisition to time out, got: {error}"
    );
    assert!(
        elapsed >= CONN_ACQUIRE_TIMEOUT && elapsed <= CONN_ACQUIRE_TIMEOUT + Duration::from_secs(1),
        "acquisition returned after {elapsed:?}, expected about {CONN_ACQUIRE_TIMEOUT:?}"
    );
    assert!(
        peer_sees_close(&mut peer).await,
        "the inbound stream stayed open after the request failed"
    );
}

#[tokio::test(start_paused = true)]
async fn a_queued_request_does_not_wait_behind_a_stalled_handshake() {
    let server = blackholed_server().await;
    let client = client(server.local_addr().unwrap());
    let (_first_peer, first) = tcp_request().await;
    let (_second_peer, second) = tcp_request().await;

    let started = tokio::time::Instant::now();
    let first = tokio::spawn({
        let client = client.clone();
        async move { client.handle(first).await }
    });
    let second = tokio::spawn({
        let client = client.clone();
        async move { client.handle(second).await }
    });
    let (first, second) = tokio::join!(first, second);
    let elapsed = started.elapsed();

    assert!(matches!(first.unwrap(), Err(SError::OutboundUnavailable)));
    assert!(matches!(second.unwrap(), Err(SError::OutboundUnavailable)));
    assert!(
        elapsed <= CONN_ACQUIRE_TIMEOUT + Duration::from_secs(1),
        "the queued request waited {elapsed:?} behind a stalled handshake"
    );
}
