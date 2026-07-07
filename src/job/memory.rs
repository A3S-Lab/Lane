use super::backend::JobQueueBackend;
use super::types::{
    deduplication_expiration, page_repeat_entries, Job, JobEvent, JobFlow, JobFlowChildValues,
    JobFlowDependencies, JobFlowDependencyCounts, JobFlowIgnoredFailures, JobId, JobListOptions,
    JobListPage, JobLogEntry, JobLogPage, JobOptions, JobPriority, JobPriorityCount,
    JobQueueSnapshot, JobQueueStats, JobRepeatEntry, JobRepeatListOptions, JobRepeatPage,
    JobRetention, JobSpec, JobState, JobStateCount, JobWorkerId, QueueName,
    DEFAULT_JOB_EVENT_RETENTION,
};
use crate::error::{LaneError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Default)]
struct InMemoryJobQueueState {
    paused: bool,
    sequence: u64,
    event_sequence: u64,
    events: Vec<JobEvent>,
    jobs: HashMap<JobId, Job>,
    deduplication_next: HashMap<String, Job>,
    released_deduplication_owners: HashSet<(String, JobId)>,
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
        let sequence = snapshot
            .jobs
            .iter()
            .chain(snapshot.deduplication_next_jobs.iter())
            .map(|job| job.enqueued_seq)
            .max()
            .unwrap_or(0);
        Self {
            queue: snapshot.queue,
            inner: Arc::new(Mutex::new(InMemoryJobQueueState {
                paused: snapshot.paused,
                event_sequence: snapshot.event_sequence,
                events: snapshot.events,
                sequence,
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
                released_deduplication_owners: snapshot
                    .released_deduplication_owners
                    .into_iter()
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
            events: inner.events.clone(),
            event_sequence: inner.event_sequence,
            jobs,
            deduplication_next_jobs,
            released_deduplication_owners: sorted_released_deduplication_owners(
                &inner.released_deduplication_owners,
            ),
        }
    }

    fn emit_job_created_events_locked(
        inner: &mut InMemoryJobQueueState,
        job: &Job,
        now: DateTime<Utc>,
    ) {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), Value::String(job.name.clone()));
        emit_event_locked(inner, "added", Some(job), None, now, fields);
        emit_event_locked(
            inner,
            job_event_for_state(job.state),
            Some(job),
            None,
            now,
            BTreeMap::new(),
        );
    }

