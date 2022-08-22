use serde::Deserialize;
use std::{path::Path, sync::Arc};
use tokio::{
    fs::OpenOptions,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{RwLock, RwLockReadGuard},
    task::JoinHandle,
};
use xdg::{BaseDirectories, BaseDirectoriesError};
use eyre::WrapErr;
use crate::{
    telemetry::{Telemetry, TELEMETRY_HEADER_NAME},
    FSM_XDG_PREFIX,
};

use self::rust::RustDependencyRegistryData;

pub(crate) mod rust;

const DEPENDENCY_REGISTRY_REMOTE_URL: &str = "https://fsm-server.fly.dev/fsm-registry.json";
const DEPENDENCY_REGISTRY_CACHE_PATH: &str = "registry.json";
const DEPENDENCY_REGISTRY_FALLBACK: &str = include_str!("../../registry.json");

#[derive(Debug, thiserror::Error)]
pub enum DependencyRegistryError {
    #[error("XDG base directories error")]
    BaseDirectories(#[from] BaseDirectoriesError),
    #[error("IO error")]
    Io(#[from] std::io::Error),
    #[error("JSON error")]
    Json(#[from] serde_json::Error),
    #[error("Request error")]
    Reqwest(#[from] reqwest::Error),
    #[error("Wrong registry data version: 1 (expected) != {0} (got)")]
    WrongVersion(usize),
}

pub struct DependencyRegistry {
    data: Arc<RwLock<DependencyRegistryData>>,
    refresh_handle: Option<JoinHandle<()>>,
}

impl DependencyRegistry {
    #[tracing::instrument(skip_all, fields(%offline))]
    pub async fn new(offline: bool) -> Result<Self, DependencyRegistryError> {
        let xdg_dirs = BaseDirectories::with_prefix(FSM_XDG_PREFIX)?;
        // Create the directory if needed
        let cached_registry_pathbuf =
            xdg_dirs.place_cache_file(Path::new(DEPENDENCY_REGISTRY_CACHE_PATH))?;
        // Create the file if needed.
        let mut cached_registry_file = OpenOptions::new()
            .read(true)
            .write(true)
            .truncate(false)
            .create(true) // We do this proactively to avoid the user seeing a non-fatal error later when we freshen the cache.
            .open(cached_registry_pathbuf.clone())
            .await?;
        let mut cached_registry_content = Default::default();
        cached_registry_file
            .read_to_string(&mut cached_registry_content)
            .await?;
        drop(cached_registry_file);

        cached_registry_content = if cached_registry_content.is_empty() {
            DEPENDENCY_REGISTRY_FALLBACK.to_string()
        } else {
            cached_registry_content
        };

        let data: DependencyRegistryData = serde_json::from_str(&cached_registry_content)?;
        if data.version != 1 {
            return Err(DependencyRegistryError::WrongVersion(data.version));
        }

        let data = Arc::new(RwLock::new(data));
        let refresh_handle = if !offline {
            // We detach the join handle as we don't actually care when/if this finishes
            let data = Arc::clone(&data);
            let refresh_handle = tokio::spawn(async move {
                // Refresh the cache
                // We don't want to fail if we can't build telemetry data...
                let telemetry = Telemetry::new().await.as_header_data().wrap_err("Serializing header data")?;
                let http_client = reqwest::Client::new();
                let mut req = http_client.get(DEPENDENCY_REGISTRY_REMOTE_URL);
                if let Some(telemetry) = telemetry {
                    req = req.header(TELEMETRY_HEADER_NAME, &telemetry);
                    tracing::trace!(%telemetry, "Fetching new registry data from {DEPENDENCY_REGISTRY_REMOTE_URL}");
                } else {
                    tracing::trace!(
                        "Fetching new registry data from {DEPENDENCY_REGISTRY_REMOTE_URL}"
                    );
                }
                let res = match req.send().await {
                    Ok(res) => res,
                    Err(err) => {
                        tracing::error!(err = %eyre::eyre!(err), "Could not fetch new registry data from {DEPENDENCY_REGISTRY_REMOTE_URL}");
                        return;
                    }
                };
                let content = match res.text().await {
                    Ok(content) => content,
                    Err(err) => {
                        tracing::error!(err = %eyre::eyre!(err), "Could not fetch new registry data body from {DEPENDENCY_REGISTRY_REMOTE_URL}");
                        return;
                    }
                };
                let fresh_data: DependencyRegistryData = match serde_json::from_str(&content) {
                    Ok(data) => data,
                    Err(err) => {
                        tracing::error!(err = %eyre::eyre!(err), "Could not parse new registry data from {DEPENDENCY_REGISTRY_REMOTE_URL}");
                        return;
                    }
                };
                *data.write().await = fresh_data;
                // Write out the update
                let mut cached_registry_file = match OpenOptions::new()
                    .truncate(true)
                    .create(true)
                    .write(true)
                    .open(cached_registry_pathbuf.clone())
                    .await
                {
                    Ok(cached_registry_file) => cached_registry_file,
                    Err(err) => {
                        tracing::error!(err = %eyre::eyre!(err), path = %cached_registry_pathbuf.display(), "Could not truncate XDG cached registry file to empty");
                        return;
                    }
                };
                match cached_registry_file
                    .write_all(content.trim().as_bytes())
                    .await
                {
                    Ok(_) => {
                        tracing::debug!(path = %cached_registry_pathbuf.display(), "Refreshed remote registry into XDG cache")
                    }
                    Err(err) => {
                        tracing::error!(err = %eyre::eyre!(err), "Could not write to {}", cached_registry_pathbuf.display());
                    }
                };
            });
            Some(refresh_handle)
        } else {
            None
        };

        Ok(Self {
            data,
            refresh_handle,
        })
    }

    pub fn fresh(&self) -> bool {
        if let Some(ref handle) = self.refresh_handle {
            handle.is_finished()
        } else {
            // We're offline
            false
        }
    }

    pub async fn language(&self) -> RwLockReadGuard<DependencyRegistryLanguageData> {
        RwLockReadGuard::map(self.data.read().await, |v| &v.language)
    }
}

/// A registry of known mappings from language specific dependencies to fsm settings
#[derive(Deserialize, Default, Clone, Debug)]
pub struct DependencyRegistryData {
    pub(crate) version: usize, // Checked for ABI compat
    pub(crate) language: DependencyRegistryLanguageData,
}

#[derive(Deserialize, Default, Clone, Debug)]
pub struct DependencyRegistryLanguageData {
    pub(crate) rust: RustDependencyRegistryData,
}
