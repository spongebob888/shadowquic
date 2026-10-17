use super::{
    TunServer, TunServerCfg,
    packet::{Packet, udp_packet},
    stack,
};
use crate::{Inbound, ProxyRequest, error::SError};
use bytes::Bytes;
use smoltcp::{
    phy::ChecksumCapabilities,
    wire::{
        IpAddress, IpProtocol, IpRepr, TcpControl, TcpPacket, TcpRepr, TcpSeqNumber, UdpPacket,
        UdpRepr,
    },
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
};

fn config() -> TunServerCfg {
    serde_saphyr::from_str("tag: tun-in\ndefault-outbound: direct").unwrap()
}

#[cfg(feature = "dns-server")]
struct PacketInbound(mpsc::Receiver<ProxyRequest>);
#[cfg(feature = "dns-server")]
#[async_trait::async_trait]
impl Inbound for PacketInbound {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        self.0.recv().await.ok_or(SError::InboundUnavailable)
    }
}
struct Harness {
    observer: Arc<crate::observe::Observer>,
    input: mpsc::Sender<Bytes>,
    output: mpsc::Receiver<Vec<u8>>,
    inbound: Box<dyn Inbound>,
    task: JoinHandle<Result<(), SError>>,
}
impl Harness {
    fn new(cfg: TunServerCfg) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let (output, output_rx) = mpsc::channel(256);
        let (requests, request_rx) = mpsc::channel(256);
        let observer = Arc::new(crate::observe::Observer::new());
        Self {
            input: tx,
            output: output_rx,
            inbound: Box::new(TunServer {
                name: "test-tun".into(),
                observer: observer.clone(),
                requests: request_rx,
                task: Mutex::new(None),
            }),
            observer,
            task: tokio::spawn(stack::run(cfg, rx, output, requests)),
        }
    }
    async fn receive(&mut self) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(2), self.output.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn accept(&mut self) -> ProxyRequest {
        let request = tokio::time::timeout(Duration::from_secs(2), self.inbound.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(
            &request.user_context().observer.upgrade().unwrap(),
            &self.observer
        ));
        let stats = request.user_context().stats.as_ref().unwrap();
        assert_eq!(stats.username, "");
        assert!(stats.conn_handle.upgrade().is_none());
        self.observer.wrap_request(request, "direct".into()).await
    }
    #[cfg(feature = "dns-server")]
    fn restore(&mut self, fake: std::sync::Arc<crate::dns::FakeIp>) {
        let (_, empty) = mpsc::channel(1);
        self.inbound = Box::new(crate::dns::RestoringInbound {
            inner: std::mem::replace(&mut self.inbound, Box::new(PacketInbound(empty))),
            fake,
        });
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn tcp_packet(
    src: SocketAddr,
    dst: SocketAddr,
    control: TcpControl,
    seq: i32,
    ack: Option<i32>,
    data: &[u8],
) -> Vec<u8> {
    let src_ip = IpAddress::from(src.ip());
    let dst_ip = IpAddress::from(dst.ip());
    let tcp = TcpRepr {
        src_port: src.port(),
        dst_port: dst.port(),
        control,
        seq_number: TcpSeqNumber(seq),
        ack_number: ack.map(TcpSeqNumber),
        window_len: 65535,
        window_scale: None,
        max_seg_size: (control == TcpControl::Syn).then_some(1200),
        sack_permitted: false,
        sack_ranges: [None; 3],
        timestamp: None,
        payload: data,
    };
    let ip = IpRepr::new(
        src_ip,
        dst_ip,
        IpProtocol::Tcp,
        tcp.header_len() + data.len(),
        64,
    );
    let mut bytes = vec![0; ip.header_len() + tcp.header_len() + data.len()];
    ip.emit(
        &mut bytes[..ip.header_len()],
        &ChecksumCapabilities::default(),
    );
    tcp.emit(
        &mut TcpPacket::new_unchecked(&mut bytes[ip.header_len()..]),
        &src_ip,
        &dst_ip,
        &ChecksumCapabilities::default(),
    );
    bytes
}

async fn connect(harness: &mut Harness, src: SocketAddr, dst: SocketAddr) -> i32 {
    harness
        .input
        .send(tcp_packet(src, dst, TcpControl::Syn, 100, None, &[]).into())
        .await
        .unwrap();
    let syn_ack = harness.receive().await;
    let ip = Packet::parse(&syn_ack).unwrap();
    assert_eq!(std::net::IpAddr::from(ip.src), dst.ip());
    let tcp = TcpPacket::new_checked(ip.payload).unwrap();
    let repr = TcpRepr::parse(&tcp, &ip.src, &ip.dst, &ChecksumCapabilities::default()).unwrap();
    assert_eq!(repr.control, TcpControl::Syn);
    assert_eq!(repr.ack_number, Some(TcpSeqNumber(101)));
    let ack = (repr.seq_number + 1).0;
    harness
        .input
        .send(tcp_packet(src, dst, TcpControl::None, 101, Some(ack), &[]).into())
        .await
        .unwrap();
    ack
}

#[tokio::test]
async fn tcp_ipv4_and_ipv6_transfer_and_preserve_half_closes() {
    for (src, dst) in [
        ("192.0.2.10:45000", "198.51.100.20:443"),
        ("[2001:db8::10]:45000", "[2001:db8:1::20]:443"),
    ] {
        let src: SocketAddr = src.parse().unwrap();
        let dst: SocketAddr = dst.parse().unwrap();
        let mut harness = Harness::new(config());
        let ack = connect(&mut harness, src, dst).await;
        let req = harness.accept().await;
        assert_eq!(req.user_context().inbound_tag, "tun-in");
        assert_eq!(
            req.user_context().preferred_outbound.as_deref(),
            Some("direct")
        );
        let ProxyRequest::Tcp(mut session) = req else {
            panic!()
        };
        assert_eq!(session.dst, dst.into());
        assert_eq!(session.src_addr, Some(src));
        harness
            .input
            .send(tcp_packet(src, dst, TcpControl::Psh, 101, Some(ack), b"hello").into())
            .await
            .unwrap();
        let mut bytes = [0; 5];
        tokio::time::timeout(
            Duration::from_secs(2),
            session.stream.read_exact(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&bytes, b"hello");
        harness
            .input
            .send(tcp_packet(src, dst, TcpControl::Fin, 106, Some(ack), &[]).into())
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), session.stream.read(&mut bytes))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        // Receiving FIN must not prevent the outbound's response.
        session.stream.write_all(b"world").await.unwrap();
        session.stream.shutdown().await.unwrap();
        #[cfg(feature = "statistics")]
        {
            assert_eq!(harness.observer.get_user_stats("").await.tcp_conns, 1);
            assert_eq!(harness.observer.get_user_stats("").await.tcp_recv, 5);
            assert_eq!(harness.observer.get_user_stats("").await.tcp_sent, 5);
        }
        let mut got_data = false;
        let mut got_fin = false;
        while !got_data || !got_fin {
            let bytes = harness.receive().await;
            let ip = Packet::parse(&bytes).unwrap();
            let tcp = TcpPacket::new_checked(ip.payload).unwrap();
            let repr =
                TcpRepr::parse(&tcp, &ip.src, &ip.dst, &ChecksumCapabilities::default()).unwrap();
            if !repr.payload.is_empty() {
                assert_eq!(repr.payload, b"world");
                got_data = true;
            }
            got_fin |= repr.control == TcpControl::Fin;
            let n = repr.payload.len() + usize::from(repr.control == TcpControl::Fin);
            if n > 0 {
                harness
                    .input
                    .send(
                        tcp_packet(
                            src,
                            dst,
                            TcpControl::None,
                            107,
                            Some((repr.seq_number + n).0),
                            &[],
                        )
                        .into(),
                    )
                    .await
                    .unwrap();
            }
        }
        drop(session);
        #[cfg(feature = "statistics")]
        assert_eq!(harness.observer.get_user_stats("").await.tcp_conns, 0);
    }
}

