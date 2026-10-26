use std::{
    collections::{HashMap, VecDeque},
    future::poll_fn,
    hash::{BuildHasher, RandomState},
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::tcp,
    time::Instant,
    wire::{
        HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpProtocol, TcpControl, TcpPacket, TcpRepr,
        UdpPacket, UdpRepr,
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    sync::mpsc,
    time::Instant as TokioInstant,
};

use super::{
    TunServerCfg,
    packet::{Packet, udp_packet},
};
use crate::{
    ProxyRequest, TcpSession, TcpTrait, UdpSend, UdpSession, UserContext, error::SError,
    msgs::socks5::SocksAddr,
};

pub(super) const QUEUE_CAPACITY: usize = 64;
const TCP_BUFFER_SIZE: usize = 5000;

#[derive(Default)]
pub(super) struct PacketDevice {
    pub input: VecDeque<Bytes>,
    pub output: VecDeque<Vec<u8>>,
    pub mtu: usize,
}

pub(super) struct Receive(Bytes);
pub(super) struct Transmit<'a>(&'a mut VecDeque<Vec<u8>>);
impl RxToken for Receive {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl TxToken for Transmit<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut packet = vec![0; len];
        let result = f(&mut packet);
        self.0.push_back(packet);
        result
    }
}
impl Device for PacketDevice {
    type RxToken<'a> = Receive;
    type TxToken<'a> = Transmit<'a>;
    fn receive(&mut self, _: Instant) -> Option<(Receive, Transmit<'_>)> {
        if self.output.len() >= QUEUE_CAPACITY {
            return None;
        }
        self.input
            .pop_front()
            .map(|packet| (Receive(packet), Transmit(&mut self.output)))
    }
    fn transmit(&mut self, _: Instant) -> Option<Transmit<'_>> {
        (self.output.len() < QUEUE_CAPACITY).then_some(Transmit(&mut self.output))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps.max_burst_size = Some(64);
        caps
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FlowKey {
    client: SocketAddr,
    destination: SocketAddr,
}

struct TcpFlow {
    handle: SocketHandle,
    bridge: DuplexStream,
    request: Option<ProxyRequest>,
    app_eof: bool,
    network_eof: bool,
}
struct TunStream(DuplexStream);
impl TcpTrait for TunStream {}
impl AsyncRead for TunStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buffer)
    }
}
impl AsyncWrite for TunStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buffer)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

struct UdpFlow {
    packets: mpsc::Sender<(Bytes, SocksAddr)>,
    generation: u64,
    last_active: TokioInstant,
}
struct Reply {
    client: SocketAddr,
    generation: u64,
    packet: Vec<u8>,
}
struct UdpReplySender {
    client: SocketAddr,
    generation: u64,
    mtu: u16,
    replies: mpsc::Sender<Reply>,
}
#[async_trait]
impl UdpSend for UdpReplySender {
    async fn send_to(&self, bytes: Bytes, source: SocksAddr) -> Result<usize, SError> {
        let source: SocketAddr = match source.addr {
            crate::msgs::socks5::AddrOrDomain::V4(ip) => SocketAddr::new(ip.into(), source.port),
            crate::msgs::socks5::AddrOrDomain::V6(ip) => SocketAddr::new(ip.into(), source.port),
            _ => {
                return Err(SError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "TUN reply source must be an IP address",
                )));
            }
        };
        let packet = udp_packet(source, self.client, &bytes, self.mtu)?;
        self.replies
            .send(Reply {
                client: self.client,
                generation: self.generation,
                packet,
            })
            .await
            .map_err(|_| SError::InboundUnavailable)?;
        Ok(bytes.len())
    }
}

pub(super) struct Stack {
    iface: Interface,
    pub device: PacketDevice,
    sockets: SocketSet<'static>,
    tcp: HashMap<FlowKey, TcpFlow>,
    udp: HashMap<SocketAddr, UdpFlow>,
    cfg: TunServerCfg,
    requests: mpsc::Sender<ProxyRequest>,
    replies: mpsc::Sender<Reply>,
    next_generation: u64,
}

