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
async fn test_self_signed_cert() {
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::filter::Targets::new()
                .with_target("shadowquic", LevelFilter::TRACE),
        )
        .try_init();

    let (cert_pem, key_pem) = generate_self_signed_cert();
    let mut temp_dir = std::env::temp_dir();
    let random_suffix: u32 = rand::random();
    temp_dir.push(format!("shadowquic_test_{}", random_suffix));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let cert_path = temp_dir.join("cert.pem");
    let key_path = temp_dir.join("key.pem");
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();

    let socks_port = 1099;
    let socks_server_addr = format!("127.0.0.1:{}", socks_port);
    let quic_port = 4449;
    let quic_addr = format!("127.0.0.1:{}", quic_port);
    let target_port = 1449;

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

    // Start Client
    let socks_server = SocksServer::new(SocksServerCfg {
        bind_addr: socks_server_addr.parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();

    let sq_client = SunnyQuicClient::new(SunnyQuicClientCfg {
        password: "password".into(),
        username: "user".into(),
        addr: quic_addr.parse().unwrap(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        zero_rtt: true,
        over_stream: false,
        cert_path: Some(cert_path.clone()), // Use the self-signed cert
        ..Default::default()
    });

    let client = Manager {
        inbound: Box::new(socks_server),
        outbound: Box::new(sq_client),
    };
    tokio::spawn(client.run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test Connection
    let mut config = Config::default();
    config.set_skip_auth(false);

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut s = Socks5Stream::connect(
            socks_server_addr.as_str(),
            "127.0.0.1".into(),
            target_port,
            config,
        )
        .await
        .expect("Failed to connect to socks server");

        let mut buf = [0u8; 10];
        s.write_all(b"1234567890").await.unwrap();
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"1234567890");
    })
    .await;

    match result {
        Ok(_) => println!("Test passed!"),
        Err(_) => panic!("Test failed/timed out!"),
    }
}

async fn tcp_peer(port: u16) {
    let lis = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    let (mut s, _addr) = lis.accept().await.unwrap();
    let (mut r, mut w) = s.split();
    tokio::io::copy(&mut r, &mut w).await.unwrap();
}