#[tokio::test]
async fn udp_keeps_each_destination_and_original_reply_source_for_both_families() {
    for (client, destinations) in [
        ("192.0.2.10:50000", ["198.51.100.1:53", "198.51.100.2:5353"]),
        (
            "[2001:db8::10]:50000",
            ["[2001:db8:1::1]:53", "[2001:db8:1::2]:5353"],
        ),
    ] {
        let mut harness = Harness::new(config());
        let client: SocketAddr = client.parse().unwrap();
        let first: SocketAddr = destinations[0].parse().unwrap();
        harness
            .input
            .send(udp_packet(client, first, b"first", 1500).unwrap().into())
            .await
            .unwrap();
        let req = harness.accept().await;
        assert_eq!(req.user_context().inbound_tag, "tun-in");
        assert_eq!(
            req.user_context().preferred_outbound.as_deref(),
            Some("direct")
        );
        let ProxyRequest::Udp(mut session) = req else {
            panic!()
        };
        assert_eq!(session.src_addr, Some(client));
        let bind: SocketAddr = if client.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        }
        .parse()
        .unwrap();
        assert_eq!(session.bind_addr, bind.into());
        for (i, destination) in destinations.into_iter().enumerate() {
            let destination: SocketAddr = destination.parse().unwrap();
            if i != 0 {
                harness
                    .input
                    .send(
                        udp_packet(client, destination, b"second", 1500)
                            .unwrap()
                            .into(),
                    )
                    .await
                    .unwrap();
            }
            let (payload, dst) =
                tokio::time::timeout(Duration::from_secs(2), session.recv.recv_from())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(dst, destination.into());
            session.send.send_to(payload.clone(), dst).await.unwrap();
            let packet = harness.receive().await;
            let ip = Packet::parse(&packet).unwrap();
            assert_eq!(std::net::IpAddr::from(ip.src), destination.ip());
            assert_eq!(std::net::IpAddr::from(ip.dst), client.ip());
            let udp = UdpPacket::new_checked(ip.payload).unwrap();
            let repr =
                UdpRepr::parse(&udp, &ip.src, &ip.dst, &ChecksumCapabilities::default()).unwrap();
            assert_eq!(repr.src_port, destination.port());
            assert_eq!(repr.dst_port, client.port());
            assert_eq!(udp.payload(), payload.as_ref());
        }
        #[cfg(feature = "statistics")]
        {
            assert_eq!(harness.observer.get_user_stats("").await.udp_conns, 1);
            assert_eq!(harness.observer.get_user_stats("").await.udp_recv, 11);
            assert_eq!(harness.observer.get_user_stats("").await.udp_sent, 11);
            let entry = harness.observer.udp_scoreboard.lock().await[0]
                .0
                .upgrade()
                .unwrap();
            assert_eq!(entry.dst.read().unwrap().to_string(), first.to_string());
            assert_eq!(entry.outbound, "direct");
        }
        drop(session);
        #[cfg(feature = "statistics")]
        assert_eq!(harness.observer.get_user_stats("").await.udp_conns, 0);
    }
}

