use super::*;
use crate::{Outbound, direct::outbound::DirectOut};
use simple_dns::{CLASS, Name, Packet, PacketFlag, Question, TYPE};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::time::timeout;

fn query(id: u16) -> Vec<u8> {
    let mut packet = Packet::new_query(id);
    packet.questions.push(Question::new(
        Name::new("doq.test").unwrap(),
        TYPE::A.into(),
        CLASS::IN.into(),
        false,
    ));
    packet.build_bytes_vec().unwrap()
}

fn fixture(trusted: bool, name: &str) -> (quinn::Endpoint, DoqClient) {
    let (endpoint, client, _) = fixture_with_zero_rtt(trusted, name, true);
    (endpoint, client)
}

fn fixture_with_zero_rtt(
    trusted: bool,
    name: &str,
    zero_rtt: bool,
) -> (quinn::Endpoint, DoqClient, quinn::ServerConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let connector = crate::utils::tls::tls_connector().unwrap();
    let provider = connector.config().crypto_provider().clone();
    let mut tls = rustls_jls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls_jls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls_jls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                .into(),
        )
        .unwrap();
    tls.jls_config = rustls_jls::jls::JlsServerConfig::default()
        .enable(false)
        .into();
    tls.alpn_protocols = vec![b"doq".to_vec()];
    // Clone preserves the resumption store while changing early-data policy.
    let rejected = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls.clone()).unwrap(),
    ));
    tls.max_early_data_size = u32::MAX;
    let server = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap(),
    ));
    let endpoint = quinn::Endpoint::server(server, "127.0.0.1:0".parse().unwrap()).unwrap();
    let mut roots = rustls_jls::RootCertStore::empty();
    if trusted {
        roots.add(cert.cert.der().clone()).unwrap();
    }
    let tls = rustls_jls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls_jls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client =
        DoqClient::new(endpoint.local_addr().unwrap(), name.into(), tls, zero_rtt).unwrap();
    (endpoint, client, rejected)
}

fn route(
    client: &DoqClient,
) -> (
    mpsc::Sender<ProxyRequest>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let (requests, mut received) = mpsc::channel::<ProxyRequest>(4);
    let count = Arc::new(AtomicUsize::new(0));
    let handled = count.clone();
    let upstream = client.upstream;
    let task = tokio::spawn(async move {
        while let Some(request) = received.recv().await {
            assert!(matches!(request, ProxyRequest::Udp(_)));
            assert_eq!(request.dst().to_string(), upstream.to_string());
            let questions = &request.user_context().dns_query;
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].name, "doq.test");
            assert_eq!(questions[0].record_type, 1);
            handled.fetch_add(1, Ordering::SeqCst);
            DirectOut::default().handle(request).await.unwrap();
        }
    });
    (requests, count, task)
}

async fn receive_query(recv: &mut quinn::RecvStream) -> Vec<u8> {
    let bytes = read_frame(recv).await.unwrap();
    assert_eq!(Packet::parse(&bytes).unwrap().id(), 0);
    assert_eq!(
        recv.read(&mut [0; 1]).await.unwrap(),
        None,
        "query must end with FIN"
    );
    bytes
}

async fn answer(mut send: quinn::SendStream, mut recv: quinn::RecvStream) {
    let query = receive_query(&mut recv).await;
    let mut reply = Packet::parse(&query).unwrap();
    reply.set_flags(PacketFlag::RESPONSE);
    write_frame(&mut send, &reply.build_bytes_vec().unwrap())
        .await
        .unwrap();
    send.finish().unwrap();
}

