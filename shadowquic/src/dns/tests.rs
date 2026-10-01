use super::config::{DnsFakeIpServerCfg, DnsSystemServerCfg, DnsUdpServerCfg};
use super::*;
use crate::{
    Manager, UdpRecv,
    config::{Config, DirectOutCfg},
    direct::outbound::DirectOut,
    msgs::socks5::AddrOrDomain,
};
use std::net::{Ipv4Addr, SocketAddr};

fn udp_cfg(upstream: SocketAddr) -> DnsUdpServerCfg {
    DnsUdpServerCfg {
        tag: "dns".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        upstream,
    }
}
fn query(name: &str, ty: TYPE, id: u16) -> Vec<u8> {
    let mut query = Packet::new_query(id);
    query.set_flags(PacketFlag::RECURSION_DESIRED);
    query.questions.push(Question::new(
        Name::new(name).unwrap(),
        ty.into(),
        CLASS::IN.into(),
        false,
    ));
    query.build_bytes_vec().unwrap()
}
fn response(query: &[u8], ttl: u32) -> Vec<u8> {
    let mut reply = reply_for(Packet::parse(query).unwrap());
    reply.answers.push(ResourceRecord::new(
        reply.questions[0].qname.clone(),
        CLASS::IN,
        ttl,
        RData::A(Ipv4Addr::LOCALHOST.into()),
    ));
    reply.build_bytes_vec().unwrap()
}
async fn exchange_udp(addr: SocketAddr, query: &[u8]) -> Vec<u8> {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.send_to(query, addr).await.unwrap();
    let mut buffer = vec![0; 2000];
    let (len, _) = tokio::time::timeout(Duration::from_secs(3), socket.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    buffer.truncate(len);
    buffer
}
fn run(
    server: DnsServer,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let manager = Manager::single(
        Box::new(server),
        Arc::new(DirectOut::new(DirectOutCfg::default())),
    );
    (
        stop,
        tokio::spawn(manager.run_until(async {
            let _ = stopped.await;
        })),
    )
}

#[tokio::test]
async fn udp_upstream_is_routed_and_cached_with_client_transaction_id() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server = udp_cfg(upstream.local_addr().unwrap())
        .build()
        .await
        .unwrap();
    let addr = server.local_addr;
    let (stop, task) = run(server);
    let mock = tokio::spawn(async move {
        let mut buffer = vec![0; 2000];
        let (len, peer) = upstream.recv_from(&mut buffer).await.unwrap();
        // A spoofed transaction is ignored before accepting the matching reply.
        let mut wrong = response(&buffer[..len], 60);
        wrong[0] ^= 1;
        upstream.send_to(&wrong, peer).await.unwrap();
        upstream
            .send_to(&response(&buffer[..len], 60), peer)
            .await
            .unwrap();
        upstream
    });
    for id in [123, 456] {
        let bytes = exchange_udp(addr, &query("routed.test", TYPE::A, id)).await;
        let reply = Packet::parse(&bytes).unwrap();
        assert_eq!(reply.id(), id);
        assert_eq!(
            cache::addresses(&reply),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        );
    }
    let upstream = mock.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), upstream.recv_from(&mut [0; 512]))
            .await
            .is_err()
    );
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn truncated_udp_retries_over_tcp_and_local_tcp_is_framed() {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = tcp.local_addr().unwrap();
    let udp = UdpSocket::bind(upstream_addr).await.unwrap();
    let server = udp_cfg(upstream_addr).build().await.unwrap();
    let addr = server.local_addr;
    let (stop, task) = run(server);
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 512];
        let (len, peer) = udp.recv_from(&mut buffer).await.unwrap();
        let mut truncated = reply_for(Packet::parse(&buffer[..len]).unwrap());
        truncated.set_flags(PacketFlag::TRUNCATION);
        udp.send_to(&truncated.build_bytes_vec().unwrap(), peer)
            .await
            .unwrap();
        let (mut stream, _) = tcp.accept().await.unwrap();
        let query = read_frame(&mut stream).await.unwrap();
        write_frame(&mut stream, &response(&query, 60))
            .await
            .unwrap();
    });
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    for id in [1, 2] {
        write_frame(&mut stream, &query("tcp-fallback.test", TYPE::A, id))
            .await
            .unwrap();
        let bytes = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut stream))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(Packet::parse(&bytes).unwrap().id(), id);
        assert_eq!(Packet::parse(&bytes).unwrap().answers.len(), 1);
    }
    mock.await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn cache_expires_and_ages_ttls() {
    let cache = DnsCache::default();
    let query = query("cache.test", TYPE::A, 1);
    let reply = response(&query, 60);
    cache.insert(&query, Packet::parse(&reply).unwrap());
    assert_eq!(
        cache.lookup_cache("CACHE.TEST."),
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
    );
    assert_eq!(
        cache
            .reverse_lookup_cache(Ipv4Addr::LOCALHOST.into())
            .as_deref(),
        Some("cache.test")
    );
    cache.age_for_test(Duration::from_secs(10));
    let bytes = cache.get(&query).unwrap().unwrap();
    assert_eq!(Packet::parse(&bytes).unwrap().answers[0].ttl, 50);
    cache.age_for_test(Duration::from_secs(51));
    assert!(cache.lookup_cache("cache.test").is_empty());
    assert!(
        cache
            .reverse_lookup_cache(Ipv4Addr::LOCALHOST.into())
            .is_none()
    );
    assert!(cache.get(&query).unwrap().is_none());
    cache.insert(&query, Packet::parse(&response(&query, 0)).unwrap());
    assert!(cache.get(&query).unwrap().is_none());
}