#[cfg(feature = "dns-server")]
#[tokio::test]
async fn fakeip_restores_tcp_before_routing_and_each_udp_packet_and_reply() {
    use crate::{dns::FakeIp, msgs::socks5::SocksAddr};
    for ipv6 in [false, true] {
        let fake = std::sync::Arc::new(
            FakeIp::new(
                "198.18.16.0/20".parse().unwrap(),
                "2001:2:1::/64".parse().unwrap(),
            )
            .unwrap(),
        );
        let ip = fake.allocate("first.test", ipv6).unwrap();
        let ip2 = fake.allocate("second.test", ipv6).unwrap();
        let client: SocketAddr = if ipv6 {
            "[2001:db8::10]:40000"
        } else {
            "192.0.2.10:40000"
        }
        .parse()
        .unwrap();
        let mut harness = Harness::new(config());
        harness.restore(fake);
        connect(&mut harness, client, SocketAddr::new(ip, 443)).await;
        let req = harness.accept().await;
        assert_eq!(req.dst(), &SocksAddr::from_domain("first.test".into(), 443));
        #[cfg(feature = "plugin")]
        {
            let context = crate::plugin::router::RouteContext::from_request(&req);
            assert_eq!(context.dst_domain.as_deref(), Some("first.test"));
        }
        drop(req);
        harness
            .input
            .send(
                udp_packet(client, SocketAddr::new(ip, 53), b"one", 1500)
                    .unwrap()
                    .into(),
            )
            .await
            .unwrap();
        let ProxyRequest::Udp(mut session) = harness.accept().await else {
            panic!()
        };
        assert_eq!(session.dst, SocksAddr::from_domain("first.test".into(), 53));
        for (i, (ip, name)) in [(ip, "first.test"), (ip2, "second.test")]
            .into_iter()
            .enumerate()
        {
            if i != 0 {
                harness
                    .input
                    .send(
                        udp_packet(client, SocketAddr::new(ip, 53), b"two", 1500)
                            .unwrap()
                            .into(),
                    )
                    .await
                    .unwrap();
            }
            let (bytes, destination) = session.recv.recv_from().await.unwrap();
            assert_eq!(destination, SocksAddr::from_domain(name.into(), 53));
            session
                .send
                .send_to(bytes.clone(), destination)
                .await
                .unwrap();
            // Dropping the TCP request above can also produce TCP FIN packets.
            loop {
                let reply = harness.receive().await;
                let packet = Packet::parse(&reply).unwrap();
                if packet.protocol != IpProtocol::Udp {
                    continue;
                }
                assert_eq!(std::net::IpAddr::from(packet.src), ip);
                assert_eq!(
                    UdpPacket::new_checked(packet.payload).unwrap().payload(),
                    bytes.as_ref()
                );
                break;
            }
        }
    }
}

