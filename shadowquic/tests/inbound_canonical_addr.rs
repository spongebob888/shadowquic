use std::{net::SocketAddr, time::Duration};

use shadowquic::{
    Inbound, ProxyRequest,
    config::SocksServerCfg,
    msgs::{
        SDecode, SEncode,
        socks5::{CmdReply, CmdReq, SocksAddr, UdpReqHeader},
    },
    socks::inbound::SocksServer,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    time::timeout,
};

async fn check_inbound(mixed: bool) {
    timeout(Duration::from_secs(10), async {
        let bind_addr = std::net::TcpListener::bind("[::]:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut inbound: Box<dyn Inbound> = if mixed {
            #[cfg(feature = "mixed")]
            {
                Box::new(
                    shadowquic::mixed::inbound::MixedServer::new(
                        shadowquic::config::MixedServerCfg {
                            tag: "canonical".into(),
                            default_outbound: None,
                            bind_addr,
                            users: vec![],
                        },
                    )
                    .await
                    .unwrap(),
                )
            }
            #[cfg(not(feature = "mixed"))]
            panic!("mixed feature required");
        } else {
            Box::new(
                SocksServer::new(SocksServerCfg {
                    tag: "canonical".into(),
                    default_outbound: None,
                    bind_addr,
                    users: vec![],
                })
                .await
                .unwrap(),
            )
        };
        inbound.init().await.unwrap();

        for client_ip in ["127.0.0.1", "::1"] {
            let server = SocketAddr::new(client_ip.parse().unwrap(), bind_addr.port());
            for (destination, expected) in [
                ("[::ffff:192.0.2.1]:8080", "192.0.2.1:8080"),
                ("[2001:db8::1]:8080", "[2001:db8::1]:8080"),
                ("192.0.2.1:8080", "192.0.2.1:8080"),
                ("example.com:8080", "example.com:8080"),
            ] {
                for mode in 0..if mixed { 4 } else { 2 } {
                    let mut client = TcpStream::connect(server).await.unwrap();
                    let client_addr = client.local_addr().unwrap();
                    let dst: SocksAddr = destination.parse().unwrap();
                    let mut udp = None;
                    if mode < 2 {
                        client.write_all(&[5, 1, 0]).await.unwrap();
                        let mut auth = [0; 2];
                        client.read_exact(&mut auth).await.unwrap();
                        assert_eq!(auth, [5, 0]);
                        CmdReq {
                            version: 5,
                            cmd: if mode == 0 { 1 } else { 3 },
                            rsv: 0,
                            dst: dst.clone(),
                        }
                        .encode(&mut client)
                        .await
                        .unwrap();
                        let reply = CmdReply::decode(&mut client).await.unwrap();
                        assert_eq!(reply.rep, 0);
                        if mode == 1 {
                            let socket = UdpSocket::bind(SocketAddr::new(client_addr.ip(), 0))
                                .await
                                .unwrap();
                            let mut packet = Vec::new();
                            UdpReqHeader {
                                rsv: 0,
                                frag: 0,
                                dst: dst.clone(),
                            }
                            .encode(&mut packet)
                            .await
                            .unwrap();
                            packet.extend_from_slice(b"ping");
                            socket.connect(reply.bind_addr.to_string()).await.unwrap();
                            socket.send(&packet).await.unwrap();
                            udp = Some((socket, packet));
                        }
                    } else {
                        let request = if mode == 2 {
                            format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n\r\n")
                        } else {
                            format!(
                                "GET http://{destination}/ HTTP/1.1\r\nHost: {destination}\r\n\r\n"
                            )
                        };
                        client.write_all(request.as_bytes()).await.unwrap();
                    }
                    let mut req = inbound.accept().await.unwrap();
                    assert_eq!(req.dst().to_string(), expected);
                    match &mut req {
                        ProxyRequest::Tcp(session) => {
                            assert_eq!(session.src_addr, Some(client_addr))
                        }
                        ProxyRequest::Udp(session) => {
                            assert_eq!(session.src_addr, Some(client_addr));
                            let (socket, packet) = udp.unwrap();
                            // Check both the retained first datagram and subsequent ones.
                            socket.send(&packet).await.unwrap();
                            for _ in 0..2 {
                                let (payload, actual) = session.recv.recv_from().await.unwrap();
                                assert_eq!(&payload[..], b"ping");
                                assert_eq!(actual.to_string(), expected);
                            }
                        }
                    }
                }
            }
        }
    })
    .await
    .expect("dual-stack inbound timed out");
}

#[tokio::test]
async fn socks_dual_stack_addresses_are_canonical() {
    check_inbound(false).await;
}

#[cfg(feature = "mixed")]
#[tokio::test]
async fn mixed_dual_stack_addresses_are_canonical() {
    check_inbound(true).await;
}
