use shadowquic_lib::{Inbound, socks::SocksServer};

#[tokio::main]
async fn main() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap_or_default();

    // env_logger::init();

    let mut socks_server = SocksServer::new("127.0.0.1:1089".parse().unwrap())
        .await
        .unwrap();

    socks_server.accept().await.unwrap();
}