#[test]
fn tun_configuration_is_validated_without_opening_a_device() {
    use crate::config::{Config, InboundCfg};
    let cfg: Config = serde_saphyr::from_str("inbounds: [{tag: tun-in, type: tun, default-outbound: direct}]\noutbounds: [{tag: direct, type: direct}]").unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.inbounds[0].tag(), "tun-in");
    assert_eq!(cfg.inbounds[0].default_outbound(), Some("direct"));
    assert!(matches!(cfg.inbounds[0], InboundCfg::Tun(_)));
    for fields in [
        "mtu: 1279",
        "max-sessions: 0",
        "idle-timeout: 0",
        "ipv4: 0.0.0.0/30",
        "ipv6: 'ff00::1/64'",
        "name: ''",
    ] {
        let cfg: TunServerCfg = serde_saphyr::from_str(&format!("tag: tun\n{fields}")).unwrap();
        assert!(cfg.validate().is_err(), "{fields}");
    }
    assert!(serde_saphyr::from_str::<TunServerCfg>("tag: tun\nunknown: true").is_err());
}

#[test]
fn invalid_or_fragmented_packets_and_oversize_replies_are_rejected() {
    assert!(Packet::parse(&[0x45]).is_none());
    let src = "192.0.2.1:1234".parse().unwrap();
    let dst = "198.51.100.1:53".parse().unwrap();
    let mut bytes = udp_packet(src, dst, b"payload", 1500).unwrap();
    bytes[6] |= 0x20;
    let mut packet = smoltcp::wire::Ipv4Packet::new_unchecked(&mut bytes[..]);
    packet.fill_checksum();
    assert!(Packet::parse(&bytes).is_none());
    assert!(udp_packet(src, dst, &[0; 1500], 1500).is_err());
    assert!(udp_packet(src, "[2001:db8::1]:53".parse().unwrap(), b"data", 1500).is_err());
}

