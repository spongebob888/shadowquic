//! Shared UDP measurement for the performance executables.

use std::{
    env,
    future::Future,
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use fast_socks5::{client::Socks5Datagram, parse_udp_request};
use shadowquic::Manager;
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::watch,
    task::JoinSet,
    time::{Duration, Instant, sleep_until, timeout},
};

use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

// Count drops without formatting or writing a log line for every lost packet.
struct FullChannelDrops(Arc<AtomicU64>);

impl<S: tracing::Subscriber> Layer<S> for FullChannelDrops {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Visitor(bool);
        impl tracing::field::Visit for Visitor {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "event" && value == "proxy:datagram_dropped" {
                    self.0 = true;
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        let mut visitor = Visitor(false);
        event.record(&mut visitor);
        if visitor.0 {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const PACKET_LEN: usize = 1000;
const DRAIN: Duration = Duration::from_millis(500);
const BENCHMARK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Both,
    Upload,
    Download,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    packets: usize,
    batch: usize,
    pause: Duration,
    direction: Direction,
}

impl Config {
    fn from_env() -> Result<Self> {
        let number = |name: &str, default: usize| -> Result<usize> {
            match env::var(name) {
                Ok(value) => Ok(value.parse()?),
                Err(env::VarError::NotPresent) => Ok(default),
                Err(error) => Err(error.into()),
            }
        };
        let direction = match env::var("PERF_UDP_DIRECTION").as_deref() {
            Err(env::VarError::NotPresent) | Ok("both") => Direction::Both,
            Ok("upload") => Direction::Upload,
            Ok("download") => Direction::Download,
            _ => {
                return Err(io::Error::other(
                    "PERF_UDP_DIRECTION must be both, upload, or download",
                )
                .into());
            }
        };
        let config = Self {
            packets: number("PERF_UDP_PACKETS", 100_000)?,
            batch: number("PERF_UDP_BATCH", 100)?,
            pause: Duration::from_micros(number("PERF_UDP_PAUSE_US", 1)?.try_into()?),
            direction,
        };
        if config.packets == 0 {
            return Err(io::Error::other("PERF_UDP_PACKETS must be positive").into());
        }
        Ok(config)
    }
}

/// Run both managers, measure a warmed-up association, and stop while its SOCKS
/// control stream is still open. Per-packet tracing is opt-in via RUST_LOG.
pub async fn run(client: Manager, server: Manager) -> Result<()> {
    run_managers(vec![client, server]).await
}

/// Run one direct manager or a client/server pair with identical measurements.
pub async fn run_managers(managers: Vec<Manager>) -> Result<()> {
    let drops = Arc::new(AtomicU64::new(0));
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "off".into()),
        ))
        .with(
            FullChannelDrops(drops.clone()).with_filter(tracing_subscriber::filter::filter_fn(
                |metadata| {
                    *metadata.level() == tracing::Level::WARN && metadata.target() == "shadowquic"
                },
            )),
        )
        .init();
    let (stop, stopped) = watch::channel(false);
    let tasks: Vec<_> = managers
        .into_iter()
        .map(|manager| {
            let mut stopped = stopped.clone();
            tokio::spawn(manager.run_until(async move {
                let _ = stopped.changed().await;
            }))
        })
        .collect();

    let result = benchmark("127.0.0.1:1089".parse()?).await;

    stop.send_replace(true);
    for task in tasks {
        task.await??;
    }
    result?.report()?;
    eprintln!(
        "proxy full-channel drops: {}",
        drops.load(Ordering::Relaxed)
    );
    Ok(())
}

/// Keep this value alive until the proxy stops so the SOCKS control connection
/// cannot close the association while its final counters are being collected.
struct Benchmark {
    _association: Arc<Socks5Datagram<TcpStream>>,
    measured: Result<(Option<Measurement>, Option<Measurement>)>,
}

impl Benchmark {
    fn report(self) -> Result<()> {
        let (upload, download) = self.measured?;
        if let Some(upload) = upload {
            upload.report("upload");
        }
        if let Some(download) = download {
            download.report("download");
        }
        Ok(())
    }
}

