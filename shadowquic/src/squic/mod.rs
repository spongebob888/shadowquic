//! This module is shared by sunnyquic and shadowquic
//! It handles the general tcp/udp proxying logic over quic connection
//! It contains an optional authentication feature for sunnyquic only

use std::{
    collections::{
        HashMap,
        hash_map::{self, Entry},
    },
    io::Cursor,
    mem::replace,
    ops::Deref,
    sync::{Arc, atomic::AtomicU16},
    time::{Duration, Instant},
};

use bytes::{BufMut, Bytes, BytesMut};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{
        Mutex, RwLock, SetOnce,
        watch::{Receiver, Sender, channel},
    },
    task::JoinSet,
};
use tracing::{Instrument, Level, debug, error, event, info, trace};

use crate::{
    AnyUdpRecv, AnyUdpSend, UdpSend,
    error::{SError, SResult},
    msgs::{
        SDecode, SEncode,
        socks5::SocksAddr,
        squic::{ConnStats, SQPacketDatagramHeader, SQReq, SQUdpControlHeader, SunnyCredential},
    },
    quic::QuicConnection,
};

pub mod inbound;
pub mod outbound;

/// SQuic connection, it is shared by shadowquic and sunnyquic and is a wrapper of quic connection.
/// It contains a connection object and two ID store for managing UDP sockets.
/// The IDStore stores the mapping between ids and the destionation addresses as well as associated sockets
#[derive(Clone)]
pub struct SQConn<T: QuicConnection> {
    pub(crate) conn: T,
    pub authed: Arc<SetOnce<SResult<String>>>,
    pub(crate) send_id_store: IDStore<()>,
    pub(crate) recv_id_store: IDStore<(AnyUdpSend, SocksAddr)>,
    /// Latest successful peer stats response and the time it was received.
    pub stats: Arc<Mutex<Option<SQConnStats>>>,
}

pub struct SQConnStats {
    pub uplink: ConnStats,
    pub downlink: ConnStats,
    pub time: Instant,
}

async fn wait_sunny_auth<T: QuicConnection>(conn: &SQConn<T>) -> SResult<String> {
    match tokio::time::timeout(Duration::from_millis(3200), conn.authed.wait()).await {
        Ok(Ok(name)) => Ok(name.clone()),
        Ok(Err(SError::SunnyAuthError(_))) => {
            Err(SError::SunnyAuthError("Wrong password/username".into()))
        }
        Err(_) => Err(SError::SunnyAuthError("timeout".into())),
        _ => unreachable!(),
    }
}

pub(crate) async fn auth_sunny<T: QuicConnection>(
    conn: &SQConn<T>,
    username: &str,
    user_hash: SunnyCredential,
) -> SResult<()> {
    if conn.authed.get().is_none() {
        let (mut send, _recv, _id) = conn.open_bi().await?;
        SQReq::SQAuthenticate(user_hash).encode(&mut send).await?;
        debug!("authentication request sent");
        conn.authed
            .set(Ok(username.to_string()))
            .expect("repeated authentication");
    }
    Ok(())
}

impl<T: QuicConnection> Deref for SQConn<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

pub(crate) struct NotifyBuffer {
    pub(crate) notify: Sender<()>,
    pub(crate) buffer: Vec<Bytes>,
}

// Use watch channel here. Notify is not suitable here
// see https://github.com/tokio-rs/tokio/issues/3757
type IDStoreVal<T> = Result<T, NotifyBuffer>;
/// IDStore is a thread-safe store for managing UDP sockets and their associated ids.
/// It uses a HashMap to store the mapping between ids and the destination addresses as well as associated sockets.
/// It also uses an atomic counter to generate unique ids for new sockets.
#[derive(Clone, Default)]
pub(crate) struct IDStore<T = (AnyUdpSend, SocksAddr)> {
    pub(crate) id_counter: Arc<AtomicU16>,
    pub(crate) inner: Arc<RwLock<HashMap<u16, IDStoreVal<T>>>>,
}

