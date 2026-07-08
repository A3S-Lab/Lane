use super::backend::JobQueueBackend;
use super::memory::InMemoryJobQueue;
use super::types::{
    Job, JobEvent, JobFinishedResult, JobFlow, JobFlowChildValues, JobFlowDependencies,
    JobFlowDependencyCountOptions, JobFlowDependencyCounts, JobFlowDependencyPage,
    JobFlowDependencyPageOptions, JobFlowDependencyPages, JobFlowDependencyPagesOptions,
    JobFlowDependencySelectedCounts, JobFlowDependencyValues, JobFlowIgnoredFailures, JobId,
    JobListOptions, JobListPage, JobLogPage, JobOptions, JobPriority, JobPriorityCount,
    JobQueueSnapshot, JobQueueStats, JobRepeatEntry, JobRepeatListOptions, JobRepeatPage, JobSpec,
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

    /// Add children to an active flow parent using the current wall-clock time.
    pub async fn add_flow_children(
        &self,
        parent_id: &str,
        lock_token: &str,
        children: Vec<JobSpec>,
    ) -> Result<Vec<Job>> {
        self.add_flow_children_at(parent_id, lock_token, children, Utc::now())
            .await
    }

    /// Add children to an active flow parent and move the parent to waiting-children.
    pub async fn add_flow_children_at(
        &self,
        parent_id: &str,
        lock_token: &str,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>> {
        let children = self
            .inner
            .add_flow_children_at(parent_id, lock_token, children, now)
            .await?;
        self.persist().await?;
        Ok(children)
    }

    /// Return a parent flow's current child dependency snapshot.
    pub async fn get_flow_dependencies(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencies>> {
        self.inner.get_flow_dependencies(parent_id).await
    }

    /// Return a parent flow's dependency counts.
    pub async fn get_flow_dependency_counts(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyCounts>> {
        self.inner.get_flow_dependency_counts(parent_id).await
    }

    /// Return selected BullMQ-style dependency counts for a flow parent.
    pub async fn get_flow_dependency_selected_counts(
        &self,
        parent_id: &str,
        options: JobFlowDependencyCountOptions,
    ) -> Result<Option<JobFlowDependencySelectedCounts>> {
        self.inner
            .get_flow_dependency_selected_counts(parent_id, options)
            .await
    }

    /// Return BullMQ-style full dependency buckets for a flow parent.
    pub async fn get_flow_dependency_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyValues>> {
        self.inner.get_flow_dependency_values(parent_id).await
    }

    /// Return one cursor page from a parent flow dependency bucket.
    pub async fn get_flow_dependency_page(
        &self,
        parent_id: &str,
        options: JobFlowDependencyPageOptions,
    ) -> Result<Option<JobFlowDependencyPage>> {
        self.inner
            .get_flow_dependency_page(parent_id, options)
            .await
    }

    /// Return cursor pages from several parent flow dependency buckets.
    pub async fn get_flow_dependency_pages(
        &self,
        parent_id: &str,
        options: JobFlowDependencyPagesOptions,
    ) -> Result<Option<JobFlowDependencyPages>> {
        self.inner
            .get_flow_dependency_pages(parent_id, options)
            .await
    }

    /// Return completed child result values for a flow parent.
    pub async fn get_flow_children_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowChildValues>> {
        self.inner.get_flow_children_values(parent_id).await
    }

    /// Return ignored child failure reasons for a flow parent.
    pub async fn get_flow_ignored_children_failures(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowIgnoredFailures>> {
        self.inner
            .get_flow_ignored_children_failures(parent_id)
            .await
    }

    /// Remove children that are still unprocessed and not active.
    pub async fn remove_unprocessed_children(
        &self,
        parent_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Vec<Job>>> {
        let removed = self
            .inner
            .remove_unprocessed_children(parent_id, now)
            .await?;
        self.persist().await?;
        Ok(removed)
    }

    /// Remove a single child from its parent dependency list without deleting the child job.
    pub async fn remove_child_dependency(
        &self,
        child_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let removed = self.inner.remove_child_dependency(child_id, now).await?;
        self.persist().await?;
        Ok(removed)
    }

    /// Return the current state for a job id.
    pub async fn get_state(&self, job_id: &str) -> Result<Option<JobState>> {
        self.inner.get_state(job_id).await
    }

    /// Return finished status and retained terminal payload for a job.
    pub async fn get_finished_result(&self, job_id: &str) -> Result<Option<JobFinishedResult>> {
        self.inner.get_finished_result(job_id).await
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

    /// Return the current active job id for a deduplication id.
    pub async fn get_deduplication_job_id(&self, deduplication_id: &str) -> Result<Option<JobId>> {
        self.inner.get_deduplication_job_id(deduplication_id).await
    }

    /// List current non-terminal repeat series owners.
    pub async fn list_repeats(&self) -> Result<Vec<JobRepeatEntry>> {
        self.inner.list_repeats().await
    }

    /// Return one repeat series / job scheduler by key.
    pub async fn get_repeat(&self, repeat_key: &str) -> Result<Option<JobRepeatEntry>> {
        self.inner.get_repeat(repeat_key).await
    }

    /// Return the number of current repeat series / job schedulers.
    pub async fn count_repeats(&self) -> Result<usize> {
        self.inner.count_repeats().await
    }

    /// Return repeat series / job schedulers ordered by next scheduled time.
    pub async fn list_repeats_page(&self, options: JobRepeatListOptions) -> Result<JobRepeatPage> {
        self.inner.list_repeats_page(options).await
    }

    /// Create or replace the current non-active occurrence for a repeat series.
    pub async fn upsert_repeat(&self, spec: JobSpec, now: DateTime<Utc>) -> Result<Job> {
        let job = self.inner.upsert_repeat(spec, now).await?;
        self.persist().await?;
        Ok(job)
    }

    /// Clear retained log entries for a job. `keep == 0` clears all logs.
    pub async fn clear_logs(&self, job_id: &str, keep: usize) -> Result<JobLogPage> {
        let logs = self.inner.clear_logs(job_id, keep).await?;
        self.persist().await?;
        Ok(logs)
    }

    /// Save retained failure diagnostics for a job.
    pub async fn save_stacktrace(
        &self,
        job_id: &str,
        stacktrace: Vec<String>,
        failed_reason: String,
    ) -> Result<Job> {
        let job = self
            .inner
            .save_stacktrace(job_id, stacktrace, failed_reason)
            .await?;
        self.persist().await?;
        Ok(job)
    }

    /// Drain waiting jobs and optionally non-repeat delayed jobs.
    pub async fn drain(&self, include_delayed: bool) -> Result<Vec<Job>> {
        let jobs = self.inner.drain(include_delayed).await?;
        self.persist().await?;
        Ok(jobs)
    }

    /// Remove every job and queue-owned metadata entry.
    pub async fn obliterate(&self, force: bool) -> Result<usize> {
        let result = self.inner.obliterate(force).await;
        self.persist().await?;
        result
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

    async fn add_flow_children(
        &self,
        parent_id: &str,
        lock_token: &str,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>> {
        self.add_flow_children_at(parent_id, lock_token, children, now)
            .await
    }

    async fn get_flow_dependencies(&self, parent_id: &str) -> Result<Option<JobFlowDependencies>> {
        LocalJobQueue::get_flow_dependencies(self, parent_id).await
    }

    async fn get_flow_dependency_counts(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyCounts>> {
        LocalJobQueue::get_flow_dependency_counts(self, parent_id).await
    }

    async fn get_flow_dependency_selected_counts(
        &self,
        parent_id: &str,
        options: JobFlowDependencyCountOptions,
    ) -> Result<Option<JobFlowDependencySelectedCounts>> {
        LocalJobQueue::get_flow_dependency_selected_counts(self, parent_id, options).await
    }

    async fn get_flow_dependency_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyValues>> {
        LocalJobQueue::get_flow_dependency_values(self, parent_id).await
    }

    async fn get_flow_dependency_page(
        &self,
        parent_id: &str,
        options: JobFlowDependencyPageOptions,
    ) -> Result<Option<JobFlowDependencyPage>> {
        LocalJobQueue::get_flow_dependency_page(self, parent_id, options).await
    }

    async fn get_flow_dependency_pages(
        &self,
        parent_id: &str,
        options: JobFlowDependencyPagesOptions,
    ) -> Result<Option<JobFlowDependencyPages>> {
        LocalJobQueue::get_flow_dependency_pages(self, parent_id, options).await
    }

    async fn get_flow_children_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowChildValues>> {
        LocalJobQueue::get_flow_children_values(self, parent_id).await
    }

    async fn get_flow_ignored_children_failures(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowIgnoredFailures>> {
        LocalJobQueue::get_flow_ignored_children_failures(self, parent_id).await
    }

    async fn remove_unprocessed_children(
        &self,
        parent_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Vec<Job>>> {
        LocalJobQueue::remove_unprocessed_children(self, parent_id, now).await
    }

    async fn remove_child_dependency(&self, child_id: &str, now: DateTime<Utc>) -> Result<bool> {
        LocalJobQueue::remove_child_dependency(self, child_id, now).await
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

    async fn fail_job_discarding_retry(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let job = self
            .inner
            .fail_job_discarding_retry(job_id, lock_token, error, now)
            .await?;
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

    async fn release_active_job(
        &self,
        job_id: &str,
        lock_token: &str,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let job = self
            .inner
            .release_active_job(job_id, lock_token, now)
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

    async fn update_priority_with_lifo(
        &self,
        job_id: &str,
        priority: JobPriority,
        lifo: bool,
    ) -> Result<Job> {
        let job = self
            .inner
            .update_priority_with_lifo(job_id, priority, lifo)
            .await?;
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

    async fn get_deduplication_job_id(&self, deduplication_id: &str) -> Result<Option<JobId>> {
        LocalJobQueue::get_deduplication_job_id(self, deduplication_id).await
    }

    async fn list_repeats(&self) -> Result<Vec<JobRepeatEntry>> {
        LocalJobQueue::list_repeats(self).await
    }

    async fn upsert_repeat(&self, spec: JobSpec, now: DateTime<Utc>) -> Result<Job> {
        LocalJobQueue::upsert_repeat(self, spec, now).await
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

    async fn obliterate(&self, force: bool) -> Result<usize> {
        LocalJobQueue::obliterate(self, force).await
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

    async fn update_data(&self, job_id: &str, payload: Value) -> Result<Job> {
        let job = self.inner.update_data(job_id, payload).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn update_progress(&self, job_id: &str, progress: Value) -> Result<Job> {
        let job = self.inner.update_progress(job_id, progress).await?;
        self.persist().await?;
        Ok(job)
    }

    async fn save_stacktrace(
        &self,
        job_id: &str,
        stacktrace: Vec<String>,
        failed_reason: String,
    ) -> Result<Job> {
        LocalJobQueue::save_stacktrace(self, job_id, stacktrace, failed_reason).await
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

    async fn clear_job_logs(&self, job_id: &str, keep: usize) -> Result<JobLogPage> {
        self.clear_logs(job_id, keep).await
    }

    async fn read_events(&self, start: &str, end: &str, limit: usize) -> Result<Vec<JobEvent>> {
        self.inner.read_events(start, end, limit).await
    }

    async fn trim_events(&self, max_len: usize) -> Result<usize> {
        let removed = self.inner.trim_events(max_len).await?;
        self.persist().await?;
        Ok(removed)
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

    async fn is_paused(&self) -> Result<bool> {
        self.inner.is_paused().await
    }

    async fn get_job(&self, job_id: &str) -> Result<Option<Job>> {
        self.inner.get_job(job_id).await
    }

    async fn get_job_state(&self, job_id: &str) -> Result<Option<JobState>> {
        self.inner.get_job_state(job_id).await
    }

    async fn get_job_finished_result(&self, job_id: &str) -> Result<Option<JobFinishedResult>> {
        LocalJobQueue::get_finished_result(self, job_id).await
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