/// Measure the same warmed-up SOCKS association for direct and QUIC forwarding.
async fn benchmark(proxy_addr: SocketAddr) -> Result<Benchmark> {
    let config = Config::from_env()?;
    eprintln!("UDP benchmark: {config:?}; payload {PACKET_LEN} bytes");
    timeout(BENCHMARK_TIMEOUT, async {
        let peer = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let target = peer.local_addr()?;
        // Use the first successful connection, rather than probing and dropping
        // an extra SOCKS connection. The overall timeout also bounds startup.
        let control = loop {
            match TcpStream::connect(proxy_addr).await {
                Ok(stream) => break stream,
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        let socks =
            Arc::new(Socks5Datagram::bind(control, "127.0.0.1:0".parse::<SocketAddr>()?).await?);

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

        let upload = async {
            if config.direction == Direction::Download {
                return Ok(None);
            }
            measure(
                Endpoint::Socks(socks.clone()),
                Endpoint::Peer(peer.clone()),
                target,
                0xa5,
                config,
            )
            .await
            .map(Some)
        };
        let download = async {
            if config.direction == Direction::Upload {
                return Ok(None);
            }
            measure(
                Endpoint::Peer(peer.clone()),
                Endpoint::Socks(socks.clone()),
                client_addr,
                0x5a,
                config,
            )
            .await
            .map(Some)
        };
        let measured = tokio::try_join!(upload, download);
        Ok(Benchmark {
            _association: socks,
            measured,
        })
    })
    .await?
}

#[derive(Clone)]
enum Endpoint {
    Socks(Arc<Socks5Datagram<TcpStream>>),
    Peer(Arc<UdpSocket>),
}

impl Endpoint {
    async fn send(&self, buffer: &[u8], target: SocketAddr) -> Result<()> {
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
}

trait PacketReceiver: Send {
    fn recv(&mut self, buffer: &mut [u8]) -> impl Future<Output = Result<usize>> + Send;
}

impl PacketReceiver for Endpoint {
    async fn recv(&mut self, buffer: &mut [u8]) -> Result<usize> {
        Ok(match self {
            Self::Socks(socket) => {
                // fast-socks5's recv_from future embeds a 64 KiB buffer. Moving
                // it through select! copies far more than our fixed payload.
                // Retain SOCKS header validation with a bounded receive buffer:
                // 262 bytes covers the largest possible SOCKS5 UDP header.
                let mut packet = [0; PACKET_LEN + 262 + 1];
                let len = socket.get_ref().recv(&mut packet).await?;
                copy_socks_payload(&packet[..len], buffer).await?
            }
            Self::Peer(socket) => socket.recv_from(buffer).await?.0,
        })
    }
}

async fn copy_socks_payload(packet: &[u8], buffer: &mut [u8]) -> Result<usize> {
    let (frag, _, payload) = parse_udp_request(packet).await?;
    if frag != 0 || payload.len() > buffer.len() {
        return Err(io::Error::other("invalid SOCKS UDP benchmark packet").into());
    }
    buffer[..payload.len()].copy_from_slice(payload);
    Ok(payload.len())
}

struct Measurement {
    sent_in: Duration,
    received: Received,
}

impl Measurement {
    fn report(&self, direction: &str) {
        eprintln!(
            "{direction}: send {:.2} MiB/s; receive {:.2} MiB/s; received {}/{} packets; loss {:.2}%; duplicates {}",
            rate(self.received.seen.len() * PACKET_LEN, self.sent_in),
            self.received.rate(),
            self.received.count,
            self.received.seen.len(),
            self.received.loss_percent(),
            self.received.duplicates,
        );
    }
}

async fn measure(
    sender: Endpoint,
    receiver: Endpoint,
    target: SocketAddr,
    marker: u8,
    config: Config,
) -> Result<Measurement> {
    let (finished, completion) = watch::channel(None);
    // Independent tasks keep a continuously ready sender from starving its
    // receiver inside join!, especially when pacing is disabled. JoinSet also
    // cancels both tasks if the benchmark fails or times out.
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        let mut buffer = [marker; PACKET_LEN];
        let start = Instant::now();
        for sequence in 0..config.packets {
            buffer[..8].copy_from_slice(&(sequence as u64).to_be_bytes());
            sender.send(&buffer, target).await?;
            if config.batch > 0
                && (sequence + 1) % config.batch == 0
                && sequence + 1 < config.packets
            {
                if config.pause.is_zero() {
                    tokio::task::yield_now().await;
                } else {
                    tokio::time::sleep(config.pause).await;
                }
            }
        }
        let end = Instant::now();
        finished.send_replace(Some(end));
        Ok(MeasurementPart::Sent(end - start)) as Result<MeasurementPart>
    });
    tasks.spawn(async move {
        receive(receiver, completion, config.packets, marker)
            .await
            .map(MeasurementPart::Received)
    });
    let first = tasks.join_next().await.unwrap()??;
    let second = tasks.join_next().await.unwrap()??;
    let (sent_in, received) = match (first, second) {
        (MeasurementPart::Sent(sent_in), MeasurementPart::Received(received))
        | (MeasurementPart::Received(received), MeasurementPart::Sent(sent_in)) => {
            (sent_in, received)
        }
        _ => unreachable!("one sender and one receiver task"),
    };
    Ok(Measurement { sent_in, received })
}

enum MeasurementPart {
    Sent(Duration),
    Received(Received),
}

async fn receive(
    mut receiver: impl PacketReceiver,
    mut finished: watch::Receiver<Option<Instant>>,
    rounds: usize,
    marker: u8,
) -> Result<Received> {
    let mut received = Received::new(rounds, marker);
    let mut buffer = [0; PACKET_LEN + 1];
    let mut deadline = finished.borrow().map(|end| end + DRAIN);
    while received.count < rounds {
        tokio::select! {
            packet = receiver.recv(&mut buffer) => {
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

    struct ChannelReceiver(tokio::sync::mpsc::UnboundedReceiver<[u8; PACKET_LEN]>);

    impl PacketReceiver for ChannelReceiver {
        async fn recv(&mut self, buffer: &mut [u8]) -> Result<usize> {
            let packet = self.0.recv().await.unwrap();
            buffer[..PACKET_LEN].copy_from_slice(&packet);
            Ok(PACKET_LEN)
        }
    }

    struct PendingReceiver;

    impl PacketReceiver for PendingReceiver {
        async fn recv(&mut self, _: &mut [u8]) -> Result<usize> {
            std::future::pending().await
        }
    }

    fn packet(sequence: u64) -> [u8; PACKET_LEN] {
        let mut packet = [0xa5; PACKET_LEN];
        packet[..8].copy_from_slice(&sequence.to_be_bytes());
        packet
    }

    #[test]
    fn drop_counter_counts_only_full_channel_events() {
        let drops = Arc::new(AtomicU64::new(0));
        let subscriber = tracing_subscriber::registry().with(FullChannelDrops(drops.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(event = "proxy:datagram_dropped", "full association channel");
            tracing::warn!(event = "unrelated", "another warning");
        });
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn bounded_socks_receiver_validates_headers_and_payload_size() {
        for address in ["127.0.0.1:1234", "[::1]:1234"] {
            let mut datagram =
                fast_socks5::new_udp_header(address.parse::<SocketAddr>().unwrap()).unwrap();
            datagram.extend_from_slice(&packet(0));
            let mut buffer = [0; PACKET_LEN + 1];
            let len = copy_socks_payload(&datagram, &mut buffer).await.unwrap();
            assert_eq!(&buffer[..len], &packet(0));
            datagram[2] = 1;
            assert!(copy_socks_payload(&datagram, &mut buffer).await.is_err());
            datagram[2] = 0;
            datagram.extend_from_slice(&[0, 0]);
            assert!(copy_socks_payload(&datagram, &mut buffer).await.is_err());
        }
        assert!(
            copy_socks_payload(b"short", &mut [0; PACKET_LEN + 1])
                .await
                .is_err()
        );
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
        let (send, packets) = tokio::sync::mpsc::unbounded_channel();
        let (finished, completion) = watch::channel(None);
        let recv = receive(ChannelReceiver(packets), completion, 3, 0xa5);
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
        let received = receive(PendingReceiver, completion, 3, 0xa5).await.unwrap();
        assert_eq!(received.count, 0);
        assert_eq!(received.loss_percent(), 100.0);
        assert_eq!(received.rate(), 0.0);
        assert_eq!(Instant::now() - start, DRAIN);
    }
}
