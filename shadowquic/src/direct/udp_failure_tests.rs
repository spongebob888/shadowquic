//! Exercise SOCKS -> ShadowQUIC client -> ShadowQUIC server -> Direct with a
//! deterministic DNS failure, without depending on an external DNS server.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::Notify,
    time::{sleep, timeout},
};

use super::DirectOut;
use crate::{
    Manager,
    config::{AuthUser, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg, SocksServerCfg},
    dns::{DnsService, ResolverManager},
    error::SError,
    msgs::{
        SDecode, SEncode,
        socks5::{CmdReply, CmdReq, SocksAddr, UdpReqHeader},
    },
    quic::QuicConnection,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::inbound::SocksServer,
};

pub(super) const FAILED_DOMAIN: &str = "udp-resolution-failure.invalid";
const DEADLINE: Duration = Duration::from_secs(10);

pub(super) struct FailingDns(pub Notify);

#[async_trait::async_trait]
impl DnsService for FailingDns {
    async fn resolve(&self, domain: &str) -> Result<Vec<IpAddr>, SError> {
        if domain == "udp-echo.test" {
            return Ok(vec![std::net::Ipv4Addr::LOCALHOST.into()]);
        }
        assert_eq!(domain, FAILED_DOMAIN);
        self.0.notify_one();
        Err(SError::DomainResolveFailed(domain.into()))
    }

    #[cfg(feature = "dns-server")]
    async fn exchange(&self, _query: &[u8]) -> Result<Vec<u8>, SError> {
        unreachable!("the Direct outbound should call resolve")
    }
}

#[tokio::test]
async fn udp_over_stream_initial_dns_failure_closes_association() {
    check_dns_failure(true, false).await;
}

#[tokio::test]
async fn udp_over_datagram_initial_dns_failure_closes_association() {
    check_dns_failure(false, false).await;
}

#[tokio::test]
async fn udp_over_stream_dns_failure_after_reply_closes_association() {
    check_dns_failure(true, true).await;
}

#[tokio::test]
async fn udp_over_datagram_dns_failure_after_reply_closes_association() {
    check_dns_failure(false, true).await;
}

async fn check_dns_failure(over_stream: bool, established: bool) {
    timeout(DEADLINE, async {
        let dns = Arc::new(FailingDns(Notify::new()));
        let (server_addr, server_task) = start_server(dns.clone()).await;

        let socks_addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let socks = SocksServer::new(SocksServerCfg {
            tag: "inbound".into(),
            default_outbound: None,
            bind_addr: socks_addr,
            users: vec![],
        })
        .await
        .unwrap();
        let client = Arc::new(ShadowQuicClient::new(
            ShadowQuicClientCfg {
                addr: server_addr.to_string(),
                server_name: "localhost".into(),
                username: "udp-test".into(),
                password: "udp-test-password".into(),
                over_stream,
                // A UDP failure must terminate independently of TCP's timeout.
                half_close_timeout: 0,
                ..Default::default()
            },
            Arc::new(ResolverManager::new()),
        ));
        let client_task = tokio::spawn(Manager::single(Box::new(socks), client.clone()).run());

        // Keep the target in the test so every successful round trip is checked.
        let target = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut association = Association::connect(socks_addr).await;
        if established {
            association.echo(&target).await;
        }
        association
            .send(SocksAddr::from_domain(FAILED_DOMAIN.into(), 9), b"fail")
            .await;
        dns.0.notified().await;

        let conn = client.quic_conn.lock().await.clone().unwrap();
        let connection_id = conn.peer_id();

        // EOF is the SOCKS client's observable notification that the UDP
        // association ended. Do not half-close the client to trigger cleanup.
        let mut byte = [0];
        assert_eq!(association.control.read(&mut byte).await.unwrap(), 0);

        // Both UDP directions must release their destination IDs. A surviving
        // send task or receive task would retain its association's IDs here.
        loop {
            if conn.send_id_store.inner.read().await.is_empty()
                && conn.recv_id_store.inner.read().await.is_empty()
            {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!(conn.close_reason().is_none());

        // The failed association must not kill the shared QUIC connection.
        let mut next = Association::connect(socks_addr).await;
        next.echo(&target).await;
        assert_eq!(
            client.quic_conn.lock().await.as_ref().unwrap().peer_id(),
            connection_id
        );
        assert!(conn.close_reason().is_none());

        drop(next);
        client_task.abort();
        server_task.abort();
    })
    .await
    .expect("DNS failure must fully close the UDP association and allow a new one");
}

pub(super) async fn start_server(
    dns: Arc<FailingDns>,
) -> (SocketAddr, tokio::task::JoinHandle<Result<(), SError>>) {
    let server = ShadowQuicServer::new(ShadowQuicServerCfg {
        tag: "inbound".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        users: vec![AuthUser {
            username: "udp-test".into(),
            password: "udp-test-password".into(),
        }],
        jls_upstream: JlsUpstream {
            addr: "127.0.0.1:9".into(),
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    let addr = server.endpoint.local_addr().unwrap();
    let task = tokio::spawn(
        Manager::single(
            Box::new(server),
            Arc::new(DirectOut {
                cfg: Default::default(),
                dns,
            }),
        )
        .run(),
    );
    (addr, task)
}

struct Association {
    control: TcpStream,
    socket: UdpSocket,
}

impl Association {
    async fn connect(socks_addr: SocketAddr) -> Self {
        let mut control = loop {
            match TcpStream::connect(socks_addr).await {
                Ok(stream) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("SOCKS connect failed: {error}"),
            }
        };
        control.write_all(&[5, 1, 0]).await.unwrap();
        let mut auth = [0; 2];
        control.read_exact(&mut auth).await.unwrap();
        assert_eq!(auth, [5, 0]);
        CmdReq {
            version: 5,
            cmd: 3,
            rsv: 0,
            dst: "0.0.0.0:0".parse().unwrap(),
        }
        .encode(&mut control)
        .await
        .unwrap();
        let reply = CmdReply::decode(&mut control).await.unwrap();
        assert_eq!(reply.rep, 0);
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(reply.bind_addr.to_string()).await.unwrap();
        Self { control, socket }
    }

    async fn send(&self, dst: SocksAddr, payload: &[u8]) {
        let mut packet = Vec::new();
        UdpReqHeader {
            rsv: 0,
            frag: 0,
            dst,
        }
        .encode(&mut packet)
        .await
        .unwrap();
        packet.extend_from_slice(payload);
        self.socket.send(&packet).await.unwrap();
    }

    async fn echo(&mut self, target: &UdpSocket) {
        let dst = SocksAddr::from(target.local_addr().unwrap());
        self.send(dst.clone(), b"ping").await;
        let mut payload = [0; 64];
        let (len, peer) = target.recv_from(&mut payload).await.unwrap();
        assert_eq!(&payload[..len], b"ping");
        target.send_to(b"pong", peer).await.unwrap();
        let len = self.socket.recv(&mut payload).await.unwrap();
        let mut packet = std::io::Cursor::new(&payload[..len]);
        let header = UdpReqHeader::decode(&mut packet).await.unwrap();
        assert_eq!(header.dst, dst);
        assert_eq!(&payload[packet.position() as usize..len], b"pong");
    }
}
