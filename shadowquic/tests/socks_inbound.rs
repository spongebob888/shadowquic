use std::time::Duration;

use shadowquic::{Inbound, ProxyRequest, config::SocksServerCfg, socks::inbound::SocksServer};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    time::timeout,
};
use tracing::{Event, Subscriber};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

struct SocksErrors(mpsc::UnboundedSender<()>);

impl<S: Subscriber> Layer<S> for SocksErrors {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        if *event.metadata().level() == tracing::Level::ERROR
            && event.metadata().target() == "shadowquic::socks::inbound"
        {
            let _ = self.0.send(());
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn socks_server_accepts_after_client_closes_before_accept() {
    let (error_tx, mut error_rx) = mpsc::unbounded_channel();
    let subscriber = tracing_subscriber::registry().with(SocksErrors(error_tx));
    let _subscriber = tracing::subscriber::set_default(subscriber);

    timeout(Duration::from_secs(5), async {
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut inbound = SocksServer::new(SocksServerCfg {
            tag: "test-socks".into(),
            default_outbound: None,
            bind_addr: addr,
            users: vec![],
        })
        .await
        .unwrap();
        inbound.init().await.unwrap();

        // init binds the listener synchronously and spawns the accept task.
        // On this single-thread runtime, connecting and closing without yielding
        // guarantees the client is gone before that task can accept it. The OS
        // may still accept the socket, in which case the SOCKS handshake sees EOF.
        drop(std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)).unwrap());
        error_rx
            .recv()
            .await
            .expect("the early close must be reported");

        let mut client = TcpStream::connect(addr).await.unwrap();
        let client_addr = client.local_addr().unwrap();
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut auth_reply = [0; 2];
        client.read_exact(&mut auth_reply).await.unwrap();
        assert_eq!(auth_reply, [0x05, 0x00]);

        // CONNECT 127.0.0.1:80; no outbound or external server is needed.
        client
            .write_all(&[0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let mut connect_reply = [0; 10];
        client.read_exact(&mut connect_reply).await.unwrap();
        assert_eq!(&connect_reply[..4], &[0x05, 0x00, 0x00, 0x01]);

        let ProxyRequest::Tcp(mut session) = inbound.accept().await.unwrap() else {
            panic!("expected the second client's TCP request");
        };
        assert_eq!(session.src_addr, Some(client_addr));
        assert_eq!(session.dst.to_string(), "127.0.0.1:80");

        client.write_all(b"ping").await.unwrap();
        let mut payload = [0; 4];
        session.stream.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"ping");
    })
    .await
    .expect("SOCKS server stopped accepting after an early client close");
}
