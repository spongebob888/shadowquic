use std::{any::Any, collections::HashMap, io::Cursor, mem::replace, ops::Deref, sync::{atomic::{AtomicI16, AtomicI64, AtomicU16}, Arc}};

use bytes::{BufMut, Bytes, BytesMut};
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::{
    mpsc::{channel, Receiver, Sender}, Mutex, Notify, RwLock
};
use tracing::{error, trace, trace_span};

use crate::{error::SError, msgs::{shadowquic::{SQPacketDatagramHeader, SQUdpControlHeader}, socks5::{SDecode, SEncode, SocksAddr}}, AnyUdpRecv, AnyUdpSend};

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

#[derive(Clone, Default)]
struct IDStore {
    id_counter: Arc<AtomicU16>,
    inner: Arc<RwLock<HashMap<u16, Result<(AnyUdpSend,SocksAddr),Arc<Notify>>>>>
}

impl IDStore {
    async fn get_socket(&self, id: u16) -> Result<(AnyUdpSend,SocksAddr),Arc<Notify>>{
        let mut h = self.inner.write().await;
        if let Some(r) = h.get_mut(&id){
            r.clone()
        } else {
            let notify = Arc::new(Notify::new());
            h.insert(id, Err(notify.clone()));
            Err(notify)
        }
    }
    async fn store_socket(&self, id: u16, socket: AnyUdpSend, dst: SocksAddr) {
        let mut h = self.inner.write().await;
        let r = h.get_mut(&id);
        if let Some(s) = r {
            match s {
                Ok(_) => {}
                Err(_) => {
                    let notify = replace(s, Ok((socket,dst)));
                    let _ = notify.map_err(|x|x.notify_one());
                }
            }
        } else {
            h.insert(id, Ok((socket,dst)));
        }
    }
    async fn fetch_new_id(&self, socket: AnyUdpSend, dst: SocksAddr) -> u16{
        let mut inner = self.inner.write().await;
        let mut r;
        loop {
            r = self.id_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if !inner.contains_key(&r) {
                inner.insert(r, Ok((socket.clone(),dst)));
                break;
            }
        }
        r
    }
}

