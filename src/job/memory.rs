use super::backend::JobQueueBackend;
use super::types::{
    deduplication_expiration, Job, JobFlow, JobId, JobListOptions, JobListPage, JobLogEntry,
    JobOptions, JobPriority, JobQueueSnapshot, JobQueueStats, JobSpec, JobState, JobWorkerId,
    QueueName,
};
use crate::error::{LaneError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Default)]
struct InMemoryJobQueueState {
    paused: bool,
    jobs: HashMap<JobId, Job>,
    deduplication_next: HashMap<String, Job>,
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
                deduplication_next: snapshot
                    .deduplication_next_jobs
                    .into_iter()
                    .filter_map(|job| {
                        let deduplication_id = job.options.deduplication.as_ref()?.id.clone();
                        Some((deduplication_id, job))
                    })
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
        let mut deduplication_next_jobs = inner
            .deduplication_next
            .values()
            .cloned()
            .collect::<Vec<_>>();
        deduplication_next_jobs.sort_by(compare_list_order);
        JobQueueSnapshot {
            queue: self.queue.clone(),
            paused: inner.paused,
            jobs,
            deduplication_next_jobs,
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
        validate_job_options(&options)?;
        let mut job = Job::new(self.queue.clone(), name.into(), payload, options, now);
        let mut inner = self.inner.lock().await;
        if let Some(existing) = inner.jobs.get(&job.id) {
            return Ok(existing.clone());
        }
        if let Some(existing) = find_active_deduplicated_job(&inner.jobs, &job, now).cloned() {
            if deduplication_replaces_delayed_owner(&job, &existing) {
                preserve_replacement_deduplication_expiration(&mut job, &existing);
                let existing_id = existing.id.clone();
                inner.jobs.remove(&existing_id);
            } else {
                Self::store_deduplicated_next_locked(&mut inner, &job, &existing);
                Self::extend_deduplication_expiration_locked(
                    &mut inner.jobs,
                    &job,
                    &existing.id,
                    now,
                );
                return Ok(inner.jobs.get(&existing.id).cloned().unwrap_or(existing));
            }
        }
        if let Some(existing) = find_active_repeat_job(&inner.jobs, &job) {
            return Ok(existing.clone());
        }
        inner.jobs.insert(job.id.clone(), job.clone());
        Ok(job)
    }

    /// Add multiple jobs using the current wall-clock time.
    pub async fn add_many(&self, jobs: Vec<JobSpec>) -> Result<Vec<Job>> {
        self.add_many_at(jobs, Utc::now()).await
    }

    /// Add multiple jobs at an explicit timestamp.
    pub async fn add_many_at(&self, jobs: Vec<JobSpec>, now: DateTime<Utc>) -> Result<Vec<Job>> {
        for spec in &jobs {
            validate_job_options(&spec.options)?;
        }
        let created = jobs
            .into_iter()
            .map(|spec| {
                Job::new(
                    self.queue.clone(),
                    spec.name,
                    spec.payload,
                    spec.options,
                    now,
                )
            })
            .collect::<Vec<_>>();

        let mut inner = self.inner.lock().await;
        let mut staged: HashMap<JobId, Job> = HashMap::new();
        let mut added = Vec::with_capacity(created.len());
        for mut job in created {
            if let Some(existing) = inner.jobs.get(&job.id) {
                added.push(existing.clone());
                continue;
            }
            if let Some(existing) = staged.get(&job.id) {
                added.push(existing.clone());
                continue;
            }
            if let Some(existing) = find_active_deduplicated_job(&inner.jobs, &job, now).cloned() {
                if deduplication_replaces_delayed_owner(&job, &existing) {
                    preserve_replacement_deduplication_expiration(&mut job, &existing);
                    let existing_id = existing.id.clone();
                    inner.jobs.remove(&existing_id);
                } else {
                    Self::store_deduplicated_next_locked(&mut inner, &job, &existing);
                    Self::extend_deduplication_expiration_locked(
                        &mut inner.jobs,
                        &job,
                        &existing.id,
                        now,
                    );
                    added.push(inner.jobs.get(&existing.id).cloned().unwrap_or(existing));
                    continue;
                }
            }
            if let Some(existing) = find_active_deduplicated_job(&staged, &job, now).cloned() {
                if deduplication_replaces_delayed_owner(&job, &existing) {
                    preserve_replacement_deduplication_expiration(&mut job, &existing);
                    let existing_id = existing.id.clone();
                    staged.remove(&existing_id);
                } else {
                    Self::extend_deduplication_expiration_locked(
                        &mut staged,
                        &job,
                        &existing.id,
                        now,
                    );
                    added.push(staged.get(&existing.id).cloned().unwrap_or(existing));
                    continue;
                }
            }
            if let Some(existing) = find_active_repeat_job(&inner.jobs, &job)
                .or_else(|| find_active_repeat_job(&staged, &job))
            {
                added.push(existing.clone());
                continue;
            }
            staged.insert(job.id.clone(), job.clone());
            added.push(job);
        }

        inner.jobs.extend(staged);
        Ok(added)
    }

