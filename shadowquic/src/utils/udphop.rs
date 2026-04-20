use rand::Rng;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::utils::port_union::PortUnion;

const DRAINING_SECS: u64 = 10;

struct ManagedSocket {
    socket: Arc<UdpSocket>,
    task: JoinHandle<()>,
}

struct DrainingSocket {
    managed: ManagedSocket,
    expires_at: Instant,
}

struct ProxyState {
    current: ManagedSocket,
    current_target_port: u16,
    draining: Vec<DrainingSocket>,
}

pub struct UdpHopAddr {
    pub host: String,
    pub ports: PortUnion,
}

impl UdpHopAddr {
    pub fn min_port(&self) -> Option<u16> {
        self.ports.min_port()
    }

    pub fn max_port(&self) -> Option<u16> {
        self.ports.max_port()
    }

    pub fn hop_enabled_with_interval(&self, hop_interval: u32) -> bool {
        self.ports.count() > 1 || hop_interval > 0
    }

    pub fn first_resolved_addrs(&self) -> std::io::Result<Vec<SocketAddr>> {
        let port = self.min_port().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty port set")
        })?;

        let host_port = format!("{}:{}", self.host, port);
        host_port.to_socket_addrs().map(|iter| iter.collect())
    }

    pub fn first_resolved_addr(&self) -> std::io::Result<SocketAddr> {
        self.first_resolved_addrs()?
            .into_iter()
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "host not found"))
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.rsplitn(2, ':').collect();
        if parts.len() != 2 {
            return Err("Invalid address format".into());
        }

        let port_str = parts[0];
        let host = parts[1].to_string();
        let ports: PortUnion = port_str.parse()?;

        Ok(Self { host, ports })
    }
}

pub struct UdpHopClientProxy;

fn select_hop_interval_ms(min_hop_interval: u32, max_hop_interval: u32) -> u64 {
    let (min_interval, max_interval) = if min_hop_interval <= max_hop_interval {
        (min_hop_interval, max_hop_interval)
    } else {
        warn!(
            "Invalid hop interval range: min_hop_interval ({}) > max_hop_interval ({}), swapping them",
            min_hop_interval, max_hop_interval
        );
        (max_hop_interval, min_hop_interval)
    };

    rand::rng().random_range(min_interval..=max_interval) as u64
}

async fn bind_and_connect_socket(
    is_ipv6: bool,
    target: SocketAddr,
) -> Result<Arc<UdpSocket>, std::io::Error> {
    let socket = Arc::new(UdpSocket::bind(if is_ipv6 { "[::]:0" } else { "0.0.0.0:0" }).await?);
    socket.connect(target).await?;
    Ok(socket)
}

