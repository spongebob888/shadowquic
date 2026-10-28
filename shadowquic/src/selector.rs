//! Runtime selection among a fixed set of outbound instances.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use serde::Serialize;

use crate::{Outbound, ProxyRequest, config::SelectorOutCfg, error::SError};

pub struct SelectorOutbound {
    tag: String,
    members: Vec<(String, Arc<dyn Outbound>)>,
    selected: AtomicUsize,
}

#[derive(Serialize)]
pub struct SelectorState<'a> {
    pub selected: &'a str,
    pub outbounds: Vec<&'a str>,
}

/// Runtime member selection, independent of outbound request handling.
pub trait Selectable: Send + Sync {
    fn state(&self) -> SelectorState<'_>;

    /// Applies to subsequent sessions. Running sessions retain their member.
    fn select(&self, tag: &str) -> Result<SelectorState<'_>, SError>;
}

impl SelectorOutbound {
    pub fn new(
        cfg: SelectorOutCfg,
        outbounds: &HashMap<String, Arc<dyn Outbound>>,
    ) -> Result<Self, SError> {
        cfg.validate()?;
        let selected = cfg
            .selected
            .as_ref()
            .map(|selected| {
                cfg.outbounds
                    .iter()
                    .position(|tag| tag == selected)
                    .unwrap()
            })
            .unwrap_or(0);
        let members = cfg
            .outbounds
            .into_iter()
            .map(|tag| {
                let outbound = outbounds.get(&tag).cloned().ok_or_else(|| {
                    SError::InvalidConfig(format!("unknown selector outbound: {tag}"))
                })?;
                Ok((tag, outbound))
            })
            .collect::<Result<_, SError>>()?;
        Ok(Self {
            tag: cfg.tag,
            members,
            selected: AtomicUsize::new(selected),
        })
    }

    fn state_at(&self, index: usize) -> SelectorState<'_> {
        SelectorState {
            selected: &self.members[index].0,
            outbounds: self.members.iter().map(|(tag, _)| tag.as_str()).collect(),
        }
    }
}

impl Selectable for SelectorOutbound {
    fn state(&self) -> SelectorState<'_> {
        self.state_at(self.selected.load(Ordering::Relaxed))
    }

    fn select(&self, tag: &str) -> Result<SelectorState<'_>, SError> {
        let index = self
            .members
            .iter()
            .position(|(member, _)| member == tag)
            .ok_or_else(|| {
                SError::InvalidConfig(format!("outbound is not a selector member: {tag}"))
            })?;
        self.selected.store(index, Ordering::Relaxed);
        Ok(self.state_at(index))
    }
}

#[async_trait::async_trait]
impl Outbound for SelectorOutbound {
    #[tracing::instrument(name = "outbound", skip_all, fields(tag = %self.tag))]
    async fn handle(&self, req: ProxyRequest) -> Result<(), SError> {
        let (tag, outbound) = &self.members[self.selected.load(Ordering::Relaxed)];
        tracing::debug!(outbound = %tag, "dispatching selector session");
        outbound.handle(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnyTcp, TcpSession, UdpSession, UserContext, msgs::socks5::SocksAddr};
    use bytes::Bytes;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    type UdpPeer = (
        mpsc::Sender<(Bytes, SocksAddr)>,
        mpsc::Receiver<(Bytes, SocksAddr)>,
    );

    struct EchoOutbound(&'static [u8]);

    #[async_trait::async_trait]
    impl Outbound for EchoOutbound {
        async fn handle(&self, req: ProxyRequest) -> Result<(), SError> {
            let marker = self.0;
            tokio::spawn(async move {
                match req {
                    ProxyRequest::Tcp(mut session) => {
                        while session.stream.read_exact(&mut [0]).await.is_ok() {
                            if session.stream.write_all(marker).await.is_err() {
                                break;
                            }
                        }
                    }
                    ProxyRequest::Udp(mut session) => {
                        while let Ok((_, dst)) = session.recv.recv_from().await {
                            if session
                                .send
                                .send_to(Bytes::from_static(marker), dst)
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
            Ok(())
        }
    }

    // The selector sees two members whose responses identify the chosen member.
    fn selector() -> SelectorOutbound {
        SelectorOutbound::new(
            SelectorOutCfg {
                tag: "choice".into(),
                outbounds: vec!["a".into(), "b".into()],
                selected: Some("b".into()),
            },
            &HashMap::from([
                (
                    "a".into(),
                    Arc::new(EchoOutbound(b"a")) as Arc<dyn Outbound>,
                ),
                (
                    "b".into(),
                    Arc::new(EchoOutbound(b"b")) as Arc<dyn Outbound>,
                ),
            ]),
        )
        .unwrap()
    }

    fn dst() -> SocksAddr {
        "127.0.0.1:80".parse::<SocketAddr>().unwrap().into()
    }

    async fn tcp_session(selector: &SelectorOutbound) -> tokio::net::TcpStream {
        // Real local streams avoid introducing a production TcpTrait implementation.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        let stream: AnyTcp = Box::new(peer);
        selector
            .handle(ProxyRequest::Tcp(TcpSession {
                stream,
                dst: dst(),
                src_addr: None,
                user_context: UserContext::default(),
            }))
            .await
            .unwrap();
        client
    }

    async fn tcp_marker(stream: &mut tokio::net::TcpStream) -> u8 {
        stream.write_all(b"?").await.unwrap();
        let mut marker = [0];
        stream.read_exact(&mut marker).await.unwrap();
        marker[0]
    }

    #[tokio::test]
    async fn switching_only_changes_new_tcp_sessions() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let selector = selector();
            let mut original = tcp_session(&selector).await;
            assert_eq!(tcp_marker(&mut original).await, b'b');
            assert!(selector.select("missing").is_err());
            assert_eq!(selector.state().selected, "b");
            selector.select("a").unwrap();
            assert_eq!(tcp_marker(&mut original).await, b'b');
            let mut next = tcp_session(&selector).await;
            assert_eq!(tcp_marker(&mut next).await, b'a');
        })
        .await
        .unwrap();
    }

    async fn udp_session(selector: &SelectorOutbound) -> UdpPeer {
        let (sender, recv) = mpsc::channel(2);
        let (send, receiver) = mpsc::channel(2);
        selector
            .handle(ProxyRequest::Udp(UdpSession {
                recv: Box::new(recv),
                send: Arc::new(send),
                stream: None,
                bind_addr: dst(),
                dst: dst(),
                src_addr: None,
                user_context: UserContext::default(),
            }))
            .await
            .unwrap();
        (sender, receiver)
    }

    async fn udp_marker(session: &mut UdpPeer) -> Bytes {
        session
            .0
            .send((Bytes::from_static(b"?"), dst()))
            .await
            .unwrap();
        session.1.recv().await.unwrap().0
    }

    #[tokio::test]
    async fn switching_only_changes_new_udp_associations() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let selector = selector();
            let mut original = udp_session(&selector).await;
            assert_eq!(udp_marker(&mut original).await, "b");
            selector.select("a").unwrap();
            assert_eq!(udp_marker(&mut original).await, "b");
            let mut next = udp_session(&selector).await;
            assert_eq!(udp_marker(&mut next).await, "a");
        })
        .await
        .unwrap();
    }
}
