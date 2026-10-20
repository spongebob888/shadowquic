#![cfg(feature = "dns-server")]

use std::{error::Error, sync::Arc, time::Duration};

use shadowquic::{Manager, config::DnsQuicServerCfg, direct::outbound::DirectOut};
use simple_dns::{CLASS, Name, Packet, PacketFlag, Question, RCODE, TYPE, rdata::RData};
use tokio::{net::UdpSocket, sync::oneshot, time::timeout};

/// cargo test --release -p shadowquic --test dns_quic_alidns -- --ignored --nocapture
#[tokio::test]
#[ignore = "requires Internet access to AliDNS over UDP port 853"]
async fn resolves_through_alidns_over_quic() -> Result<(), Box<dyn Error>> {
    assert_alidns_queries(true).await
}

#[tokio::test]
#[ignore = "requires Internet access to AliDNS over UDP port 853"]
async fn resolves_through_alidns_over_quic_with_zero_rtt_disabled() -> Result<(), Box<dyn Error>> {
    assert_alidns_queries(false).await
}

async fn assert_alidns_queries(zero_rtt: bool) -> Result<(), Box<dyn Error>> {
    let server = DnsQuicServerCfg {
        tag: "alidns".into(),
        bind_addr: "127.0.0.1:0".parse()?,
        upstream: "223.5.5.5:853".parse()?,
        server_name: "dns.alidns.com".into(),
        zero_rtt,
        // Every exchange must reach DoQ, including repeated questions.
        bypass_cache: true,
    }
    .build()
    .await?;
    let local_addr = server.local_addr;
    let resolver = server.resolver.clone();
    let mut manager = Manager::single(Box::new(server), Arc::new(DirectOut::default()));
    let inbound = manager.inbounds.remove("inbound").unwrap();
    manager.inbounds.insert("alidns".into(), inbound);
    let (stop, stopped) = oneshot::channel();
    let manager_task = tokio::spawn(manager.run_until(async {
        let _ = stopped.await;
    }));

    let exchange = timeout(Duration::from_secs(12), async {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        socket.connect(local_addr).await?;
        // Repeat over the local listener, then issue concurrent exchanges through
        // the resolver to exercise both connection reuse and multiplexing.
        for id in [0xa11d, 0xa11e] {
            let query = query(id)?;
            socket.send(&query).await?;
            let mut buffer = [0; 2000];
            let size = socket.recv(&mut buffer).await?;
            assert_reply(&buffer[..size], id)?;
        }
        use shadowquic::dns::DnsService;
        let q1 = query(0xa11f)?;
        let q2 = query(0xa120)?;
        let (r1, r2) = tokio::join!(resolver.exchange(&q1), resolver.exchange(&q2));
        assert_reply(&r1?, 0xa11f)?;
        assert_reply(&r2?, 0xa120)?;
        Ok::<_, Box<dyn Error>>(())
    })
    .await;

    let _ = stop.send(());
    timeout(Duration::from_secs(2), manager_task).await???;
    exchange??;
    Ok(())
}

fn query(id: u16) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut query = Packet::new_query(id);
    query.set_flags(PacketFlag::RECURSION_DESIRED);
    query.questions.push(Question::new(
        Name::new("www.aliyun.com")?,
        TYPE::A.into(),
        CLASS::IN.into(),
        false,
    ));
    Ok(query.build_bytes_vec()?)
}

fn assert_reply(bytes: &[u8], id: u16) -> Result<(), Box<dyn Error>> {
    let reply = Packet::parse(bytes)?;
    assert_eq!(reply.id(), id);
    assert!(reply.has_flags(PacketFlag::RESPONSE));
    assert!(!reply.has_flags(PacketFlag::TRUNCATION));
    assert_eq!(
        reply.rcode(),
        RCODE::NoError,
        "AliDNS query failed: {reply:?}"
    );
    assert_eq!(reply.questions.len(), 1);
    assert_eq!(reply.questions[0].qname, Name::new("www.aliyun.com")?);
    assert_eq!(reply.questions[0].qtype, TYPE::A.into());
    assert!(
        reply
            .answers
            .iter()
            .any(|record| matches!(record.rdata, RData::A(_))),
        "AliDNS returned no IPv4 answers: {reply:?}"
    );
    Ok(())
}
