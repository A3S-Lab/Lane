use super::backend::JobQueueBackend;
use super::types::{
    Job, JobId, JobListOptions, JobListPage, JobLogEntry, JobOptions, JobQueueSnapshot,
    JobQueueStats, JobState, JobWorkerId, QueueName,
};
use crate::error::{LaneError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Debug, Default)]
struct InMemoryJobQueueState {
    paused: bool,
    jobs: HashMap<JobId, Job>,
}

/// In-memory implementation of the generic job queue backend.
///
/// This backend is process-local. It is intentionally useful for development,
/// tests, embedded runtimes, and as a reference implementation for Redis,
/// Postgres, or NATS-backed implementations.
#[derive(Debug, Clone)]
pub struct InMemoryJobQueue {
    queue: QueueName,
    inner: Arc<Mutex<InMemoryJobQueueState>>,
}

impl InMemoryJobQueue {
    /// Create an empty queue.
    pub fn new(queue: impl Into<String>) -> Self {
        Self {
            queue: queue.into(),
            inner: Arc::new(Mutex::new(InMemoryJobQueueState::default())),
        }
    }

    /// Restore an in-memory queue from a durable snapshot.
    pub fn from_snapshot(snapshot: JobQueueSnapshot) -> Self {
        Self {
            queue: snapshot.queue,
            inner: Arc::new(Mutex::new(InMemoryJobQueueState {
                paused: snapshot.paused,
                jobs: snapshot
                    .jobs
                    .into_iter()
                    .map(|job| (job.id.clone(), job))
                    .collect(),
            })),
        }
    }

    /// Queue name.
    pub fn queue_name(&self) -> &str {
        &self.queue
    }

    /// Capture the current queue state for durable storage.
    pub async fn snapshot(&self) -> JobQueueSnapshot {
        let inner = self.inner.lock().await;
        let mut jobs = inner.jobs.values().cloned().collect::<Vec<_>>();
        jobs.sort_by(compare_list_order);
        JobQueueSnapshot {
            queue: self.queue.clone(),
            paused: inner.paused,
            jobs,
        }
    }

    /// Add a job using the current wall-clock time.
    pub async fn add(
        &self,
        name: impl Into<String>,
        payload: Value,
        options: JobOptions,
    ) -> Result<Job> {
        self.add_at(name, payload, options, Utc::now()).await
    }

    /// Add a job at an explicit timestamp. Primarily useful for deterministic tests.
    pub async fn add_at(
        &self,
        name: impl Into<String>,
        payload: Value,
        options: JobOptions,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let job = Job::new(self.queue.clone(), name.into(), payload, options, now);
        let mut inner = self.inner.lock().await;
        inner.jobs.insert(job.id.clone(), job.clone());
        Ok(job)
    }

    /// Remove a job from the queue.
    pub async fn remove(&self, job_id: &str) -> Result<Option<Job>> {
        let mut inner = self.inner.lock().await;
        Ok(inner.jobs.remove(job_id))
    }

    /// Promote a single delayed job to waiting.
    pub async fn promote(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        if job.state == JobState::Delayed {
            job.state = JobState::Waiting;
            job.scheduled_at = now;
        }
        Ok(job.clone())
    }

    /// Manually retry a failed job by moving it back to the waiting state.
    pub async fn retry(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        if job.state != JobState::Failed {
            return Err(LaneError::JobStateConflict(format!(
                "cannot retry job {} from state {:?}",
                job.id, job.state
            )));
        }
        job.state = JobState::Waiting;
        job.scheduled_at = now;
        job.processed_at = None;
        job.finished_at = None;
        job.worker_id = None;
        job.lease_expires_at = None;
        job.failed_reason = None;
        Ok(job.clone())
    }

    /// Renew the active worker lease for a job.
    pub async fn renew(
        &self,
        job_id: &str,
        worker_id: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        require_active(job, "renew lease")?;
        require_worker(job, worker_id)?;
        job.lease_expires_at = Some(add_duration(now, lease_for));
        Ok(job.clone())
    }

    /// List jobs with deterministic pagination.
    pub async fn list(&self, options: JobListOptions) -> Result<JobListPage> {
        let inner = self.inner.lock().await;
        let mut jobs = inner
            .jobs
            .values()
            .filter(|job| match options.state {
                Some(state) => job.state == state,
                None => true,
            })
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by(compare_list_order);

        let total = jobs.len();
        let start = options.offset.min(total);
        let end = start.saturating_add(options.limit).min(total);
        let jobs = if options.limit == 0 {
            Vec::new()
        } else {
            jobs[start..end].to_vec()
        };

        Ok(JobListPage {
            jobs,
            total,
            offset: options.offset,
            limit: options.limit,
        })
    }

