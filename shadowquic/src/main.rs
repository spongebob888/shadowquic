use std::{io::IsTerminal, path::PathBuf};

use clap::{Parser, Subcommand};
use shadowquic::{
    config::{AuthUser, Config, LogLevel, OutboundCfg},
    shadowquic::outbound::ShadowQuicClient,
    squic::inbound::UserManager,
    sunnyquic::outbound::SunnyQuicClient,
};
use tracing::{Instrument, Level, info};
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
        help = "configuration file (repeat to run multiple instances)"
    )]
    config: Vec<PathBuf>,

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
    /// Get stats for a user, or all users when username is omitted
    #[command(name = "get-stats")]
    GetUserStats { username: Option<String> },
    /// Kill all online connections for a user
    #[command(name = "kill-conn")]
    KillUserConn { username: String },
    /// Clear traffic stats for a user, or all users when username is omitted
    #[command(name = "clear-stats")]
    ClearUserStats { username: Option<String> },
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    if matches!(cli.command, Some(Command::Api { .. })) && cli.config.len() != 1 {
        panic!("api requires exactly one configuration file");
    }
    // Validate every config before opening any listeners.
    let configs = cli
        .config
        .into_iter()
        .map(|path| {
            let content = std::fs::read_to_string(&path)
                .map_err(|error| format!("can't open config {}: {error}", path.display()))?;
            let cfg: Config = serde_saphyr::from_str(&content)
                .map_err(|error| format!("failed to parse config {}: {error}", path.display()))?;
            Ok((path, cfg))
        })
        .collect::<Result<Vec<_>, String>>()?;
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => {
            let level = configs
                .iter()
                .filter_map(|(_, cfg)| cfg.log_level.clone())
                .max_by_key(LogLevel::as_tracing_level)
                .unwrap_or_default();
            setup_log(level);
            let mut managers = Vec::with_capacity(configs.len());
            for (path, cfg) in configs {
                let span = tracing::info_span!(
                    "instance",
                    listen = %cfg.inbound.bind_addr(),
                );
                let manager =
                    cfg.build_manager()
                        .instrument(span.clone())
                        .await
                        .map_err(|error| {
                            format!("creating instance {} failed: {error}", path.display())
                        })?;
                managers.push((path, manager, span));
            }

            info!("shadowquic {} running", env!("CARGO_PKG_VERSION"));
            let _ =
                std::env::current_dir().inspect(|x| info!("current working directory: {:?}", x));
            let mut instances = tokio::task::JoinSet::new();
            for (path, manager, span) in managers {
                instances.spawn(
                    async move {
                        manager.run().await.map_err(|error| {
                            format!("instance {} stopped: {error}", path.display())
                        })
                    }
                    .instrument(span),
                );
            }
            // Wait for every instance to flush its state on graceful shutdown.
            // A fatal error aborts the other instances when the set is dropped.
            while let Some(result) = instances.join_next().await {
                result.map_err(|error| format!("instance task failed: {error}"))??;
            }
            Ok(())
        }
        Command::Api { command } => {
            let (_, cfg) = configs.into_iter().next().expect("one API config");
            call_api(cfg.outbound, command).await
        }
    }
}

async fn call_api(outbound: OutboundCfg, command: ApiCommand) -> Result<(), String> {
    match outbound {
        OutboundCfg::ShadowQuic(cfg) => {
            let client = ShadowQuicClient::new(cfg);
            call_user_manager_api(&client, command).await
        }
        OutboundCfg::SunnyQuic(cfg) => {
            let client = SunnyQuicClient::new(cfg);
            call_user_manager_api(&client, command).await
        }
        OutboundCfg::Socks(_) | OutboundCfg::Direct(_) => {
            Err("api requires a shadowquic or sunnyquic outbound config".into())
        }
    }
}

