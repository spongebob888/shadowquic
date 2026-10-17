use super::*;
use crate::ProxyRequest;
#[cfg(feature = "dns-server")]
use crate::dns::DnsService;
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

struct FixtureOutbound {
    destination: SocketAddr,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl Outbound for FixtureOutbound {
    async fn handle(&self, request: ProxyRequest) -> Result<(), SError> {
        let ProxyRequest::Tcp(mut session) = request else {
            panic!("HTTP probes must dispatch TCP sessions");
        };
        assert!(session.src_addr.is_none());
        assert!(session.user_context.observer.upgrade().is_none());
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut upstream = TcpStream::connect(self.destination).await?;
        tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut session.stream, &mut upstream).await;
        });
        Ok(())
    }
}

#[async_trait::async_trait]
impl SQStatsTrait for FixtureOutbound {
    async fn get_sq_stats(&self) -> Option<crate::squic::SQConnStats> {
        let stats = crate::msgs::squic::ConnStats {
            lost_packets: 0,
            sent_packets: self.calls.load(Ordering::Relaxed) as u64,
            rtt: 1.5,
            current_mtu: 1280,
        };
        Some(crate::squic::SQConnStats {
            uplink: stats.clone(),
            downlink: stats,
            time: Instant::now(),
        })
    }
}

fn manager(
    outbounds: HashMap<String, Arc<dyn Outbound>>,
    sq_outbounds: HashMap<String, Arc<dyn SQStatsTrait>>,
) -> Arc<ApiManager> {
    Arc::new(ApiManager::new(
        outbounds,
        sq_outbounds,
        HashMap::new(),
        #[cfg(feature = "dns-server")]
        None,
    ))
}

fn probing_manager(tag: &str, outbound: Arc<FixtureOutbound>) -> Arc<ApiManager> {
    manager(
        HashMap::from([(tag.into(), outbound.clone() as Arc<dyn Outbound>)]),
        HashMap::from([(tag.into(), outbound as Arc<dyn SQStatsTrait>)]),
    )
}

async fn server(
    api: Arc<ApiManager>,
) -> (
    SocketAddr,
    watch::Sender<bool>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = bind(&ApiConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
    })
    .await
    .unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    (addr, stop, tokio::spawn(api.serve(listener, stopped)))
}

async fn call(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, Value) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let driver = AbortOnDrop(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(3), sender.send_request(request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.headers()["content-type"], "application/json");
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    drop(driver);
    (status, serde_json::from_slice(&body).unwrap())
}

async fn fixture() -> (
    SocketAddr,
    mpsc::Receiver<(String, TcpStream)>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (send, recv) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                if stream.read_exact(&mut byte).await.is_err() {
                    return;
                }
                request.push(byte[0]);
            }
            if send
                .send((String::from_utf8(request).unwrap(), stream))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    (addr, recv, task)
}