    /// Add a parent-child flow. The parent remains blocked until every child is
    /// completed; a terminal child failure fails the parent.
    pub async fn add_flow(&self, parent: JobSpec, children: Vec<JobSpec>) -> Result<JobFlow> {
        self.add_flow_at(parent, children, Utc::now()).await
    }

    /// Add a parent-child flow at an explicit timestamp.
    pub async fn add_flow_at(
        &self,
        parent: JobSpec,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<JobFlow> {
        validate_job_options(&parent.options)?;
        for child in &children {
            validate_job_options(&child.options)?;
        }
        let mut parent_job = Job::new(
            self.queue.clone(),
            parent.name,
            parent.payload,
            parent.options,
            now,
        );
        parent_job.state = if children.is_empty() {
            state_after_dependencies(parent_job.scheduled_at, now)
        } else {
            JobState::WaitingChildren
        };

        let mut child_jobs = Vec::with_capacity(children.len());
        for child in children {
            let mut child_job = Job::new(
                self.queue.clone(),
                child.name,
                child.payload,
                child.options,
                now,
            );
            child_job.parent_id = Some(parent_job.id.clone());
            parent_job.child_ids.push(child_job.id.clone());
            child_jobs.push(child_job);
        }
        validate_flow_job_ids(&parent_job, &child_jobs)?;

        let mut inner = self.inner.lock().await;
        for id in std::iter::once(&parent_job.id).chain(child_jobs.iter().map(|job| &job.id)) {
            if inner.jobs.contains_key(id) {
                return Err(LaneError::ConfigError(format!(
                    "flow job id `{id}` already exists"
                )));
            }
        }
        let mut flow_deduplication_ids = HashSet::new();
        for job in std::iter::once(&parent_job).chain(child_jobs.iter()) {
            if let Some(deduplication_id) = active_deduplication_id(job, now) {
                if find_active_deduplication_id(&inner.jobs, deduplication_id, now).is_some()
                    || !flow_deduplication_ids.insert(deduplication_id.to_string())
                {
                    return Err(LaneError::ConfigError(format!(
                        "flow deduplication id `{deduplication_id}` already active"
                    )));
                }
            }
        }
        let mut flow_repeat_keys = HashSet::new();
        for job in std::iter::once(&parent_job).chain(child_jobs.iter()) {
            if let Some(repeat_key) = active_repeat_key(job) {
                if find_active_repeat_key(&inner.jobs, repeat_key).is_some()
                    || !flow_repeat_keys.insert(repeat_key.to_string())
                {
                    return Err(LaneError::ConfigError(format!(
                        "flow repeat key `{repeat_key}` already active"
                    )));
                }
            }
        }
        inner.jobs.insert(parent_job.id.clone(), parent_job.clone());
        for child in &child_jobs {
            inner.jobs.insert(child.id.clone(), child.clone());
        }

        Ok(JobFlow {
            parent: parent_job,
            children: child_jobs,
        })
    }

    /// Remove a job from the queue.
    pub async fn remove(&self, job_id: &str) -> Result<Option<Job>> {
        let mut inner = self.inner.lock().await;
        if let Some(job) = inner.jobs.get(job_id) {
            require_removable(job)?;
        }
        let removed = inner.jobs.remove(job_id);
        if let Some(parent_id) = removed.as_ref().and_then(|job| job.parent_id.clone()) {
            Self::release_parent_if_ready_locked(&mut inner, &parent_id, Utc::now());
        }
        Ok(removed)
    }

    /// Remove the current non-terminal occurrence for a repeat series.
    pub async fn remove_repeat(&self, repeat_key: &str) -> Result<Option<Job>> {
        let mut inner = self.inner.lock().await;
        let Some(job_id) =
            find_active_repeat_key(&inner.jobs, repeat_key).map(|job| job.id.clone())
        else {
            return Ok(None);
        };
        if let Some(job) = inner.jobs.get(&job_id) {
            require_removable(job)?;
        }
        let removed = inner.jobs.remove(&job_id);
        if let Some(parent_id) = removed.as_ref().and_then(|job| job.parent_id.clone()) {
            Self::release_parent_if_ready_locked(&mut inner, &parent_id, Utc::now());
        }
        Ok(removed)
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
        let current = inner
            .jobs
            .get(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        if current.state != JobState::Failed {
            return Err(LaneError::JobStateConflict(format!(
                "cannot retry job {} from state {:?}",
                current.id, current.state
            )));
        }
        if let Some(deduplication_id) = current
            .options
            .deduplication
            .as_ref()
            .map(|value| &value.id)
        {
            if let Some(existing) =
                find_active_deduplication_id_except(&inner.jobs, deduplication_id, job_id, now)
            {
                return Err(LaneError::JobStateConflict(format!(
                    "cannot retry job {job_id}; deduplication id `{deduplication_id}` is active on job {}",
                    existing.id
                )));
            }
        }
        if let Some(repeat_key) = current.repeat_key.as_deref() {
            if let Some(existing) = find_active_repeat_key_except(&inner.jobs, repeat_key, job_id) {
                return Err(LaneError::JobStateConflict(format!(
                    "cannot retry job {job_id}; repeat key `{repeat_key}` is active on job {}",
                    existing.id
                )));
            }
        }

        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        job.state = JobState::Waiting;
        job.scheduled_at = now;
        job.deduplication_expires_at = deduplication_expiration(&job.options, now);
        job.processed_at = None;
        job.finished_at = None;
        job.worker_id = None;
        job.lock_token = None;
        job.lease_expires_at = None;
        job.failed_reason = None;
        Ok(job.clone())
    }

    /// Renew the active worker lease for a job.
    pub async fn renew(
        &self,
        job_id: &str,
        lock_token: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        require_active(job, "renew lease")?;
        require_lock_token(job, lock_token)?;
        job.lease_expires_at = Some(add_duration(now, lease_for));
        Ok(job.clone())
    }

    /// Update a non-terminal job priority.
    pub async fn set_priority(&self, job_id: &str, priority: JobPriority) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        if job.state.is_terminal() {
            return Err(LaneError::JobStateConflict(format!(
                "cannot update priority for terminal job {}",
                job.id
            )));
        }
        job.priority = priority;
        job.options.priority = priority;
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

        let parent_ids = jobs
            .iter()
            .filter_map(|job| job.parent_id.clone())
            .collect::<Vec<_>>();

        for job in &jobs {
            inner.jobs.remove(&job.id);
        }
        for parent_id in parent_ids {
            Self::release_parent_if_ready_locked(&mut inner, &parent_id, now);
        }

        Ok(jobs)
    }

    /// Drain waiting jobs and optionally non-repeat delayed jobs.
    pub async fn drain(&self, include_delayed: bool) -> Result<Vec<Job>> {
        let mut inner = self.inner.lock().await;
        let mut jobs = inner
            .jobs
            .values()
            .filter(|job| {
                job.state == JobState::Waiting
                    || (include_delayed
                        && job.state == JobState::Delayed
                        && !is_delayed_repeat_owner(&inner.jobs, job))
            })
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by(compare_list_order);

        let parent_ids = jobs
            .iter()
            .filter_map(|job| job.parent_id.clone())
            .collect::<Vec<_>>();

        for job in &jobs {
            inner.jobs.remove(&job.id);
        }
        for parent_id in parent_ids {
            Self::release_parent_if_ready_locked(&mut inner, &parent_id, Utc::now());
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

    fn store_deduplicated_next_locked(
        inner: &mut InMemoryJobQueueState,
        candidate: &Job,
        existing: &Job,
    ) -> bool {
        if !deduplication_stores_next_if_active(candidate, existing) {
            return false;
        }
        let Some(deduplication_id) = job_deduplication_id(candidate) else {
            return false;
        };
        inner
            .deduplication_next
            .insert(deduplication_id.to_string(), candidate.clone());
        if let Some(owner) = inner.jobs.get_mut(&existing.id) {
            owner.deduplication_expires_at = None;
        }
        true
    }

    fn enqueue_deduplicated_next_locked(
        inner: &mut InMemoryJobQueueState,
        owner: &Job,
        now: DateTime<Utc>,
    ) -> Option<Job> {
        let deduplication_id = job_deduplication_id(owner)?;
        let mut next = inner.deduplication_next.remove(deduplication_id)?;
        prepare_deduplicated_next_job(&mut next, now);
        if inner.jobs.contains_key(&next.id) {
            return None;
        }
        inner.jobs.insert(next.id.clone(), next.clone());
        Some(next)
    }

    fn extend_deduplication_expiration_locked(
        jobs: &mut HashMap<JobId, Job>,
        candidate: &Job,
        existing_id: &str,
        now: DateTime<Utc>,
    ) -> bool {
        if !deduplication_extends_ttl(candidate) {
            return false;
        }
        let Some(existing) = jobs.get_mut(existing_id) else {
            return false;
        };
        existing.deduplication_expires_at = deduplication_expiration(&candidate.options, now);
        true
    }

    fn release_parent_if_ready_locked(
        inner: &mut InMemoryJobQueueState,
        parent_id: &str,
        now: DateTime<Utc>,
    ) -> Option<Job> {
        let parent = inner.jobs.get(parent_id)?;
        if parent.state != JobState::WaitingChildren {
            return Some(parent.clone());
        }

        let child_ids = parent.child_ids.clone();
        let mut child_failure = None;
        for child_id in &child_ids {
            let Some(child) = inner.jobs.get(child_id) else {
                continue;
            };
            match child.state {
                JobState::Completed => {}
                JobState::Failed => {
                    child_failure = Some((child.id.clone(), child.failed_reason.clone()))
                }
                _ => return Some(parent.clone()),
            }
        }
        if let Some((child_id, reason)) = child_failure {
            return Self::fail_waiting_parent_locked(
                inner,
                parent_id,
                format!(
                    "child job {child_id} failed: {}",
                    reason.as_deref().unwrap_or("unknown error")
                ),
                now,
            );
        }

        let parent = inner.jobs.get_mut(parent_id)?;
        parent.state = state_after_dependencies(parent.scheduled_at, now);
        parent.processed_at = None;
        parent.finished_at = None;
        parent.worker_id = None;
        parent.lock_token = None;
        parent.lease_expires_at = None;
        parent.failed_reason = None;
        Some(parent.clone())
    }

    fn fail_waiting_parent_locked(
        inner: &mut InMemoryJobQueueState,
        parent_id: &str,
        reason: String,
        now: DateTime<Utc>,
    ) -> Option<Job> {
        let parent = inner.jobs.get_mut(parent_id)?;
        if parent.state.is_terminal() {
            return Some(parent.clone());
        }
        parent.state = JobState::Failed;
        parent.finished_at = Some(now);
        parent.worker_id = None;
        parent.lock_token = None;
        parent.lease_expires_at = None;
        parent.failed_reason = Some(reason);
        let failed = parent.clone();
        if failed.options.remove_on_fail {
            inner.jobs.remove(parent_id);
        }
        Some(failed)
    }
}
#[async_trait]
impl JobQueueBackend for InMemoryJobQueue {
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
        job.lock_token = Some(Uuid::new_v4().to_string());
        job.lease_expires_at = Some(add_duration(now, lease_for));
        job.failed_reason = None;
        Ok(Some(job.clone()))
    }

    async fn complete_job(
        &self,
        job_id: &str,
        lock_token: &str,
        value: Value,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let completed = {
            let job = inner
                .jobs
                .get_mut(job_id)
                .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
            require_active(job, "complete")?;
            require_lock_token(job, lock_token)?;
            job.state = JobState::Completed;
            job.finished_at = Some(now);
            job.worker_id = None;
            job.lock_token = None;
            job.lease_expires_at = None;
            job.return_value = Some(value);
            job.clone()
        };
        if completed.options.remove_on_complete {
            inner.jobs.remove(job_id);
        }
        if let Some(next_job) = next_repeat_job(&completed, now)? {
            inner.jobs.insert(next_job.id.clone(), next_job);
        }
        Self::enqueue_deduplicated_next_locked(&mut inner, &completed, now);
        if let Some(parent_id) = &completed.parent_id {
            Self::release_parent_if_ready_locked(&mut inner, parent_id, now);
        }
        Ok(completed)
    }

    async fn fail_job(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let failed = {
            let job = inner
                .jobs
                .get_mut(job_id)
                .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
            require_active(job, "fail")?;
            require_lock_token(job, lock_token)?;
            job.worker_id = None;
            job.lock_token = None;
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

            job.clone()
        };
        if failed.state == JobState::Failed && failed.options.remove_on_fail {
            inner.jobs.remove(job_id);
        }
        if failed.state == JobState::Failed {
            Self::enqueue_deduplicated_next_locked(&mut inner, &failed, now);
            if let Some(parent_id) = &failed.parent_id {
                Self::fail_waiting_parent_locked(
                    &mut inner,
                    parent_id,
                    format!(
                        "child job {} failed: {}",
                        failed.id,
                        failed.failed_reason.as_deref().unwrap_or("unknown error")
                    ),
                    now,
                );
            }
        }
        Ok(failed)
    }

    async fn renew_lease(
        &self,
        job_id: &str,
        lock_token: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        self.renew(job_id, lock_token, lease_for, now).await
    }

    async fn promote_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        self.promote(job_id, now).await
    }

    async fn retry_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        self.retry(job_id, now).await
    }