#[tokio::test]
async fn fakeip_is_stable_dual_stack_and_restores_ports() {
    let server = DnsFakeIpServerCfg {
        tag: "dns".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
    }
    .build()
    .await
    .unwrap();
    let fake = server.resolver.fake_ip.as_ref().unwrap();
    let ips = server.resolve("Example.Test").await.unwrap();
    assert_eq!(ips.len(), 2);
    for ip in ips {
        let restored = fake.restore(&SocketAddr::new(ip, 443).into()).unwrap();
        assert_eq!(restored, SocksAddr::from_domain("example.test".into(), 443));
        assert_eq!(fake.allocate("EXAMPLE.TEST.", ip.is_ipv6()).unwrap(), ip);
    }
    assert!(
        fake.restore(&"198.18.10.10:443".parse::<SocketAddr>().unwrap().into())
            .is_err()
    );
    let bytes = server
        .exchange(&query("example.test", TYPE::HTTPS, 2))
        .await
        .unwrap();
    assert!(Packet::parse(&bytes).unwrap().answers.is_empty());
}

#[tokio::test]
async fn system_resolves_localhost_and_rejects_unsupported_records() {
    let server = DnsSystemServerCfg {
        tag: "dns".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
    }
    .build()
    .await
    .unwrap();
    assert!(
        server
            .resolve("localhost")
            .await
            .unwrap()
            .iter()
            .any(IpAddr::is_loopback)
    );
    let bytes = server
        .exchange(&query("localhost", TYPE::MX, 2))
        .await
        .unwrap();
    assert_eq!(
        Packet::parse(&bytes).unwrap().rcode(),
        RCODE::NotImplemented
    );
    assert!(server.exchange(&[0, 1, 2]).await.is_err());
}

#[test]
fn rejects_invalid_resolver_configuration() {
    let base = "inbounds:\n  - {tag: dns, type: dns-system, bind-addr: '127.0.0.1:0'}\noutbounds:\n  - {tag: direct, type: direct, dns: dns}\n";
    serde_saphyr::from_str::<Config>(base)
        .unwrap()
        .validate()
        .unwrap();
    for yaml in [
        base.replace("dns: dns", "dns: missing"),
        base.replace("dns-system", "dns-fakeip"),
        base.replace("outbounds:", "  - {tag: fake1, type: dns-fakeip, bind-addr: '127.0.0.1:0'}\n  - {tag: fake2, type: dns-fakeip, bind-addr: '127.0.0.1:0'}\noutbounds:"),
    ] {
        assert!(serde_saphyr::from_str::<Config>(&yaml).unwrap().validate().is_err(), "{yaml}");
    }
}

#[tokio::test]
async fn hijacked_dns_keeps_original_reply_source() {
    let config: Config = serde_saphyr::from_str(
        "inbounds: [{tag: fake, type: dns-fakeip, bind-addr: '127.0.0.1:0'}]",
    )
    .unwrap();
    let manager = config.build_manager().await.unwrap();
    assert_eq!(manager.default_outbound, "fake");
    assert_eq!(manager.outbounds.len(), 1);
    let (send, recv) = mpsc::channel(1);
    let (reply_send, mut reply_recv) = mpsc::channel(1);
    let dst: SocksAddr = "192.0.2.53:53".parse::<SocketAddr>().unwrap().into();
    send.send((query("hijacked.test", TYPE::A, 42).into(), dst.clone()))
        .await
        .unwrap();
    let session = UdpSession::from_recv(
        Arc::new(reply_send),
        Box::new(recv),
        None,
        "0.0.0.0:0".parse::<SocketAddr>().unwrap().into(),
        UserContext::default(),
    )
    .await
    .unwrap();
    manager.outbounds["fake"]
        .handle(ProxyRequest::Udp(session))
        .await
        .unwrap();
    let (bytes, source) = tokio::time::timeout(Duration::from_secs(1), reply_recv.recv_from())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source, dst);
    assert_eq!(Packet::parse(&bytes).unwrap().id(), 42);
}

