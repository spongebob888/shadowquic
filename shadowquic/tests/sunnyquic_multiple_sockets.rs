#![cfg(feature = "sunnyquic-noq")]

use std::{net::SocketAddr, time::Duration};

use noq::{Connection, PathId};
use shadowquic::{
    config::{Interface, QuicPath, SunnyQuicClientCfg, SunnyQuicServerCfg},
    quic::{QuicClient, QuicServer},
    sunnyquic::{EndClient, EndServer},
};
use tokio::time::{sleep, timeout};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn server(bind: &str, extra: &[&str]) -> Result<EndServer> {
    Ok(EndServer::new(&SunnyQuicServerCfg {
        bind_addr: bind.parse()?,
        extra_bind_addrs: extra
            .iter()
            .map(|a| a.parse())
            .collect::<std::result::Result<_, _>>()?,
        server_name: "localhost".into(),
        cert_path: "../assets/certs/localhost.crt".into(),
        key_path: "../assets/certs/localhost.key".into(),
        ..Default::default()
    })
    .await?)
}

async fn client(primary: SocketAddr, extra_paths: Vec<QuicPath>) -> Result<EndClient> {
    let mut cfg = SunnyQuicClientCfg {
        addr: primary.to_string(),
        server_name: "localhost".into(),
        cert_path: Some("../assets/certs/MyCA.pem".into()),
        extra_paths,
        ..Default::default()
    };
    cfg.socket_opt.bind_interface = Some(Interface::Address("127.0.0.1:0".parse()?));
    Ok(EndClient::new(&cfg).await?)
}

async fn validated_extra(conn: &Connection) -> Result<noq::Path> {
    loop {
        if let Some(path) = conn.path(1u32.into())
            && path.stats().frame_rx.path_response > 0
        {
            return Ok(conn
                .open_path_ensure(path.network_path()?, Default::default())
                .await?);
        }
        sleep(Duration::from_millis(5)).await;
    }
}

async fn exercise(
    server: EndServer,
    primary: SocketAddr,
    secondary: SocketAddr,
    local: Option<&str>,
) -> Result<()> {
    let extra = match local {
        None => QuicPath::Remote(secondary.to_string()),
        Some(bind) => QuicPath::Bound {
            bind_addr: bind.parse()?,
            addr: (primary != secondary).then(|| secondary.to_string()),
        },
    };
    let client = client(primary, vec![extra]).await?;
    let accept = async {
        // Exercise the endpoint's stateless response path before accepting.
        let initial = noq::Endpoint::accept(&server)
            .await
            .ok_or("server closed")?;
        initial.retry()?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(QuicServer::accept(&server).await?)
    };
    let (c, s) = tokio::join!(client.connect(primary, "localhost"), accept);
    let (c, s) = (c?, s?);
    let second = validated_extra(&c).await?;
    let first = c.path(PathId::ZERO).unwrap();
    let server_primary = s.path(PathId::ZERO).unwrap().remote_address()?;
    let server_extra = s.path(second.id()).unwrap().remote_address()?;
    let primary_ip = server_primary.ip().to_canonical();
    let extra_ip = server_extra.ip().to_canonical();
    if primary_ip == extra_ip {
        assert_eq!(
            server_primary.port() != server_extra.port(),
            local.is_some()
        );
    } else {
        assert!(local.is_some());
        assert!(primary_ip.is_loopback() && extra_ip.is_loopback());
    }
    assert_eq!(first.remote_address()?.port(), primary.port());
    assert_eq!(second.remote_address()?.port(), secondary.port());

    // Keep the same application stream open while abandoning the initial path.
    let (mut tx, mut rx) = c.open_bi().await?;
    tx.write_all(b"first").await?;
    let (mut reply, mut request) = s.accept_bi().await?;
    let mut buf = [0; 5];
    request.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"first");

    first.close()?;
    let before = second.stats().frame_tx.stream;
    let payload = vec![0xab; 128 * 1024];
    tx.write_all(&payload).await?;
    tx.finish()?;
    assert_eq!(request.read_to_end(payload.len()).await?, payload);
    reply.write_all(&payload).await?;
    reply.finish()?;
    assert_eq!(rx.read_to_end(payload.len()).await?, payload);
    assert!(second.stats().frame_tx.stream > before);
    assert!(second.stats().frame_rx.stream > 0);
    Ok(())
}

#[tokio::test]
async fn two_client_ports_to_one_server_port() -> Result<()> {
    let server = server("127.0.0.1:0", &[]).await?;
    let addr = server.bound_addrs()[0];
    timeout(
        Duration::from_secs(15),
        exercise(server, addr, addr, Some("0.0.0.0:0")),
    )
    .await?
}

#[tokio::test]
async fn one_client_port_to_two_wildcard_listeners() -> Result<()> {
    let server = server("0.0.0.0:0", &["0.0.0.0:0"]).await?;
    // Start the handshake on the second listener. Send the extra path to the
    // first listener. Both must reply from the IP and port originally addressed.
    let alternate_ip = if std::net::UdpSocket::bind("127.0.0.2:0").is_ok() {
        "127.0.0.2"
    } else {
        "127.0.0.1"
    };
    let addr = SocketAddr::new(alternate_ip.parse()?, server.bound_addrs()[1].port());
    let extra = SocketAddr::new("127.0.0.1".parse()?, server.bound_addrs()[0].port());
    timeout(Duration::from_secs(15), exercise(server, addr, extra, None)).await?
}

#[tokio::test]
async fn two_client_ports_to_two_server_ports() -> Result<()> {
    let server = server("127.0.0.1:0", &["127.0.0.1:0"]).await?;
    let addr = server.bound_addrs()[0];
    let extra = server.bound_addrs()[1];
    timeout(
        Duration::from_secs(15),
        exercise(server, addr, extra, Some("127.0.0.1:0")),
    )
    .await?
}

#[tokio::test]
async fn ipv4_primary_and_ipv6_extra_socket() -> Result<()> {
    if std::net::UdpSocket::bind("[::1]:0").is_err() {
        return Ok(());
    }
    let server = server("127.0.0.1:0", &["[::1]:0"]).await?;
    let primary = server.bound_addrs()[0];
    let extra = server.bound_addrs()[1];
    timeout(
        Duration::from_secs(15),
        exercise(server, primary, extra, Some("[::1]:0")),
    )
    .await?
}

#[test]
fn extra_paths_accept_remote_strings_and_explicit_socket_bindings() {
    let yaml = r#"
- "localhost:443"
- bind-addr: "0.0.0.0:0"
- bind-addr: "127.0.0.1:50001"
  addr: "localhost:8443"
"#;
    let paths: Vec<QuicPath> = serde_saphyr::from_str(yaml).unwrap();
    assert!(matches!(&paths[0], QuicPath::Remote(addr) if addr == "localhost:443"));
    assert!(
        matches!(&paths[1], QuicPath::Bound { addr: None, bind_addr } if bind_addr.port() == 0)
    );
    assert!(
        matches!(&paths[2], QuicPath::Bound { addr: Some(addr), bind_addr } if addr == "localhost:8443" && bind_addr.port() == 50001)
    );
    assert!(
        serde_saphyr::from_str::<QuicPath>(
            "bind-addr: '127.0.0.1:0'
unknown: true"
        )
        .is_err()
    );
}
