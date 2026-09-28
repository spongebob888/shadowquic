use super::*;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::sync::{Barrier, mpsc};

struct TestInbound {
    requests: mpsc::Receiver<ProxyRequest>,
    initialized: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
    fail_init: bool,
    fail_shutdown: bool,
}

#[async_trait]
impl Inbound for TestInbound {
    async fn init(&self) -> Result<(), SError> {
        self.initialized.fetch_add(1, Ordering::SeqCst);
        if self.fail_init {
            return Err(SError::InboundUnavailable);
        }
        Ok(())
    }

    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        match self.requests.recv().await {
            Some(request) => Ok(request),
            None => std::future::pending().await,
        }
    }

    async fn shutdown(&self) -> Result<(), SError> {
        self.stopped.fetch_add(1, Ordering::SeqCst);
        if self.fail_shutdown {
            return Err(SError::InboundUnavailable);
        }
        Ok(())
    }
}

struct TestOutbound {
    called: Arc<Barrier>,
}

#[async_trait]
impl Outbound for TestOutbound {
    async fn handle(&self, _: ProxyRequest) -> Result<(), SError> {
        self.called.wait().await;
        // Shutdown must interrupt a stalled handler.
        std::future::pending().await
    }
}

fn request() -> ProxyRequest {
    let (send, recv) = mpsc::channel(1);
    ProxyRequest::Udp(UdpSession {
        recv: Box::new(recv),
        send: Arc::new(send),
        stream: None,
        bind_addr: "127.0.0.1:0"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into(),
        user_context: None,
    })
}

async fn check_manager(fail_init: bool, fail_shutdown: bool) {
    let initialized = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let called = Arc::new(Barrier::new(3));
    let mut inbounds: HashMap<String, Box<dyn Inbound>> = HashMap::new();
    for i in 0..2 {
        let (send, recv) = mpsc::channel(1);
        send.send(request())
            .await
            .unwrap_or_else(|_| panic!("request channel closed"));
        inbounds.insert(
            i.to_string(),
            Box::new(TestInbound {
                requests: recv,
                initialized: initialized.clone(),
                stopped: stopped.clone(),
                fail_init,
                fail_shutdown: fail_shutdown && i == 0,
            }),
        );
    }
    let manager = Manager {
        inbounds,
        outbounds: HashMap::from([
            (
                "selected".into(),
                Arc::new(TestOutbound {
                    called: called.clone(),
                }) as Arc<dyn Outbound>,
            ),
            // Selecting this outbound would deadlock the test.
            (
                "unused".into(),
                Arc::new(TestOutbound {
                    called: Arc::new(Barrier::new(100)),
                }) as Arc<dyn Outbound>,
            ),
        ]),
        default_outbound: "selected".into(),
    };
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        manager.run_until(async {
            called.wait().await;
        }),
    )
    .await
    .expect("manager stalled");
    assert_eq!(result.is_err(), fail_init || fail_shutdown);
    assert_eq!(stopped.load(Ordering::SeqCst), 2);
    if !fail_init {
        assert_eq!(initialized.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn concurrent_listeners_share_outbound_and_shutdown() {
    check_manager(false, false).await;
}

#[tokio::test]
async fn shutdown_failure_does_not_skip_other_listeners() {
    check_manager(false, true).await;
}

#[tokio::test]
async fn initialization_failure_cleans_up_listeners() {
    check_manager(true, false).await;
}
