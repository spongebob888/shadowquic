use anyhow::Result;
use async_trait::async_trait;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{io::AsyncReadExt, net::TcpListener};

use crate::{
    Inbound, ProxyRequest,
    config::AuthUser,
    config::MixedServerCfg,
    error::SError,
    http::inbound::{HttpProxyServer, ProxyBasicAuth},
    socks::inbound::SocksServer,
    utils::dual_socket::to_ipv4_mapped,
    utils::replay_stream::ReplayStream,
};

pub struct MixedServer {
    listener: TcpListener,
    http: HttpProxyServer,
    users: Vec<AuthUser>,
}

impl MixedServer {
    pub async fn new(cfg: MixedServerCfg) -> Result<Self, SError> {
        let MixedServerCfg { bind_addr, users } = cfg;

        let dual_stack = bind_addr.is_ipv6();
        let socket = Socket::new(
            if dual_stack {
                Domain::IPV6
            } else {
                Domain::IPV4
            },
            Type::STREAM,
            Some(Protocol::TCP),
        )?;
        if dual_stack {
            let _ = socket
                .set_only_v6(false)
                .map_err(|e| tracing::warn!("failed to set dual stack for socket: {}", e));
        }
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&bind_addr.into())?;
        socket.listen(256)?;

        let listener = TcpListener::from_std(socket.into())
            .map_err(|e| SError::SocksError(format!("failed to create TcpListener: {e}")))?;

        let http_users = users
            .iter()
            .map(|u| ProxyBasicAuth {
                username: u.username.clone(),
                password: u.password.clone(),
            })
            .collect();

        Ok(Self {
            listener,
            http: HttpProxyServer::with_users(http_users),
            users,
        })
    }
}

#[async_trait]
impl Inbound for MixedServer {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        let (mut stream, _addr) = self.listener.accept().await?;
        let local_addr = to_ipv4_mapped(stream.local_addr().unwrap());

        // 只读一个字节，客户端没发数据就自然卡住（和 socks5 一样）
        let first_byte = stream.read_u8().await?;

        if first_byte == 0x05 {
            let prefix = vec![first_byte];
            return SocksServer::accept_stream_with_local_addr(
                ReplayStream::new(prefix, stream),
                local_addr,
                &self.users,
            )
            .await;
        }

        // 否则当 HTTP 处理，把第一个字节也塞回去，让 HTTP 解析器自己慢慢读
        let prefix = vec![first_byte];
        self.http
            .accept_stream(ReplayStream::new(prefix, stream))
            .await
    }
}