impl<T> IDStore<T>
where
    T: Clone,
{
    async fn get_socket_or_notify(&self, id: u16) -> Result<T, Receiver<()>> {
        if let Some(r) = self.inner.read().await.get(&id) {
            r.as_ref().map_err(|x| x.notify.subscribe()).cloned()
        } else {
            // Need to recheck
            // During change from read lock to write lock, hashmap may be modified
            match self.inner.write().await.entry(id) {
                Entry::Occupied(occupied_entry) => occupied_entry
                    .get()
                    .as_ref()
                    .map_err(|x| x.notify.subscribe())
                    .cloned(),
                Entry::Vacant(vacant_entry) => {
                    let (s, r) = channel(());
                    vacant_entry.insert(Err(NotifyBuffer {
                        notify: s,
                        buffer: Vec::new(),
                    }));
                    Err(r)
                }
            }
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
                //
                let ret = self
                    .try_get_socket(id)
                    .await
                    .ok_or(SError::UDPSessionClosed("UDP session closed".to_string()))?;
                Ok(ret)
            }
        }
    }
    /// Drop a pending id that nobody waits for and whose datagrams are already
    /// buffered, without waiting for the store lock. Returns false when the
    /// lock was busy, so the caller can arrange the cleanup elsewhere.
    fn try_remove_unwatched_pending(&self, id: u16) -> bool {
        let Ok(mut inner) = self.inner.try_write() else {
            return false;
        };
        remove_unwatched_pending_locked(&mut inner, id);
        true
    }

    /// The same cleanup, waiting for the store lock if it has to. Only used as
    /// the fallback of [`Self::try_remove_unwatched_pending`], so a busy store
    /// cannot leave an orphan behind for the life of the connection.
    async fn remove_unwatched_pending(&self, id: u16) {
        let mut inner = self.inner.write().await;
        remove_unwatched_pending_locked(&mut inner, id);
    }
    #[allow(dead_code)]
    async fn store_socket(&self, id: u16, val: T) -> Option<Vec<Bytes>> {
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
                            n.notify.send(()).unwrap_or_else(|_| {
                                debug!("id:{} notifier without subscriber", id)
                            });
                            event!(Level::TRACE, "notify socket id:{}", id);
                            return Some(n.buffer);
                        }
                    }
                }
            }
        } else {
            h.insert(id, Ok(val));
        }
        None
    }
    async fn fetch_new_id(&self, val: T) -> u16 {
        let mut inner = self.inner.write().await;
        trace!("sending side socket number: {}", inner.len());
        let mut r;
        loop {
            r = self
                .id_counter
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst); // Wrapping occured if overflow
            if let Entry::Vacant(e) = inner.entry(r) {
                e.insert(Ok(val));
                break;
            }
        }
        r
    }
}

/// Hand one datagram to the association's local socket without waiting for the
/// consumer to make room.
///
/// The consumer is another task and can stop draining, and waiting for it —
/// however briefly — parks the connection's receive loop on this one
/// association. A datagram that cannot be taken right now is dropped instead,
/// which is what UDP does under pressure anyway. The send is polled once: an
/// mpsc sender completes immediately when it has room.
async fn deliver(socket: AnyUdpSend, packet: Bytes, addr: SocksAddr) -> SResult<()> {
    match tokio::time::timeout(Duration::ZERO, socket.send_to(packet, addr)).await {
        Ok(result) => {
            result?;
            Ok(())
        }
        Err(_) => Err(SError::ChannelError(
            "local udp consumer is not draining".into(),
        )),
    }
}

/// Drop a pending entry for `id` when nothing waits for it. A registered socket
/// and an entry that still has a subscriber are both left alone.
fn remove_unwatched_pending_locked<T>(inner: &mut HashMap<u16, IDStoreVal<T>>, id: u16) {
    if let Entry::Occupied(entry) = inner.entry(id)
        && let Err(pending) = entry.get()
        && pending.notify.receiver_count() == 0
    {
        let _ = entry.remove();
    }
}

