//! Lengths on the wire are chosen by the peer: decoding must not trust them
//! with memory before the bytes have arrived.
use super::{
    SDecode, SEncode,
    squic::{ConnStats, UserStats},
};

#[tokio::test]
async fn vec_length_prefix_does_not_reserve_memory_up_front() {
    // u32::MAX elements of 64 KiB each (256 TiB) exceed any address space.
    let mut wire: &[u8] = &[0xff, 0xff, 0xff, 0xff];
    assert!(Vec::<[u8; 1 << 16]>::decode(&mut wire).await.is_err());
}

#[tokio::test]
async fn size_tagged_struct_does_not_reserve_its_size_up_front() {
    // 4 GiB announced, two bytes sent.
    let mut wire: &[u8] = &[0xff, 0xff, 0xff, 0xff, 0, 0];
    assert!(ConnStats::decode(&mut wire).await.is_err());
}

#[tokio::test]
async fn size_tagged_struct_cut_short_is_an_error() {
    let mut wire = Vec::new();
    UserStats::default().encode(&mut wire).await.unwrap();
    wire.pop();
    assert!(UserStats::decode(&mut &wire[..]).await.is_err());
}

#[tokio::test]
async fn size_tagged_struct_of_any_size_decodes_what_it_encodes() {
    let stats = UserStats {
        username: "u".repeat(1 << 20),
        ..Default::default()
    };
    let mut wire = Vec::new();
    stats.encode(&mut wire).await.unwrap();
    // A stream positioned right after the struct, as when it is followed by more.
    wire.push(0xAB);
    let mut reader = &wire[..];
    let decoded = UserStats::decode(&mut reader).await.unwrap();
    assert!(decoded == stats);
    assert_eq!(reader, [0xAB]);
}
