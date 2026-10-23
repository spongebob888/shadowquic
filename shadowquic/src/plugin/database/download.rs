use std::{sync::Arc, time::Duration};

use super::{Result, Slot, import_database};
use crate::{
    config::RouterDatabaseCfg,
    utils::download::{DownloadInbound, DownloadOptions},
};

pub(super) fn inbound(cfg: RouterDatabaseCfg, slot: Arc<Slot>) -> DownloadInbound {
    DownloadInbound::new(cfg.tag().to_owned(), move |mut client| async move {
        let result: Result<_> = async {
            let mut source = tempfile::NamedTempFile::new()?;
            client
                .download(
                    cfg.url(),
                    &mut source,
                    DownloadOptions {
                        timeout: Duration::from_secs(300),
                        max_size: 512 * 1024 * 1024,
                    },
                )
                .await?;
            let import_cfg = cfg.clone();
            // Always join import before graceful shutdown finishes: blocking
            // work cannot be cancelled once it has started.
            tokio::task::spawn_blocking(move || import_database(&import_cfg, source.path())).await?
        }
        .await;
        match result {
            Ok(db) => {
                *slot.value.write().unwrap() = Ok(db);
                tracing::info!(tag = %cfg.tag(), "router database ready");
            }
            Err(error) => {
                *slot.value.write().unwrap() = Err(error.to_string());
                tracing::error!(tag = %cfg.tag(), %error, "router database download/import failed");
            }
        }
    })
}