struct AssociateSendSession {
    id_store: IDStore,
    dst_map: HashMap<SocksAddr, u16>,
    unistream_map: Option<HashMap<SocksAddr, SendStream>>,
}
impl AssociateSendSession {
    pub fn get_id(&self, addr: &SocksAddr) -> Option<u16> {
        self.dst_map.get(addr).copied()
    }
    pub async fn get_id_or_insert(&mut self, addr: &SocksAddr, socket: AnyUdpSend) -> u16 {
        if let Some(id) = self.dst_map.get(addr) {
            id.clone()
        } else {
            let id = self.id_store.fetch_new_id(socket, addr.clone()).await;
            self.dst_map.insert( addr.clone(), id);
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
            let _ = id_remove.iter().map(|(_,k)|id_store.remove(k));
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

    pub fn get_addr(&self, id: &u16) -> Option<&SocksAddr> {
        self.id_map.get(id)
    }
    pub fn get_addr_or_insert(&self, id: &u16) -> &SocksAddr {
        if let Some(addr) = self.id_map.get(id) {
            addr
        } else {
            todo!()
        }
    }
    pub async fn store_socket(&mut self, id: &u16, dst: SocksAddr, socks: AnyUdpSend) {
        if self.id_map.contains_key(id) {
            return;
        } else {
            self.id_store.store_socket(id.clone(), socks, dst.clone()).await;
            self.id_map.insert(*id,dst);
        }
    }
}

impl Drop for AssociateRecvSession {
    fn drop(&mut self) {
        let id_store = self.id_store.inner.clone();
        let id_remove = self.id_map.clone();
        tokio::spawn(async move {
            let mut id_store = id_store.write().await;
            let _ = id_remove.iter().map(|(k,_)|id_store.remove(k));
        });
    }
}


pub async fn handle_udp_send_overdatagram(
    mut send: SendStream,
    udp_send: AnyUdpSend,
    udp_recv: AnyUdpRecv,
    conn: SQConn,
    over_stream: bool,
) -> Result<(), SError> {
    let mut down_stream = udp_recv;
    let mut buf_down = vec![0u8; 1600];
    let mut buf_up: Vec<u8> = vec![0u8; 1600];
    let mut session = AssociateSendSession {
        id_store: conn.id_store.clone(),
        dst_map: Default::default(),
        unistream_map: Default::default(),
    };
    let quic_conn = conn.conn.clone();
        loop {
            let (bytes, dst) = down_stream.recv_from().await?;
            let id = session.get_id_or_insert(&dst, udp_send.clone()).await;
            let span = trace_span!("udp",id=id);
            let _ = span.enter();
            let ctl_header = SQUdpControlHeader { dst, id };
            let dg_header = SQPacketDatagramHeader { id };
            let fut1 = async {
                let r = ctl_header
                    .encode(&mut send)
                    .await
                    .map_err(|x| error!("{}", x));
                trace!("udp control header sent");
            };
            let fut2 = async {
                let mut content = BytesMut::with_capacity(1600);
                let mut head = Vec::<u8>::new();
                let r = dg_header
                    .encode(&mut head)
                    .await
                    .map_err(|x| error!("{}", x));
                content.put(Bytes::from(head));
                content.put(bytes);
                let content = content.freeze();
                let r = quic_conn.send_datagram(content);
                trace!("udp datagram sent");
            };
            tokio::join!(fut1, fut2);
        }

    Ok(())
}

pub async fn handle_udp_recv_overdatagram(
    mut recv: RecvStream,
    udp_socket: AnyUdpSend,
    conn: SQConn,
    over_stream: bool,
)  -> Result<(), SError>
{
    let mut session = AssociateRecvSession {
        id_store: conn.id_store.clone(),
        id_map: Default::default(),
    };
    loop {
        let SQUdpControlHeader{id,dst} = SQUdpControlHeader::decode(&mut recv).await?;
        session.store_socket(&id, dst, udp_socket.clone()).await;
    }
    Ok(())
}

pub async fn handle_udp_packet_recv(    
    conn: SQConn) -> Result<(), SError> {
        let id_store = conn.id_store.clone();

        let mut id_map = HashMap::<u16, (AnyUdpSend,SocksAddr)>::new();
        let (send,mut recv) = channel(10);
        let over_stream = false;
        if over_stream == false {
            loop {
                tokio::select! {
                Ok(b) = conn.read_datagram() => {
                    trace!("datagram accepted");
                    let mut b = BytesMut::from(b);
                    let mut cur = Cursor::new(b);
                    let SQPacketDatagramHeader{id} = SQPacketDatagramHeader::decode(&mut cur).await?;
                    // To be fixed, may block here
                    if let Some((udp,addr)) = id_map.get(&id) {
                        let pos = cur.position() as usize;
                        let b = cur.into_inner().freeze();
                        udp.send_to(b.slice(pos..b.len()), addr.clone()).await?;
                    } else {
                        let id_store = id_store.clone();
                        let sender = send.clone();
                        tokio::spawn(async move {
                            let (udp,addr) = id_store.get_socket(id).await.unwrap();
                            let pos = cur.position() as usize;
                            let b = cur.into_inner().freeze();
                            let _ = udp.clone().send_to(b.slice(pos..b.len()), addr.clone()).await
                            .map_err(|x|error!("{}",x));
                            let _ = sender.send((id, udp,addr)).await.map_err(|e|SError::ChannelError("can't open send udp trait".into()));
                        });
                    }



                }   
                Some((id, udp, addr)) = recv.recv() => { 
                    id_map.insert(id, (udp,addr));
                }
            }
            }
        }
        
        Ok(())
    }