    /// Remove old jobs in a specific state and return their snapshots.
    pub async fn clean(
        &self,
        state: JobState,
        grace: Duration,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>> {
        if state == JobState::Active || limit == 0 {
            return Ok(Vec::new());
        }

        let cutoff = subtract_duration(now, grace);
        let mut inner = self.inner.lock().await;
        let mut jobs = inner
            .jobs
            .values()
            .filter(|job| job.state == state && job_reference_time(job) <= cutoff)
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by(|a, b| {
            job_reference_time(a)
                .cmp(&job_reference_time(b))
                .then_with(|| a.id.cmp(&b.id))
        });
        jobs.truncate(limit);

        for job in &jobs {
            inner.jobs.remove(&job.id);
        }

        Ok(jobs)
    }

    /// Update progress for a non-terminal job.
    pub async fn set_progress(&self, job_id: &str, progress: Value) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        if job.state.is_terminal() {
            return Err(LaneError::JobStateConflict(format!(
                "cannot update progress for terminal job {}",
                job.id
            )));
        }
        job.progress = Some(progress);
        Ok(job.clone())
    }

    /// Append a log line. `keep == 0` retains all log lines.
    pub async fn log(
        &self,
        job_id: &str,
        line: String,
        keep: usize,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        job.logs.push(JobLogEntry {
            timestamp: now,
            line,
        });
        if keep > 0 && job.logs.len() > keep {
            let remove_count = job.logs.len() - keep;
            job.logs.drain(0..remove_count);
        }
        Ok(job.clone())
    }

    fn promote_due_locked(inner: &mut InMemoryJobQueueState, now: DateTime<Utc>) -> usize {
        let mut promoted = 0;
        for job in inner.jobs.values_mut() {
            if job.state == JobState::Delayed && job.scheduled_at <= now {
                job.state = JobState::Waiting;
                promoted += 1;
            }
        }
        promoted
    }
}
#[async_trait]
impl JobQueueBackend for InMemoryJobQueue {
    async fn add_job(&self, name: String, payload: Value, options: JobOptions) -> Result<Job> {
        self.add(name, payload, options).await
    }

    async fn claim_next(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>> {
        let mut inner = self.inner.lock().await;
        if inner.paused {
            return Ok(None);
        }
        Self::promote_due_locked(&mut inner, now);

        let selected_id = inner
            .jobs
            .values()
            .filter(|job| job.state == JobState::Waiting)
            .min_by(compare_claim_order)
            .map(|job| job.id.clone());

        let Some(job_id) = selected_id else {
            return Ok(None);
        };

        let job = inner
            .jobs
            .get_mut(&job_id)
            .expect("selected job must still exist");
        job.state = JobState::Active;
        job.attempts_made = job.attempts_made.saturating_add(1);
        job.processed_at = Some(now);
        job.worker_id = Some(worker_id);
        job.lease_expires_at = Some(add_duration(now, lease_for));
        job.failed_reason = None;
        Ok(Some(job.clone()))
    }

    async fn complete_job(&self, job_id: &str, value: Value, now: DateTime<Utc>) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        require_active(job, "complete")?;
        job.state = JobState::Completed;
        job.finished_at = Some(now);
        job.worker_id = None;
        job.lease_expires_at = None;
        job.return_value = Some(value);
        let completed = job.clone();
        if completed.options.remove_on_complete {
            inner.jobs.remove(job_id);
        }
        Ok(completed)
    }

    async fn fail_job(&self, job_id: &str, error: String, now: DateTime<Utc>) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        require_active(job, "fail")?;
        job.worker_id = None;
        job.lease_expires_at = None;
        job.failed_reason = Some(error);

        if should_retry(job) {
            let delay = job
                .options
                .retry_policy
                .delay_for_attempt(job.attempts_made);
            job.state = JobState::Delayed;
            job.scheduled_at = add_duration(now, delay);
            job.finished_at = None;
        } else {
            job.state = JobState::Failed;
            job.finished_at = Some(now);
        }

