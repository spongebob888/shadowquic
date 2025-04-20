use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use fast_socks5::client::{Config, Socks5Datagram, Socks5Stream};
use fast_socks5::util::target_addr::TargetAddr;

use shadowquic::config::{
    CongestionControl, ShadowQuicClientCfg, ShadowQuicServerCfg, SocksServerCfg,
};
use shadowquic::socks;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::select;
use tokio::time::Duration;

use shadowquic::{
    Manager,
    direct::outbound::DirectOut,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::inbound::SocksServer,
};

use tracing::info;
use tracing::{Level, level_filters::LevelFilter, trace};

#[tokio::main]
async fn main() {
    let socks_server:String = "127.0.0.1:1089".into();
    let target_addr = ("127.0.0.1", 1445);
    let mut config = Config::default();
    config.set_skip_auth(false);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let backing_socket = TcpStream::connect(socks_server.clone()).await.unwrap();
    let socks = Socks5Datagram::bind(
        backing_socket,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
    )
    .await
    .unwrap();


    let fut1= forward_tcp(socks_server, 1080,5201);
    let fut2 = forward_udp(socks, 1080, 5201);
    tokio::join!(fut1, fut2);
}

async fn forward_udp(socks_out: Socks5Datagram<TcpStream>, local_port: u16, target_port: u16) {
    let socks = Arc::new(UdpSocket::bind(("0.0.0.0", local_port)).await.unwrap());

    const CHUNK_LEN: usize = 2000;
    let mut sendbuf = vec![0u8; CHUNK_LEN];
    let mut recvbuf = vec![0u8; CHUNK_LEN];
    // let mut s1:TcpStream = s.get_socket();

    let socks1 = socks.clone();
    loop{
        eprintln!("accepting incoming");
        // let (r, addr) = socks_out.recv_from(&mut sendbuf).await.unwrap();
        // let addr = addr.to_socket_addrs().unwrap().next().unwrap();
        // socks.send_to(&mut sendbuf, addr).await.unwrap();
        let (u, addr) = socks1.recv_from(&mut recvbuf).await.unwrap();
        socks_out.send_to(&mut recvbuf,("127.0.0.1",target_port)).await.unwrap();
    
        loop {
            tokio::select! {
                r = socks1.recv_from(&mut recvbuf) => {
                    r.unwrap();
                    socks_out.send_to(&mut recvbuf,("127.0.0.1",target_port)).await.unwrap();
                }

                r = socks_out.recv_from(&mut sendbuf) => {
                    r.unwrap();
                    socks.send_to(&mut sendbuf, addr).await.unwrap();
                }
            }
        }
    }
}

async fn forward_tcp(socks_server: String,local_port: u16,target_port: u16) {

    let lis = TcpListener::bind(("127.0.0.1", local_port)).await.unwrap();
    loop {
    let (mut up,_) = lis.accept().await.unwrap();
    let socks_server = socks_server.clone();
    tokio::spawn(async move {
    let mut config = Config::default();
    config.set_skip_auth(false);

    let mut s = Socks5Stream::connect(socks_server.clone(), "127.0.0.1".into(), target_port, config)
    .await
    .unwrap();
    eprintln!("stream accpeted");
    tokio::io::copy_bidirectional(&mut s, &mut up).await.unwrap();
    });
    }
}
