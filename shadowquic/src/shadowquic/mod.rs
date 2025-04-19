use std::{
    collections::HashMap,
    io::Cursor,
    mem::replace,
    ops::Deref,
    sync::{Arc, atomic::AtomicU16},
};

use bytes::{BufMut, Bytes, BytesMut};
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::{Notify, RwLock, mpsc::channel};
use tracing::{error, info, trace, trace_span};

use crate::{
    AnyUdpRecv, AnyUdpSend,
    error::SError,
    msgs::{
        shadowquic::{SQPacketDatagramHeader, SQPacketStreamHeader, SQUdpControlHeader},
        socks5::{SDecode, SEncode, SocksAddr},
    },
};

pub mod inbound;
pub mod outbound;

#[derive(Clone)]
pub struct SQConn {
    conn: Connection,
    id_store: IDStore,
}

impl Deref for SQConn {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

type IDStoreVal = Result<(AnyUdpSend, SocksAddr), Arc<Notify>>;
#[derive(Clone, Default)]
struct IDStore {
    id_counter: Arc<AtomicU16>,
    inner: Arc<RwLock<HashMap<u16, IDStoreVal>>>,
}

impl IDStore {
    async fn get_socket(&self, id: u16) -> Result<(AnyUdpSend, SocksAddr), Arc<Notify>> {
        let mut h = self.inner.write().await;
        if let Some(r) = h.get_mut(&id) {
            r.clone()
        } else {
            let notify = Arc::new(Notify::new());
            h.insert(id, Err(notify.clone()));
            Err(notify)
        }
    }
    async fn get_socket_or_wait(&self, id: u16) -> (AnyUdpSend, SocksAddr) {
        match self.get_socket(id).await {
            Ok(r) => r,
            Err(n) => {
                n.notified().await;
                self.get_socket(id).await.unwrap()
            }
        }
    }
    async fn store_socket(&self, id: u16, socket: AnyUdpSend, dst: SocksAddr) {
        let mut h = self.inner.write().await;
        let r = h.get_mut(&id);
        if let Some(s) = r {
            match s {
                Ok(_) => {}
                Err(_) => {
                    let notify = replace(s, Ok((socket, dst)));
                    let _ = notify.map_err(|x| x.notify_one());
                }
            }
        } else {
            h.insert(id, Ok((socket, dst)));
        }
    }
    async fn fetch_new_id(&self, socket: AnyUdpSend, dst: SocksAddr) -> u16 {
        let mut inner = self.inner.write().await;
        let mut r;
        loop {
            r = self
                .id_counter
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let std::collections::hash_map::Entry::Vacant(e) = inner.entry(r) {
                e.insert(Ok((socket.clone(), dst)));
                break;
            }
        }
        r
    }
}

struct AssociateSendSession {
    id_store: IDStore,
    dst_map: HashMap<SocksAddr, u16>,
    unistream_map: HashMap<SocksAddr, SendStream>,
}
impl AssociateSendSession {
    pub async fn get_id_or_insert(&mut self, addr: &SocksAddr, socket: AnyUdpSend) -> u16 {
        if let Some(id) = self.dst_map.get(addr) {
            *id
        } else {
            let id = self.id_store.fetch_new_id(socket, addr.clone()).await;
            self.dst_map.insert(addr.clone(), id);
            id
        }
    }
}

impl Drop for AssociateSendSession {
    fn drop(&mut self) {
        let id_store = self.id_store.inner.clone();
        let id_remove = self.dst_map.clone();
        tokio::spawn(async move {
            let mut id_store = id_store.write().await;
            let _ = id_remove.values().map(|k| id_store.remove(k));
        });
    }
}
// There are two usages for id_map
// First, it works as local cache avoiding using global store repeatedly witch is more expensive
struct AssociateRecvSession {
    id_store: IDStore,
    id_map: HashMap<u16, SocksAddr>,
}
impl AssociateRecvSession {
    pub async fn store_socket(&mut self, id: &u16, dst: SocksAddr, socks: AnyUdpSend) {
        if self.id_map.contains_key(id) {
        } else {
            self.id_store.store_socket(*id, socks, dst.clone()).await;
            self.id_map.insert(*id, dst);
        }
    }
}

impl Drop for AssociateRecvSession {
    fn drop(&mut self) {
        let id_store = self.id_store.inner.clone();
        let id_remove = self.id_map.clone();
        tokio::spawn(async move {
            let mut id_store = id_store.write().await;
            let _ = id_remove.keys().map(|k| id_store.remove(k));
        });
    }
}

pub async fn handle_udp_send(
    mut send: SendStream,
    udp_send: AnyUdpSend,
    udp_recv: AnyUdpRecv,
    conn: SQConn,
    over_stream: bool,
) -> Result<(), SError> {
    let mut down_stream = udp_recv;
    let mut session = AssociateSendSession {
        id_store: conn.id_store.clone(),
        dst_map: Default::default(),
        unistream_map: Default::default(),
    };
    let quic_conn = conn.conn.clone();
    loop {
        let (bytes, dst) = down_stream.recv_from().await?;
        let id = session.get_id_or_insert(&dst, udp_send.clone()).await;
        let span = trace_span!("udp", id = id);
        let _ = span.enter();
        let ctl_header = SQUdpControlHeader {
            dst: dst.clone(),
            id,
        };
        let dg_header = SQPacketDatagramHeader { id };
        if over_stream && !session.unistream_map.contains_key(&dst) {
            session
                .unistream_map
                .insert(dst.clone(), conn.open_uni().await?);
        }

        let fut1 = async {
            ctl_header.encode(&mut send).await?;
            trace!("udp control header sent");
            Ok(()) as Result<(), SError>
        };
        let fut2 = async {
            let mut content = BytesMut::with_capacity(1600);
            let mut head = Vec::<u8>::new();
            dg_header.encode(&mut head).await?;
            if over_stream {
                (bytes.len() as u16).encode(&mut head).await?;
            }
            content.put(Bytes::from(head));
            content.put(bytes);
            let content = content.freeze();
            if over_stream {
                if let Some(conn) = session.unistream_map.get_mut(&dst) {
                    conn.write_all(&content).await?;
                }
            } else {
                let _ = quic_conn.send_datagram(content);
            }
            Ok(())
        };
        tokio::try_join!(fut1, fut2)?;
    }
    #[allow(unreachable_code)]
    Ok(())
}

pub async fn handle_udp_recv_ctrl(
    mut recv: RecvStream,
    udp_socket: AnyUdpSend,
    conn: SQConn,
) -> Result<(), SError> {
    let mut session = AssociateRecvSession {
        id_store: conn.id_store.clone(),
        id_map: Default::default(),
    };
    loop {
        let SQUdpControlHeader { id, dst } = SQUdpControlHeader::decode(&mut recv).await?;
        session.store_socket(&id, dst, udp_socket.clone()).await;
    }
    #[allow(unreachable_code)]
    Ok(())
}

pub async fn handle_udp_packet_recv(conn: SQConn) -> Result<(), SError> {
    let id_store = conn.id_store.clone();

    let mut id_map = HashMap::<u16, (AnyUdpSend, SocksAddr)>::new();
    let (send, mut recv) = channel(10);

    loop {
        tokio::select! {
            Ok(b) = conn.read_datagram() => {
                let b = BytesMut::from(b);
                let mut cur = Cursor::new(b);
                let SQPacketDatagramHeader{id} = SQPacketDatagramHeader::decode(&mut cur).await?;
                
                if let Some((udp,addr)) = id_map.get(&id) {
                    info!("datagram accepted: id:{},dst:{}",id, addr);
                    let pos = cur.position() as usize;
                    let b = cur.into_inner().freeze();
                    udp.send_to(b.slice(pos..b.len()), addr.clone()).await?;
                } else {
                    let id_store = id_store.clone();
                    let sender = send.clone();
                    tokio::spawn(async move {
                        let (udp,addr) = id_store.get_socket_or_wait(id).await;
                        let pos = cur.position() as usize;
                        let b = cur.into_inner().freeze();
                        let _ = udp.clone().send_to(b.slice(pos..b.len()), addr.clone()).await
                        .map_err(|x|error!("{}",x));
                        let _ = sender.send((id, udp,addr)).await.map_err(|_e|SError::ChannelError("can't open send udp trait".into()));
                    });
                }
            }

            Some((id, udp, addr)) = recv.recv() => {
                id_map.insert(id, (udp,addr));
            }

            r = async {
                let mut uni_stream = conn.accept_uni().await?;
                trace!("unistream accepted");
                let SQPacketStreamHeader{id,len} = SQPacketStreamHeader::decode(&mut uni_stream).await?;
                let (udp,addr) = match id_map.get(&id){
                    Some(r) => r,
                    None => {
                    let (udp,addr) = id_store.get_socket_or_wait(id).await;

                let _ = send.send((id, udp.clone(),addr.clone())).await.map_err(|_e|SError::ChannelError("can't open send udp trait".into()));
                &(udp.to_owned(),addr.to_owned())
                }};
                info!("unistream datagram accepted: id:{},dst:{}",id, addr);
                Ok((uni_stream,udp.clone(),addr.clone(),len)) as Result<(RecvStream,AnyUdpSend,SocksAddr,u16),SError>
            } => {
                let  (mut uni_stream,udp,addr,mut len) = r?;

                tokio::spawn(async move {
                    loop {
                        let l = usize::from(len);
                        let mut b = BytesMut::with_capacity(l);
                        b.resize(l,0);
                        uni_stream.read_exact(&mut b).await?;
                        udp.send_to(b.freeze(), addr.clone()).await?;
                        SQPacketStreamHeader{id:_,len} = SQPacketStreamHeader::decode(&mut uni_stream).await?;
                    }
                    #[allow(unreachable_code)]
                    (Ok(()) as Result<(), SError>)
                });
            }
        }
    }
    #[allow(unreachable_code)]
    Ok(())
}
