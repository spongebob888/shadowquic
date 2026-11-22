use std::{sync::Arc, time::Duration};

use shadowquic::{
    config::{AuthUser, JlsUpstream, PortHoppingCfg, ShadowQuicClientCfg, ShadowQuicServerCfg},
    dns::ResolverManager,
    quic::QuicServer,
    shadowquic::{EndServer, outbound::ShadowQuicClient},
};
use tokio::time::timeout;

#[tokio::test]
async fn port_hopping_changes_the_udp_source_port_and_preserves_an_open_stream() {
    let user = AuthUser {
        username: "alice".into(),
        password: "test-password".into(),
    };
    let server = EndServer::new(&ShadowQuicServerCfg {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        users: vec![user.clone()],
        jls_upstream: JlsUpstream {
            addr: "127.0.0.1:443".into(),
            ..Default::default()
        },
        alpn: vec!["h3".into()],
        ..Default::default()
    })
    .await
    .unwrap();
    let client = ShadowQuicClient::new(
        ShadowQuicClientCfg {
            addr: server.local_addr().unwrap().to_string(),
            server_name: "localhost".into(),
            username: user.username,
            password: user.password,
            alpn: vec!["h3".into()],
            port_hopping: PortHoppingCfg {
                enable: true,
                interval: 1_000.into(),
                interval_jitter: 0.into(),
            },
            ..Default::default()
        },
        Arc::new(ResolverManager::new()),
    );

    timeout(Duration::from_secs(15), async {
        let (client_conn, server_conn) =
            tokio::join!(client.get_conn(), QuicServer::accept(&server));
        let client_conn = client_conn.unwrap();
        let server_conn = server_conn.unwrap();
        let mut addresses = server_conn.watch_remote_address();
        let mut previous_port = server_conn.remote_address().port();

        let (mut client_send, mut client_recv) = client_conn.open_bi().await.unwrap();
        client_send.write_all(b"a").await.unwrap();
        let (mut server_send, mut server_recv) = server_conn.accept_bi().await.unwrap();
        let mut received = [0];
        server_recv.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"a");

        for (request, response) in [(b"b", b"x"), (b"c", b"y")] {
            let address = timeout(Duration::from_secs(5), addresses.wait())
                .await
                .expect("server did not observe the next UDP source port")
                .expect("connection closed while waiting for a port hop");
            assert_ne!(address.port(), previous_port);
            previous_port = address.port();

            client_send.write_all(request).await.unwrap();
            server_recv.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, request);
            server_send.write_all(response).await.unwrap();
            client_recv.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, response);
        }
    })
    .await
    .expect("timed out exchanging data over the port-hopping connection");
}
