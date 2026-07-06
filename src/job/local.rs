use super::backend::JobQueueBackend;
use super::memory::InMemoryJobQueue;
use super::types::{
    Job, JobFlow, JobFlowDependencies, JobListOptions, JobListPage, JobLogPage, JobOptions,
    JobPriority, JobPriorityCount, JobQueueSnapshot, JobQueueStats, JobRepeatEntry, JobSpec,
    JobState, JobStateCount, JobWorkerId,
};
use crate::error::{LaneError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;

/// Filesystem-backed durable job queue.
///
/// This backend is single-process and writes a JSON snapshot after every state
/// mutation. It is intended for local durable runtimes and as a persistence
/// reference for remote backends with atomic primitives.
#[derive(Debug, Clone)]
pub struct LocalJobQueue {
    inner: InMemoryJobQueue,
    snapshot_path: PathBuf,
}

impl LocalJobQueue {
    /// Open a durable local queue from a snapshot file.
    pub async fn open(queue: impl Into<String>, snapshot_path: impl AsRef<Path>) -> Result<Self> {
        let queue = queue.into();
        let snapshot_path = snapshot_path.as_ref().to_path_buf();
        let snapshot = load_job_snapshot(&snapshot_path).await?;
        let inner = match snapshot {
            Some(snapshot) => {
                if snapshot.queue != queue {
                    return Err(LaneError::ConfigError(format!(
                        "snapshot queue '{}' does not match requested queue '{}'",
                        snapshot.queue, queue
                    )));
                }
                InMemoryJobQueue::from_snapshot(snapshot)
            }
            None => InMemoryJobQueue::new(queue),
        };

        Ok(Self {
            inner,
            snapshot_path,
        })
    }

    /// Queue name.
    pub fn queue_name(&self) -> &str {
        self.inner.queue_name()
    }

    /// Snapshot file path.
    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot_path
    }

    /// Add a job using the current wall-clock time.
    pub async fn add(
        &self,
        name: impl Into<String>,
        payload: Value,
        options: JobOptions,
    ) -> Result<Job> {
        let job = self.inner.add(name, payload, options).await?;
        self.persist().await?;
        Ok(job)
    }

    /// Add a job at an explicit timestamp. Primarily useful for deterministic tests.
    pub async fn add_at(
        &self,
        name: impl Into<String>,
        payload: Value,
        options: JobOptions,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let job = self.inner.add_at(name, payload, options, now).await?;
        self.persist().await?;
        Ok(job)
    }

    /// Add multiple jobs using the current wall-clock time.
    pub async fn add_many(&self, jobs: Vec<JobSpec>) -> Result<Vec<Job>> {
        let jobs = self.inner.add_many(jobs).await?;
        self.persist().await?;
        Ok(jobs)
    }

    /// Add multiple jobs at an explicit timestamp.
    pub async fn add_many_at(&self, jobs: Vec<JobSpec>, now: DateTime<Utc>) -> Result<Vec<Job>> {
        let jobs = self.inner.add_many_at(jobs, now).await?;
        self.persist().await?;
        Ok(jobs)
    }

    /// Add a parent-child flow using the current wall-clock time.
    pub async fn add_flow(&self, parent: JobSpec, children: Vec<JobSpec>) -> Result<JobFlow> {
        let flow = self.inner.add_flow(parent, children).await?;
        self.persist().await?;
        Ok(flow)
    }

    /// Add a parent-child flow at an explicit timestamp.
    pub async fn add_flow_at(
        &self,
        parent: JobSpec,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<JobFlow> {
        let flow = self.inner.add_flow_at(parent, children, now).await?;
        self.persist().await?;
        Ok(flow)
    }

    /// Return a parent flow's current child dependency snapshot.
    pub async fn get_flow_dependencies(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencies>> {
        self.inner.get_flow_dependencies(parent_id).await
    }

    /// Return the current state for a job id.
    pub async fn get_state(&self, job_id: &str) -> Result<Option<JobState>> {
        self.inner.get_state(job_id).await
    }

    /// Capture the durable queue snapshot.
    pub async fn snapshot(&self) -> JobQueueSnapshot {
        self.inner.snapshot().await
    }

    /// Remove the current non-terminal occurrence for a repeat series.
    pub async fn remove_repeat(&self, repeat_key: &str) -> Result<Option<Job>> {
        let job = self.inner.remove_repeat(repeat_key).await?;
        self.persist().await?;
        Ok(job)
    }

    /// Remove the active deduplication owner key.
    pub async fn remove_deduplication_key(&self, deduplication_id: &str) -> Result<bool> {
        let removed = self
            .inner
            .remove_deduplication_key(deduplication_id)
            .await?;
        self.persist().await?;
        Ok(removed)
    }

    /// List current non-terminal repeat series owners.
    pub async fn list_repeats(&self) -> Result<Vec<JobRepeatEntry>> {
        self.inner.list_repeats().await
    }

    /// Drain waiting jobs and optionally non-repeat delayed jobs.
    pub async fn drain(&self, include_delayed: bool) -> Result<Vec<Job>> {
        let jobs = self.inner.drain(include_delayed).await?;
        self.persist().await?;
        Ok(jobs)
    }

    async fn persist(&self) -> Result<()> {
        persist_job_snapshot(&self.snapshot_path, &self.inner.snapshot().await).await
    }
}
#[async_trait]
impl JobQueueBackend for LocalJobQueue {
    async fn add_job(&self, name: String, payload: Value, options: JobOptions) -> Result<Job> {
        self.add(name, payload, options).await
    }

    async fn add_jobs(&self, jobs: Vec<JobSpec>, now: DateTime<Utc>) -> Result<Vec<Job>> {
        self.add_many_at(jobs, now).await
    }

    async fn add_flow(
        &self,
        parent: JobSpec,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<JobFlow> {
        self.add_flow_at(parent, children, now).await
    }

    async fn get_flow_dependencies(&self, parent_id: &str) -> Result<Option<JobFlowDependencies>> {
        LocalJobQueue::get_flow_dependencies(self, parent_id).await
    }

    async fn claim_next(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>> {
        let job = self.inner.claim_next(worker_id, lease_for, now).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn complete_job(
        &self,
        job_id: &str,
        lock_token: &str,
        value: Value,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let job = self
            .inner
            .complete_job(job_id, lock_token, value, now)
            .await?;
        self.persist().await?;
        Ok(job)
    }

    async fn fail_job(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let job = self.inner.fail_job(job_id, lock_token, error, now).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn renew_lease(
        &self,
        job_id: &str,
        lock_token: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let job = self
            .inner
            .renew_lease(job_id, lock_token, lease_for, now)
            .await?;
        self.persist().await?;
        Ok(job)
    }

    async fn delay_active_job(
        &self,
        job_id: &str,
        lock_token: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let job = self
            .inner
            .delay_active_job(job_id, lock_token, delay, now)
            .await?;
        self.persist().await?;
        Ok(job)
    }

    async fn promote_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let job = self.inner.promote_job(job_id, now).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn reschedule_job(
        &self,
        job_id: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let job = self.inner.reschedule_job(job_id, delay, now).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn retry_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let job = self.inner.retry_job(job_id, now).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn update_priority(&self, job_id: &str, priority: JobPriority) -> Result<Job> {
        let job = self.inner.update_priority(job_id, priority).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn remove_job(&self, job_id: &str) -> Result<Option<Job>> {
        let job = self.inner.remove_job(job_id).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn remove_repeat(&self, repeat_key: &str) -> Result<Option<Job>> {
        LocalJobQueue::remove_repeat(self, repeat_key).await
    }

    async fn remove_deduplication_key(&self, deduplication_id: &str) -> Result<bool> {
        LocalJobQueue::remove_deduplication_key(self, deduplication_id).await
    }

    async fn list_repeats(&self) -> Result<Vec<JobRepeatEntry>> {
        LocalJobQueue::list_repeats(self).await
    }

    async fn clean_jobs(
        &self,
        state: JobState,
        grace: Duration,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>> {
        let jobs = self.inner.clean_jobs(state, grace, limit, now).await?;
        self.persist().await?;
        Ok(jobs)
    }

    async fn drain_jobs(&self, include_delayed: bool) -> Result<Vec<Job>> {
        LocalJobQueue::drain(self, include_delayed).await
    }

    async fn list_jobs(&self, options: JobListOptions) -> Result<JobListPage> {
        self.inner.list_jobs(options).await
    }

    async fn get_job_counts(&self, states: &[JobState]) -> Result<Vec<JobStateCount>> {
        self.inner.get_job_counts(states).await
    }

    async fn get_counts_per_priority(
        &self,
        priorities: &[JobPriority],
    ) -> Result<Vec<JobPriorityCount>> {
        self.inner.get_counts_per_priority(priorities).await
    }

    async fn update_progress(&self, job_id: &str, progress: Value) -> Result<Job> {
        let job = self.inner.update_progress(job_id, progress).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn add_log(
        &self,
        job_id: &str,
        line: String,
        keep: usize,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let job = self.inner.add_log(job_id, line, keep, now).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn get_job_logs(
        &self,
        job_id: &str,
        start: isize,
        end: isize,
        ascending: bool,
    ) -> Result<JobLogPage> {
        self.inner.get_job_logs(job_id, start, end, ascending).await
    }

    async fn promote_due_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let promoted = self.inner.promote_due_jobs(now).await?;
        if promoted > 0 {
            self.persist().await?;
        }
        Ok(promoted)
    }

    async fn recover_stalled_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let recovered = self.inner.recover_stalled_jobs(now).await?;
        if recovered > 0 {
            self.persist().await?;
        }
        Ok(recovered)
    }

    async fn pause(&self) -> Result<()> {
        self.inner.pause().await?;
        self.persist().await
    }

    async fn resume(&self) -> Result<()> {
        self.inner.resume().await?;
        self.persist().await
    }

    async fn get_job(&self, job_id: &str) -> Result<Option<Job>> {
        self.inner.get_job(job_id).await
    }

    async fn get_job_state(&self, job_id: &str) -> Result<Option<JobState>> {
        self.inner.get_job_state(job_id).await
    }

    async fn stats(&self) -> Result<JobQueueStats> {
        self.inner.stats().await
    }
}

async fn load_job_snapshot(path: &Path) -> Result<Option<JobQueueSnapshot>> {
    match fs::read(path).await {
        Ok(bytes) => serde_json::from_slice::<JobQueueSnapshot>(&bytes)
            .map(Some)
            .map_err(|error| LaneError::Other(format!("failed to decode job snapshot: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(LaneError::Other(format!(
            "failed to read job snapshot: {error}"
        ))),
    }
}

async fn persist_job_snapshot(path: &Path, snapshot: &JobQueueSnapshot) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).await.map_err(|error| {
            LaneError::Other(format!("failed to create job snapshot directory: {error}"))
        })?;
    }

    let data = serde_json::to_vec_pretty(snapshot)
        .map_err(|error| LaneError::Other(format!("failed to encode job snapshot: {error}")))?;
    let tmp_path = path.with_extension(format!(
        "{}tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!("{extension}."))
            .unwrap_or_default()
    ));

    fs::write(&tmp_path, data)
        .await
        .map_err(|error| LaneError::Other(format!("failed to write job snapshot: {error}")))?;
    fs::rename(&tmp_path, path)
        .await
        .map_err(|error| LaneError::Other(format!("failed to replace job snapshot: {error}")))?;

    Ok(())
}
