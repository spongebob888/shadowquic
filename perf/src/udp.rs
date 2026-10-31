//! Shared UDP measurement for the QUIC performance executables.

use std::{io, net::SocketAddr};

use fast_socks5::client::Socks5Datagram;
use shadowquic::Manager;
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::watch,
    time::{Duration, Instant, sleep_until, timeout},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const PACKET_LEN: usize = 1000;
const ROUND: usize = 100_000;
const DRAIN: Duration = Duration::from_millis(500);
const BENCHMARK_TIMEOUT: Duration = Duration::from_secs(30);

/// Run both managers, measure a warmed-up association, and stop while its SOCKS
/// control stream is still open. Per-packet tracing is opt-in via RUST_LOG.
pub async fn run(client: Manager, server: Manager) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "off".into()),
        )
        .init();
    let (stop, stopped) = watch::channel(false);
    let mut server_stopped = stopped.clone();
    let server = tokio::spawn(server.run_until(async move {
        let _ = server_stopped.changed().await;
    }));
    let mut client_stopped = stopped;
    let client = tokio::spawn(client.run_until(async move {
        let _ = client_stopped.changed().await;
    }));

    let result = timeout(BENCHMARK_TIMEOUT, async {
        let peer = UdpSocket::bind("127.0.0.1:0").await?;
        let target = peer.local_addr()?;
        // Use the first successful connection, rather than probing and dropping
        // an extra SOCKS connection. The overall timeout also bounds startup.
        let control = loop {
            match TcpStream::connect("127.0.0.1:1089").await {
                Ok(stream) => break stream,
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        let socks = Socks5Datagram::bind(control, "127.0.0.1:0".parse::<SocketAddr>()?).await?;

        // Complete a round trip before bulk traffic so authentication, the UDP
        // context, and the return address are established outside the timing.
        socks.send_to(b"ready", target).await?;
        let mut buffer = [0; PACKET_LEN];
        let (len, client_addr) = peer.recv_from(&mut buffer).await?;
        if &buffer[..len] != b"ready" {
            return Err(io::Error::other("invalid UDP warmup packet").into());
        }
        peer.send_to(b"ready", client_addr).await?;
        let (len, _) = socks.recv_from(&mut buffer).await?;
        if &buffer[..len] != b"ready" {
            return Err(io::Error::other("invalid UDP warmup reply").into());
        }

        let upload = measure(Endpoint::Socks(&socks), Endpoint::Peer(&peer), target, 0xa5);
        let download = measure(
            Endpoint::Peer(&peer),
            Endpoint::Socks(&socks),
            client_addr,
            0x5a,
        );
        let measured = tokio::try_join!(upload, download);
        // Keep the association open through manager shutdown, including when
        // measurement fails. Otherwise EOF can race with the final counters.
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((socks, measured))
    })
    .await;

    stop.send_replace(true);
    let (client, server) = tokio::join!(client, server);
    client??;
    server??;
    let (_association, measured) = result??;
    let (upload, download) = measured?;
    upload.report("upload");
    download.report("download");
    Ok(())
}

#[derive(Clone, Copy)]
enum Endpoint<'a> {
    Socks(&'a Socks5Datagram<TcpStream>),
    Peer(&'a UdpSocket),
}

impl Endpoint<'_> {
    async fn send(self, buffer: &[u8], target: SocketAddr) -> Result<()> {
        match self {
            Self::Socks(socket) => {
                socket.send_to(buffer, target).await?;
            }
            Self::Peer(socket) => {
                socket.send_to(buffer, target).await?;
            }
        }
        Ok(())
    }

    async fn recv(self, buffer: &mut [u8]) -> Result<usize> {
        Ok(match self {
            Self::Socks(socket) => socket.recv_from(buffer).await?.0,
            Self::Peer(socket) => socket.recv_from(buffer).await?.0,
        })
    }
}

struct Measurement {
    sent_in: Duration,
    received: Received,
}

impl Measurement {
    fn report(&self, direction: &str) {
        eprintln!(
            "{direction}: send {:.2} MiB/s; receive {:.2} MiB/s; received {}/{} packets; loss {:.2}%; duplicates {}",
            rate(ROUND * PACKET_LEN, self.sent_in),
            self.received.rate(),
            self.received.count,
            ROUND,
            self.received.loss_percent(),
            self.received.duplicates,
        );
    }
}

async fn measure(
    sender: Endpoint<'_>,
    receiver: Endpoint<'_>,
    target: SocketAddr,
    marker: u8,
) -> Result<Measurement> {
    let (finished, completion) = watch::channel(None);
    let send = async {
        let mut buffer = [marker; PACKET_LEN];
        let start = Instant::now();
        for sequence in 0..ROUND {
            buffer[..8].copy_from_slice(&(sequence as u64).to_be_bytes());
            sender.send(&buffer, target).await?;
            if sequence % 100 == 99 && sequence + 1 < ROUND {
                tokio::time::sleep(Duration::from_micros(1)).await;
            }
        }
        let end = Instant::now();
        finished.send_replace(Some(end));
        Ok(end - start) as Result<Duration>
    };
    let recv = receive(
        async |buffer: &mut [u8]| receiver.recv(buffer).await,
        completion,
        ROUND,
        marker,
    );
    let (sent_in, received) = tokio::try_join!(send, recv)?;
    Ok(Measurement { sent_in, received })
}

async fn receive(
    mut recv: impl AsyncFnMut(&mut [u8]) -> Result<usize>,
    mut finished: watch::Receiver<Option<Instant>>,
    rounds: usize,
    marker: u8,
) -> Result<Received> {
    let mut received = Received::new(rounds, marker);
    let mut buffer = [0; PACKET_LEN + 1];
    let mut deadline = finished.borrow().map(|end| end + DRAIN);
    while received.count < rounds {
        tokio::select! {
            packet = recv(&mut buffer) => {
                let len = packet?;
                received.record(&buffer[..len], Instant::now())?;
            }
            changed = finished.changed(), if deadline.is_none() => {
                changed?;
                deadline = finished.borrow().map(|end| end + DRAIN);
            }
            _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => break,
        }
    }
    Ok(received)
}

struct Received {
    seen: Vec<bool>,
    marker: u8,
    count: usize,
    duplicates: usize,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Received {
    fn new(rounds: usize, marker: u8) -> Self {
        Self {
            seen: vec![false; rounds],
            marker,
            count: 0,
            duplicates: 0,
            first: None,
            last: None,
        }
    }

    fn record(&mut self, packet: &[u8], now: Instant) -> Result<()> {
        if packet.len() != PACKET_LEN || packet[8..].iter().any(|&byte| byte != self.marker) {
            return Err(io::Error::other("invalid UDP benchmark payload").into());
        }
        let sequence = u64::from_be_bytes(packet[..8].try_into()?);
        let sequence = usize::try_from(sequence)?;
        let Some(seen) = self.seen.get_mut(sequence) else {
            return Err(io::Error::other("UDP sequence outside benchmark range").into());
        };
        if *seen {
            self.duplicates += 1;
        } else {
            *seen = true;
            self.count += 1;
            self.first.get_or_insert(now);
            self.last = Some(now);
        }
        Ok(())
    }

    fn rate(&self) -> f64 {
        let elapsed = self.first.zip(self.last).map(|(first, last)| last - first);
        rate(self.count * PACKET_LEN, elapsed.unwrap_or_default())
    }

    fn loss_percent(&self) -> f64 {
        100.0 * (self.seen.len() - self.count) as f64 / self.seen.len() as f64
    }
}

fn rate(bytes: usize, elapsed: Duration) -> f64 {
    if elapsed.is_zero() {
        0.0
    } else {
        bytes as f64 / elapsed.as_secs_f64() / 1048576.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(sequence: u64) -> [u8; PACKET_LEN] {
        let mut packet = [0xa5; PACKET_LEN];
        packet[..8].copy_from_slice(&sequence.to_be_bytes());
        packet
    }

    #[test]
    fn reordered_and_duplicate_packets_do_not_inflate_delivery_or_extend_timing() {
        let start = Instant::now();
        let mut received = Received::new(4, 0xa5);
        received.record(&packet(2), start).unwrap();
        received
            .record(&packet(0), start + Duration::from_secs(1))
            .unwrap();
        received
            .record(&packet(2), start + Duration::from_secs(10))
            .unwrap();
        assert_eq!(received.count, 2);
        assert_eq!(received.duplicates, 1);
        assert_eq!(received.loss_percent(), 50.0);
        assert_eq!(received.rate(), 2000.0 / 1048576.0);
    }

    #[test]
    fn no_packets_or_one_packet_produce_finite_rates() {
        let mut received = Received::new(2, 0xa5);
        assert_eq!(received.rate(), 0.0);
        assert_eq!(received.loss_percent(), 100.0);
        received.record(&packet(0), Instant::now()).unwrap();
        assert_eq!(received.rate(), 0.0);
        assert_eq!(received.loss_percent(), 50.0);
    }

    #[test]
    fn malformed_packets_are_rejected() {
        let mut received = Received::new(2, 0xa5);
        assert!(received.record(b"short", Instant::now()).is_err());
        assert!(received.record(&packet(2), Instant::now()).is_err());
        let mut wrong_payload = packet(0);
        wrong_payload[8] = 0;
        assert!(received.record(&wrong_payload, Instant::now()).is_err());
        assert_eq!(received.count, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn receiving_continues_until_sender_finishes_and_drain_is_not_timed() {
        let (send, mut packets) = tokio::sync::mpsc::unbounded_channel();
        let (finished, completion) = watch::channel(None);
        let recv = receive(
            async |buffer: &mut [u8]| {
                let packet: [u8; PACKET_LEN] = packets.recv().await.unwrap();
                buffer[..PACKET_LEN].copy_from_slice(&packet);
                Ok(PACKET_LEN)
            },
            completion,
            3,
            0xa5,
        );
        let start = Instant::now();
        let sender = async {
            send.send(packet(0)).unwrap();
            // A gap longer than the drain must not stop an active sender.
            tokio::time::sleep(DRAIN * 2).await;
            send.send(packet(2)).unwrap();
            finished.send_replace(Some(Instant::now()));
        };
        let (received, ()) = tokio::join!(recv, sender);
        let received = received.unwrap();
        assert_eq!(received.count, 2);
        assert_eq!(received.last.unwrap() - received.first.unwrap(), DRAIN * 2);
        assert_eq!(Instant::now() - start, DRAIN * 3);
        assert_eq!(received.rate(), 2000.0 / 1048576.0);
    }

    #[tokio::test(start_paused = true)]
    async fn complete_loss_terminates_after_sender_completion() {
        let start = Instant::now();
        let (_finished, completion) = watch::channel(Some(start));
        let received = receive(
            async |_: &mut [u8]| std::future::pending::<Result<usize>>().await,
            completion,
            3,
            0xa5,
        )
        .await
        .unwrap();
        assert_eq!(received.count, 0);
        assert_eq!(received.loss_percent(), 100.0);
        assert_eq!(received.rate(), 0.0);
        assert_eq!(Instant::now() - start, DRAIN);
    }
}