#[tokio::test]
async fn concurrent_tcp_clients_can_use_the_same_destination() {
    let destination: SocketAddr = "198.51.100.1:443".parse().unwrap();
    let mut harness = Harness::new(config());
    let mut sessions = Vec::new();
    for port in [40000, 40001] {
        let client = SocketAddr::new("192.0.2.1".parse().unwrap(), port);
        let ack = connect(&mut harness, client, destination).await;
        let ProxyRequest::Tcp(session) = harness.accept().await else {
            panic!()
        };
        sessions.push((client, ack, session));
    }
    for (i, (client, ack, session)) in sessions.iter_mut().enumerate() {
        let payload = [i as u8; 32];
        harness
            .input
            .send(
                tcp_packet(
                    *client,
                    destination,
                    TcpControl::Psh,
                    101,
                    Some(*ack),
                    &payload,
                )
                .into(),
            )
            .await
            .unwrap();
        let mut received = [0; 32];
        tokio::time::timeout(
            Duration::from_secs(2),
            session.stream.read_exact(&mut received),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(received, payload);
    }
}

#[tokio::test(start_paused = true)]
async fn udp_idle_cleanup_releases_capacity_and_discards_late_replies() {
    let mut cfg = config();
    cfg.max_sessions = 1;
    cfg.idle_timeout = 2;
    let mut harness = Harness::new(cfg);
    let destination = "198.51.100.1:53".parse().unwrap();
    let first = "192.0.2.1:40000".parse().unwrap();
    let second = "192.0.2.2:40000".parse().unwrap();
    harness
        .input
        .send(
            udp_packet(first, destination, b"first", 1500)
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    let ProxyRequest::Udp(mut old) = harness.accept().await else {
        panic!()
    };
    old.recv.recv_from().await.unwrap();
    harness
        .input
        .send(
            udp_packet(second, destination, b"blocked", 1500)
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    tokio::task::yield_now().await;
    // Expire the first client's session without a wall-clock sleep.
    tokio::time::advance(Duration::from_secs(3)).await;
    tokio::task::yield_now().await;
    harness
        .input
        .send(
            udp_packet(second, destination, b"second", 1500)
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    let ProxyRequest::Udp(mut new) = harness.accept().await else {
        panic!()
    };
    assert_eq!(new.src_addr, Some(second));
    assert_eq!(new.recv.recv_from().await.unwrap().0.as_ref(), b"second");
    old.send
        .send_to(Bytes::from_static(b"late"), destination.into())
        .await
        .unwrap();
    new.send
        .send_to(Bytes::from_static(b"current"), destination.into())
        .await
        .unwrap();
    let bytes = harness.receive().await;
    let packet = Packet::parse(&bytes).unwrap();
    assert_eq!(std::net::IpAddr::from(packet.dst), second.ip());
    assert_eq!(
        UdpPacket::new_checked(packet.payload).unwrap().payload(),
        b"current"
    );
}

#[cfg(feature = "dns-server")]
#[tokio::test]
async fn unmapped_fake_destinations_are_rejected_before_routing() {
    let mut harness = Harness::new(config());
    harness.restore(std::sync::Arc::new(crate::dns::FakeIp::default()));
    let client = "192.0.2.10:40000".parse().unwrap();
    let unknown = "198.18.0.15:443".parse().unwrap();
    connect(&mut harness, client, unknown).await;
    let result = tokio::time::timeout(Duration::from_secs(2), harness.inbound.accept())
        .await
        .unwrap();
    assert!(matches!(result, Err(SError::DnsError(_))));
    harness
        .input
        .send(
            udp_packet(client, unknown, b"unknown", 1500)
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), harness.inbound.accept())
        .await
        .unwrap();
    assert!(matches!(result, Err(SError::DnsError(_))));
}

#[cfg(target_os = "android")]
#[tokio::test]
async fn android_tun_requires_a_descriptor_from_the_embedding_application() {
    let cfg = config();
    assert!(cfg.android_fd.is_none());
    let error = super::TunServer::new(cfg).await.err().unwrap();
    assert!(error.to_string().contains("VpnService descriptor"));
    assert!(serde_saphyr::from_str::<TunServerCfg>("tag: tun\nandroid-fd: 123\n").is_err());
}

#[cfg(target_os = "android")]
#[tokio::test]
async fn android_tun_preserves_the_callers_descriptor_on_shutdown() {
    use std::{os::fd::OwnedFd, os::unix::net::UnixDatagram, sync::Arc};

    let (application, device) = UnixDatagram::pair().unwrap();
    let fd = Arc::new(OwnedFd::from(device));
    let mut cfg = config();
    cfg.android_fd = Some(fd.clone());
    let mut server = super::TunServer::new(cfg).await.unwrap();
    let src: SocketAddr = "172.19.0.2:45000".parse().unwrap();
    let dst: SocketAddr = "203.0.113.1:443".parse().unwrap();
    application
        .send(&udp_packet(src, dst, b"android", 1500).unwrap())
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(2), server.accept())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(&request, ProxyRequest::Udp(_)));
    assert_eq!(*request.dst(), dst.into());
    assert_eq!(*request.src_addr(), Some(src));
    server.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    drop(server);
    assert_eq!(Arc::strong_count(&fd), 1);
    assert!(fd.try_clone().is_ok());
}

#[cfg(not(target_os = "android"))]
#[tokio::test]
#[ignore = "requires permission to create a kernel TUN adapter (Linux CAP_NET_ADMIN/root; administrator on other platforms)"]
async fn kernel_tun_device_can_start_and_shutdown() {
    let server = super::TunServer::new(config()).await.unwrap();
    assert!(!server.name.is_empty());
    server.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}
