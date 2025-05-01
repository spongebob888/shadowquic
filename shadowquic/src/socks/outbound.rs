use std::{net::ToSocketAddrs, sync::Arc};

use crate::{
    TcpSession, UdpRecv, UdpSend, UdpSession,
    msgs::socks5::{
        CmdReply, SOCKS5_CMD_TCP_CONNECT, SOCKS5_CMD_UDP_ASSOCIATE, SOCKS5_RESERVE, SOCKS5_VERSION,
    },
    socks::UdpSocksWrap,
};
use tokio::{
    io::{AsyncReadExt, copy_bidirectional_with_sizes},
    net::{TcpStream, UdpSocket},
    sync::OnceCell,
};

use async_trait::async_trait;
use tracing::{Instrument, error, trace, trace_span};

use crate::{
    Outbound, ProxyRequest,
    config::SocksClientCfg,
    error::SError,
    msgs::socks5::{AuthReply, AuthReq, CmdReq, SDecode, SEncode, SOCKS5_AUTH_METHOD_NONE, VarVec},
};

#[derive(Debug)]
pub struct SocksClient {
    pub addr: String,
}

impl SocksClient {
    pub fn new(cfg: SocksClientCfg) -> Self {
        Self { addr: cfg.addr }
    }
}

#[async_trait]
impl Outbound for SocksClient {
    async fn handle(&mut self, req: ProxyRequest) -> Result<(), SError> {
        let span = trace_span!("socks", server = self.addr);
        let addr = self.addr.clone();
        let fut = async move {
            match req {
                ProxyRequest::Tcp(tcp_session) => handle_tcp(tcp_session, addr).await,
                ProxyRequest::Udp(udp_session) => handle_udp(udp_session, addr).await,
            }
        };

        tokio::spawn(
            async {
                fut.await
                    .map_err(|x| error!("error due to handle socks request:{}", x))
            }
            .instrument(span),
        );
        Ok(())
    }
}

async fn handle_tcp(mut tcp_session: TcpSession, socks_server: String) -> Result<(), SError> {
    let mut tcp = TcpStream::connect(socks_server).await?;

    let auth = AuthReq {
        version: SOCKS5_VERSION,
        methods: VarVec {
            len: 1,
            contents: vec![SOCKS5_AUTH_METHOD_NONE],
        },
    };
    trace!("sending socks tcp request with dst:{}", tcp_session.dst);
    auth.encode(&mut tcp).await?;
    let rep = AuthReply::decode(&mut tcp).await?;
    assert!(rep.version == SOCKS5_VERSION, "socks version not supported");
    assert!(
        rep.method == SOCKS5_AUTH_METHOD_NONE,
        "socks auth not supported"
    );

    let socksreq = CmdReq {
        version: SOCKS5_VERSION,
        cmd: SOCKS5_CMD_TCP_CONNECT,
        rsv: SOCKS5_RESERVE,
        dst: tcp_session.dst,
    };
    socksreq.encode(&mut tcp).await?;
    let _rep = CmdReply::decode(&mut tcp).await?;

    copy_bidirectional_with_sizes(&mut tcp, &mut tcp_session.stream, 16 * 1024, 16 * 1024).await?;
    Ok(())
}

async fn handle_udp(mut udp_session: UdpSession, socks_server: String) -> Result<(), SError> {
    let mut tcp = TcpStream::connect(socks_server).await?;

    let auth = AuthReq {
        version: SOCKS5_VERSION,
        methods: VarVec {
            len: 1,
            contents: vec![SOCKS5_AUTH_METHOD_NONE],
        },
    };
    trace!("sending socks udp request with dst:{}", udp_session.dst);
    auth.encode(&mut tcp).await?;
    let rep = AuthReply::decode(&mut tcp).await?;
    assert!(rep.version == SOCKS5_VERSION, "socks version not supported");
    assert!(
        rep.method == SOCKS5_AUTH_METHOD_NONE,
        "socks auth not supported"
    );

    let socksreq = CmdReq {
        version: SOCKS5_VERSION,
        cmd: SOCKS5_CMD_UDP_ASSOCIATE,
        rsv: SOCKS5_RESERVE,
        dst: udp_session.dst.clone(),
    };
    socksreq.encode(&mut tcp).await?;
    let rep = CmdReply::decode(&mut tcp).await?;
    let peer_addr = rep
        .bind_addr
        .to_socket_addrs()
        .expect("socks server return a unresolvable address")
        .next()
        .expect("socks server return a unresolvable address");
    let bind_addr = if peer_addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };

    let socket = UdpSocket::bind(bind_addr).await?;
    socket.connect(peer_addr).await?;
    let mut upstream = UdpSocksWrap(Arc::new(socket), OnceCell::new_with(Some(peer_addr)));

    let upstream_clone = upstream.clone();
    let fut1 = async move {
        loop {
            let (buf, dst) = upstream.recv_from().await?;

            let _ = udp_session.send.send_to(buf, dst).await?;
        }
        #[allow(unreachable_code)]
        (Ok(()) as Result<(), SError>)
    };
    let fut2 = async move {
        loop {
            let (buf, dst) = udp_session.recv.recv_from().await?;

            let _ = upstream_clone.send_to(buf, dst).await?;
        }
        #[allow(unreachable_code)]
        (Ok(()) as Result<(), SError>)
    };
    // control stream, in socks5 inbound, end of control stream
    // means end of udp association.
    let fut3 = async {
        if udp_session.stream.is_none() {
            return Ok(());
        }
        let mut buf = [0u8];
        udp_session
            .stream
            .unwrap()
            .read_exact(&mut buf)
            .await
            .map_err(|_| SError::UDPCtrlStreamClosed)?;
        error!("unexpected data received from socks control stream");
        Err(SError::UDPCtrlStreamClosed) as Result<(), SError>
    };
    // We can use spawn, but it requirs communication to shutdown the other
    // Flatten spawn handle using try_join! doesn't work. Don't know why
    tokio::try_join!(fut1, fut2, fut3)?;

    Ok(())
}