impl Stack {
    fn new(
        cfg: TunServerCfg,
        requests: mpsc::Sender<ProxyRequest>,
        replies: mpsc::Sender<Reply>,
    ) -> Self {
        let mut device = PacketDevice {
            mtu: cfg.mtu as usize,
            ..Default::default()
        };
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_seed = RandomState::new().hash_one(&cfg.tag);
        let mut iface = Interface::new(config, &mut device, Instant::ZERO);
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(cfg.ipv4.addr().into(), cfg.ipv4.prefix_len()))
                .unwrap();
            if let Some(ipv6) = cfg.ipv6 {
                addrs
                    .push(IpCidr::new(ipv6.addr().into(), ipv6.prefix_len()))
                    .unwrap();
            }
        });
        // AnyIP accepts all routed destination addresses, not only the TUN's
        // local addresses. These routes exist only in the userspace stack.
        iface
            .routes_mut()
            .add_default_ipv4_route(cfg.ipv4.addr())
            .unwrap();
        if let Some(ipv6) = cfg.ipv6 {
            iface
                .routes_mut()
                .add_default_ipv6_route(ipv6.addr())
                .unwrap();
        }
        iface.set_any_ip(true);
        Self {
            iface,
            device,
            sockets: SocketSet::new(vec![]),
            tcp: HashMap::new(),
            udp: HashMap::new(),
            cfg,
            requests,
            replies,
            next_generation: 0,
        }
    }

    fn context(&self) -> UserContext {
        UserContext {
            inbound_tag: self.cfg.tag.clone(),
            preferred_outbound: self.cfg.default_outbound.clone(),
            ..Default::default()
        }
    }

    fn ingest(&mut self, bytes: Bytes) {
        if bytes.len() > usize::from(self.cfg.mtu) {
            return;
        }
        let Some(packet) = Packet::parse(&bytes) else {
            return;
        };
        if self.cfg.ipv6.is_none() && matches!(packet.src, IpAddress::Ipv6(_)) {
            return;
        }
        match packet.protocol {
            IpProtocol::Tcp => {
                let Ok(tcp_packet) = TcpPacket::new_checked(packet.payload) else {
                    return;
                };
                let Ok(repr) =
                    TcpRepr::parse(&tcp_packet, &packet.src, &packet.dst, &Default::default())
                else {
                    return;
                };
                let key = FlowKey {
                    client: SocketAddr::new(packet.src.into(), repr.src_port),
                    destination: SocketAddr::new(packet.dst.into(), repr.dst_port),
                };
                if repr.control == TcpControl::Syn
                    && repr.ack_number.is_none()
                    && !self.tcp.contains_key(&key)
                {
                    if self.tcp.len() + self.udp.len() >= self.cfg.max_sessions {
                        return;
                    }
                    let mut socket = tcp::Socket::new(
                        tcp::SocketBuffer::new(vec![0; TCP_BUFFER_SIZE]),
                        tcp::SocketBuffer::new(vec![0; TCP_BUFFER_SIZE]),
                    );
                    let timeout = self.cfg.tcp_idle_timeout();
                    socket.set_timeout(
                        (timeout != 0).then(|| smoltcp::time::Duration::from_secs(timeout)),
                    );
                    if socket
                        .listen(IpEndpoint::new(packet.dst, repr.dst_port))
                        .is_err()
                    {
                        return;
                    }
                    let handle = self.sockets.add(socket);
                    let (app, bridge) = tokio::io::duplex(TCP_BUFFER_SIZE);
                    let request: ProxyRequest = ProxyRequest::Tcp(TcpSession {
                        stream: Box::new(TunStream(app)),
                        dst: key.destination.into(),
                        src_addr: Some(key.client),
                        user_context: self.context(),
                    });
                    self.tcp.insert(
                        key,
                        TcpFlow {
                            handle,
                            bridge,
                            request: Some(request),
                            app_eof: false,
                            network_eof: false,
                        },
                    );
                }
                self.device.input.push_back(bytes);
            }
            IpProtocol::Udp => {
                let Ok(udp) = UdpPacket::new_checked(packet.payload) else {
                    return;
                };
                let Ok(repr) = UdpRepr::parse(&udp, &packet.src, &packet.dst, &Default::default())
                else {
                    return;
                };
                let client = SocketAddr::new(packet.src.into(), repr.src_port);
                let destination = SocketAddr::new(packet.dst.into(), repr.dst_port);
                let now = TokioInstant::now();
                if self
                    .udp
                    .get(&client)
                    .is_some_and(|flow| flow.packets.is_closed())
                {
                    self.udp.remove(&client);
                }
                if !self.udp.contains_key(&client) {
                    if self.tcp.len() + self.udp.len() >= self.cfg.max_sessions {
                        return;
                    }
                    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
                    self.next_generation = self.next_generation.wrapping_add(1);
                    let send = std::sync::Arc::new(UdpReplySender {
                        client,
                        generation: self.next_generation,
                        mtu: self.cfg.mtu,
                        replies: self.replies.clone(),
                    });
                    let request: ProxyRequest = ProxyRequest::Udp(UdpSession {
                        recv: Box::new(rx),
                        send,
                        stream: None,
                        bind_addr: SocketAddr::new(
                            if destination.is_ipv4() {
                                Ipv4Addr::UNSPECIFIED.into()
                            } else {
                                Ipv6Addr::UNSPECIFIED.into()
                            },
                            0,
                        )
                        .into(),
                        dst: destination.into(),
                        src_addr: Some(client),
                        user_context: self.context(),
                    });
                    if self.requests.try_send(request).is_err() {
                        return;
                    }
                    tracing::info!(src = %client, dst = %destination, "accepted TUN UDP session");
                    self.udp.insert(
                        client,
                        UdpFlow {
                            packets: tx,
                            generation: self.next_generation,
                            last_active: now,
                        },
                    );
                }
                let flow = self.udp.get_mut(&client).unwrap();
                flow.last_active = now;
                // Congested UDP sessions drop packets without stalling TCP or
                // other clients. Destination is retained on every datagram.
                let _ = flow
                    .packets
                    .try_send((Bytes::copy_from_slice(udp.payload()), destination.into()));
            }
            _ => {}
        }
    }

    fn poll(&mut self, now: Instant) {
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        let timeout = Duration::from_secs(self.cfg.udp_idle_timeout());
        self.udp
            .retain(|_, flow| !flow.packets.is_closed() && flow.last_active.elapsed() < timeout);
        self.tcp.retain(|_, flow| {
            if !self.sockets.get::<tcp::Socket>(flow.handle).is_open() {
                self.sockets.remove(flow.handle);
                false
            } else {
                true
            }
        });
    }

    fn bridges(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let mut progress = false;
        for (key, flow) in &mut self.tcp {
            let socket = self.sockets.get_mut::<tcp::Socket>(flow.handle);
            if matches!(
                socket.state(),
                tcp::State::Established | tcp::State::CloseWait
            ) && let Some(request) = flow.request.take()
            {
                if self.requests.try_send(request).is_err() {
                    socket.abort();
                } else {
                    tracing::info!(
                        src = %key.client,
                        dst = %key.destination,
                        "accepted TUN TCP session"
                    );
                }
                progress = true;
            }
            if flow.request.is_some() {
                continue;
            }
            if socket.can_recv() {
                let result =
                    socket.recv(
                        |buffer| match Pin::new(&mut flow.bridge).poll_write(cx, buffer) {
                            Poll::Ready(Ok(n)) => (n, Ok(n)),
                            Poll::Ready(Err(error)) => (0, Err(error)),
                            Poll::Pending => (0, Ok(0)),
                        },
                    );
                match result {
                    Ok(Ok(n)) => progress |= n > 0,
                    Ok(Err(_)) => {
                        socket.abort();
                        progress = true;
                    }
                    Err(_) => {}
                }
            }
            if socket.can_send() && !flow.app_eof {
                let result = socket.send(|buffer| {
                    let mut buffer = ReadBuf::new(buffer);
                    match Pin::new(&mut flow.bridge).poll_read(cx, &mut buffer) {
                        Poll::Ready(Ok(())) => {
                            let n = buffer.filled().len();
                            (n, Some(Ok(n)))
                        }
                        Poll::Ready(Err(error)) => (0, Some(Err(error))),
                        Poll::Pending => (0, None),
                    }
                });
                match result {
                    Ok(Some(Ok(0))) => {
                        flow.app_eof = true;
                        socket.close();
                        progress = true;
                    }
                    Ok(Some(Ok(_))) => progress = true,
                    Ok(Some(Err(_))) => {
                        socket.abort();
                        progress = true;
                    }
                    _ => {}
                }
            }
            if !socket.may_recv()
                && !socket.can_recv()
                && !flow.network_eof
                && Pin::new(&mut flow.bridge).poll_shutdown(cx).is_ready()
            {
                flow.network_eof = true;
                progress = true;
            }
        }
        if progress {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

pub(super) async fn run(
    cfg: TunServerCfg,
    mut input: mpsc::Receiver<Bytes>,
    output: mpsc::Sender<Vec<u8>>,
    requests: mpsc::Sender<ProxyRequest>,
) -> Result<(), SError> {
    let (reply_tx, mut replies) = mpsc::channel(QUEUE_CAPACITY);
    let mut stack = Stack::new(cfg, requests, reply_tx);
    let epoch = TokioInstant::now();
    loop {
        let now = Instant::from_millis(epoch.elapsed().as_millis() as i64);
        stack.poll(now);
        while let Some(packet) = stack.device.output.pop_front() {
            output
                .send(packet)
                .await
                .map_err(|_| SError::InboundUnavailable)?;
        }
        let delay = stack
            .iface
            .poll_delay(now, &stack.sockets)
            .map_or(Duration::from_secs(1), |delay| {
                Duration::from_millis(delay.total_millis())
            })
            .min(Duration::from_secs(1));
        tokio::select! {
            packet = input.recv() => {
                let Some(packet) = packet else { return Ok(()); };
                stack.ingest(packet);
            }
            Some(reply) = replies.recv() => {
                if let Some(flow) = stack.udp.get_mut(&reply.client)
                    && flow.generation == reply.generation && !flow.packets.is_closed() {
                        flow.last_active = TokioInstant::now();
                        stack.device.output.push_back(reply.packet);
                    }
            }
            _ = poll_fn(|cx| stack.bridges(cx)) => {},
            _ = tokio::time::sleep(delay) => {},
        }
    }
}
