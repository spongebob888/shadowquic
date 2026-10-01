#![cfg(feature = "dns-server")]

use std::{error::Error, net::SocketAddr, sync::Arc, time::Duration};

use shadowquic::{
    Manager,
    config::{DirectOutCfg, DnsTlsServerCfg},
    direct::outbound::DirectOut,
};
use simple_dns::{CLASS, Name, Packet, PacketFlag, Question, RCODE, TYPE, rdata::RData};
use tokio::{net::UdpSocket, sync::oneshot, time::timeout};

/// Public endpoint: https://www.alidns.com/ (223.5.5.5, TLS identity dns.alidns.com).
/// Run with:
/// cargo test --release -p shadowquic --features dns-server --test dns_tls_alidns -- --ignored --nocapture
#[tokio::test]
#[ignore = "requires Internet access to AliDNS over TCP port 853"]
async fn resolves_through_alidns_over_tls() -> Result<(), Box<dyn Error>> {
    let server = DnsTlsServerCfg {
        tag: "alidns".into(),
        bind_addr: "127.0.0.1:0".parse()?,
        upstream: "223.5.5.5:853".parse()?,
        server_name: "dns.alidns.com".into(),
    }
    .build()
    .await?;
    let local_addr = server.local_addr;
    let manager = Manager::single(
        Box::new(server),
        Arc::new(DirectOut::new(DirectOutCfg::default())),
    );
    assert_alidns_query(manager, local_addr, "www.aliyun.com").await
}

/// Verify that the UDP resolver's upstream traffic is intercepted by the
/// automatically registered TLS outbound, whose TCP traffic goes to direct.
#[cfg(feature = "plugin")]
#[tokio::test]
#[ignore = "requires Internet access to AliDNS over TCP port 853"]
async fn router_hijacks_dns_udp_to_dns_tls_then_direct() -> Result<(), Box<dyn Error>> {
    // A local upstream that never answers makes accidental direct UDP routing
    // fail, and lets us verify that hijacking sends no packets to that upstream.
    let unused_upstream = UdpSocket::bind("127.0.0.1:0").await?;
    let upstream_addr = unused_upstream.local_addr()?;
    // Reserve an ephemeral listener port until immediately before construction;
    // Config::build_manager owns the listener and does not expose its address.
    let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
    let local_addr = reservation.local_addr()?;
    let config: shadowquic::config::Config = serde_saphyr::from_str(&format!(
        r#"
inbounds:
  - tag: dns-udp
    type: dns-udp
    bind-addr: "{local_addr}"
    upstream: "{upstream_addr}"
  - tag: dns-tls
    type: dns-tls
    bind-addr: "127.0.0.1:0"
    upstream: "223.5.5.5:853"
    server-name: dns.alidns.com
outbounds:
  - tag: unused-default
    type: drop
  - tag: direct
    type: direct
router:
  src: |
    return function(ctx)
      if ctx.inbound_tag == "dns-udp" then
        assert(ctx.network_type == "udp")
        assert(ctx.dst_ip_v4 == "127.0.0.1")
        assert(ctx.dst_port == {upstream_port})
        return "dns-tls"
      end
      if ctx.inbound_tag == "dns-tls" then
        assert(ctx.network_type == "tcp")
        assert(ctx.dst_ip_v4 == "223.5.5.5")
        assert(ctx.dst_port == 853)
        return "direct"
      end
      error("unexpected inbound: " .. ctx.inbound_tag)
    end
"#,
        upstream_port = upstream_addr.port()
    ))?;
    drop(reservation);
    let manager = config.build_manager().await?;
    assert!(manager.inbounds.contains_key("dns-udp"));
    assert!(manager.inbounds.contains_key("dns-tls"));
    assert!(manager.outbounds.contains_key("dns-tls"));
    assert_eq!(manager.default_outbound, "unused-default");

    // A distinct domain ensures the other test cannot satisfy this query from cache.
    assert_alidns_query(manager, local_addr, "dns.alidns.com").await?;
    let error = unused_upstream
        .try_recv(&mut [0; 2000])
        .expect_err("DNS UDP upstream received a packet instead of being hijacked");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    Ok(())
}

async fn assert_alidns_query(
    manager: Manager,
    local_addr: SocketAddr,
    hostname: &str,
) -> Result<(), Box<dyn Error>> {
    let (stop, stopped) = oneshot::channel();
    let manager_task = tokio::spawn(manager.run_until(async {
        let _ = stopped.await;
    }));

    let domain = Name::new(hostname)?;
    let mut query = Packet::new_query(0xa11d);
    query.set_flags(PacketFlag::RECURSION_DESIRED);
    query.questions.push(Question::new(
        domain.clone(),
        TYPE::A.into(),
        CLASS::IN.into(),
        false,
    ));
    let query = query.build_bytes_vec()?;

    // Exercise the local listener, manager routing, direct outbound, TLS
    // handshake with certificate verification, and DNS response framing.
    let exchange = timeout(Duration::from_secs(8), async {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        socket.connect(local_addr).await?;
        socket.send(&query).await?;
        let mut buffer = [0; 2000];
        let size = socket.recv(&mut buffer).await?;
        Ok::<_, std::io::Error>(buffer[..size].to_vec())
    })
    .await;

    // Shut down the listener even if the public endpoint is unavailable.
    let _ = stop.send(());
    timeout(Duration::from_secs(2), manager_task).await???;
    let bytes = exchange??;
    let reply = Packet::parse(&bytes)?;
    assert_eq!(reply.id(), 0xa11d);
    assert!(reply.has_flags(PacketFlag::RESPONSE));
    assert!(!reply.has_flags(PacketFlag::TRUNCATION));
    assert_eq!(
        reply.rcode(),
        RCODE::NoError,
        "AliDNS query failed: {reply:?}"
    );
    assert_eq!(reply.questions.len(), 1);
    assert_eq!(reply.questions[0].qname, domain);
    assert_eq!(reply.questions[0].qtype, TYPE::A.into());
    assert_eq!(reply.questions[0].qclass, CLASS::IN.into());
    assert!(
        reply
            .answers
            .iter()
            .any(|record| matches!(record.rdata, RData::A(_))),
        "AliDNS returned no IPv4 answers for {hostname}: {reply:?}"
    );
    Ok(())
}
