//! TCP/UDP proxying from a layer-3 TUN interface.
mod config;
mod packet;
mod stack;
#[cfg(test)]
mod tests;

use async_trait::async_trait;
use bytes::Bytes;
pub use config::TunServerCfg;
use std::{
    io,
    sync::{Arc, Mutex},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::Instrument;
#[cfg(not(target_os = "android"))]
use tun_rs::DeviceBuilder;

use crate::{Inbound, ProxyRequest, error::SError, observe::Observer};

pub struct TunServer {
    pub name: String,
    /// Shared with the manager's observer map when registering this inbound.
    pub observer: Arc<Observer>,
    requests: mpsc::Receiver<ProxyRequest>,
    task: Mutex<Option<JoinHandle<Result<(), SError>>>>,
}

impl TunServer {
    pub async fn new(cfg: TunServerCfg) -> Result<Self, SError> {
        cfg.validate()?;
        #[cfg(not(target_os = "android"))]
        let (device, name) = {
            let mut builder = DeviceBuilder::new()
                .ipv4(cfg.ipv4.addr(), cfg.ipv4.prefix_len(), None)
                .ipv6(cfg.ipv6.addr(), cfg.ipv6.prefix_len())
                .mtu(cfg.mtu);
            if let Some(name) = &cfg.name {
                builder = builder.name(name);
            }
            // Routes belong to the user; creating the adapter must not redirect the
            // process's own upstream traffic into itself.
            #[cfg(target_os = "macos")]
            {
                builder = builder.associate_route(false);
            }
            let device = builder.build_async()?;
            let name = device.name()?;
            (device, name)
        };
        #[cfg(target_os = "android")]
        let (device, name) = {
            use std::os::fd::IntoRawFd;
            let fd = cfg
                .android_fd
                .as_ref()
                .ok_or_else(|| {
                    SError::InvalidConfig("Android TUN requires a VpnService descriptor".into())
                })?
                .try_clone()?;
            // from_fd owns this independent duplicate, even on setup failure.
            let device = unsafe { tun_rs::AsyncDevice::from_fd(fd.into_raw_fd())? };
            (device, "android-vpn".to_owned())
        };
        let (request_tx, requests) = mpsc::channel(stack::QUEUE_CAPACITY);
        let (input_tx, input_rx) = mpsc::channel(stack::QUEUE_CAPACITY);
        let (output_tx, mut output_rx) = mpsc::channel::<Vec<u8>>(stack::QUEUE_CAPACITY);
        let task = tokio::spawn(
            async move {
                let read = async {
                    let mut buffer = vec![0; 65535];
                    loop {
                        let len = device.recv(&mut buffer).await?;
                        if len == 0 {
                            return Err(SError::Io(io::Error::from(io::ErrorKind::UnexpectedEof)));
                        }
                        input_tx
                            .send(Bytes::copy_from_slice(&buffer[..len]))
                            .await
                            .map_err(|_| SError::InboundUnavailable)?;
                    }
                };
                let write = async {
                    while let Some(packet) = output_rx.recv().await {
                        if device.send(&packet).await? != packet.len() {
                            return Err(SError::Io(io::Error::from(io::ErrorKind::WriteZero)));
                        }
                    }
                    Err(SError::InboundUnavailable)
                };
                let stack = stack::run(cfg, input_rx, output_tx, request_tx);
                let result: Result<((), (), ()), SError> = tokio::try_join!(read, write, stack);
                if let Err(error) = &result {
                    tracing::error!(%error, "TUN packet loop stopped");
                }
                result.map(|_| ())
            }
            .in_current_span(),
        );
        Ok(Self {
            name,
            observer: Arc::new(Observer::new()),
            requests,
            task: Mutex::new(Some(task)),
        })
    }
}

#[async_trait]
impl Inbound for TunServer {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        let mut request = self
            .requests
            .recv()
            .await
            .ok_or(SError::InboundUnavailable)?;
        request.user_context_mut().observer = Arc::downgrade(&self.observer);
        request
            .user_context_mut()
            .stats
            .get_or_insert_with(Default::default);
        Ok(request)
    }

    async fn shutdown(&self) -> Result<(), SError> {
        let task = self.task.lock().unwrap().take();
        if let Some(task) = task {
            task.abort();
            match task.await {
                Ok(result) => result?,
                Err(error) if error.is_cancelled() => {}
                Err(error) => return Err(SError::Io(io::Error::other(error))),
            }
            tracing::info!(name = %self.name, "TUN inbound stopped");
        }
        Ok(())
    }
}

impl Drop for TunServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().unwrap().take() {
            task.abort();
        }
    }
}
