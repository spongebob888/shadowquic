//! Run from the repository root with cargo run -p perf --release --bin sunnyquic_multipath.
use std::{io, sync::Arc, time::Duration};

use shadowquic::{
    Manager,
    config::{AuthUser, QuicPath, SunnyQuicClientCfg, SunnyQuicServerCfg},
    direct::outbound::DirectOut,
    dns::ResolverManager,
    squic::outbound::connect_tcp,
    sunnyquic::{inbound::SunnyQuicServer, outbound::SunnyQuicClient},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    time::{sleep, timeout},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const LOCAL: &str = "127.0.0.1:0";

async fn transfer(
    sender: &mut (impl AsyncWrite + Unpin),
    receiver: &mut (impl AsyncRead + Unpin),
    chunk_size: usize,
    chunks: usize,
    pause: Duration,
) -> io::Result<()> {
    let write = async {
        let mut buf = vec![0; chunk_size];
        for i in 0..chunks {
            buf.fill(i as u8);
            sender.write_all(&buf).await?;
            if !pause.is_zero() {
                sender.flush().await?;
                sleep(pause).await;
            }
        }
        sender.flush().await
    };
    let read = async {
        let mut buf = vec![0; chunk_size];
        for i in 0..chunks {
            receiver.read_exact(&mut buf).await?;
            if buf.iter().any(|&byte| byte != i as u8) {
                return Err(io::Error::other("received payload mismatch"));
            }
        }
        Ok(())
    };
    tokio::try_join!(write, read)?;
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    timeout(Duration::from_secs(60), experiment()).await?
}

async fn experiment() -> Result<()> {
    let user = AuthUser {
        username: "experiment".into(),
        password: "loopback".into(),
    };
    let server = SunnyQuicServer::new(SunnyQuicServerCfg {
        tag: "inbound".into(),
        bind_addr: LOCAL.parse()?,
        extra_bind_addrs: vec![LOCAL.parse()?],
        users: vec![user.clone()],
        alpn: vec!["h3".into()],
        cert_path: "assets/certs/localhost.crt".into(),
        key_path: "assets/certs/localhost.key".into(),
        ..Default::default()
    })
    .await?;
    let primary = server.endpoint.bound_addrs()[0];
    let extra = server.endpoint.bound_addrs()[1];
    let manager = Manager::single(Box::new(server), Arc::new(DirectOut::default()));
    let client = SunnyQuicClient::new(
        SunnyQuicClientCfg {
            addr: primary.to_string(),
            extra_paths: vec![QuicPath::Bound {
                addr: Some(extra.to_string()),
                bind_addr: LOCAL.parse()?,
            }],
            username: user.username,
            password: user.password,
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            cert_path: Some("assets/certs/MyCA.pem".into()),
            ..Default::default()
        },
        Arc::new(ResolverManager::new()),
    );

    let measure = async {
        let conn = client.get_conn().await?;
        if !conn.is_multipath_enabled() {
            return Err("multipath negotiation failed".into());
        }
        // Wait for SunnyQUIC's configured extra-path task to create path 1.
        // Joining its open future then waits for validation, even if already complete.
        timeout(Duration::from_secs(10), async {
            while conn.path(1u32.into()).is_none() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .map_err(|_| "SunnyQUIC did not open the configured extra path")?;
        let paths = [
            conn.path(0u32.into()).ok_or("primary path closed")?,
            conn.open_path_ensure(extra, Default::default()).await?,
        ];
        let target = TcpListener::bind(LOCAL).await?;
        let mut stream = connect_tcp(&conn, target.local_addr()?.into()).await?;
        let (mut peer, _) = target.accept().await?;
        peer.set_nodelay(true)?;
        // Finish a round trip on the proxied stream before taking baseline counters.
        transfer(&mut stream, &mut peer, 1, 1, Duration::ZERO).await?;
        transfer(&mut peer, &mut stream, 1, 1, Duration::ZERO).await?;

        for path in &paths {
            println!(
                "path {} {} {:?}",
                path.id(),
                path.remote_address()?,
                path.status()?
            );
        }
        println!("load direction payload_bytes path stream_frames udp_bytes");
        for (load, size, chunks, pause) in [
            ("light", 512, 32, Duration::from_millis(10)),
            ("bulk", 65536, 1024, Duration::ZERO),
        ] {
            for direction in ["upload", "download"] {
                let counters = || {
                    paths.each_ref().map(|path| {
                        let stats = path.stats();
                        if direction == "upload" {
                            (stats.frame_tx.stream, stats.udp_tx.bytes)
                        } else {
                            (stats.frame_rx.stream, stats.udp_rx.bytes)
                        }
                    })
                };
                let before = counters();
                if direction == "upload" {
                    transfer(&mut stream, &mut peer, size, chunks, pause).await?;
                } else {
                    transfer(&mut peer, &mut stream, size, chunks, pause).await?;
                }
                for ((path, start), end) in paths.iter().zip(before).zip(counters()) {
                    path.status()?; // Reject a sample if a path disappeared during the transfer.
                    println!(
                        "{load} {direction} {} {} {} {}",
                        size * chunks,
                        path.id(),
                        end.0 - start.0,
                        end.1 - start.1
                    );
                }
            }
        }
        Ok(())
    };
    tokio::select! {
        result = measure => result,
        result = tokio::spawn(manager.run()) => { result??; Err("server stopped early".into()) },
    }
}
