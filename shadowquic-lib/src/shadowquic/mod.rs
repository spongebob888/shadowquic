use std::{any::Any, collections::HashMap, ops::Deref, sync::Arc};

use bytes::Bytes;
use quinn::{Connection, SendStream};
use tokio::sync::{
    Mutex,
    mpsc::{Receiver, Sender, channel},
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
struct IDStore(Arc<Mutex<HashMap<u16, (Option<Sender<AnyUdp>>, Option<Receiver<AnyUdp>>)>>>);

impl IDStore {
    async fn get_recv_or_insert(&self, id: u16) -> Option<Receiver<AnyUdp>>{
        let mut h = self.0.lock().await;
        let r = h.get_mut(&id);
        if let Some((_, x)) = r {
            let out = x.take();
            out
        } else {
            let (s,r)  = channel::<AnyUdp>(10);
            h.insert(id, (Some(s),None));
            Some(r)
        }
    }
    async fn get_send_or_insert(&self, id: u16) -> Option<Sender<AnyUdp>>{
        let mut h = self.0.lock().await;
        let r = h.get_mut(&id);
        if let Some((x,_)) = r {
            let out = x.take();
            out
        } else {
            let (s,r)  = channel::<AnyUdp>(10);
            h.insert(id, (None,Some(r)));
            Some(s)
        }
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
    pub async fn try_send_socket(&mut self, id: &u16,  dst: T, udp_socket: AnyUdp) -> Result<(),SError> {
        if self.id_map.contains_key(id) {
           return Ok(()); 
        } else {
            self.id_map.insert(id.clone(), dst);
            if let Some(s) = self.id_store.get_send_or_insert(id.clone()).await {
                s.send(udp_socket).await
                .map_err(|x|SError::ChannelError("can't send udp socket to driver".into()))?;
            }
        }
        Ok(())
    }
}
impl<T> Drop for AssociateRecvSession<T> {
    fn drop(&mut self) {
        todo!()
    }
}