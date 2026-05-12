use rand::Rng;
use std::{net::UdpSocket, time::Duration};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::format_duration;

/// QUIC endpoint socket rebinding abstraction
pub trait Rebindable {
    fn rebind(&self, socket: UdpSocket) -> Result<(), std::io::Error>;
}

#[derive(Clone, Debug)]
pub struct RebindConfig {
    pub interval_ms: Option<u32>,
    pub min_ms: Option<u32>,
    pub max_ms: Option<u32>,
}

impl RebindConfig {
    pub fn range(&self) -> Option<(u32, u32)> {
        let min = self.min_ms.or(self.interval_ms);
        let max = self.max_ms.or(self.interval_ms);
        match (min, max) {
            (None, None) => None,
            (Some(m), None) | (None, Some(m)) => Some((m, m)),
            (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
        }
    }
}

pub struct RebindEndpoint<E> {
    endpoint: E,
    rebind_handle: Option<tokio::task::JoinHandle<()>>,
}

impl<E> RebindEndpoint<E> {
    pub fn endpoint(&self) -> &E {
        &self.endpoint
    }

    pub fn without_rebind(endpoint: E) -> Self {
        Self {
            endpoint,
            rebind_handle: None,
        }
    }
}

impl<E> RebindEndpoint<E>
where
    E: Rebindable + Send + Sync + Clone + 'static,
{
    pub fn with_rebind(endpoint: E, cfg: RebindConfig, bind_ipv6: bool) -> Self {
        // Clone a handle for the background rebind task; keep the original in ManagedEndpoint.
        let ep = endpoint.clone();

        let handle = cfg.range().and_then(move |(lo, hi)| {
            if hi == 0 {
                warn!("rebind_interval is 0, disabling rebind");
                return None;
            }

            info!(
                "rebind enabled (interval: {} - {})",
                format_duration(lo),
                format_duration(hi)
            );

            let bind_addr = if bind_ipv6 { "[::]:0" } else { "0.0.0.0:0" };

            Some(tokio::spawn(async move {
                loop {
                    let interval_ms = {
                        let mut rng = rand::rng();
                        rng.random_range(lo..=hi) as u64
                    };

                    sleep(Duration::from_millis(interval_ms)).await;

                    match tokio::net::UdpSocket::bind(bind_addr).await {
                        Ok(tokio_socket) => match tokio_socket.into_std() {
                            Ok(std_socket) => {
                                if let Err(e) = ep.rebind(std_socket) {
                                    error!("rebind failed: {}", e);
                                } else {
                                    info!("rebound to new local port");
                                }
                            }
                            Err(e) => error!("into_std failed for rebind socket: {}", e),
                        },
                        Err(e) => error!("failed to bind socket for rebind: {}", e),
                    }
                }
            }))
        });

        Self {
            endpoint,
            rebind_handle: handle,
        }
    }
}

impl<E> Drop for RebindEndpoint<E> {
    fn drop(&mut self) {
        if let Some(h) = self.rebind_handle.take() {
            h.abort();
        }
    }
}
