use std::{any::Any, collections::HashMap, mem::replace, ops::Deref, sync::{atomic::{AtomicI16, AtomicI64, AtomicU16}, Arc}};

use bytes::Bytes;
use quinn::{Connection, SendStream};
use tokio::sync::{
    mpsc::{channel, Receiver, Sender}, Mutex, Notify, RwLock
};

use crate::{error::SError, msgs::socks5::SocksAddr, AnyUdpSend};

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
        // TODO
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
        //TODO
    }
}