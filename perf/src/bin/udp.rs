use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use fast_socks5::client::{Config, Socks5Datagram};
use fast_socks5::util::target_addr::TargetAddr;

use shadowquic::config::{
    CongestionControl, ShadowQuicClientCfg, ShadowQuicServerCfg, SocksServerCfg,
    default_initial_mtu,
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
use tracing::{Level, level_filters::LevelFilter, trace};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const CHUNK_LEN: usize = 1000;
const ROUND: usize = 100000; //1000*100;
#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    let socks_server = "127.0.0.1:1089";
    let target_addr = ("127.0.0.1", 1445);
    let mut config = Config::default();
    config.set_skip_auth(false);
    test_shadowquic().await;
    tokio::spawn(echo_tcp(1445));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let backing_socket = TcpStream::connect(socks_server).await.unwrap();
    let socks = Socks5Datagram::bind(
        backing_socket,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
    )
    .await
    .unwrap();
    let sendbuf = vec![0u8; CHUNK_LEN];
    let mut recvbuf = vec![0u8; CHUNK_LEN];

    let fut_2 = async {
        let now = tokio::time::Instant::now();
        for ii in 0..ROUND {
            socks.send_to(&sendbuf, target_addr).await.unwrap();
            if ii % 100 == 0 {
                tokio::time::sleep(tokio::time::Duration::from_micros(1)).await;
            }
        }
        let after = tokio::time::Instant::now();
        let dura = after - now;
        info!(
            "average local send speed:{} MB/s",
            (ROUND * CHUNK_LEN) as f64 / dura.as_secs_f64() / 1024.0 / 1024.0
        );
    };

    fut_2.await;
    // let mut s1:TcpStream = s.get_socket();
    let fut_1 = async move {
        let now = tokio::time::Instant::now();
        let mut addr: TargetAddr =
            TargetAddr::Ip(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
        let mut total = 0;
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            let mut len;
            for _ii in 0..ROUND {
                (len, addr) = socks.recv_from(&mut recvbuf).await.unwrap();
                total += len;
            }
        })
        .await;
        let after = tokio::time::Instant::now();
        let dura = after - now;
        info!(
            "average local recv speed:{} MB/s",
            (total) as f64 / dura.as_secs_f64() / 1024.0 / 1024.0
        );
        addr
    };

    //tokio::join!(fut_1,fut_2);

    fut_1.await;
}

async fn test_shadowquic() {
    let filter = tracing_subscriber::filter::Targets::new()
        // Enable the `INFO` level for anything in `my_crate`
        .with_target("udp", Level::TRACE)
        .with_target("shadowquic", Level::INFO)
        .with_target("shadowquic::msgs::socks", LevelFilter::OFF);

    // Enable the `DEBUG` level for a specific module.

    // Build a new subscriber with the `fmt` layer using the `Targets`
    // filter we constructed above.
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    // env_logger::init();
    trace!("Running");

    let socks_server = SocksServer::new(SocksServerCfg {
        bind_addr: "127.0.0.1:1089".parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();
    let sq_client = ShadowQuicClient::new(ShadowQuicClientCfg {
        password: "123".into(),
        username: "123".into(),
        addr: "127.0.0.1:4444".parse().unwrap(),
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
        bind_addr: "127.0.0.1:4444".parse().unwrap(),
        jls_pwd: "123".into(),
        jls_iv: "123".into(),
        jls_upstream: "localhost:443".into(),
        alpn: vec!["h3".into()],
        zero_rtt: true,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    })
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
async fn echo_tcp(port: u16) {
    let socks = Arc::new(UdpSocket::bind(("0.0.0.0", port)).await.unwrap());

    let sendbuf = vec![0u8; CHUNK_LEN];
    let mut recvbuf = vec![0u8; CHUNK_LEN];
    // let mut s1:TcpStream = s.get_socket();

    let socks1 = socks.clone();
    let fut_1 = async move {
        let now = tokio::time::Instant::now();
        let mut addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let mut total = 0;

        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            for _ii in 0..ROUND {
                let len;
                (len, addr) = socks1.recv_from(&mut recvbuf).await.unwrap();
                total += len;
            }
        })
        .await;

        let after = tokio::time::Instant::now();
        let dura = after - now;
        info!(
            "average peer recv speed:{} MB/s",
            (total) as f64 / dura.as_secs_f64() / 1024.0 / 1024.0
        );
        addr
    };
    let addr = fut_1.await;
    let fut_2 = async move {
        let now = tokio::time::Instant::now();
        for ii in 0..ROUND {
            socks.send_to(&sendbuf, addr).await.unwrap();
            if ii % 100 == 0 {
                tokio::time::sleep(tokio::time::Duration::from_micros(1)).await;
            }
        }
        let after = tokio::time::Instant::now();
        let dura = after - now;
        info!(
            "average peer send speed:{} MB/s",
            (CHUNK_LEN * ROUND) as f64 / dura.as_secs_f64() / 1024.0 / 1024.0
        );
    };

    fut_2.await;
}
