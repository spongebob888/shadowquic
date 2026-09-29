use crate::{Outbound, ProxyRequest, error::SError};

/// Outbound that closes TCP sessions and discards UDP sessions.
pub(crate) struct DropOutbound;

#[async_trait::async_trait]
impl Outbound for DropOutbound {
    async fn handle(&self, req: ProxyRequest) -> Result<(), SError> {
        drop(req);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{UdpSession, UserContext};
    use bytes::Bytes;
    use std::{net::SocketAddr, sync::Arc};

    #[tokio::test]
    async fn dropping_udp_request_closes_its_receiver() {
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let dst: SocketAddr = "127.0.0.1:53".parse().unwrap();
        sender.send((Bytes::new(), dst.into())).await.unwrap();
        let session = UdpSession::from_recv(
            Arc::new(sender.clone()),
            Box::new(receiver),
            None,
            "127.0.0.1:0".parse::<SocketAddr>().unwrap().into(),
            UserContext::default(),
        )
        .await
        .unwrap();

        DropOutbound
            .handle(ProxyRequest::Udp(session))
            .await
            .unwrap();

        assert!(sender.try_send((Bytes::new(), dst.into())).is_err());
    }
}
