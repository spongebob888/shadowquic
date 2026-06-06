use std::{io::IsTerminal, path::PathBuf};

use clap::{Parser, Subcommand};
use shadowquic::{
    config::{Config, LogLevel, OutboundCfg},
    shadowquic::outbound::ShadowQuicClient,
    sunnyquic::outbound::SunnyQuicClient,
};
use tracing::{Level, info};
use tracing_subscriber::{fmt::time::LocalTime, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[clap(author, about, long_about = None, version)]
struct Cli {
    #[clap(
        short,
        long,
        global = true,
        visible_short_aliases = ['c'],
        value_parser,
        value_name = "FILE",
        default_value = "config.yaml",
        help = "configuration file"
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy
    Run,
    /// Call SQuic control-plane APIs using the outbound config
    /// The username must be admin or permission will get denied by server.
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
}

#[derive(Subcommand)]
#[command(rename_all = "kebab-case")]
enum ApiCommand {
    /// List usernames
    ListUsers,
    /// Add or update a user
    AddUser { username: String, password: String },
    /// Remove a user
    RemoveUser { username: String },
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    let cli = Cli::parse();
    let content = std::fs::read_to_string(cli.config).expect("can't open config yaml file");
    let cfg: Config = match serde_saphyr::from_str(&content) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("failed to parse config file: {e}");
            std::process::exit(1);
        }
    };
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => {
            setup_log(cfg.log_level.clone());
            let manager = cfg
                .build_manager()
                .await
                .expect("creating inbound/outbound failed");

            info!("shadowquic {} running", env!("CARGO_PKG_VERSION"));
            let _ =
                std::env::current_dir().inspect(|x| info!("current working directory: {:?}", x));
            manager.run().await.expect("shadowquic stopped");
        }
        Command::Api { command } => {
            if let Err(error) = call_api(cfg.outbound, command).await {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
}

async fn call_api(outbound: OutboundCfg, command: ApiCommand) -> Result<(), String> {
    match outbound {
        OutboundCfg::ShadowQuic(cfg) => {
            let mut client = ShadowQuicClient::new(cfg);
            match command {
                ApiCommand::ListUsers => {
                    let users = client
                        .list_users()
                        .await
                        .map_err(|error| error.to_string())?;
                    match users {
                        Ok(users) => {
                            for user in users {
                                println!("{user}");
                            }
                            Ok(())
                        }
                        Err(error) => Err(format!("list-users failed: {error:?}")),
                    }
                }
                ApiCommand::AddUser { username, password } => {
                    let result = client
                        .add_user(&username, &password)
                        .await
                        .map_err(|error| error.to_string())?;
                    result
                        .map(|()| println!("user added: {username}"))
                        .map_err(|error| format!("add-user failed: {error:?}"))
                }
                ApiCommand::RemoveUser { username } => {
                    let result = client
                        .remove_user(&username)
                        .await
                        .map_err(|error| error.to_string())?;
                    result
                        .map(|()| println!("user removed: {username}"))
                        .map_err(|error| format!("remove-user failed: {error:?}"))
                }
            }
        }
        OutboundCfg::SunnyQuic(cfg) => {
            let mut client = SunnyQuicClient::new(cfg);
            match command {
                ApiCommand::ListUsers => {
                    let users = client
                        .list_users()
                        .await
                        .map_err(|error| error.to_string())?;
                    match users {
                        Ok(users) => {
                            for user in users {
                                println!("{user}");
                            }
                            Ok(())
                        }
                        Err(error) => Err(format!("list-users failed: {error:?}")),
                    }
                }
                ApiCommand::AddUser { username, password } => {
                    let result = client
                        .add_user(&username, &password)
                        .await
                        .map_err(|error| error.to_string())?;
                    result
                        .map(|()| println!("user added: {username}"))
                        .map_err(|error| format!("add-user failed: {error:?}"))
                }
                ApiCommand::RemoveUser { username } => {
                    let result = client
                        .remove_user(&username)
                        .await
                        .map_err(|error| error.to_string())?;
                    result
                        .map(|()| println!("user removed: {username}"))
                        .map_err(|error| format!("remove-user failed: {error:?}"))
                }
            }
        }
        OutboundCfg::Socks(_) | OutboundCfg::Direct(_) => {
            Err("api requires a shadowquic or sunnyquic outbound config".into())
        }
    }
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
