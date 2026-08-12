use std::{collections::HashMap, path::Path};

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
pub fn load_store(path: &Path) -> Result<Option<UserStore>, SError> {
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

/// Atomically write the store to disk (tmp file + rename).
pub fn save_store(path: &Path, store: &UserStore) -> Result<(), SError> {
    let content = serde_saphyr::to_string(store).map_err(|e| {
        SError::Io(std::io::Error::other(format!(
            "failed to serialize user store: {e}"
        )))
    })?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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