struct OneRequest(Option<ProxyRequest>);
#[async_trait]
impl Inbound for OneRequest {
    async fn accept(&mut self) -> Result<ProxyRequest> {
        Ok(self.0.take().unwrap())
    }
}
#[tokio::test]
async fn fakeip_udp_is_restored_before_routing_and_replies_use_fake_source() {
    let fake = Arc::new(FakeIp::default());
    let ip = fake.allocate("transparent.test", false).unwrap();
    let dst: SocksAddr = SocketAddr::new(ip, 8080).into();
    let (tx, rx) = mpsc::channel(2);
    let (reply_tx, mut reply_rx) = mpsc::channel(2);
    tx.send((Bytes::from_static(b"one"), dst.clone()))
        .await
        .unwrap();
    let session = UdpSession::from_recv(
        Arc::new(reply_tx),
        Box::new(rx),
        None,
        "0.0.0.0:0".parse::<SocketAddr>().unwrap().into(),
        UserContext::default(),
    )
    .await
    .unwrap();
    let mut inbound = RestoringInbound {
        inner: Box::new(OneRequest(Some(ProxyRequest::Udp(session)))),
        fake,
    };
    let request = inbound.accept().await.unwrap();
    assert!(matches!(request.dst().addr, AddrOrDomain::Domain(_)));
    let ProxyRequest::Udp(mut session) = request else {
        panic!()
    };
    let (bytes, domain) = session.recv.recv_from().await.unwrap();
    assert_eq!(
        domain,
        SocksAddr::from_domain("transparent.test".into(), 8080)
    );
    session.send.send_to(bytes, domain).await.unwrap();
    assert_eq!(reply_rx.recv_from().await.unwrap().1, dst);
    tx.send((Bytes::from_static(b"two"), dst)).await.unwrap();
    assert!(matches!(
        session.recv.recv_from().await.unwrap().1.addr,
        AddrOrDomain::Domain(_)
    ));
}

async fn tls_exchange(trusted: bool, server_name: &str) -> Result<Vec<u8>> {
    let cert = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let cert_der = cert.cert.der().clone();
    let key = rustls_jls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let provider = {
        #[cfg(feature = "ring")]
        {
            Arc::new(rustls_jls::crypto::ring::default_provider())
        }
        #[cfg(all(not(feature = "ring"), feature = "aws-lc-rs"))]
        {
            Arc::new(rustls_jls::crypto::aws_lc_rs::default_provider())
        }
    };
    let mut config = rustls_jls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key.into())
        .unwrap();
    config.jls_config = rustls_jls::jls::JlsServerConfig::default()
        .enable(false)
        .into();
    let acceptor = tokio_rustls_jls::TlsAcceptor::from(Arc::new(config));
    let mut roots = rustls_jls::RootCertStore::empty();
    if trusted {
        roots.add(cert_der).unwrap();
    }
    let mut client = rustls_jls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.jls_config.enable = false;
    let (requests, mut received) = mpsc::channel(1);
    let resolver = Resolver {
        tag: "dns".into(),
        backend: Backend::Tls {
            upstream: "192.0.2.1:853".parse().unwrap(),
            server_name: rustls_jls::pki_types::ServerName::try_from(server_name.to_owned())
                .unwrap(),
            connector: tokio_rustls_jls::TlsConnector::from(Arc::new(client)),
        },
        requests,
        fake_ip: None,
    };
    let upstream = tokio::spawn(async move {
        let Some(ProxyRequest::Tcp(session)) = received.recv().await else {
            panic!("TLS must emit a TCP proxy request")
        };
        assert_eq!(session.dst.to_string(), "192.0.2.1:853");
        if let Ok(mut stream) = acceptor.accept(session.stream).await {
            let query = read_frame(&mut stream).await.unwrap();
            // Zero TTL ensures later certificate tests cannot hit this response.
            write_frame(&mut stream, &response(&query, 0))
                .await
                .unwrap();
        }
    });
    let result = resolver.exchange(&query("tls.test", TYPE::A, 17)).await;
    upstream.await.unwrap();
    result
}

