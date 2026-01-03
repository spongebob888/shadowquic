use std::future::poll_fn;
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use std::{io, u8};

use async_trait::async_trait;
use bytes::Bytes;
use gm_quic::prelude::handy::{client_parameters, server_parameters};

use gm_quic::prelude::handy::ToPrivateKey;
use gm_quic::prelude::StreamWriter;
use gm_quic::prelude::{StreamReader};

use rustls::{crypto, RootCertStore};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use thiserror::Error;

// Add import for DefaultSeqLogger
//use qevent::telemetry::DefaultSeqLogger;

use crate::quic::QuicConnection;
use crate::quic::{QuicClient, QuicServer};

pub use gm_quic::prelude::QuicClient as EndClient;
pub type EndServer = Arc<gm_quic::prelude::QuicListeners>;

/// Right now(202506), gm-quic doesn't provide BBR support. 
/// So we stopped here.
//#[deprecated(note = "Use quinn instead")]
#[derive(Clone)]
pub struct Connection {
    inner: Arc<gm_quic::prelude::Connection>,
    datagram_reader: gm_quic::prelude::DatagramReader,
    datagram_writer: gm_quic::prelude::DatagramWriter,
}

#[async_trait]
impl QuicClient for gm_quic::prelude::QuicClient {
    type C = Connection;

    async fn new(
        cfg: &crate::config::ShadowQuicClientCfg,
        ipv6: bool,
    ) -> crate::error::SResult<Self> {
        let roots = RootCertStore::empty();
        let mut cli_para = client_parameters();
        cli_para
            .set(gm_quic::prelude::ParameterId::MaxDatagramFrameSize, 2000)
            .unwrap();

        let mut client = gm_quic::prelude::QuicClient::builder()
            .with_root_certificates(roots)
            .without_cert()
            .with_parameters(cli_para)

            ;
        if cfg.zero_rtt {
            client = client.enable_0rtt();
        }
        Ok(client.build())
    }

    fn new_with_socket(
        cfg: &crate::config::ShadowQuicClientCfg,
        socket: std::net::UdpSocket,
    ) -> crate::error::SResult<Self> {
        unimplemented!()
    }

    async fn connect(
        &self,
        addr: std::net::SocketAddr,
        server_name: &str,
    ) -> Result<Self::C, QuicErrorRepr> {
        let conn = self.connected_to(server_name, [addr]).unwrap();
        Ok(Connection {
            datagram_reader: conn.unreliable_reader()??,
            datagram_writer: conn.unreliable_writer().await??,
            inner: conn.into(),
        })
    }
}

#[async_trait]
impl QuicConnection for Connection {
    type SendStream = StreamWriter;
    type RecvStream = StreamReader;
    async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr> {
        let (id, (r, w)) = self.inner.open_bi_stream().await?.unwrap();
        Ok((w, r, id.id()))
    }
    async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr> {
        let (id, (r, w)) = self.inner.accept_bi_stream().await?;
        Ok((w, r, id.id()))
    }
    async fn open_uni(&self) -> Result<(Self::SendStream, u64), QuicErrorRepr> {
        let (id, w) = self.inner.open_uni_stream().await?.unwrap();
        Ok((w, id.id()))
    }
    async fn accept_uni(&self) -> Result<(Self::RecvStream, u64), QuicErrorRepr> {
        let (id, r) = self.inner.accept_uni_stream().await?;
        Ok((r, id.id()))
    }
    async fn read_datagram(&self) -> Result<Bytes, QuicErrorRepr> {
        let bytes = poll_fn(|cx| self.datagram_reader.poll_recv(cx)).await?;
        Ok(bytes)
    }
    async fn send_datagram(&self, bytes: Bytes) -> Result<(), QuicErrorRepr> {
        self.datagram_writer.send_bytes(bytes)?;
        Ok(())
    }
    fn close_reason(&self) -> Option<QuicErrorRepr> {
        // match self.deref().is_active() {
        //     true => None,
        //     false => Some(QuicErrorRepr::QuicError(qbase::error::Error::new(
        //         "Connection closed",
        //     ))),
        // }
        None
    }
    fn remote_address(&self) -> SocketAddr {
        "0.0.0.0:0".parse().unwrap()
    }
    fn peer_id(&self) -> u64 {
        let mut id: [u8; 8] = [0; 8];
        id.copy_from_slice(self.inner.origin_dcid().unwrap().as_ref());
        u64::from_be_bytes(id)
    }
}

#[async_trait]
impl QuicServer for EndServer {
    type C = Connection;

    async fn new(cfg: &crate::config::ShadowQuicServerCfg) -> crate::error::SResult<Self> {
        // let qlogger: Arc<dyn qevent::telemetry::Log + Send + Sync> =
        //     Arc::new(DefaultSeqLogger::new(PathBuf::from("./server.qlog")));
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let priv_key = cert.signing_key.serialize_der();
        let mut crypto = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], PrivateKeyDer::Pkcs8(priv_key.clone().into()))?;
        // crypto.alpn_protocols = cfg
        //     .alpn
        //     .iter()
        //     .cloned()
        //     .map(|alpn| alpn.into_bytes())
        //     .collect();
        // crypto.max_early_data_size = if cfg.zero_rtt { u32::MAX } else { 0 };
        // crypto.send_half_rtt_data = cfg.zero_rtt;

        let mut jls_config = rustls::JlsServerConfig::default();
        for user in &cfg.users {
            jls_config = jls_config.add_user(user.password.clone(), user.username.clone());
            jls_config.inner.push(rustls::JlsConfig::default());
        }
        if let Some(sni) = &cfg.server_name {
            jls_config = jls_config.with_server_name(sni.clone());
        }
        jls_config = jls_config.with_rate_limit(cfg.jls_upstream.rate_limit);
        jls_config = jls_config.with_upstream_addr(cfg.jls_upstream.addr.clone());
        crypto.jls_config = jls_config;

        let mut server_para = server_parameters();
        server_para
            .set(gm_quic::prelude::ParameterId::MaxDatagramFrameSize, 2000)
            .unwrap();

        let listeners = gm_quic::prelude::QuicListeners::builder_with_tls(crypto).unwrap()
            .with_parameters(server_para)
            .enable_0rtt()
            .listen(128);
        listeners.add_server(
            "localhost",
            cert_der.as_ref(),
            priv_key.to_private_key(),
            [cfg.bind_addr],
            None,
        ).unwrap();
        Ok(listeners)
    }

    async fn accept(&self) -> Result<Self::C, QuicErrorRepr> {
        let (conn, sni, path, link) = self.deref().accept().await.unwrap();
        Ok(Connection {
            datagram_reader: conn.unreliable_reader()??,
            datagram_writer: conn.unreliable_writer().await??,
            inner: conn.into(),
        })
    }
}

#[derive(Error, Debug)]
#[error(transparent)]
pub enum QuicErrorRepr {
    #[error("QUIC IO Error:{0}")]
    QuicIoError(#[from] io::Error),
    #[error("QUIC Error:{0}")]
    QuicError(#[from] qbase::error::Error),
    #[error("Failed to build Quic Listener:{0}")]
    QuicListenerBuilderError(#[from] gm_quic::prelude::BuildListenersError),
    #[error("JLS Authentication failed")]
    JlsAuthFailed,
}
