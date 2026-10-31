mod common;

use fast_socks5::client::{Config, Socks5Stream};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{Duration, Instant, timeout},
};

const CHUNK_LEN: usize = 10 * 1024 * 1024;
const ROUND: usize = 100;

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    timeout(Duration::from_secs(60), benchmark())
        .await
        .expect("TCP direct benchmark timed out");
}

async fn benchmark() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let buffer = vec![0x5a; CHUNK_LEN];
        for _ in 0..ROUND {
            stream.write_all(&buffer).await.unwrap();
        }
        stream.shutdown().await.unwrap();
        let mut buffer = vec![0; CHUNK_LEN];
        for _ in 0..ROUND {
            stream.read_exact(&mut buffer).await.unwrap();
            assert!(buffer.iter().all(|&byte| byte == 0xa5));
        }
    });
    let (proxy_addr, proxy) = common::direct_proxy().await.unwrap();
    let mut stream = Socks5Stream::connect(
        proxy_addr,
        target.ip().to_string(),
        target.port(),
        Config::default(),
    )
    .await
    .unwrap();
    let mut buffer = vec![0; CHUNK_LEN];
    let start = Instant::now();
    for _ in 0..ROUND {
        stream.read_exact(&mut buffer).await.unwrap();
        assert!(buffer.iter().all(|&byte| byte == 0x5a));
    }
    eprintln!(
        "average download speed: {:.2} MiB/s",
        (CHUNK_LEN * ROUND) as f64 / start.elapsed().as_secs_f64() / 1048576.0
    );
    buffer.fill(0xa5);
    let start = Instant::now();
    for _ in 0..ROUND {
        stream.write_all(&buffer).await.unwrap();
    }
    stream.shutdown().await.unwrap();
    // Include delivery to the peer, rather than just local socket acceptance.
    peer.await.unwrap();
    eprintln!(
        "average upload speed: {:.2} MiB/s",
        (CHUNK_LEN * ROUND) as f64 / start.elapsed().as_secs_f64() / 1048576.0
    );
    proxy.await.unwrap().unwrap();
}
