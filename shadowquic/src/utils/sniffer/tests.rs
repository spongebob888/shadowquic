use super::tls::{Parse, Parser};
use super::*;
use crate::config::SnifferRule;
use crate::{TcpSession, TcpTrait, UserContext};
use std::io;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

pub(crate) fn hello(name: &[u8]) -> Vec<u8> {
    let mut names = vec![0];
    names.extend_from_slice(&(name.len() as u16).to_be_bytes());
    names.extend_from_slice(name);
    let mut sni = (names.len() as u16).to_be_bytes().to_vec();
    sni.extend(names);
    let mut extensions = vec![0, 0];
    extensions.extend_from_slice(&(sni.len() as u16).to_be_bytes());
    extensions.extend(sni);
    // Include an unknown (GREASE) extension to exercise skipping.
    extensions.extend_from_slice(&[0x1a, 0x1a, 0, 3, 1, 2, 3]);
    let mut body = vec![3, 3];
    body.extend_from_slice(&[0; 32]);
    body.extend_from_slice(&[0, 0, 2, 0x13, 1, 1, 0]);
    body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
    body.extend(extensions);
    let len = body.len();
    let mut result = vec![1, (len >> 16) as u8, (len >> 8) as u8, len as u8];
    result.extend(body);
    result
}

pub(crate) fn records(handshake: &[u8], chunk: usize) -> Vec<u8> {
    let mut wire = Vec::new();
    for part in handshake.chunks(chunk) {
        wire.extend_from_slice(&[22, 3, 1]);
        wire.extend_from_slice(&(part.len() as u16).to_be_bytes());
        wire.extend_from_slice(part);
    }
    wire
}

fn found(wire: &[u8]) -> Option<String> {
    match Parser::default().parse(wire, 32768) {
        Parse::Done(name) => name.map(|r| r.domain),
        Parse::NeedMore => panic!("expected complete result"),
    }
}

#[test]
fn every_read_boundary_and_record_fragmentation_preserves_sni() {
    let handshake = hello(b"API.Example.com");
    for chunk in 1..=handshake.len() {
        let wire = records(&handshake, chunk);
        let mut parser = Parser::default();
        for end in 0..wire.len() {
            assert!(matches!(parser.parse(&wire[..end], 32768), Parse::NeedMore));
        }
        assert!(
            matches!(parser.parse(&wire, 32768), Parse::Done(Some(name)) if name.domain == "api.example.com")
        );
    }
    // Every possible two-record split, including inside the handshake header.
    for split in 1..handshake.len() {
        let mut wire = records(&handshake[..split], split);
        wire.extend(records(&handshake[split..], handshake.len()));
        assert_eq!(found(&wire).as_deref(), Some("api.example.com"));
    }
}

#[test]
fn malformed_lengths_names_and_non_tls_are_not_used_for_routing() {
    for name in [
        b"".as_slice(),
        b"a..example",
        b"-a.example",
        b"a-.example",
        b"a.example.",
        b"a\0.example",
        b"127.0.0.1",
        b"\xff.example",
    ] {
        assert_eq!(found(&records(&hello(name), 16384)), None);
    }
    for wire in [
        b"GET /".as_slice(),
        &[22, 4],
        &[22, 3, 3, 0, 0],
        &[22, 3, 3, 0x40, 1],
    ] {
        assert!(matches!(
            Parser::default().parse(wire, 32768),
            Parse::Done(None)
        ));
    }
    let mut handshake = hello(b"valid.example");
    handshake[0] = 2; // ServerHello
    assert_eq!(found(&records(&handshake, 16384)), None);
    handshake[0] = 1;
    handshake[1] = 0xff; // Unbounded handshake length
    assert_eq!(found(&records(&handshake, 16384)), None);
    let mut handshake = hello(b"valid.example");
    let n = handshake.len();
    handshake[n - 5] = 0xff; // Truncated extension after an otherwise valid SNI
    assert_eq!(found(&records(&handshake, 16384)), None);
    // Valid ClientHello without extensions.
    let mut handshake = hello(b"valid.example");
    handshake.truncate(4 + 41);
    handshake[3] = 41;
    assert_eq!(found(&records(&handshake, 16384)), None);
}

