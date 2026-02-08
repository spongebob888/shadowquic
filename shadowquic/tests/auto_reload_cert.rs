use fast_socks5::client::{Config, Socks5Stream};
use shadowquic::config::{
    AuthUser, CongestionControl, SocksServerCfg, SunnyQuicClientCfg, SunnyQuicServerCfg,
    default_initial_mtu,
};
use shadowquic::sunnyquic::inbound::SunnyQuicServer;
use shadowquic::sunnyquic::outbound::SunnyQuicClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tokio::{net::TcpListener, time::Duration};

use shadowquic::{Manager, direct::outbound::DirectOut, socks::inbound::SocksServer};

use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing::{Level, level_filters::LevelFilter, trace};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const CHUNK_LEN: usize = 1024;
const ROUND: usize = 10;

fn to_pem(tag: &str, body: &[u8]) -> String {
    use std::fmt::Write;
    let b64 = base64_encode(body);
    let mut pem = String::new();
    writeln!(&mut pem, "-----BEGIN {}-----", tag).unwrap();
    for chunk in b64.as_bytes().chunks(64) {
        writeln!(&mut pem, "{}", std::str::from_utf8(chunk).unwrap()).unwrap();
    }
    writeln!(&mut pem, "-----END {}-----", tag).unwrap();
    pem
}

fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < input.len() {
        let chunk_len = std::cmp::min(3, input.len() - i);
        let chunk = &input[i..i + chunk_len];
        let b = match chunk_len {
            1 => ((chunk[0] as u32) << 16),
            2 => ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8),
            3 => ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32),
            _ => unreachable!(),
        };
        out.push(CHARS[((b >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((b >> 12) & 0x3F) as usize] as char);
        match chunk_len {
            1 => {
                out.push('=');
                out.push('=');
            }
            2 => {
                out.push(CHARS[((b >> 6) & 0x3F) as usize] as char);
                out.push('=');
            }
            3 => {
                out.push(CHARS[((b >> 6) & 0x3F) as usize] as char);
                out.push(CHARS[(b & 0x3F) as usize] as char);
            }
            _ => unreachable!(),
        }
        i += 3;
    }
    out
}

fn generate_self_signed_cert() -> (String, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_pem = to_pem("CERTIFICATE", &cert.cert.der());
    let key_pem = to_pem("PRIVATE KEY", &cert.signing_key.serialize_der());
    (cert_pem, key_pem)
}