#[tokio::test]
async fn routes_return_empty_collections_and_json_errors() {
    let (addr, stop, task) = server(manager(HashMap::new(), HashMap::new())).await;
    for (path, expected) in [
        ("/api/v1/outbounds", json!({"outbounds": []})),
        ("/api/v1/connections", json!({"connections": []})),
        ("/api/v1/traffic", json!({"inbounds": []})),
    ] {
        let (status, body) = call(addr, "GET", path, "").await;
        assert_eq!(status, 200);
        assert_eq!(body, expected);
        assert_eq!(call(addr, "POST", path, "{}").await.0, 405);
    }
    assert_eq!(call(addr, "GET", "/missing", "").await.0, 404);
    assert_eq!(
        call(addr, "POST", "/api/v1/outbounds/missing/probe", "{}")
            .await
            .0,
        404
    );
    let (status, body) = call(addr, "GET", "/api/v1/memory", "").await;
    if !cfg!(feature = "jemalloc") {
        assert_eq!(status, 404);
        assert_eq!(body["error"]["code"], "not_found");
    } else if cfg!(any(
        target_os = "android",
        all(
            any(target_os = "linux", target_os = "macos"),
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    )) {
        assert_eq!(status, 200);
        assert_eq!(body["allocator"], "jemalloc");
        assert!(body["allocated_bytes"].as_u64().unwrap() > 0);
    } else {
        assert_eq!(status, 501);
        assert_eq!(body["error"]["code"], "memory_stats_unavailable");
    }
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn probes_preserve_http_target_cache_results_and_release_connections() {
    let (target, mut requests, fixture_task) = fixture().await;
    let outbound = Arc::new(FixtureOutbound {
        destination: target,
        calls: 0.into(),
    });
    let api = probing_manager("proxy/one", outbound.clone());
    let (addr, stop, task) = server(api.clone()).await;
    // Listing quality must not initiate network activity.
    assert!(api.get_outbounds().await["outbounds"][0]["quality"]["probe"].is_null());
    assert_eq!(outbound.calls.load(Ordering::Relaxed), 0);
    let body = r#"{"url":"http://probe.test:8080/path?q=value"}"#;
    let probe = tokio::spawn(call(
        addr,
        "POST",
        "/api/v1/outbounds/proxy%2Fone/probe",
        body,
    ));
    let (request, mut peer) = requests.recv().await.unwrap();
    assert!(request.starts_with("GET /path?q=value HTTP/1.1\r\n"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("host: probe.test:8080\r\n")
    );
    peer.write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
        .await
        .unwrap();
    let (status, result) = probe.await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(result["status"], "ok");
    assert!(result["latency_ms"].is_u64());
    assert_eq!(
        api.get_outbounds().await["outbounds"][0]["quality"]["probe"],
        result
    );
    let quality = api.get_outbounds().await;
    let sample = &quality["outbounds"][0]["quality"]["sq_conn_stats"];
    assert_eq!(sample["uplink"]["sent_packets"], 1);
    assert_eq!(sample["uplink"]["rtt"], 1.5);
    assert_eq!(sample["downlink"]["current_mtu"], 1280);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );

    let probe = tokio::spawn(call(
        addr,
        "POST",
        "/api/v1/outbounds/proxy%2Fone/probe",
        body,
    ));
    let (_, mut peer) = requests.recv().await.unwrap();
    peer.write_all(
        b"HTTP/1.1 302 Found\r\nLocation: http://elsewhere.test/\r\nContent-Length: 0\r\n\r\n",
    )
    .await
    .unwrap();
    assert_eq!(probe.await.unwrap().0, 502);
    let cached = api.get_outbounds().await;
    let result = &cached["outbounds"][0]["quality"]["probe"];
    assert_eq!(result["status"], "failed");
    assert!(result["latency_ms"].is_null());
    assert_eq!(outbound.calls.load(Ordering::Relaxed), 2);
    assert_eq!(api.get_connections().await, json!({"connections": []}));
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    fixture_task.abort();
}

#[tokio::test]
async fn probe_timeout_and_shutdown_cancel_outbound_sessions() {
    let (target, mut requests, fixture_task) = fixture().await;
    let outbound = Arc::new(FixtureOutbound {
        destination: target,
        calls: 0.into(),
    });
    let api = probing_manager("proxy", outbound);
    let (addr, stop, task) = server(api.clone()).await;
    let body = r#"{"url":"http://probe.test/","timeout_ms":200}"#;
    let probe = tokio::spawn(call(addr, "POST", "/api/v1/outbounds/proxy/probe", body));
    let (_, mut peer) = requests.recv().await.unwrap();
    assert_eq!(
        call(addr, "POST", "/api/v1/outbounds/proxy/probe", body)
            .await
            .0,
        409
    );
    assert_eq!(probe.await.unwrap().0, 504);
    assert_eq!(
        api.get_outbounds().await["outbounds"][0]["quality"]["probe"]["status"],
        "timeout"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );

    // Start a second probe and stop the server without waiting for its long timeout.
    let client = TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(client))
        .await
        .unwrap();
    let _driver = AbortOnDrop(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/outbounds/proxy/probe")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from_static(
            br#"{"url":"http://probe.test/"}"#,
        )))
        .unwrap();
    let probe = tokio::spawn(async move { sender.send_request(request).await });
    let (_, mut peer) = requests.recv().await.unwrap();
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(probe.await.unwrap().is_err());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(api.outbounds["proxy"].probe_lock.try_lock().is_ok());
    assert_eq!(
        api.get_outbounds().await["outbounds"][0]["quality"]["probe"]["status"],
        "timeout"
    );
    fixture_task.abort();
}

