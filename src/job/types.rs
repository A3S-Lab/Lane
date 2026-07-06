use crate::error::{LaneError, Result};
use crate::retry::RetryPolicy;
use chrono::{DateTime, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;
use std::time::Duration;
use uuid::Uuid;

/// Unique identifier for a generic queue job.
pub type JobId = String;

/// Queue name for a generic job queue.
pub type QueueName = String;

/// Worker identifier used for leased processing.
pub type JobWorkerId = String;

/// Opaque token proving ownership of a claimed job lease.
pub type JobLockToken = String;

/// Job priority. Lower values run first.
pub type JobPriority = u32;

/// Default priority for jobs that do not specify one.
pub const DEFAULT_JOB_PRIORITY: JobPriority = 1000;

/// Queue-level rate limit for claiming generic jobs.
///
/// The limit is counted when a job is successfully moved from waiting to
/// active. Workers that hit the limit simply receive no job and can poll again
/// later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobRateLimit {
    /// Maximum number of claimed jobs in the window.
    pub max_claims: u64,
    /// Window duration.
    pub window: Duration,
}

impl JobRateLimit {
    /// Create a rate limit for claimed jobs.
    pub fn new(max_claims: u64, window: Duration) -> Self {
        Self { max_claims, window }
    }

    /// Limit claimed jobs per second.
    pub fn per_second(max_claims: u64) -> Self {
        Self::new(max_claims, Duration::from_secs(1))
    }

    /// Limit claimed jobs per minute.
    pub fn per_minute(max_claims: u64) -> Self {
        Self::new(max_claims, Duration::from_secs(60))
    }

    /// Validate the rate limit values.
    pub fn validate(&self) -> Result<()> {
        if self.max_claims == 0 {
            return Err(LaneError::ConfigError(
                "job claim rate limit max_claims must be greater than zero".to_string(),
            ));
        }
        if self.window.is_zero() {
            return Err(LaneError::ConfigError(
                "job claim rate limit window must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

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

/// A retained log line for a generic job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobLogEntry {
    pub timestamp: DateTime<Utc>,
    pub line: String,
}

/// Options for listing jobs from a backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobListOptions {
    /// Optional state filter. `None` lists jobs from all states.
    pub state: Option<JobState>,
    /// Number of matching jobs to skip.
    pub offset: usize,
    /// Maximum number of jobs to return.
    pub limit: usize,
}

impl Default for JobListOptions {
    fn default() -> Self {
        Self {
            state: None,
            offset: 0,
            limit: 100,
        }
    }
}

impl JobListOptions {
    /// Create default list options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict results to a single state.
    pub fn with_state(mut self, state: JobState) -> Self {
        self.state = Some(state);
        self
    }

    /// Set the pagination offset.
    pub fn with_offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// Set the maximum result count.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// A page of jobs returned by a backend list operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobListPage {
    pub jobs: Vec<Job>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

/// Job input used when creating a flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobSpec {
    pub name: String,
    pub payload: Value,
    pub options: JobOptions,
}

impl JobSpec {
    /// Create a job specification with default options.
    pub fn new(name: impl Into<String>, payload: Value) -> Self {
        Self {
            name: name.into(),
            payload,
            options: JobOptions::new(),
        }
    }

    /// Attach explicit job options.
    pub fn with_options(mut self, options: JobOptions) -> Self {
        self.options = options;
        self
    }
}

/// Jobs created by a parent-child flow submission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobFlow {
    pub parent: Job,
    pub children: Vec<Job>,
}

/// Repeat schedule used by a generic job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RepeatSchedule {
    /// Repeat at a fixed interval after each successful completion.
    Every {
        /// Delay between completed occurrence and the next scheduled occurrence.
        interval: Duration,
    },
    /// Repeat on a cron expression in UTC.
    Cron {
        /// Seven-field cron expression: second, minute, hour, day of month,
        /// month, day of week, and year.
        #[serde(rename = "cron")]
        expression: String,
    },
}

/// Repeat settings for a generic job.
///
/// `limit` counts total executions, including the first job. For example,
/// `limit = 3` allows the original job plus two automatically scheduled
/// successors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepeatOptions {
    /// The repeat schedule. This is flattened in JSON so legacy interval
    /// snapshots keep using the `{ "interval": ... }` shape.
    #[serde(flatten)]
    pub schedule: RepeatSchedule,
    /// Optional maximum total execution count for this repeat series.
    pub limit: Option<u32>,
    /// Optional latest scheduled time for a new occurrence.
    pub end_at: Option<DateTime<Utc>>,
    /// Optional stable key that groups occurrences from the same series.
    pub key: Option<String>,
}