#[tokio::test]
async fn test_auto_reload_cert() {
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::filter::Targets::new()
                .with_target("shadowquic", LevelFilter::INFO)
                .with_target("auto_reload_cert", LevelFilter::TRACE),
        )
        .try_init();

    // 1. Initial Certificate
    let (cert_pem_1, key_pem_1) = generate_self_signed_cert();
    let mut temp_dir = std::env::temp_dir();
    let random_suffix: u32 = rand::random();
    temp_dir.push(format!("shadowquic_reload_test_{}", random_suffix));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let cert_path = temp_dir.join("cert.pem");
    let key_path = temp_dir.join("key.pem");
    std::fs::write(&cert_path, &cert_pem_1).unwrap();
    std::fs::write(&key_path, &key_pem_1).unwrap();

    let socks_port = 2099;
    let socks_server_addr = format!("127.0.0.1:{}", socks_port);
    let quic_port = 5449;
    let quic_addr = format!("127.0.0.1:{}", quic_port);
    let target_port = 2449;

    // Start TCP Peer
    tokio::spawn(tcp_peer(target_port));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Start Server
    let sq_server = SunnyQuicServer::new(SunnyQuicServerCfg {
        bind_addr: quic_addr.parse().unwrap(),
        users: vec![AuthUser {
            username: "user".into(),
            password: "password".into(),
        }],
        alpn: vec!["h3".into()],
        zero_rtt: true,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        server_name: "localhost".into(),
        cert_path: cert_path.clone(),
        key_path: key_path.clone(),
        ..Default::default()
    })
    .unwrap();

    let direct_client = DirectOut::default();
    let server = Manager {
        inbound: Box::new(sq_server),
        outbound: Box::new(direct_client),
    };
    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Start Client 1 (Uses Cert 1)
    let socks_server_1 = SocksServer::new(SocksServerCfg {
        bind_addr: socks_server_addr.parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();

    let sq_client_1 = SunnyQuicClient::new(SunnyQuicClientCfg {
        password: "password".into(),
        username: "user".into(),
        addr: quic_addr.parse().unwrap(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        zero_rtt: true,
        over_stream: false,
        cert_path: Some(cert_path.clone()),
        ..Default::default()
    });

    let client_1 = Manager {
        inbound: Box::new(socks_server_1),
        outbound: Box::new(sq_client_1),
    };
    tokio::spawn(client_1.run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Helper to test connection
    async fn test_conn(socks_addr: &str, port: u16) -> bool {
        let mut config = Config::default();
        config.set_skip_auth(false);
        let res = tokio::time::timeout(Duration::from_secs(5), async {
            let mut s = Socks5Stream::connect(socks_addr, "127.0.0.1".into(), port, config)
                .await
                .ok()?;
            let mut buf = [0u8; 5];
            s.write_all(b"hello").await.ok()?;
            s.read_exact(&mut buf).await.ok()?;
            Some(buf)
        })
        .await;
        if let Ok(Some(buf)) = res {
            buf == *b"hello"
        } else {
            false
        }
    }

    assert!(
        test_conn(&socks_server_addr, target_port).await,
        "Client 1 failed with initial cert"
    );

    // 2. Rotate Certificate
    // We generate a NEW cert. The old client might still work if session resumption or if verification is loose,
    // but a NEW client configured with ONLY the new cert should work ONLY if server reloaded.
    // However, the client needs to trust the new cert.
    // To verify server reload, we can:
    // a) Make client strictly trust only Cert 2.
    // b) Update server file to Cert 2.
    // c) Wait for reload.
    // d) Client connects. If server didn't reload, it presents Cert 1, Client rejects (because it expects Cert 2).
    // e) If server reloaded, it presents Cert 2, Client accepts.

    let (cert_pem_2, key_pem_2) = generate_self_signed_cert();

    // Create a NEW file for client trust (simulating client config update)
    let cert_path_2 = temp_dir.join("cert2.pem");

    tokio::time::sleep(Duration::from_millis(1100)).await;

    // Overwrite SERVER files
    info!("Overwriting server certificate...");
    std::fs::write(&cert_path, &cert_pem_2).unwrap();
    std::fs::write(&key_path, &key_pem_2).unwrap();
    std::fs::write(&cert_path_2, &cert_pem_2).unwrap();

    // Wait for reload (watcher has 1s debounce, plus some buffer)
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Start Client 2 (Trusts Cert 2 ONLY)
    let socks_port_2 = 3099;
    let socks_server_addr_2 = format!("127.0.0.1:{}", socks_port_2);

    let socks_server_2 = SocksServer::new(SocksServerCfg {
        bind_addr: socks_server_addr_2.parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();

    let sq_client_2 = SunnyQuicClient::new(SunnyQuicClientCfg {
        password: "password".into(),
        username: "user".into(),
        addr: quic_addr.parse().unwrap(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        zero_rtt: true,
        over_stream: false,
        cert_path: Some(cert_path_2.clone()), // TRUST ONLY CERT 2
        ..Default::default()
    });
    let client_2 = Manager {
        inbound: Box::new(socks_server_2),
        outbound: Box::new(sq_client_2),
    };
    tokio::spawn(client_2.run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        test_conn(&socks_server_addr_2, target_port).await,
        "Client 2 failed - server likely didn't reload cert!"
    );
    println!("Test passed: Server successfully reloaded certificate!");
}

async fn tcp_peer(port: u16) {
    let lis = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    loop {
        if let Ok((mut s, _addr)) = lis.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    }
}
