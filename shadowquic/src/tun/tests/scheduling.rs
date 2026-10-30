use super::*;

#[tokio::test]
async fn queued_syn_burst_is_drained_without_further_input() {
    let mut harness = Harness::new(config());
    let destination = "198.51.100.1:443".parse().unwrap();
    // More than one batch and more than the stack's packet queue capacity.
    // Queue everything before letting the stack run on this current-thread runtime.
    for port in 40000..40160 {
        let client = SocketAddr::from(([192, 0, 2, 1], port));
        harness
            .input
            .try_send(tcp_packet(client, destination, TcpControl::Syn, 100, None, &[]).into())
            .unwrap();
    }
    let mut ports = std::collections::HashSet::new();
    for _ in 0..160 {
        let packet = harness.receive().await;
        let ip = Packet::parse(&packet).unwrap();
        let tcp = TcpPacket::new_checked(ip.payload).unwrap();
        assert!(tcp.syn() && tcp.ack());
        assert!(ports.insert(tcp.dst_port()), "duplicate SYN reply");
    }
    assert_eq!(ports, (40000..40160).collect());
}

#[tokio::test]
async fn reset_and_reconnect_in_one_batch_reuses_the_session_slot() {
    let mut cfg = config();
    cfg.max_sessions = 1;
    let mut harness = Harness::new(cfg);
    let client = "192.0.2.1:40000".parse().unwrap();
    let destination = "198.51.100.1:443".parse().unwrap();
    let ack = connect(&mut harness, client, destination).await;
    let old = harness.accept().await;
    harness
        .input
        .try_send(tcp_packet(client, destination, TcpControl::Rst, 101, Some(ack), &[]).into())
        .unwrap();
    let ack = connect(&mut harness, client, destination).await;
    let ProxyRequest::Tcp(mut session) = harness.accept().await else {
        panic!()
    };
    harness
        .input
        .send(tcp_packet(client, destination, TcpControl::Psh, 101, Some(ack), b"new").into())
        .await
        .unwrap();
    let mut data = [0; 3];
    tokio::time::timeout(Duration::from_secs(2), session.stream.read_exact(&mut data))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&data, b"new");
    drop(old);
}
