use super::*;
use crate::{Outbound, RequestDispatcher, Stoppable, TcpSession, UdpSession, UserContext};
use std::{future::Future, task::Waker};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

struct TestConnection;

impl Stoppable for TestConnection {
    fn stop(&self) {}
}

fn user_context(conn: &Arc<dyn Stoppable>, conn_id: u64) -> UserContext {
    UserContext {
        stats: Some(StatsContext {
            username: "alice".into(),
            conn_handle: Arc::downgrade(conn),
            conn_id,
        }),
        ..Default::default()
    }
}

fn addr() -> SocksAddr {
    "192.0.2.1:443".parse::<SocketAddr>().unwrap().into()
}

struct TestTcp {
    fail: bool,
}

impl AsyncRead for TestTcp {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.fail {
            return Poll::Ready(Err(io::Error::other("read failed")));
        }
        buf.put_slice(b"read");
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for TestTcp {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.fail {
            return Poll::Ready(Err(io::Error::other("write failed")));
        }
        Poll::Ready(Ok(buf.len().min(3)))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_write(cx, bufs.first().map_or(&[], |buf| &**buf))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl TcpTrait for TestTcp {
    fn peer_addr(&self) -> Option<SocketAddr> {
        Some("192.0.2.2:1234".parse().unwrap())
    }
}

fn tcp_request(user_context: UserContext, fail: bool) -> ProxyRequest {
    ProxyRequest::Tcp(TcpSession {
        stream: Box::new(TestTcp { fail }),
        dst: addr(),
        src_addr: Some("192.0.2.2:1234".parse().unwrap()),
        user_context,
    })
}

type Packet = (Bytes, SocksAddr);

fn udp_request(
    user_context: UserContext,
) -> (ProxyRequest, mpsc::Sender<Packet>, mpsc::Receiver<Packet>) {
    let (send_request, recv_request) = mpsc::channel(2);
    let (send_reply, recv_reply) = mpsc::channel(2);
    (
        ProxyRequest::Udp(UdpSession {
            recv: Box::new(recv_request),
            send: Arc::new(send_reply),
            stream: None,
            bind_addr: addr(),
            dst: addr(),
            src_addr: None,
            user_context,
        }),
        send_request,
        recv_reply,
    )
}

#[tokio::test]
async fn tcp_tracks_partial_and_vectored_io_per_session_and_per_user() {
    let observer = Observer::new();
    let conn: Arc<dyn Stoppable> = Arc::new(TestConnection);
    let context = user_context(&conn, 1);
    let request = observer
        .wrap_request(tcp_request(context.clone(), false), "selected".into())
        .await;
    let second = observer
        .wrap_request(tcp_request(context, false), "selected".into())
        .await;
    let ProxyRequest::Tcp(mut tcp) = request else {
        unreachable!()
    };
    let (send, recv) = observer.tcp_scoreboard.lock().await[0].clone();
    {
        let send = send.upgrade().unwrap();
        let recv = recv.upgrade().unwrap();
        assert_eq!(recv.src_addr, tcp.src_addr);
        assert_eq!(*send.dst.read().unwrap(), tcp.dst);
        assert!(recv.created.elapsed() < Duration::from_secs(5));
        assert!(send.first_send.get().is_none());
        assert_eq!(send.outbound, "selected");
    }
    assert_eq!(tcp.stream.peer_addr(), tcp.src_addr);
    assert!(tcp.stream.is_write_vectored());
    assert_eq!(tcp.stream.write(b"long write").await.unwrap(), 3);
    assert!(send.upgrade().unwrap().first_send.get().is_some());
    assert_eq!(
        tcp.stream
            .write_vectored(&[IoSlice::new(b"abcdef"), IoSlice::new(b"ghi")])
            .await
            .unwrap(),
        3
    );
    // The wrapper must count only bytes appended to an already filled buffer.
    let mut buf = [0; 16];
    let mut read_buf = ReadBuf::new(&mut buf);
    read_buf.put_slice(b"old");
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(
        Pin::new(&mut tcp.stream).poll_read(&mut cx, &mut read_buf),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(read_buf.filled(), b"oldread");
    assert_eq!(
        send.upgrade().unwrap().bytes_send.load(Ordering::Relaxed),
        6
    );
    assert_eq!(
        recv.upgrade().unwrap().bytes_recv.load(Ordering::Relaxed),
        4
    );
    let scoreboard = observer.tcp_scoreboard.lock().await;
    assert_eq!(scoreboard.len(), 2);
    assert_eq!(
        scoreboard[1]
            .0
            .upgrade()
            .unwrap()
            .bytes_send
            .load(Ordering::Relaxed),
        0
    );
    drop(scoreboard);
    let stats = observer.get_user_stats("alice").await;
    assert_eq!(
        (
            stats.tcp_sent,
            stats.tcp_recv,
            stats.tcp_conns,
            stats.conn_num
        ),
        (6, 4, 2, 1)
    );

    observer.clear_user_stats("alice").await;
    let stats = observer.get_user_stats("alice").await;
    assert_eq!((stats.tcp_sent, stats.tcp_recv, stats.tcp_conns), (0, 0, 2));
    assert_eq!(
        send.upgrade().unwrap().bytes_send.load(Ordering::Relaxed),
        6
    );
    tcp.stream.write_all(b"ok").await.unwrap();
    assert_eq!(observer.get_user_stats("alice").await.tcp_sent, 2);
    drop(tcp);
    assert!(send.upgrade().is_none());
    assert!(recv.upgrade().is_none());
    assert_eq!(observer.get_user_stats("alice").await.tcp_conns, 1);
    drop(second);
    assert_eq!(observer.get_user_stats("alice").await.tcp_conns, 0);
}

#[tokio::test]
async fn failed_tcp_io_does_not_increment_traffic() {
    let observer = Observer::new();
    let conn: Arc<dyn Stoppable> = Arc::new(TestConnection);
    let request = observer
        .wrap_request(tcp_request(user_context(&conn, 1), true), "selected".into())
        .await;
    let ProxyRequest::Tcp(mut tcp) = request else {
        unreachable!()
    };
    assert!(tcp.stream.read(&mut [0; 16]).await.is_err());
    assert!(tcp.stream.write(b"fail").await.is_err());
    assert!(
        tcp.stream
            .write_vectored(&[IoSlice::new(b"fail")])
            .await
            .is_err()
    );
    let scoreboard = observer.tcp_scoreboard.lock().await;
    assert_eq!(
        scoreboard[0]
            .0
            .upgrade()
            .unwrap()
            .bytes_send
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        scoreboard[0]
            .1
            .upgrade()
            .unwrap()
            .bytes_recv
            .load(Ordering::Relaxed),
        0
    );
    let stats = observer.get_user_stats("alice").await;
    assert_eq!((stats.tcp_sent, stats.tcp_recv), (0, 0));
}

#[tokio::test]
async fn udp_tracks_first_packet_errors_and_independent_half_lifetimes() {
    let observer = Observer::new();
    let conn: Arc<dyn Stoppable> = Arc::new(TestConnection);
    let (request, send_request, mut recv_reply) = udp_request(user_context(&conn, 1));
    send_request
        .send((Bytes::from_static(b"first"), addr()))
        .await
        .unwrap();
    let ProxyRequest::Udp(udp) = request else {
        unreachable!()
    };
    let request = observer
        .wrap_request(
            ProxyRequest::Udp(udp.wait_first_packet().await.unwrap()),
            "selected".into(),
        )
        .await;
    let ProxyRequest::Udp(mut udp) = request else {
        unreachable!()
    };
    let (send, recv) = observer.udp_scoreboard.lock().await[0].clone();
    assert_eq!(recv.upgrade().unwrap().src_addr, None);
    assert_eq!(udp.recv.recv_from().await.unwrap().0, b"first"[..]);
    send_request
        .send((Bytes::from_static(b"next"), addr()))
        .await
        .unwrap();
    assert_eq!(udp.recv.recv_from().await.unwrap().0, b"next"[..]);
    let cloned_send = udp.send.clone();
    assert_eq!(
        cloned_send
            .send_to(Bytes::from_static(b"reply"), addr())
            .await
            .unwrap(),
        5
    );
    assert_eq!(recv_reply.recv().await.unwrap().0, b"reply"[..]);
    drop(send_request);
    drop(recv_reply);
    assert!(udp.recv.recv_from().await.is_err());
    assert!(
        udp.send
            .send_to(Bytes::from_static(b"failed"), addr())
            .await
            .is_err()
    );
    assert_eq!(
        send.upgrade().unwrap().bytes_send.load(Ordering::Relaxed),
        5
    );
    assert_eq!(
        recv.upgrade().unwrap().bytes_recv.load(Ordering::Relaxed),
        9
    );
    let stats = observer.get_user_stats("alice").await;
    assert_eq!((stats.udp_sent, stats.udp_recv, stats.udp_conns), (5, 9, 1));
    drop(udp);
    assert_eq!(observer.get_user_stats("alice").await.udp_conns, 0);
    assert!(recv.upgrade().is_none());
    assert!(send.upgrade().is_some());
    *observer.last_cleanup.lock().await = Instant::now() - CLEANUP_INTERVAL;
    observer.cleanup().await;
    assert_eq!(observer.udp_scoreboard.lock().await.len(), 1);
    drop(cloned_send);
    assert!(send.upgrade().is_none());
    assert!(recv.upgrade().is_none());
    assert_eq!(observer.get_user_stats("alice").await.udp_conns, 0);
    *observer.last_cleanup.lock().await = Instant::now() - CLEANUP_INTERVAL;
    observer.cleanup().await;
    assert!(observer.udp_scoreboard.lock().await.is_empty());
}

#[tokio::test]
async fn cleanup_is_shared_throttled_and_preserves_live_sessions() {
    let observer = Observer::new();
    let clone = observer.clone();
    let conn: Arc<dyn Stoppable> = Arc::new(TestConnection);
    let live_conn: Arc<dyn Stoppable> = Arc::new(TestConnection);
    let expired = observer
        .wrap_request(
            tcp_request(user_context(&conn, 1), false),
            "selected".into(),
        )
        .await;
    let live = observer
        .wrap_request(
            tcp_request(user_context(&live_conn, 2), false),
            "selected".into(),
        )
        .await;
    drop(expired);
    drop(conn);
    clone.cleanup().await;
    assert_eq!(observer.tcp_scoreboard.lock().await.len(), 2);
    *clone.last_cleanup.lock().await = Instant::now() - CLEANUP_INTERVAL;
    clone
        .on_new_request(live.user_context().stats.as_ref().unwrap())
        .await;
    assert_eq!(observer.tcp_scoreboard.lock().await.len(), 1);
    assert_eq!(observer.sq_conns.lock().await.len(), 1);
    let stats = observer.get_all_stats(&["alice".into()]).await;
    assert_eq!((stats[0].tcp_conns, stats[0].conn_num), (1, 1));
}

#[tokio::test]
async fn cancelling_udp_registration_does_not_leak_connection_count() {
    let observer = Observer::new();
    let conn: Arc<dyn Stoppable> = Arc::new(TestConnection);
    let (request, _, _) = udp_request(user_context(&conn, 1));
    let _scoreboard = observer.udp_scoreboard.lock().await;
    let mut registration = Box::pin(observer.wrap_request(request, "selected".into()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(registration.as_mut().poll(&mut cx).is_pending());
    drop(registration);
    assert_eq!(observer.get_user_stats("alice").await.udp_conns, 0);
}

#[tokio::test]
async fn cancelling_tcp_registration_rolls_back_user_counts() {
    let observer = Observer::new();
    let conn: Arc<dyn Stoppable> = Arc::new(TestConnection);
    let _scoreboard = observer.tcp_scoreboard.lock().await;
    let mut registration = Box::pin(observer.wrap_request(
        tcp_request(user_context(&conn, 1), false),
        "selected".into(),
    ));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(registration.as_mut().poll(&mut cx).is_pending());
    assert_eq!(observer.get_user_stats("alice").await.tcp_conns, 1);
    drop(registration);
    assert_eq!(observer.get_user_stats("alice").await.tcp_conns, 0);
}

#[tokio::test]
async fn unauthenticated_requests_use_the_empty_username() {
    let observer = Observer::new();
    let tcp = observer
        .wrap_request(
            tcp_request(UserContext::default(), false),
            "selected".into(),
        )
        .await;
    let (udp, _, _) = udp_request(UserContext::default());
    let udp = observer.wrap_request(udp, "selected".into()).await;
    assert!(tcp.user_context().stats.is_none());
    assert!(udp.user_context().stats.is_none());
    assert_eq!(observer.user_stats.lock().await.len(), 1);
    assert!(observer.user_stats.lock().await.contains_key(""));
    assert!(observer.sq_conns.lock().await.is_empty());
    assert_eq!(observer.tcp_scoreboard.lock().await.len(), 1);
    assert_eq!(observer.udp_scoreboard.lock().await.len(), 1);
    assert_eq!(observer.get_user_stats("").await.tcp_conns, 1);
    assert_eq!(observer.get_user_stats("").await.udp_conns, 1);
    drop(tcp);
    drop(udp);
    assert_eq!(observer.get_user_stats("").await.tcp_conns, 0);
    assert_eq!(observer.get_user_stats("").await.udp_conns, 0);
}

#[tokio::test]
async fn first_response_requires_nonempty_successful_io_and_is_set_once() {
    let observer = Observer::new();
    let ProxyRequest::Tcp(mut tcp) = observer
        .wrap_request(
            tcp_request(UserContext::default(), false),
            "selected".into(),
        )
        .await
    else {
        unreachable!()
    };
    let (send, _) = observer.tcp_scoreboard.lock().await[0].clone();
    assert_eq!(tcp.stream.write(&[]).await.unwrap(), 0);
    assert_eq!(tcp.stream.write_vectored(&[]).await.unwrap(), 0);
    assert_eq!(tcp.stream.read(&mut [0; 16]).await.unwrap(), 4);
    assert!(send.upgrade().unwrap().first_send.get().is_none());
    assert_eq!(
        tcp.stream
            .write_vectored(&[IoSlice::new(b"response")])
            .await
            .unwrap(),
        3
    );
    let first = *send.upgrade().unwrap().first_send.get().unwrap();
    assert_eq!(tcp.stream.write(b"another").await.unwrap(), 3);
    assert_eq!(send.upgrade().unwrap().first_send.get(), Some(&first));

    let (request, _, mut replies) = udp_request(UserContext::default());
    let ProxyRequest::Udp(udp) = observer.wrap_request(request, "selected".into()).await else {
        unreachable!()
    };
    let (send, _) = observer.udp_scoreboard.lock().await[0].clone();
    udp.send.send_to(Bytes::new(), addr()).await.unwrap();
    replies.recv().await.unwrap();
    assert!(send.upgrade().unwrap().first_send.get().is_none());
    udp.send
        .send_to(Bytes::from_static(b"reply"), addr())
        .await
        .unwrap();
    assert!(send.upgrade().unwrap().first_send.get().is_some());
}

#[tokio::test]
async fn user_counters_include_restored_traffic_and_follow_resets_and_removal() {
    let observer = Observer::new();
    observer
        .restore_stats(&[UserStats {
            username: "alice".into(),
            tcp_sent: 1000,
            ..Default::default()
        }])
        .await;
    assert_eq!(observer.get_user_stats("alice").await.tcp_sent, 1000);
    let conn: Arc<dyn Stoppable> = Arc::new(TestConnection);
    let ProxyRequest::Tcp(mut tcp) = observer
        .wrap_request(
            tcp_request(user_context(&conn, 1), false),
            "selected".into(),
        )
        .await
    else {
        unreachable!()
    };
    assert_eq!(tcp.stream.write(b"one").await.unwrap(), 3);
    assert_eq!(observer.get_user_stats("alice").await.tcp_sent, 1003);
    observer.clear_user_stats("alice").await;
    assert_eq!(observer.get_user_stats("alice").await.tcp_sent, 0);
    assert_eq!(tcp.stream.write(b"two").await.unwrap(), 3);
    assert_eq!(observer.get_user_stats("alice").await.tcp_sent, 3);
    observer.clear_all_stats(&["alice".into()]).await;
    assert_eq!(observer.get_user_stats("alice").await.tcp_sent, 0);
    observer.remove_user("alice").await;
    assert_eq!(tcp.stream.write(b"three").await.unwrap(), 3);
    assert_eq!(tcp.stream.read(&mut [0; 16]).await.unwrap(), 4);
    drop(tcp);
    assert_eq!(observer.get_user_stats("alice").await.tcp_sent, 0);
    assert_eq!(observer.get_user_stats("alice").await.tcp_recv, 0);
    assert_eq!(observer.get_user_stats("alice").await.tcp_conns, 0);
    assert!(observer.user_stats.lock().await.is_empty());
}

#[tokio::test]
async fn udp_receive_half_keeps_only_its_metadata_and_connection_count_until_drop() {
    let observer = Observer::new();
    let (request, sender, _) = udp_request(UserContext::default());
    let ProxyRequest::Udp(udp) = observer.wrap_request(request, "selected".into()).await else {
        unreachable!()
    };
    let (send, recv) = observer.udp_scoreboard.lock().await[0].clone();
    let mut receiver = udp.recv;
    drop(udp.send);
    assert!(send.upgrade().is_none());
    assert!(recv.upgrade().is_some());
    assert_eq!(observer.get_user_stats("").await.udp_conns, 1);
    sender
        .send((Bytes::from_static(b"upload"), addr()))
        .await
        .unwrap();
    receiver.recv_from().await.unwrap();
    assert_eq!(observer.get_user_stats("").await.udp_recv, 6);
    drop(receiver);
    assert_eq!(observer.get_user_stats("").await.udp_conns, 0);
    assert!(send.upgrade().is_none());
    assert!(recv.upgrade().is_none());
}

struct CompleteOutbound(mpsc::UnboundedSender<ProxyRequest>);

#[async_trait]
impl Outbound for CompleteOutbound {
    async fn handle(&self, req: ProxyRequest) -> Result<(), SError> {
        assert!(self.0.send(req).is_ok());
        Ok(())
    }
}

#[tokio::test]
async fn dispatcher_records_selected_outbound_for_tcp_and_udp() {
    let observer = Arc::new(Observer::new());
    let conn: Arc<dyn Stoppable> = Arc::new(TestConnection);
    let mut context = user_context(&conn, 1);
    context.observer = Arc::downgrade(&observer);
    let tcp = tcp_request(context.clone(), false);
    let (udp, _, _) = udp_request(context);
    let (send, mut requests) = mpsc::unbounded_channel();
    let dispatcher = RequestDispatcher {
        outbounds: HashMap::from([(
            "selected".into(),
            Arc::new(CompleteOutbound(send)) as Arc<dyn Outbound>,
        )]),
        default_outbound: "selected".into(),
        #[cfg(feature = "plugin")]
        router: None,
    };
    dispatcher.dispatch(tcp).await;
    dispatcher.dispatch(udp).await;
    let tcp = requests.try_recv().unwrap();
    let udp = requests.try_recv().unwrap();
    assert!(matches!(tcp, ProxyRequest::Tcp(_)));
    assert!(matches!(udp, ProxyRequest::Udp(_)));
    let tcp_scoreboard = observer.tcp_scoreboard.lock().await;
    let udp_scoreboard = observer.udp_scoreboard.lock().await;
    assert_eq!(tcp_scoreboard.len(), 1);
    assert_eq!(udp_scoreboard.len(), 1);
    let tcp_stats = tcp_scoreboard[0].0.upgrade().unwrap();
    let udp_stats = udp_scoreboard[0].0.upgrade().unwrap();
    assert_eq!(tcp_stats.outbound, "selected");
    assert_eq!(udp_stats.outbound, "selected");
}

#[cfg(feature = "plugin")]
#[tokio::test]
async fn dispatcher_records_router_rewrite_and_outbound_override() {
    let observer = Arc::new(Observer::new());
    let conn: Arc<dyn Stoppable> = Arc::new(TestConnection);
    let mut context = user_context(&conn, 1);
    context.observer = Arc::downgrade(&observer);
    context.preferred_outbound = Some("preferred".into());
    let tcp = tcp_request(context, false);
    let (send, mut requests) = mpsc::unbounded_channel();
    let dispatcher = RequestDispatcher {
        outbounds: HashMap::from([(
            "selected".into(),
            Arc::new(CompleteOutbound(send)) as Arc<dyn Outbound>,
        )]),
        default_outbound: "fallback".into(),
        router: Some(Arc::new(
            crate::plugin::router::Router::from_source(
                r#"return function(ctx)
                ctx.dst_ip_v4 = "192.0.2.99"
                ctx.dst_domain = nil
                ctx.dst_port = 8443
                return "selected"
            end"#,
            )
            .unwrap(),
        )),
    };
    dispatcher.dispatch(tcp).await;
    let tcp = requests.try_recv().unwrap();
    let scoreboard = observer.tcp_scoreboard.lock().await;
    assert_eq!(scoreboard.len(), 1);
    let stats = scoreboard[0].0.upgrade().unwrap();
    assert_eq!(stats.outbound, "selected");
    assert_eq!(*stats.dst.read().unwrap(), *tcp.dst());
    assert_eq!(
        *stats.dst.read().unwrap(),
        "192.0.2.99:8443".parse::<SocketAddr>().unwrap().into()
    );
}