#[test]
fn duplicate_sni_and_nested_length_mismatches_are_rejected() {
    let mut handshake = hello(b"valid.example");
    let extensions = handshake[47..].to_vec();
    handshake.extend_from_slice(&extensions);
    let extensions_len = (extensions.len() * 2) as u16;
    handshake[45..47].copy_from_slice(&extensions_len.to_be_bytes());
    handshake[3] = (handshake.len() - 4) as u8;
    assert_eq!(found(&records(&handshake, 16384)), None);
    for offset in [45, 49, 51, 54] {
        let mut handshake = hello(b"valid.example");
        handshake[offset] = 0xff;
        assert_eq!(found(&records(&handshake, 16384)), None);
    }
}

#[test]
fn bounded_parser_handles_mutated_and_truncated_inputs_without_panicking() {
    let wire = records(&hello(b"valid.example"), 13);
    for index in 0..wire.len() {
        for value in [0, 1, 0x16, 0x80, 0xff] {
            let mut mutated = wire.clone();
            mutated[index] = value;
            let mut parser = Parser::default();
            for end in 0..=mutated.len() {
                if matches!(parser.parse(&mutated[..end], 32768), Parse::Done(_)) {
                    break;
                }
            }
        }
    }
}

struct Stream(DuplexStream);
impl TcpTrait for Stream {}
impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

fn request(port: u16) -> (DuplexStream, ProxyRequest) {
    let (client, server) = tokio::io::duplex(65536);
    (
        client,
        ProxyRequest::Tcp(TcpSession {
            stream: Box::new(Stream(server)),
            dst: std::net::SocketAddr::from(([192, 0, 2, 1], port)).into(),
            src_addr: None,
            user_context: UserContext::default(),
        }),
    )
}

async fn replay(request: ProxyRequest, expected: &[u8]) {
    let ProxyRequest::Tcp(mut session) = request else {
        panic!()
    };
    let mut bytes = vec![0; expected.len()];
    session.stream.read_exact(&mut bytes).await.unwrap();
    assert_eq!(bytes, expected);
}