async fn call_user_manager_api(
    user_manager: &impl UserManager,
    command: ApiCommand,
) -> Result<(), String> {
    match command {
        ApiCommand::ListUsers => {
            let users = user_manager
                .list_users()
                .await
                .map_err(|error| format!("list-users failed: {error:?}"))?;
            for user in users {
                println!("{user}");
            }
            Ok(())
        }
        ApiCommand::AddUser { username, password } => {
            user_manager
                .add_user(AuthUser {
                    username: username.clone(),
                    password,
                })
                .await
                .map_err(|error| format!("add-user failed: {error:?}"))?;
            println!("user added: {username}");
            Ok(())
        }
        ApiCommand::RemoveUser { username } => {
            user_manager
                .remove_user(&username)
                .await
                .map_err(|error| format!("remove-user failed: {error:?}"))?;
            println!("user removed: {username}");
            Ok(())
        }
        ApiCommand::GetUserStats {
            username: Some(username),
        } => {
            let stats = user_manager
                .get_user_stats(&username)
                .await
                .map_err(|error| format!("get-stats of user {username} failed: {error:?}"))?;
            print_user_stats(&username, &stats);
            Ok(())
        }
        ApiCommand::GetUserStats { username: None } => {
            let stats = user_manager
                .get_all_stats()
                .await
                .map_err(|error| format!("get-stats of all users failed: {error:?}"))?;
            for (index, user_stats) in stats.iter().enumerate() {
                if index > 0 {
                    println!();
                }
                print_user_stats(&user_stats.username, user_stats);
            }
            Ok(())
        }
        ApiCommand::KillUserConn { username } => {
            user_manager
                .kill_user_conns(&username)
                .await
                .map_err(|error| format!("kill-conn of user {username} failed: {error:?}"))?;
            println!("user connections killed: {username}");
            Ok(())
        }
        ApiCommand::ClearUserStats {
            username: Some(username),
        } => {
            user_manager
                .clear_user_stats(&username)
                .await
                .map_err(|error| format!("clear-stats of user {username} failed: {error:?}"))?;
            println!("user stats cleared: {username}");
            Ok(())
        }
        ApiCommand::ClearUserStats { username: None } => {
            user_manager
                .clear_all_stats()
                .await
                .map_err(|error| format!("clear-stats of all users failed: {error:?}"))?;
            println!("all user stats cleared");
            Ok(())
        }
    }
}

fn print_user_stats(username: &str, stats: &shadowquic::msgs::squic::UserStats) {
    println!("username: {username}");
    println!("conn_num: {}", stats.conn_num);
    println!("tcp_conns: {}", stats.tcp_conns);
    println!("tcp_sent: {}", stats.tcp_sent);
    println!("tcp_recv: {}", stats.tcp_recv);
    println!("udp_conns: {}", stats.udp_conns);
    println!("udp_sent: {}", stats.udp_sent);
    println!("udp_recv: {}", stats.udp_recv);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_log_levels_take_precedence_over_missing_levels() {
        let base =
            "inbound:\n  type: socks\n  bind-addr: '127.0.0.1:1080'\noutbound:\n  type: direct\n";
        for (levels, expected) in [
            (["", "log-level: warn\n"], Level::WARN),
            (["log-level: error\n", "log-level:\n"], Level::ERROR),
            (["", "log-level: null\n"], Level::INFO),
            (["log-level: info\n", "log-level: warn\n"], Level::INFO),
        ] {
            let configs: Vec<Config> = levels
                .iter()
                .map(|level| serde_saphyr::from_str(&format!("{base}{level}")).unwrap())
                .collect();
            let selected = configs
                .iter()
                .filter_map(|cfg| cfg.log_level.clone())
                .max_by_key(LogLevel::as_tracing_level)
                .unwrap_or_default();
            assert_eq!(selected.as_tracing_level(), expected);
        }
    }

    #[test]
    fn config_arguments() {
        for (args, expected) in [
            (vec!["shadowquic"], vec!["config.yaml"]),
            (vec!["shadowquic", "-c", "one.yaml"], vec!["one.yaml"]),
            (
                vec!["shadowquic", "-c", "one.yaml", "--config", "two.yaml"],
                vec!["one.yaml", "two.yaml"],
            ),
            (
                vec!["shadowquic", "run", "-c", "one.yaml", "-c", "two.yaml"],
                vec!["one.yaml", "two.yaml"],
            ),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(
                cli.config,
                expected.into_iter().map(PathBuf::from).collect::<Vec<_>>()
            );
        }
    }

    #[tokio::test]
    #[should_panic(expected = "api requires exactly one configuration file")]
    async fn api_rejects_multiple_configs_before_reading_files() {
        let cli = Cli::try_parse_from([
            "shadowquic",
            "-c",
            "one.yaml",
            "-c",
            "two.yaml",
            "api",
            "list-users",
        ])
        .unwrap();
        let _ = run(cli).await;
    }
}
