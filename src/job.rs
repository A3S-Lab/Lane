//! Generic job runtime primitives for distributed priority queues.
//!
//! The existing lane scheduler executes in-process [`Command`](crate::Command)
//! values. This module is the durable job-queue foundation: jobs are plain JSON
//! payloads with explicit lifecycle state, priority ordering, delayed
//! scheduling, worker leases, retries, and stalled-job recovery. Storage-backed
//! implementations can implement [`JobQueueBackend`] while the in-memory backend
//! provides deterministic behavior for local runtimes and tests.

use crate::error::{LaneError, Result};
use crate::retry::RetryPolicy;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Unique identifier for a generic queue job.
pub type JobId = String;

/// Queue name for a generic job queue.
pub type QueueName = String;

/// Worker identifier used for leased processing.
pub type JobWorkerId = String;

/// Job priority. Lower values run first.
pub type JobPriority = u32;

/// Default priority for jobs that do not specify one.
pub const DEFAULT_JOB_PRIORITY: JobPriority = 1000;

/// Lifecycle state for a durable job.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Ready to be claimed by a worker.
    Waiting,
    /// Scheduled for the future.
    Delayed,
    /// Leased to a worker and currently processing.
    Active,
    /// Parent job waiting for children to finish.
    WaitingChildren,
    /// Finished successfully.
    Completed,
    /// Finished with a terminal failure.
    Failed,
}

impl JobState {
    /// Whether this state is terminal and should not be claimed again.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// Options used when adding a generic queue job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobOptions {
    /// Lower values run before higher values.
    pub priority: JobPriority,
    /// Optional delay before the job becomes claimable.
    pub delay: Option<Duration>,
    /// Retry policy used after processing failure.
    pub retry_policy: RetryPolicy,
    /// Optional execution timeout hint for workers.
    pub timeout: Option<Duration>,
    /// Remove the job record after successful completion.
    pub remove_on_complete: bool,
    /// Remove the job record after terminal failure.
    pub remove_on_fail: bool,
    /// Number of lease expirations tolerated before terminal failure.
    pub max_stalled_count: u32,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            priority: DEFAULT_JOB_PRIORITY,
            delay: None,
            retry_policy: RetryPolicy::none(),
            timeout: None,
            remove_on_complete: false,
            remove_on_fail: false,
            max_stalled_count: 1,
        }
    }
}

impl JobOptions {
    /// Create default job options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set job priority. Lower values run first.
    pub fn with_priority(mut self, priority: JobPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Delay the job before it can be claimed.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Set retry behavior for processing failures.
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Set an execution timeout hint for workers.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Configure whether completed jobs are retained.
    pub fn remove_on_complete(mut self, remove: bool) -> Self {
        self.remove_on_complete = remove;
        self
    }

    /// Configure whether failed jobs are retained.
    pub fn remove_on_fail(mut self, remove: bool) -> Self {
        self.remove_on_fail = remove;
        self
    }

    /// Configure stalled-job tolerance.
    pub fn with_max_stalled_count(mut self, count: u32) -> Self {
        self.max_stalled_count = count;
        self
    }
}

/// Durable generic job record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: JobId,
    pub queue: QueueName,
    pub name: String,
    pub payload: Value,
    pub options: JobOptions,
    pub priority: JobPriority,
    pub state: JobState,
    pub attempts_made: u32,
    pub stalled_count: u32,
    pub created_at: DateTime<Utc>,
    pub scheduled_at: DateTime<Utc>,
    pub processed_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub worker_id: Option<JobWorkerId>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub failed_reason: Option<String>,
    pub return_value: Option<Value>,
    pub parent_id: Option<JobId>,
    pub child_ids: Vec<JobId>,
}

impl Job {
    fn new(
        queue: QueueName,
        name: String,
        payload: Value,
        options: JobOptions,
        now: DateTime<Utc>,
    ) -> Self {
        let scheduled_at = options
            .delay
            .map(|delay| add_duration(now, delay))
            .unwrap_or(now);
        let state = if scheduled_at > now {
            JobState::Delayed
        } else {
            JobState::Waiting
        };

        Self {
            id: Uuid::new_v4().to_string(),
            queue,
            name,
            payload,
            priority: options.priority,
            options,
            state,
            attempts_made: 0,
            stalled_count: 0,
            created_at: now,
            scheduled_at,
            processed_at: None,
            finished_at: None,
            worker_id: None,
            lease_expires_at: None,
            failed_reason: None,
            return_value: None,
            parent_id: None,
            child_ids: Vec::new(),
        }
    }
}

/// Queue state counts for generic jobs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobQueueStats {
    pub total: usize,
    pub waiting: usize,
    pub delayed: usize,
    pub active: usize,
    pub waiting_children: usize,
    pub completed: usize,
    pub failed: usize,
    pub paused: bool,
}

/// Backend contract for a durable distributed job queue.
#[async_trait]
pub trait JobQueueBackend: Send + Sync {
    async fn add_job(&self, name: String, payload: Value, options: JobOptions) -> Result<Job>;