#[tokio::test]
async fn routed_doq_multiplexes_reuses_and_reconnects() {
    timeout(Duration::from_secs(10), async {
        let (server, client) = fixture(true, "dns.test");
        let (requests, count, routing) = route(&client);
        let endpoint = server.clone();
        let (close, closing) = tokio::sync::oneshot::channel();
        let serving = tokio::spawn(async move {
            let connection = endpoint.accept().await.unwrap().await.unwrap();
            let (send, recv) = connection.accept_bi().await.unwrap();
            answer(send, recv).await;
            // Accept both queries before answering either: serialization would deadlock.
            let (mut send1, mut recv1) = connection.accept_bi().await.unwrap();
            let query1 = receive_query(&mut recv1).await;
            let (send2, recv2) = connection.accept_bi().await.unwrap();
            // A stalled response on the first stream must not hold up the second.
            answer(send2, recv2).await;
            let mut reply = Packet::parse(&query1).unwrap();
            reply.set_flags(PacketFlag::RESPONSE);
            write_frame(&mut send1, &reply.build_bytes_vec().unwrap())
                .await
                .unwrap();
            send1.finish().unwrap();
            closing.await.unwrap();
            connection.close(0u32.into(), b"reconnect test");
            let connection = endpoint.accept().await.unwrap().await.unwrap();
            let (send, recv) = connection.accept_bi().await.unwrap();
            answer(send, recv).await;
            // Keep the server endpoint and connection until the client is done.
            connection.closed().await;
        });
        let first = client.exchange(&query(17), &requests).await.unwrap();
        assert_eq!(Packet::parse(&first).unwrap().id(), 17);
        let q1 = query(18);
        let q2 = query(19);
        let (first, second) = tokio::join!(
            client.exchange(&q1, &requests),
            client.exchange(&q2, &requests)
        );
        assert_eq!(Packet::parse(&first.unwrap()).unwrap().id(), 18);
        assert_eq!(Packet::parse(&second.unwrap()).unwrap().id(), 19);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let old = client.session.lock().await.as_ref().unwrap().clone();
        close.send(()).unwrap();
        old.connection.closed().await;
        let reply = client.exchange(&query(20), &requests).await.unwrap();
        assert_eq!(Packet::parse(&reply).unwrap().id(), 20);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        drop(old);
        drop(client);
        serving.await.unwrap();
        drop(requests);
        routing.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn doq_verifies_certificates_and_hostnames() {
    for (trusted, name) in [(false, "dns.test"), (true, "wrong.test")] {
        let (server, client) = fixture(trusted, name);
        let (requests, _, routing) = route(&client);
        let serving = tokio::spawn(async move {
            let _ = server.accept().await.unwrap().await;
        });
        assert!(
            timeout(
                Duration::from_secs(3),
                client.exchange(&query(1), &requests)
            )
            .await
            .unwrap()
            .is_err()
        );
        serving.await.unwrap();
        drop(requests);
        routing.await.unwrap();
    }
}

#[tokio::test]
async fn cancelled_doq_query_stops_its_stream_and_preserves_connection() {
    let (server, client) = fixture(true, "dns.test");
    let (requests, count, routing) = route(&client);
    let serving = tokio::spawn(async move {
        let connection = server.accept().await.unwrap().await.unwrap();
        let (send, mut recv) = connection.accept_bi().await.unwrap();
        receive_query(&mut recv).await;
        assert_eq!(
            send.stopped().await.unwrap(),
            Some(DOQ_REQUEST_CANCELLED.into())
        );
        let (send, recv) = connection.accept_bi().await.unwrap();
        answer(send, recv).await;
        connection.closed().await;
    });
    assert!(
        timeout(
            Duration::from_millis(300),
            client.exchange(&query(1), &requests)
        )
        .await
        .is_err()
    );
    client.exchange(&query(2), &requests).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    drop(client);
    timeout(Duration::from_secs(3), serving)
        .await
        .unwrap()
        .unwrap();
    drop(requests);
    routing.await.unwrap();
}

#[tokio::test]
async fn proxy_socket_applies_backpressure_and_detects_closed_session() {
    let upstream = "127.0.0.1:853".parse().unwrap();
    let (mut socket, request) = ProxySocket::new(upstream, UserContext::default());
    let ProxyRequest::Udp(mut session) = request else {
        panic!("expected UDP")
    };
    let mut sender = socket.create_sender();
    let transmit = Transmit {
        destination: upstream,
        contents: b"quic",
        ecn: None,
        segment_size: None,
        src_ip: None,
    };
    let mut cx = Context::from_waker(std::task::Waker::noop());
    for _ in 0..QUEUE_SIZE {
        assert!(matches!(
            sender.as_mut().poll_send(&transmit, &mut cx),
            Poll::Ready(Ok(()))
        ));
    }
    assert!(sender.as_mut().poll_send(&transmit, &mut cx).is_pending());
    let (packet, destination) = session.recv.recv_from().await.unwrap();
    assert_eq!(packet.as_ref(), b"quic");
    assert_eq!(destination.to_string(), upstream.to_string());
    assert!(matches!(
        sender.as_mut().poll_send(&transmit, &mut cx),
        Poll::Ready(Ok(()))
    ));
    session
        .send
        .send_to(
            Bytes::from_static(b"wrong"),
            "127.0.0.1:54".parse::<SocketAddr>().unwrap().into(),
        )
        .await
        .unwrap();
    session
        .send
        .send_to(Bytes::from_static(b"reply"), upstream.into())
        .await
        .unwrap();
    let mut buffer = [0; 1500];
    let mut meta = [RecvMeta::default()];
    assert!(matches!(
        socket.poll_recv(&mut cx, &mut [IoSliceMut::new(&mut buffer)], &mut meta),
        Poll::Ready(Ok(1))
    ));
    assert_eq!(&buffer[..meta[0].len], b"reply");
    assert_eq!(meta[0].addr, upstream);
    drop(session);
    assert!(matches!(
        sender.as_mut().poll_send(&transmit, &mut cx),
        Poll::Ready(Err(_))
    ));
    assert!(matches!(
        socket.poll_recv(&mut cx, &mut [IoSliceMut::new(&mut buffer)], &mut meta),
        Poll::Ready(Err(_))
    ));
}

#[tokio::test]
async fn doq_reads_large_dns_responses_through_small_receive_windows() {
    timeout(Duration::from_secs(5), async {
        let (server, client) = fixture(true, "dns.test");
        let (requests, _, routing) = route(&client);
        let serving = tokio::spawn(async move {
            let connection = server.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            let query = receive_query(&mut recv).await;
            let mut reply = Packet::parse(&query).unwrap();
            reply.set_flags(PacketFlag::RESPONSE);
            let text = "a".repeat(240);
            for _ in 0..200 {
                reply.answers.push(simple_dns::ResourceRecord::new(
                    Name::new("doq.test").unwrap(),
                    CLASS::IN,
                    60,
                    simple_dns::rdata::RData::TXT(text.as_str().try_into().unwrap()),
                ));
            }
            let bytes = reply.build_bytes_vec().unwrap();
            assert!(bytes.len() > 48 * 1024);
            write_frame(&mut send, &bytes).await.unwrap();
            send.finish().unwrap();
            connection.closed().await;
        });
        let reply = client.exchange(&query(123), &requests).await.unwrap();
        let reply = Packet::parse(&reply).unwrap();
        assert_eq!(reply.id(), 123);
        assert_eq!(reply.answers.len(), 200);
        drop(client);
        serving.await.unwrap();
        drop(requests);
        routing.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn doq_rejects_invalid_ids_framing_and_extra_responses() {
    for mode in ["id", "extra", "short", "truncated"] {
        timeout(Duration::from_secs(5), async {
            let (server, client) = fixture(true, "dns.test");
            let (requests, count, routing) = route(&client);
            let serving = tokio::spawn(async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let query = receive_query(&mut recv).await;
                let mut reply = Packet::parse(&query).unwrap();
                reply.set_flags(PacketFlag::RESPONSE);
                if mode == "id" {
                    reply.set_id(1);
                }
                let bytes = reply.build_bytes_vec().unwrap();
                if mode == "short" {
                    send.write_all(&[0, 1, 0]).await.unwrap();
                } else if mode == "truncated" {
                    send.write_all(&(bytes.len() as u16).to_be_bytes())
                        .await
                        .unwrap();
                    send.write_all(&bytes[..bytes.len() - 1]).await.unwrap();
                } else {
                    write_frame(&mut send, &bytes).await.unwrap();
                }
                if mode == "extra" {
                    write_frame(&mut send, &bytes).await.unwrap();
                }
                send.finish().unwrap();
                let quinn::ConnectionError::ApplicationClosed(error) = connection.closed().await
                else {
                    panic!("expected DoQ application error")
                };
                assert_eq!(error.error_code, DOQ_PROTOCOL_ERROR.into());
            });
            assert!(client.exchange(&query(123), &requests).await.is_err());
            assert_eq!(
                count.load(Ordering::SeqCst),
                1,
                "protocol errors must not be retried"
            );
            serving.await.unwrap();
            drop(client);
            drop(requests);
            routing.await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn doq_configuration_rejects_invalid_server_name_before_binding() {
    let config = crate::config::DnsQuicServerCfg {
        tag: "quic".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        upstream: "127.0.0.1:853".parse().unwrap(),
        server_name: "invalid name".into(),
        zero_rtt: true,
        bypass_cache: false,
    };
    assert!(config.validate().is_err());
    assert!(config.build().await.is_err());
}

#[test]
fn doq_zero_rtt_defaults_to_disabled_and_can_be_enabled() {
    for (option, expected) in [
        ("", false),
        (", zero-rtt: true", true),
        (", zero-rtt: false", false),
    ] {
        let config: crate::config::DnsCfg = serde_saphyr::from_str(&format!(
            "{{tag: quic, type: dns-quic, bind-addr: '127.0.0.1:0', upstream: '127.0.0.1:853', server-name: dns.test{option}}}"
        )).unwrap();
        let crate::config::DnsCfg::Quic(config) = config else {
            panic!("expected QUIC")
        };
        assert_eq!(config.zero_rtt, expected);
    }
}

#[tokio::test]
async fn doq_resumes_with_early_data_and_retries_rejected_early_queries() {
    for (enabled, accepted) in [(true, true), (true, false), (false, true)] {
        timeout(Duration::from_secs(5), async {
            let (server, client, rejected) = fixture_with_zero_rtt(true, "dns.test", enabled);
            let (requests, count, routing) = route(&client);
            let (reconnect, reconnecting) = tokio::sync::oneshot::channel();
            let endpoint = server.clone();
            let serving = tokio::spawn(async move {
                let connection = endpoint.accept().await.unwrap().await.unwrap();
                let (send, recv) = connection.accept_bi().await.unwrap();
                answer(send, recv).await;
                reconnecting.await.unwrap();
                if !accepted {
                    endpoint.set_server_config(Some(rejected));
                }
                connection.close(0u32.into(), b"resume test");
                let connection = endpoint.accept().await.unwrap().await.unwrap();
                // Both concurrent requests must survive a rejected 0-RTT flight.
                for _ in 0..2 {
                    let (send, recv) = connection.accept_bi().await.unwrap();
                    answer(send, recv).await;
                }
                connection.closed().await;
            });
            client.exchange(&query(1), &requests).await.unwrap();
            let old = client.session.lock().await.as_ref().unwrap().clone();
            assert!(
                old.acceptance.is_none(),
                "the first connection cannot use 0-RTT"
            );
            reconnect.send(()).unwrap();
            old.connection.closed().await;
            let q1 = query(2);
            let q2 = query(3);
            let (r1, r2) = tokio::join!(
                client.exchange(&q1, &requests),
                client.exchange(&q2, &requests)
            );
            assert_eq!(Packet::parse(&r1.unwrap()).unwrap().id(), 2);
            assert_eq!(Packet::parse(&r2.unwrap()).unwrap().id(), 3);
            {
                let cached = client.session.lock().await;
                let session = cached.as_ref().unwrap();
                assert_eq!(session.acceptance.is_some(), enabled);
                if enabled {
                    assert_eq!(session.accepted.get(), Some(&accepted));
                }
            }
            assert_eq!(
                count.load(Ordering::SeqCst),
                2,
                "resumption must use one new UDP session"
            );
            drop(old);
            drop(client);
            serving.await.unwrap();
            drop(requests);
            routing.await.unwrap();
        })
        .await
        .unwrap();
    }
}

/// Proves 0-RTT negotiation on a replacement AliDNS connection, rather than
/// merely reusing an already established connection. No DNS response cache is used.
#[tokio::test]
#[ignore = "requires Internet access to AliDNS over UDP port 853"]
async fn alidns_resumes_with_zero_rtt_attempt_and_fallback() {
    timeout(Duration::from_secs(12), async {
        let tls = crate::utils::tls::tls_connector()
            .unwrap()
            .config()
            .as_ref()
            .clone();
        let client = DoqClient::new(
            "223.5.5.5:853".parse().unwrap(),
            "dns.alidns.com".into(),
            tls,
            true,
        )
        .unwrap();
        let (requests, mut received) = mpsc::channel::<ProxyRequest>(4);
        let routing = tokio::spawn(async move {
            while let Some(request) = received.recv().await {
                DirectOut::default().handle(request).await.unwrap();
            }
        });
        let mut packet = Packet::new_query(123);
        packet.set_flags(PacketFlag::RECURSION_DESIRED);
        packet.questions.push(Question::new(
            Name::new("www.aliyun.com").unwrap(),
            TYPE::A.into(),
            CLASS::IN.into(),
            false,
        ));
        let query = packet.build_bytes_vec().unwrap();
        for resumed in [false, true] {
            let reply = client.exchange(&query, &requests).await.unwrap();
            let reply = Packet::parse(&reply).unwrap();
            assert_eq!(reply.id(), 123);
            assert_eq!(reply.rcode(), simple_dns::RCODE::NoError);
            assert!(
                reply
                    .answers
                    .iter()
                    .any(|record| matches!(record.rdata, simple_dns::rdata::RData::A(_)))
            );
            let cached = client.session.lock().await;
            let session = cached.as_ref().unwrap();
            assert_eq!(session.acceptance.is_some(), resumed);
            if resumed {
                let accepted = session
                    .accepted
                    .get()
                    .expect("0-RTT handshake result must be observed");
                eprintln!(
                    "AliDNS resumed connection: 0-RTT accepted={accepted}; DNS query succeeded"
                );
            }
            session.connection.close(0u32.into(), b"resumption test");
            session.connection.closed().await;
        }
        drop(client);
        drop(requests);
        routing.await.unwrap();
    })
    .await
    .unwrap();
}