impl IDStore {
    async fn feed_datagram(&self, id: u16, packet: Bytes) -> SResult<()> {
        // Resolve the target under the lock, then release it before sending:
        // the send talks to the local consumer, another task that can be slow,
        // so holding `inner` across it would stall every other id here.
        let target = {
            let mut inner = self.inner.write().await;
            match inner.entry(id) {
                Entry::Occupied(mut entry) => match entry.get_mut() {
                    Ok((socket, addr)) => Some((socket.clone(), addr.clone())),
                    Err(notify) => {
                        notify.buffer.push(packet.clone());
                        None
                    }
                },
                Entry::Vacant(vacant_entry) => {
                    let (s, _r) = channel(());
                    vacant_entry.insert(Err(NotifyBuffer {
                        notify: s,
                        buffer: vec![packet.clone()],
                    }));
                    None
                }
            }
        };
        match target {
            Some((socket, addr)) => deliver(socket, packet, addr).await,
            None => Ok(()),
        }
    }
    async fn store_socket_with_prelude(
        &self,
        id: u16,
        val: (Arc<dyn UdpSend>, SocksAddr),
    ) -> SResult<()> {
        let (socket, addr) = (val.0.clone(), val.1.clone());
        // The datagrams that arrived before this header are flushed after the
        // lock is released: the flush talks to the local consumer, another task
        // that can be slow, so holding `inner` across it would stall every
        // other id here.
        let prelude = {
            let mut h = self.inner.write().await;
            trace!("receiving side alive socket number: {}", h.len());
            match h.get_mut(&id) {
                Some(s) => match s {
                    Ok(_) => {
                        error!("id:{} already exists", id);
                        Vec::new()
                    }
                    Err(_) => {
                        let notify = replace(s, Ok(val));
                        //let _ = notify.map_err(|x| x.notify_one());
                        match notify {
                            Ok(_) => {
                                panic!("should be notify"); // should never happen
                            }
                            Err(n) => {
                                n.notify.send(()).unwrap_or_else(|_| {
                                    debug!("id:{} notifier without subscriber", id)
                                });
                                event!(Level::TRACE, "notify socket id:{}", id);
                                n.buffer
                            }
                        }
                    }
                },
                None => {
                    h.insert(id, Ok(val));
                    Vec::new()
                }
            }
        };
        for bytes in prelude {
            // A datagram that cannot be handed over right now is lost, which is
            // what UDP does under pressure. Failing here instead would leave
            // the id registered in the store but unknown to the association, so
            // its teardown would never remove it.
            if let Err(e) = deliver(socket.clone(), bytes, addr.clone()).await {
                debug!("dropping buffered udp datagram for id {}: {}", id, e);
            }
        }
        Ok(())
    }
}

/// AssociateSendSession is a session for sending UDP packets.
/// It is created for each association task
/// The local dst_map works as a inverse map from destination to id
/// When session ended, the ids created by this session will be removed from the IDStore.
struct AssociateSendSession<W: AsyncWrite> {
    id_store: IDStore<()>,
    dst_map: HashMap<SocksAddr, u16>,
    unistream_map: HashMap<SocksAddr, W>,
}
impl<W: AsyncWrite> AssociateSendSession<W> {
    pub async fn get_id_or_insert(&mut self, addr: &SocksAddr) -> (u16, bool) {
        if let Some(id) = self.dst_map.get(addr) {
            (*id, false)
        } else {
            let id = self.id_store.fetch_new_id(()).await;
            self.dst_map.insert(addr.clone(), id);
            debug!(context_id = id, dst = %addr, "send session insert");
            (id, true)
        }
    }
}