impl UdpHopClientProxy {
    pub async fn start(
        addr: &UdpHopAddr,
        min_hop_interval: u32,
        max_hop_interval: u32,
    ) -> Result<SocketAddr, std::io::Error> {
        let host_addrs = tokio::net::lookup_host(format!("{}:0", addr.host))
            .await?
            .collect::<Vec<_>>();

        if host_addrs.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Host not found",
            ));
        }

        let base_addr = host_addrs[0];
        let is_ipv6 = base_addr.is_ipv6();

        let local_socket =
            Arc::new(UdpSocket::bind(if is_ipv6 { "[::1]:0" } else { "127.0.0.1:0" }).await?);
        let local_port = local_socket.local_addr()?;

        let quinn_addr = Arc::new(RwLock::new(None));

        let current_target_port = {
            let mut rng = rand::rng();
            addr.ports.random_port(&mut rng).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty port set")
            })?
        };

        let mut current_target = base_addr;
        current_target.set_port(current_target_port);

        let current_socket = bind_and_connect_socket(is_ipv6, current_target).await?;
        let current_task = spawn_internet_receiver(
            current_socket.clone(),
            local_socket.clone(),
            quinn_addr.clone(),
        );

        let min_port = addr.ports.min_port().unwrap_or(0);
        let max_port = addr.ports.max_port().unwrap_or(0);

        info!(
            "UdpHop initialized with {} target ports (from {} to {})",
            addr.ports.count(),
            min_port,
            max_port
        );

        let state = Arc::new(RwLock::new(ProxyState {
            current: ManagedSocket {
                socket: current_socket,
                task: current_task,
            },
            current_target_port,
            draining: Vec::new(),
        }));

        let state_hop = state.clone();
        let local_socket_hop = local_socket.clone();
        let quinn_addr_hop = quinn_addr.clone();
        let ports = addr.ports.clone();

        tokio::spawn(async move {
            loop {
                let interval = select_hop_interval_ms(min_hop_interval, max_hop_interval);
                tokio::time::sleep(Duration::from_millis(interval)).await;

                let new_port = {
                    let mut rng = rand::rng();
                    match ports.random_port(&mut rng) {
                        Some(port) => port,
                        None => {
                            error!("UDP hop port set is empty");
                            continue;
                        }
                    }
                };
                let mut new_target = base_addr;
                new_target.set_port(new_port);

                let new_socket = match bind_and_connect_socket(is_ipv6, new_target).await {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Failed to bind/connect new socket for hop: {}", e);
                        continue;
                    }
                };

                let new_task = spawn_internet_receiver(
                    new_socket.clone(),
                    local_socket_hop.clone(),
                    quinn_addr_hop.clone(),
                );

                let mut st = state_hop.write().await;

                cleanup_draining_sockets(&mut st.draining);

                let old_current = std::mem::replace(
                    &mut st.current,
                    ManagedSocket {
                        socket: new_socket,
                        task: new_task,
                    },
                );

                st.draining.push(DrainingSocket {
                    managed: old_current,
                    expires_at: Instant::now() + Duration::from_secs(DRAINING_SECS),
                });

                st.current_target_port = new_port;

                debug!(
                    "Hopped to new socket, new target port: {}, draining old socket for {}s",
                    new_port, DRAINING_SECS
                );
            }
        });

        let state_local = state.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 65535];
            loop {
                match local_socket.recv_from(&mut buf).await {
                    Ok((len, src)) => {
                        let mut qa = quinn_addr.write().await;
                        if qa.is_none() || qa.unwrap() != src {
                            *qa = Some(src);
                        }
                        drop(qa);

                        let st = state_local.read().await;
                        let socket = st.current.socket.clone();
                        drop(st);

                        if len > 1500 {
                            debug!(
                                "Warning: Large UDP packet received from local Quinn: {} bytes",
                                len
                            );
                        }

                        if let Err(e) = socket.send(&buf[..len]).await {
                            error!("Failed to forward packet to internet: {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Local proxy socket recv error: {}", e);
                        break;
                    }
                }
            }
        });

        info!("UdpHop proxy started on {}", local_port);
        Ok(local_port)
    }
}

fn spawn_internet_receiver(
    socket: Arc<UdpSocket>,
    local_socket: Arc<UdpSocket>,
    quinn_addr: Arc<RwLock<Option<SocketAddr>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 65535];
        loop {
            match socket.recv(&mut buf).await {
                Ok(len) => {
                    let qa = quinn_addr.read().await;
                    if let Some(addr) = *qa
                        && let Err(e) = local_socket.send_to(&buf[..len], addr).await
                    {
                        error!("Failed to forward packet to local Quinn: {}", e);
                    }
                }
                Err(e) => {
                    debug!("UDP hop receiver exited: {}", e);
                    break;
                }
            }
        }
    })
}

fn cleanup_draining_sockets(draining: &mut Vec<DrainingSocket>) {
    let now = Instant::now();
    let mut i = 0;

    while i < draining.len() {
        if draining[i].expires_at <= now {
            draining[i].managed.task.abort();
            draining.swap_remove(i);
        } else {
            i += 1;
        }
    }
}
