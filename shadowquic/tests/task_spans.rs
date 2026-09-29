use shadowquic::config::Config;
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// A current-thread runtime keeps the scoped subscriber installed while spawned
// listener and connection tasks run, without changing the global subscriber.
#[tokio::test(flavor = "current_thread")]
async fn spawned_connection_errors_keep_endpoint_and_peer_spans() {
    let logs = LogBuffer(Arc::new(Mutex::new(Vec::new())));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);

    for kind in [
        "socks",
        #[cfg(feature = "mixed")]
        "mixed",
    ] {
        // Reserve a free port until immediately before the manager binds it.
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reservation.local_addr().unwrap();
        let cfg: Config = serde_saphyr::from_str(&format!(
            "inbounds: [{{type: {kind}, tag: local-{kind}, bind-addr: '{addr}'}}]\n\
             outbounds: [{{type: direct, tag: direct}}]\n"
        ))
        .unwrap();
        let manager = cfg.build_manager().await.unwrap();
        drop(reservation);
        let probe = async {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            // An incomplete SOCKS greeting fails after the handler has yielded.
            stream.write_all(&[5]).await.unwrap();
            stream.shutdown().await.unwrap();
            let mut reply = Vec::new();
            stream.read_to_end(&mut reply).await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), manager.run_until(probe))
            .await
            .unwrap()
            .unwrap();
        let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let error = output
            .lines()
            .find(|line| line.contains(&format!("failed to handle {kind} connection")))
            .unwrap_or_else(|| panic!("missing connection error: {output}"));
        assert!(
            error.contains(&format!("inbound{{tag=local-{kind}}}")),
            "{error}"
        );
        assert!(error.contains(&format!("{kind}{{src=")), "{error}");
    }
}