fn config() -> SnifferCfg {
    SnifferCfg {
        tcp: vec![SnifferRule {
            ports: vec![80, 443],
            protocols: vec![SniffProtocol::Http, SniffProtocol::Tls],
        }],
        udp: vec![SnifferRule {
            ports: vec![],
            protocols: vec![SniffProtocol::Quic],
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn sniff_replays_bytes_and_exposes_read_only_lua_metadata() {
    let sniffer = Sniffer::new(config()).unwrap();
    for (wire, protocol, version) in [
        (
            b"GET / HTTP/1.1\r\nHost: Example.COM:8080\r\n\r\nbody".to_vec(),
            "http",
            "1.1",
        ),
        (records(&hello(b"Example.COM"), 11), "TLS", "1.2"),
    ] {
        let (mut client, req) = request(443);
        let dst = req.dst().clone();
        client.write_all(&wire).await.unwrap();
        let req = sniffer.sniff(req).await.unwrap();
        assert_eq!(req.dst(), &dst);
        let result = req.user_context().sniffer.as_ref().unwrap();
        assert_eq!(
            (&*result.domain, result.protocol, &*result.version),
            ("example.com", protocol, version)
        );
        let mut ctx = crate::plugin::router::RouteContext::from_request(&req);
        let router = crate::plugin::router::Router::from_source(
            r#"return function(ctx)
            assert(ctx.sniffer.domain == 'example.com')
            assert(not pcall(function() ctx.sniffer.domain = 'changed' end))
            return 'direct'
        end"#,
        )
        .unwrap();
        assert_eq!(router.route(&mut ctx).await.unwrap(), "direct");
        assert!(sniffer.memory.available_permits() < sniffer.config.max_buffered_bytes);
        replay(req, &wire).await;
        assert_eq!(
            sniffer.memory.available_permits(),
            sniffer.config.max_buffered_bytes
        );
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_and_budget_exhaustion_preserve_input() {
    let sniffer = Sniffer::new(config()).unwrap();
    let (mut client, req) = request(443);
    client.write_all(&[22, 3, 1]).await.unwrap();
    let req = sniffer.sniff(req).await.unwrap();
    assert!(req.user_context().sniffer.is_none());
    replay(req, &[22, 3, 1]).await;
    let _permit = sniffer
        .memory
        .clone()
        .acquire_many_owned(sniffer.config.max_buffered_bytes as u32)
        .await
        .unwrap();
    let (mut client, req) = request(443);
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .await
        .unwrap();
    let req = sniffer.sniff(req).await.unwrap();
    assert!(req.user_context().sniffer.is_none());
    replay(req, b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n").await;
}

#[test]
fn http_rejects_ambiguous_hosts_and_handles_fragmentation() {
    for wire in [
        b"GET / HTTP/1.1\r\nHost: a.example\r\nHost: b.example\r\n\r\n".as_slice(),
        b"GET / HTTP/1.1\r\nHost: example.com:bad\r\n\r\n",
        b"GET / HTTP/1.1\r\n Host: example.com\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        b"GET / HTTP/1.1\r\n\r\n",
    ] {
        assert!(matches!(http::parse(wire), Parse::Done(None)));
    }
    let wire = b"GET http://other.example/ HTTP/1.1\r\nhOsT: Example.COM\r\n\r\n";
    for n in 0..wire.len() {
        assert!(matches!(http::parse(&wire[..n]), Parse::NeedMore));
    }
    assert!(matches!(http::parse(wire), Parse::Done(Some(r)) if r.domain == "example.com"));
}

fn varint(value: usize) -> Vec<u8> {
    if value < 64 {
        vec![value as u8]
    } else {
        ((value as u16) | 0x4000).to_be_bytes().to_vec()
    }
}
fn initial(version: rustls_jls::quic::Version, frames: &[u8], pn: u8) -> Vec<u8> {
    let (wire_version, first) = match version {
        rustls_jls::quic::Version::V2 => (0x6b3343cfu32, 0xd0),
        _ => (1, 0xc0),
    };
    let dcid = b"12345678";
    let keys = quic::suite()
        .unwrap()
        .keys(dcid, rustls_jls::Side::Client, version)
        .local;
    let mut packet = vec![first];
    packet.extend(wire_version.to_be_bytes());
    packet.push(8);
    packet.extend(dcid);
    packet.extend([0, 0]);
    let mut payload = frames.to_vec();
    payload.resize(1200, 0);
    packet.extend(varint(1 + payload.len() + 16));
    let pn_offset = packet.len();
    packet.push(pn);
    let tag = keys
        .packet
        .encrypt_in_place(pn as u64, &packet, &mut payload)
        .unwrap();
    packet.extend(payload);
    packet.extend(tag.as_ref());
    let sample: [u8; 16] = packet[pn_offset + 4..pn_offset + 20].try_into().unwrap();
    let (first, rest) = packet.split_at_mut(1);
    keys.header
        .encrypt_in_place(&sample, &mut first[0], &mut rest[pn_offset - 1..pn_offset])
        .unwrap();
    packet
}
fn crypto_frame(offset: usize, data: &[u8]) -> Vec<u8> {
    let mut result = vec![6];
    result.extend(varint(offset));
    result.extend(varint(data.len()));
    result.extend(data);
    result
}

#[test]
fn quic_v1_v2_authentication_reassembly_and_first_datagram_limit() {
    let suite = quic::suite().unwrap();
    let hello = hello(b"Quic.Example");
    for v in [rustls_jls::quic::Version::V1, rustls_jls::quic::Version::V2] {
        let split = hello.len() / 2;
        let mut frames = crypto_frame(split, &hello[split..]);
        frames.extend(crypto_frame(0, &hello[..split]));
        let packet = initial(v, &frames, 0);
        let result = quic::parse(&packet, 32768, &suite).unwrap();
        assert_eq!(result.domain, "quic.example");
        assert_eq!(result.protocol, "QUIC");
        assert_eq!(
            result.version,
            if matches!(v, rustls_jls::quic::Version::V2) {
                "2"
            } else {
                "1"
            }
        );
        for end in 0..packet.len() {
            assert!(quic::parse(&packet[..end], 32768, &suite).is_none());
        }
        let mut bad = packet.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(quic::parse(&bad, 32768, &suite).is_none());
        let part = initial(v, &crypto_frame(0, &hello[..split]), 0);
        assert!(quic::parse(&part, 32768, &suite).is_none());
        let mut coalesced = part;
        coalesced.extend(initial(v, &crypto_frame(split, &hello[split..]), 1));
        assert!(quic::parse(&coalesced, 32768, &suite).is_some());
    }
    assert!(quic::parse(b"not QUIC", 32768, &suite).is_none());
}

#[tokio::test]
async fn complete_first_datagram_finishes_inspection_before_other_destinations() {
    let sniffer = Sniffer::new(config()).unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    let dst: crate::SocksAddr = "192.0.2.1:443"
        .parse::<std::net::SocketAddr>()
        .unwrap()
        .into();
    let other: crate::SocksAddr = "192.0.2.2:443"
        .parse::<std::net::SocketAddr>()
        .unwrap()
        .into();
    let packet = bytes::Bytes::from(initial(
        rustls_jls::quic::Version::V1,
        &crypto_frame(0, &hello(b"quic.example")),
        0,
    ));
    tx.send((packet.clone(), dst.clone())).await.unwrap();
    tx.send((bytes::Bytes::from_static(b"second"), other.clone()))
        .await
        .unwrap();
    let req: ProxyRequest = ProxyRequest::Udp(crate::UdpSession {
        recv: Box::new(rx),
        send: Arc::new(tx),
        stream: None,
        bind_addr: dst.clone(),
        dst: dst.clone(),
        src_addr: None,
        user_context: Default::default(),
    });
    let req = sniffer.sniff(req).await.unwrap();
    assert_eq!(
        req.user_context().sniffer.as_ref().unwrap().domain,
        "quic.example"
    );
    let ProxyRequest::Udp(mut session) = req else {
        panic!()
    };
    assert_eq!(session.recv.recv_from().await.unwrap(), (packet, dst));
    assert_eq!(
        sniffer.memory.available_permits(),
        sniffer.config.max_buffered_bytes
    );
    assert_eq!(
        session.recv.recv_from().await.unwrap(),
        (bytes::Bytes::from_static(b"second"), other)
    );
}

#[test]
fn quic_rfc9001_and_rfc9369_client_initial_vectors() {
    // Published independent vectors, Appendix A.2 in each RFC.
    for (packet, version) in [
        (
            include_bytes!("../../../tests/fixtures/sniffer/rfc9001-initial.bin"),
            "1",
        ),
        (
            include_bytes!("../../../tests/fixtures/sniffer/rfc9369-initial.bin"),
            "2",
        ),
    ] {
        let result = quic::parse(packet, 32768, &quic::suite().unwrap()).unwrap();
        assert_eq!(result.domain, "example.com");
        assert_eq!(result.version, version);
    }
}

#[tokio::test]
async fn configured_sniffer_runs_before_dispatch_to_outbound() {
    use crate::{Outbound, RequestDispatcher};
    struct Capture(tokio::sync::mpsc::Sender<ProxyRequest>);
    #[async_trait::async_trait]
    impl Outbound for Capture {
        async fn handle(&self, req: ProxyRequest) -> Result<(), SError> {
            self.0
                .send(req)
                .await
                .map_err(|_| SError::OutboundUnavailable)
        }
    }
    let config: crate::config::Config = serde_saphyr::from_str(
        r#"
inbounds:
  - {tag: local, type: socks, bind-addr: "127.0.0.1:0"}
outbounds:
  - {tag: direct, type: direct}
router:
  sniffer:
    tcp:
      - {ports: [80], protocols: [http]}
  src: |
    return function(ctx)
      assert(ctx.sniffer.domain == 'example.com')
      assert(ctx.sniffer.type == 'http')
      assert(ctx.sniffer.version == '1.1')
      return 'direct'
    end
"#,
    )
    .unwrap();
    let manager = config.build_manager().await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let dispatcher = RequestDispatcher {
        outbounds: std::collections::HashMap::from([(
            "direct".into(),
            Arc::new(Capture(tx)) as Arc<dyn Outbound>,
        )]),
        default_outbound: "direct".into(),
        router: manager.router,
    };
    let (mut client, req) = request(80);
    let wire = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
    client.write_all(wire).await.unwrap();
    dispatcher.dispatch(req).await;
    let req = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    replay(req, wire).await;
}

#[tokio::test(start_paused = true)]
async fn exclusions_overlaps_and_size_limits() {
    let mut cfg = config();
    cfg.tcp = vec![
        SnifferRule {
            ports: vec![],
            protocols: vec![SniffProtocol::Tls],
        },
        SnifferRule {
            ports: vec![80],
            protocols: vec![SniffProtocol::Http],
        },
    ];
    cfg.max_bytes = 256;
    let sniffer = Sniffer::new(cfg).unwrap();
    for (port, detected) in [(80, true), (8080, false)] {
        let (mut client, req) = request(port);
        let wire = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        client.write_all(wire).await.unwrap();
        let req = sniffer.sniff(req).await.unwrap();
        assert_eq!(req.user_context().sniffer.is_some(), detected);
        replay(req, wire).await;
    }
    let (mut client, req) = request(80);
    let wire = vec![b'A'; 512];
    client.write_all(&wire).await.unwrap();
    let req = sniffer.sniff(req).await.unwrap();
    assert!(req.user_context().sniffer.is_none());
    replay(req, &wire).await;
    assert_eq!(
        sniffer.memory.available_permits(),
        sniffer.config.max_buffered_bytes
    );
}

#[cfg(feature = "ring")]
#[test]
fn actual_tls13_client_hello_reports_advertised_version() {
    let config = rustls_jls::ClientConfig::builder_with_provider(Arc::new(
        rustls_jls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(rustls_jls::RootCertStore::empty())
    .with_no_client_auth();
    let mut conn =
        rustls_jls::ClientConnection::new(Arc::new(config), "example.com".try_into().unwrap())
            .unwrap();
    let mut wire = Vec::new();
    conn.write_tls(&mut wire).unwrap();
    let Parse::Done(Some(result)) = Parser::default().parse(&wire, 32768) else {
        panic!("missing ClientHello")
    };
    assert_eq!(result.version, "1.3");
    assert_eq!(result.domain, "example.com");
}

#[tokio::test]
async fn replay_releases_memory_before_stream_close() {
    let sniffer = Sniffer::new(config()).unwrap();
    let (mut client, req) = request(80);
    let wire = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
    client.write_all(wire).await.unwrap();
    let ProxyRequest::Tcp(mut req) = sniffer.sniff(req).await.unwrap() else {
        panic!()
    };
    assert!(sniffer.memory.available_permits() < sniffer.config.max_buffered_bytes);
    let mut received = vec![0; wire.len()];
    req.stream.read_exact(&mut received).await.unwrap();
    assert_eq!(received, wire);
    assert_eq!(
        sniffer.memory.available_permits(),
        sniffer.config.max_buffered_bytes
    );
    req.stream.write_all(b"reply").await.unwrap();
    let mut reply = [0; 5];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"reply");
}

#[test]
fn incremental_http_scan_handles_every_read_boundary() {
    let wire = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
    for split in 0..wire.len() {
        let mut parser = http::Parser::default();
        assert!(matches!(parser.parse(&wire[..split]), Parse::NeedMore));
        assert!(matches!(parser.parse(wire),Parse::Done(Some(r)) if r.domain == "example.com"));
    }
    let mut parser = http::Parser::default();
    for end in 0..wire.len() {
        assert!(matches!(parser.parse(&wire[..end]), Parse::NeedMore));
    }
    assert!(matches!(parser.parse(wire), Parse::Done(Some(_))));
}

#[tokio::test(start_paused = true)]
async fn quic_assembles_up_to_five_datagrams_and_replays_every_packet() {
    for version in [rustls_jls::quic::Version::V1, rustls_jls::quic::Version::V2] {
        for (parts, changed_destination) in [(2, false), (5, false), (6, false), (3, true)] {
            let sniffer = Sniffer::new(config()).unwrap();
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let dst: crate::SocksAddr = "192.0.2.1:443"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into();
            let other: crate::SocksAddr = "192.0.2.2:443"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into();
            let hello = hello(b"split.example");
            let mut packets = Vec::new();
            for i in 0..parts {
                let start = hello.len() * i / parts;
                let end = hello.len() * (i + 1) / parts;
                let packet = (
                    bytes::Bytes::from(initial(
                        version,
                        &crypto_frame(start, &hello[start..end]),
                        i as u8,
                    )),
                    if changed_destination && i == 1 {
                        other.clone()
                    } else {
                        dst.clone()
                    },
                );
                tx.send(packet.clone()).await.unwrap();
                packets.push(packet);
            }
            let req: ProxyRequest = ProxyRequest::Udp(crate::UdpSession {
                recv: Box::new(rx),
                send: Arc::new(tx),
                stream: None,
                bind_addr: dst.clone(),
                dst,
                src_addr: None,
                user_context: Default::default(),
            });
            let req = sniffer.sniff(req).await.unwrap();
            assert_eq!(
                req.user_context()
                    .sniffer
                    .as_ref()
                    .map(|r| r.domain.as_str()),
                if parts <= 5 && !changed_destination {
                    Some("split.example")
                } else {
                    None
                }
            );
            let ProxyRequest::Udp(mut session) = req else {
                panic!()
            };
            let buffered = if changed_destination { 2 } else { parts.min(5) };
            for (i, packet) in packets.into_iter().enumerate() {
                assert_eq!(session.recv.recv_from().await.unwrap(), packet);
                if i + 1 < buffered {
                    assert!(sniffer.memory.available_permits() < sniffer.config.max_buffered_bytes);
                } else {
                    assert_eq!(
                        sniffer.memory.available_permits(),
                        sniffer.config.max_buffered_bytes
                    );
                }
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn incomplete_quic_times_out_once_and_non_quic_bypasses_waiting() {
    for non_quic in [false, true] {
        let sniffer = Sniffer::new(config()).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let dst: crate::SocksAddr = "192.0.2.1:443"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into();
        let hello = hello(b"split.example");
        let data = if non_quic {
            b"not QUIC".to_vec()
        } else {
            initial(
                rustls_jls::quic::Version::V1,
                &crypto_frame(0, &hello[..10]),
                0,
            )
        };
        let packet = (bytes::Bytes::from(data), dst.clone());
        tx.send(packet.clone()).await.unwrap();
        let req: ProxyRequest = ProxyRequest::Udp(crate::UdpSession {
            recv: Box::new(rx),
            send: Arc::new(tx),
            stream: None,
            bind_addr: dst.clone(),
            dst,
            src_addr: None,
            user_context: Default::default(),
        });
        let started = tokio::time::Instant::now();
        let req = sniffer.sniff(req).await.unwrap();
        assert!(req.user_context().sniffer.is_none());
        assert_eq!(
            started.elapsed(),
            if non_quic {
                Duration::ZERO
            } else {
                Duration::from_millis(100)
            }
        );
        let ProxyRequest::Udp(mut session) = req else {
            panic!()
        };
        assert_eq!(session.recv.recv_from().await.unwrap(), packet);
        assert_eq!(
            sniffer.memory.available_permits(),
            sniffer.config.max_buffered_bytes
        );
    }
}
