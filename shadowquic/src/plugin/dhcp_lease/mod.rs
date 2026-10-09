//! Local DHCP identities shared by Lua runtime generations.
mod parse;
#[cfg(test)]
mod tests;

use arc_swap::ArcSwap;
use mlua::{Lua, Value};
use notify::{RecursiveMode, Watcher};
use std::{
    collections::{HashMap, HashSet},
    fs,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tracing::{debug, warn};

use crate::{config::DhcpLeaseCfg, error::SError};
use parse::{Snapshot, parse};

struct Source {
    tag: String,
    path: PathBuf,
    odhcp: bool,
    snapshot: ArcSwap<Snapshot>,
}

impl Source {
    fn reload(&self) {
        let result = (|| {
            let before = fs::metadata(&self.path).map_err(|e| e.to_string())?;
            let text = fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
            let after = fs::metadata(&self.path).map_err(|e| e.to_string())?;
            if before.len() != after.len()
                || after.len() != text.len() as u64
                || before.modified().ok() != after.modified().ok()
            {
                return Err("lease file changed during read".into());
            }
            parse(&text, self.odhcp)
        })();
        match result {
            Ok(snapshot) => {
                self.snapshot.store(Arc::new(snapshot));
                debug!(tag = %self.tag, path = %self.path.display(), "DHCP lease file reloaded");
            }
            Err(error) => {
                warn!(tag = %self.tag, path = %self.path.display(), %error, "DHCP lease reload failed; retaining previous snapshot")
            }
        }
    }
}

enum Signal {
    Wake,
    Stop,
}

struct Worker {
    tx: mpsc::SyncSender<Signal>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // The receiver drains events before reloading, including a queued stop.
        let _ = self.tx.send(Signal::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Default)]
pub(crate) struct LeaseStore {
    sources: HashMap<String, Arc<Source>>,
    _worker: Option<Worker>,
}

impl LeaseStore {
    pub(crate) fn build(configs: &[DhcpLeaseCfg]) -> Result<Arc<Self>, SError> {
        if configs.is_empty() {
            return Ok(Arc::default());
        }
        let cwd = std::env::current_dir().map_err(|e| SError::InvalidConfig(e.to_string()))?;
        let mut sources = HashMap::new();
        for config in configs {
            if config.tag().trim().is_empty()
                || config.path().as_os_str().is_empty()
                || sources.contains_key(config.tag())
            {
                return Err(SError::InvalidConfig(
                    "DHCP lease tags must be nonempty and unique, and paths nonempty".into(),
                ));
            }
            let source = Arc::new(Source {
                tag: config.tag().into(),
                path: cwd.join(config.path()),
                odhcp: matches!(config, DhcpLeaseCfg::Odhcp(_)),
                snapshot: ArcSwap::from_pointee(Snapshot::default()),
            });
            source.reload();
            sources.insert(config.tag().into(), source);
        }
        let watched: Vec<_> = sources.values().cloned().collect();
        let (tx, rx) = mpsc::sync_channel(1);
        let events = tx.clone();
        let handle = thread::Builder::new()
            .name("dhcp-leases".into())
            .spawn(move || run_worker(watched, events, rx))
            .map_err(|e| {
                SError::InvalidConfig(format!("failed to start DHCP lease worker: {e}"))
            })?;
        Ok(Arc::new(Self {
            sources,
            _worker: Some(Worker {
                tx,
                handle: Some(handle),
            }),
        }))
    }

    pub(crate) fn install(self: &Arc<Self>, lua: &Lua) -> mlua::Result<()> {
        for (name, v4, attribute) in [
            ("find_dhcp_mac_v4", true, "mac"),
            ("find_dhcp_host_v4", true, "host"),
            ("find_dhcp_duid_v6", false, "duid"),
            ("find_dhcp_iaid_v6", false, "iaid"),
            ("find_dhcp_mac_v6", false, "mac"),
            ("find_dhcp_host_v6", false, "host"),
        ] {
            let store = self.clone();
            lua.globals().set(
                name,
                lua.create_function(move |lua, (tag, ip): (Value, Value)| {
                    let (Value::String(tag), Value::String(ip)) = (tag, ip) else {
                        return Err(mlua::Error::runtime(
                            "DHCP lookup requires string tag and IP",
                        ));
                    };
                    let tag = tag.to_str()?;
                    let source = store.sources.get(tag.as_ref()).ok_or_else(|| {
                        mlua::Error::runtime(format!("unknown DHCP lease tag: {tag}"))
                    })?;
                    let ip: IpAddr = ip.to_str()?.parse().map_err(mlua::Error::external)?;
                    if ip.is_ipv4() != v4 {
                        return Err(mlua::Error::runtime("wrong DHCP lookup address family"));
                    }
                    let snapshot = source.snapshot.load();
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let Some(lease) = snapshot.lookup(ip, now) else {
                        return Ok(Value::Nil);
                    };
                    if attribute == "iaid" {
                        return Ok(lease
                            .iaid
                            .map_or(Value::Nil, |id| Value::Number(f64::from(id))));
                    }
                    let bytes = match attribute {
                        "mac" => lease.mac.as_ref().map(|s| s.as_bytes()),
                        "duid" => lease.duid.as_ref().map(|s| s.as_bytes()),
                        _ => lease.host.as_deref(),
                    };
                    bytes
                        .map(|s| lua.create_string(s).map(Value::String))
                        .unwrap_or(Ok(Value::Nil))
                })?,
            )?;
        }
        Ok(())
    }
}

fn run_worker(
    watched: Vec<Arc<Source>>,
    events: mpsc::SyncSender<Signal>,
    rx: mpsc::Receiver<Signal>,
) {
    loop {
        // Renew watches on reconciliation too: directories may have been
        // removed and recreated, invalidating an old watch silently.
        let callback = events.clone();
        let paths: Vec<_> = watched.iter().map(|s| s.path.clone()).collect();
        let watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event)
                    if !event.kind.is_access()
                        && (event.need_rescan()
                            || event.paths.iter().any(|p| {
                                paths.iter().any(|target| {
                                    p == target || Some(p.as_path()) == target.parent()
                                })
                            })) =>
                {
                    let _ = callback.try_send(Signal::Wake);
                }
                Err(error) => {
                    warn!(%error, "DHCP lease watch failed");
                    let _ = callback.try_send(Signal::Wake);
                }
                _ => {}
            });
        let mut watcher = match watcher {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                warn!(%error, "DHCP lease watcher unavailable; polling");
                None
            }
        };
        if let Some(watcher) = &mut watcher {
            let parents: HashSet<_> = watched.iter().filter_map(|s| s.path.parent()).collect();
            for parent in parents {
                if let Err(error) = watcher.watch(parent, RecursiveMode::NonRecursive) {
                    warn!(path = %parent.display(), %error, "DHCP lease directory watch failed; polling");
                }
            }
        }
        // Initial load happened synchronously. This read closes the watch
        // registration race, and subsequent iterations reconcile all files.
        for source in &watched {
            source.reload();
        }
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Signal::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Signal::Wake) => {
                thread::sleep(Duration::from_millis(50));
                if rx.try_iter().any(|signal| matches!(signal, Signal::Stop)) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
