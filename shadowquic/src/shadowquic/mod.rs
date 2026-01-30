use std::{
     collections::{
        HashMap,
        hash_map::{self, Entry},
    }, io::Cursor, mem::replace, ops::Deref, sync::{Arc, atomic::AtomicU16}
};

use bytes::{BufMut, Bytes, BytesMut};
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{
        OnceCell, RwLock, watch::{Receiver, Sender, channel}
    },
};
use tracing::{Instrument, Level, debug, error, event, info, trace};

use crate::{
    AnyUdpRecv, AnyUdpSend,
    error::SError,
    msgs::{
        squic::{SQPacketDatagramHeader, SQUdpControlHeader},
        socks5::{SDecode, SEncode, SocksAddr},
    },
    quic::QuicConnection,
};

pub mod inbound;
pub mod outbound;
mod quinn_wrapper;

pub use quinn_wrapper::{Connection, EndClient, EndServer};


