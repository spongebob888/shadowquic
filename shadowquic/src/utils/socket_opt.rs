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
            crate::utils::platform::bind_device(&socket, device_name)?;
            tracing::debug!("udp socket bound to device {}", device_name);
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
            crate::utils::platform::bind_device(&socket, device_name)?;
            tracing::debug!("tcp socket bound to device {}", device_name);
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

    /// Returns the local IP address that would be used when connecting to 1.1.1.1,
    /// by using the OS routing table via a non-blocking UDP connect (no packets sent).
    fn get_local_ip() -> Option<IpAddr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("1.1.1.1:53").ok()?;
        socket.local_addr().ok().map(|a| a.ip())
    }

    /// Returns the default network interface name by reading the kernel routing table.
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    fn get_default_interface_name() -> Option<String> {
        let content = std::fs::read_to_string("/proc/net/route").ok()?;
        for line in content.lines().skip(1) {
            let mut parts = line.split_whitespace();
            let iface = parts.next()?;
            let dest = parts.next()?;
            // "00000000" is the default route (0.0.0.0)
            if dest == "00000000" {
                return Some(iface.to_string());
            }
        }
        None
    }

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

    /// Test that UdpSocketFactory correctly binds to an existing network interface
    /// (via Interface::Address) and that the resulting socket can be used to send
    /// a DNS query to 1.1.1.1:53.
    #[tokio::test]
    async fn test_udp_socket_factory_bind_interface() {
        let local_ip = match get_local_ip() {
            Some(ip) => ip,
            None => return, // skip: cannot determine local interface address
        };

        let factory = UdpSocketFactory {
            addr: "1.1.1.1:53".to_string(),
            interface: Some(Interface::Address(local_ip)),
            fw_mark: None,
            protect_path: None,
            try_dual_stack: false,
        };

        let socket = factory
            .create_socket()
            .await
            .expect("socket creation with interface binding should succeed");

        // Verify the socket is bound to the specified interface IP
        let bound_ip = socket.local_addr().unwrap().as_socket().unwrap().ip();
        assert_eq!(
            bound_ip, local_ip,
            "UDP socket should be bound to the interface IP"
        );

        // Convert to a tokio UdpSocket and issue a query to 1.1.1.1:53 (DNS over UDP)
        let std_socket: std::net::UdpSocket = socket.into();
        let tokio_socket = tokio::net::UdpSocket::from_std(std_socket).unwrap();

        // Minimal DNS query: ID=0x0001, RD=1, QDCOUNT=1, question=one.one.one.one A IN
        let dns_query: &[u8] = &[
            0x00, 0x01, // ID
            0x01, 0x00, // flags: recursion desired
            0x00, 0x01, // QDCOUNT
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // ANCOUNT, NSCOUNT, ARCOUNT
            // QNAME: one.one.one.one
            0x03, b'o', b'n', b'e', 0x03, b'o', b'n', b'e', 0x03, b'o', b'n', b'e', 0x03, b'o',
            b'n', b'e', 0x00, // root label
            0x00, 0x01, // QTYPE A
            0x00, 0x01, // QCLASS IN
        ];

        // The send may fail if the environment blocks outbound UDP; both outcomes are
        // acceptable — what matters is that the socket was successfully bound.
        let _ = tokio_socket.send_to(dns_query, "1.1.1.1:53").await;
    }

    /// Test that TcpSocketFactory correctly binds to an existing network interface
    /// (via Interface::Address) and that the resulting socket can be used to attempt
    /// a TCP connection to 1.1.1.1:80.
    #[tokio::test]
    async fn test_tcp_socket_factory_bind_interface() {
        let local_ip = match get_local_ip() {
            Some(ip) => ip,
            None => return, // skip: cannot determine local interface address
        };

        let factory = TcpSocketFactory {
            addr: "1.1.1.1:80".to_string(),
            interface: Some(Interface::Address(local_ip)),
            fw_mark: None,
            protect_path: None,
        };

        let socket = factory
            .create_socket()
            .await
            .expect("socket creation with interface binding should succeed");

        // Verify the socket is bound to the specified interface IP
        let bound_ip = socket.local_addr().unwrap().as_socket().unwrap().ip();
        assert_eq!(
            bound_ip, local_ip,
            "TCP socket should be bound to the interface IP"
        );

        // Convert to a tokio TcpSocket and attempt a connection to 1.1.1.1:80.
        // A short timeout ensures the test does not hang in restricted environments.
        let std_stream: std::net::TcpStream = socket.into();
        let tokio_socket = tokio::net::TcpSocket::from_std_stream(std_stream);
        let target: std::net::SocketAddr = "1.1.1.1:80".parse().unwrap();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            tokio_socket.connect(target),
        )
        .await;
        // The connection attempt may succeed or fail depending on network availability;
        // what matters is that the socket was successfully bound to the interface.
    }

    /// Test that UdpSocketFactory correctly binds to an existing network device
    /// (via Interface::Device) and that the resulting socket can be used to send
    /// a DNS query to 1.1.1.1:53.  Requires CAP_NET_RAW on Linux; if the capability
    /// is absent the test accepts the OS-level permission error and exits cleanly.
    #[tokio::test]
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    async fn test_udp_socket_factory_bind_device() {
        let iface = match get_default_interface_name() {
            Some(name) => name,
            None => return, // skip: no default interface found
        };

        let factory = UdpSocketFactory {
            addr: "1.1.1.1:53".to_string(),
            interface: Some(Interface::Device(iface.clone())),
            fw_mark: None,
            protect_path: None,
            try_dual_stack: false,
        };

        let socket = match factory.create_socket().await {
            Ok(s) => s,
            Err(e) => {
                // Acceptable when the process lacks the required capability
                assert!(
                    e.raw_os_error().is_some(),
                    "unexpected error binding to device {iface}: {e}"
                );
                return;
            }
        };

        let std_socket: std::net::UdpSocket = socket.into();
        let tokio_socket = tokio::net::UdpSocket::from_std(std_socket).unwrap();

        let dns_query: &[u8] = &[
            0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, b'o',
            b'n', b'e', 0x03, b'o', b'n', b'e', 0x03, b'o', b'n', b'e', 0x03, b'o', b'n', b'e',
            0x00, 0x00, 0x01, 0x00, 0x01,
        ];
        let _ = tokio_socket.send_to(dns_query, "1.1.1.1:53").await;
    }

    /// Test that TcpSocketFactory correctly binds to an existing network device
    /// (via Interface::Device) and that the resulting socket can be used to attempt
    /// a TCP connection to 1.1.1.1:80.  Requires CAP_NET_RAW on Linux; if the
    /// capability is absent the test accepts the OS-level permission error.
    #[tokio::test]
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    async fn test_tcp_socket_factory_bind_device() {
        let iface = match get_default_interface_name() {
            Some(name) => name,
            None => return, // skip: no default interface found
        };

        let factory = TcpSocketFactory {
            addr: "1.1.1.1:80".to_string(),
            interface: Some(Interface::Device(iface.clone())),
            fw_mark: None,
            protect_path: None,
        };

        let socket = match factory.create_socket().await {
            Ok(s) => s,
            Err(e) => {
                assert!(
                    e.raw_os_error().is_some(),
                    "unexpected error binding to device {iface}: {e}"
                );
                return;
            }
        };

        let std_stream: std::net::TcpStream = socket.into();
        let tokio_socket = tokio::net::TcpSocket::from_std_stream(std_stream);
        let target: std::net::SocketAddr = "1.1.1.1:80".parse().unwrap();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            tokio_socket.connect(target),
        )
        .await;
    }
}
