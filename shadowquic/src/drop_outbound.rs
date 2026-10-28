use crate::{Outbound, ProxyRequest, error::SError};

/// Outbound that closes TCP sessions and discards UDP sessions.
#[derive(Default)]
pub(crate) struct DropOutbound {
    pub tag: String,
}

#[async_trait::async_trait]
impl Outbound for DropOutbound {
    #[tracing::instrument(name = "outbound", skip_all, fields(tag = %self.tag))]
    async fn handle(&self, req: ProxyRequest) -> Result<(), SError> {
        drop(req);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnyUdpRecv, AnyUdpSend, UdpSession, UserContext};
    use bytes::Bytes;
    use std::{net::SocketAddr, sync::Arc};

    #[tokio::test]
    async fn dropping_udp_request_closes_its_receiver() {
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let dst: SocketAddr = "127.0.0.1:53".parse().unwrap();
        sender.send((Bytes::new(), dst.into())).await.unwrap();
        let session = UdpSession::<AnyUdpRecv, AnyUdpSend> {
            recv: Box::new(receiver),
            send: Arc::new(sender.clone()),
            stream: None,
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap().into(),
            dst: "127.0.0.1:0".parse::<SocketAddr>().unwrap().into(),
            src_addr: None,
            user_context: UserContext::default(),
        }
        .wait_first_packet()
        .await
        .unwrap();

        DropOutbound::default()
            .handle(ProxyRequest::Udp(session))
            .await
            .unwrap();

        assert!(sender.try_send((Bytes::new(), dst.into())).is_err());
    }
}