#[test]
fn probe_validation_rejects_invalid_urls_and_timeouts() {
    assert!(
        ProbeRequest {
            url: format!("http://{}/", "a".repeat(256)),
            timeout_ms: 1000,
        }
        .validate()
        .is_err()
    );
    for url in [
        "https://example.test/",
        "/relative",
        "http://user:pass@example.test/",
        "file:///tmp/test",
    ] {
        assert!(
            ProbeRequest {
                url: url.into(),
                timeout_ms: 1000
            }
            .validate()
            .is_err()
        );
    }
    for timeout_ms in [0, 60_001] {
        assert!(
            ProbeRequest {
                url: "http://example.test/".into(),
                timeout_ms
            }
            .validate()
            .is_err()
        );
    }
    assert!(decode_tag("bad%2").is_err());
    assert!(decode_tag("%FF").is_err());
}

#[tokio::test]
async fn api_binding_rejects_non_loopback_addresses() {
    for addr in ["0.0.0.0:0", "[::]:0", "192.0.2.1:8080"] {
        assert!(
            bind(&ApiConfig {
                bind_addr: addr.parse().unwrap()
            })
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn disconnecting_api_client_cancels_probe_without_overwriting_cache() {
    let (target, mut requests, fixture_task) = fixture().await;
    let outbound = Arc::new(FixtureOutbound {
        destination: target,
        calls: 0.into(),
    });
    let api = probing_manager("proxy", outbound);
    let (addr, stop, task) = server(api.clone()).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    let body = r#"{"url":"http://probe.test/"}"#;
    client.write_all(format!("POST /api/v1/outbounds/proxy/probe HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
    let (_, mut peer) = requests.recv().await.unwrap();
    drop(client);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if api.outbounds["proxy"].probe_lock.try_lock().is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(api.get_outbounds().await["outbounds"][0]["quality"]["probe"].is_null());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    fixture_task.abort();
}

#[tokio::test]
async fn cached_quality_reads_do_not_wait_for_a_reconnecting_quic_outbound() {
    let cache = Mutex::<Option<crate::squic::SQConn<crate::shadowquic::Connection>>>::new(None);
    let _connecting = cache.lock().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), quality::cached_stats(&cache))
            .await
            .unwrap()
            .is_none()
    );
}

#[cfg(not(feature = "dns-server"))]
impl crate::TcpTrait for tokio::io::DuplexStream {}

fn tcp_request(dst: SocksAddr) -> (ProxyRequest, tokio::io::DuplexStream) {
    let (stream, peer) = tokio::io::duplex(1024);
    (
        ProxyRequest::Tcp(crate::TcpSession {
            stream: Box::new(stream),
            dst,
            src_addr: Some("[::1]:1234".parse().unwrap()),
            user_context: crate::UserContext::default(),
        }),
        peer,
    )
}

#[tokio::test]
async fn traffic_sums_named_and_anonymous_users_and_follows_user_counter_changes() {
    struct Connection;
    impl crate::Stoppable for Connection {
        fn stop(&self) {}
    }
    let observer = Arc::new(Observer::new());
    observer
        .restore_stats(&[
            crate::msgs::squic::UserStats {
                username: "alice".into(),
                tcp_sent: 10,
                tcp_recv: 20,
                tcp_conns: 9,
                ..Default::default()
            },
            crate::msgs::squic::UserStats {
                username: "bob".into(),
                udp_sent: 30,
                udp_recv: 40,
                udp_conns: 9,
                ..Default::default()
            },
        ])
        .await;
    let api = ApiManager::new(
        HashMap::new(),
        HashMap::new(),
        HashMap::from([("local".into(), observer.clone())]),
        #[cfg(feature = "dns-server")]
        None,
    );
    assert_eq!(
        api.get_traffic().await["inbounds"][0],
        json!({
            "inbound": "local", "sent_bytes": 40, "recv_bytes": 60, "tcp_conns": 0, "udp_conns": 0,
        })
    );
    let (request, mut peer) = tcp_request("192.0.2.1:443".parse::<SocketAddr>().unwrap().into());
    let ProxyRequest::Tcp(mut tcp) = observer.wrap_request(request, "selected".into()).await else {
        unreachable!()
    };
    peer.write_all(b"upload").await.unwrap();
    tcp.stream.read_exact(&mut [0; 6]).await.unwrap();
    tcp.stream.write_all(b"reply").await.unwrap();
    let conn: Arc<dyn crate::Stoppable> = Arc::new(Connection);
    let (send, recv) = mpsc::channel(1);
    let (reply, mut replies) = mpsc::channel(1);
    let dst: SocksAddr = "192.0.2.1:53".parse::<SocketAddr>().unwrap().into();
    let request: ProxyRequest = ProxyRequest::Udp(crate::UdpSession {
        recv: Box::new(recv),
        send: Arc::new(reply),
        stream: None,
        dst: dst.clone(),
        bind_addr: dst.clone(),
        src_addr: None,
        user_context: crate::UserContext {
            stats: Some(crate::StatsContext {
                username: "alice".into(),
                conn_id: 1,
                conn_handle: Arc::downgrade(&conn),
            }),
            ..Default::default()
        },
    });
    let ProxyRequest::Udp(mut udp) = observer.wrap_request(request, "selected".into()).await else {
        unreachable!()
    };
    send.send((Bytes::from_static(b"four"), dst.clone()))
        .await
        .unwrap();
    udp.recv.recv_from().await.unwrap();
    udp.send
        .send_to(Bytes::from_static(b"yes"), dst)
        .await
        .unwrap();
    replies.recv().await.unwrap();
    assert_eq!(
        api.get_traffic().await["inbounds"][0],
        json!({
            "inbound": "local", "sent_bytes": 48, "recv_bytes": 70, "tcp_conns": 1, "udp_conns": 1,
        })
    );
    observer.clear_user_stats("alice").await;
    assert_eq!(
        api.get_traffic().await["inbounds"][0],
        json!({
            "inbound": "local", "sent_bytes": 35, "recv_bytes": 46, "tcp_conns": 1, "udp_conns": 1,
        })
    );
    observer.remove_user("bob").await;
    assert_eq!(
        api.get_traffic().await["inbounds"][0],
        json!({
            "inbound": "local", "sent_bytes": 5, "recv_bytes": 6, "tcp_conns": 1, "udp_conns": 1,
        })
    );
    observer.clear_all_stats(&["".into()]).await;
    assert_eq!(
        api.get_traffic().await["inbounds"][0],
        json!({
            "inbound": "local", "sent_bytes": 0, "recv_bytes": 0, "tcp_conns": 1, "udp_conns": 1,
        })
    );
    drop(tcp);
    drop(udp);
    assert_eq!(
        api.get_traffic().await["inbounds"][0],
        json!({
            "inbound": "local", "sent_bytes": 0, "recv_bytes": 0, "tcp_conns": 0, "udp_conns": 0,
        })
    );
}

#[tokio::test]
async fn snapshots_report_active_sessions_and_preserve_udp_half_metadata() {
    let observer = Arc::new(Observer::new());
    let api = ApiManager::new(
        HashMap::new(),
        HashMap::new(),
        HashMap::from([
            ("local".into(), observer.clone()),
            ("idle".into(), Arc::new(Observer::new())),
        ]),
        #[cfg(feature = "dns-server")]
        None,
    );
    let (request, mut peer) = tcp_request(SocksAddr::from_domain("destination.test".into(), 8443));
    let ProxyRequest::Tcp(mut tcp) = observer.wrap_request(request, "chosen".into()).await else {
        unreachable!()
    };
    let created = observer.tcp_scoreboard.lock().await[0]
        .1
        .upgrade()
        .unwrap()
        .created;
    let elapsed_before = millis(created.elapsed());
    let before = api.get_connections().await;
    let initial = &before["connections"][0];
    assert_eq!(initial["protocol"], "tcp");
    assert_eq!(initial["src"], "[::1]:1234");
    assert!(initial["dst_ip"].is_null());
    assert_eq!(initial["dst_domain"], "destination.test");
    assert_eq!(initial["dst_port"], 8443);
    assert_eq!(initial["outbound"], "chosen");
    assert_eq!(initial["inbound"], "local");
    assert_eq!(initial["half_closed"], false);
    assert!(initial["first_response_ms"].is_null());
    let duration_ms = initial["duration_ms"].as_u64().unwrap();
    assert!(duration_ms >= elapsed_before);
    assert!(duration_ms <= millis(created.elapsed()));
    assert!(initial.get("created_at").is_none());
    peer.write_all(b"upload").await.unwrap();
    tcp.stream.read_exact(&mut [0; 6]).await.unwrap();
    tcp.stream.write_all(b"reply").await.unwrap();
    let after = api.get_connections().await;
    assert_eq!(after["connections"][0]["id"], initial["id"]);
    assert_eq!(after["connections"][0]["recv_bytes"], 6);
    assert_eq!(after["connections"][0]["sent_bytes"], 5);
    assert!(after["connections"][0]["duration_ms"].as_u64().unwrap() >= duration_ms);
    assert!(after["connections"][0]["first_response_ms"].is_u64());
    // Liveness and half-close status follow the scoreboard's weak pointers.
    let held_metadata = observer.tcp_scoreboard.lock().await[0].1.upgrade().unwrap();
    drop(tcp);
    let snapshot = api.get_connections().await;
    assert_eq!(snapshot["connections"][0]["id"], initial["id"]);
    assert_eq!(snapshot["connections"][0]["half_closed"], true);
    assert!(snapshot["connections"][0]["sent_bytes"].is_null());
    assert_eq!(api.get_traffic().await["inbounds"][1]["tcp_conns"], 1);
    drop(held_metadata);
    assert_eq!(api.get_connections().await, json!({"connections": []}));
    assert_eq!(api.get_traffic().await["inbounds"][1]["tcp_conns"], 0);

    let (send_request, recv_request) = mpsc::channel(1);
    let (send_reply, mut recv_reply) = mpsc::channel(1);
    let dst: SocksAddr = "[2001:db8::1]:53".parse::<SocketAddr>().unwrap().into();
    let request: ProxyRequest = ProxyRequest::Udp(crate::UdpSession {
        recv: Box::new(recv_request),
        send: Arc::new(send_reply),
        stream: None,
        bind_addr: dst.clone(),
        dst: dst.clone(),
        src_addr: None,
        user_context: Default::default(),
    });
    let ProxyRequest::Udp(mut udp) = observer.wrap_request(request, "chosen".into()).await else {
        unreachable!()
    };
    send_request
        .send((Bytes::from_static(b"request"), dst.clone()))
        .await
        .unwrap();
    udp.recv.recv_from().await.unwrap();
    udp.send
        .send_to(Bytes::from_static(b"answer"), dst.clone())
        .await
        .unwrap();
    recv_reply.recv().await.unwrap();
    let before = api.get_connections().await;
    assert_eq!(before["connections"][0]["half_closed"], false);
    assert_eq!(before["connections"][0]["recv_bytes"], 7);
    let send = udp.send.clone();
    drop(udp);
    let snapshot = api.get_connections().await;
    let session = &snapshot["connections"][0];
    assert_eq!(session["protocol"], "udp");
    assert_eq!(session["id"], before["connections"][0]["id"]);
    assert_eq!(session["half_closed"], true);
    assert_eq!(session["dst_ip"], "2001:db8::1");
    assert!(session["dst_domain"].is_null());
    assert!(session["src"].is_null());
    assert!(session["recv_bytes"].is_null());
    assert!(session["duration_ms"].is_null());
    assert!(session["first_response_ms"].is_null());
    assert_eq!(session["sent_bytes"], 6);
    assert_eq!(
        api.get_traffic().await,
        json!({"inbounds": [
            {"inbound": "idle", "sent_bytes": 0, "recv_bytes": 0, "tcp_conns": 0, "udp_conns": 0},
            {"inbound": "local", "sent_bytes": 11, "recv_bytes": 13, "tcp_conns": 0, "udp_conns": 0},
        ]})
    );
    drop(send);
    assert_eq!(api.get_connections().await, json!({"connections": []}));
    assert_eq!(api.get_traffic().await["inbounds"][1]["udp_conns"], 0);
    assert_eq!(api.get_traffic().await["inbounds"][1]["sent_bytes"], 11);
}

#[tokio::test]
async fn connections_report_half_closed_when_only_udp_receiver_remains() {
    let observer = Arc::new(Observer::new());
    let api = ApiManager::new(
        HashMap::new(),
        HashMap::new(),
        HashMap::from([("local".into(), observer.clone())]),
        #[cfg(feature = "dns-server")]
        None,
    );
    let (send, recv) = mpsc::channel(1);
    let (reply, _replies) = mpsc::channel(1);
    let dst: SocksAddr = "192.0.2.1:53".parse::<SocketAddr>().unwrap().into();
    let request: ProxyRequest = ProxyRequest::Udp(crate::UdpSession {
        recv: Box::new(recv),
        send: Arc::new(reply),
        stream: None,
        dst: dst.clone(),
        bind_addr: dst.clone(),
        src_addr: Some("127.0.0.1:1234".parse().unwrap()),
        user_context: Default::default(),
    });
    let ProxyRequest::Udp(udp) = observer.wrap_request(request, "chosen".into()).await else {
        unreachable!()
    };
    let before = api.get_connections().await;
    assert_eq!(before["connections"][0]["half_closed"], false);
    let mut receiver = udp.recv;
    drop(udp.send);
    send.send((Bytes::from_static(b"upload"), dst))
        .await
        .unwrap();
    receiver.recv_from().await.unwrap();
    let after = api.get_connections().await;
    let session = &after["connections"][0];
    assert_eq!(session["id"], before["connections"][0]["id"]);
    assert!(
        session["duration_ms"].as_u64().unwrap()
            >= before["connections"][0]["duration_ms"].as_u64().unwrap()
    );
    assert_eq!(session["half_closed"], true);
    assert_eq!(session["src"], "127.0.0.1:1234");
    assert_eq!(session["recv_bytes"], 6);
    for field in [
        "outbound",
        "dst_ip",
        "dst_domain",
        "dst_port",
        "sent_bytes",
        "first_response_ms",
    ] {
        assert!(session[field].is_null(), "{field}");
    }
    assert_eq!(api.get_traffic().await["inbounds"][0]["udp_conns"], 1);
    drop(receiver);
    assert_eq!(api.get_connections().await, json!({"connections": []}));
    assert_eq!(api.get_traffic().await["inbounds"][0]["udp_conns"], 0);
}

#[tokio::test]
async fn invalid_and_unsupported_probe_requests_leave_cached_results_unchanged() {
    struct UnsupportedOutbound;
    #[async_trait::async_trait]
    impl Outbound for UnsupportedOutbound {
        async fn handle(&self, _: ProxyRequest) -> Result<(), SError> {
            Err(SError::ProtocolUnimpl)
        }
    }
    let (target, _requests, fixture_task) = fixture().await;
    let outbound = Arc::new(FixtureOutbound {
        destination: target,
        calls: 0.into(),
    });
    let api = manager(
        HashMap::from([
            ("proxy".into(), outbound.clone() as Arc<dyn Outbound>),
            (
                "drop".into(),
                Arc::new(crate::drop_outbound::DropOutbound) as Arc<dyn Outbound>,
            ),
            (
                "unsupported".into(),
                Arc::new(UnsupportedOutbound) as Arc<dyn Outbound>,
            ),
        ]),
        HashMap::new(),
    );
    let cached = ProbeResult {
        url: "http://previous.test/".into(),
        status: "ok",
        latency_ms: Some(12),
        measured_at: timestamp(SystemTime::now()),
        error: None,
    };
    *api.outbounds["proxy"].probe.lock().unwrap() = Some(cached.clone());
    let (addr, stop, task) = server(api.clone()).await;
    for body in [
        "{}",
        "not json",
        r#"{"url":"https://example.test/"}"#,
        r#"{"url":"http://example.test/","timeout_ms":0}"#,
        r#"{"url":"http://example.test/","unknown":1}"#,
    ] {
        assert_eq!(
            call(addr, "POST", "/api/v1/outbounds/proxy/probe", body)
                .await
                .0,
            400
        );
    }
    assert_eq!(
        call(
            addr,
            "POST",
            "/api/v1/outbounds/unsupported/probe",
            r#"{"url":"http://example.test/"}"#
        )
        .await
        .0,
        501
    );
    assert_eq!(
        call(
            addr,
            "POST",
            "/api/v1/outbounds/drop/probe",
            r#"{"url":"http://example.test/"}"#
        )
        .await
        .0,
        502
    );
    assert_eq!(outbound.calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        api.get_outbounds().await["outbounds"][1]["quality"]["probe"],
        json!(cached)
    );
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    fixture_task.abort();
}

#[cfg(feature = "dns-server")]
#[tokio::test]
async fn connections_use_cached_domains_without_resolving_on_a_miss() {
    use simple_dns::{CLASS, Packet, PacketFlag, ResourceRecord, TYPE, rdata::RData};
    let upstream = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let responder = tokio::spawn(async move {
        let mut buf = [0; 2048];
        loop {
            let (len, peer) = upstream.recv_from(&mut buf).await.unwrap();
            let query = Packet::parse(&buf[..len]).unwrap();
            let mut response = Packet::new_reply(query.id());
            response.set_flags(PacketFlag::RECURSION_AVAILABLE);
            response.questions = query.questions.clone();
            let q = &query.questions[0];
            if q.qtype == TYPE::A.into() {
                response.answers.push(ResourceRecord::new(
                    q.qname.clone(),
                    CLASS::IN,
                    60,
                    RData::A("192.0.2.7".parse::<std::net::Ipv4Addr>().unwrap().into()),
                ));
            }
            upstream
                .send_to(&response.build_bytes_vec().unwrap(), peer)
                .await
                .unwrap();
        }
    });
    let cache = Arc::new(crate::dns::DnsCache::default());
    let dns = crate::dns::config::DnsUdpServerCfg {
        tag: "dns".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        upstream: upstream_addr,
        bypass_cache: false,
    }
    .build_with_cache(cache.clone())
    .await
    .unwrap();
    let resolver = dns.resolver.clone();
    let _worker = AbortOnDrop(tokio::spawn(async move {
        let mut dns = dns;
        let direct = crate::direct::outbound::DirectOut::default();
        while let Ok(request) = crate::Inbound::accept(&mut dns).await {
            direct.handle(request).await.unwrap();
        }
    }));
    tokio::time::timeout(Duration::from_secs(3), resolver.resolve("cached.test"))
        .await
        .unwrap()
        .unwrap();
    // Stop the upstream: connection snapshots must use cache-only lookups.
    responder.abort();
    let observer = Arc::new(Observer::new());
    let api = ApiManager::new(
        HashMap::new(),
        HashMap::new(),
        HashMap::from([("local".into(), observer.clone())]),
        Some(cache),
    );
    let (hit, _peer) = tcp_request("192.0.2.7:443".parse::<SocketAddr>().unwrap().into());
    let _hit = observer.wrap_request(hit, "chosen".into()).await;
    let (miss, _other_peer) = tcp_request("192.0.2.8:80".parse::<SocketAddr>().unwrap().into());
    let _miss = observer.wrap_request(miss, "chosen".into()).await;
    let snapshot = tokio::time::timeout(Duration::from_millis(100), api.get_connections())
        .await
        .unwrap();
    let entries = snapshot["connections"].as_array().unwrap();
    let hit = entries
        .iter()
        .find(|entry| entry["dst_ip"] == "192.0.2.7")
        .unwrap();
    assert_eq!(hit["dst_domain"], "cached.test");
    let miss = entries
        .iter()
        .find(|entry| entry["dst_ip"] == "192.0.2.8")
        .unwrap();
    assert!(miss["dst_domain"].is_null());
}