    async fn update_priority(&self, job_id: &str, priority: JobPriority) -> Result<Job> {
        self.set_priority(job_id, priority).await
    }

    async fn remove_job(&self, job_id: &str) -> Result<Option<Job>> {
        self.remove(job_id).await
    }

    async fn remove_repeat(&self, repeat_key: &str) -> Result<Option<Job>> {
        InMemoryJobQueue::remove_repeat(self, repeat_key).await
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

    async fn drain_jobs(&self, include_delayed: bool) -> Result<Vec<Job>> {
        self.drain(include_delayed).await
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
        let mut failed_children = Vec::new();
        let mut terminal_failures = Vec::new();

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
            job.lock_token = None;
            job.lease_expires_at = None;
            job.failed_reason = Some("job stalled after worker lease expired".to_string());
            if job.stalled_count > job.options.max_stalled_count {
                job.state = JobState::Failed;
                job.finished_at = Some(now);
                if let Some(parent_id) = &job.parent_id {
                    failed_children.push((
                        parent_id.clone(),
                        job.id.clone(),
                        job.failed_reason.clone(),
                    ));
                }
                terminal_failures.push(job.clone());
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
        for failed in terminal_failures {
            Self::enqueue_deduplicated_next_locked(&mut inner, &failed, now);
        }
        for (parent_id, child_id, reason) in failed_children {
            Self::fail_waiting_parent_locked(
                &mut inner,
                &parent_id,
                format!(
                    "child job {child_id} failed: {}",
                    reason.as_deref().unwrap_or("unknown error")
                ),
                now,
            );
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

fn active_deduplication_id(job: &Job, now: DateTime<Utc>) -> Option<&str> {
    if job.state.is_terminal() {
        return None;
    }
    if matches!(job.deduplication_expires_at, Some(expires_at) if expires_at <= now) {
        return None;
    }

    job.options
        .deduplication
        .as_ref()
        .map(|deduplication| deduplication.id.as_str())
}

fn job_deduplication_id(job: &Job) -> Option<&str> {
    job.options
        .deduplication
        .as_ref()
        .map(|deduplication| deduplication.id.as_str())
}

fn find_active_deduplicated_job<'a>(
    jobs: &'a HashMap<JobId, Job>,
    candidate: &Job,
    now: DateTime<Utc>,
) -> Option<&'a Job> {
    let deduplication_id = active_deduplication_id(candidate, now)?;
    find_active_deduplication_id(jobs, deduplication_id, now)
}

fn find_active_deduplication_id<'a>(
    jobs: &'a HashMap<JobId, Job>,
    deduplication_id: &str,
    now: DateTime<Utc>,
) -> Option<&'a Job> {
    jobs.values()
        .find(|job| active_deduplication_id(job, now) == Some(deduplication_id))
}

fn find_active_deduplication_id_except<'a>(
    jobs: &'a HashMap<JobId, Job>,
    deduplication_id: &str,
    excluded_job_id: &str,
    now: DateTime<Utc>,
) -> Option<&'a Job> {
    jobs.values().find(|job| {
        job.id != excluded_job_id && active_deduplication_id(job, now) == Some(deduplication_id)
    })
}

