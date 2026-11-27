//! The same as `dual_stack_endpoint`, for SunnyQUIC: a client whose socket
//! was created for one address family must keep connecting when the server
//! name later resolves to the other family first, or to the other family only.
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
};

use shadowquic::{
    Manager,
    config::{AuthUser, CongestionControl, SunnyQuicClientCfg, SunnyQuicServerCfg},
    direct::outbound::DirectOut,
    dns::DnsService,
    error::SError,
    quic::QuicConnection,
    sunnyquic::{inbound::SunnyQuicServer, outbound::SunnyQuicClient},
    utils::socket_opt::UdpSocketFactory,
};

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

async fn start_server(bind_addr: SocketAddr) {
    let server = SunnyQuicServer::new(SunnyQuicServerCfg {
        tag: "inbound".into(),
        bind_addr,
        users: vec![AuthUser {
            username: "user".into(),
            password: "pass".into(),
        }],
        alpn: vec!["h3".into()],
        congestion_control: CongestionControl::Bbr,
        server_name: "localhost".into(),
        cert_path: "../assets/certs/localhost.crt".into(),
        key_path: "../assets/certs/localhost.key".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    tokio::spawn(Manager::single(Box::new(server), Arc::new(DirectOut::default())).run());
}

#[tokio::test]
async fn client_follows_the_address_family_of_the_server_name() {
    const V4: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let mapped = IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped());
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
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let dns = Arc::new(FakeDns(Mutex::new(vec![V4])));
    let addr = format!("sq.test:{port}");
    let mut client = SunnyQuicClient::new(
        SunnyQuicClientCfg {
            tag: "outbound".into(),
            username: "user".into(),
            password: "pass".into(),
            addr: addr.clone(),
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            cert_path: Some("../assets/certs/MyCA.pem".into()),
            ..Default::default()
        },
        Arc::new(shadowquic::dns::ResolverManager::new()),
    );
    client.addr_resolver = dns.clone();
    // A host without a dual-stack socket: the socket takes the family of the
    // first resolved address.
    client.socket_factory = Arc::new(UdpSocketFactory {
        addr,
        interface: None,
        fw_mark: None,
        protect_path: None,
        try_dual_stack: false,
        resolver: dns.clone(),
    });

    let connect = async |ips: &[IpAddr], case: &str| {
        *dns.0.lock().unwrap() = ips.to_vec();
        let conn = tokio::time::timeout(std::time::Duration::from_secs(5), client.get_conn())
            .await
            .unwrap_or_else(|_| panic!("{case}: timed out"))
            .unwrap_or_else(|e| panic!("{case}: {e}"));
        let remote = conn.remote_address().ip().to_canonical();
        QuicConnection::close(&***conn, 0, b"");
        remote
    };
    assert_eq!(connect(&[V4], "IPv4 only").await, V4);
    assert_eq!(connect(&[mapped], "IPv4 in mapped form").await, V4);
    assert_eq!(connect(&[V6, V4], "IPv6 listed first").await, V4);
    assert_eq!(connect(&[V6], "IPv6 only").await, V6);
    // Either family: whether the IPv6 socket also carries IPv4 is up to the OS.
    connect(&[V4, V6], "IPv4 listed first").await;
    assert_eq!(connect(&[V4], "IPv4 only again").await, V4);
}
