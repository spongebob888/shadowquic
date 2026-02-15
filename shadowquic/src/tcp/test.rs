use crate::config::{TcpClientCfg, TcpServerCfg};
use crate::tcp::inbound::TcpServer;
use crate::tcp::outbound::TcpClient;
use crate::{Inbound, Manager, Outbound};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn test_tcp_tfo_chain() {
    // 1. Start Backend Echo Server
    // This server mimics the final destination.
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();
    println!("Backend Echo Server listening on {}", backend_addr);

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_listener.accept().await {
            tokio::spawn(async move {
                let (mut rd, mut wr) = stream.split();
                if let Err(e) = tokio::io::copy(&mut rd, &mut wr).await {
                    eprintln!("Backend echo error: {}", e);
                }
            });
        }
    });

    // 2. Setup ShadowQuic Manager (Inbound -> Outbound)

    // Inbound: TcpServer listening on a random port
    let inbound_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
    let inbound_cfg = TcpServerCfg {
        bind_addr: inbound_addr,
    };
    let server = TcpServer::new(inbound_cfg)
        .await
        .expect("Failed to create TcpServer");
    let server_addr = server.local_addr();
    println!("TcpServer (Inbound) listening on {}", server_addr);

    let inbound = Box::new(server);

    // Outbound: TcpClient pointing to backend_addr
    // The TcpClient is configured to connect to `backend_addr`.
    // This ensures that regardless of the dst in the ProxyRequest, it connects to backend_addr.
    let outbound_cfg = TcpClientCfg {
        addr: backend_addr.to_string(),
    };
    let outbound = Box::new(TcpClient::new(outbound_cfg));

    // Start Manager
    let manager = Manager { inbound, outbound };
    tokio::spawn(async move {
        if let Err(e) = manager.run().await {
            eprintln!("Manager error: {}", e);
        }
    });

    // 3. Test Client -> Inbound
    tokio::time::sleep(Duration::from_millis(100)).await; // Give time for server to start

    // Connect to the Inbound Server
    let mut client = tokio::net::TcpStream::connect(server_addr)
        .await
        .expect("Failed to connect to TcpServer");

    // Send data
    let msg = b"Hello TFO, acting as a transparent proxy to echo server";
    client
        .write_all(msg)
        .await
        .expect("Failed to write to server");

    // Read echo
    let mut buf = vec![0u8; msg.len()];
    client
        .read_exact(&mut buf)
        .await
        .expect("Failed to read from server");

    assert_eq!(&buf, msg, "Echo response mismatch");
    println!("Echo test passed!");
}

#[tokio::test]
async fn test_tcp_tfo_dual_stack() {
    // 1. Start Backend Echo Server
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();
    println!("Backend Echo Server listening on {}", backend_addr);

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_listener.accept().await {
            tokio::spawn(async move {
                let (mut rd, mut wr) = stream.split();
                if let Err(e) = tokio::io::copy(&mut rd, &mut wr).await {
                    eprintln!("Backend echo error: {}", e);
                }
            });
        }
    });

    // 2. Setup ShadowQuic Manager (Inbound -> Outbound)

    // Inbound: TcpServer listening on [::]:0 (IPv6 Any)
    // This should accept IPv4 connections (Dual Stack)
    let inbound_addr = SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0);
    let inbound_cfg = TcpServerCfg {
        bind_addr: inbound_addr,
    };
    let server = TcpServer::new(inbound_cfg)
        .await
        .expect("Failed to create TcpServer");
    let server_addr = server.local_addr(); // This will be [::]:port
    println!("TcpServer (Inbound) listening on {}", server_addr);

    let inbound = Box::new(server);

    // Outbound: TcpClient pointing to backend_addr
    let outbound_cfg = TcpClientCfg {
        addr: backend_addr.to_string(),
    };
    let outbound = Box::new(TcpClient::new(outbound_cfg));

    // Start Manager
    let manager = Manager { inbound, outbound };
    tokio::spawn(async move {
        if let Err(e) = manager.run().await {
            eprintln!("Manager error: {}", e);
        }
    });

    // 3. Test Client -> Inbound
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect using IPv4 address 127.0.0.1 but with the port from the IPv6 bind
    let client_target =
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), server_addr.port());
    println!("Connecting to Inbound via IPv4: {}", client_target);

    let mut client = tokio::net::TcpStream::connect(client_target)
        .await
        .expect("Failed to connect to TcpServer via IPv4");

    // Send data
    let msg = b"Hello Dual Stack TFO";
    client
        .write_all(msg)
        .await
        .expect("Failed to write to server");

    // Read echo
    let mut buf = vec![0u8; msg.len()];
    client
        .read_exact(&mut buf)
        .await
        .expect("Failed to read from server");

    assert_eq!(&buf, msg, "Echo response mismatch");
    println!("Dual Stack Echo test passed!");
}
