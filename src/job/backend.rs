use super::types::{
    Job, JobListOptions, JobListPage, JobOptions, JobQueueStats, JobState, JobWorkerId,
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

    async fn claim_next(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>>;

    async fn complete_job(&self, job_id: &str, value: Value, now: DateTime<Utc>) -> Result<Job>;

    async fn fail_job(&self, job_id: &str, error: String, now: DateTime<Utc>) -> Result<Job>;

    async fn renew_lease(
        &self,
        job_id: &str,
        worker_id: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    async fn promote_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job>;

    async fn retry_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job>;

    async fn remove_job(&self, job_id: &str) -> Result<Option<Job>>;

    async fn clean_jobs(
        &self,
        state: JobState,
        grace: Duration,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>>;

    async fn list_jobs(&self, options: JobListOptions) -> Result<JobListPage>;

    async fn update_progress(&self, job_id: &str, progress: Value) -> Result<Job>;

    async fn add_log(
        &self,
        job_id: &str,
        line: String,
        keep: usize,
        now: DateTime<Utc>,
    ) -> Result<Job>;

    async fn promote_due_jobs(&self, now: DateTime<Utc>) -> Result<usize>;

    async fn recover_stalled_jobs(&self, now: DateTime<Utc>) -> Result<usize>;

    async fn pause(&self) -> Result<()>;

    async fn resume(&self) -> Result<()>;

    async fn get_job(&self, job_id: &str) -> Result<Option<Job>>;

    async fn stats(&self) -> Result<JobQueueStats>;
}
