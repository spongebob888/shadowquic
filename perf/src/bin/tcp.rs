use fast_socks5::client::{Config, Socks5Stream};
use shadowquic::config::{CongestionControl, ShadowQuicClientCfg, ShadowQuicServerCfg, SocksServerCfg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tokio::{net::TcpListener, time::Duration};

use shadowquic::{
    Manager,
    direct::outbound::DirectOut,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::inbound::SocksServer,
};

use tracing::info;
use tracing::{Level, level_filters::LevelFilter, trace};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const CHUNK_LEN: usize = 10 * 1024 * 1024;
const ROUND: usize = 100;
#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    let socks_server = "127.0.0.1:1089";
    let target_addr = "127.0.0.1";
    let target_port = 1445;
    let mut config = Config::default();
    config.set_skip_auth(false);
    test_shadowquic().await;
    tokio::spawn(tcp_peer(1445));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut s = Socks5Stream::connect(socks_server, target_addr.into(), target_port, config)
        .await
        .unwrap();

    let mut sendbuf = vec![0u8; CHUNK_LEN];
    let mut recvbuf = vec![0u8; CHUNK_LEN];
    // let mut s1:TcpStream = s.get_socket();
    let (mut r, mut w) = s.get_socket_mut().split();
    let fut_1 = async move {
        let now = tokio::time::Instant::now();
        for ii in 0..ROUND {
            r.read_exact(&mut recvbuf).await.unwrap();
        }
        let after = tokio::time::Instant::now();
        let dura = after - now;
        eprintln!(
            "average download speed:{} MB/s",
            (CHUNK_LEN * ROUND) as f64 / dura.as_secs_f64() / 1024.0 / 1024.0
        );
    };
    let fut_2 = async move {
        let now = tokio::time::Instant::now();
        for _ in 0..ROUND {
            w.write_all(&mut sendbuf).await.unwrap();
        }
        w.flush().await.unwrap();
        let after = tokio::time::Instant::now();
        let dura = after - now;
        eprintln!(
            "average upload speed:{} MB/s",
            (CHUNK_LEN * ROUND) as f64 / dura.as_secs_f64() / 1024.0 / 1024.0
        );
    };

    //tokio::join!(fut_1,fut_2);
    fut_1.await;
    fut_2.await;
}

async fn test_shadowquic() {
    let filter = tracing_subscriber::filter::Targets::new()
        // Enable the `INFO` level for anything in `my_crate`
        .with_target("tcp", Level::TRACE)
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

    let socks_server = SocksServer::new(SocksServerCfg{bind_addr:"127.0.0.1:1089".parse().unwrap()})
        .await
        .unwrap();
    let sq_client = ShadowQuicClient::new(
        ShadowQuicClientCfg {
            jls_pwd: "123".into(),
            jls_iv: "123".into(),
            addr: "127.0.0.1:4444".parse().unwrap(),
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            zero_rtt: true,
            over_stream: true,
        });

    let client = Manager {
        inbound: Box::new(socks_server),
        outbound: Box::new(sq_client),
    };

    let sq_server = ShadowQuicServer::new(
        ShadowQuicServerCfg {
            bind_addr: "127.0.0.1:4444".parse().unwrap(),
            jls_pwd: "123".into(),
            jls_iv: "123".into(),
            jls_upstream: "localhost:443".into(),
            alpn: vec!["h3".into()],
            zero_rtt: true,
            congestion_control: CongestionControl::Bbr,
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
async fn tcp_peer(port: u16) {
    let lis = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    let (mut s, addr) = lis.accept().await.unwrap();
    info!("accepted");
    let (mut r, mut w) = s.split();

    let mut sendbuf = vec![0u8; CHUNK_LEN];
    let mut recvbuf = vec![0u8; CHUNK_LEN];
    // let mut s1:TcpStream = s.get_socket();

    let fut_1 = async move {
        let now = tokio::time::Instant::now();
        for ii in 0..ROUND {
            r.read_exact(&mut recvbuf).await.unwrap();
        }
        let after = tokio::time::Instant::now();
        let dura = after - now;
        //info!("average download speed:{} MB/s",(len*round) as f64 / dura.as_secs_f64()/1024.0/1024.0);
    };
    let fut_2 = async move {
        let now = tokio::time::Instant::now();
        for _ in 0..ROUND {
            w.write_all(&mut sendbuf).await.unwrap();
        }
        w.flush().await.unwrap();
        let after = tokio::time::Instant::now();
        let dura = after - now;
        //info!("average upload speed:{} MB/s",(len*round) as f64 / dura.as_secs_f64()/1024.0/1024.0);
    };

    tokio::join!(fut_1, fut_2);
}