    async fn fail_active_job(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
        allow_retry: bool,
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
            job.deferred_failure = None;
            job.failed_reason = Some(error);

            if allow_retry && should_retry(job) {
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
        if failed.state == JobState::Failed {
            Self::forget_released_deduplication_owner_locked(&mut inner, &failed);
            if failed.options.removes_failed_immediately() {
                Self::remove_job_record_locked(&mut inner, job_id);
            } else if let Some(retention) = failed.options.failed_retention() {
                Self::apply_finished_retention_locked(&mut inner, JobState::Failed, retention, now);
            }
            Self::enqueue_deduplicated_next_locked(&mut inner, &failed, now);
            if let Some(parent_id) = &failed.parent_id {
                if failed.options.fail_parent_on_failure {
                    Self::defer_parent_failure_after_child_failure_locked(
                        &mut inner, parent_id, &failed.id, now,
                    );
                } else if failed.options.continue_parent_on_failure {
                    Self::continue_parent_after_child_failure_locked(&mut inner, parent_id, now);
                } else if child_failure_releases_dependency(&failed) {
                    Self::release_parent_if_ready_locked(&mut inner, parent_id, now);
                } else {
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
        }
        let mut fields = BTreeMap::new();
        if let Some(reason) = &failed.failed_reason {
            fields.insert("failedReason".to_string(), Value::String(reason.clone()));
        }
        emit_event_locked(
            &mut inner,
            job_event_for_state(failed.state),
            Some(&failed),
            Some(JobState::Active),
            now,
            fields,
        );
        Ok(failed)
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
        if let Some(existing) = find_active_deduplicated_job(
            &inner.jobs,
            &inner.released_deduplication_owners,
            &job,
            now,
        )
        .cloned()
        {
            if deduplication_replaces_delayed_owner(&job, &existing) {
                preserve_replacement_deduplication_expiration(&mut job, &existing);
                let existing_id = existing.id.clone();
                Self::remove_job_record_locked(&mut inner, &existing_id);
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
        Self::forget_released_deduplication_owner_locked(&mut inner, &job);
        assign_waiting_order(&mut inner.sequence, &mut job);
        inner.jobs.insert(job.id.clone(), job.clone());
        Self::emit_job_created_events_locked(&mut inner, &job, now);
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
            if let Some(existing) = find_active_deduplicated_job(
                &inner.jobs,
                &inner.released_deduplication_owners,
                &job,
                now,
            )
            .cloned()
            {
                if deduplication_replaces_delayed_owner(&job, &existing) {
                    preserve_replacement_deduplication_expiration(&mut job, &existing);
                    let existing_id = existing.id.clone();
                    Self::remove_job_record_locked(&mut inner, &existing_id);
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
            if let Some(existing) =
                find_active_deduplicated_job(&staged, &HashSet::new(), &job, now).cloned()
            {
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
            assign_waiting_order(&mut inner.sequence, &mut job);
            staged.insert(job.id.clone(), job.clone());
            added.push(job);
        }

        for (job_id, job) in staged {
            Self::forget_released_deduplication_owner_locked(&mut inner, &job);
            inner.jobs.insert(job_id, job.clone());
            Self::emit_job_created_events_locked(&mut inner, &job, now);
        }
        Ok(added)
    }

    /// Add a parent-child flow. The parent remains blocked until every child is
    /// completed, removed, or released by a terminal failure policy.
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
                if find_active_deduplication_id(
                    &inner.jobs,
                    &inner.released_deduplication_owners,
                    deduplication_id,
                    now,
                )
                .is_some()
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
        assign_waiting_order(&mut inner.sequence, &mut parent_job);
        Self::forget_released_deduplication_owner_locked(&mut inner, &parent_job);
        inner.jobs.insert(parent_job.id.clone(), parent_job.clone());
        Self::emit_job_created_events_locked(&mut inner, &parent_job, now);
        for child in &mut child_jobs {
            assign_waiting_order(&mut inner.sequence, child);
            Self::forget_released_deduplication_owner_locked(&mut inner, child);
            inner.jobs.insert(child.id.clone(), child.clone());
            Self::emit_job_created_events_locked(&mut inner, child, now);
        }

        Ok(JobFlow {
            parent: parent_job,
            children: child_jobs,
        })
    }

    /// Return a parent flow's current child dependency snapshot.
    pub async fn get_flow_dependencies(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencies>> {
        let inner = self.inner.lock().await;
        let Some(parent) = inner.jobs.get(parent_id).cloned() else {
            return Ok(None);
        };

        let mut children = Vec::new();
        let mut pending_child_ids = Vec::new();
        let mut missing_child_ids = Vec::new();
        for child_id in &parent.child_ids {
            match inner.jobs.get(child_id).cloned() {
                Some(child) => {
                    if !child.state.is_terminal() {
                        pending_child_ids.push(child.id.clone());
                    }
                    children.push(child);
                }
                None => missing_child_ids.push(child_id.clone()),
            }
        }

        Ok(Some(JobFlowDependencies {
            parent,
            children,
            pending_child_ids,
            missing_child_ids,
        }))
    }

    /// Return dependency counts for a flow parent.
    pub async fn get_flow_dependency_counts(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyCounts>> {
        let inner = self.inner.lock().await;
        let Some(parent) = inner.jobs.get(parent_id) else {
            return Ok(None);
        };

        Ok(Some(flow_dependency_counts(parent, &inner.jobs)))
    }

    /// Return completed child result values for a flow parent.
    pub async fn get_flow_children_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowChildValues>> {
        let inner = self.inner.lock().await;
        let Some(parent) = inner.jobs.get(parent_id) else {
            return Ok(None);
        };

        Ok(Some(flow_children_values(parent, &inner.jobs)))
    }

    /// Return ignored child failure reasons for a flow parent.
    pub async fn get_flow_ignored_children_failures(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowIgnoredFailures>> {
        let inner = self.inner.lock().await;
        let Some(parent) = inner.jobs.get(parent_id) else {
            return Ok(None);
        };

        Ok(Some(flow_ignored_children_failures(parent, &inner.jobs)))
    }

    /// Remove children that are still unprocessed and not active.
    pub async fn remove_unprocessed_children(
        &self,
        parent_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Vec<Job>>> {
        let mut inner = self.inner.lock().await;
        let Some(parent) = inner.jobs.get(parent_id) else {
            return Ok(None);
        };

        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        collect_removable_unprocessed_children(&inner.jobs, &parent.child_ids, &mut ids, &mut seen);

        let mut removed = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(job) = Self::remove_job_record_locked(&mut inner, &id) {
                removed.push(job);
            }
        }
        Self::release_parent_if_ready_locked(&mut inner, parent_id, now);

        Ok(Some(removed))
    }

    /// Remove a single child from its parent dependency list without deleting the child job.
    pub async fn remove_child_dependency(
        &self,
        child_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let mut inner = self.inner.lock().await;
        let Some(child) = inner.jobs.get(child_id) else {
            return Err(LaneError::JobNotFound(child_id.to_string()));
        };
        let Some(parent_id) = child.parent_id.clone() else {
            return Ok(false);
        };
        let Some(parent) = inner.jobs.get(&parent_id) else {
            return Err(LaneError::JobNotFound(parent_id));
        };
        if child.state.is_terminal() || !parent.child_ids.iter().any(|id| id == child_id) {
            return Ok(false);
        }

        if let Some(parent) = inner.jobs.get_mut(&parent_id) {
            parent.child_ids.retain(|id| id != child_id);
        }
        if let Some(child) = inner.jobs.get_mut(child_id) {
            child.parent_id = None;
        }
        Self::release_parent_if_ready_locked(&mut inner, &parent_id, now);
        Ok(true)
    }

    /// Return the current state for a job id.
    pub async fn get_state(&self, job_id: &str) -> Result<Option<JobState>> {
        let inner = self.inner.lock().await;
        Ok(inner.jobs.get(job_id).map(|job| job.state))
    }

    /// Remove a job from the queue.
    pub async fn remove(&self, job_id: &str) -> Result<Option<Job>> {
        let mut inner = self.inner.lock().await;
        if let Some(job) = inner.jobs.get(job_id) {
            require_removable(job)?;
        }
        let removed = Self::remove_job_record_locked(&mut inner, job_id);
        if let Some(parent_id) = removed.as_ref().and_then(|job| job.parent_id.clone()) {
            Self::release_parent_if_ready_locked(&mut inner, &parent_id, Utc::now());
        }
        Ok(removed)
    }

    /// Remove the active deduplication owner key, allowing a new owner to be added.
    pub async fn remove_deduplication_key(&self, deduplication_id: &str) -> Result<bool> {
        if deduplication_id.is_empty() {
            return Ok(false);
        }

        let mut inner = self.inner.lock().await;
        let Some(owner) = find_active_deduplication_id(
            &inner.jobs,
            &inner.released_deduplication_owners,
            deduplication_id,
            Utc::now(),
        ) else {
            return Ok(false);
        };
        let owner_id = owner.id.clone();
        inner
            .released_deduplication_owners
            .insert((deduplication_id.to_string(), owner_id));
        inner.deduplication_next.remove(deduplication_id);
        Ok(true)
    }

    /// Return the current active job id for a deduplication id.
    pub async fn get_deduplication_job_id(&self, deduplication_id: &str) -> Result<Option<JobId>> {
        if deduplication_id.is_empty() {
            return Ok(None);
        }

        let inner = self.inner.lock().await;
        Ok(find_active_deduplication_id(
            &inner.jobs,
            &inner.released_deduplication_owners,
            deduplication_id,
            Utc::now(),
        )
        .map(|job| job.id.clone()))
    }

    /// List current non-terminal repeat series owners.
    pub async fn list_repeats(&self) -> Result<Vec<JobRepeatEntry>> {
        let inner = self.inner.lock().await;
        let mut repeats = inner
            .jobs
            .values()
            .filter_map(repeat_entry)
            .collect::<Vec<_>>();
        repeats.sort_by(|a, b| a.key.cmp(&b.key).then_with(|| a.job_id.cmp(&b.job_id)));
        Ok(repeats)
    }

    /// Return one repeat series / job scheduler by key.
    pub async fn get_repeat(&self, repeat_key: &str) -> Result<Option<JobRepeatEntry>> {
        Ok(self
            .list_repeats()
            .await?
            .into_iter()
            .find(|entry| entry.key == repeat_key))
    }

    /// Return the number of current repeat series / job schedulers.
    pub async fn count_repeats(&self) -> Result<usize> {
        Ok(self.list_repeats().await?.len())
    }

    /// Return repeat series / job schedulers ordered by next scheduled time.
    pub async fn list_repeats_page(&self, options: JobRepeatListOptions) -> Result<JobRepeatPage> {
        Ok(page_repeat_entries(self.list_repeats().await?, options))
    }

    /// Create or replace the current non-active occurrence for a repeat series.
    pub async fn upsert_repeat(&self, spec: JobSpec, now: DateTime<Utc>) -> Result<Job> {
        validate_job_options(&spec.options)?;
        if spec.options.repeat.is_none() {
            return Err(LaneError::ConfigError(
                "repeat options are required to upsert a repeat series".to_string(),
            ));
        }

        let mut job = Job::new(
            self.queue.clone(),
            spec.name,
            spec.payload,
            spec.options,
            now,
        );
        let repeat_key = job
            .repeat_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| LaneError::ConfigError("repeat key must not be empty".to_string()))?
            .to_string();

        let mut inner = self.inner.lock().await;
        let current_owner = find_active_repeat_key(&inner.jobs, &repeat_key).cloned();
        if let Some(owner) = &current_owner {
            require_removable(owner)?;
            if owner.parent_id.is_some() || !owner.child_ids.is_empty() {
                return Err(LaneError::JobStateConflict(format!(
                    "cannot upsert repeat key `{repeat_key}`; current owner {} has flow dependencies",
                    owner.id
                )));
            }
        }

        if let Some(existing) = inner.jobs.get(&job.id) {
            if current_owner.as_ref().map(|owner| owner.id.as_str()) != Some(existing.id.as_str()) {
                return Err(LaneError::ConfigError(format!(
                    "job id `{}` already exists",
                    job.id
                )));
            }
        }

        if let Some(deduplication_id) = job_deduplication_id(&job) {
            if let Some(existing) = find_active_deduplication_id(
                &inner.jobs,
                &inner.released_deduplication_owners,
                deduplication_id,
                now,
            ) {
                if current_owner.as_ref().map(|owner| owner.id.as_str())
                    != Some(existing.id.as_str())
                {
                    return Err(LaneError::JobStateConflict(format!(
                        "cannot upsert repeat key `{repeat_key}`; deduplication id `{deduplication_id}` is active on job {}",
                        existing.id
                    )));
                }
            }
        }

        if let Some(owner) = current_owner {
            Self::remove_job_record_locked(&mut inner, &owner.id);
        }

        Self::forget_released_deduplication_owner_locked(&mut inner, &job);
        assign_waiting_order(&mut inner.sequence, &mut job);
        inner.jobs.insert(job.id.clone(), job.clone());
        Self::emit_job_created_events_locked(&mut inner, &job, now);
        Ok(job)
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
        let removed = Self::remove_job_record_locked(&mut inner, &job_id);
        if let Some(parent_id) = removed.as_ref().and_then(|job| job.parent_id.clone()) {
            Self::release_parent_if_ready_locked(&mut inner, &parent_id, Utc::now());
        }
        Ok(removed)
    }

    /// Promote a single delayed job to waiting.
    pub async fn promote(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let should_promote = inner
            .jobs
            .get(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?
            .state
            == JobState::Delayed;
        let enqueued_seq = should_promote.then(|| next_waiting_sequence(&mut inner.sequence));
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        if should_promote {
            job.state = JobState::Waiting;
            job.scheduled_at = now;
            job.enqueued_seq = enqueued_seq.expect("promoted jobs get a waiting sequence");
        }
        let job = job.clone();
        if should_promote {
            emit_event_locked(
                &mut inner,
                "waiting",
                Some(&job),
                Some(JobState::Delayed),
                now,
                BTreeMap::new(),
            );
        }
        Ok(job)
    }

    /// Reschedule a delayed job relative to `now`.
    pub async fn reschedule(
        &self,
        job_id: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        if delay.is_zero() {
            return Err(LaneError::ConfigError(
                "job delay must be greater than zero".to_string(),
            ));
        }

        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        if job.state != JobState::Delayed {
            return Err(LaneError::JobStateConflict(format!(
                "cannot reschedule job {} from state {:?}",
                job.id, job.state
            )));
        }

        job.options.delay = Some(delay);
        job.scheduled_at = add_duration(now, delay);
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
        let retry_deduplication_id = current
            .options
            .deduplication
            .as_ref()
            .map(|value| value.id.clone());
        let retry_repeat_key = current.repeat_key.clone();

        if let Some(deduplication_id) = retry_deduplication_id.as_deref() {
            if let Some(existing) = find_active_deduplication_id_except(
                &inner.jobs,
                &inner.released_deduplication_owners,
                deduplication_id,
                job_id,
                now,
            ) {
                return Err(LaneError::JobStateConflict(format!(
                    "cannot retry job {job_id}; deduplication id `{deduplication_id}` is active on job {}",
                    existing.id
                )));
            }
        }
        if let Some(repeat_key) = retry_repeat_key.as_deref() {
            if let Some(existing) = find_active_repeat_key_except(&inner.jobs, repeat_key, job_id) {
                return Err(LaneError::JobStateConflict(format!(
                    "cannot retry job {job_id}; repeat key `{repeat_key}` is active on job {}",
                    existing.id
                )));
            }
        }
        if let Some(deduplication_id) = retry_deduplication_id {
            inner
                .released_deduplication_owners
                .remove(&(deduplication_id, job_id.to_string()));
        }

        let enqueued_seq = next_waiting_sequence(&mut inner.sequence);
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
        job.deferred_failure = None;
        job.failed_reason = None;
        job.enqueued_seq = enqueued_seq;
        let job = job.clone();
        emit_event_locked(
            &mut inner,
            "waiting",
            Some(&job),
            Some(JobState::Failed),
            now,
            BTreeMap::new(),
        );
        Ok(job)
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

    /// Move an active leased job back to delayed state.
    pub async fn delay_active(
        &self,
        job_id: &str,
        lock_token: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        require_active(job, "delay active")?;
        require_lock_token(job, lock_token)?;
        job.state = JobState::Delayed;
        job.options.delay = Some(delay);
        job.scheduled_at = add_duration(now, delay);
        job.processed_at = None;
        job.worker_id = None;
        job.lock_token = None;
        job.lease_expires_at = None;
        job.deferred_failure = None;
        job.failed_reason = None;
        let job = job.clone();
        emit_event_locked(
            &mut inner,
            "delayed",
            Some(&job),
            Some(JobState::Active),
            now,
            BTreeMap::new(),
        );
        Ok(job)
    }

    /// Release an active leased job back to waiting state.
    pub async fn release_active(
        &self,
        job_id: &str,
        lock_token: &str,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        {
            let job = inner
                .jobs
                .get(job_id)
                .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
            require_active(job, "release active")?;
            require_lock_token(job, lock_token)?;
        }
        let enqueued_seq = next_waiting_sequence(&mut inner.sequence);
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        job.state = JobState::Waiting;
        job.scheduled_at = now;
        job.processed_at = None;
        job.worker_id = None;
        job.lock_token = None;
        job.lease_expires_at = None;
        job.deferred_failure = None;
        job.failed_reason = None;
        job.enqueued_seq = enqueued_seq;
        let job = job.clone();
        emit_event_locked(
            &mut inner,
            "waiting",
            Some(&job),
            Some(JobState::Active),
            now,
            BTreeMap::new(),
        );
        Ok(job)
    }

    /// Update a non-terminal job priority.
    pub async fn set_priority(&self, job_id: &str, priority: JobPriority) -> Result<Job> {
        self.set_priority_order(job_id, priority, None).await
    }

    /// Update a non-terminal job priority and waiting reinsert order.
    pub async fn set_priority_with_lifo(
        &self,
        job_id: &str,
        priority: JobPriority,
        lifo: bool,
    ) -> Result<Job> {
        self.set_priority_order(job_id, priority, Some(lifo)).await
    }

    async fn set_priority_order(
        &self,
        job_id: &str,
        priority: JobPriority,
        lifo: Option<bool>,
    ) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let should_requeue = {
            let job = inner
                .jobs
                .get(job_id)
                .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
            if job.state.is_terminal() {
                return Err(LaneError::JobStateConflict(format!(
                    "cannot update priority for terminal job {}",
                    job.id
                )));
            }
            job.state == JobState::Waiting
        };
        let enqueued_seq = should_requeue.then(|| next_waiting_sequence(&mut inner.sequence));
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        job.priority = priority;
        job.options.priority = priority;
        if let Some(enqueued_seq) = enqueued_seq {
            job.enqueued_seq = enqueued_seq;
        }
        if let Some(lifo) = lifo {
            job.options.lifo = lifo;
        }
        Ok(job.clone())
    }

    /// List jobs with deterministic pagination.
    pub async fn list(&self, options: JobListOptions) -> Result<JobListPage> {
        let inner = self.inner.lock().await;
        let states = options.selected_states();
        let mut jobs = inner
            .jobs
            .values()
            .filter(|job| states.contains(&job.state))
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by(compare_list_order);
        if !options.ascending {
            jobs.reverse();
        }

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

    /// Count jobs for requested lifecycle states.
    pub async fn counts_by_state(&self, states: &[JobState]) -> Result<Vec<JobStateCount>> {
        let states = unique_states(states);
        let inner = self.inner.lock().await;
        Ok(states
            .into_iter()
            .map(|state| {
                let count = inner.jobs.values().filter(|job| job.state == state).count();
                JobStateCount { state, count }
            })
            .collect())
    }

    /// Count waiting jobs for each requested priority.
    pub async fn counts_per_priority(
        &self,
        priorities: &[JobPriority],
    ) -> Result<Vec<JobPriorityCount>> {
        let priorities = unique_priorities(priorities);
        let inner = self.inner.lock().await;
        Ok(priorities
            .into_iter()
            .map(|priority| {
                let count = inner
                    .jobs
                    .values()
                    .filter(|job| job.state == JobState::Waiting && job.priority == priority)
                    .count();
                JobPriorityCount { priority, count }
            })
            .collect())
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
            Self::remove_job_record_locked(&mut inner, &job.id);
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
            Self::remove_job_record_locked(&mut inner, &job.id);
        }
        for parent_id in parent_ids {
            Self::release_parent_if_ready_locked(&mut inner, &parent_id, Utc::now());
        }

        Ok(jobs)
    }

    /// Remove every job and queue-owned metadata entry.
    pub async fn obliterate(&self, force: bool) -> Result<usize> {
        let mut inner = self.inner.lock().await;
        inner.paused = true;
        let active = inner.jobs.values().any(|job| job.state == JobState::Active);
        if active && !force {
            return Err(LaneError::JobStateConflict(
                "cannot obliterate queue with active jobs".to_string(),
            ));
        }

        let removed = inner.jobs.len();
        inner.jobs.clear();
        inner.deduplication_next.clear();
        inner.released_deduplication_owners.clear();
        inner.paused = false;
        Ok(removed)
    }

    /// Update progress for a non-terminal job.
    pub async fn set_data(&self, job_id: &str, payload: Value) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        let job = inner
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
        job.payload = payload;
        Ok(job.clone())
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
        job.progress = Some(progress.clone());
        let job = job.clone();
        let mut fields = BTreeMap::new();
        fields.insert("data".to_string(), progress);
        emit_event_locked(&mut inner, "progress", Some(&job), None, Utc::now(), fields);
        Ok(job)
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

    /// Return retained log entries for a job.
    pub async fn get_logs(
        &self,
        job_id: &str,
        start: isize,
        end: isize,
        ascending: bool,
    ) -> Result<JobLogPage> {
        let inner = self.inner.lock().await;
        Ok(inner
            .jobs
            .get(job_id)
            .map(|job| log_page(&job.logs, start, end, ascending))
            .unwrap_or_else(|| JobLogPage {
                logs: Vec::new(),
                count: 0,
            }))
    }

    /// Clear retained log entries for a job. `keep == 0` clears all logs.
    pub async fn clear_logs(&self, job_id: &str, keep: usize) -> Result<JobLogPage> {
        let mut inner = self.inner.lock().await;
        let Some(job) = inner.jobs.get_mut(job_id) else {
            return Ok(JobLogPage {
                logs: Vec::new(),
                count: 0,
            });
        };

        if keep == 0 {
            job.logs.clear();
        } else if job.logs.len() > keep {
            let remove_count = job.logs.len() - keep;
            job.logs.drain(0..remove_count);
        }

        Ok(log_page(&job.logs, 0, -1, true))
    }

    fn promote_due_locked(inner: &mut InMemoryJobQueueState, now: DateTime<Utc>) -> usize {
        let mut promoted = 0;
        let mut due_jobs = inner
            .jobs
            .values()
            .filter(|job| job.state == JobState::Delayed && job.scheduled_at <= now)
            .cloned()
            .collect::<Vec<_>>();
        due_jobs.sort_by(|a, b| {
            a.scheduled_at
                .cmp(&b.scheduled_at)
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        for due_job in due_jobs {
            let enqueued_seq = next_waiting_sequence(&mut inner.sequence);
            let promoted_job = if let Some(job) = inner.jobs.get_mut(&due_job.id) {
                job.state = JobState::Waiting;
                job.enqueued_seq = enqueued_seq;
                promoted += 1;
                Some(job.clone())
            } else {
                None
            };
            if let Some(job) = promoted_job {
                emit_event_locked(
                    inner,
                    "waiting",
                    Some(&job),
                    Some(JobState::Delayed),
                    now,
                    BTreeMap::new(),
                );
            }
        }
        promoted
    }

    fn remove_job_record_locked(inner: &mut InMemoryJobQueueState, job_id: &str) -> Option<Job> {
        let removed = inner.jobs.remove(job_id)?;
        Self::forget_released_deduplication_owner_locked(inner, &removed);
        Some(removed)
    }

    fn apply_finished_retention_locked(
        inner: &mut InMemoryJobQueueState,
        state: JobState,
        retention: &JobRetention,
        now: DateTime<Utc>,
    ) {
        if let Some(age) = retention.age {
            let cutoff = subtract_duration(now, age);
            let limit = retention.limit.unwrap_or(1_000);
            let mut expired = inner
                .jobs
                .values()
                .filter(|job| job.state == state)
                .filter_map(|job| Some((job.id.clone(), job.finished_at?)))
                .filter(|(_, finished_at)| *finished_at <= cutoff)
                .collect::<Vec<_>>();
            expired.sort_by(
                |(left_id, left_finished_at), (right_id, right_finished_at)| {
                    right_finished_at
                        .cmp(left_finished_at)
                        .then_with(|| right_id.cmp(left_id))
                },
            );
            for (job_id, _) in expired.into_iter().take(limit) {
                Self::remove_job_record_locked(inner, &job_id);
            }
        }

        if let Some(count) = retention.count {
            let mut retained = inner
                .jobs
                .values()
                .filter(|job| job.state == state)
                .filter_map(|job| Some((job.id.clone(), job.finished_at?)))
                .collect::<Vec<_>>();
            retained.sort_by(
                |(left_id, left_finished_at), (right_id, right_finished_at)| {
                    right_finished_at
                        .cmp(left_finished_at)
                        .then_with(|| right_id.cmp(left_id))
                },
            );
            for (job_id, _) in retained.into_iter().skip(count) {
                Self::remove_job_record_locked(inner, &job_id);
            }
        }
    }

    fn forget_released_deduplication_owner_locked(inner: &mut InMemoryJobQueueState, job: &Job) {
        if let Some(deduplication_id) = job_deduplication_id(job) {
            inner
                .released_deduplication_owners
                .remove(&(deduplication_id.to_string(), job.id.clone()));
        }
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
        let repeat_next_count = repeat_keep_last_next_count(owner, &next)?;
        prepare_deduplicated_next_job(&mut next, now);
        if let Some(next_count) = repeat_next_count {
            next.repeat_key = owner.repeat_key.clone();
            next.repeat_count = next_count;
        }
        if inner.jobs.contains_key(&next.id) {
            return None;
        }
        Self::forget_released_deduplication_owner_locked(inner, &next);
        assign_waiting_order(&mut inner.sequence, &mut next);
        inner.jobs.insert(next.id.clone(), next.clone());
        Self::emit_job_created_events_locked(inner, &next, now);
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
        let mut deferred_child_failure = None;
        for child_id in &child_ids {
            let Some(child) = inner.jobs.get(child_id) else {
                continue;
            };
            match child.state {
                JobState::Completed => {}
                JobState::Failed if child.options.fail_parent_on_failure => {
                    deferred_child_failure = Some(child.id.clone())
                }
                JobState::Failed if child_failure_releases_dependency(child) => {}
                JobState::Failed => {
                    child_failure = Some((child.id.clone(), child.failed_reason.clone()))
                }
                _ => return Some(parent.clone()),
            }
        }
        if let Some(child_id) = deferred_child_failure {
            return Self::defer_parent_failure_after_child_failure_locked(
                inner, parent_id, &child_id, now,
            );
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

        let parent_scheduled_at = inner.jobs.get(parent_id)?.scheduled_at;
        let parent_state = state_after_dependencies(parent_scheduled_at, now);
        let enqueued_seq =
            (parent_state == JobState::Waiting).then(|| next_waiting_sequence(&mut inner.sequence));
        let parent = inner.jobs.get_mut(parent_id)?;
        parent.state = parent_state;
        parent.processed_at = None;
        parent.finished_at = None;
        parent.worker_id = None;
        parent.lock_token = None;
        parent.lease_expires_at = None;
        parent.deferred_failure = None;
        parent.failed_reason = None;
        if let Some(enqueued_seq) = enqueued_seq {
            parent.enqueued_seq = enqueued_seq;
        }
        let parent = parent.clone();
        emit_event_locked(
            inner,
            job_event_for_state(parent.state),
            Some(&parent),
            Some(JobState::WaitingChildren),
            now,
            BTreeMap::new(),
        );
        Some(parent)
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
        parent.deferred_failure = None;
        parent.failed_reason = Some(reason);
        let failed = parent.clone();
        Self::forget_released_deduplication_owner_locked(inner, &failed);
        if failed.options.removes_failed_immediately() {
            Self::remove_job_record_locked(inner, parent_id);
        } else if let Some(retention) = failed.options.failed_retention() {
            Self::apply_finished_retention_locked(inner, JobState::Failed, retention, now);
        }
        let mut fields = BTreeMap::new();
        if let Some(reason) = &failed.failed_reason {
            fields.insert("failedReason".to_string(), Value::String(reason.clone()));
        }
        emit_event_locked(
            inner,
            "failed",
            Some(&failed),
            Some(JobState::WaitingChildren),
            now,
            fields,
        );
        Some(failed)
    }

    fn continue_parent_after_child_failure_locked(
        inner: &mut InMemoryJobQueueState,
        parent_id: &str,
        now: DateTime<Utc>,
    ) -> Option<Job> {
        let parent = inner.jobs.get(parent_id)?;
        if parent.state != JobState::WaitingChildren {
            return Some(parent.clone());
        }

        let parent_scheduled_at = parent.scheduled_at;
        let parent_state = state_after_dependencies(parent_scheduled_at, now);
        let enqueued_seq =
            (parent_state == JobState::Waiting).then(|| next_waiting_sequence(&mut inner.sequence));
        let parent = inner.jobs.get_mut(parent_id)?;
        parent.state = parent_state;
        parent.processed_at = None;
        parent.finished_at = None;
        parent.worker_id = None;
        parent.lock_token = None;
        parent.lease_expires_at = None;
        parent.deferred_failure = None;
        parent.failed_reason = None;
        if let Some(enqueued_seq) = enqueued_seq {
            parent.enqueued_seq = enqueued_seq;
        }
        let parent = parent.clone();
        emit_event_locked(
            inner,
            job_event_for_state(parent.state),
            Some(&parent),
            Some(JobState::WaitingChildren),
            now,
            BTreeMap::new(),
        );
        Some(parent)
    }

    fn defer_parent_failure_after_child_failure_locked(
        inner: &mut InMemoryJobQueueState,
        parent_id: &str,
        child_id: &str,
        now: DateTime<Utc>,
    ) -> Option<Job> {
        let parent = inner.jobs.get(parent_id)?;
        if parent.state.is_terminal() {
            return Some(parent.clone());
        }

        let parent_scheduled_at = parent.scheduled_at;
        let parent_state = state_after_dependencies(parent_scheduled_at, now);
        let previous_state = parent.state;
        let enqueued_seq =
            (parent_state == JobState::Waiting).then(|| next_waiting_sequence(&mut inner.sequence));
        let parent = inner.jobs.get_mut(parent_id)?;
        parent.state = parent_state;
        parent.processed_at = None;
        parent.finished_at = None;
        parent.worker_id = None;
        parent.lock_token = None;
        parent.lease_expires_at = None;
        parent.deferred_failure = Some(format!("child job {child_id} failed"));
        parent.failed_reason = None;
        if let Some(enqueued_seq) = enqueued_seq {
            parent.enqueued_seq = enqueued_seq;
        }
        let parent = parent.clone();
        emit_event_locked(
            inner,
            job_event_for_state(parent.state),
            Some(&parent),
            Some(previous_state),
            now,
            BTreeMap::new(),
        );
        Some(parent)
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

    async fn get_flow_dependencies(&self, parent_id: &str) -> Result<Option<JobFlowDependencies>> {
        InMemoryJobQueue::get_flow_dependencies(self, parent_id).await
    }

    async fn get_flow_dependency_counts(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyCounts>> {
        InMemoryJobQueue::get_flow_dependency_counts(self, parent_id).await
    }

    async fn get_flow_children_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowChildValues>> {
        InMemoryJobQueue::get_flow_children_values(self, parent_id).await
    }

    async fn get_flow_ignored_children_failures(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowIgnoredFailures>> {
        InMemoryJobQueue::get_flow_ignored_children_failures(self, parent_id).await
    }

    async fn remove_unprocessed_children(
        &self,
        parent_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Vec<Job>>> {
        InMemoryJobQueue::remove_unprocessed_children(self, parent_id, now).await
    }

    async fn remove_child_dependency(&self, child_id: &str, now: DateTime<Utc>) -> Result<bool> {
        InMemoryJobQueue::remove_child_dependency(self, child_id, now).await
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

        let claimed = {
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
            job.clone()
        };
        emit_event_locked(
            &mut inner,
            "active",
            Some(&claimed),
            Some(JobState::Waiting),
            now,
            BTreeMap::new(),
        );
        Ok(Some(claimed))
    }

    async fn complete_job(
        &self,
        job_id: &str,
        lock_token: &str,
        value: Value,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut inner = self.inner.lock().await;
        {
            let job = inner
                .jobs
                .get(job_id)
                .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
            require_active(job, "complete")?;
            require_lock_token(job, lock_token)?;
            ensure_flow_dependencies_are_resolved(job, &inner.jobs, "complete")?;
        }
        let completed = {
            let job = inner
                .jobs
                .get_mut(job_id)
                .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))?;
            job.state = JobState::Completed;
            job.finished_at = Some(now);
            job.worker_id = None;
            job.lock_token = None;
            job.lease_expires_at = None;
            job.deferred_failure = None;
            job.return_value = Some(value);
            job.clone()
        };
        Self::forget_released_deduplication_owner_locked(&mut inner, &completed);
        if completed.options.removes_completed_immediately() {
            Self::remove_job_record_locked(&mut inner, job_id);
        } else if let Some(retention) = completed.options.completed_retention() {
            Self::apply_finished_retention_locked(&mut inner, JobState::Completed, retention, now);
        }
        let enqueued_deduplicated_next =
            Self::enqueue_deduplicated_next_locked(&mut inner, &completed, now);
        if enqueued_deduplicated_next.is_none() {
            if let Some(next_job) = next_repeat_job(&completed, now)? {
                let mut next_job = next_job;
                Self::forget_released_deduplication_owner_locked(&mut inner, &next_job);
                assign_waiting_order(&mut inner.sequence, &mut next_job);
                inner.jobs.insert(next_job.id.clone(), next_job.clone());
                Self::emit_job_created_events_locked(&mut inner, &next_job, now);
            }
        }
        if let Some(parent_id) = &completed.parent_id {
            Self::release_parent_if_ready_locked(&mut inner, parent_id, now);
        }
        let mut fields = BTreeMap::new();
        if let Some(value) = &completed.return_value {
            fields.insert("returnvalue".to_string(), value.clone());
        }
        emit_event_locked(
            &mut inner,
            "completed",
            Some(&completed),
            Some(JobState::Active),
            now,
            fields,
        );
        Ok(completed)
    }

    async fn fail_job(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        self.fail_active_job(job_id, lock_token, error, now, true)
            .await
    }

    async fn fail_job_discarding_retry(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        self.fail_active_job(job_id, lock_token, error, now, false)
            .await
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

    async fn delay_active_job(
        &self,
        job_id: &str,
        lock_token: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        self.delay_active(job_id, lock_token, delay, now).await
    }

    async fn release_active_job(
        &self,
        job_id: &str,
        lock_token: &str,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        self.release_active(job_id, lock_token, now).await
    }

    async fn promote_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        self.promote(job_id, now).await
    }

    async fn reschedule_job(
        &self,
        job_id: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        self.reschedule(job_id, delay, now).await
    }

    async fn retry_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        self.retry(job_id, now).await
    }

    async fn update_priority(&self, job_id: &str, priority: JobPriority) -> Result<Job> {
        self.set_priority(job_id, priority).await
    }

    async fn update_priority_with_lifo(
        &self,
        job_id: &str,
        priority: JobPriority,
        lifo: bool,
    ) -> Result<Job> {
        self.set_priority_with_lifo(job_id, priority, lifo).await
    }

    async fn remove_job(&self, job_id: &str) -> Result<Option<Job>> {
        self.remove(job_id).await
    }

    async fn remove_repeat(&self, repeat_key: &str) -> Result<Option<Job>> {
        InMemoryJobQueue::remove_repeat(self, repeat_key).await
    }

    async fn remove_deduplication_key(&self, deduplication_id: &str) -> Result<bool> {
        InMemoryJobQueue::remove_deduplication_key(self, deduplication_id).await
    }

    async fn get_deduplication_job_id(&self, deduplication_id: &str) -> Result<Option<JobId>> {
        InMemoryJobQueue::get_deduplication_job_id(self, deduplication_id).await
    }

    async fn list_repeats(&self) -> Result<Vec<JobRepeatEntry>> {
        InMemoryJobQueue::list_repeats(self).await
    }

    async fn upsert_repeat(&self, spec: JobSpec, now: DateTime<Utc>) -> Result<Job> {
        InMemoryJobQueue::upsert_repeat(self, spec, now).await
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

    async fn obliterate(&self, force: bool) -> Result<usize> {
        InMemoryJobQueue::obliterate(self, force).await
    }

    async fn list_jobs(&self, options: JobListOptions) -> Result<JobListPage> {
        self.list(options).await
    }

    async fn get_job_counts(&self, states: &[JobState]) -> Result<Vec<JobStateCount>> {
        self.counts_by_state(states).await
    }

    async fn get_counts_per_priority(
        &self,
        priorities: &[JobPriority],
    ) -> Result<Vec<JobPriorityCount>> {
        self.counts_per_priority(priorities).await
    }

    async fn update_data(&self, job_id: &str, payload: Value) -> Result<Job> {
        self.set_data(job_id, payload).await
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

    async fn get_job_logs(
        &self,
        job_id: &str,
        start: isize,
        end: isize,
        ascending: bool,
    ) -> Result<JobLogPage> {
        self.get_logs(job_id, start, end, ascending).await
    }

    async fn clear_job_logs(&self, job_id: &str, keep: usize) -> Result<JobLogPage> {
        self.clear_logs(job_id, keep).await
    }

    async fn read_events(&self, start: &str, end: &str, limit: usize) -> Result<Vec<JobEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let inner = self.inner.lock().await;
        Ok(inner
            .events
            .iter()
            .filter(|event| event_in_range(event, start, end))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn trim_events(&self, max_len: usize) -> Result<usize> {
        let mut inner = self.inner.lock().await;
        Ok(trim_events_locked(&mut inner, max_len))
    }

    async fn promote_due_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut inner = self.inner.lock().await;
        Ok(Self::promote_due_locked(&mut inner, now))
    }

    async fn recover_stalled_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut inner = self.inner.lock().await;
        let mut recovered = 0;
        let mut remove_ids = Vec::new();
        let mut deferred_parent_failures = Vec::new();
        let mut continuing_failed_children = Vec::new();
        let mut dependency_releasing_failed_children = Vec::new();
        let mut failed_children = Vec::new();
        let mut terminal_failures = Vec::new();
        let mut recovered_events = Vec::new();

        let mut stalled_jobs = inner
            .jobs
            .values()
            .filter(|job| {
                job.state == JobState::Active
                    && matches!(job.lease_expires_at, Some(expires_at) if expires_at <= now)
            })
            .cloned()
            .collect::<Vec<_>>();
        stalled_jobs.sort_by(|a, b| {
            a.lease_expires_at
                .cmp(&b.lease_expires_at)
                .then_with(|| a.processed_at.cmp(&b.processed_at))
                .then_with(|| a.id.cmp(&b.id))
        });

        for stalled_job in stalled_jobs {
            let id = stalled_job.id;
            let mut requeue = false;
            if let Some(job) = inner.jobs.get_mut(&id) {
                job.stalled_count = job.stalled_count.saturating_add(1);
                job.worker_id = None;
                job.lock_token = None;
                job.lease_expires_at = None;
                job.deferred_failure = None;
                job.failed_reason = Some("job stalled after worker lease expired".to_string());
                if job.stalled_count > job.options.max_stalled_count
                    && !is_repeat_scheduler_job(job)
                {
                    job.state = JobState::Failed;
                    job.finished_at = Some(now);
                    if let Some(parent_id) = &job.parent_id {
                        if job.options.fail_parent_on_failure {
                            deferred_parent_failures.push((parent_id.clone(), job.id.clone()));
                        } else if job.options.continue_parent_on_failure {
                            continuing_failed_children.push(parent_id.clone());
                        } else if child_failure_releases_dependency(job) {
                            dependency_releasing_failed_children.push(parent_id.clone());
                        } else {
                            failed_children.push((
                                parent_id.clone(),
                                job.id.clone(),
                                job.failed_reason.clone(),
                            ));
                        }
                    }
                    let failed = job.clone();
                    terminal_failures.push(failed.clone());
                    recovered_events.push(failed);
                    if job.options.removes_failed_immediately() {
                        remove_ids.push(job.id.clone());
                    }
                } else {
                    job.state = JobState::Waiting;
                    job.processed_at = None;
                    requeue = true;
                }
            }
            if requeue {
                let enqueued_seq = next_waiting_sequence(&mut inner.sequence);
                if let Some(job) = inner.jobs.get_mut(&id) {
                    job.enqueued_seq = enqueued_seq;
                    recovered_events.push(job.clone());
                }
            }
            recovered += 1;
        }

        for id in remove_ids {
            Self::remove_job_record_locked(&mut inner, &id);
        }
        for failed in terminal_failures {
            Self::forget_released_deduplication_owner_locked(&mut inner, &failed);
            Self::enqueue_deduplicated_next_locked(&mut inner, &failed, now);
            if let Some(retention) = failed.options.failed_retention() {
                Self::apply_finished_retention_locked(&mut inner, JobState::Failed, retention, now);
            }
        }
        for (parent_id, child_id) in deferred_parent_failures {
            Self::defer_parent_failure_after_child_failure_locked(
                &mut inner, &parent_id, &child_id, now,
            );
        }
        for parent_id in continuing_failed_children {
            Self::continue_parent_after_child_failure_locked(&mut inner, &parent_id, now);
        }
        for parent_id in dependency_releasing_failed_children {
            Self::release_parent_if_ready_locked(&mut inner, &parent_id, now);
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
        for job in recovered_events {
            let mut fields = BTreeMap::new();
            if let Some(reason) = &job.failed_reason {
                fields.insert("failedReason".to_string(), Value::String(reason.clone()));
            }
            emit_event_locked(
                &mut inner,
                "stalled",
                Some(&job),
                Some(JobState::Active),
                now,
                fields.clone(),
            );
            emit_event_locked(
                &mut inner,
                job_event_for_state(job.state),
                Some(&job),
                Some(JobState::Active),
                now,
                fields,
            );
        }
        Ok(recovered)
    }

    async fn pause(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        inner.paused = true;
        emit_event_locked(
            &mut inner,
            "paused",
            None,
            None,
            Utc::now(),
            BTreeMap::new(),
        );
        Ok(())
    }

    async fn resume(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        inner.paused = false;
        emit_event_locked(
            &mut inner,
            "resumed",
            None,
            None,
            Utc::now(),
            BTreeMap::new(),
        );
        Ok(())
    }

    async fn is_paused(&self) -> Result<bool> {
        let inner = self.inner.lock().await;
        Ok(inner.paused)
    }

    async fn get_job(&self, job_id: &str) -> Result<Option<Job>> {
        let inner = self.inner.lock().await;
        Ok(inner.jobs.get(job_id).cloned())
    }

    async fn get_job_state(&self, job_id: &str) -> Result<Option<JobState>> {
        self.get_state(job_id).await
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
        .then_with(|| compare_waiting_order(a, b))
        .then_with(|| a.scheduled_at.cmp(&b.scheduled_at))
        .then_with(|| a.created_at.cmp(&b.created_at))
        .then_with(|| a.id.cmp(&b.id))
}

fn emit_event_locked(
    inner: &mut InMemoryJobQueueState,
    event: impl Into<String>,
    job: Option<&Job>,
    prev: Option<JobState>,
    timestamp: DateTime<Utc>,
    fields: BTreeMap<String, Value>,
) {
    inner.event_sequence = inner.event_sequence.saturating_add(1);
    inner.events.push(JobEvent {
        id: format!("{}-0", inner.event_sequence),
        event: event.into(),
        timestamp,
        job_id: job.map(|job| job.id.clone()),
        prev,
        fields,
    });
    trim_events_locked(inner, DEFAULT_JOB_EVENT_RETENTION);
}

fn trim_events_locked(inner: &mut InMemoryJobQueueState, max_len: usize) -> usize {
    if inner.events.len() <= max_len {
        return 0;
    }
    let remove_count = inner.events.len() - max_len;
    inner.events.drain(0..remove_count);
    remove_count
}

fn event_in_range(event: &JobEvent, start: &str, end: &str) -> bool {
    let Some(event_id) = parse_stream_id(&event.id) else {
        return false;
    };
    let after_start = match start {
        "-" => true,
        other => match parse_stream_id(other) {
            Some(start_id) => event_id >= start_id,
            None => true,
        },
    };
    let before_end = match end {
        "+" => true,
        other => match parse_stream_id(other) {
            Some(end_id) => event_id <= end_id,
            None => true,
        },
    };
    after_start && before_end
}

fn parse_stream_id(id: &str) -> Option<(i64, u64)> {
    let (millis, sequence) = id.split_once('-')?;
    Some((millis.parse().ok()?, sequence.parse().ok()?))
}

fn job_event_for_state(state: JobState) -> &'static str {
    match state {
        JobState::Waiting => "waiting",
        JobState::Delayed => "delayed",
        JobState::Active => "active",
        JobState::WaitingChildren => "waiting-children",
        JobState::Completed => "completed",
        JobState::Failed => "failed",
    }
}

fn assign_waiting_order(sequence: &mut u64, job: &mut Job) {
    if job.state == JobState::Waiting {
        job.enqueued_seq = next_waiting_sequence(sequence);
    }
}

fn next_waiting_sequence(sequence: &mut u64) -> u64 {
    *sequence = sequence.saturating_add(1);
    *sequence
}

fn compare_waiting_order(a: &Job, b: &Job) -> Ordering {
    match (a.options.lifo, b.options.lifo) {
        (true, true) => b.enqueued_seq.cmp(&a.enqueued_seq),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.enqueued_seq.cmp(&b.enqueued_seq),
    }
}

fn log_page(logs: &[JobLogEntry], start: isize, end: isize, ascending: bool) -> JobLogPage {
    let count = logs.len();
    let selected = if ascending {
        redis_range(logs, start, end)
    } else {
        let reverse_start = end.saturating_add(1).saturating_neg();
        let reverse_end = start.saturating_add(1).saturating_neg();
        let mut logs = redis_range(logs, reverse_start, reverse_end);
        logs.reverse();
        logs
    };

    JobLogPage {
        logs: selected,
        count,
    }
}

fn redis_range<T: Clone>(items: &[T], start: isize, end: isize) -> Vec<T> {
    let len = items.len();
    if len == 0 {
        return Vec::new();
    }

    let start = normalize_redis_index(start, len);
    let end = normalize_redis_index(end, len);
    if start > end || start >= len {
        return Vec::new();
    }
    let end = end.min(len - 1);
    items[start..=end].to_vec()
}

fn normalize_redis_index(index: isize, len: usize) -> usize {
    if index >= 0 {
        return index as usize;
    }

    let normalized = len as isize + index;
    if normalized < 0 {
        0
    } else {
        normalized as usize
    }
}

fn unique_priorities(priorities: &[JobPriority]) -> Vec<JobPriority> {
    let mut unique = Vec::new();
    for &priority in priorities {
        if !unique.contains(&priority) {
            unique.push(priority);
        }
    }
    unique
}

fn unique_states(states: &[JobState]) -> Vec<JobState> {
    let states = if states.is_empty() {
        JobState::ALL.as_slice()
    } else {
        states
    };
    let mut unique = Vec::new();
    for &state in states {
        if !unique.contains(&state) {
            unique.push(state);
        }
    }
    unique
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

fn sorted_released_deduplication_owners(
    released_owners: &HashSet<(String, JobId)>,
) -> Vec<(String, JobId)> {
    let mut owners = released_owners.iter().cloned().collect::<Vec<_>>();
    owners.sort();
    owners
}

fn flow_dependency_counts(parent: &Job, jobs: &HashMap<JobId, Job>) -> JobFlowDependencyCounts {
    let mut counts = JobFlowDependencyCounts {
        processed: 0,
        unprocessed: 0,
        failed: 0,
        ignored: 0,
        missing: 0,
    };

    for child_id in &parent.child_ids {
        match jobs.get(child_id) {
            Some(child) if child.state == JobState::Completed => counts.processed += 1,
            Some(child)
                if child.state == JobState::Failed
                    && (child.options.ignore_dependency_on_failure
                        || child.options.continue_parent_on_failure) =>
            {
                counts.ignored += 1
            }
            Some(child)
                if child.state == JobState::Failed
                    && child.options.remove_dependency_on_failure => {}
            Some(child) if child.state == JobState::Failed => counts.failed += 1,
            Some(_) => counts.unprocessed += 1,
            None => counts.missing += 1,
        }
    }

    counts
}

fn flow_children_values(parent: &Job, jobs: &HashMap<JobId, Job>) -> JobFlowChildValues {
    let mut values = BTreeMap::new();
    for child_id in &parent.child_ids {
        let Some(child) = jobs.get(child_id) else {
            continue;
        };
        if child.state == JobState::Completed {
            if let Some(return_value) = &child.return_value {
                values.insert(child.id.clone(), return_value.clone());
            }
        }
    }
    values
}

fn flow_ignored_children_failures(
    parent: &Job,
    jobs: &HashMap<JobId, Job>,
) -> JobFlowIgnoredFailures {
    let mut failures = BTreeMap::new();
    for child_id in &parent.child_ids {
        let Some(child) = jobs.get(child_id) else {
            continue;
        };
        if child.state == JobState::Failed
            && (child.options.ignore_dependency_on_failure
                || child.options.continue_parent_on_failure)
        {
            failures.insert(
                child.id.clone(),
                child.failed_reason.clone().unwrap_or_default(),
            );
        }
    }
    failures
}

fn child_failure_releases_dependency(child: &Job) -> bool {
    child.options.ignore_dependency_on_failure
        || child.options.remove_dependency_on_failure
        || child.options.continue_parent_on_failure
}

fn collect_removable_unprocessed_children(
    jobs: &HashMap<JobId, Job>,
    child_ids: &[JobId],
    out: &mut Vec<JobId>,
    seen: &mut HashSet<JobId>,
) {
    for child_id in child_ids {
        if !seen.insert(child_id.clone()) {
            continue;
        }
        let Some(child) = jobs.get(child_id) else {
            continue;
        };
        if child.state == JobState::Active || child.state.is_terminal() {
            continue;
        }
        collect_removable_unprocessed_children(jobs, &child.child_ids, out, seen);
        out.push(child.id.clone());
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
    released_owners: &HashSet<(String, JobId)>,
    candidate: &Job,
    now: DateTime<Utc>,
) -> Option<&'a Job> {
    let deduplication_id = active_deduplication_id(candidate, now)?;
    find_active_deduplication_id(jobs, released_owners, deduplication_id, now)
}

fn find_active_deduplication_id<'a>(
    jobs: &'a HashMap<JobId, Job>,
    released_owners: &HashSet<(String, JobId)>,
    deduplication_id: &str,
    now: DateTime<Utc>,
) -> Option<&'a Job> {
    jobs.values().find(|job| {
        active_deduplication_id(job, now) == Some(deduplication_id)
            && !deduplication_owner_released(released_owners, deduplication_id, &job.id)
    })
}

fn find_active_deduplication_id_except<'a>(
    jobs: &'a HashMap<JobId, Job>,
    released_owners: &HashSet<(String, JobId)>,
    deduplication_id: &str,
    excluded_job_id: &str,
    now: DateTime<Utc>,
) -> Option<&'a Job> {
    jobs.values().find(|job| {
        job.id != excluded_job_id
            && active_deduplication_id(job, now) == Some(deduplication_id)
            && !deduplication_owner_released(released_owners, deduplication_id, &job.id)
    })
}

fn deduplication_owner_released(
    released_owners: &HashSet<(String, JobId)>,
    deduplication_id: &str,
    job_id: &str,
) -> bool {
    released_owners.contains(&(deduplication_id.to_string(), job_id.to_string()))
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
        && candidate.repeat_key.as_deref() == existing.repeat_key.as_deref()
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
    job.deferred_failure = None;
    job.failed_reason = None;
    job.return_value = None;
    job.progress = None;
    job.logs.clear();
    job.deduplication_expires_at = deduplication_expiration(&job.options, now);
}

fn repeat_keep_last_next_count(owner: &Job, candidate: &Job) -> Option<Option<u32>> {
    let Some(owner_repeat_key) = owner.repeat_key.as_deref() else {
        return Some(None);
    };
    if candidate.repeat_key.as_deref() != Some(owner_repeat_key) {
        return None;
    }

    let next_count = owner.repeat_count.saturating_add(1);
    if matches!(
        owner.options.repeat.as_ref().and_then(|repeat| repeat.limit),
        Some(limit) if next_count >= limit
    ) {
        return None;
    }

    Some(Some(next_count))
}

fn active_repeat_key(job: &Job) -> Option<&str> {
    if job.state.is_terminal() {
        return None;
    }

    job.repeat_key.as_deref()
}

fn is_repeat_scheduler_job(job: &Job) -> bool {
    active_repeat_key(job).is_some() && job.options.repeat.is_some()
}

fn repeat_entry(job: &Job) -> Option<JobRepeatEntry> {
    let key = active_repeat_key(job)?.to_string();
    let options = job.options.repeat.clone()?;
    Some(JobRepeatEntry {
        key,
        job_id: job.id.clone(),
        name: job.name.clone(),
        state: job.state,
        scheduled_at: job.scheduled_at,
        repeat_count: job.repeat_count,
        options,
    })
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

fn ensure_flow_dependencies_are_resolved(
    job: &Job,
    jobs: &HashMap<JobId, Job>,
    action: &str,
) -> Result<()> {
    let counts = flow_dependency_counts(job, jobs);
    if counts.unprocessed > 0 {
        return Err(LaneError::JobStateConflict(format!(
            "cannot {action} job {}; it has {} pending flow dependencies",
            job.id, counts.unprocessed
        )));
    }
    if counts.failed > 0 {
        return Err(LaneError::JobStateConflict(format!(
            "cannot {action} job {}; it has {} failed flow dependencies",
            job.id, counts.failed
        )));
    }
    Ok(())
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