fn deduplication_replaces_delayed_owner(candidate: &Job, existing: &Job) -> bool {
    matches!(
        candidate
            .options
            .deduplication
            .as_ref()
            .map(|deduplication| deduplication.replace),
        Some(true)
    ) && existing.state == JobState::Delayed
        && candidate.repeat_key.is_none()
        && existing.parent_id.is_none()
        && existing.child_ids.is_empty()
        && existing.repeat_key.is_none()
}

fn deduplication_stores_next_if_active(candidate: &Job, existing: &Job) -> bool {
    matches!(
        candidate
            .options
            .deduplication
            .as_ref()
            .map(|deduplication| deduplication.keep_last_if_active),
        Some(true)
    ) && existing.state == JobState::Active
        && candidate.parent_id.is_none()
        && candidate.child_ids.is_empty()
        && candidate.repeat_key.is_none()
}

fn deduplication_extends_ttl(candidate: &Job) -> bool {
    matches!(
        candidate.options.deduplication.as_ref(),
        Some(deduplication)
            if deduplication.extend
                && deduplication.ttl.is_some()
                && !deduplication.keep_last_if_active
    )
}

fn preserve_replacement_deduplication_expiration(candidate: &mut Job, existing: &Job) {
    let preserves_ttl = candidate
        .options
        .deduplication
        .as_ref()
        .and_then(|deduplication| deduplication.ttl)
        .is_some();
    if preserves_ttl {
        candidate.deduplication_expires_at = existing.deduplication_expires_at;
    }
}

