use super::types::{
    page_repeat_entries, Job, JobEvent, JobFinishedResult, JobFlow, JobFlowChildValues,
    JobFlowDependencies, JobFlowDependencyCountOptions, JobFlowDependencyCounts,
    JobFlowDependencyPage, JobFlowDependencyPageOptions, JobFlowDependencyPages,
    JobFlowDependencyPagesOptions, JobFlowDependencySelectedCounts, JobFlowDependencyValues,
    JobFlowIgnoredFailures, JobId, JobLeaseRenewal, JobListOptions, JobListPage, JobLogPage,
    JobOptions, JobPriority, JobPriorityCount, JobQueueStats, JobRepeatEntry, JobRepeatListOptions,
    JobRepeatPage, JobSpec, JobState, JobStateCount, JobWorkerId,
};
use crate::error::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::time::Duration;

/// Backend contract for a durable distributed job queue.
#[async_trait]
pub trait JobQueueBackend: Send + Sync {
    async fn add_job(&self, name: String, payload: Value, options: JobOptions) -> Result<Job>;

    /// Add multiple jobs, preserving input order and `add_job` idempotency semantics.
    async fn add_jobs(&self, jobs: Vec<JobSpec>, now: DateTime<Utc>) -> Result<Vec<Job>>;

    async fn add_flow(
        &self,
        parent: JobSpec,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<JobFlow>;

    /// Add children to an active parent and move that parent to `waiting_children`.
    ///
    /// This is the same-queue dynamic fan-out counterpart to BullMQ's
    /// `moveToWaitingChildren()` flow path: the parent must be active and
    /// token-owned, child jobs are added atomically, and the parent is parked
    /// until its new dependencies resolve.
    async fn add_flow_children(
        &self,
        parent_id: &str,
        lock_token: &str,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>>;

    async fn get_flow_dependencies(&self, parent_id: &str) -> Result<Option<JobFlowDependencies>>;

    async fn get_flow_dependency_counts(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyCounts>>;

    /// Return selected BullMQ-style dependency counts for a flow parent.
    async fn get_flow_dependency_selected_counts(
        &self,
        parent_id: &str,
        options: JobFlowDependencyCountOptions,
    ) -> Result<Option<JobFlowDependencySelectedCounts>>;

    /// Return BullMQ-style full dependency buckets for a flow parent.
    async fn get_flow_dependency_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyValues>>;

    /// Return one cursor page from a parent flow dependency bucket.
    async fn get_flow_dependency_page(
        &self,
        parent_id: &str,
        options: JobFlowDependencyPageOptions,
    ) -> Result<Option<JobFlowDependencyPage>>;

    /// Return cursor pages from several parent flow dependency buckets.
    async fn get_flow_dependency_pages(
        &self,
        parent_id: &str,
        options: JobFlowDependencyPagesOptions,
    ) -> Result<Option<JobFlowDependencyPages>>;

    /// Return completed child result values for a flow parent.
    ///
    /// This mirrors BullMQ's `getChildrenValues()` fan-in getter: only completed
    /// children with retained return values are included.
    async fn get_flow_children_values(&self, parent_id: &str)
        -> Result<Option<JobFlowChildValues>>;

    /// Return ignored child failure reasons for a flow parent.
    ///
    /// This mirrors BullMQ's `getIgnoredChildrenFailures()` getter: only failed
    /// children configured with `ignoreDependencyOnFailure` are included.
    async fn get_flow_ignored_children_failures(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowIgnoredFailures>>;

    async fn remove_unprocessed_children(
        &self,
        parent_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Vec<Job>>>;

    async fn remove_child_dependency(&self, child_id: &str, now: DateTime<Utc>) -> Result<bool>;

    async fn claim_next(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>>;

    /// Claim the next job, optionally waiting for backend-native work signals.
    ///
    /// Backends that do not have a blocking primitive fall back to one immediate
    /// `claim_next()` attempt. Redis overrides this with its marker zset wait
    /// path, where the blocking wake-up is only a signal and the actual claim
    /// still runs through the normal atomic ownership script.
    async fn claim_next_blocking(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        _block_for: Duration,
    ) -> Result<Option<Job>> {
        self.claim_next(worker_id, lease_for, Utc::now()).await
    }

    async fn complete_job(
        &self,
        job_id: &str,
        lock_token: &str,
        value: Value,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    async fn fail_job(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    /// Fail an active job without applying its automatic retry policy.
    ///
    /// This mirrors BullMQ's runtime `discard()` behavior for the current failure
    /// path: the job still must be active and token-owned, but the failure is
    /// terminal even when retries remain.
    async fn fail_job_discarding_retry(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    async fn renew_lease(
        &self,
        job_id: &str,
        lock_token: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    /// Renew multiple active job leases.
    ///
    /// This mirrors BullMQ's `extendLocks` script shape: valid token-owned
    /// active jobs are renewed, and failures are returned as job ids instead of
    /// aborting the whole batch. Backend transport/script failures still return
    /// `Err`.
    async fn renew_leases(
        &self,
        renewals: &[JobLeaseRenewal],
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Vec<JobId>> {
        let mut failed = Vec::new();
        for renewal in renewals {
            if self
                .renew_lease(&renewal.job_id, &renewal.lock_token, lease_for, now)
                .await
                .is_err()
            {
                failed.push(renewal.job_id.clone());
            }
        }
        Ok(failed)
    }

    async fn delay_active_job(
        &self,
        job_id: &str,
        lock_token: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    async fn release_active_job(
        &self,
        job_id: &str,
        lock_token: &str,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    async fn promote_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job>;

    async fn reschedule_job(
        &self,
        job_id: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    /// Reprocess a retained failed or completed job by moving it back to waiting.
    async fn retry_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job>;

    /// Update an existing job's stored priority.
    ///
    /// Waiting jobs are reinserted into the ready index. Retained terminal jobs
    /// keep their terminal state and only update the stored snapshot, matching
    /// BullMQ's `changePriority` script behavior.
    async fn update_priority(&self, job_id: &str, priority: JobPriority) -> Result<Job>;

    /// Update priority and choose how the job is reinserted within the same-priority group.
    ///
    /// This mirrors BullMQ's `changePriority({ priority, lifo })` shape: waiting
    /// jobs get a fresh waiting index position, and `lifo = true` puts the job in
    /// the LIFO side of that priority range.
    async fn update_priority_with_lifo(
        &self,
        job_id: &str,
        priority: JobPriority,
        _lifo: bool,
    ) -> Result<Job> {
        self.update_priority(job_id, priority).await
    }

    async fn remove_job(&self, job_id: &str) -> Result<Option<Job>>;

    async fn remove_repeat(&self, repeat_key: &str) -> Result<Option<Job>>;

    async fn remove_deduplication_key(&self, deduplication_id: &str) -> Result<bool>;

    async fn get_deduplication_job_id(&self, deduplication_id: &str) -> Result<Option<JobId>>;

    async fn list_repeats(&self) -> Result<Vec<JobRepeatEntry>>;

    /// Create or replace the current non-active occurrence for a repeat series.
    ///
    /// This follows BullMQ's `upsertJobScheduler(..., override: true)` shape at
    /// the current Lane repeat-owner layer: the repeat key is taken from
    /// `spec.options.repeat.key` or falls back to Lane's `queue:name` key,
    /// non-active current owners are replaced, and active leased owners are
    /// rejected.
    async fn upsert_repeat(&self, spec: JobSpec, now: DateTime<Utc>) -> Result<Job>;

    /// Return one repeat series / job scheduler by key.
    async fn get_repeat(&self, repeat_key: &str) -> Result<Option<JobRepeatEntry>> {
        Ok(self
            .list_repeats()
            .await?
            .into_iter()
            .find(|entry| entry.key == repeat_key))
    }

    /// Return the number of current repeat series / job schedulers.
    async fn count_repeats(&self) -> Result<usize> {
        Ok(self.list_repeats().await?.len())
    }

    /// Return repeat series / job schedulers with BullMQ-style pagination.
    ///
    /// Results are ordered by next scheduled time; descending order is the
    /// default, matching BullMQ's `getJobSchedulers()`.
    async fn list_repeats_page(&self, options: JobRepeatListOptions) -> Result<JobRepeatPage> {
        Ok(page_repeat_entries(self.list_repeats().await?, options))
    }

    async fn clean_jobs(
        &self,
        state: JobState,
        grace: Duration,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>>;

    async fn drain_jobs(&self, include_delayed: bool) -> Result<Vec<Job>>;

    /// Remove all queue data.
    ///
    /// This follows BullMQ's `obliterate()` shape: the queue is paused first,
    /// active jobs are rejected unless `force` is true, and a successful
    /// obliteration removes the pause marker along with all queue data.
    async fn obliterate(&self, force: bool) -> Result<usize>;

    async fn list_jobs(&self, options: JobListOptions) -> Result<JobListPage>;

    /// Return counts for the requested states.
    ///
    /// Empty input returns all lifecycle states. Duplicate states are counted once,
    /// preserving the first requested order.
    async fn get_job_counts(&self, states: &[JobState]) -> Result<Vec<JobStateCount>>;

    /// Return the aggregate count for the requested states.
    ///
    /// This mirrors BullMQ's `getJobCountByTypes()` shape: it reuses per-state
    /// counts, so empty input means all states and duplicate states are counted
    /// once.
    async fn get_job_count(&self, states: &[JobState]) -> Result<usize> {
        let counts = self.get_job_counts(states).await?;
        Ok(counts.into_iter().map(|count| count.count).sum())
    }

    /// Return jobs that are waiting to be processed.
    ///
    /// This follows BullMQ's queue `count()` meaning: waiting, delayed, and
    /// waiting-children jobs are included; active and terminal jobs are not.
    async fn count_pending_jobs(&self) -> Result<usize> {
        self.get_job_count(JobState::PENDING.as_slice()).await
    }

    /// Return waiting-job counts for the requested priorities.
    ///
    /// Duplicate priorities are counted once, preserving the first requested order.
    async fn get_counts_per_priority(
        &self,
        priorities: &[JobPriority],
    ) -> Result<Vec<JobPriorityCount>>;

    async fn update_data(&self, job_id: &str, payload: Value) -> Result<Job>;

    /// Update an existing job's stored progress and emit a progress event.
    ///
    /// This mirrors BullMQ's `updateProgress` script behavior: any retained job
    /// can be updated, including terminal jobs, and missing job records return
    /// `JobNotFound`.
    async fn update_progress(&self, job_id: &str, progress: Value) -> Result<Job>;

    /// Save retained failure diagnostics for a job.
    ///
    /// This mirrors BullMQ's `saveStacktrace` script shape: any retained job can
    /// be updated, the stacktrace replaces the previous retained stacktrace
    /// array, and missing job records return `JobNotFound`.
    async fn save_stacktrace(
        &self,
        job_id: &str,
        stacktrace: Vec<String>,
        failed_reason: String,
    ) -> Result<Job>;

    async fn add_log(
        &self,
        job_id: &str,
        line: String,
        keep: usize,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    async fn get_job_logs(
        &self,
        job_id: &str,
        start: isize,
        end: isize,
        ascending: bool,
    ) -> Result<JobLogPage>;

    /// Clear retained job logs, optionally keeping the newest entries.
    ///
    /// This mirrors BullMQ's `clearLogs()` storage behavior: `keep == 0`
    /// removes the log list, while positive values keep the newest `keep`
    /// entries.
    async fn clear_job_logs(&self, job_id: &str, keep: usize) -> Result<JobLogPage>;

    /// Read retained queue events in stream-id order.
    ///
    /// `start` and `end` follow Redis stream range semantics for the supported
    /// forms: `-`, `+`, or concrete `<milliseconds>-<sequence>` ids. `limit ==
    /// 0` returns no events.
    async fn read_events(&self, _start: &str, _end: &str, _limit: usize) -> Result<Vec<JobEvent>> {
        Ok(Vec::new())
    }

    /// Trim retained queue events to approximately `max_len` entries.
    async fn trim_events(&self, _max_len: usize) -> Result<usize> {
        Ok(0)
    }

    async fn promote_due_jobs(&self, now: DateTime<Utc>) -> Result<usize>;

    async fn recover_stalled_jobs(&self, now: DateTime<Utc>) -> Result<usize>;

    async fn pause(&self) -> Result<()>;

    async fn resume(&self) -> Result<()>;

    /// Return whether this queue is currently paused.
    async fn is_paused(&self) -> Result<bool>;

    async fn get_job(&self, job_id: &str) -> Result<Option<Job>>;

    async fn get_job_state(&self, job_id: &str) -> Result<Option<JobState>>;

    /// Return whether a retained job has finished and include its terminal payload.
    ///
    /// This mirrors BullMQ's `isFinished(..., returnValue=true)` Redis shape at
    /// the Lane type level: missing retained records return `None`, non-terminal
    /// jobs return `NotFinished`, and terminal jobs return the retained success
    /// value or failure reason.
    async fn get_job_finished_result(&self, job_id: &str) -> Result<Option<JobFinishedResult>>;

    async fn stats(&self) -> Result<JobQueueStats>;
}
