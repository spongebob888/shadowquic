use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use fast_socks5::client::{Config, Socks5Datagram, Socks5Stream};
use shadowquic::config::{SocksClientCfg, SocksServerCfg};
use shadowquic::socks::outbound::SocksClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tokio::net::TcpStream;
use tokio::time::Duration;

use shadowquic::{Manager, direct::outbound::DirectOut, socks::inbound::SocksServer};

use tracing::{Level, level_filters::LevelFilter, trace};
use tracing::{debug, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::test]
async fn test_tcp() {
    let socks_server = "127.0.0.1:1093";
    let target_addr = "8.8.8.8";
    let target_port = 53;
    let mut config = Config::default();
    config.set_skip_auth(false);
    spawn_socks().await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut s = Socks5Stream::connect(socks_server, target_addr.into(), target_port, config)
        .await
        .unwrap();

    let query = dns_request("www.baidu.com".into());
    let len_byte = (query.len() as u16).to_be_bytes();
    s.write_all(&len_byte).await.unwrap();
    s.write_all(&query).await.unwrap();

    // Read the response length (2 bytes)
    let mut len_buffer = [0u8; 2];
    s.read_exact(&mut len_buffer).await.unwrap();
    let response_len = u16::from_be_bytes(len_buffer) as usize;

    // Read the response
    let mut response = vec![0u8; response_len];
    s.read_exact(&mut response).await.unwrap();
    info!("from tcp 8.8.8.8:53: {:?}", &response[0..response_len]);
    assert_eq!(response[0], 0x13);
    assert_eq!(response[1], 0x37);

    // Creating a SOCKS stream to the target address through the socks server
    let backing_socket = TcpStream::connect(socks_server).await.unwrap();
    // At least on some platforms it is important to use the same protocol as the server
    // XXX: assumes the returned UDP proxy will have the same protocol as the socks_server
    let client_bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

    let socks = Socks5Datagram::bind(backing_socket, client_bind_addr)
        .await
        .unwrap();

    let mut recv = [0u8; 200];
    socks
        .send_to(&query, (target_addr, target_port))
        .await
        .unwrap();
    let (len, _) = socks.recv_from(&mut recv).await.unwrap();
    info!("from udp 8.8.8.8:53: {:?}", &recv[0..len]);
    assert_eq!(recv[0], 0x13);
    assert_eq!(recv[1], 0x37);
}

fn dns_request(domain: String) -> Vec<u8> {
    debug!("Requesting results...");

    let mut query: Vec<u8> = vec![
        0x13, 0x37, // txid
        0x01, 0x00, // flags
        0x00, 0x01, // questions
        0x00, 0x00, // answer RRs
        0x00, 0x00, // authority RRs
        0x00, 0x00, // additional RRs
    ];
    for part in domain.split('.') {
        query.push(part.len() as u8);
        query.extend(part.chars().map(|c| c as u8));
    }
    query.extend_from_slice(&[0, 0, 1, 0, 1]);
    debug!("query: {:?}", query);

    query
    // assert_eq!(msg[0], 0x13);
    // assert_eq!(msg[1], 0x37);
}

async fn spawn_socks() {
    let filter = tracing_subscriber::filter::Targets::new()
        // Enable the `INFO` level for anything in `my_crate`
        .with_target("socks_out", Level::TRACE)
        .with_target("shadowquic", LevelFilter::TRACE);

    // Enable the `DEBUG` level for a specific module.

    // Build a new subscriber with the `fmt` layer using the `Targets`
    // filter we constructed above.
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .try_init();

    // env_logger::init();
    trace!("Running");

    let socks_server = SocksServer::new(SocksServerCfg {
        bind_addr: "127.0.0.1:1093".parse().unwrap(),
    })
    .await
    .unwrap();
    let sq_client = SocksClient::new(SocksClientCfg {
        addr: "[::1]:1094".into(),
    });

    let client = Manager {
        inbound: Box::new(socks_server),
        outbound: Box::new(sq_client),
    };

    let sq_server = SocksServer::new(SocksServerCfg {
        bind_addr: "[::1]:1094".parse().unwrap(),
    })
    .await
    .unwrap();
    let direct_client = DirectOut;
    let server = Manager {
        inbound: Box::new(sq_server),
        outbound: Box::new(direct_client),
    };

    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::spawn(client.run());
    tokio::time::sleep(Duration::from_millis(100)).await;
}
