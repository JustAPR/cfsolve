use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;

use anyhow::{Context, Result, anyhow};
use cfsolve::Browser;

/// Global application state shared across all async Axum worker tasks.
#[derive(Clone)]
pub struct AppState {
    pub default_profile_name: String,
    pub profile_dir: PathBuf,
    pub browsers: Arc<RwLock<HashMap<String, Arc<Browser>>>>,
    pub solve_semaphore: Arc<tokio::sync::Semaphore>,
    pub active_requests: Arc<AtomicU64>,
    pub total_requests: Arc<AtomicU64>,
    pub failed_requests: Arc<AtomicU64>,
}

/// RAII guard to track active requests cleanly and reliably.
pub struct RequestTracker {
    counter: Arc<AtomicU64>,
}

impl RequestTracker {
    pub fn new(counter: Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for RequestTracker {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

impl AppState {
    /// Initialize state and load available profiles.
    pub async fn new(profile_dir: PathBuf, default_profile_name: String) -> Result<Self> {
        let mut map = HashMap::new();

        // Resolve default profile path (must be a file, not the answers directory)
        let mut default_path = profile_dir.join(&default_profile_name);
        if default_path.is_dir() || !default_path.exists() {
            default_path = profile_dir.join(format!("{}.json", default_profile_name));
        }
        if default_path.is_dir() || !default_path.exists() {
            default_path = PathBuf::from(&default_profile_name);
            if default_path.is_dir() {
                default_path = default_path.with_extension("json");
            }
        }

        if !default_path.is_file() {
            return Err(anyhow!(
                "Default profile file not found at '{}'",
                default_path.display()
            ));
        }

        tracing::info!("Loading default profile from {}", default_path.display());
        let browser = Browser::read(&default_path)
            .with_context(|| format!("Reading default profile {}", default_path.display()))?;

        let canonical_default = if default_profile_name.ends_with(".json") {
            default_profile_name
                .strip_suffix(".json")
                .unwrap_or(&default_profile_name)
                .to_string()
        } else {
            default_profile_name.clone()
        };

        map.insert(canonical_default.clone(), Arc::new(browser));

        // Scan profile_dir for additional .json profiles
        if profile_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&profile_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("json") {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            if stem != canonical_default && !map.contains_key(stem) {
                                match Browser::read(&path) {
                                    Ok(b) => {
                                        tracing::info!("Pre-loaded profile: {}", stem);
                                        map.insert(stem.to_string(), Arc::new(b));
                                    }
                                    Err(err) => {
                                        tracing::warn!("Skipping profile {}: {}", stem, err);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let max_concurrent = std::env::var("MAX_CONCURRENT_SOLVES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get() * 4)
                    .unwrap_or(16)
            });
        let solve_semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));

        Ok(Self {
            default_profile_name: canonical_default,
            profile_dir,
            browsers: Arc::new(RwLock::new(map)),
            solve_semaphore,
            active_requests: Arc::new(AtomicU64::new(0)),
            total_requests: Arc::new(AtomicU64::new(0)),
            failed_requests: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Retrieve or dynamically load a Browser by profile name or path.
    pub async fn get_browser(&self, name_or_path: Option<&str>) -> Result<Arc<Browser>> {
        let name = match name_or_path {
            Some(n) if !n.trim().is_empty() => n.trim(),
            _ => &self.default_profile_name,
        };

        let canonical_name = if name.ends_with(".json") {
            name.strip_suffix(".json").unwrap_or(name)
        } else {
            name
        };

        // Check cache with fast read lock
        {
            let map = self.browsers.read().await;
            if let Some(browser) = map.get(canonical_name) {
                return Ok(browser.clone());
            }
        }

        // Acquire write lock with double-check to prevent cache stampede
        let mut map = self.browsers.write().await;
        if let Some(browser) = map.get(canonical_name) {
            return Ok(browser.clone());
        }

        // Try candidate paths (must be files, not directories)
        let mut candidate = self.profile_dir.join(canonical_name);
        if candidate.is_dir() || !candidate.exists() {
            candidate = self.profile_dir.join(format!("{}.json", canonical_name));
        }
        if candidate.is_dir() || !candidate.exists() {
            candidate = PathBuf::from(name);
            if candidate.is_dir() {
                candidate = candidate.with_extension("json");
            }
        }

        if candidate.is_file() {
            let candidate_clone = candidate.clone();
            let browser = tokio::task::spawn_blocking(move || Browser::read(&candidate_clone))
                .await
                .context("Joining profile read task")?
                .with_context(|| format!("Reading profile at {}", candidate.display()))?;
            let arc_browser = Arc::new(browser);

            map.insert(canonical_name.to_string(), arc_browser.clone());
            tracing::info!("Dynamically loaded profile: {}", canonical_name);
            Ok(arc_browser)
        } else {
            Err(anyhow!(
                "Browser profile '{}' not found in cache or filesystem",
                name
            ))
        }
    }

    pub async fn list_profiles(&self) -> Vec<String> {
        let map = self.browsers.read().await;
        map.keys().cloned().collect()
    }
}