impl<W: AsyncWrite> Drop for AssociateSendSession<W> {
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
    pub async fn store_socket(
        &mut self,
        id: u16,
        dst: SocksAddr,
        socks: AnyUdpSend,
    ) -> SResult<()> {
        if let hash_map::Entry::Vacant(e) = self.id_map.entry(id) {
            self.id_store
                .store_socket_with_prelude(id, (socks, dst.clone()))
                .await?;
            debug!(context_id = id, dst = %dst, "recv session insert");
            e.insert(dst);
        }
        Ok(())
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
pub async fn handle_udp_send<C: QuicConnection>(
    mut send: C::SendStream,
    udp_recv: AnyUdpRecv,
    conn: SQConn<C>,
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
            let (uni, _id) = conn.open_uni().await?;
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
                quic_conn.send_datagram(content).await?;
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
pub async fn handle_udp_recv_ctrl<C: QuicConnection>(
    mut recv: C::RecvStream,
    udp_socket: AnyUdpSend,
    conn: SQConn<C>,
) -> Result<(), SError> {
    let mut session = AssociateRecvSession {
        id_store: conn.recv_id_store.clone(),
        id_map: Default::default(),
    };
    loop {
        let SQUdpControlHeader { id, dst } = SQUdpControlHeader::decode(&mut recv).await?;
        info!(context_id = id, dst = %dst, "udp control header received");
        let _ = session
            .store_socket(id, dst, udp_socket.clone())
            .await
            .map_err(|e| error!("failed to writing data to udp socket:{e}"));
    }
    #[allow(unreachable_code)]
    Ok(())
}

/// Handle udp packet receive task
/// It watches udp packets from quic connection and sends them to the udp socket.
/// The udp socket could be downstream(inbound) or upstream(outbound)
/// This function is symetrical for both clients and servers.
pub async fn handle_udp_packet_recv<C: QuicConnection>(conn: SQConn<C>) -> Result<(), SError> {
    let id_store = conn.recv_id_store.clone();
    wait_sunny_auth(&conn).await?;
    let mut datagram_retry_after = tokio::time::Instant::now();
    // Every accepted unistream is served by its own task and the set owns
    // them: leaving this loop drops the set, so tokio aborts whatever is still
    // parked instead of letting those tasks outlive their connection.
    let mut streams: JoinSet<()> = JoinSet::new();
    loop {
        // Reap finished streams so the set tracks live ones only.
        while streams.try_join_next().is_some() {}
        tokio::select! {
            b = async {
                tokio::time::sleep_until(datagram_retry_after).await;
                conn.read_datagram().await
            } => {
                let b = match b {
                    Ok(b) => b,
                    Err(e) if conn.close_reason().is_some() => return Err(e.into()),
                    Err(e) => {
                        // No backend exposes a typed unsupported-datagrams
                        // read error. Retry a nonterminal error with a delay
                        // rather than permanently disabling the branch.
                        error!("udp datagram receive failed: {}", e);
                        datagram_retry_after = tokio::time::Instant::now() + Duration::from_millis(100);
                        continue;
                    }
                };
                let b = BytesMut::from(b);
                let mut cur = Cursor::new(b);
                let SQPacketDatagramHeader{id} = match SQPacketDatagramHeader::decode(&mut cur).await {
                    Ok(header) => header,
                    Err(e) => {
                        error!("dropping malformed udp datagram: {}", e);
                        continue;
                    }
                };
                let pos = cur.position() as usize;
                if let Err(e) = id_store.feed_datagram(id, cur.into_inner().split_off(pos).freeze()).await {
                    // One lost datagram is not a reason to stop serving the
                    // whole connection. A consumer that could not take it in
                    // time is ordinary loss under pressure; anything else (no
                    // live socket for the id, or a failing local socket) is
                    // worth an error.
                    if matches!(e, SError::ChannelError(_)) {
                        debug!("dropping udp datagram for id {}: {}", id, e);
                    } else {
                        error!("dropping udp datagram for id {}: {}", id, e);
                    }
                }
            }

            // Only the accept is polled here: once the stream is taken from
            // the queue the rest runs in its own task, so a cancellation — or
            // a failure while resolving its id — can neither lose the stream
            // nor stop the loop.
            uni = conn.accept_uni() => {
                let (uni_stream, _id) = uni?;
                trace!("unistream accepted");
                let id_store = id_store.clone();
                let conn = conn.clone();
                streams.spawn(
                    async move {
                        match serve_uni_stream(uni_stream, id_store, conn).await {
                            // A parked stream losing its association is
                            // ordinary teardown, not a failure.
                            Err(SError::UDPSessionClosed(msg)) => {
                                debug!("udp over stream ended: {}", msg)
                            }
                            Err(e) => error!("udp over stream ended: {}", e),
                            Ok(()) => {}
                        }
                    }
                    .in_current_span(),
                );
            }
        }
    }
    #[allow(unreachable_code)]
    Ok(())
}

/// Bound how long an unmatched stream can hold resources. This fixed budget
/// covers both the stream header and its control header; it is not the QUIC
/// connection idle timeout. Remove the orphaned pending id when it expires.
const UDP_SESSION_RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);

/// Serve one udp-over-stream unistream: resolve the session id it carries to
/// the socket that receives the payload, then relay the length-prefixed
/// packets until the stream ends.
async fn serve_uni_stream<C: QuicConnection>(
    mut uni_stream: C::RecvStream,
    id_store: IDStore,
    conn: SQConn<C>,
) -> Result<(), SError> {
    let deadline = tokio::time::Instant::now() + UDP_SESSION_RESOLVE_TIMEOUT;
    let SQPacketDatagramHeader { id } =
        tokio::time::timeout_at(deadline, SQPacketDatagramHeader::decode(&mut uni_stream))
            .await
            .map_err(|_| SError::UDPSessionClosed("udp stream header timed out".into()))??;
    trace!(context_id = id, "resolving datagram id");

    let (udp, addr) = resolve_udp_session(&id_store, id, deadline).await?;

    info!(context_id = id, peer_addr = %conn.remote_address(), dst = %addr, "udp over stream");

    relay_uni_stream(&mut uni_stream, udp.as_ref(), &addr).await
}

/// Relay the length-prefixed packets of one udp-over-stream unistream to the
/// socket its association registered.
///
/// The peer decides whether a payload is ever completed, and a QUIC connection
/// can outlive its associations by hours, so the relay also ends as soon as the
/// association's receiver is gone.
async fn relay_uni_stream<R: AsyncRead + Unpin + Send>(
    uni_stream: &mut R,
    udp: &dyn UdpSend,
    addr: &SocksAddr,
) -> Result<(), SError> {
    loop {
        let packet = tokio::select! {
            packet = read_packet(uni_stream) => packet?,
            _ = udp.closed() => {
                return Err(SError::UDPSessionClosed("udp association ended".into()));
            }
        };
        udp.send_to(packet, addr.clone()).await?;
    }
}

async fn read_packet<R: AsyncRead + Unpin + Send>(uni_stream: &mut R) -> Result<Bytes, SError> {
    let l: usize = u16::decode(uni_stream).await? as usize;
    let mut b = BytesMut::with_capacity(l);
    b.resize(l, 0);
    uni_stream.read_exact(&mut b).await?;
    Ok(b.freeze())
}

async fn resolve_udp_session(
    id_store: &IDStore,
    id: u16,
    deadline: tokio::time::Instant,
) -> Result<(AnyUdpSend, SocksAddr), SError> {
    match tokio::time::timeout_at(deadline, id_store.get_socket_or_wait(id)).await {
        Ok(result) => result,
        Err(_) => {
            // The deadline is what bounds this task, so the cleanup does not
            // wait for the store lock here. Every critical section in the store
            // is synchronous, so this helper finishes promptly and cannot be
            // held up by a peer.
            if !id_store.try_remove_unwatched_pending(id) {
                let id_store = id_store.clone();
                tokio::spawn(async move { id_store.remove_unwatched_pending(id).await });
            }
            Err(SError::UDPSessionClosed(format!(
                "udp session id {id} not resolved in time"
            )))
        }
    }
}

#[cfg(test)]
mod udp_receive_tests {
    use super::*;
    use crate::quic::QuicErrorRepr;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::AsyncRead;
    use tokio::sync::{Mutex, mpsc};

    fn store() -> IDStore {
        IDStore {
            id_counter: Default::default(),
            inner: Default::default(),
        }
    }

    /// A sender whose receiver is alive, so deliveries are recorded.
    struct LiveSend(mpsc::UnboundedSender<Bytes>);

    #[async_trait::async_trait]
    impl UdpSend for LiveSend {
        async fn send_to(&self, buf: Bytes, _addr: SocksAddr) -> Result<usize, SError> {
            let len = buf.len();
            let _ = self.0.send(buf);
            Ok(len)
        }
    }

    fn test_addr() -> SocksAddr {
        SocksAddr::from("127.0.0.1:9000".parse::<std::net::SocketAddr>().unwrap())
    }

    /// A stream that never yields a byte, and reports when it is dropped.
    struct ParkedStream {
        dropped: Arc<AtomicBool>,
    }

    impl Drop for ParkedStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl AsyncRead for ParkedStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    /// A connection that hands out one parked unistream and then reports
    /// itself gone, so the receive loop leaves while that stream is parked.
    #[derive(Clone)]
    struct OneStreamConn {
        stream: Arc<Mutex<Option<ParkedStream>>>,
        accepted: Arc<AtomicBool>,
        datagrams: Arc<Mutex<VecDeque<Bytes>>>,
        /// Report the connection as closed once the stream is taken, or just
        /// have nothing more to offer.
        stay_open: bool,
    }

    #[async_trait::async_trait]
    impl QuicConnection for OneStreamConn {
        type SendStream = tokio::io::Sink;
        type RecvStream = ParkedStream;

        async fn open_bi(
            &self,
        ) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr> {
            Err(QuicErrorRepr::QuicConnection("unused".into()))
        }
        async fn accept_bi(
            &self,
        ) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr> {
            Err(QuicErrorRepr::QuicConnection("unused".into()))
        }
        async fn open_uni(&self) -> Result<(Self::SendStream, u64), QuicErrorRepr> {
            Err(QuicErrorRepr::QuicConnection("unused".into()))
        }
        async fn accept_uni(&self) -> Result<(Self::RecvStream, u64), QuicErrorRepr> {
            // Only hand out the stream once a queued datagram has been taken,
            // so a test can measure what the datagram branch costs acceptance.
            loop {
                let taken = self.datagrams.lock().await.is_empty();
                if taken {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            match self.stream.lock().await.take() {
                Some(stream) => {
                    self.accepted.store(true, Ordering::SeqCst);
                    Ok((stream, 0))
                }
                None if self.stay_open => std::future::pending().await,
                // What a closed connection reports, and what ends the loop.
                None => Err(QuicErrorRepr::QuicConnection("closed".into())),
            }
        }
        async fn read_datagram(&self) -> Result<Bytes, QuicErrorRepr> {
            // Bind the pop before matching: a `match` scrutinee keeps its
            // temporaries alive for the whole match, which would hold this lock
            // across the pending branch below.
            let next = self.datagrams.lock().await.pop_front();
            match next {
                Some(datagram) => Ok(datagram),
                None => std::future::pending().await,
            }
        }
        async fn send_datagram(&self, _bytes: Bytes) -> Result<(), QuicErrorRepr> {
            Ok(())
        }
        fn close(&self, _error_code: u64, _reason: &[u8]) {}
        fn close_reason(&self) -> Option<QuicErrorRepr> {
            None
        }
        fn remote_address(&self) -> std::net::SocketAddr {
            "127.0.0.1:1".parse().unwrap()
        }
        fn peer_id(&self) -> u64 {
            0
        }
    }

    /// A sender whose send never completes, i.e. a consumer that stopped
    /// draining while its receiver is still alive.
    struct StuckSend {
        entered: mpsc::UnboundedSender<()>,
    }

    #[async_trait::async_trait]
    impl UdpSend for StuckSend {
        async fn send_to(&self, _buf: Bytes, _addr: SocksAddr) -> Result<usize, SError> {
            let _ = self.entered.send(());
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn datagram_delivery_does_not_wait_for_a_stalled_consumer() {
        let store = store();
        let (entered, _entered_rx) = mpsc::unbounded_channel();
        store
            .store_socket_with_prelude(7, (Arc::new(StuckSend { entered }), test_addr()))
            .await
            .unwrap();

        let started = tokio::time::Instant::now();
        let delivered = store.feed_datagram(7, Bytes::from_static(b"payload")).await;
        let waited = started.elapsed();

        // A consumer that stopped draining must cost a drop, not a wait: the
        // receive loop that awaits this has other associations to serve.
        assert!(delivered.is_err(), "the datagram should have been dropped");
        assert!(
            waited < Duration::from_millis(500),
            "delivery waited {waited:?} for a stalled consumer"
        );
    }

    #[tokio::test]
    async fn a_stalled_consumer_does_not_stop_accepting_streams() {
        let store = store();
        let (entered, _entered_rx) = mpsc::unbounded_channel();
        store
            .store_socket_with_prelude(7, (Arc::new(StuckSend { entered }), test_addr()))
            .await
            .unwrap();

        let accepted = Arc::new(AtomicBool::new(false));
        let conn = SQConn {
            conn: OneStreamConn {
                stream: Arc::new(Mutex::new(Some(ParkedStream {
                    dropped: Arc::new(AtomicBool::new(false)),
                }))),
                accepted: accepted.clone(),
                datagrams: Arc::new(Mutex::new(VecDeque::from([datagram_for(7).await]))),
                stay_open: false,
            },
            authed: Arc::new(SetOnce::new_with(Some(Ok("user".to_string())))),
            send_id_store: Default::default(),
            recv_id_store: store,
            stats: Default::default(),
        };
        let receiving = tokio::spawn(handle_udp_packet_recv(conn));

        // The datagram's consumer never drains, and the fake only offers the
        // stream after that datagram was taken, so acceptance is measured
        // across the delivery.
        tokio::time::timeout(Duration::from_millis(500), async {
            while !accepted.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a stalled consumer must not stop the loop from accepting streams");
        receiving.abort();
    }

    async fn datagram_for(id: u16) -> Bytes {
        let mut cursor = std::io::Cursor::new(vec![0u8; 2]);
        SQPacketDatagramHeader { id }
            .encode(&mut cursor)
            .await
            .unwrap();
        Bytes::from(cursor.into_inner())
    }

    #[tokio::test]
    async fn a_malformed_datagram_does_not_stop_the_loop() {
        let store = store();
        let (tx, mut rx) = mpsc::unbounded_channel();
        store
            .store_socket_with_prelude(7, (Arc::new(LiveSend(tx)), test_addr()))
            .await
            .unwrap();

        let conn = SQConn {
            conn: OneStreamConn {
                stream: Arc::new(Mutex::new(None)),
                accepted: Arc::new(AtomicBool::new(false)),
                datagrams: Arc::new(Mutex::new(VecDeque::from([
                    // Truncated header: decoding this fails.
                    Bytes::from_static(&[0x00]),
                    datagram_for(7).await,
                ]))),
                stay_open: true,
            },
            authed: Arc::new(SetOnce::new_with(Some(Ok("user".to_string())))),
            send_id_store: Default::default(),
            recv_id_store: store.clone(),
            stats: Default::default(),
        };
        let receiving = tokio::spawn(handle_udp_packet_recv(conn));

        // The malformed datagram must not end the connection's reception: the
        // valid one queued behind it still arrives.
        let delivered = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a malformed datagram must not stop the loop");
        assert!(delivered.is_some(), "the valid datagram should arrive");
        receiving.abort();
    }

    #[tokio::test]
    async fn datagram_delivery_does_not_hold_the_store_lock() {
        let store = store();
        let (entered, mut entered_rx) = mpsc::unbounded_channel();
        store
            .store_socket_with_prelude(7, (Arc::new(StuckSend { entered }), test_addr()))
            .await
            .unwrap();

        let feeding = {
            let store = store.clone();
            tokio::spawn(
                async move { store.feed_datagram(7, Bytes::from_static(b"payload")).await },
            )
        };
        // Wait until the send is in flight, so the lookup is over and only the
        // delivery is left.
        entered_rx
            .recv()
            .await
            .expect("delivery should have started");

        // Any other id on this connection must still be able to use the store.
        assert!(
            store.inner.try_write().is_ok(),
            "the store lock must not be held across the delivery"
        );
        feeding.abort();
    }

    #[tokio::test]
    async fn a_failed_prelude_still_registers_the_socket() {
        let store = store();
        // A datagram arrived before its control header, so the id is pending
        // with one buffered packet.
        store
            .feed_datagram(7, Bytes::from_static(b"early"))
            .await
            .unwrap();

        // The consumer is stuck, so flushing that prelude cannot succeed.
        let (entered, _entered_rx) = mpsc::unbounded_channel();
        let mut session = AssociateRecvSession {
            id_store: store.clone(),
            id_map: Default::default(),
        };
        session
            .store_socket(7, test_addr(), Arc::new(StuckSend { entered }))
            .await
            .expect("a lost prelude datagram must not fail the registration");
        assert!(
            matches!(store.inner.read().await.get(&7), Some(Ok(_))),
            "the id should be registered"
        );

        // The association ends, and it must take its id with it.
        drop(session);
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.inner.read().await.contains_key(&7) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the association teardown must remove its registered id");
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_control_header_still_resolves_stream() {
        let store = store();
        let lookup = resolve_udp_session(
            &store,
            7,
            tokio::time::Instant::now() + UDP_SESSION_RESOLVE_TIMEOUT,
        );
        tokio::pin!(lookup);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(6)) => {}
            result = &mut lookup => panic!("lookup ended early: {:?}", result.err()),
        }
        let (tx, _rx) = mpsc::channel(1);
        let addr = SocksAddr::from("127.0.0.1:9000".parse::<std::net::SocketAddr>().unwrap());
        store
            .store_socket_with_prelude(7, (Arc::new(tx), addr.clone()))
            .await
            .unwrap();
        assert_eq!(lookup.await.unwrap().1, addr);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_stream_cleans_its_pending_id() {
        let store = store();
        // A datagram can arrive before the control header and be buffered in
        // the same pending entry as the stream waiter.
        store
            .feed_datagram(7, Bytes::from_static(b"orphan"))
            .await
            .unwrap();
        assert!(matches!(
            resolve_udp_session(
                &store,
                7,
                tokio::time::Instant::now() + UDP_SESSION_RESOLVE_TIMEOUT,
            )
            .await,
            Err(SError::UDPSessionClosed(_))
        ));
        assert!(!store.inner.read().await.contains_key(&7));
    }

    #[tokio::test]
    async fn timeout_cleanup_does_not_remove_registered_socket() {
        let store = store();
        let (tx, _rx) = mpsc::channel(1);
        let addr = SocksAddr::from("127.0.0.1:9000".parse::<std::net::SocketAddr>().unwrap());
        store
            .store_socket_with_prelude(7, (Arc::new(tx), addr))
            .await
            .unwrap();
        assert!(store.try_remove_unwatched_pending(7));
        assert!(matches!(store.inner.read().await.get(&7), Some(Ok(_))));
    }

    #[tokio::test]
    async fn timed_out_waiter_does_not_remove_another_waiter() {
        let store = store();
        let first = match store.get_socket_or_notify(7).await {
            Err(waiter) => waiter,
            Ok(_) => panic!("expected pending id"),
        };
        let second = match store.get_socket_or_notify(7).await {
            Err(waiter) => waiter,
            Ok(_) => panic!("expected pending id"),
        };
        drop(first);
        assert!(store.try_remove_unwatched_pending(7));
        assert!(store.inner.read().await.contains_key(&7));
        drop(second);
        assert!(store.try_remove_unwatched_pending(7));
        assert!(!store.inner.read().await.contains_key(&7));
    }

    /// A sender that is already gone, i.e. its association ended.
    struct GoneSend(mpsc::UnboundedSender<Bytes>);

    #[async_trait::async_trait]
    impl UdpSend for GoneSend {
        async fn send_to(&self, buf: Bytes, _addr: SocksAddr) -> Result<usize, SError> {
            let len = buf.len();
            let _ = self.0.send(buf);
            Ok(len)
        }
        async fn closed(&self) {}
    }

    #[tokio::test]
    async fn relay_forwards_packets_while_the_association_lives() {
        let (mut reader, mut writer) = tokio::io::duplex(64);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let addr = test_addr();
        let relay =
            tokio::spawn(async move { relay_uni_stream(&mut reader, &LiveSend(tx), &addr).await });

        (5u16).encode(&mut writer).await.unwrap();
        writer.write_all(b"hello").await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), Bytes::from_static(b"hello"));

        // A closed stream ends the relay.
        drop(writer);
        assert!(relay.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn relay_ends_when_the_association_is_gone() {
        // The writer stays alive, so the stream is silent forever and only the
        // association can end the relay.
        let (mut reader, _writer) = tokio::io::duplex(64);
        let (tx, _rx) = mpsc::unbounded_channel();
        let addr = test_addr();
        let relay = tokio::time::timeout(
            Duration::from_secs(5),
            relay_uni_stream(&mut reader, &GoneSend(tx), &addr),
        )
        .await
        .expect("a parked relay must not wait for a payload that never comes");
        assert!(matches!(relay, Err(SError::UDPSessionClosed(_))));
    }

    #[tokio::test]
    async fn stream_tasks_do_not_outlive_the_connection() {
        let dropped = Arc::new(AtomicBool::new(false));
        let conn = SQConn {
            conn: OneStreamConn {
                stream: Arc::new(Mutex::new(Some(ParkedStream {
                    dropped: dropped.clone(),
                }))),
                accepted: Arc::new(AtomicBool::new(false)),
                datagrams: Arc::new(Mutex::new(VecDeque::new())),
                stay_open: false,
            },
            authed: Arc::new(SetOnce::new_with(Some(Ok("user".to_string())))),
            send_id_store: Default::default(),
            recv_id_store: store(),
            stats: Default::default(),
        };

        // A connection that is gone is what ends the receive loop.
        assert!(handle_udp_packet_recv(conn).await.is_err());

        tokio::time::timeout(Duration::from_secs(5), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a parked stream task must not outlive the connection");
    }

    #[tokio::test]
    async fn orphaned_pending_id_is_removed_after_a_busy_cleanup() {
        let store = store();
        // A datagram for an id with no control header leaves a pending entry
        // that no association owns.
        store
            .feed_datagram(7, Bytes::from_static(b"orphan"))
            .await
            .unwrap();
        assert!(store.inner.read().await.contains_key(&7));

        let held = store.inner.write().await;
        let ended = tokio::time::timeout(
            Duration::from_secs(5),
            resolve_udp_session(&store, 7, tokio::time::Instant::now()),
        )
        .await;
        assert!(
            matches!(ended, Ok(Err(SError::UDPSessionClosed(_)))),
            "the stream task must end at its deadline"
        );
        drop(held);

        // The deadline cleanup could not take the lock, and nothing else owns
        // this id, so it must not be lost.
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.inner.read().await.contains_key(&7) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("an orphaned pending id must be removed once the store is free");
    }

    #[tokio::test]
    async fn expired_stream_cleanup_does_not_wait_for_the_store_lock() {
        let store = store();
        let held = store.inner.write().await;

        // The deadline is what bounds the stream task, so the cleanup after it
        // must not queue behind a held lock.
        let ended = tokio::time::timeout(
            Duration::from_secs(5),
            resolve_udp_session(&store, 7, tokio::time::Instant::now()),
        )
        .await;
        drop(held);
        assert!(
            matches!(ended, Ok(Err(SError::UDPSessionClosed(_)))),
            "the stream task must end at its deadline"
        );
    }
}