        let failed = job.clone();
        if failed.state == JobState::Failed && failed.options.remove_on_fail {
            inner.jobs.remove(job_id);
        }
        Ok(failed)
    }

    async fn renew_lease(
        &self,
        job_id: &str,
        worker_id: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        self.renew(job_id, worker_id, lease_for, now).await
    }

    async fn promote_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        self.promote(job_id, now).await
    }

    async fn retry_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        self.retry(job_id, now).await
    }

    async fn remove_job(&self, job_id: &str) -> Result<Option<Job>> {
        self.remove(job_id).await
    }

    async fn clean_jobs(
        &self,
        state: JobState,
        grace: Duration,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>> {
        self.clean(state, grace, limit, now).await
    }

    async fn list_jobs(&self, options: JobListOptions) -> Result<JobListPage> {
        self.list(options).await
    }

    async fn update_progress(&self, job_id: &str, progress: Value) -> Result<Job> {
        self.set_progress(job_id, progress).await
    }

    async fn add_log(
        &self,
        job_id: &str,
        line: String,
        keep: usize,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        self.log(job_id, line, keep, now).await
    }

    async fn promote_due_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut inner = self.inner.lock().await;
        Ok(Self::promote_due_locked(&mut inner, now))
    }

    async fn recover_stalled_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut inner = self.inner.lock().await;
        let mut recovered = 0;
        let mut remove_ids = Vec::new();

        for job in inner.jobs.values_mut() {
            if job.state != JobState::Active {
                continue;
            }
            let Some(expires_at) = job.lease_expires_at else {
                continue;
            };
            if expires_at > now {
                continue;
            }

            job.stalled_count = job.stalled_count.saturating_add(1);
            job.worker_id = None;
            job.lease_expires_at = None;
            job.failed_reason = Some("job stalled after worker lease expired".to_string());
            if job.stalled_count > job.options.max_stalled_count {
                job.state = JobState::Failed;
                job.finished_at = Some(now);
                if job.options.remove_on_fail {
                    remove_ids.push(job.id.clone());
                }
            } else {
                job.state = JobState::Waiting;
                job.processed_at = None;
            }
            recovered += 1;
        }

        for id in remove_ids {
            inner.jobs.remove(&id);
        }
        Ok(recovered)
    }

    async fn pause(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        inner.paused = true;
        Ok(())
    }

    async fn resume(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        inner.paused = false;
        Ok(())
    }

    async fn get_job(&self, job_id: &str) -> Result<Option<Job>> {
        let inner = self.inner.lock().await;
        Ok(inner.jobs.get(job_id).cloned())
    }

    async fn stats(&self) -> Result<JobQueueStats> {
        let inner = self.inner.lock().await;
        let mut stats = JobQueueStats {
            total: inner.jobs.len(),
            paused: inner.paused,
            ..JobQueueStats::default()
        };
        for job in inner.jobs.values() {
            match job.state {
                JobState::Waiting => stats.waiting += 1,
                JobState::Delayed => stats.delayed += 1,
                JobState::Active => stats.active += 1,
                JobState::WaitingChildren => stats.waiting_children += 1,
                JobState::Completed => stats.completed += 1,
                JobState::Failed => stats.failed += 1,
            }
        }
        Ok(stats)
    }
}
fn compare_claim_order(a: &&Job, b: &&Job) -> Ordering {
    a.priority
        .cmp(&b.priority)
        .then_with(|| a.scheduled_at.cmp(&b.scheduled_at))
        .then_with(|| a.created_at.cmp(&b.created_at))
        .then_with(|| a.id.cmp(&b.id))
}

fn compare_list_order(a: &Job, b: &Job) -> Ordering {
    state_rank(a.state)
        .cmp(&state_rank(b.state))
        .then_with(|| a.priority.cmp(&b.priority))
        .then_with(|| a.scheduled_at.cmp(&b.scheduled_at))
        .then_with(|| a.created_at.cmp(&b.created_at))
        .then_with(|| a.id.cmp(&b.id))
}

fn state_rank(state: JobState) -> u8 {
    match state {
        JobState::Waiting => 0,
        JobState::Delayed => 1,
        JobState::Active => 2,
        JobState::WaitingChildren => 3,
        JobState::Completed => 4,
        JobState::Failed => 5,
    }
}

fn require_active(job: &Job, action: &str) -> Result<()> {
    if job.state == JobState::Active {
        Ok(())
    } else {
        Err(LaneError::JobStateConflict(format!(
            "cannot {action} job {} from state {:?}",
            job.id, job.state
        )))
    }
}

fn require_worker(job: &Job, worker_id: &str) -> Result<()> {
    if job.worker_id.as_deref() == Some(worker_id) {
        Ok(())
    } else {
        Err(LaneError::JobLeaseConflict(format!(
            "worker {worker_id} does not own job {}",
            job.id
        )))
    }
}

fn job_reference_time(job: &Job) -> DateTime<Utc> {
    job.finished_at
        .or(job.processed_at)
        .unwrap_or(job.scheduled_at)
}

fn should_retry(job: &Job) -> bool {
    job.options.retry_policy.max_retries > 0
        && job.attempts_made <= job.options.retry_policy.max_retries
}

fn add_duration(at: DateTime<Utc>, duration: Duration) -> DateTime<Utc> {
    match chrono::Duration::from_std(duration) {
        Ok(delta) => at.checked_add_signed(delta).unwrap_or(at),
        Err(_) => at,
    }
}

fn subtract_duration(at: DateTime<Utc>, duration: Duration) -> DateTime<Utc> {
    match chrono::Duration::from_std(duration) {
        Ok(delta) => at.checked_sub_signed(delta).unwrap_or(at),
        Err(_) => at,
    }
}
