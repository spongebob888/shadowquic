use rand::Rng;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::utils::port_union::PortUnion;

struct ProxyState {
    current_target_port: u16,
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

async fn pick_reachable_base_addr(host_addrs: &[SocketAddr]) -> std::io::Result<SocketAddr> {
    let mut candidates_v6 = Vec::new();
    let mut candidates_v4 = Vec::new();

    for addr in host_addrs.iter().copied() {
        if addr.is_ipv6() {
            candidates_v6.push(addr);
        } else if addr.is_ipv4() {
            candidates_v4.push(addr);
        }
    }

    for addr in candidates_v6.iter().chain(candidates_v4.iter()) {
        let bind_addr = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };

        let sock = match UdpSocket::bind(bind_addr).await {
            Ok(s) => s,
            Err(_) => continue,
        };

        if sock.connect(*addr).await.is_ok() {
            return Ok(*addr);
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AddrNotAvailable,
        "no reachable resolved address",
    ))
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

        let base_addr = pick_reachable_base_addr(&host_addrs).await?;
        let is_ipv6 = base_addr.is_ipv6();

        info!(
            "UdpHop selected reachable base address {} (family: {})",
            base_addr,
            if base_addr.is_ipv6() { "IPv6" } else { "IPv4" }
        );
        let local_socket =
            Arc::new(UdpSocket::bind(if is_ipv6 { "[::1]:0" } else { "127.0.0.1:0" }).await?);
        let local_port = local_socket.local_addr()?;

        let internet_socket =
            Arc::new(UdpSocket::bind(if is_ipv6 { "[::]:0" } else { "0.0.0.0:0" }).await?);

        let quinn_addr = Arc::new(RwLock::new(None));

        let current_target_port = {
            let mut rng = rand::rng();
            addr.ports.random_port(&mut rng).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty port set")
            })?
        };

        let min_port = addr.ports.min_port().unwrap_or(0);
        let max_port = addr.ports.max_port().unwrap_or(0);

        info!(
            "UdpHop initialized with {} target ports (from {} to {})",
            addr.ports.count(),
            min_port,
            max_port
        );

        let state = Arc::new(RwLock::new(ProxyState {
            current_target_port,
        }));

        let expected_ip = base_addr.ip();
        let internet_socket_recv = internet_socket.clone();
        let local_socket_recv = local_socket.clone();
        let quinn_addr_recv = quinn_addr.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 65535];
            loop {
                match internet_socket_recv.recv_from(&mut buf).await {
                    Ok((len, peer_addr)) => {
                        if peer_addr.ip() != expected_ip {
                            debug!(
                                "Dropping UDP hop packet from unexpected peer IP {}, expected {}",
                                peer_addr.ip(),
                                expected_ip
                            );
                            continue;
                        }

                        let qa = quinn_addr_recv.read().await;
                        if let Some(addr) = *qa
                            && let Err(e) = local_socket_recv.send_to(&buf[..len], addr).await
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
        });

        let state_hop = state.clone();
        let ports = addr.ports.clone();

        tokio::spawn(async move {
            loop {
                let interval = select_hop_interval_ms(min_hop_interval, max_hop_interval);
                tokio::time::sleep(Duration::from_millis(interval)).await;

                let mut st = state_hop.write().await;
                let current = st.current_target_port;

                let new_port = {
                    let mut rng = rand::rng();
                    let mut selected = current;

                    for _ in 0..8 {
                        match ports.random_port(&mut rng) {
                            Some(port) => {
                                selected = port;
                                if port != current || ports.count() <= 1 {
                                    break;
                                }
                            }
                            None => {
                                error!("UDP hop port set is empty");
                                selected = current;
                                break;
                            }
                        }
                    }

                    selected
                };

                if new_port == st.current_target_port {
                    debug!("UDP hop selected the same target port: {}", new_port);
                    continue;
                }

                st.current_target_port = new_port;
                debug!("Hopped to new target port: {}", new_port);
            }
        });

        let state_local = state.clone();
        let internet_socket_send = internet_socket.clone();

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
                        let mut target = base_addr;
                        target.set_port(st.current_target_port);
                        drop(st);

                        if len > 1500 {
                            debug!(
                                "Warning: Large UDP packet received from local Quinn: {} bytes",
                                len
                            );
                        }

                        if let Err(e) = internet_socket_send.send_to(&buf[..len], target).await {
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

        info!(
            "UdpHop proxy started on {}, internet socket {}",
            local_port,
            internet_socket.local_addr()?
        );

        Ok(local_port)
    }
}
