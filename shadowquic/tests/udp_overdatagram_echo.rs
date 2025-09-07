use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use fast_socks5::client::{Config, Socks5Datagram};

use shadowquic::config::{
    AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
    SocksServerCfg, default_initial_mtu,
};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::Duration;

use shadowquic::{
    Manager,
    direct::outbound::DirectOut,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::inbound::SocksServer,
};

use tracing::info;
use tracing::{Level, trace};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const CHUNK_LEN: usize = 1000;
const ROUND: usize = 3;
#[tokio::test]
async fn main() {
    let mut config = Config::default();
    config.set_skip_auth(false);
    test_shadowquic().await;
    tokio::spawn(echo_udp(1448));
    tokio::time::sleep(Duration::from_millis(100)).await;

    const TEST_ROUND: usize = 10;
    for _ii in 0..TEST_ROUND {
        test_udp().await
    }
}

async fn test_udp() {
    let socks_server = "127.0.0.1:1032";
    let target_addr = ("127.0.0.1", 1448);

    let backing_socket = TcpStream::connect(socks_server).await.unwrap();
    let socks = Socks5Datagram::bind(
        backing_socket,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
    )
    .await
    .unwrap();
    let mut sendbuf: Vec<Vec<u8>> = (0..ROUND)
        .map(|_| (0..CHUNK_LEN).map(|_| rand::random()).collect())
        .collect();
    let mut recvbuf: Vec<Vec<u8>> = vec![];
    let mut ii = 0;
    let mut jj = 0;

    let fut = async {
        loop {
            let mut localbuf = vec![0u8; CHUNK_LEN];
            tokio::select! {
                r = async {
                    if ii == ROUND {
                        tokio::time::sleep(Duration::from_millis(2000000)).await;
                    }
                    socks.send_to(&sendbuf[ii], target_addr).await
                 } => {
                    r.unwrap();
                    ii += 1;
                    #[warn(clippy::modulo_one)]
                    if ii % 1 == 0 {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                }
                r = socks.recv_from(&mut localbuf) => {
                    let (len, _addr) = r.unwrap();
                    assert!(len == CHUNK_LEN);
                    recvbuf.push(localbuf);
                    jj += 1;
                    if jj == ROUND {
                        break;
                    }
                }
            }
        }
    };

    tokio::time::timeout(Duration::from_secs(60), fut)
        .await
        .unwrap();

    sendbuf.sort();
    recvbuf.sort();
    assert!(sendbuf == recvbuf);
}

async fn test_shadowquic() {
    let filter = tracing_subscriber::filter::Targets::new()
        // Enable the `INFO` level for anything in `my_crate`
        .with_target("udp", Level::TRACE)
        .with_target("shadowquic", Level::TRACE);

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
        bind_addr: "127.0.0.1:1032".parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();
    let sq_client = ShadowQuicClient::new(ShadowQuicClientCfg {
        password: "123".into(),
        username: "123".into(),
        addr: "127.0.0.1:4449".parse().unwrap(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        zero_rtt: true,
        over_stream: false,
        ..Default::default()
    });

    let client = Manager {
        inbound: Box::new(socks_server),
        outbound: Box::new(sq_client),
    };

    let sq_server = ShadowQuicServer::new(ShadowQuicServerCfg {
        bind_addr: "127.0.0.1:4449".parse().unwrap(),
        users: vec![AuthUser {
            username: "123".into(),
            password: "123".into(),
        }],
        jls_upstream: JlsUpstream {
            addr: "localhost:443".into(),
            ..Default::default()
        },
        alpn: vec!["h3".into()],
        zero_rtt: true,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    })
    .unwrap();
    let direct_client = DirectOut::default();
    let server = Manager {
        inbound: Box::new(sq_server),
        outbound: Box::new(direct_client),
    };

    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::spawn(client.run());
    tokio::time::sleep(Duration::from_millis(100)).await;
}
async fn echo_udp(port: u16) {
    let socks = Arc::new(UdpSocket::bind(("0.0.0.0", port)).await.unwrap());

    let mut recvbuf = vec![0u8; CHUNK_LEN];
    // let mut s1:TcpStream = s.get_socket();

    let socks1 = socks.clone();
    let mut ii = 0;
    loop {
        let (_len, addr) = socks1.recv_from(&mut recvbuf).await.unwrap();
        ii += 1;
        info!("packet number: {}, recv {} bytes from {:?}", ii, _len, addr);
        socks.send_to(&recvbuf, addr).await.unwrap();
    }
}
