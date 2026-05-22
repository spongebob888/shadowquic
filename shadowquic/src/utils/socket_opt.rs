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
        let socket = if let Some(Interface::Address(ip)) = self.interface {
            let domain = if ip.is_ipv4() {
                Domain::IPV4
            } else {
                Domain::IPV6
            };
            let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
            let bind_addr = SocketAddr::new(ip, 0);
            socket.bind(&bind_addr.into())?;
            socket
        } else {
            let ipv6 = addr.is_ipv6();
            let try_create_dual_stack = || {
                let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
                socket.set_only_v6(false)?;
                let bind_addr: SocketAddr = "[::]:0".parse().unwrap();
                socket.bind(&bind_addr.into())?;
                Ok(socket) as Result<Socket, io::Error>
            };
            if let Ok(socket) = try_create_dual_stack() {
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
            }
        };

        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        {
            if let Some(fw_mark) = self.fw_mark {
                socket.set_mark(fw_mark)?;
            }
        }

        if let Some(Interface::Device(ref device_name)) = self.interface {
            crate::utils::platform::bind_device(&socket, device_name)?;
            tracing::debug!("socket bound to device {}", device_name);
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[tokio::test]
    async fn test_udp_socket_factory_creation() {
        // Create factory with no special options
        let factory = UdpSocketFactory {
            addr: "127.0.0.1:0".to_string(),
            interface: None,
            fw_mark: None,
            protect_path: None,
        };
        let socket = factory.create_socket().await.unwrap();
        assert!(socket.local_addr().is_ok());

        // Create factory with interface address
        let factory_ip = UdpSocketFactory {
            addr: "127.0.0.1:0".to_string(),
            interface: Some(Interface::Address("127.0.0.1".parse().unwrap())),
            fw_mark: None,
            protect_path: None,
        };
        let socket_ip = factory_ip.create_socket().await.unwrap();
        assert_eq!(
            socket_ip.local_addr().unwrap().as_socket().unwrap().ip(),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );

        // Create factory with firewall mark (only on supported platforms)
        let factory_mark = UdpSocketFactory {
            addr: "127.0.0.1:0".to_string(),
            interface: None,
            fw_mark: Some(123),
            protect_path: None,
        };

        let res = factory_mark.create_socket().await;
        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        {
            if let Err(e) = res {
                // Setting non-zero SO_MARK behavior varies by runtime/kernel (including cross/qemu).
                // It may fail with permission, unsupported, or invalid argument style errors.
                let kind = e.kind();
                let os_err = e.raw_os_error();
                assert!(
                    kind == std::io::ErrorKind::PermissionDenied
                        || kind == std::io::ErrorKind::Unsupported
                        || kind == std::io::ErrorKind::InvalidInput
                        || os_err == Some(1) // EPERM
                        || os_err == Some(13) // EACCES
                        || os_err == Some(22) // EINVAL
                        || os_err == Some(92) // ENOPROTOOPT
                        || os_err == Some(95) // EOPNOTSUPP
                );
            }
        }
        #[cfg(not(any(target_os = "android", target_os = "fuchsia", target_os = "linux")))]
        {
            // On unsupported platforms, the mark option is ignored, so it should succeed
            assert!(res.is_ok());
        }
    }
}
