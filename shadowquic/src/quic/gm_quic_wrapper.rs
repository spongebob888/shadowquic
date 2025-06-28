use std::future::poll_fn;
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::{io, u8};

use async_trait::async_trait;
use bytes::Bytes;
use gm_quic::handy::{client_parameters, server_parameters};

use gm_quic::StreamWriter;
use gm_quic::{StreamReader, ToCertificate, ToPrivateKey};
use qevent::telemetry::handy::{DefaultSeqLogger, NoopLogger};

use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use thiserror::Error;
use tokio::sync::OnceCell;

use crate::quic::QuicConnection;
use crate::quic::{QuicClient, QuicServer};

pub use gm_quic::QuicClient as EndClient;
pub type EndServer = Arc<gm_quic::QuicListeners>;

#[derive(Clone)]
pub struct Connection {
    inner: Arc<gm_quic::Connection>,
    datagram_reader: gm_quic::DatagramReader,
    datagram_writer: gm_quic::DatagramWriter,
}

#[async_trait]
impl QuicClient for gm_quic::QuicClient {
    type C = Connection;

    async fn new(
        cfg: &crate::config::ShadowQuicClientCfg,
        ipv6: bool,
    ) -> crate::error::SResult<Self> {
        let mut roots = RootCertStore::empty();
        let mut cli_para = client_parameters();
        cli_para
            .set(gm_quic::ParameterId::MaxDatagramFrameSize, 2000)
            .unwrap();

        let mut client = gm_quic::QuicClient::builder()
            .with_root_certificates(roots)
            .without_cert()
            .with_parameters(cli_para)
            .reuse_connection();
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
        zero_rtt: bool,
    ) -> Result<Self::C, QuicErrorRepr> {
        let conn = self.connect(server_name, addr)?;
        Ok(Connection {
            datagram_reader: conn.unreliable_reader()??,
            datagram_writer: conn.unreliable_writer().await??,
            inner: conn,
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
        let qlogger: Arc<dyn qevent::telemetry::Log + Send + Sync> =
            Arc::new(DefaultSeqLogger::new(PathBuf::from("./server.qlog")));

        let mut server_para = server_parameters();
        server_para
            .set(gm_quic::ParameterId::MaxDatagramFrameSize, 2000)
            .unwrap();

        let listeners = gm_quic::QuicListeners::builder()?
            .without_client_cert_verifier()
            .with_parameters(server_para)
            .enable_0rtt()
            .listen(128)
            .await;
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let priv_key = cert.key_pair.serialize_der();
        listeners.add_server(
            "localhost",
            cert_der.as_ref(),
            priv_key.to_private_key(),
            [cfg.bind_addr],
            None,
        )?;
        Ok(listeners)
    }

    async fn accept(&self, zero_rtt: bool) -> Result<Self::C, QuicErrorRepr> {
        let (conn, sni, path, link) = self.deref().accept().await?;
        Ok(Connection {
            datagram_reader: conn.unreliable_reader()??,
            datagram_writer: conn.unreliable_writer().await??,
            inner: conn,
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
    #[error("JLS Authentication failed")]
    JlsAuthFailed,
}
