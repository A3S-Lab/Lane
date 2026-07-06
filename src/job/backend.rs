use super::types::{
    Job, JobFlow, JobFlowDependencies, JobListOptions, JobListPage, JobLogPage, JobOptions,
    JobPriority, JobQueueStats, JobRepeatEntry, JobSpec, JobState, JobWorkerId,
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

    async fn get_flow_dependencies(&self, parent_id: &str) -> Result<Option<JobFlowDependencies>>;

    async fn claim_next(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>>;

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

    async fn renew_lease(
        &self,
        job_id: &str,
        lock_token: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    async fn delay_active_job(
        &self,
        job_id: &str,
        lock_token: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    async fn promote_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job>;

    async fn reschedule_job(
        &self,
        job_id: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    async fn retry_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job>;

    async fn update_priority(&self, job_id: &str, priority: JobPriority) -> Result<Job>;

    async fn remove_job(&self, job_id: &str) -> Result<Option<Job>>;

    async fn remove_repeat(&self, repeat_key: &str) -> Result<Option<Job>>;

    async fn remove_deduplication_key(&self, deduplication_id: &str) -> Result<bool>;

    async fn list_repeats(&self) -> Result<Vec<JobRepeatEntry>>;

    async fn clean_jobs(
        &self,
        state: JobState,
        grace: Duration,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>>;

    async fn drain_jobs(&self, include_delayed: bool) -> Result<Vec<Job>>;

    async fn list_jobs(&self, options: JobListOptions) -> Result<JobListPage>;

    async fn update_progress(&self, job_id: &str, progress: Value) -> Result<Job>;

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

    async fn promote_due_jobs(&self, now: DateTime<Utc>) -> Result<usize>;

    async fn recover_stalled_jobs(&self, now: DateTime<Utc>) -> Result<usize>;

    async fn pause(&self) -> Result<()>;

    async fn resume(&self) -> Result<()>;

    async fn get_job(&self, job_id: &str) -> Result<Option<Job>>;

    async fn get_job_state(&self, job_id: &str) -> Result<Option<JobState>>;

    async fn stats(&self) -> Result<JobQueueStats>;
}
