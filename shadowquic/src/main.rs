use std::{io::IsTerminal, path::PathBuf};

use clap::Parser;
use shadowquic::config::{Config, LogLevel};
use tracing::{Level, info};
use tracing_subscriber::{fmt::time::LocalTime, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[clap(author, about, long_about = None, version)]
struct Cli {
    #[clap(
        short,
        long,
        visible_short_aliases = ['c'],
        value_parser,
        value_name = "FILE",
        default_value = "config.yaml",
        help = "configuration file"
    )]
    config: PathBuf,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    let cli = Cli::parse();
    let content = std::fs::read_to_string(cli.config).expect("can't open config yaml file");
    let cfg: Config = serde_yaml::from_str(&content).expect("invalid yaml file content");
    setup_log(cfg.log_level.clone());
    let manager = cfg
        .build_manager()
        .await
        .expect("creating inbound/outbound failed");

    info!("shadowquic {} running", env!("CARGO_PKG_VERSION"));
    let _ = std::env::current_dir().inspect(|x| info!("current working directory: {:?}", x));
    manager.run().await.expect("shadowquic stopped");
}

fn setup_log(level: LogLevel) {
    let filter = tracing_subscriber::filter::Targets::new()
        // Enable the `INFO` level for anything in `my_crate`
        .with_target("shadowquic", level.as_tracing_level())
        .with_target(
            "quinn",
            std::cmp::min(Level::WARN, level.as_tracing_level()),
        );

    #[cfg(feature = "tokio-console")]
    let filter = filter
        .with_target("tokio", Level::TRACE)
        .with_target("runtime", Level::TRACE);
    #[cfg(feature = "tokio-console")]
    let console_layer = console_subscriber::spawn();

    let timer = LocalTime::new(time::macros::format_description!(
        "[year repr:last_two]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]"
    ));

    let fmt = tracing_subscriber::fmt::Layer::new()
        .with_timer(timer)
        .with_ansi(std::io::stdout().is_terminal())
        //.compact()
        .with_target(cfg!(debug_assertions))
        .with_file(false)
        .with_line_number(false)
        .with_level(true)
        .with_writer(std::io::stdout);
    let sub = tracing_subscriber::registry().with(fmt).with(filter);
    #[cfg(feature = "tokio-console")]
    let sub = sub.with(console_layer);
    sub.init();
}
