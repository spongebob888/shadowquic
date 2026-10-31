use std::{net::SocketAddr, sync::Arc};

use shadowquic::{
    Outbound, config::DirectOutCfg, direct::outbound::DirectOut, dns::ResolverManager,
    error::SError, socks::inbound::SocksServer,
};
use tokio::{net::TcpListener, task::JoinHandle};

// Accept one SOCKS connection on an OS-assigned loopback port. Binding before
// spawning avoids startup sleeps and collisions with the other benchmarks.
pub async fn direct_proxy() -> std::io::Result<(SocketAddr, JoinHandle<Result<(), SError>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?;
        let mut request = SocksServer::accept_stream_with_local_addr(stream, addr, &[]).await?;
        request.user_context_mut().inbound_tag = "inbound".into();
        let direct = DirectOut::new(
            DirectOutCfg {
                tag: "outbound".into(),
                ..Default::default()
            },
            Arc::new(ResolverManager::new()),
        );
        direct.handle(request).await
    });
    Ok((addr, task))
}