fn prepare_deduplicated_next_job(job: &mut Job, now: DateTime<Utc>) {
    job.created_at = now;
    job.scheduled_at = job
        .options
        .delay
        .map(|delay| add_duration(now, delay))
        .unwrap_or(now);
    job.state = state_after_dependencies(job.scheduled_at, now);
    job.attempts_made = 0;
    job.stalled_count = 0;
    job.processed_at = None;
    job.finished_at = None;
    job.worker_id = None;
    job.lock_token = None;
    job.lease_expires_at = None;
    job.failed_reason = None;
    job.return_value = None;
    job.progress = None;
    job.logs.clear();
    job.deduplication_expires_at = deduplication_expiration(&job.options, now);
}

fn active_repeat_key(job: &Job) -> Option<&str> {
    if job.state.is_terminal() {
        return None;
    }

    job.repeat_key.as_deref()
}

fn find_active_repeat_job<'a>(jobs: &'a HashMap<JobId, Job>, candidate: &Job) -> Option<&'a Job> {
    let repeat_key = active_repeat_key(candidate)?;
    find_active_repeat_key(jobs, repeat_key)
}

fn find_active_repeat_key<'a>(jobs: &'a HashMap<JobId, Job>, repeat_key: &str) -> Option<&'a Job> {
    jobs.values()
        .find(|job| active_repeat_key(job) == Some(repeat_key))
}

