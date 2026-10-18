use super::Observer;
use crate::{
    Inbound, ProxyRequest,
    config::{AuthUser, SocksServerCfg},
    msgs::{SDecode, socks5::CmdReply},
    socks::inbound::SocksServer,
};
use bytes::Bytes;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
};

const KINDS: &[&str] = &[
    "socks",
    #[cfg(feature = "mixed")]
    "mixed",
];

async fn inbound(kind: &str, authenticated: bool) -> (Box<dyn Inbound>, Arc<Observer>, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind_addr = listener.local_addr().unwrap();
    drop(listener);
    let users = if authenticated {
        vec![
            AuthUser {
                username: "bob".into(),
                password: "other".into(),
            },
            AuthUser {
                username: "alice".into(),
                password: "pass".into(),
            },
        ]
    } else {
        vec![]
    };
    let (inbound, observer): (Box<dyn Inbound>, _) = match kind {
        "socks" => {
            let server = SocksServer::new(SocksServerCfg {
                tag: "local".into(),
                default_outbound: Some("selected".into()),
                bind_addr,
                users,
            })
            .await
            .unwrap();
            let observer = server.observer.clone();
            (Box::new(server), observer)
        }
        #[cfg(feature = "mixed")]
        "mixed" => {
            let server = crate::mixed::inbound::MixedServer::new(crate::config::MixedServerCfg {
                tag: "local".into(),
                default_outbound: Some("selected".into()),
                bind_addr,
                users,
            })
            .await
            .unwrap();
            let observer = server.observer.clone();
            (Box::new(server), observer)
        }
        _ => unreachable!(),
    };
    inbound.init().await.unwrap();
    (inbound, observer, bind_addr)
}

async fn socks_client(
    bind_addr: SocketAddr,
    authenticated: bool,
    udp: bool,
) -> (TcpStream, CmdReply) {
    let mut client = TcpStream::connect(bind_addr).await.unwrap();
    let method = if authenticated { 2 } else { 0 };
    client.write_all(&[5, 1, method]).await.unwrap();
    let mut reply = [0; 2];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [5, method]);
    if authenticated {
        client.write_all(b"\x01\x05alice\x04pass").await.unwrap();
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [1, 0]);
    }
    client
        .write_all(&[5, if udp { 3 } else { 1 }, 0, 1, 192, 0, 2, 1, 1, 187])
        .await
        .unwrap();
    let reply = CmdReply::decode(&mut client).await.unwrap();
    assert_eq!(reply.rep, 0);
    (client, reply)
}

async fn tracked(
    inbound: &mut dyn Inbound,
    observer: &Arc<Observer>,
    username: &str,
) -> ProxyRequest {
    let request = inbound.accept().await.unwrap();
    assert!(Arc::ptr_eq(
        &request.user_context().observer.upgrade().unwrap(),
        observer
    ));
    assert_eq!(request.user_context().inbound_tag, "local");
    assert_eq!(
        request.user_context().preferred_outbound.as_deref(),
        Some("selected")
    );
    let stats = request.user_context().stats.as_ref().unwrap();
    assert_eq!(stats.username, username);
    assert!(stats.conn_handle.upgrade().is_none());
    let request = observer.wrap_request(request, "selected".into()).await;
    assert!(observer.sq_conns.lock().await.is_empty());
    request
}

async fn tcp_traffic(request: ProxyRequest, observer: &Observer, mut client: TcpStream) {
    let username = request
        .user_context()
        .stats
        .as_ref()
        .unwrap()
        .username
        .clone();
    let ProxyRequest::Tcp(mut session) = request else {
        panic!()
    };
    assert_eq!(observer.get_user_stats(&username).await.tcp_conns, 1);
    let entry = observer.tcp_scoreboard.lock().await[0].0.upgrade().unwrap();
    assert_eq!(entry.outbound, "selected");
    assert_eq!(entry.dst.read().unwrap().to_string(), "192.0.2.1:443");
    client.write_all(b"hello").await.unwrap();
    session.stream.read_exact(&mut [0; 5]).await.unwrap();
    session.stream.write_all(b"world").await.unwrap();
    client.read_exact(&mut [0; 5]).await.unwrap();
    assert_eq!(observer.get_user_stats(&username).await.tcp_recv, 5);
    assert_eq!(observer.get_user_stats(&username).await.tcp_sent, 5);
    assert!(entry.first_send.get().is_some());
    drop(session);
    assert_eq!(observer.get_user_stats(&username).await.tcp_conns, 0);
}

