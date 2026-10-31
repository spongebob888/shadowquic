mod common;

use std::{net::SocketAddr, sync::Arc};

use fast_socks5::client::Socks5Datagram;
use tokio::{
    net::{TcpStream, UdpSocket},
    time::{Duration, Instant, timeout},
};

const CHUNK_LEN: usize = 1000;
const ROUND: usize = 100_000;
const IDLE_TIMEOUT: Duration = Duration::from_millis(500);

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    timeout(Duration::from_secs(30), benchmark())
        .await
        .expect("UDP direct benchmark timed out");
}

fn report(direction: &str, received: usize, elapsed: Duration) {
    assert!(received > 0, "no UDP packets received");
    eprintln!(
        "average {direction} speed: {:.2} MiB/s; received {received}/{ROUND} packets; loss {:.2}%",
        (received * CHUNK_LEN) as f64 / elapsed.as_secs_f64() / 1048576.0,
        100.0 * (ROUND - received) as f64 / ROUND as f64,
    );
}

async fn benchmark() {
    let peer = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let target = peer.local_addr().unwrap();
    let (proxy_addr, proxy) = common::direct_proxy().await.unwrap();
    let control = TcpStream::connect(proxy_addr).await.unwrap();
    let socks = Socks5Datagram::bind(control, "127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();

    // Establish the association before timing or sending the bulk traffic.
    socks.send_to(b"ready", target).await.unwrap();
    let mut buffer = [0; CHUNK_LEN];
    let (len, client_addr) = peer.recv_from(&mut buffer).await.unwrap();
    assert_eq!(&buffer[..len], b"ready");
    peer.send_to(b"ready", client_addr).await.unwrap();
    let (len, _) = socks.recv_from(&mut buffer).await.unwrap();
    assert_eq!(&buffer[..len], b"ready");

    let upload = async {
        let buffer = [0xa5; CHUNK_LEN];
        for round in 0..ROUND {
            socks.send_to(&buffer, target).await.unwrap();
            if round % 100 == 99 {
                tokio::time::sleep(Duration::from_micros(1)).await;
            }
        }
    };
    let upload_receive = async {
        let mut buffer = [0; CHUNK_LEN + 1];
        let mut received = 0;
        let mut first = None;
        let mut last = Instant::now();
        while received < ROUND {
            let Ok(packet) = timeout(IDLE_TIMEOUT, peer.recv_from(&mut buffer)).await else {
                break;
            };
            let (len, addr) = packet.unwrap();
            assert_eq!(addr, client_addr);
            assert_eq!(len, CHUNK_LEN);
            assert!(buffer[..len].iter().all(|&byte| byte == 0xa5));
            last = Instant::now();
            first.get_or_insert(last);
            received += 1;
        }
        report(
            "upload",
            received,
            last - first.expect("no upload received"),
        );
    };
    let download = async {
        let buffer = [0x5a; CHUNK_LEN];
        for round in 0..ROUND {
            peer.send_to(&buffer, client_addr).await.unwrap();
            if round % 100 == 99 {
                tokio::time::sleep(Duration::from_micros(1)).await;
            }
        }
    };
    let download_receive = async {
        let mut buffer = [0; CHUNK_LEN + 1];
        let mut received = 0;
        let mut first = None;
        let mut last = Instant::now();
        while received < ROUND {
            let Ok(packet) = timeout(IDLE_TIMEOUT, socks.recv_from(&mut buffer)).await else {
                break;
            };
            let (len, _) = packet.unwrap();
            assert_eq!(len, CHUNK_LEN);
            assert!(buffer[..len].iter().all(|&byte| byte == 0x5a));
            last = Instant::now();
            first.get_or_insert(last);
            received += 1;
        }
        report(
            "download",
            received,
            last - first.expect("no download received"),
        );
    };

    // Receive throughout both transfers; exclude the idle drain from the rate.
    tokio::join!(upload, upload_receive, download, download_receive);
    // Cancel while the SOCKS control stream is still alive to avoid an EOF error.
    proxy.abort();
    assert!(proxy.await.unwrap_err().is_cancelled());
}
