use std::{
    collections::{HashMap, hash_map},
    io::Cursor,
    mem::replace,
    ops::Deref,
    sync::{Arc, atomic::AtomicU16},
};

use bytes::{BufMut, Bytes, BytesMut};
use quinn::{Connection, RecvStream, SendDatagramError, SendStream};
use tokio::sync::{
    RwLock,
    watch::{Receiver, Sender, channel},
};
use tracing::{Instrument, Level, debug, error, event, info, trace, warn};

use crate::{
    AnyUdpRecv, AnyUdpSend,
    error::SError,
    msgs::{
        shadowquic::{SQPacketDatagramHeader, SQUdpControlHeader},
        socks5::{SDecode, SEncode, SocksAddr},
    },
};

pub mod inbound;
pub mod outbound;

// 4 times larger than quinn default value
// Better decrease the size for portable device
pub const MAX_WINDOW_BASE: u64 = 4 * 12_500_000 * 100 / 1000; // 100ms RTT
pub const MAX_STREAM_WINDOW: u64 = MAX_WINDOW_BASE;
pub const MAX_SEND_WINDOW: u64 = MAX_WINDOW_BASE * 8;
pub const MAX_DATAGRAM_WINDOW: u64 = MAX_WINDOW_BASE * 2;

/// ShadowQuic connection, which is a wrapper around quinn::Connection.
/// It contains a connection object and an ID store for managing UDP sockets.
/// The IDStore stores the mapping between ids and the destionation addresses as well as associated sockets
#[derive(Clone)]
pub struct SQConn {
    conn: Connection,
    send_id_store: IDStore<()>,
    recv_id_store: IDStore<(AnyUdpSend, SocksAddr)>,
}

