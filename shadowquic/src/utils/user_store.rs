use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{config::AuthUser, error::SError, msgs::squic::UserStats, observe::Observer};

/// A single user entry persisted to disk: credentials + accumulated traffic.
/// Connection counters (`tcp_conns`/`udp_conns`) are ephemeral runtime state
/// and are intentionally not persisted.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct PersistedUser {
    pub username: String,
    pub password: String,
    pub tcp_sent: u64,
    pub tcp_recv: u64,
    pub udp_sent: u64,
    pub udp_recv: u64,
}

/// Root of the user store YAML file.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct UserStore {
    pub users: Vec<PersistedUser>,
}

/// Load the store from disk. Returns `Ok(None)` if the file does not exist.
///
/// This is the startup entry point, so it also reaps tmp files orphaned by a
/// crashed previous process — the one safe moment to do so, before this
/// process creates any tmp file of its own.
pub fn load_store(path: &Path) -> Result<Option<UserStore>, SError> {
    clean_stale_tmp(path);
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)?;
    let store: UserStore = serde_saphyr::from_str(&content).map_err(|e| {
        SError::Io(std::io::Error::other(format!(
            "failed to parse user store {}: {e}",
            path.display()
        )))
    })?;
    Ok(Some(store))
}

/// Counter making tmp file names unique within this process. Pointer-width
/// on purpose: 64-bit atomics are not available on every supported target
/// (e.g. MIPS32), and a wrapping 32-bit counter is all a name needs —
/// `create_new` exclusivity settles any residual collision.
static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Exclusively creates a fresh, uniquely named tmp file next to `target`,
/// returning its path together with the open handle. The caller writes into
/// the handle instead of reopening the path, so no other writer can swap the
/// file out between creation and write (the handle is kernel-pinned).
///
/// A deterministic tmp name derived from the target (e.g. replacing its
/// extension) would collide across different store targets sharing a stem —
/// `users.yaml` and `users.json` would both map to `users.tmp` — and could
/// even place one instance's tmp file on another instance's real store path
/// (a target literally named `users.yaml.tmp`).
fn create_unique_tmp(target: &Path) -> Result<(PathBuf, std::fs::File), SError> {
    let mut base = target.as_os_str().to_os_string();
    base.push(".tmp");
    for _ in 0..64 {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut candidate = base.clone();
        candidate.push(format!(".{}.{}", std::process::id(), n));
        let candidate = PathBuf::from(candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(SError::Io(std::io::Error::other(
        "could not create a unique tmp file for the user store",
    )))
}

/// Parent of `path`, defaulting to `.` for a bare file name whose parent is
/// the empty path.
fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Flushes the parent directory entry so the rename itself survives a power
/// loss. Directory fsync is a Unix concept; other platforms degrade to a
/// no-op (the file content is already fsynced by the caller).
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(parent_dir(path))?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Atomically write the store to disk: exclusively create a uniquely named
/// tmp file in the target's directory, write and fsync the snapshot there,
/// rename it over the target, then fsync the directory entry.
pub fn save_store(path: &Path, store: &UserStore) -> Result<(), SError> {
    let content = serde_saphyr::to_string(store).map_err(|e| {
        SError::Io(std::io::Error::other(format!(
            "failed to serialize user store: {e}"
        )))
    })?;
    let (tmp, mut file) = create_unique_tmp(path)?;
    let result = (|| {
        use std::io::Write;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        // Durability of the directory entry itself. A failure here reports
        // the save as failed even though the rename already landed — the
        // new content IS on disk; the logged error is merely misleading and
        // a retry rewrites the same content (harmless).
        sync_parent_dir(path)?;
        Ok(())
    })();
    if result.is_err() {
        // Do not leave the orphaned tmp file behind (best effort). After a
        // successful rename the tmp no longer exists and this is a no-op.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Removes tmp files (`<target>.tmp.<pid>.<n>`) orphaned when a previous
/// process was killed between tmp creation and rename. The old deterministic
/// tmp name was silently reused by the next write; the unique per-write name
/// accumulates instead, which matters on flash storage. Best effort: errors
/// (including the directory not existing yet) are ignored.
///
/// Tmp files of *this* process are kept. Tmp files of other live processes
/// are removed too: sharing one store path across processes is unsupported
/// anyway (last-writer-wins), and the worst case is that the other process's
/// in-flight flush errors out and is logged, never corruption.
pub fn clean_stale_tmp(target: &Path) {
    let Some(name) = target.file_name() else {
        return;
    };
    let mut prefix = name.to_os_string();
    prefix.push(".tmp.");
    let Ok(entries) = std::fs::read_dir(parent_dir(target)) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(rest) = file_name
            .to_str()
            .and_then(|s| s.strip_prefix(prefix.to_string_lossy().as_ref()))
        else {
            continue;
        };
        // Suffix is `<pid>.<n>`; parse the pid.
        let Some(pid) = rest.split('.').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Merge users loaded from the store into the config users.
/// Config users take precedence; store-only users (added via API) are appended.
pub fn merge_users(users: &mut Vec<AuthUser>, store: &UserStore) {
    for stored in &store.users {
        if !users.iter().any(|u| u.username == stored.username) {
            users.push(AuthUser {
                username: stored.username.clone(),
                password: stored.password.clone(),
            });
        }
    }
}

/// Convert persisted entries into stats suitable for [`Observer::restore_stats`].
pub fn stored_stats(store: &UserStore) -> Vec<UserStats> {
    store
        .users
        .iter()
        .map(|u| UserStats {
            username: u.username.clone(),
            tcp_sent: u.tcp_sent,
            tcp_recv: u.tcp_recv,
            udp_sent: u.udp_sent,
            udp_recv: u.udp_recv,
            ..Default::default()
        })
        .collect()
}

/// Build a store snapshot from the current user list and live traffic stats.
pub(crate) async fn build_store(users: &[AuthUser], observer: &Observer) -> UserStore {
    let usernames: Vec<String> = users.iter().map(|u| u.username.clone()).collect();
    let stats: HashMap<String, UserStats> = observer
        .get_all_stats(&usernames)
        .await
        .into_iter()
        .map(|s| (s.username.clone(), s))
        .collect();
    UserStore {
        users: users
            .iter()
            .map(|u| {
                let s = stats.get(&u.username);
                PersistedUser {
                    username: u.username.clone(),
                    password: u.password.clone(),
                    tcp_sent: s.map(|s| s.tcp_sent).unwrap_or_default(),
                    tcp_recv: s.map(|s| s.tcp_recv).unwrap_or_default(),
                    udp_sent: s.map(|s| s.udp_sent).unwrap_or_default(),
                    udp_recv: s.map(|s| s.udp_recv).unwrap_or_default(),
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_store(marker: &str) -> UserStore {
        UserStore {
            users: vec![PersistedUser {
                username: marker.into(),
                ..Default::default()
            }],
        }
    }

    /// A save must leave exactly the target file behind: no tmp leftovers on
    /// the success path, and the content readable back.
    #[test]
    fn save_store_leaves_no_tmp_leftovers() {
        let dir = std::env::temp_dir().join(format!("sq-store-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("users.yaml");
        save_store(&target, &marker_store("u1")).unwrap();
        let stored = load_store(&target).unwrap().expect("store exists");
        assert_eq!(stored.users[0].username, "u1");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "tmp leftovers: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Loading reaps tmp files orphaned by a crashed previous process (pid in
    /// the name is not ours) while keeping this process's own tmp files.
    #[test]
    fn load_store_reaps_crash_orphans_but_keeps_own_pid() {
        let dir = std::env::temp_dir().join(format!("sq-store-reap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("users.yaml");
        let orphan = dir.join("users.yaml.tmp.999999.7");
        let own = dir.join(format!("users.yaml.tmp.{}.9", std::process::id()));
        std::fs::write(&orphan, "junk").unwrap();
        std::fs::write(&own, "junk").unwrap();
        // Target missing → Ok(None), but the reap still runs.
        assert!(load_store(&target).unwrap().is_none());
        assert!(!orphan.exists(), "orphaned tmp must be reaped");
        assert!(own.exists(), "own-process tmp must be kept");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
