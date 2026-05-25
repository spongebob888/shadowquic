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
    pub try_dual_stack: bool,
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
            if self.try_dual_stack
                && let Ok(socket) = try_create_dual_stack()
            {
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
        socket.set_nonblocking(true)?;
        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        {
            if let Some(fw_mark) = self.fw_mark {
                socket.set_mark(fw_mark)?;
            }
        }

        if let Some(Interface::Device(ref device_name)) = self.interface {
            if addr.ip().is_loopback() {
                tracing::debug!(
                    "skipping bind_device for udp socket to loopback destination {}",
                    addr
                );
            } else {
                crate::utils::platform::bind_device(&socket, device_name)?;
                tracing::debug!("udp socket bound to device {}", device_name);
            }
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

pub struct TcpSocketFactory {
    pub addr: String,
    pub interface: Option<Interface>,
    pub fw_mark: Option<u32>,
    pub protect_path: Option<PathBuf>,
}
#[async_trait::async_trait]
impl SocketFactory for TcpSocketFactory {
    async fn create_socket(&self) -> std::io::Result<socket2::Socket> {
        let addr = self
            .addr
            .to_socket_addrs()
            .unwrap_or_else(|_| panic!("resolve tcp addr faile: {}", self.addr))
            .next()
            .unwrap_or_else(|| panic!("resolve tcp addr faile: {}", self.addr));
        let socket = if let Some(Interface::Address(ip)) = self.interface {
            let domain = if ip.is_ipv4() {
                Domain::IPV4
            } else {
                Domain::IPV6
            };
            let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
            let bind_addr = SocketAddr::new(ip, 0);
            socket.bind(&bind_addr.into())?;
            socket
        } else {
            let ipv6 = addr.is_ipv6();
            if ipv6 {
                let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
                let bind_addr: SocketAddr = "[::]:0".parse().unwrap();
                socket.bind(&bind_addr.into())?;
                socket
            } else {
                let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
                let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
                socket.bind(&bind_addr.into())?;
                socket
            }
        };
        socket.set_nonblocking(true)?;

        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        {
            if let Some(fw_mark) = self.fw_mark {
                socket.set_mark(fw_mark)?;
            }
        }

        if let Some(Interface::Device(ref device_name)) = self.interface {
            if addr.ip().is_loopback() {
                tracing::debug!(
                    "skipping bind_device for tcp socket to loopback destination {}",
                    addr
                );
            } else {
                crate::utils::platform::bind_device(&socket, device_name)?;
                tracing::debug!("tcp socket bound to device {}", device_name);
            }
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
            try_dual_stack: true,
        };
        let socket = factory.create_socket().await.unwrap();
        assert!(socket.local_addr().is_ok());

        // Create factory with interface address
        let factory_ip = UdpSocketFactory {
            addr: "127.0.0.1:0".to_string(),
            interface: Some(Interface::Address("127.0.0.1".parse().unwrap())),
            fw_mark: None,
            protect_path: None,
            try_dual_stack: true,
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
            try_dual_stack: true,
        };

        let res = factory_mark.create_socket().await;
        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        {
            if let Err(e) = res {
                // Setting non-zero SO_MARK typically requires CAP_NET_ADMIN.
                // It should either succeed or fail with an OS-level error
                // (e.g. PermissionDenied/EPERM/EACCES, or ENOPROTOOPT in emulated
                // environments such as QEMU used by cross for armv7/aarch64).
                assert!(e.raw_os_error().is_some());
            }
        }
        #[cfg(not(any(target_os = "android", target_os = "fuchsia", target_os = "linux")))]
        {
            // On unsupported platforms, the mark option is ignored, so it should succeed
            assert!(res.is_ok());
        }
    }

    #[tokio::test]
    async fn test_tcp_socket_factory_creation() {
        // Create factory with no special options
        let factory = TcpSocketFactory {
            addr: "127.0.0.1:0".to_string(),
            interface: None,
            fw_mark: None,
            protect_path: None,
        };
        let socket = factory.create_socket().await.unwrap();
        assert!(socket.local_addr().is_ok());

        // Create factory with interface address
        let factory_ip = TcpSocketFactory {
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
        let factory_mark = TcpSocketFactory {
            addr: "127.0.0.1:0".to_string(),
            interface: None,
            fw_mark: Some(123),
            protect_path: None,
        };

        let res = factory_mark.create_socket().await;
        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        {
            if let Err(e) = res {
                assert!(e.raw_os_error().is_some());
            }
        }
        #[cfg(not(any(target_os = "android", target_os = "fuchsia", target_os = "linux")))]
        {
            assert!(res.is_ok());
        }
    }

    // Pick a real, existing non-loopback network interface name on Linux by
    // reading /sys/class/net. Returns None if no such interface is available
    // (in which case the test is skipped).
    #[cfg(target_os = "linux")]
    fn first_non_loopback_interface() -> Option<String> {
        std::fs::read_dir("/sys/class/net")
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|name| name != "lo")
    }

    // Verify that when an outbound interface (Interface::Device) is configured
    // with a *real* non-loopback interface, sockets targeting a loopback
    // address still work. Without the loopback skip in create_socket(), the
    // socket would be restricted to the named device by SO_BINDTODEVICE and
    // would not be able to reach 127.0.0.1, so the end-to-end send/recv below
    // would time out.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_udp_socket_factory_device_loopback() {
        let Some(iface) = first_non_loopback_interface() else {
            eprintln!("no non-loopback interface available; skipping");
            return;
        };

        // Bind a receiver on loopback.
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = receiver.local_addr().unwrap();

        let factory = UdpSocketFactory {
            addr: recv_addr.to_string(),
            interface: Some(Interface::Device(iface.clone())),
            fw_mark: None,
            protect_path: None,
            try_dual_stack: false,
        };
        let socket = factory
            .create_socket()
            .await
            .expect("create_socket must succeed for loopback destination");
        let std_socket: std::net::UdpSocket = socket.into();
        let sender = tokio::net::UdpSocket::from_std(std_socket).unwrap();

        // If bind_device had been applied for the non-loopback interface, this
        // send would fail (Network unreachable) because loopback traffic does
        // not flow through that device. The loopback skip in create_socket()
        // is what makes this work.
        sender
            .send_to(b"hello", recv_addr)
            .await
            .expect("send to loopback must succeed when bind_device is skipped");

        let mut buf = [0u8; 16];
        let (n, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("receive must not time out")
        .unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_tcp_socket_factory_device_loopback() {
        let Some(iface) = first_non_loopback_interface() else {
            eprintln!("no non-loopback interface available; skipping");
            return;
        };

        // Start a TCP listener on loopback.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();

        let factory = TcpSocketFactory {
            addr: listen_addr.to_string(),
            interface: Some(Interface::Device(iface.clone())),
            fw_mark: None,
            protect_path: None,
        };
        let socket = factory
            .create_socket()
            .await
            .expect("create_socket must succeed for loopback destination");

        // Initiate a non-blocking connect on the socket2 Socket directly.
        // create_socket() already set it non-blocking, so the call typically
        // returns WouldBlock/InProgress and the connection completes
        // asynchronously. If bind_device had been applied to a non-loopback
        // interface, the connect would fail synchronously (ENETUNREACH) and no
        // SYN would reach the listener, so the accept below would time out.
        let _ = socket.connect(&listen_addr.into());

        let accept = tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
            .await
            .expect("accept must not time out (bind_device must be skipped for loopback)");
        assert!(accept.is_ok(), "loopback connect must succeed");
    }

    // On non-Linux platforms, retain a basic smoke test that exercises the
    // loopback-skip path with a nonexistent device name: create_socket must
    // still succeed because bind_device is skipped for loopback destinations.
    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn test_udp_socket_factory_device_loopback() {
        let factory = UdpSocketFactory {
            addr: "127.0.0.1:0".to_string(),
            interface: Some(Interface::Device("nonexistent_device_name_123".to_string())),
            fw_mark: None,
            protect_path: None,
            try_dual_stack: false,
        };
        let socket = factory.create_socket().await.unwrap();
        assert!(socket.local_addr().is_ok());
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn test_tcp_socket_factory_device_loopback() {
        let factory = TcpSocketFactory {
            addr: "127.0.0.1:0".to_string(),
            interface: Some(Interface::Device("nonexistent_device_name_123".to_string())),
            fw_mark: None,
            protect_path: None,
        };
        let socket = factory.create_socket().await.unwrap();
        assert!(socket.local_addr().is_ok());
    }
}