fn find_active_repeat_key_except<'a>(
    jobs: &'a HashMap<JobId, Job>,
    repeat_key: &str,
    excluded_job_id: &str,
) -> Option<&'a Job> {
    jobs.values()
        .find(|job| job.id != excluded_job_id && active_repeat_key(job) == Some(repeat_key))
}

fn is_delayed_repeat_owner(jobs: &HashMap<JobId, Job>, job: &Job) -> bool {
    if job.state != JobState::Delayed {
        return false;
    }
    let Some(repeat_key) = job.repeat_key.as_deref() else {
        return false;
    };
    find_active_repeat_key(jobs, repeat_key).is_some_and(|owner| owner.id == job.id)
}

fn state_after_dependencies(scheduled_at: DateTime<Utc>, now: DateTime<Utc>) -> JobState {
    if scheduled_at > now {
        JobState::Delayed
    } else {
        JobState::Waiting
    }
}

fn validate_job_options(options: &JobOptions) -> Result<()> {
    options.validate()
}

fn validate_flow_job_ids(parent: &Job, children: &[Job]) -> Result<()> {
    let mut ids = HashSet::with_capacity(children.len() + 1);
    for id in std::iter::once(&parent.id).chain(children.iter().map(|job| &job.id)) {
        if !ids.insert(id) {
            return Err(LaneError::ConfigError(format!(
                "flow contains duplicate job id `{id}`"
            )));
        }
    }
    Ok(())
}