#[tokio::test]
async fn tls_uses_routed_stream_and_verifies_certificate_and_hostname() {
    let reply = tls_exchange(true, "dns.test").await.unwrap();
    assert_eq!(Packet::parse(&reply).unwrap().answers.len(), 1);
    assert!(tls_exchange(false, "dns.test").await.is_err());
    assert!(tls_exchange(true, "wrong.test").await.is_err());
}

struct Capture(tokio::sync::mpsc::Sender<ProxyRequest>);
#[async_trait]
impl Outbound for Capture {
    async fn handle(&self, req: ProxyRequest) -> Result<()> {
        self.0.send(req).await.map_err(dns_error)
    }
}

#[tokio::test]
async fn selected_resolver_handles_tcp_and_udp_destinations_and_preserves_ports() {
    let server = DnsSystemServerCfg {
        tag: "dns".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
    }
    .build()
    .await
    .unwrap();
    let (tx, mut rx) = mpsc::channel(2);
    let outbound = ResolvingOutbound {
        inner: Arc::new(Capture(tx)),
        resolver: server.resolver.clone(),
        strategy: crate::config::DnsStrategy::Ipv4Only,
    };
    let (stream, _peer) = tokio::io::duplex(64);
    outbound
        .handle(ProxyRequest::Tcp(TcpSession {
            stream: Box::new(stream),
            dst: SocksAddr::from_domain("localhost".into(), 1234),
            src_addr: None,
            user_context: UserContext::default(),
        }))
        .await
        .unwrap();
    let request = rx.recv().await.unwrap();
    assert_eq!(request.dst().to_string(), "127.0.0.1:1234");
    let (tx, recv) = mpsc::channel(2);
    let (send, mut replies) = mpsc::channel(2);
    let first = SocksAddr::from_domain("localhost".into(), 1234);
    let second = SocksAddr::from_domain("localhost".into(), 5678);
    tx.send((Bytes::from_static(b"first"), first.clone()))
        .await
        .unwrap();
    tx.send((Bytes::from_static(b"second"), second.clone()))
        .await
        .unwrap();
    let session = UdpSession::from_recv(
        Arc::new(send),
        Box::new(recv),
        None,
        "0.0.0.0:0".parse::<SocketAddr>().unwrap().into(),
        UserContext::default(),
    )
    .await
    .unwrap();
    outbound.handle(ProxyRequest::Udp(session)).await.unwrap();
    let Some(ProxyRequest::Udp(mut session)) = rx.recv().await else {
        panic!()
    };
    for expected in [first, second] {
        let (bytes, dst) = session.recv.recv_from().await.unwrap();
        assert_eq!(
            dst,
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), expected.port).into()
        );
        session.send.send_to(bytes, dst).await.unwrap();
        assert_eq!(replies.recv_from().await.unwrap().1, expected);
    }
}

