//! A client whose socket was created for one address family must keep
//! connecting when the server name later resolves to the other family first,
//! or to the other family only (upstream issue #125).
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use shadowquic::{
    Manager,
    config::{
        AuthUser, CongestionControl, JlsUpstream, PortHoppingCfg, ShadowQuicClientCfg,
        ShadowQuicServerCfg,
    },
    direct::outbound::DirectOut,
    dns::DnsService,
    error::SError,
    quic::{QuicConnection, QuicServer},
    shadowquic::{EndServer, inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    utils::socket_opt::UdpSocketFactory,
};
use tokio::time::timeout;

struct FakeDns(Mutex<Vec<IpAddr>>);

#[async_trait::async_trait]
impl DnsService for FakeDns {
    async fn resolve(&self, _domain: &str) -> Result<Vec<IpAddr>, SError> {
        Ok(self.0.lock().unwrap().clone())
    }
    #[cfg(feature = "dns-server")]
    async fn exchange(&self, _query: &[u8]) -> Result<Vec<u8>, SError> {
        Err(SError::ProtocolUnimpl)
    }
}

const V4: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

fn server_cfg(bind_addr: SocketAddr) -> ShadowQuicServerCfg {
    ShadowQuicServerCfg {
        tag: "inbound".into(),
        bind_addr,
        users: vec![AuthUser {
            username: "user".into(),
            password: "pass".into(),
        }],
        jls_upstream: JlsUpstream {
            addr: "127.0.0.1:9".into(),
            ..Default::default()
        },
        alpn: vec!["h3".into()],
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    }
}

async fn start_server(bind_addr: SocketAddr) {
    let server = ShadowQuicServer::new(server_cfg(bind_addr)).await.unwrap();
    tokio::spawn(Manager::single(Box::new(server), Arc::new(DirectOut::default())).run());
}

/// A client of `sq.test:{port}` that resolves the name with `dns` and, like a
/// host without a dual-stack socket, creates a socket of the family of the
/// first resolved address.
fn client(port: u16, dns: &Arc<FakeDns>, port_hopping: PortHoppingCfg) -> ShadowQuicClient {
    let addr = format!("sq.test:{port}");
    let mut client = ShadowQuicClient::new(
        ShadowQuicClientCfg {
            tag: "outbound".into(),
            username: "user".into(),
            password: "pass".into(),
            addr: addr.clone(),
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            port_hopping,
            ..Default::default()
        },
        Arc::new(shadowquic::dns::ResolverManager::new()),
    );
    client.addr_resolver = dns.clone();
    client.socket_factory = Arc::new(UdpSocketFactory {
        addr,
        interface: None,
        fw_mark: None,
        protect_path: None,
        try_dual_stack: false,
        resolver: dns.clone(),
    });
    client
}

#[tokio::test]
async fn client_follows_the_address_family_of_the_server_name() {
    let port = {
        let probe = std::net::UdpSocket::bind((V4, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        if std::net::UdpSocket::bind((V6, port)).is_err() {
            eprintln!("no IPv6 loopback on this host: test not run");
            return;
        }
        port
    };
    start_server(SocketAddr::new(V4, port)).await;
    start_server(SocketAddr::new(V6, port)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let dns = Arc::new(FakeDns(Mutex::new(vec![V4])));
    let client = client(port, &dns, PortHoppingCfg::default());

    let connect = async |ips: &[IpAddr], case: &str| {
        *dns.0.lock().unwrap() = ips.to_vec();
        let conn = timeout(Duration::from_secs(5), client.get_conn())
            .await
            .unwrap_or_else(|_| panic!("{case}: timed out"))
            .unwrap_or_else(|e| panic!("{case}: {e}"));
        let remote = conn.remote_address().ip().to_canonical();
        QuicConnection::close(&***conn, 0, b"");
        remote
    };
    assert_eq!(connect(&[V4], "IPv4 only").await, V4);
    assert_eq!(connect(&[V6, V4], "IPv6 listed first").await, V4);
    assert_eq!(connect(&[V6], "IPv6 only").await, V6);
    // Either family: whether the IPv6 socket also carries IPv4 is up to the OS.
    connect(&[V4, V6], "IPv4 listed first").await;
    assert_eq!(connect(&[V4], "IPv4 only again").await, V4);
}

/// A resolver may hand out an IPv4 address in its IPv6-mapped form. A socket
/// of the IPv4 family reaches it, but only under its IPv4 form.
#[tokio::test]
async fn mapped_ipv4_address_reaches_an_ipv4_endpoint() {
    let mapped = IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped());
    let port = std::net::UdpSocket::bind((V4, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    start_server(SocketAddr::new(V4, port)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let dns = Arc::new(FakeDns(Mutex::new(vec![V4])));
    let client = client(port, &dns, PortHoppingCfg::default());
    for (ips, case) in [(vec![V4], "IPv4"), (vec![mapped], "IPv4 in mapped form")] {
        *dns.0.lock().unwrap() = ips;
        let conn = timeout(Duration::from_secs(5), client.get_conn())
            .await
            .unwrap_or_else(|_| panic!("{case}: timed out"))
            .unwrap_or_else(|e| panic!("{case}: {e}"));
        assert_eq!(conn.remote_address().ip().to_canonical(), V4, "{case}");
        QuicConnection::close(&***conn, 0, b"");
    }
}

/// A port hop must not move a connection to a socket of the other address
/// family, which cannot reach the server; the hops go on once the server name
/// resolves to the family of the connection again.
#[tokio::test]
async fn port_hop_keeps_the_address_family_of_the_connection() {
    if std::net::UdpSocket::bind((V6, 0)).is_err() {
        eprintln!("no IPv6 loopback on this host: test not run");
        return;
    }
    let server = EndServer::new(&server_cfg(SocketAddr::new(V6, 0)))
        .await
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let dns = Arc::new(FakeDns(Mutex::new(vec![V6])));
    let client = client(
        port,
        &dns,
        PortHoppingCfg {
            enable: true,
            interval: 1_000.into(),
            interval_jitter: 0.into(),
        },
    );

    timeout(Duration::from_secs(20), async {
        let (client_conn, server_conn) =
            tokio::join!(client.get_conn(), QuicServer::accept(&server));
        let client_conn = client_conn.unwrap();
        let server_conn = server_conn.unwrap();
        let mut addresses = server_conn.watch_remote_address();

        let (mut client_send, mut client_recv) = client_conn.open_bi().await.unwrap();
        client_send.write_all(b"a").await.unwrap();
        let (mut server_send, mut server_recv) = server_conn.accept_bi().await.unwrap();
        let mut received = [0];
        server_recv.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"a");

        // Two hops come while the name resolves to IPv4 only.
        *dns.0.lock().unwrap() = vec![V4];
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        timeout(Duration::from_secs(3), async {
            client_send.write_all(b"b").await.unwrap();
            server_recv.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"b");
            server_send.write_all(b"x").await.unwrap();
            client_recv.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"x");
        })
        .await
        .expect("a port hop cut the connection off from the server");

        *dns.0.lock().unwrap() = vec![V6];
        let port_before = server_conn.remote_address().port();
        timeout(Duration::from_secs(5), async {
            while addresses
                .wait()
                .await
                .expect("connection closed while waiting for a port hop")
                .port()
                == port_before
            {}
        })
        .await
        .expect("no port hop once the name resolved to IPv6 again");
        client_send.write_all(b"c").await.unwrap();
        server_recv.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"c");
        server_send.write_all(b"y").await.unwrap();
        client_recv.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"y");
    })
    .await
    .expect("timed out exchanging data over the port-hopping connection");
}