#[tokio::test]
async fn socks_and_mixed_observers_track_authenticated_and_anonymous_tcp() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for kind in KINDS {
            for authenticated in [false, true] {
                let (mut inbound, observer, bind_addr) = inbound(kind, authenticated).await;
                let (client, _) = socks_client(bind_addr, authenticated, false).await;
                let request = tracked(
                    &mut *inbound,
                    &observer,
                    if authenticated { "alice" } else { "" },
                )
                .await;
                tcp_traffic(request, &observer, client).await;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn socks_and_mixed_observers_track_udp_payloads_and_association_lifetimes() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for kind in KINDS {
            for authenticated in [false, true] {
                let (mut inbound, observer, bind_addr) = inbound(kind, authenticated).await;
                let (_control, reply) = socks_client(bind_addr, authenticated, true).await;
                let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let relay: SocketAddr = reply.bind_addr.to_string().parse().unwrap();
                client
                    .send_to(b"\x00\x00\x00\x01\xc0\x00\x02\x01\x01\xbbhello", relay)
                    .await
                    .unwrap();
                let ProxyRequest::Udp(mut session) = tracked(
                    &mut *inbound,
                    &observer,
                    if authenticated { "alice" } else { "" },
                )
                .await
                else {
                    panic!()
                };
                let (payload, destination) = session.recv.recv_from().await.unwrap();
                assert_eq!(payload.as_ref(), b"hello");
                assert_eq!(
                    session
                        .send
                        .send_to(Bytes::from_static(b"world"), destination)
                        .await
                        .unwrap(),
                    5
                );
                let mut packet = [0; 64];
                let (len, _) = client.recv_from(&mut packet).await.unwrap();
                assert_eq!(&packet[10..len], b"world");
                assert_eq!(
                    observer
                        .get_user_stats(if authenticated { "alice" } else { "" })
                        .await
                        .udp_recv,
                    5
                );
                assert_eq!(
                    observer
                        .get_user_stats(if authenticated { "alice" } else { "" })
                        .await
                        .udp_sent,
                    5
                );
                let send = session.send.clone();
                drop(session);
                assert_eq!(
                    observer
                        .get_user_stats(if authenticated { "alice" } else { "" })
                        .await
                        .udp_conns,
                    0
                );
                drop(send);
                assert_eq!(
                    observer
                        .get_user_stats(if authenticated { "alice" } else { "" })
                        .await
                        .udp_conns,
                    0
                );
            }
        }
    })
    .await
    .unwrap();
}

#[cfg(feature = "mixed")]
#[tokio::test]
async fn mixed_http_connect_uses_the_same_observer() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for authenticated in [false, true] {
            let (mut inbound, observer, bind_addr) = inbound("mixed", authenticated).await;
            let mut client = TcpStream::connect(bind_addr).await.unwrap();
            let authorization = if authenticated { "Proxy-Authorization: Basic YWxpY2U6cGFzcw==\r\n" } else { "" };
            client.write_all(format!("CONNECT 192.0.2.1:443 HTTP/1.1\r\nHost: 192.0.2.1:443\r\n{authorization}\r\n").as_bytes()).await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(client.read_u8().await.unwrap());
            }
            assert!(header.starts_with(b"HTTP/1.1 200"));
            let request = tracked(&mut *inbound, &observer, if authenticated { "alice" } else { "" }).await;
            tcp_traffic(request, &observer, client).await;
        }
    }).await.unwrap();
}

#[cfg(feature = "mixed")]
#[tokio::test]
async fn mixed_http_forward_preserves_authenticated_and_anonymous_stats_context() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for authenticated in [false, true] {
            let (mut inbound, observer, bind_addr) = inbound("mixed", authenticated).await;
            let mut client = TcpStream::connect(bind_addr).await.unwrap();
            let authorization = if authenticated {
                "Proxy-Authorization: Basic YWxpY2U6cGFzcw==\r\n"
            } else {
                ""
            };
            client.write_all(format!(
                "GET http://192.0.2.1:8080/path HTTP/1.1\r\nHost: 192.0.2.1:8080\r\n{authorization}\r\n"
            ).as_bytes()).await.unwrap();
            let username = if authenticated { "alice" } else { "" };
            let ProxyRequest::Tcp(mut session) = tracked(&mut *inbound, &observer, username).await else {
                panic!()
            };
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(session.stream.read_u8().await.unwrap());
            }
            assert!(header.starts_with(b"GET /path HTTP/1.1\r\n"));
            assert_eq!(observer.get_user_stats(username).await.tcp_recv, header.len() as u64);
            assert_eq!(observer.get_user_stats(username).await.tcp_conns, 1);
            if authenticated {
                assert!(!observer.user_stats.lock().await.contains_key(""));
                assert!(!observer.user_stats.lock().await.contains_key("bob"));
            }
            drop(session);
            assert_eq!(observer.get_user_stats(username).await.tcp_conns, 0);
        }
    }).await.unwrap();
}
