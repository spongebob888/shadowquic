use std::{
    io,
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
};

use socket2::{Domain, Protocol, Socket, Type};

use crate::config::Interface;

#[async_trait::async_trait]
pub trait SocketFactory: Send + Sync {
    async fn create_socket(&self) -> std::io::Result<socket2::Socket>;
}
pub struct UdpSocketFactory {
    pub addr: String,
    pub interface: Option<Interface>,
    pub fw_mark: Option<u32>,
    pub protect_path: Option<PathBuf>,
}
#[async_trait::async_trait]
impl SocketFactory for UdpSocketFactory {
    async fn create_socket(&self) -> std::io::Result<socket2::Socket> {
        let addr = self
            .addr
            .to_socket_addrs()
            .unwrap_or_else(|_| panic!("resolve quic addr faile: {}", self.addr))
            .next()
            .unwrap_or_else(|| panic!("resolve quic addr faile: {}", self.addr));
        let ipv6 = addr.is_ipv6();
        let try_create_dual_stack = || {
            let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            socket.set_only_v6(false)?;
            let bind_addr: SocketAddr = "[::]:0".parse().unwrap();
            socket.bind(&bind_addr.into())?;
            Ok(socket) as Result<Socket, io::Error>
        };
        let socket = if let Ok(socket) = try_create_dual_stack() {
            tracing::trace!("dual stack udp socket created");
            socket
        } else if ipv6 {
            let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            let bind_addr: SocketAddr = "[::]:0".parse().unwrap();
            socket.bind(&bind_addr.into())?;
            socket
        } else {
            let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
            socket.bind(&bind_addr.into())?;
            socket
        };

        #[cfg(target_os = "android")]
        if let Some(path) = &self.protect_path {
            use crate::utils::protect_socket::protect_socket_with_retry;
            use std::os::fd::AsRawFd;

            tracing::debug!("trying protect socket");
            tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                protect_socket_with_retry(path, socket.as_raw_fd()),
            )
            .await
            .map_err(|_| io::Error::other("protecting socket timeout"))
            .and_then(|x| x)
            .map_err(|e| {
                tracing::error!("error during protecing socket:{}", e);
                e
            })?;
        }

        Ok(socket)
    }
}