impl RepeatOptions {
    /// Repeat at a fixed interval after each successful completion.
    pub fn every(interval: Duration) -> Self {
        Self {
            schedule: RepeatSchedule::Every { interval },
            limit: None,
            end_at: None,
            key: None,
        }
    }

    /// Repeat according to a UTC cron expression.
    ///
    /// The expression uses seven fields: second, minute, hour, day of month,
    /// month, day of week, and year.
    pub fn cron(expression: impl Into<String>) -> Self {
        Self {
            schedule: RepeatSchedule::Cron {
                expression: expression.into(),
            },
            limit: None,
            end_at: None,
            key: None,
        }
    }

    /// Return the fixed interval when this repeat uses interval scheduling.
    pub fn interval(&self) -> Option<Duration> {
        match &self.schedule {
            RepeatSchedule::Every { interval } => Some(*interval),
            RepeatSchedule::Cron { .. } => None,
        }
    }

    /// Return the cron expression when this repeat uses cron scheduling.
    pub fn cron_expression(&self) -> Option<&str> {
        match &self.schedule {
            RepeatSchedule::Every { .. } => None,
            RepeatSchedule::Cron { expression } => Some(expression),
        }
    }

    /// Limit total executions, including the first occurrence.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Stop scheduling new occurrences after this timestamp.
    pub fn until(mut self, end_at: DateTime<Utc>) -> Self {
        self.end_at = Some(end_at);
        self
    }

    /// Set a stable repeat-series key.
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        match &self.schedule {
            RepeatSchedule::Every { interval } => {
                if interval.is_zero() {
                    return Err(LaneError::ConfigError(
                        "repeat interval must be greater than zero".to_string(),
                    ));
                }
            }
            RepeatSchedule::Cron { expression } => {
                parse_cron_expression(expression)?;
            }
        }

        if self.limit == Some(0) {
            return Err(LaneError::ConfigError(
                "repeat limit must be greater than zero".to_string(),
            ));
        }

        Ok(())
    }

    pub(crate) fn next_scheduled_at(&self, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        let scheduled_at = match &self.schedule {
            RepeatSchedule::Every { interval } => Some(add_duration(after, *interval)),
            RepeatSchedule::Cron { expression } => {
                let schedule = parse_cron_expression(expression)?;
                schedule.after(&after).next()
            }
        };

        Ok(scheduled_at)
    }
}

/// Simple deduplication settings for a generic job.
///
/// Jobs with the same deduplication id are coalesced while the first job is
/// still in a non-terminal state. The deduplication id is released when that
/// job completes, fails terminally, is removed, or its optional TTL expires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeduplicationOptions {
    /// Queue-local id used to coalesce duplicate submissions.
    pub id: String,
    /// Optional owner-key TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Duration>,
    /// Replace an existing delayed owner with the new job.
    #[serde(default)]
    pub replace: bool,
    /// Keep the latest duplicate while the current owner is active.
    #[serde(default)]
    pub keep_last_if_active: bool,
}

impl DeduplicationOptions {
    /// Create simple deduplication options.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ttl: None,
            replace: false,
            keep_last_if_active: false,
        }
    }

    /// Set how long this job owns its deduplication id.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Replace the current delayed owner instead of returning it.
    ///
    /// This mirrors BullMQ's replace path for delayed deduplicated jobs. Active
    /// keep-last-if-active behavior is a separate mode and is not enabled here.
    pub fn replace_delayed(mut self, replace: bool) -> Self {
        self.replace = replace;
        self
    }

    /// Store the latest duplicate while the current owner is active.
    ///
    /// This mirrors BullMQ's `keepLastIfActive` mechanism for standalone jobs:
    /// duplicate adds return the active owner, but the latest duplicate is
    /// materialized as a new job when the owner finishes terminally.
    pub fn keep_last_if_active(mut self, keep: bool) -> Self {
        self.keep_last_if_active = keep;
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(LaneError::ConfigError(
                "deduplication id must not be empty".to_string(),
            ));
        }
        if matches!(self.ttl, Some(ttl) if ttl.is_zero()) {
            return Err(LaneError::ConfigError(
                "deduplication ttl must be greater than zero".to_string(),
            ));
        }

        Ok(())
    }
}

