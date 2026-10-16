use smoltcp::{
    phy::ChecksumCapabilities,
    wire::{
        IpAddress, IpProtocol, IpRepr, Ipv4Packet, Ipv4Repr, Ipv6Packet, Ipv6Repr, UdpPacket,
        UdpRepr,
    },
};
use std::{io, net::SocketAddr};

pub(super) struct Packet<'a> {
    pub src: IpAddress,
    pub dst: IpAddress,
    pub protocol: IpProtocol,
    pub payload: &'a [u8],
}

impl<'a> Packet<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        let packet = match bytes.first()? >> 4 {
            4 => {
                let packet = Ipv4Packet::new_checked(bytes).ok()?;
                if packet.more_frags() || packet.frag_offset() != 0 {
                    return None;
                }
                let repr = Ipv4Repr::parse(&packet, &ChecksumCapabilities::default()).ok()?;
                Self {
                    src: repr.src_addr.into(),
                    dst: repr.dst_addr.into(),
                    protocol: repr.next_header,
                    payload: packet.payload(),
                }
            }
            6 => {
                let packet = Ipv6Packet::new_checked(bytes).ok()?;
                let repr = Ipv6Repr::parse(&packet).ok()?;
                Self {
                    src: repr.src_addr.into(),
                    dst: repr.dst_addr.into(),
                    protocol: repr.next_header,
                    payload: packet.payload(),
                }
            }
            _ => return None,
        };
        let src = std::net::IpAddr::from(packet.src);
        let dst = std::net::IpAddr::from(packet.dst);
        if src.is_unspecified()
            || src.is_multicast()
            || dst.is_unspecified()
            || dst.is_multicast()
            || src == std::net::Ipv4Addr::BROADCAST
            || dst == std::net::Ipv4Addr::BROADCAST
        {
            return None;
        }
        Some(packet)
    }
}

pub(super) fn udp_packet(
    src: SocketAddr,
    dst: SocketAddr,
    payload: &[u8],
    mtu: u16,
) -> io::Result<Vec<u8>> {
    if src.is_ipv4() != dst.is_ipv4() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TUN UDP address families differ",
        ));
    }
    let src_ip = IpAddress::from(src.ip());
    let dst_ip = IpAddress::from(dst.ip());
    let ip = IpRepr::new(src_ip, dst_ip, IpProtocol::Udp, 8 + payload.len(), 64);
    let len = ip.header_len() + 8 + payload.len();
    if len > usize::from(mtu) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TUN UDP datagram exceeds MTU",
        ));
    }
    let mut bytes = vec![0; len];
    ip.emit(
        &mut bytes[..ip.header_len()],
        &ChecksumCapabilities::default(),
    );
    UdpRepr {
        src_port: src.port(),
        dst_port: dst.port(),
    }
    .emit(
        &mut UdpPacket::new_unchecked(&mut bytes[ip.header_len()..]),
        &src_ip,
        &dst_ip,
        payload.len(),
        |buffer| buffer.copy_from_slice(payload),
        &ChecksumCapabilities::default(),
    );
    Ok(bytes)
}
