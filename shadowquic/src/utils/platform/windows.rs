use socket2::{Socket, Type};
use std::{ffi::CString, io, os::windows::io::AsRawSocket};
use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::{
    IpHelper::{ConvertInterfaceAliasToLuid, ConvertInterfaceLuidToIndex, if_nametoindex},
    Ndis::NET_LUID_LH,
};
use windows::Win32::Networking::WinSock::{
    IP_MULTICAST_IF, IP_UNICAST_IF, IPPROTO_IP, IPPROTO_IPV6, IPV6_MULTICAST_IF, IPV6_UNICAST_IF,
    SOCKET, WSAGetLastError, setsockopt,
};
use windows::core::{PCSTR, PCWSTR};

fn interface_index(device_name: &str) -> io::Result<u32> {
    let name = CString::new(device_name)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let index = unsafe { if_nametoindex(PCSTR::from_raw(name.as_ptr().cast())) };
    if index != 0 {
        return Ok(index);
    }

    let alias: Vec<u16> = device_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut luid = NET_LUID_LH::default();
    let status =
        unsafe { ConvertInterfaceAliasToLuid(PCWSTR::from_raw(alias.as_ptr()), &mut luid) };
    if status == ERROR_INVALID_PARAMETER {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Windows interface {device_name:?} was not found"),
        ));
    }
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.0 as i32));
    }

    let mut index = 0;
    let status = unsafe { ConvertInterfaceLuidToIndex(&luid, &mut index) };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.0 as i32));
    }
    Ok(index)
}

fn set_interface_option(
    socket: SOCKET,
    level: i32,
    option: i32,
    index: &[u8; 4],
) -> io::Result<()> {
    if unsafe { setsockopt(socket, level, option, Some(index)) } == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }.0))
    }
}

pub fn bind_device(socket: &Socket, device_name: &str) -> io::Result<()> {
    let index = interface_index(device_name)?;
    let handle = SOCKET(socket.as_raw_socket() as usize);
    let is_udp = socket.r#type()? == Type::DGRAM;
    let bind_family = |level, unicast, multicast, index| -> io::Result<()> {
        set_interface_option(handle, level, unicast, &index)?;
        if is_udp {
            set_interface_option(handle, level, multicast, &index)?;
        }
        Ok(())
    };

    // An IPv6 socket can carry IPv4 traffic even if the adapter has IPv6 disabled.
    let ipv4 = bind_family(
        IPPROTO_IP.0,
        IP_UNICAST_IF,
        IP_MULTICAST_IF,
        index.to_be_bytes(),
    );
    let ipv6 = bind_family(
        IPPROTO_IPV6.0,
        IPV6_UNICAST_IF,
        IPV6_MULTICAST_IF,
        index.to_ne_bytes(),
    );
    if let (Err(ipv4_error), Err(ipv6_error)) = (ipv4, ipv6) {
        tracing::error!(
            event = "connectivity:interface_binding_failed",
            interface = device_name,
            %ipv4_error,
            %ipv6_error,
            "failed to bind socket to interface"
        );
        return Err(if socket.local_addr().is_ok_and(|addr| addr.is_ipv6()) {
            ipv6_error
        } else {
            ipv4_error
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use socket2::{Domain, Protocol};
    use std::{ffi::CStr, net::SocketAddr};
    use windows::Win32::NetworkManagement::IpHelper::{GetBestInterface, if_indextoname};

    #[test]
    fn binds_ipv4_ipv6_and_dual_stack_sockets_by_interface_name() {
        let mut index = 0;
        let loopback = u32::from_ne_bytes([127, 0, 0, 1]);
        assert_eq!(unsafe { GetBestInterface(loopback, &mut index) }, 0);
        let mut name = [0; 256];
        assert!(!unsafe { if_indextoname(index, &mut name) }.is_null());
        let name = CStr::from_bytes_until_nul(&name).unwrap().to_str().unwrap();

        for (bind_addr, only_v6) in [
            ("0.0.0.0:0", None),
            ("[::]:0", Some(true)),
            ("[::]:0", Some(false)),
        ] {
            let addr: SocketAddr = bind_addr.parse().unwrap();
            let socket =
                Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP)).unwrap();
            if let Some(only_v6) = only_v6 {
                socket.set_only_v6(only_v6).unwrap();
            }
            socket.bind(&addr.into()).unwrap();
            bind_device(&socket, name).unwrap();

            if only_v6 != Some(true) {
                assert_eq!(
                    socket.multicast_if_v4().unwrap().octets(),
                    index.to_be_bytes()
                );
            }
            if addr.is_ipv6() {
                assert_eq!(socket.multicast_if_v6().unwrap(), index);
            }
        }
    }
}