/// Options used when adding a generic queue job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobOptions {
    /// Optional caller-assigned id used for idempotent job submission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<JobId>,
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
    /// Optional repeat schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<RepeatOptions>,
    /// Optional simple deduplication settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deduplication: Option<DeduplicationOptions>,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            job_id: None,
            priority: DEFAULT_JOB_PRIORITY,
            delay: None,
            retry_policy: RetryPolicy::none(),
            timeout: None,
            remove_on_complete: false,
            remove_on_fail: false,
            max_stalled_count: 1,
            repeat: None,
            deduplication: None,
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

    /// Set a caller-assigned id for idempotent job submission.
    pub fn with_job_id(mut self, job_id: impl Into<String>) -> Self {
        self.job_id = Some(job_id.into());
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

    /// Configure repeat scheduling.
    pub fn with_repeat(mut self, repeat: RepeatOptions) -> Self {
        self.repeat = Some(repeat);
        self
    }

    /// Coalesce duplicate submissions while a matching job is still non-terminal.
    pub fn with_deduplication_id(mut self, id: impl Into<String>) -> Self {
        self.deduplication = Some(DeduplicationOptions::new(id));
        self
    }

    /// Configure deduplication with explicit options such as TTL.
    pub fn with_deduplication(mut self, deduplication: DeduplicationOptions) -> Self {
        self.deduplication = Some(deduplication);
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if matches!(self.job_id.as_deref(), Some(job_id) if job_id.trim().is_empty()) {
            return Err(LaneError::ConfigError(
                "job id must not be empty".to_string(),
            ));
        }

        if let Some(repeat) = &self.repeat {
            repeat.validate()?;
        }

        if let Some(deduplication) = &self.deduplication {
            deduplication.validate()?;
        }

        Ok(())
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
    #[serde(default, skip)]
    pub lock_token: Option<JobLockToken>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub failed_reason: Option<String>,
    pub return_value: Option<Value>,
    pub progress: Option<Value>,
    pub logs: Vec<JobLogEntry>,
    pub parent_id: Option<JobId>,
    pub child_ids: Vec<JobId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_key: Option<String>,
    #[serde(default)]
    pub repeat_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deduplication_expires_at: Option<DateTime<Utc>>,
}

impl Job {
    pub(crate) fn new(
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
        let repeat_key = options.repeat.as_ref().map(|repeat| {
            repeat
                .key
                .clone()
                .unwrap_or_else(|| format!("{queue}:{name}"))
        });
        let deduplication_expires_at = deduplication_expiration(&options, now);
        let id = options
            .job_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        Self {
            id,
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
            lock_token: None,
            lease_expires_at: None,
            failed_reason: None,
            return_value: None,
            progress: None,
            logs: Vec::new(),
            parent_id: None,
            child_ids: Vec::new(),
            repeat_key,
            repeat_count: 0,
            deduplication_expires_at,
        }
    }
}

pub(crate) fn deduplication_expiration(
    options: &JobOptions,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    options
        .deduplication
        .as_ref()
        .filter(|deduplication| !deduplication.keep_last_if_active)
        .and_then(|deduplication| deduplication.ttl)
        .map(|ttl| add_duration(now, ttl))
}

pub(crate) fn add_duration(at: DateTime<Utc>, duration: Duration) -> DateTime<Utc> {
    match chrono::Duration::from_std(duration) {
        Ok(delta) => at.checked_add_signed(delta).unwrap_or(at),
        Err(_) => at,
    }
}

fn parse_cron_expression(expression: &str) -> Result<Schedule> {
    let expression = expression.trim();
    if expression.is_empty() {
        return Err(LaneError::ConfigError(
            "repeat cron expression must not be empty".to_string(),
        ));
    }

    Schedule::from_str(expression).map_err(|error| {
        LaneError::ConfigError(format!(
            "invalid repeat cron expression `{expression}`: {error}"
        ))
    })
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

/// Serializable snapshot used by durable job backends.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobQueueSnapshot {
    pub queue: QueueName,
    pub paused: bool,
    pub jobs: Vec<Job>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deduplication_next_jobs: Vec<Job>,
}
