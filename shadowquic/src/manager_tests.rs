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
        dst: "127.0.0.1:53"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into(),
        src_addr: None,
        recv: Box::new(recv),
        send: Arc::new(send),
        stream: None,
        bind_addr: "127.0.0.1:0"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into(),
        user_context: Default::default(),
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
        #[cfg(feature = "plugin")]
        router: None,
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

struct DispatchDropCounter(Arc<AtomicUsize>);

impl Drop for DispatchDropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct StalledOutbound {
    started: Arc<Barrier>,
    cancelled: Arc<AtomicUsize>,
}

#[async_trait]
impl Outbound for StalledOutbound {
    async fn handle(&self, req: ProxyRequest) -> Result<(), SError> {
        let _on_drop = DispatchDropCounter(self.cancelled.clone());
        let ProxyRequest::Udp(session) = req else {
            panic!("expected UDP request");
        };
        assert_eq!(session.user_context.inbound_tag, "inbound");
        self.started.wait().await;
        std::future::pending().await
    }
}

#[tokio::test]
async fn one_inbound_dispatches_concurrently_and_cancels_requests_on_shutdown() {
    let initialized = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let cancelled = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Barrier::new(3));
    let (send, recv) = mpsc::channel(2);
    for _ in 0..2 {
        send.send(request())
            .await
            .unwrap_or_else(|_| panic!("request channel closed"));
    }
    let outbound = Arc::new(StalledOutbound {
        started: started.clone(),
        cancelled: cancelled.clone(),
    });
    let manager = Manager::single(
        Box::new(TestInbound {
            requests: recv,
            initialized: initialized.clone(),
            stopped: stopped.clone(),
            fail_init: false,
            fail_shutdown: false,
        }),
        outbound.clone(),
    );
    #[cfg(feature = "plugin")]
    let manager = {
        let mut manager = manager;
        manager.outbounds.insert("selected".into(), outbound);
        manager.outbounds.insert(
            "outbound".into(),
            Arc::new(TestOutbound {
                called: Arc::new(Barrier::new(100)),
            }),
        );
        manager.router = Some(Arc::new(
            plugin::router::Router::from_source(
                r#"
            return function(ctx)
                assert(ctx.inbound_tag == "inbound")
                assert(ctx.network_type == "udp")
                return "selected"
            end
        "#,
            )
            .unwrap(),
        ));
        manager
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        manager.run_until(async {
            // Both handlers must start even though neither has completed.
            started.wait().await;
        }),
    )
    .await
    .expect("a stalled request blocked the next accept or shutdown")
    .unwrap();
    assert_eq!(initialized.load(Ordering::SeqCst), 1);
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    assert_eq!(cancelled.load(Ordering::SeqCst), 2);
}

struct PausingInbound {
    requests: mpsc::Receiver<ProxyRequest>,
    accepting: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Inbound for PausingInbound {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        let Some(req) = self.requests.recv().await else {
            return std::future::pending().await;
        };
        if req.dst().port == 54 {
            self.accepting.notify_one();
            self.resume.notified().await;
        }
        Ok(req)
    }
}

struct CompletingOutbound {
    accepting: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
    finished: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Outbound for CompletingOutbound {
    async fn handle(&self, req: ProxyRequest) -> Result<(), SError> {
        if req.dst().port == 53 {
            // Complete this dispatch only after the next accept holds a request.
            self.accepting.notified().await;
            self.resume.notify_one();
        } else {
            self.finished.notify_one();
        }
        Ok(())
    }
}

#[tokio::test]
async fn completing_dispatch_does_not_cancel_an_in_progress_accept() {
    let accepting = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(tokio::sync::Notify::new());
    let (send, recv) = mpsc::channel(2);
    for port in [53, 54] {
        let mut req = request();
        req.set_dst(SocketAddr::from(([127, 0, 0, 1], port)).into());
        send.send(req)
            .await
            .unwrap_or_else(|_| panic!("request channel closed"));
    }
    let manager = Manager::single(
        Box::new(PausingInbound {
            requests: recv,
            accepting: accepting.clone(),
            resume: resume.clone(),
        }),
        Arc::new(CompletingOutbound {
            accepting,
            resume,
            finished: finished.clone(),
        }),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        manager.run_until(finished.notified()),
    )
    .await
    .expect("pending accept was cancelled when the first dispatch completed")
    .unwrap();
}
