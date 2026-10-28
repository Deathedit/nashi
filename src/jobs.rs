use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, Semaphore, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const INPUT_FILE: &str = "input.iso";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Ps2,
    X360,
}

impl Platform {
    /// Parses the `X-Platform` header value. `auto` (or empty) means "detect it".
    pub fn from_header(value: &str) -> Result<Option<Self>, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(None),
            "ps2" => Ok(Some(Self::Ps2)),
            "x360" => Ok(Some(Self::X360)),
            other => Err(format!("unknown platform {other:?}")),
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Ps2 => "chd",
            Self::X360 => "zar",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Uploading,
    Queued,
    Detecting,
    Extracting,
    Compressing,
    Done,
    Failed,
    Cancelled,
}

impl Stage {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// Snapshot of a job as sent to the browser.
#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: Uuid,
    pub name: String,
    pub platform: Option<Platform>,
    pub stage: Stage,
    pub percent: f32,
    pub message: Option<String>,
    pub output_size: Option<u64>,
}

pub struct Job {
    pub id: Uuid,
    pub name: String,
    pub dir: PathBuf,
    pub cancel: CancellationToken,
    created: Instant,
    touched: Mutex<Instant>,
    tx: watch::Sender<JobView>,
}

impl Job {
    pub fn view(&self) -> JobView {
        self.tx.borrow().clone()
    }

    pub fn update(&self, f: impl FnOnce(&mut JobView)) {
        self.tx.send_modify(f);
    }

    pub fn subscribe(&self) -> watch::Receiver<JobView> {
        self.tx.subscribe()
    }

    /// Restarts the expiry timer (on completion and on every download).
    pub fn touch(&self) {
        *self.touched.lock().unwrap() = Instant::now();
    }

    pub fn input_path(&self) -> PathBuf {
        self.dir.join(INPUT_FILE)
    }

    pub fn output_path(&self, platform: Platform) -> PathBuf {
        self.dir.join(format!("output.{}", platform.extension()))
    }

    /// `Halo 3.iso` → `Halo 3.zar`
    pub fn download_name(&self, platform: Platform) -> String {
        let stem = self
            .name
            .rsplit_once('.')
            .map_or(self.name.as_str(), |(stem, _)| stem);
        format!("{stem}.{}", platform.extension())
    }
}

#[derive(Clone)]
pub struct AppState {
    jobs: Arc<RwLock<HashMap<Uuid, Arc<Job>>>>,
    pub sem: Arc<Semaphore>,
    work_dir: PathBuf,
    ttl: Duration,
}

impl AppState {
    pub fn new(work_dir: PathBuf, max_jobs: usize, ttl: Duration) -> Self {
        Self {
            jobs: Default::default(),
            sem: Arc::new(Semaphore::new(max_jobs.max(1))),
            work_dir,
            ttl,
        }
    }

    pub async fn create_job(&self, name: String) -> io::Result<Arc<Job>> {
        let id = Uuid::new_v4();
        let dir = self.work_dir.join(id.to_string());
        tokio::fs::create_dir_all(&dir).await?;

        let (tx, _) = watch::channel(JobView {
            id,
            name: name.clone(),
            platform: None,
            stage: Stage::Uploading,
            percent: 0.0,
            message: None,
            output_size: None,
        });
        let now = Instant::now();
        let job = Arc::new(Job {
            id,
            name,
            dir,
            cancel: CancellationToken::new(),
            created: now,
            touched: Mutex::new(now),
            tx,
        });
        self.jobs.write().await.insert(id, job.clone());
        Ok(job)
    }

    pub async fn get(&self, id: Uuid) -> Option<Arc<Job>> {
        self.jobs.read().await.get(&id).cloned()
    }

    pub async fn remove(&self, id: Uuid) -> Option<Arc<Job>> {
        self.jobs.write().await.remove(&id)
    }

    pub async fn list(&self) -> Vec<JobView> {
        let jobs = self.jobs.read().await;
        let mut jobs: Vec<_> = jobs.values().collect();
        jobs.sort_by_key(|j| j.created);
        jobs.iter().map(|j| j.view()).collect()
    }

    /// Drops finished jobs whose output hasn't been touched for `ttl`.
    pub async fn sweep_expired(&self) {
        let expired: Vec<Arc<Job>> = {
            let mut jobs = self.jobs.write().await;
            let ids: Vec<Uuid> = jobs
                .values()
                .filter(|j| {
                    j.view().stage.is_finished() && j.touched.lock().unwrap().elapsed() > self.ttl
                })
                .map(|j| j.id)
                .collect();
            ids.iter().filter_map(|id| jobs.remove(id)).collect()
        };
        for job in expired {
            tracing::info!(id = %job.id, name = %job.name, "expired, removing");
            let _ = tokio::fs::remove_dir_all(&job.dir).await;
        }
    }

    /// Jobs only live in memory, so job dirs left by a previous run are orphans.
    pub async fn clean_orphans(&self) -> io::Result<()> {
        tokio::fs::create_dir_all(&self.work_dir).await?;
        let mut entries = tokio::fs::read_dir(&self.work_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let is_job_dir = entry
                .file_name()
                .to_str()
                .is_some_and(|n| Uuid::parse_str(n).is_ok());
            if is_job_dir && entry.file_type().await?.is_dir() {
                tracing::info!(path = %entry.path().display(), "removing leftover job dir");
                tokio::fs::remove_dir_all(entry.path()).await?;
            }
        }
        Ok(())
    }
}
