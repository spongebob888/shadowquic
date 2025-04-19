use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use fast_socks5::{Result, client::Socks5Datagram};
use shadowquic_lib::{Manager, direct::outbound::DirectOut, socks::inbound::SocksServer};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tracing::{Level, debug, info, level_filters::LevelFilter, trace};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// # How to use it:
///
/// Query by IPv4 address:
///   `$ RUST_LOG=debug cargo run --example udp_client -- --socks-server 127.0.0.1:1337 --username admin --password password -a 8.8.8.8 -d github.com`
///
/// Query by IPv6 address:
///   `$ RUST_LOG=debug cargo run --example udp_client -- --socks-server 127.0.0.1:1337 --username admin --password password -a 2001:4860:4860::8888 -d github.com`
///
/// Query by domain name:
///   `$ RUST_LOG=debug cargo run --example udp_client -- --socks-server 127.0.0.1:1337 --username admin --password password -a dns.google -d github.com`
///
#[derive(Debug)]
struct Opt {
    /// Socks5 server address + port, e.g. `127.0.0.1:1080`
    pub socks_server: SocketAddr,

    /// Target (DNS) server address, e.g. `8.8.8.8`
    pub target_server: String,

    /// Target (DNS) server port, by default 53
    pub target_port: Option<u16>,

    pub query_domain: String,

    pub username: Option<String>,

    pub password: Option<String>,
}

#[tokio::test]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::filter::Targets::new()
        // Enable the `INFO` level for anything in `my_crate`
        .with_target("shadowquic_lib", Level::TRACE)
        .with_target("dns_udp", Level::TRACE)
        .with_target("shadowquic_lib::msgs::socks", LevelFilter::OFF);

    // Enable the `DEBUG` level for a specific module.

    // Build a new subscriber with the `fmt` layer using the `Targets`
    // filter we constructed above.
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .init();

    spawn_socks_server().await;
    spawn_socks_client().await
}

async fn spawn_socks_client() -> Result<()> {
    let opt: Opt = Opt {
        socks_server: "127.0.0.1:1089".parse().unwrap(),
        target_server: String::from("dns.google.com"),
        target_port: None,
        query_domain: String::from("www.gstatic.com"),
        username: None,
        password: None,
    };

    // Creating a SOCKS stream to the target address through the socks server
    let backing_socket = TcpStream::connect(opt.socks_server).await?;
    // At least on some platforms it is important to use the same protocol as the server
    // XXX: assumes the returned UDP proxy will have the same protocol as the socks_server
    let client_bind_addr = if opt.socks_server.is_ipv4() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
    };
    let mut socks = match opt.username {
        Some(username) => {
            Socks5Datagram::bind_with_password(
                backing_socket,
                client_bind_addr,
                &username,
                &opt.password.expect("Please fill the password"),
            )
            .await?
        }

        _ => Socks5Datagram::bind(backing_socket, client_bind_addr).await?,
    };

    // Once socket creation is completed, can start to communicate with the server
    dns_request(
        &mut socks,
        opt.target_server,
        opt.target_port.unwrap_or(53),
        opt.query_domain,
    )
    .await?;

    Ok(())
}

async fn spawn_socks_server() {
    // env_logger::init();
    trace!("Running");

    let socks_server = SocksServer::new("127.0.0.1:1089".parse().unwrap())
        .await
        .unwrap();
    let direct_client = DirectOut;
    let server = Manager {
        inbound: Box::new(socks_server),
        outbound: Box::new(direct_client),
    };
    tokio::spawn(server.run());
}
/// Simple DNS request
async fn dns_request<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut Socks5Datagram<S>,
    server: String,
    port: u16,
    domain: String,
) -> Result<()> {
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

    let _sent = socket.send_to(&query, (&server[..], port)).await?;

    let mut buf = [0u8; 256];
    let (len, adr) = socket.recv_from(&mut buf).await?;
    let msg = &buf[..len];
    info!(
        "response: {:?} from {:?}",
        String::from_utf8(msg.to_vec()),
        adr
    );

    assert_eq!(msg[0], 0x13);
    assert_eq!(msg[1], 0x37);

    Ok(())
}
