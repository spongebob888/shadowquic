use crate::msgs::socks5::SOCKS5_ADDR_TYPE_DOMAIN_NAME;

use super::socks5::{AddrOrDomain, SOCKS5_ADDR_TYPE_IPV4, SOCKS5_ADDR_TYPE_IPV6, SocksAddr};
use std::io::Cursor;
use tokio::io::{AsyncRead, AsyncWrite};

#[tokio::test]
async fn test_socksaddr_ipv4_encode_decode() {
    let addr = SocksAddr {
        addr: AddrOrDomain::V4([127, 0, 0, 1]),
        port: 8080,
    };
    let mut buf = Vec::new();
    <SocksAddr as super::SEncode>::encode(&addr, &mut buf)
        .await
        .expect("encode failed");
    // Intermediate check: buffer should be 1 (atype) + 4 (IPv4) + 2 (port) = 7 bytes
    assert_eq!(buf.len(), 7);
    assert_eq!(buf[0], SOCKS5_ADDR_TYPE_IPV4);
    assert_eq!(&buf[1..5], &[127, 0, 0, 1]);
    assert_eq!(u16::from_be_bytes([buf[5], buf[6]]), 8080);

    let mut reader = Cursor::new(&buf);
    let decoded = <SocksAddr as super::SDecode>::decode(&mut reader)
        .await
        .expect("decode failed");
    // Intermediate check: decoded fields
    assert_eq!(decoded.port, 8080);
    match decoded.addr {
        AddrOrDomain::V4(ip) => assert_eq!(ip, [127, 0, 0, 1]),
        _ => panic!("Decoded addr is not V4"),
    }
    assert_eq!(addr, decoded);
}

#[tokio::test]
async fn test_socksaddr_ipv6_encode_decode() {
    let addr = SocksAddr {
        addr: AddrOrDomain::V6([0u8; 16]),
        port: 443,
    };
    let mut buf = Vec::new();
    <SocksAddr as super::SEncode>::encode(&addr, &mut buf)
        .await
        .expect("encode failed");
    // Intermediate check: buffer should be 1 (atype) + 16 (IPv6) + 2 (port) = 19 bytes
    assert_eq!(buf.len(), 19);
    assert_eq!(buf[0], SOCKS5_ADDR_TYPE_IPV6);
    assert_eq!(&buf[1..17], &[0u8; 16]);
    assert_eq!(u16::from_be_bytes([buf[17], buf[18]]), 443);

    let mut reader = Cursor::new(&buf);
    let decoded = <SocksAddr as super::SDecode>::decode(&mut reader)
        .await
        .expect("decode failed");
    // Intermediate check: decoded fields
    assert_eq!(decoded.port, 443);
    match decoded.addr {
        AddrOrDomain::V6(ip) => assert_eq!(ip, [0u8; 16]),
        _ => panic!("Decoded addr is not V6"),
    }
    assert_eq!(addr, decoded);
}

#[tokio::test]
async fn test_socksaddr_domain_encode_decode() {
    let domain = "example.com".to_string();
    let addr = SocksAddr::from_domain(domain.clone(), 1234);
    let mut buf = Vec::new();
    <SocksAddr as super::SEncode>::encode(&addr, &mut buf)
        .await
        .expect("encode failed");
    // Intermediate check: buffer should be 1 (atype) + 1 (len) + N (domain) + 2 (port)
    assert_eq!(buf[0], SOCKS5_ADDR_TYPE_DOMAIN_NAME);
    let domain_len = domain.len();
    assert_eq!(buf[1] as usize, domain_len);
    assert_eq!(&buf[2..2 + domain_len], domain.as_bytes());
    assert_eq!(
        u16::from_be_bytes([buf[2 + domain_len], buf[3 + domain_len]]),
        1234
    );

    let mut reader = Cursor::new(&buf);
    let decoded = <SocksAddr as super::SDecode>::decode(&mut reader)
        .await
        .expect("decode failed");
    // Intermediate check: decoded fields
    assert_eq!(decoded.port, 1234);
    if let AddrOrDomain::Domain(varvec) = &decoded.addr {
        assert_eq!(varvec.len as usize, domain_len);
        assert_eq!(String::from_utf8_lossy(&varvec.contents), domain);
    } else {
        panic!("Decoded address is not a domain");
    }
    assert_eq!(addr, decoded);
}
