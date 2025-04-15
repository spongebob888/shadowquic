use std::time::Duration;

use shadowquic_lib::{
    Manager,
    direct::DirectOut,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::inbound::SocksServer,
};
use tracing::{Level, level_filters::LevelFilter, trace};
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    let filter = tracing_subscriber::filter::Targets::new()
        // Enable the `INFO` level for anything in `my_crate`
        .with_target("shadowquic_lib", Level::TRACE)
        .with_target("shadowquic_lib::msgs::socks", LevelFilter::OFF);

    // Enable the `DEBUG` level for a specific module.

    // Build a new subscriber with the `fmt` layer using the `Targets`
    // filter we constructed above.
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    // env_logger::init();
    trace!("Running");

    let mut socks_server = SocksServer::new("127.0.0.1:1089".parse().unwrap())
        .await
        .unwrap();
    let direct_client = DirectOut;
    let server = Manager {
        inbound: Box::new(socks_server),
        outbound: Box::new(direct_client),
    };

    server.run().await.unwrap();
}

async fn test_shadowquic() {
    let filter = tracing_subscriber::filter::Targets::new()
        // Enable the `INFO` level for anything in `my_crate`
        .with_target("shadowquic_lib", Level::TRACE)
        .with_target("shadowquic_lib::msgs::socks", LevelFilter::OFF);

    // Enable the `DEBUG` level for a specific module.

    // Build a new subscriber with the `fmt` layer using the `Targets`
    // filter we constructed above.
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    // env_logger::init();
    trace!("Running");

    let mut socks_server = SocksServer::new("127.0.0.1:1089".parse().unwrap())
        .await
        .unwrap();
    let sq_client = ShadowQuicClient::new(
        "123".into(),
        "123".into(),
        "127.0.0.1:4444".parse().unwrap(),
        "localhost".into(),
        vec!["h3".into()],
        1200,
        "bbr".into(),
        true,
    );

    let client = Manager {
        inbound: Box::new(socks_server),
        outbound: Box::new(sq_client),
    };

    let sq_server = ShadowQuicServer::new(
        "127.0.0.1:4444".parse().unwrap(),
        "123".into(),
        "123".into(),
        "localhost:443".into(),
        vec!["h3".into()],
        true,
        "bbr".into(),
    )
    .unwrap();
    let direct_client = DirectOut;
    let server = Manager {
        inbound: Box::new(sq_server),
        outbound: Box::new(direct_client),
    };

    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(500)).await;
    client.run().await.unwrap();
}