fn next_repeat_job(job: &Job, now: DateTime<Utc>) -> Result<Option<Job>> {
    let Some(repeat) = job.options.repeat.as_ref() else {
        return Ok(None);
    };
    let next_count = job.repeat_count.saturating_add(1);
    if matches!(repeat.limit, Some(limit) if next_count >= limit) {
        return Ok(None);
    }

    let Some(scheduled_at) = repeat.next_scheduled_at(now)? else {
        return Ok(None);
    };
    if matches!(repeat.end_at, Some(end_at) if scheduled_at > end_at) {
        return Ok(None);
    }

    let mut options = job.options.clone();
    options.job_id = None;
    let mut next = Job::new(
        job.queue.clone(),
        job.name.clone(),
        job.payload.clone(),
        options,
        now,
    );
    next.scheduled_at = scheduled_at;
    next.state = state_after_dependencies(scheduled_at, now);
    next.repeat_key = job.repeat_key.clone();
    next.repeat_count = next_count;
    Ok(Some(next))
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

fn require_lock_token(job: &Job, lock_token: &str) -> Result<()> {
    if job.lock_token.as_deref() == Some(lock_token) {
        Ok(())
    } else {
        Err(LaneError::JobLeaseConflict(format!(
            "lock token does not own job {}",
            job.id
        )))
    }
}

fn require_removable(job: &Job) -> Result<()> {
    if job.state == JobState::Active {
        Err(LaneError::JobLeaseConflict(format!(
            "cannot remove active leased job {}",
            job.id
        )))
    } else {
        Ok(())
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
