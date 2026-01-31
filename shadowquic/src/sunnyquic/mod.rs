use std::{
    collections::{
        HashMap,
        hash_map::{self, Entry},
    },
    io::Cursor,
    mem::replace,
    ops::Deref,
    sync::{Arc, atomic::AtomicU16},
};

use bytes::{BufMut, Bytes, BytesMut};
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{
        OnceCell, RwLock,
        watch::{Receiver, Sender, channel},
    },
};
use tracing::{Instrument, Level, debug, error, event, info, trace};

use crate::{
    AnyUdpRecv, AnyUdpSend,
    config::AuthUser,
    error::SError,
    msgs::{
        socks5::{SDecode, SEncode, SocksAddr},
        squic::{SQPacketDatagramHeader, SQUdpControlHeader, SUNNY_QUIC_AUTH_LEN},
    },
    quic::QuicConnection,
};

#[cfg(feature = "sunnyquic-gm-quic")]
mod gm_quic_wrapper;
pub mod inbound;
pub mod outbound;

#[cfg(feature = "sunnyquic-gm-quic")]
pub use gm_quic_wrapper::{Connection, EndClient, EndServer};

#[cfg(feature = "sunnyquic-iroh-quinn")]
mod iroh_wrapper;
#[cfg(feature = "sunnyquic-iroh-quinn")]
pub use iroh_wrapper::{Connection, EndClient, EndServer};

pub(crate) fn gen_sunny_user_hash(username: &str, password: &str) -> [u8; SUNNY_QUIC_AUTH_LEN] {
    let hash_in = username.to_string() + ":" + password;
    let hash_out = ring::digest::digest(&ring::digest::SHA256, hash_in.into_bytes().as_ref());
    let mut arr = [0u8; SUNNY_QUIC_AUTH_LEN];
    let hash_bytes = hash_out.as_ref();
    let len = arr.len().min(hash_bytes.len());
    arr[..len].copy_from_slice(&hash_bytes[..len]);
    arr
}