    async fn claim_next(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>>;

    async fn complete_job(&self, job_id: &str, value: Value, now: DateTime<Utc>) -> Result<Job>;

    async fn fail_job(&self, job_id: &str, error: String, now: DateTime<Utc>) -> Result<Job>;

    async fn promote_due_jobs(&self, now: DateTime<Utc>) -> Result<usize>;

    async fn recover_stalled_jobs(&self, now: DateTime<Utc>) -> Result<usize>;

    async fn pause(&self) -> Result<()>;

    async fn resume(&self) -> Result<()>;

    async fn get_job(&self, job_id: &str) -> Result<Option<Job>>;

    async fn stats(&self) -> Result<JobQueueStats>;
}

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

    /// Queue name.
    pub fn queue_name(&self) -> &str {
        &self.queue
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
        Ok(Some(job.clone()))
    }

    async fn complete_job(&self, job_id: &str, value: Value, now: DateTime<Utc>) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(ms).unwrap()
    }

    #[tokio::test]
    async fn claims_waiting_jobs_by_priority_then_fifo() {
        let queue = InMemoryJobQueue::new("email");
        let now = ts(1_000);
        let low = queue
            .add_at(
                "low",
                serde_json::json!({"n": 1}),
                JobOptions::new().with_priority(50),
                now,
            )
            .await
            .unwrap();
        let high = queue
            .add_at(
                "high",
                serde_json::json!({"n": 2}),
                JobOptions::new().with_priority(5),
                now,
            )
            .await
            .unwrap();

        let claimed = queue
            .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, high.id);
        assert_eq!(claimed.state, JobState::Active);
        assert_eq!(claimed.worker_id.as_deref(), Some("worker-a"));

        let claimed = queue
            .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, low.id);
    }

    #[tokio::test]
    async fn delayed_jobs_wait_until_due() {
        let queue = InMemoryJobQueue::new("reports");
        let now = ts(1_000);
        let job = queue
            .add_at(
                "generate",
                serde_json::json!({}),
                JobOptions::new()
                    .with_priority(1)
                    .with_delay(Duration::from_secs(5)),
                now,
            )
            .await
            .unwrap();
        assert_eq!(job.state, JobState::Delayed);

        let early = queue
            .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(2_000))
            .await
            .unwrap();
        assert!(early.is_none());

        assert_eq!(queue.promote_due_jobs(ts(6_000)).await.unwrap(), 1);
        let due = queue
            .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(6_000))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(due.id, job.id);
    }

    #[tokio::test]
    async fn failed_jobs_retry_with_backoff_then_terminal_failure() {
        let queue = InMemoryJobQueue::new("webhooks");
        let now = ts(1_000);
        let job = queue
            .add_at(
                "deliver",
                serde_json::json!({}),
                JobOptions::new().with_retry_policy(RetryPolicy::fixed(1, Duration::from_secs(2))),
                now,
            )
            .await
            .unwrap();

        let first = queue
            .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.id, job.id);

        let retry = queue
            .fail_job(&job.id, "network".to_string(), ts(1_100))
            .await
            .unwrap();
        assert_eq!(retry.state, JobState::Delayed);
        assert_eq!(retry.scheduled_at, ts(3_100));

        let second = queue
            .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(3_100))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.attempts_made, 2);

        let failed = queue
            .fail_job(&job.id, "still down".to_string(), ts(3_200))
            .await
            .unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.failed_reason.as_deref(), Some("still down"));
    }

    #[tokio::test]
    async fn stalled_jobs_are_recovered_until_limit() {
        let queue = InMemoryJobQueue::new("video");
        let now = ts(1_000);
        let job = queue
            .add_at(
                "transcode",
                serde_json::json!({}),
                JobOptions::new().with_max_stalled_count(1),
                now,
            )
            .await
            .unwrap();
        queue
            .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
            .await
            .unwrap();

        assert_eq!(queue.recover_stalled_jobs(ts(2_001)).await.unwrap(), 1);
        let recovered = queue.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(recovered.state, JobState::Waiting);
        assert_eq!(recovered.stalled_count, 1);

        queue
            .claim_next("worker-b".to_string(), Duration::from_secs(1), ts(2_100))
            .await
            .unwrap();
        assert_eq!(queue.recover_stalled_jobs(ts(3_200)).await.unwrap(), 1);
        let failed = queue.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.stalled_count, 2);
    }

    #[tokio::test]
    async fn pause_blocks_claiming_without_rejecting_adds() {
        let queue = InMemoryJobQueue::new("paused");
        let now = ts(1_000);
        queue.pause().await.unwrap();
        queue
            .add_at("task", serde_json::json!({}), JobOptions::new(), now)
            .await
            .unwrap();
        assert!(queue
            .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
            .await
            .unwrap()
            .is_none());

        let stats = queue.stats().await.unwrap();
        assert!(stats.paused);
        assert_eq!(stats.waiting, 1);

        queue.resume().await.unwrap();
        assert!(queue
            .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn remove_on_complete_deletes_record_after_returning_snapshot() {
        let queue = InMemoryJobQueue::new("cleanup");
        let now = ts(1_000);
        let job = queue
            .add_at(
                "task",
                serde_json::json!({}),
                JobOptions::new().remove_on_complete(true),
                now,
            )
            .await
            .unwrap();
        queue
            .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
            .await
            .unwrap();

        let completed = queue
            .complete_job(&job.id, serde_json::json!({"ok": true}), ts(1_100))
            .await
            .unwrap();
        assert_eq!(completed.state, JobState::Completed);
        assert!(queue.get_job(&job.id).await.unwrap().is_none());
    }
}
