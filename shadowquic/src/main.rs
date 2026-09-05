use std::{io::IsTerminal, path::PathBuf};

use clap::{Parser, Subcommand};
use shadowquic::{
    config::{AuthUser, Config, LogLevel, OutboundCfg},
    shadowquic::outbound::ShadowQuicClient,
    squic::inbound::UserManager,
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
        /// Zero-based index of the instance (in config order) whose outbound
        /// is used for the API. Required when the config has multiple
        /// shadowquic/sunnyquic outbounds.
        #[clap(short, long)]
        instance: Option<usize>,
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
        Command::Api { instance, command } => {
            let outbound = match pick_quic_outbound(&cfg, instance) {
                Ok(outbound) => outbound,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            };
            if let Err(error) = call_api(outbound, command).await {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
}

/// Resolves the outbound for control-plane API calls.
///
/// With a single shadowquic/sunnyquic outbound it is picked automatically
/// (backward compatible). With several, the caller must name the instance
/// explicitly via `--instance <index>`; silently operating on the first one
/// would risk mutating the wrong remote user store.
fn pick_quic_outbound(cfg: &Config, instance: Option<usize>) -> Result<OutboundCfg, String> {
    let is_quic =
        |o: &OutboundCfg| matches!(o, OutboundCfg::ShadowQuic(_) | OutboundCfg::SunnyQuic(_));
    let quic_instances: Vec<usize> = cfg
        .instances
        .iter()
        .enumerate()
        .filter(|(_, i)| is_quic(&i.outbound))
        .map(|(i, _)| i)
        .collect();

    let index = match instance {
        Some(index) => index,
        None if quic_instances.is_empty() => {
            return Err("config has no shadowquic or sunnyquic outbound; \
                 api requires one of those outbound types"
                .into());
        }
        None if quic_instances.len() == 1 => quic_instances[0],
        None => {
            let mut msg = String::from(
                "config has multiple shadowquic/sunnyquic outbounds; \
                 select one with `--instance <index>` (zero-based):\n",
            );
            for &i in &quic_instances {
                msg.push_str(&format!(
                    "  instance {}: {} -> {}\n",
                    i,
                    cfg.instances[i].outbound.type_name(),
                    outbound_addr(&cfg.instances[i].outbound)
                ));
            }
            return Err(msg);
        }
    };

    match cfg.instances.get(index) {
        Some(inst) if is_quic(&inst.outbound) => Ok(inst.outbound.clone()),
        Some(inst) => Err(format!(
            "instance {index} has a {} outbound; api requires shadowquic or sunnyquic",
            inst.outbound.type_name()
        )),
        None => Err(format!(
            "instance index {index} out of range (config has {} instance(s))",
            cfg.instances.len()
        )),
    }
}

fn outbound_addr(outbound: &OutboundCfg) -> String {
    match outbound {
        OutboundCfg::ShadowQuic(cfg) => cfg.addr.clone(),
        OutboundCfg::SunnyQuic(cfg) => cfg.addr.clone(),
        OutboundCfg::Socks(cfg) => cfg.addr.clone(),
        OutboundCfg::Direct(_) => "-".into(),
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
    use shadowquic::config::{
        DirectOutCfg, InboundCfg, InstanceCfg, ShadowQuicClientCfg, SocksServerCfg,
    };

    fn config(pairs: Vec<(InboundCfg, OutboundCfg)>) -> Config {
        Config {
            instances: pairs
                .into_iter()
                .map(|(inbound, outbound)| InstanceCfg { inbound, outbound })
                .collect(),
            log_level: LogLevel::default(),
        }
    }

    fn socks_inbound(port: u16) -> InboundCfg {
        InboundCfg::Socks(SocksServerCfg {
            bind_addr: format!("127.0.0.1:{port}").parse().unwrap(),
            users: vec![],
        })
    }

    fn quic_outbound(port: u16) -> OutboundCfg {
        OutboundCfg::ShadowQuic(ShadowQuicClientCfg {
            addr: format!("127.0.0.1:{port}"),
            ..Default::default()
        })
    }

    fn direct_outbound() -> OutboundCfg {
        OutboundCfg::Direct(DirectOutCfg::default())
    }

    fn assert_quic_addr(outbound: OutboundCfg, addr: &str) {
        match outbound {
            OutboundCfg::ShadowQuic(cfg) => assert_eq!(cfg.addr, addr),
            other => panic!("expected shadowquic outbound, got {}", other.type_name()),
        }
    }

    #[test]
    fn api_errors_when_config_has_no_quic_outbound() {
        let cfg = config(vec![
            (socks_inbound(1080), direct_outbound()),
            (socks_inbound(1081), direct_outbound()),
        ]);
        let err = pick_quic_outbound(&cfg, None).unwrap_err();
        assert!(err.contains("no shadowquic"), "got: {err}");
    }

    #[test]
    fn api_requires_instance_flag_when_multiple_quic_outbounds() {
        let cfg = config(vec![
            (socks_inbound(1080), quic_outbound(1443)),
            (socks_inbound(1081), quic_outbound(1444)),
        ]);
        let err = pick_quic_outbound(&cfg, None).unwrap_err();
        assert!(err.contains("multiple"), "got: {err}");
        assert!(
            err.contains("instance 0") && err.contains("instance 1"),
            "error must list the candidates, got: {err}"
        );
    }

    #[test]
    fn api_rejects_non_quic_instance_target() {
        let cfg = config(vec![
            (socks_inbound(1080), quic_outbound(1443)),
            (socks_inbound(1081), direct_outbound()),
        ]);
        let err = pick_quic_outbound(&cfg, Some(1)).unwrap_err();
        assert!(err.contains("direct"), "got: {err}");
    }

    #[test]
    fn api_rejects_out_of_range_instance() {
        let cfg = config(vec![(socks_inbound(1080), quic_outbound(1443))]);
        let err = pick_quic_outbound(&cfg, Some(5)).unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }

    #[test]
    fn api_auto_selects_single_quic_outbound() {
        let cfg = config(vec![
            (socks_inbound(1080), direct_outbound()),
            (socks_inbound(1081), quic_outbound(1444)),
        ]);
        assert_quic_addr(pick_quic_outbound(&cfg, None).unwrap(), "127.0.0.1:1444");
    }

    #[test]
    fn api_selects_instance_by_index() {
        let cfg = config(vec![
            (socks_inbound(1080), quic_outbound(1443)),
            (socks_inbound(1081), quic_outbound(1444)),
        ]);
        assert_quic_addr(pick_quic_outbound(&cfg, Some(1)).unwrap(), "127.0.0.1:1444");
    }
}