impl Deref for SQConn {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

// Use watch channel here. Notify is not suitable here
// see https://github.com/tokio-rs/tokio/issues/3757
type IDStoreVal<T> = Result<T, Sender<()>>;
/// IDStore is a thread-safe store for managing UDP sockets and their associated ids.
/// It uses a HashMap to store the mapping between ids and the destination addresses as well as associated sockets.
/// It also uses an atomic counter to generate unique ids for new sockets.
#[derive(Clone, Default)]
struct IDStore<T = (AnyUdpSend, SocksAddr)> {
    id_counter: Arc<AtomicU16>,
    inner: Arc<RwLock<HashMap<u16, IDStoreVal<T>>>>,
}

impl<T> IDStore<T>
where
    T: Clone,
{
    async fn get_socket_or_notify(&self, id: u16) -> Result<T, Receiver<()>> {
        if let Some(r) = self.inner.read().await.get(&id) {
            r.clone().map_err(|x| x.subscribe())
        } else {
            let (s, r) = channel(());
            self.inner.write().await.insert(id, Err(s));
            Err(r)
        }
    }
    async fn try_get_socket(&self, id: u16) -> Option<T> {
        if let Some(r) = self.inner.read().await.get(&id) {
            match r {
                Ok(s) => Some(s.clone()),
                Err(_) => None,
            }
        } else {
            None
        }
    }
    async fn get_socket_or_wait(&self, id: u16) -> Result<T, SError> {
        match self.get_socket_or_notify(id).await {
            Ok(r) => Ok(r),
            Err(mut n) => {
                // This may fail is UDP session is closed right at this moment.
                n.changed()
                    .await
                    .map_err(|_| SError::UDPSessionClosed("notify sender dropped".to_string()))?;
                Ok(self.get_socket_or_notify(id).await.unwrap())
            }
        }
    }
    async fn store_socket(&self, id: u16, val: T) {
        let mut h = self.inner.write().await;
        trace!("receiving side alive socket number: {}", h.len());
        let r = h.get_mut(&id);
        if let Some(s) = r {
            match s {
                Ok(_) => {
                    error!("id:{} already exists", id);
                }
                Err(_) => {
                    let notify = replace(s, Ok(val));
                    //let _ = notify.map_err(|x| x.notify_one());
                    match notify {
                        Ok(_) => {
                            panic!("should be notify"); // should never happen
                        }
                        Err(n) => {
                            n.send(()).unwrap();
                            event!(Level::TRACE, "notify socket id:{}", id);
                        }
                    }
                }
            }
        } else {
            h.insert(id, Ok(val));
        }
    }
    async fn fetch_new_id(&self, val: T) -> u16 {
        let mut inner = self.inner.write().await;
        trace!("sending side socket number: {}", inner.len());
        let mut r;
        loop {
            r = self
                .id_counter
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let hash_map::Entry::Vacant(e) = inner.entry(r) {
                e.insert(Ok(val));
                break;
            }
        }
        r
    }
}

/// AssociateSendSession is a session for sending UDP packets.
/// It is created for each association task
/// The local dst_map works as a inverse map from destination to id
/// When session ended, the ids created by this session will be removed from the IDStore.
struct AssociateSendSession {
    id_store: IDStore<()>,
    dst_map: HashMap<SocksAddr, u16>,
    unistream_map: HashMap<SocksAddr, SendStream>,
}
impl AssociateSendSession {
    pub async fn get_id_or_insert(&mut self, addr: &SocksAddr) -> (u16, bool) {
        if let Some(id) = self.dst_map.get(addr) {
            (*id, false)
        } else {
            let id = self.id_store.fetch_new_id(()).await;
            self.dst_map.insert(addr.clone(), id);
            trace!("send session: insert id:{}, addr:{}", id, addr);
            (id, true)
        }
    }
}

impl Drop for AssociateSendSession {
    fn drop(&mut self) {
        let id_store = self.id_store.inner.clone();
        let id_remove = self.dst_map.clone();
        tokio::spawn(
            async move {
                let mut id_store = id_store.write().await;
                let len = id_store.len();
                id_remove.values().for_each(|k| {
                    id_store.remove(k);
                });
                let decrease = len - id_store.len();
                event!(
                    Level::TRACE,
                    "AssociateSendSession dropped, session id size:{}, {} ids cleaned",
                    id_remove.len(),
                    decrease
                );
            }
            .in_current_span(),
        );
    }
}
/// AssociateRecvSession is a session for receiving UDP ctrl stream.
/// It is created for each association task
/// There are two usages for id_map
/// First, it works as local cache avoiding using global store repeatedly which is more expensive
/// Second. it records ids created by this session and clean those ids when session ended.
struct AssociateRecvSession {
    id_store: IDStore<(AnyUdpSend, SocksAddr)>,
    id_map: HashMap<u16, SocksAddr>,
}
impl AssociateRecvSession {
    pub async fn store_socket(&mut self, id: u16, dst: SocksAddr, socks: AnyUdpSend) {
        if let hash_map::Entry::Vacant(e) = self.id_map.entry(id) {
            self.id_store.store_socket(id, (socks, dst.clone())).await;
            trace!("recv session: insert id:{}, addr:{}", id, dst);
            e.insert(dst);
        }
    }
}

impl Drop for AssociateRecvSession {
    fn drop(&mut self) {
        let id_store = self.id_store.inner.clone();
        let id_remove = self.id_map.clone();
        tokio::spawn(
            async move {
                let mut id_store = id_store.write().await;
                let len = id_store.len();

                id_remove.keys().for_each(|k| {
                    id_store.remove(k);
                });
                let decrease = len - id_store.len();
                event!(
                    Level::TRACE,
                    "AssociateRecvSession dropped, session id size:{}, {} ids cleaned",
                    id_remove.len(),
                    decrease
                );
            }
            .in_current_span(),
        );
    }
}

/// Handle udp packets send
/// It watches the udp socket and sends the packets to the quic connection.
/// This function is symetrical for both clients and servers.
pub async fn handle_udp_send(
    mut send: SendStream,
    udp_recv: AnyUdpRecv,
    conn: SQConn,
    over_stream: bool,
) -> Result<(), SError> {
    let mut down_stream = udp_recv;
    let mut session = AssociateSendSession {
        id_store: conn.send_id_store.clone(),
        dst_map: Default::default(),
        unistream_map: Default::default(),
    };
    let quic_conn = conn.conn.clone();
    loop {
        let (bytes, dst) = down_stream.recv_from().await?;
        let (id, is_new) = session.get_id_or_insert(&dst).await;
        //let span = trace_span!("udp", id = id);
        let ctl_header = SQUdpControlHeader {
            dst: dst.clone(),
            id,
        };
        let dg_header = SQPacketDatagramHeader { id };
        if over_stream && !session.unistream_map.contains_key(&dst) {
            let uni = conn.open_uni().await?;
            session.unistream_map.insert(dst.clone(), uni);
        }

        let fut1 = async {
            if is_new {
                ctl_header.encode(&mut send).await?;
            }
            //trace!("udp control header sent");
            Ok(()) as Result<(), SError>
        };
        let fut2 = async {
            let mut content = BytesMut::with_capacity(2000);
            let mut head = Vec::<u8>::new();
            dg_header.clone().encode(&mut head).await?;

            if over_stream {
                // Must be opened and inserted.
                let conn = session.unistream_map.get_mut(&dst).unwrap();
                let mut head = Vec::<u8>::new();
                if is_new {
                    dg_header.encode(&mut head).await?
                }
                (bytes.len() as u16).encode(&mut head).await?;
                conn.write_all(&head).await?;
                conn.write_all(&bytes).await?;
            } else {
                content.put(Bytes::from(head));
                content.put(bytes);
                let content = content.freeze();
                let len = content.len();
                match quic_conn.send_datagram(content) {
                    Ok(_) => (),
                    Err(SendDatagramError::TooLarge) => warn!(
                        "datagram too large:{}>{}",
                        len,
                        quic_conn.max_datagram_size().unwrap()
                    ),
                    e => e?,
                }
            }
            Ok(())
        };
        tokio::try_join!(fut1, fut2)?;
    }
    #[allow(unreachable_code)]
    Ok(())
}

/// Handle udp ctrl stream receive task
/// it retrieves the dst id pair from the bistream and records related socket and address
/// This function is symetrical for both clients and servers.
pub async fn handle_udp_recv_ctrl(
    mut recv: RecvStream,
    udp_socket: AnyUdpSend,
    conn: SQConn,
) -> Result<(), SError> {
    let mut session = AssociateRecvSession {
        id_store: conn.recv_id_store.clone(),
        id_map: Default::default(),
    };
    loop {
        let SQUdpControlHeader { id, dst } = SQUdpControlHeader::decode(&mut recv).await?;
        trace!("udp control header received: id:{},dst:{}", id, dst);
        session.store_socket(id, dst, udp_socket.clone()).await;
    }
    #[allow(unreachable_code)]
    Ok(())
}

/// Handle udp packet receive task
/// It watches udp packets from quic connection and sends them to the udp socket.
/// The udp socket could be downstream(inbound) or upstream(outbound)
/// This function is symetrical for both clients and servers.
pub async fn handle_udp_packet_recv(conn: SQConn) -> Result<(), SError> {
    let id_store = conn.recv_id_store.clone();

    loop {
        tokio::select! {
            b = conn.read_datagram() => {
                let b = b?;
                let b = BytesMut::from(b);
                let mut cur = Cursor::new(b);
                let SQPacketDatagramHeader{id} = SQPacketDatagramHeader::decode(&mut cur).await?;

                match id_store.get_socket_or_notify(id).await {
                 Ok((udp,addr)) =>  {
                    let pos = cur.position() as usize;
                    let b = cur.into_inner().freeze();
                    udp.send_to(b.slice(pos..b.len()), addr.clone()).await?;
                }
                Err(mut notify) =>  {
                    let id_store = id_store.clone();
                    event!(Level::TRACE, "resolving datagram id:{}",id);
                    // Might spawn too many tasks
                    tokio::spawn(async move {
                        // It's safe to sender to be dropped
                        let _ = notify.changed().await.map_err(|_|debug!("id:{} notifier dropped",id));
                        // session may be closed
                        let (udp,addr) = id_store.try_get_socket(id).await.ok_or(SError::UDPSessionClosed("UDP session closed".to_string()))?;
                        debug!("datagram id resolve: id:{}:,dst:{}",id, addr);
                        let pos = cur.position() as usize;
                        let b = cur.into_inner().freeze();
                        let _ = udp.clone().send_to(b.slice(pos..b.len()), addr.clone()).await
                        .map_err(|x|error!("{}",x));
                        Ok(()) as Result<(), SError>
                     }.in_current_span());
                }
            }
            }

            r = async {
                let mut uni_stream = conn.accept_uni().await?;
                trace!("unistream accepted");
                let SQPacketDatagramHeader{id} = SQPacketDatagramHeader::decode(&mut uni_stream).await?;

                let (udp,addr) = id_store.get_socket_or_wait(id).await?;

                info!("unistream datagram accepted: id:{},dst:{}",id, addr);
                Ok((uni_stream,udp.clone(),addr.clone())) as Result<(RecvStream,AnyUdpSend,SocksAddr),SError>
            } => {
                let  (mut uni_stream,udp,addr) = r?;

                tokio::spawn(async move {
                    loop {
                        let l: usize = u16::decode(&mut uni_stream).await? as usize;
                        let mut b = BytesMut::with_capacity(l);
                        b.resize(l,0);
                        uni_stream.read_exact(&mut b).await?;
                        udp.send_to(b.freeze(), addr.clone()).await?;
                    }
                    #[allow(unreachable_code)]
                    (Ok(()) as Result<(), SError>)
                }.in_current_span());
            }
        }
    }
    #[allow(unreachable_code)]
    Ok(())
}