#[tokio::test]
async fn manager_builds_all_dns_transports_and_resolver_references() {
    let config: Config = serde_saphyr::from_str(r#"
inbounds:
  - {tag: system, type: dns-system, bind-addr: '127.0.0.1:0'}
  - {tag: fake, type: dns-fakeip, bind-addr: '127.0.0.1:0'}
  - {tag: udp, type: dns-udp, bind-addr: '127.0.0.1:0', upstream: '127.0.0.1:53'}
  - {tag: tcp, type: dns-tcp, bind-addr: '127.0.0.1:0', upstream: '127.0.0.1:53'}
  - {tag: tls, type: dns-tls, bind-addr: '127.0.0.1:0', upstream: '127.0.0.1:853', server-name: dns.test}
outbounds:
  - {tag: direct, type: direct, dns: system}
  - {tag: socks, type: socks, addr: '127.0.0.1:1080', dns: system}
  - {tag: shadow, type: shadowquic, addr: '127.0.0.1:443', username: test, password: test, server-name: localhost, dns: system}
  - {tag: sunny, type: sunnyquic, addr: '127.0.0.1:443', username: test, password: test, server-name: localhost, dns: system}
"#).unwrap();
    let manager = config.build_manager().await.unwrap();
    assert_eq!(manager.inbounds.len(), 5);
    assert_eq!(manager.outbounds.len(), 9);
    assert_eq!(manager.default_outbound, "direct");
    for tag in ["system", "fake", "udp", "tcp", "tls"] {
        assert!(manager.outbounds.contains_key(tag));
    }
}

#[cfg(feature = "plugin")]
#[tokio::test]
async fn routing_scripts_can_query_shared_cache() {
    use crate::plugin::router::{RouteContext, Router};
    let ip: Ipv4Addr = "203.0.113.250".parse().unwrap();
    let query = query("lua-cache.test", TYPE::A, 1);
    let mut reply = reply_for(Packet::parse(&query).unwrap());
    reply.answers.push(ResourceRecord::new(
        reply.questions[0].qname.clone(),
        CLASS::IN,
        60,
        RData::A(ip.into()),
    ));
    global_cache().insert(&query, reply);
    let router = Router::from_source(&format!(
        r#"
        return function(ctx)
            local ips = lookup_cache("lua-cache.test")
            assert(#ips == 1)
            assert(reverse_lookup_cache("{ip}") == "lua-cache.test")
            assert(#lookup_cache("absent.test") == 0)
            assert(reverse_lookup_cache("192.0.2.234") == nil)
            return "direct"
        end
    "#
    ))
    .unwrap();
    let (stream, _peer) = tokio::io::duplex(64);
    let request: ProxyRequest = ProxyRequest::Tcp(TcpSession {
        stream: Box::new(stream),
        dst: "127.0.0.1:80".parse::<SocketAddr>().unwrap().into(),
        src_addr: None,
        user_context: UserContext::default(),
    });
    assert_eq!(
        router
            .route(&mut RouteContext::from_request(&request))
            .unwrap(),
        "direct"
    );
}

#[tokio::test]
async fn dns_udp_reply_respects_client_limit() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server = udp_cfg(upstream.local_addr().unwrap())
        .build()
        .await
        .unwrap();
    let resolver = server.resolver.clone();
    let addr = server.local_addr;
    let (stop, task) = run(server);
    let query = query("large.test", TYPE::A, 1);
    let expected = {
        let mut reply = reply_for(Packet::parse(&query).unwrap());
        for index in 1..=40u8 {
            reply.answers.push(ResourceRecord::new(
                reply.questions[0].qname.clone(),
                CLASS::IN,
                60,
                RData::A(Ipv4Addr::new(192, 0, 2, index).into()),
            ));
        }
        reply.build_bytes_vec().unwrap()
    };
    assert!(expected.len() > 512 && expected.len() <= 2000);
    let upstream_reply = expected.clone();
    let mock = tokio::spawn(async move {
        let (_, peer) = upstream.recv_from(&mut [0; 512]).await.unwrap();
        upstream.send_to(&upstream_reply, peer).await.unwrap();
    });
    let reply = resolver.exchange(&query).await.unwrap();
    assert_eq!(reply, expected);
    let bytes = exchange_udp(addr, &query).await;
    assert!(bytes.len() <= 512);
    assert!(
        Packet::parse(&bytes)
            .unwrap()
            .has_flags(PacketFlag::TRUNCATION)
    );
    mock.await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[cfg(feature = "plugin")]
#[test]
fn dns_example_is_valid() {
    serde_saphyr::from_str::<Config>(include_str!("../../config_examples/dns.yaml"))
        .unwrap()
        .validate()
        .unwrap();
}

#[cfg(feature = "plugin")]
#[tokio::test]
async fn fakeip_tcp_is_restored_before_lua_routing() {
    let fake = Arc::new(FakeIp::default());
    let ip = fake.allocate("transparent-tcp.test", true).unwrap();
    let (stream, _peer) = tokio::io::duplex(64);
    let session = TcpSession {
        stream: Box::new(stream) as AnyTcp,
        dst: SocketAddr::new(ip, 8443).into(),
        src_addr: None,
        user_context: UserContext::default(),
    };
    let mut inbound = RestoringInbound {
        inner: Box::new(OneRequest(Some(ProxyRequest::Tcp(session)))),
        fake,
    };
    let req = inbound.accept().await.unwrap();
    let mut context = crate::plugin::router::RouteContext::from_request(&req);
    let router = crate::plugin::router::Router::from_source(
        r#"
        return function(ctx)
            assert(ctx.dst_domain == "transparent-tcp.test")
            assert(ctx.dst_ip_v4 == nil and ctx.dst_ip_v6 == nil)
            assert(ctx.dst_port == 8443)
            return "direct"
        end
    "#,
    )
    .unwrap();
    assert_eq!(router.route(&mut context).unwrap(), "direct");
}

#[tokio::test]
async fn automatic_dns_outbound_can_be_selected_as_default() {
    let config: Config = serde_saphyr::from_str(
        r#"
inbounds:
  - {tag: resolver, type: dns-system, bind-addr: '127.0.0.1:0'}
outbounds:
  - {tag: direct, type: direct}
router:
  default-outbound: resolver
"#,
    )
    .unwrap();
    let manager = config.build_manager().await.unwrap();
    assert_eq!(manager.default_outbound, "resolver");
    assert!(manager.outbounds.contains_key("resolver"));
}

#[test]
fn automatic_dns_outbound_rejects_explicit_tag_collisions() {
    let config: Config = serde_saphyr::from_str(
        r#"
inbounds:
  - {tag: resolver, type: dns-system, bind-addr: '127.0.0.1:0'}
outbounds:
  - {tag: resolver, type: direct}
"#,
    )
    .unwrap();
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate outbound tag: resolver")
    );
}

#[test]
fn explicit_dns_outbound_is_not_a_config_type() {
    assert!(
        serde_saphyr::from_str::<crate::config::OutboundCfg>(
            "{tag: hijack, type: dns, dns: resolver}"
        )
        .is_err()
    );
}

#[test]
fn dns_configurations_require_only_their_own_fields() {
    use crate::config::InboundCfg;

    for (kind, fields, forbidden) in [
        (
            "dns-udp",
            ", upstream: '127.0.0.1:53'",
            ", server-name: dns.test",
        ),
        (
            "dns-tcp",
            ", upstream: '127.0.0.1:53'",
            ", server-name: dns.test",
        ),
        (
            "dns-tls",
            ", upstream: '127.0.0.1:853', server-name: dns.test",
            ", unknown: true",
        ),
        ("dns-fakeip", "", ", upstream: '127.0.0.1:53'"),
        ("dns-system", "", ", upstream: '127.0.0.1:53'"),
    ] {
        let base = format!("tag: dns, type: {kind}, bind-addr: '127.0.0.1:0'");
        let valid = format!("{{{base}{fields}}}");
        assert!(
            serde_saphyr::from_str::<InboundCfg>(&valid)
                .unwrap()
                .is_dns()
        );
        let invalid = format!("{{{base}{fields}{forbidden}}}");
        assert!(
            serde_saphyr::from_str::<InboundCfg>(&invalid).is_err(),
            "{invalid}"
        );
        if !fields.is_empty() {
            assert!(serde_saphyr::from_str::<InboundCfg>(&format!("{{{base}}}")).is_err());
            assert!(
                serde_saphyr::from_str::<InboundCfg>(
                    &valid
                        .replace("upstream: '127.0.0.1:53'", "upstream: null")
                        .replace("upstream: '127.0.0.1:853'", "upstream: null")
                )
                .is_err()
            );
        }
    }
    for kind in ["dns-fakeip", "dns-system"] {
        assert!(
            serde_saphyr::from_str::<InboundCfg>(&format!(
                "{{tag: dns, type: {kind}, bind-addr: '127.0.0.1:0', server-name: dns.test}}"
            ))
            .is_err()
        );
    }
    for name in ["", ", server-name: null"] {
        assert!(serde_saphyr::from_str::<InboundCfg>(&format!(
            "{{tag: dns, type: dns-tls, bind-addr: '127.0.0.1:0', upstream: '127.0.0.1:853'{name}}}"
        )).is_err());
    }
}

#[tokio::test]
async fn tls_configuration_rejects_invalid_server_name_before_binding() {
    let config = super::config::DnsTlsServerCfg {
        tag: "tls".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        upstream: "127.0.0.1:853".parse().unwrap(),
        server_name: "invalid name".into(),
    };
    assert!(config.validate().is_err());
    assert!(config.build().await.is_err());
}

#[test]
fn dns_tls_connector_disables_jls() {
    let connector = tls_connector().unwrap();
    assert!(!connector.config().jls_config.enable);
}

#[tokio::test]
async fn reverse_lookup_routes_ipv4_and_ipv6_ptr_queries_and_caches_answers() {
    use simple_dns::rdata::{CNAME, PTR};
    for (ip, expected_name) in [
        ("192.0.2.7", "7.2.0.192.in-addr.arpa"),
        (
            "2001:db8::1",
            "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.ip6.arpa",
        ),
    ] {
        let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = udp_cfg(upstream.local_addr().unwrap())
            .build()
            .await
            .unwrap();
        let resolver = server.resolver.clone();
        let (stop, task) = run(server);
        let mock = tokio::spawn(async move {
            let mut buffer = [0; 2000];
            let (size, peer) = upstream.recv_from(&mut buffer).await.unwrap();
            let query = Packet::parse(&buffer[..size]).unwrap();
            assert!(query.has_flags(PacketFlag::RECURSION_DESIRED));
            assert_eq!(query.questions[0].qname.to_string(), expected_name);
            assert_eq!(query.questions[0].qtype, TYPE::PTR.into());
            assert_eq!(query.questions[0].qclass, CLASS::IN.into());
            let mut reply = reply_for(query);
            let alias = Name::new("delegated.reverse.test").unwrap();
            reply.answers.push(ResourceRecord::new(
                reply.questions[0].qname.clone(),
                CLASS::IN,
                60,
                RData::CNAME(CNAME(alias.clone())),
            ));
            for hostname in ["host.test", "HOST.TEST", "other.test"] {
                reply.answers.push(ResourceRecord::new(
                    alias.clone(),
                    CLASS::IN,
                    60,
                    RData::PTR(PTR(Name::new(hostname).unwrap())),
                ));
            }
            reply.answers.push(ResourceRecord::new(
                Name::new("unrelated.reverse.test").unwrap(),
                CLASS::IN,
                60,
                RData::PTR(PTR(Name::new("unrelated.test").unwrap())),
            ));
            upstream
                .send_to(&reply.build_bytes_vec().unwrap(), peer)
                .await
                .unwrap();
            upstream
        });
        for _ in 0..2 {
            let names = resolver.reverse_lookup(ip.parse().unwrap()).await.unwrap();
            assert_eq!(names, ["host.test", "other.test"]);
            assert_eq!(
                global_cache()
                    .reverse_lookup_cache(ip.parse().unwrap())
                    .as_deref(),
                Some("host.test")
            );
        }
        let upstream = mock.await.unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                upstream.recv_from(&mut [0; 2000])
            )
            .await
            .is_err()
        );
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}

struct StaticDnsReply(Vec<u8>);
#[async_trait]
impl DnsService for StaticDnsReply {
    async fn exchange(&self, _: &[u8]) -> Result<Vec<u8>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn reverse_lookup_rejects_failed_empty_malformed_and_unrelated_answers() {
    use simple_dns::rdata::PTR;
    let query = query("7.2.0.192.in-addr.arpa", TYPE::PTR, 0);
    for mode in [
        "nxdomain",
        "servfail",
        "truncated",
        "empty",
        "unrelated",
        "wrong-id",
        "wrong-class",
    ] {
        let mut reply = reply_for(Packet::parse(&query).unwrap());
        match mode {
            "nxdomain" => *reply.rcode_mut() = RCODE::NameError,
            "servfail" => *reply.rcode_mut() = RCODE::ServerFailure,
            "truncated" => reply.set_flags(PacketFlag::TRUNCATION),
            "wrong-id" => reply.set_id(1),
            _ => {}
        }
        if mode != "empty" {
            reply.answers.push(ResourceRecord::new(
                if mode == "unrelated" {
                    Name::new("unrelated.test").unwrap()
                } else {
                    reply.questions[0].qname.clone()
                },
                if mode == "wrong-class" {
                    CLASS::CH
                } else {
                    CLASS::IN
                },
                60,
                RData::PTR(PTR(Name::new("host.test").unwrap())),
            ));
        }
        let service = StaticDnsReply(reply.build_bytes_vec().unwrap());
        assert!(
            service
                .reverse_lookup("192.0.2.7".parse().unwrap())
                .await
                .is_err(),
            "{mode}"
        );
    }
    assert!(
        StaticDnsReply(vec![0, 1, 2])
            .reverse_lookup("192.0.2.7".parse().unwrap())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn reverse_lookup_rejects_cname_loops() {
    use simple_dns::rdata::CNAME;
    let query = query("7.2.0.192.in-addr.arpa", TYPE::PTR, 0);
    let mut reply = reply_for(Packet::parse(&query).unwrap());
    let name = reply.questions[0].qname.clone();
    reply.answers.push(ResourceRecord::new(
        name.clone(),
        CLASS::IN,
        60,
        RData::CNAME(CNAME(name)),
    ));
    let service = StaticDnsReply(reply.build_bytes_vec().unwrap());
    let error = service
        .reverse_lookup("192.0.2.7".parse().unwrap())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("CNAME loop"));
}

#[test]
fn reverse_lookup_cache_searches_ptr_answers_and_preserves_forward_fallback() {
    use simple_dns::rdata::PTR;
    let cache = DnsCache::default();
    let ip = Ipv4Addr::LOCALHOST.into();
    let forward_query = query("forward.test", TYPE::A, 1);
    let forward_reply = response(&forward_query, 60);
    cache.insert(&forward_query, Packet::parse(&forward_reply).unwrap());
    cache.age_for_test(Duration::from_secs(1));

    let ptr_query = query("1.0.0.127.IN-ADDR.ARPA", TYPE::PTR, 2);
    let mut ptr_reply = reply_for(Packet::parse(&ptr_query).unwrap());
    for hostname in ["reverse.test", "other.test"] {
        ptr_reply.answers.push(ResourceRecord::new(
            ptr_reply.questions[0].qname.clone(),
            CLASS::IN,
            2,
            RData::PTR(PTR(Name::new(hostname).unwrap())),
        ));
    }
    cache.insert(&ptr_query, ptr_reply.clone());
    assert_eq!(
        cache.reverse_lookup_cache(ip).as_deref(),
        Some("reverse.test")
    );
    assert!(
        cache
            .reverse_lookup_cache("127.0.0.2".parse().unwrap())
            .is_none()
    );

    cache.age_for_test(Duration::from_secs(3));
    assert_eq!(
        cache.reverse_lookup_cache(ip).as_deref(),
        Some("forward.test")
    );
    cache.insert(&ptr_query, ptr_reply);
    cache.age_for_test(Duration::from_secs(1));
    cache.insert(&forward_query, Packet::parse(&forward_reply).unwrap());
    assert_eq!(
        cache.reverse_lookup_cache(ip).as_deref(),
        Some("forward.test")
    );
    cache.age_for_test(Duration::from_secs(61));
    assert!(cache.reverse_lookup_cache(ip).is_none());
}

#[test]
fn reverse_lookup_cache_ignores_unrelated_ptr_records_and_cname_loops() {
    use simple_dns::rdata::{CNAME, PTR};
    for mode in [
        "unrelated",
        "wrong-class",
        "wrong-question-type",
        "cname-loop",
    ] {
        let cache = DnsCache::default();
        let query = query(
            "7.2.0.192.in-addr.arpa",
            if mode == "wrong-question-type" {
                TYPE::TXT
            } else {
                TYPE::PTR
            },
            0,
        );
        let mut reply = reply_for(Packet::parse(&query).unwrap());
        let owner = reply.questions[0].qname.clone();
        if mode == "cname-loop" {
            reply.answers.push(ResourceRecord::new(
                owner.clone(),
                CLASS::IN,
                60,
                RData::CNAME(CNAME(owner.clone())),
            ));
        }
        reply.answers.push(ResourceRecord::new(
            if mode == "unrelated" {
                Name::new("unrelated.test").unwrap()
            } else {
                owner
            },
            if mode == "wrong-class" {
                CLASS::CH
            } else {
                CLASS::IN
            },
            60,
            RData::PTR(PTR(Name::new("host.test").unwrap())),
        ));
        cache.insert(&query, reply);
        assert!(
            cache
                .reverse_lookup_cache("192.0.2.7".parse().unwrap())
                .is_none(),
            "{mode}"
        );
    }
}

#[tokio::test]
async fn different_resolvers_share_cached_responses_and_restore_transaction_ids() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let first = udp_cfg(upstream.local_addr().unwrap())
        .build()
        .await
        .unwrap();
    let resolver = first.resolver.clone();
    let (stop, task) = run(first);
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 2000];
        let (size, peer) = upstream.recv_from(&mut buffer).await.unwrap();
        upstream
            .send_to(&response(&buffer[..size], 60), peer)
            .await
            .unwrap();
    });
    let first_query = query("shared-resolver-cache.test", TYPE::A, 123);
    let bytes = resolver.exchange(&first_query).await.unwrap();
    assert_eq!(Packet::parse(&bytes).unwrap().id(), 123);
    mock.await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();

    let unused_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second = super::config::DnsTcpServerCfg {
        tag: "second-resolver".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        upstream: unused_upstream.local_addr().unwrap(),
    }
    .build()
    .await
    .unwrap();
    // No manager services this resolver's upstream queue: only a shared cache
    // hit can complete. The different transport must not affect cache lookup.
    let second_query = query("shared-resolver-cache.test", TYPE::A, 456);
    let bytes = tokio::time::timeout(Duration::from_secs(1), second.exchange(&second_query))
        .await
        .unwrap()
        .unwrap();
    let reply = Packet::parse(&bytes).unwrap();
    assert_eq!(reply.id(), 456);
    assert_eq!(
        cache::addresses(&reply),
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), unused_upstream.accept())
            .await
            .is_err()
    );
}
