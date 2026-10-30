//! Opt-in measurements of the packet stack, without a kernel TUN device.

use super::*;
use std::time::Instant;

const PAYLOAD_SIZE: usize = 1200;
const UDP_BATCH: usize = 32;
const ADDRESSES: [(&str, &str); 2] = [
    ("192.0.2.10:45000", "198.51.100.20:443"),
    ("[2001:db8::10]:45000", "[2001:db8:1::20]:443"),
];

fn transfer_size() -> usize {
    let bytes = std::env::var("SHADOWQUIC_TUN_BENCH_BYTES")
        .map(|value| {
            value
                .parse::<usize>()
                .expect("invalid benchmark byte count")
        })
        .unwrap_or(16 * 1024 * 1024);
    assert!((1..=1024 * 1024 * 1024).contains(&bytes));
    bytes
}

fn report(protocol: &str, client: SocketAddr, bytes: usize, elapsed: Duration) {
    println!(
        "TUN stack {protocol} IPv{}: {bytes} verified echo bytes in {:.3}s, {:.2} Mbps",
        if client.is_ipv4() { 4 } else { 6 },
        elapsed.as_secs_f64(),
        bytes as f64 * 8.0 / elapsed.as_secs_f64() / 1_000_000.0,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "throughput measurement; run in release mode with --ignored --nocapture"]
async fn tcp_stack_throughput() {
    let total = transfer_size();
    let payload: Vec<u8> = (0..total).map(|offset| (offset % 251) as u8).collect();
    for (client, destination) in ADDRESSES {
        let client = client.parse().unwrap();
        let destination = destination.parse().unwrap();
        let mut harness = Harness::new(config());
        let server_seq = connect(&mut harness, client, destination).await;
        let ProxyRequest::Tcp(session) = harness.accept().await else {
            panic!("expected TCP session")
        };
        // Drive the application and packet peer concurrently, exercising the
        // bounded bridges in both directions rather than waiting per segment.
        let echo = async {
            let (mut reader, mut writer) = tokio::io::split(session.stream);
            let copied = tokio::io::copy(&mut reader, &mut writer).await.unwrap();
            writer.shutdown().await.unwrap();
            assert_eq!(copied, total as u64);
        };
        let peer = async {
            let mut sent = 0;
            let mut acknowledged = 0;
            let mut received = 0;
            let mut window = PAYLOAD_SIZE;
            let mut fin_sent = false;
            let mut next_server_seq = TcpSeqNumber(server_seq);
            let start = Instant::now();
            loop {
                let available = window.saturating_sub(sent - acknowledged);
                tokio::select! {
                    permit = harness.input.reserve(), if sent < total && available > 0 => {
                        let end = (sent + PAYLOAD_SIZE.min(available)).min(total);
                        permit.unwrap().send(tcp_packet(
                            client, destination, TcpControl::Psh,
                            101 + sent as i32, Some(next_server_seq.0), &payload[sent..end],
                        ).into());
                        sent = end;
                    }
                    packet = harness.output.recv() => {
                        let packet = packet.expect("packet stack stopped");
                        let ip = Packet::parse(&packet).unwrap();
                        assert_eq!(std::net::IpAddr::from(ip.src), destination.ip());
                        assert_eq!(std::net::IpAddr::from(ip.dst), client.ip());
                        let tcp = TcpPacket::new_checked(ip.payload).unwrap();
                        let repr = TcpRepr::parse(
                            &tcp, &ip.src, &ip.dst, &ChecksumCapabilities::default(),
                        ).unwrap();
                        assert_ne!(repr.control, TcpControl::Rst);
                        assert_eq!((repr.src_port, repr.dst_port), (destination.port(), client.port()));
                        if let Some(ack) = repr.ack_number {
                            let count = (ack.0.wrapping_sub(101) as usize).min(total);
                            assert!(count <= sent, "acknowledged unsent bytes");
                            if count >= acknowledged {
                                acknowledged = count;
                                window = usize::from(repr.window_len);
                            }
                        }
                        if !repr.payload.is_empty() {
                            assert!(repr.seq_number <= next_server_seq, "out-of-order echo");
                            // Ignore bytes already verified if a segment was retransmitted.
                            let skip = (next_server_seq - repr.seq_number).min(repr.payload.len());
                            let data = &repr.payload[skip..];
                            assert!(received + data.len() <= total);
                            assert_eq!(data, &payload[received..received + data.len()]);
                            received += data.len();
                            next_server_seq = next_server_seq + data.len();
                        }
                        let finished = repr.control == TcpControl::Fin;
                        if finished {
                            assert_eq!(received, total, "TCP echo ended early");
                            next_server_seq = next_server_seq + 1;
                        }
                        if !repr.payload.is_empty() || finished {
                            harness.input.send(tcp_packet(
                                client, destination, TcpControl::None,
                                101 + sent as i32 + i32::from(fin_sent),
                                Some(next_server_seq.0), &[],
                            ).into()).await.unwrap();
                        }
                        if finished {
                            break;
                        }
                    }
                }
                if acknowledged == total && !fin_sent {
                    harness
                        .input
                        .send(
                            tcp_packet(
                                client,
                                destination,
                                TcpControl::Fin,
                                101 + sent as i32,
                                Some(next_server_seq.0),
                                &[],
                            )
                            .into(),
                        )
                        .await
                        .unwrap();
                    fin_sent = true;
                }
            }
            report("TCP", client, received, start.elapsed());
        };
        tokio::time::timeout(Duration::from_secs(120), async {
            tokio::join!(echo, peer);
        })
        .await
        .expect("TCP throughput test stalled");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "throughput measurement; run in release mode with --ignored --nocapture"]
async fn udp_stack_throughput() {
    let total = transfer_size();
    for (client, destination) in ADDRESSES {
        let client = client.parse().unwrap();
        let destination = destination.parse().unwrap();
        let mut harness = Harness::new(config());
        // Establish the session before measuring steady-state throughput.
        harness
            .input
            .send(
                udp_packet(client, destination, b"warmup", 1500)
                    .unwrap()
                    .into(),
            )
            .await
            .unwrap();
        let ProxyRequest::Udp(mut session) = harness.accept().await else {
            panic!("expected UDP session")
        };
        let (data, dst) = session.recv.recv_from().await.unwrap();
        session.send.send_to(data, dst).await.unwrap();
        harness.receive().await;

        let measure = async {
            let start = Instant::now();
            let mut transferred = 0;
            let mut sequence = 0_u64;
            let mut payload = [0x5a; PAYLOAD_SIZE];
            while transferred < total {
                let mut batch = Vec::with_capacity(UDP_BATCH);
                // Stay below the session queue capacity, so overload cannot
                // turn an expected reply into an unbounded wait.
                for _ in 0..UDP_BATCH {
                    if transferred == total {
                        break;
                    }
                    let len = PAYLOAD_SIZE.min(total - transferred);
                    payload[..8].copy_from_slice(&sequence.to_be_bytes());
                    let data = Bytes::copy_from_slice(&payload[..len]);
                    harness
                        .input
                        .send(udp_packet(client, destination, &data, 1500).unwrap().into())
                        .await
                        .unwrap();
                    batch.push(data);
                    transferred += len;
                    sequence += 1;
                }
                for expected in &batch {
                    let (data, dst) = session.recv.recv_from().await.unwrap();
                    assert_eq!(dst, destination.into());
                    assert_eq!(&data, expected);
                    session.send.send_to(data, dst).await.unwrap();
                }
                for expected in &batch {
                    let reply = harness.receive().await;
                    let ip = Packet::parse(&reply).unwrap();
                    assert_eq!(std::net::IpAddr::from(ip.src), destination.ip());
                    assert_eq!(std::net::IpAddr::from(ip.dst), client.ip());
                    let udp = UdpPacket::new_checked(ip.payload).unwrap();
                    let repr =
                        UdpRepr::parse(&udp, &ip.src, &ip.dst, &ChecksumCapabilities::default())
                            .unwrap();
                    assert_eq!(
                        (repr.src_port, repr.dst_port),
                        (destination.port(), client.port())
                    );
                    assert_eq!(udp.payload(), expected.as_ref());
                }
            }
            report("UDP", client, transferred, start.elapsed());
        };
        tokio::time::timeout(Duration::from_secs(120), measure)
            .await
            .expect("UDP throughput test stalled");
    }
}
