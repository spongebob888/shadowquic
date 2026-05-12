#![cfg(all(target_os = "linux", feature = "tproxy"))]

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tracing::{info, Level};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use shadowquic::config::TproxyServerCfg;
use shadowquic::error::SError;
use shadowquic::tproxy::inbound::TproxyServer;
use shadowquic::{Manager, Outbound, ProxyRequest};
use async_trait::async_trait;

#[derive(Default, Clone)]
pub struct EchoOutbound;

#[async_trait]
impl Outbound for EchoOutbound {
    async fn handle(&mut self, req: ProxyRequest) -> Result<(), SError> {
        match req {
            ProxyRequest::Tcp(mut session) => {
                tokio::spawn(async move {
                    let (mut r, mut w) = tokio::io::split(&mut session.stream);
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
            ProxyRequest::Udp(mut session) => {
                tokio::spawn(async move {
                    loop {
                        match session.recv.recv_from().await {
                            Ok((buf, addr)) => {
                                let _ = session.send.send_to(buf, addr).await;
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
        }
        Ok(())
    }
}

const CHUNK_LEN: usize = 1024;
const ROUND: usize = 10;

#[tokio::test]
async fn test_tproxy_echo() {
    let filter = tracing_subscriber::filter::Targets::new()
        .with_target("tproxy_echo", Level::TRACE)
        .with_target("shadowquic", Level::TRACE);

    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .try_init();

    let tproxy_server = TproxyServer::new(TproxyServerCfg {
        bind_addr: "0.0.0.0:1089".parse().unwrap(),
    })
    .await
    .unwrap();

    let echo_outbound = EchoOutbound::default();

    let manager = Manager {
        inbound: Box::new(tproxy_server),
        outbound: Box::new(echo_outbound),
    };

    tokio::spawn(manager.run());
    
    // Wait for the tproxy server to start listening
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Test TCP
    info!("Starting TCP Echo Test");
    let mut tcp_stream = TcpStream::connect("10.255.255.1:8080").await.unwrap();
    let sendbuf: Vec<u8> = (0..(CHUNK_LEN * ROUND)).map(|_| rand::random()).collect();
    let mut recvbuf = vec![0u8; CHUNK_LEN * ROUND];
    
    let (mut r, mut w) = tcp_stream.split();
    
    let fut_1 = async {
        for ii in 0..ROUND {
            r.read_exact(&mut recvbuf[ii * CHUNK_LEN..(ii + 1) * CHUNK_LEN])
                .await
                .unwrap();
        }
    };
    let fut_2 = async {
        for ii in 0..ROUND {
            w.write_all(&sendbuf[ii * CHUNK_LEN..(ii + 1) * CHUNK_LEN])
                .await
                .unwrap();
        }
        w.flush().await.unwrap();
    };
    tokio::join!(fut_1, fut_2);
    assert_eq!(sendbuf, recvbuf);
    info!("TCP Echo successful");

    // Test UDP
    info!("Starting UDP Echo Test");
    let udp_socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    udp_socket.connect("10.255.255.1:8080").await.unwrap();
    
    let sendbuf_udp: Vec<u8> = (0..1024).map(|_| rand::random()).collect();
    udp_socket.send(&sendbuf_udp).await.unwrap();
    
    let mut recvbuf_udp = vec![0u8; 1024];
    let len = udp_socket.recv(&mut recvbuf_udp).await.unwrap();
    assert_eq!(len, 1024);
    assert_eq!(&sendbuf_udp[..], &recvbuf_udp[..len]);
    info!("UDP Echo successful");
}
