use std::{any::Any, collections::HashMap, mem::replace, ops::Deref, sync::{atomic::{AtomicI16, AtomicI64, AtomicU16}, Arc}};

use bytes::Bytes;
use quinn::{Connection, SendStream};
use tokio::sync::{
    mpsc::{channel, Receiver, Sender}, Mutex, Notify, RwLock
};

use crate::{error::SError, msgs::socks5::SocksAddr, AnyUdp, UdpSocketTrait};

pub mod inbound;
pub mod outbound;

#[derive(Clone)]
struct SQConn {
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
    inner: Arc<RwLock<HashMap<u16, Result<AnyUdp,Arc<Notify>>>>>
}

impl IDStore {
    async fn get_socket(&self, id: u16) -> Result<AnyUdp,Arc<Notify>>{
        let mut h = self.inner.write().await;
        if let Some(r) = h.get_mut(&id){
            r.clone()
        } else {
            let notify = Arc::new(Notify::new());
            h.insert(id, Err(notify.clone()));
            Err(notify)
        }
    }
    async fn store_socket(&self, id: u16, socket: AnyUdp) {
        let mut h = self.inner.write().await;
        let r = h.get_mut(&id);
        if let Some(s) = r {
            match s {
                Ok(_) => {}
                Err(_) => {
                    let notify = replace(s, Result::<AnyUdp,Arc<Notify>>::Ok(socket));
                    let _ = notify.map_err(|x|x.notify_one());
                }
            }
        } else {
            h.insert(id, Result::<AnyUdp,Arc<Notify>>::Ok(socket));
        }
    }
    async fn fetch_new_id(&self, socket: AnyUdp) -> u16{
        let mut inner = self.inner.write().await;
        let mut r;
        loop {
            r = self.id_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if !inner.contains_key(&r) {
                inner.insert(r, Ok(socket.clone()));
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
    pub async fn get_id_or_insert(&mut self, addr: &SocksAddr, socket: AnyUdp) -> u16 {
        if let Some(id) = self.dst_map.get(addr) {
            id.clone()
        } else {
            let id = self.id_store.fetch_new_id(socket).await;
            self.dst_map.insert( addr.clone(), id);
            id
        }
    }
}

impl Drop for AssociateSendSession {
    fn drop(&mut self) {
        todo!()
    }
}

struct AssociateRecvSession<T> {
    id_store: IDStore,
    id_map: HashMap<u16, T>,
}
impl<T> AssociateRecvSession<T> {

    pub fn get_addr(&self, id: &u16) -> Option<&T> {
        self.id_map.get(id)
    }
    pub fn get_addr_or_insert(&self, id: &u16) -> &T {
        if let Some(addr) = self.id_map.get(id) {
            addr
        } else {
            todo!()
        }
    }
    pub async fn store_socket(&mut self, id: &u16, dst: T, socks: AnyUdp) {
        if self.id_map.contains_key(id) {
            return;
        } else {
            self.id_store.store_socket(id.clone(), socks).await;
            self.id_map.insert(*id,dst);
        }
    }
}

impl<T> Drop for AssociateRecvSession<T> {
    fn drop(&mut self) {
        todo!()
    }
}