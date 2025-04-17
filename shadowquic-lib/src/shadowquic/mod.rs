use std::{collections::HashMap, ops::Deref, sync::{ Arc}};

use bytes::Bytes;
use quinn::Connection;
use tokio::sync::{
    Mutex,
    mpsc::{Receiver, Sender, channel},
};

use crate::msgs::socks5::SocksAddr;

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

#[derive(Clone)]
struct IDStore(Arc<Mutex<HashMap<u16, (Option<Sender<Bytes>>, Option<Receiver<Bytes>>)>>>);

impl IDStore {
    async fn get_recv_or_insert(&self, id: u16) -> Option<Receiver<Bytes>>{
        let mut h = self.0.lock().await;
        let r = h.get_mut(&id);
        if let Some((_, x)) = r {
            let out = x.take();
            out
        } else {
            let (s,r)  = channel::<Bytes>(10);
            h.insert(id, (Some(s),None));
            Some(r)
        }
    }
}

struct AssociateSendSession {
    id_store: IDStore,
    dst_map: HashMap<SocksAddr, u16>,
}
impl AssociateSendSession {
    pub fn get_id(&self, addr: &SocksAddr) -> Option<u16> {
        self.dst_map.get(addr).copied()
    }
    pub fn get_id_or_insert(&self, addr: &SocksAddr) -> u16 {
        if let Some(id) = self.dst_map.get(addr) {
            id.clone()
        } else {
            todo!()
        }
    }
}

impl Drop for AssociateSendSession {
    fn drop(&mut self) {
        todo!()
    }
}

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
}
impl Drop for AssociateRecvSession {
    fn drop(&mut self) {
        todo!()
    }
}