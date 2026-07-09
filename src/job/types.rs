use crate::error::{LaneError, Result};
use crate::retry::RetryPolicy;
use chrono::{DateTime, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
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

/// Job lease ownership tuple used for batch renewal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobLeaseRenewal {
    pub job_id: JobId,
    pub lock_token: JobLockToken,
}

impl JobLeaseRenewal {
    /// Create a lease renewal request for a claimed job.
    pub fn new(job_id: impl Into<JobId>, lock_token: impl Into<JobLockToken>) -> Self {
        Self {
            job_id: job_id.into(),
            lock_token: lock_token.into(),
        }
    }
}

/// Job priority. Lower values run first.
pub type JobPriority = u32;

/// Default priority for jobs that do not specify one.
pub const DEFAULT_JOB_PRIORITY: JobPriority = 1000;

/// Maximum BullMQ-compatible priority. Lower values run first.
pub const MAX_JOB_PRIORITY: JobPriority = 2_u32.pow(21);

pub(crate) fn validate_job_priority(priority: JobPriority) -> Result<()> {
    if priority > MAX_JOB_PRIORITY {
        return Err(LaneError::ConfigError(format!(
            "priority must be between 0 and {MAX_JOB_PRIORITY}"
        )));
    }

    Ok(())
}

/// Default retained queue event count, matching BullMQ's default stream length.
pub const DEFAULT_JOB_EVENT_RETENTION: usize = 10_000;

/// Default retained Redis job metric data points.
pub const DEFAULT_JOB_METRICS_RETENTION: usize = 10_000;

/// Waiting-job count for a priority value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobPriorityCount {
    pub priority: JobPriority,
    pub count: usize,
}

/// BullMQ-style terminal job metrics metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobMetricsMeta {
    /// Total number of terminal transitions recorded for the metric type.
    pub count: usize,
    /// Previous closed metric timestamp in milliseconds, or `0` before one exists.
    pub previous_timestamp_millis: i64,
    /// Counter value at the previous closed metric timestamp.
    pub previous_count: usize,
}

/// BullMQ-style per-minute terminal job metrics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobMetrics {
    pub meta: JobMetricsMeta,
    /// Newest-to-oldest per-minute closed-window counts.
    pub data: Vec<usize>,
    /// Total number of retained data points.
    pub count: usize,
}

/// Retained job queue event.
///
/// Redis-backed queues store these entries in a Redis stream, while memory and
/// local queues keep the same stream-id shape in their snapshots.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobEvent {
    /// Redis-stream-style event id: `<milliseconds>-<sequence>`.
    pub id: String,
    /// Event name, for example `added`, `waiting`, `active`, or `completed`.
    pub event: String,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
    /// Job associated with this event, when the event is job-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<JobId>,
    /// Previous lifecycle state for state-transition events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<JobState>,
    /// Additional event fields such as job name, progress data, or failure reason.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, Value>,
}

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
    /// All durable lifecycle states in queue-count order.
    pub const ALL: [Self; 6] = [
        Self::Waiting,
        Self::Delayed,
        Self::Active,
        Self::WaitingChildren,
        Self::Completed,
        Self::Failed,
    ];

    /// States counted as pending work, matching BullMQ's queue `count()` shape.
    pub const PENDING: [Self; 3] = [Self::Waiting, Self::Delayed, Self::WaitingChildren];

    /// Whether this state is terminal and should not be claimed again.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// Job count for a lifecycle state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobStateCount {
    pub state: JobState,
    pub count: usize,
}

/// A retained log line for a generic job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobLogEntry {
    pub timestamp: DateTime<Utc>,
    pub line: String,
}

/// A page of retained log entries for a generic job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobLogPage {
    pub logs: Vec<JobLogEntry>,
    pub count: usize,
}

/// Options for listing jobs from a backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobListOptions {
    /// Optional legacy single-state filter. `None` lists jobs from all states.
    pub state: Option<JobState>,
    /// Optional multi-state filter. When non-empty, this takes precedence over `state`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<JobState>,
    /// Number of matching jobs to skip.
    pub offset: usize,
    /// Maximum number of jobs to return.
    pub limit: usize,
    /// Return jobs in ascending order when true, descending order when false.
    #[serde(default = "default_list_ascending")]
    pub ascending: bool,
}

impl Default for JobListOptions {
    fn default() -> Self {
        Self {
            state: None,
            states: Vec::new(),
            offset: 0,
            limit: 100,
            ascending: true,
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
        self.states.clear();
        self
    }

    /// Restrict results to one or more states.
    ///
    /// Empty input lists all lifecycle states. Duplicate states are removed while
    /// preserving the caller's order.
    pub fn with_states(mut self, states: impl IntoIterator<Item = JobState>) -> Self {
        self.state = None;
        self.states.clear();
        for state in states {
            if !self.states.contains(&state) {
                self.states.push(state);
            }
        }
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

    /// Return jobs in ascending order.
    pub fn ascending(mut self) -> Self {
        self.ascending = true;
        self
    }

    /// Return jobs in descending order.
    pub fn descending(mut self) -> Self {
        self.ascending = false;
        self
    }

    /// Configure result direction explicitly.
    pub fn with_ascending(mut self, ascending: bool) -> Self {
        self.ascending = ascending;
        self
    }

    pub(crate) fn selected_states(&self) -> Vec<JobState> {
        let mut states = Vec::new();
        if !self.states.is_empty() {
            for &state in &self.states {
                if !states.contains(&state) {
                    states.push(state);
                }
            }
        } else if let Some(state) = self.state {
            states.push(state);
        } else {
            states.extend_from_slice(JobState::ALL.as_slice());
        }
        states
    }
}

fn default_list_ascending() -> bool {
    true
}

fn default_repeat_list_ascending() -> bool {
    false
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

/// Current dependency snapshot for a flow parent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobFlowDependencies {
    /// Parent job snapshot.
    pub parent: Job,
    /// Existing child job snapshots in the parent's child order.
    pub children: Vec<Job>,
    /// Child ids that still block a `waiting_children` parent.
    pub pending_child_ids: Vec<JobId>,
    /// Child ids recorded on the parent but no longer retained.
    pub missing_child_ids: Vec<JobId>,
}

/// Dependency counts for a flow parent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobFlowDependencyCounts {
    /// Retained children that completed successfully.
    pub processed: usize,
    /// Retained children that are still waiting, delayed, active, or waiting on children.
    pub unprocessed: usize,
    /// Retained children that failed terminally.
    pub failed: usize,
    /// Retained failed children that no longer block the parent.
    pub ignored: usize,
    /// Child ids recorded on the parent but no longer retained.
    pub missing: usize,
}

/// Flow dependency count buckets to read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobFlowDependencyCountOptions {
    pub processed: bool,
    pub unprocessed: bool,
    pub ignored: bool,
    pub failed: bool,
}

impl JobFlowDependencyCountOptions {
    /// Create empty count options; empty options default to all buckets when read.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read all BullMQ dependency count buckets.
    pub fn all() -> Self {
        Self {
            processed: true,
            unprocessed: true,
            ignored: true,
            failed: true,
        }
    }

    /// Include the processed bucket.
    pub fn with_processed(mut self, enabled: bool) -> Self {
        self.processed = enabled;
        self
    }

    /// Include the unprocessed bucket.
    pub fn with_unprocessed(mut self, enabled: bool) -> Self {
        self.unprocessed = enabled;
        self
    }

    /// Include the ignored-failure bucket.
    pub fn with_ignored(mut self, enabled: bool) -> Self {
        self.ignored = enabled;
        self
    }

    /// Include the fail-parent bucket.
    pub fn with_failed(mut self, enabled: bool) -> Self {
        self.failed = enabled;
        self
    }

    /// Include one bucket by kind.
    pub fn with_kind(mut self, kind: JobFlowDependencyKind, enabled: bool) -> Self {
        match kind {
            JobFlowDependencyKind::Processed => self.processed = enabled,
            JobFlowDependencyKind::Unprocessed => self.unprocessed = enabled,
            JobFlowDependencyKind::Ignored => self.ignored = enabled,
            JobFlowDependencyKind::Failed => self.failed = enabled,
        }
        self
    }

    pub(crate) fn selected(&self) -> Vec<JobFlowDependencyKind> {
        let mut kinds = Vec::new();
        if self.processed {
            kinds.push(JobFlowDependencyKind::Processed);
        }
        if self.unprocessed {
            kinds.push(JobFlowDependencyKind::Unprocessed);
        }
        if self.ignored {
            kinds.push(JobFlowDependencyKind::Ignored);
        }
        if self.failed {
            kinds.push(JobFlowDependencyKind::Failed);
        }
        if kinds.is_empty() {
            kinds.extend_from_slice(&[
                JobFlowDependencyKind::Processed,
                JobFlowDependencyKind::Unprocessed,
                JobFlowDependencyKind::Ignored,
                JobFlowDependencyKind::Failed,
            ]);
        }
        kinds
    }
}

/// Selected BullMQ-style dependency counts for a flow parent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobFlowDependencySelectedCounts {
    pub processed: Option<usize>,
    pub unprocessed: Option<usize>,
    pub ignored: Option<usize>,
    pub failed: Option<usize>,
}

impl JobFlowDependencySelectedCounts {
    /// Return the count for one dependency bucket, when it was requested.
    pub fn get(&self, kind: JobFlowDependencyKind) -> Option<usize> {
        match kind {
            JobFlowDependencyKind::Processed => self.processed,
            JobFlowDependencyKind::Unprocessed => self.unprocessed,
            JobFlowDependencyKind::Ignored => self.ignored,
            JobFlowDependencyKind::Failed => self.failed,
        }
    }

    pub(crate) fn insert(&mut self, kind: JobFlowDependencyKind, count: usize) {
        match kind {
            JobFlowDependencyKind::Processed => self.processed = Some(count),
            JobFlowDependencyKind::Unprocessed => self.unprocessed = Some(count),
            JobFlowDependencyKind::Ignored => self.ignored = Some(count),
            JobFlowDependencyKind::Failed => self.failed = Some(count),
        }
    }
}

/// BullMQ-style full dependency buckets for a flow parent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobFlowDependencyValues {
    /// Completed child return values from the parent-scoped `:processed` hash.
    pub processed: BTreeMap<JobId, Value>,
    /// Child ids that still block the parent in the pending dependency set.
    pub unprocessed: Vec<JobId>,
    /// Fail-parent child ids from the parent-scoped `:unsuccessful` zset.
    pub failed: Vec<JobId>,
    /// Ignored or continued failure reasons from the parent-scoped `:failed` hash.
    pub ignored: BTreeMap<JobId, String>,
}

/// Flow dependency bucket used by paginated dependency inspection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobFlowDependencyKind {
    /// Completed children stored in the parent-scoped `:processed` hash.
    Processed,
    /// Children still stored in the parent-scoped pending dependency set.
    Unprocessed,
    /// Ignored or continued failures stored in the parent-scoped `:failed` hash.
    Ignored,
    /// Fail-parent failures stored in the parent-scoped `:unsuccessful` zset.
    Failed,
}

#[cfg(feature = "redis-backend")]
impl JobFlowDependencyKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Processed => "processed",
            Self::Unprocessed => "unprocessed",
            Self::Ignored => "ignored",
            Self::Failed => "failed",
        }
    }
}

/// Options for reading one flow dependency bucket incrementally.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobFlowDependencyPageOptions {
    /// Dependency bucket to scan.
    pub kind: JobFlowDependencyKind,
    /// Redis cursor for hash/set buckets, or zset offset for failed dependencies.
    pub cursor: u64,
    /// Scan count hint or zset page size. BullMQ defaults this to 20.
    pub count: usize,
}

impl JobFlowDependencyPageOptions {
    /// Create dependency page options for one bucket.
    pub fn new(kind: JobFlowDependencyKind) -> Self {
        Self {
            kind,
            cursor: 0,
            count: 20,
        }
    }

    /// Set the cursor returned by the previous page.
    pub fn with_cursor(mut self, cursor: u64) -> Self {
        self.cursor = cursor;
        self
    }

    /// Set the scan count hint or zset page size.
    pub fn with_count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }
}

/// Cursor options shared by multi-bucket flow dependency page reads.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobFlowDependencyPageCursor {
    /// Redis cursor for hash/set buckets, or zset offset for failed dependencies.
    pub cursor: u64,
    /// Scan count hint or zset page size. BullMQ defaults this to 20.
    pub count: usize,
}

impl Default for JobFlowDependencyPageCursor {
    fn default() -> Self {
        Self {
            cursor: 0,
            count: 20,
        }
    }
}

impl JobFlowDependencyPageCursor {
    /// Create default cursor options for one bucket.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the cursor returned by the previous page.
    pub fn with_cursor(mut self, cursor: u64) -> Self {
        self.cursor = cursor;
        self
    }

    /// Set the scan count hint or zset page size.
    pub fn with_count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }
}

impl From<JobFlowDependencyPageOptions> for JobFlowDependencyPageCursor {
    fn from(options: JobFlowDependencyPageOptions) -> Self {
        Self {
            cursor: options.cursor,
            count: options.count,
        }
    }
}

/// Options for reading several flow dependency buckets in one backend call.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobFlowDependencyPagesOptions {
    pub processed: Option<JobFlowDependencyPageCursor>,
    pub unprocessed: Option<JobFlowDependencyPageCursor>,
    pub ignored: Option<JobFlowDependencyPageCursor>,
    pub failed: Option<JobFlowDependencyPageCursor>,
}

impl JobFlowDependencyPagesOptions {
    /// Create empty multi-bucket dependency page options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read all dependency buckets with default cursors and counts.
    pub fn all() -> Self {
        let cursor = JobFlowDependencyPageCursor::default();
        Self {
            processed: Some(cursor),
            unprocessed: Some(cursor),
            ignored: Some(cursor),
            failed: Some(cursor),
        }
    }

    /// Include the processed bucket.
    pub fn with_processed(mut self, cursor: JobFlowDependencyPageCursor) -> Self {
        self.processed = Some(cursor);
        self
    }

    /// Include the unprocessed bucket.
    pub fn with_unprocessed(mut self, cursor: JobFlowDependencyPageCursor) -> Self {
        self.unprocessed = Some(cursor);
        self
    }

    /// Include the ignored-failure bucket.
    pub fn with_ignored(mut self, cursor: JobFlowDependencyPageCursor) -> Self {
        self.ignored = Some(cursor);
        self
    }

    /// Include the fail-parent bucket.
    pub fn with_failed(mut self, cursor: JobFlowDependencyPageCursor) -> Self {
        self.failed = Some(cursor);
        self
    }

    /// Include one bucket by kind.
    pub fn with_kind(
        mut self,
        kind: JobFlowDependencyKind,
        cursor: JobFlowDependencyPageCursor,
    ) -> Self {
        match kind {
            JobFlowDependencyKind::Processed => self.processed = Some(cursor),
            JobFlowDependencyKind::Unprocessed => self.unprocessed = Some(cursor),
            JobFlowDependencyKind::Ignored => self.ignored = Some(cursor),
            JobFlowDependencyKind::Failed => self.failed = Some(cursor),
        }
        self
    }

    pub(crate) fn selected(&self) -> Vec<JobFlowDependencyPageOptions> {
        [
            (JobFlowDependencyKind::Processed, self.processed),
            (JobFlowDependencyKind::Unprocessed, self.unprocessed),
            (JobFlowDependencyKind::Ignored, self.ignored),
            (JobFlowDependencyKind::Failed, self.failed),
        ]
        .into_iter()
        .filter_map(|(kind, cursor)| {
            cursor.map(|cursor| JobFlowDependencyPageOptions {
                kind,
                cursor: cursor.cursor,
                count: cursor.count,
            })
        })
        .collect()
    }
}

/// One entry from a paginated flow dependency bucket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobFlowDependencyPageItem {
    /// Completed child return value.
    Processed { child_id: JobId, value: Value },
    /// Pending child id.
    Unprocessed { child_id: JobId },
    /// Ignored or continued child failure reason.
    Ignored {
        child_id: JobId,
        failed_reason: String,
    },
    /// Fail-parent child id.
    Failed { child_id: JobId },
}

/// A cursor page for one flow dependency bucket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobFlowDependencyPage {
    pub kind: JobFlowDependencyKind,
    pub items: Vec<JobFlowDependencyPageItem>,
    /// Next cursor to pass back for this same bucket. `0` means the scan is done.
    pub next_cursor: u64,
    /// Requested scan count hint or zset page size.
    pub count: usize,
}

/// Cursor pages for several requested flow dependency buckets.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobFlowDependencyPages {
    pub processed: Option<JobFlowDependencyPage>,
    pub unprocessed: Option<JobFlowDependencyPage>,
    pub ignored: Option<JobFlowDependencyPage>,
    pub failed: Option<JobFlowDependencyPage>,
}

impl JobFlowDependencyPages {
    /// Return the page for one dependency bucket, when it was requested.
    pub fn get(&self, kind: JobFlowDependencyKind) -> Option<&JobFlowDependencyPage> {
        match kind {
            JobFlowDependencyKind::Processed => self.processed.as_ref(),
            JobFlowDependencyKind::Unprocessed => self.unprocessed.as_ref(),
            JobFlowDependencyKind::Ignored => self.ignored.as_ref(),
            JobFlowDependencyKind::Failed => self.failed.as_ref(),
        }
    }

    pub(crate) fn insert(&mut self, page: JobFlowDependencyPage) {
        match page.kind {
            JobFlowDependencyKind::Processed => self.processed = Some(page),
            JobFlowDependencyKind::Unprocessed => self.unprocessed = Some(page),
            JobFlowDependencyKind::Ignored => self.ignored = Some(page),
            JobFlowDependencyKind::Failed => self.failed = Some(page),
        }
    }
}

/// Finished status for a retained job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobFinishedResult {
    /// The job exists but has not reached a terminal state yet.
    NotFinished,
    /// The job completed successfully.
    Completed {
        /// Retained completion value, when the job record still stores one.
        return_value: Option<Value>,
    },
    /// The job failed terminally.
    Failed {
        /// Retained failure reason, when the job record still stores one.
        failed_reason: Option<String>,
    },
}

/// Completed child return values keyed by child job id.
pub type JobFlowChildValues = BTreeMap<JobId, Value>;

/// Ignored child failure reasons keyed by child job id.
pub type JobFlowIgnoredFailures = BTreeMap<JobId, String>;

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

/// Current owner snapshot for a repeat series.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobRepeatEntry {
    /// Queue-local repeat series key.
    pub key: String,
    /// Current non-terminal owner job id for the series.
    pub job_id: JobId,
    /// Job name used by the current owner.
    pub name: String,
    /// Current lifecycle state of the owner job.
    pub state: JobState,
    /// Scheduled time of the current owner occurrence.
    pub scheduled_at: DateTime<Utc>,
    /// Completed occurrence count carried by the current owner.
    pub repeat_count: u32,
    /// Repeat schedule and limits for the series.
    pub options: RepeatOptions,
}

/// Options for listing repeat series / job schedulers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobRepeatListOptions {
    /// Number of matching repeat series to skip.
    pub offset: usize,
    /// Maximum number of repeat series to return.
    pub limit: usize,
    /// Return repeat series by next scheduled time ascending when true.
    ///
    /// The default is descending to match BullMQ's `getJobSchedulers()`.
    #[serde(default = "default_repeat_list_ascending")]
    pub ascending: bool,
}

impl Default for JobRepeatListOptions {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 100,
            ascending: false,
        }
    }
}

impl JobRepeatListOptions {
    /// Create default repeat-list options.
    pub fn new() -> Self {
        Self::default()
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

    /// Return repeat series by next scheduled time ascending.
    pub fn ascending(mut self) -> Self {
        self.ascending = true;
        self
    }

    /// Return repeat series by next scheduled time descending.
    pub fn descending(mut self) -> Self {
        self.ascending = false;
        self
    }
}

/// A page of repeat series / job schedulers returned by a backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobRepeatPage {
    pub repeats: Vec<JobRepeatEntry>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

pub(crate) fn page_repeat_entries(
    mut repeats: Vec<JobRepeatEntry>,
    options: JobRepeatListOptions,
) -> JobRepeatPage {
    repeats.sort_by(|a, b| {
        let order = a
            .scheduled_at
            .cmp(&b.scheduled_at)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.job_id.cmp(&b.job_id));
        if options.ascending {
            order
        } else {
            order.reverse()
        }
    });
    let total = repeats.len();
    let page = repeats
        .into_iter()
        .skip(options.offset)
        .take(options.limit)
        .collect();
    JobRepeatPage {
        repeats: page,
        total,
        offset: options.offset,
        limit: options.limit,
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
    /// Refresh the deduplication TTL when a duplicate is added.
    #[serde(default)]
    pub extend: bool,
}

impl DeduplicationOptions {
    /// Create simple deduplication options.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ttl: None,
            replace: false,
            keep_last_if_active: false,
            extend: false,
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

    /// Refresh this deduplication id's TTL when a duplicate is added.
    ///
    /// This mirrors BullMQ's `extend` debounce option. It only has an effect
    /// when a positive TTL is configured and keep-last-if-active is disabled.
    pub fn extend_ttl(mut self, extend: bool) -> Self {
        self.extend = extend;
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

/// Retention policy for finished jobs.
///
/// This mirrors BullMQ's `KeepJobs` shape for `removeOnComplete` and
/// `removeOnFail`: `count` keeps the newest N jobs in a terminal set, `age`
/// keeps jobs younger than the configured duration, and `limit` bounds how many
/// aged jobs are removed per terminal transition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobRetention {
    /// Maximum age of jobs to keep in a terminal set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age: Option<Duration>,
    /// Maximum number of jobs to keep in a terminal set. `0` removes the
    /// currently finished job immediately, matching BullMQ's `{ count: 0 }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    /// Maximum number of aged jobs removed per terminal transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl JobRetention {
    /// Keep at most `count` newest finished jobs.
    pub fn count(count: usize) -> Self {
        Self {
            age: None,
            count: Some(count),
            limit: None,
        }
    }

    /// Keep finished jobs younger than `age`.
    pub fn age(age: Duration) -> Self {
        Self {
            age: Some(age),
            count: None,
            limit: None,
        }
    }

    /// Keep finished jobs that satisfy both `age` and `count`.
    pub fn age_and_count(age: Duration, count: usize) -> Self {
        Self {
            age: Some(age),
            count: Some(count),
            limit: None,
        }
    }

    /// Set or replace the maximum age.
    pub fn with_age(mut self, age: Duration) -> Self {
        self.age = Some(age);
        self
    }

    /// Set or replace the maximum count.
    pub fn with_count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    /// Limit how many aged jobs are removed per terminal transition.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub(crate) fn removes_current(&self) -> bool {
        self.count == Some(0)
    }

    pub(crate) fn validate(&self, field: &str) -> Result<()> {
        if self.age.is_none() && self.count.is_none() {
            return Err(LaneError::ConfigError(format!(
                "{field} must specify an age or count"
            )));
        }
        if matches!(self.age, Some(age) if age.is_zero()) {
            return Err(LaneError::ConfigError(format!(
                "{field} age must be greater than zero"
            )));
        }
        if self.limit == Some(0) {
            return Err(LaneError::ConfigError(format!(
                "{field} limit must be greater than zero"
            )));
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
    /// Insert ready jobs at the front of their same-priority waiting group.
    #[serde(default, skip_serializing_if = "is_false")]
    pub lifo: bool,
    /// Optional delay before the job becomes claimable.
    pub delay: Option<Duration>,
    /// Retry policy used after processing failure.
    pub retry_policy: RetryPolicy,
    /// Optional execution timeout hint for workers.
    pub timeout: Option<Duration>,
    /// Remove the job record after successful completion.
    pub remove_on_complete: bool,
    /// BullMQ-style completed-job retention policy. The legacy
    /// `remove_on_complete` bool takes precedence when set to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_retention: Option<JobRetention>,
    /// Remove the job record after terminal failure.
    pub remove_on_fail: bool,
    /// BullMQ-style failed-job retention policy. The legacy `remove_on_fail`
    /// bool takes precedence when set to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_retention: Option<JobRetention>,
    /// Number of lease expirations tolerated before terminal failure.
    pub max_stalled_count: u32,
    /// Optional repeat schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<RepeatOptions>,
    /// Optional simple deduplication settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deduplication: Option<DeduplicationOptions>,
    /// Do not fail the parent flow when this child reaches terminal failure.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ignore_dependency_on_failure: bool,
    /// Remove this child from its parent dependencies when it reaches terminal failure.
    #[serde(default, skip_serializing_if = "is_false")]
    pub remove_dependency_on_failure: bool,
    /// Continue the parent flow when this child reaches terminal failure.
    #[serde(default, skip_serializing_if = "is_false")]
    pub continue_parent_on_failure: bool,
    /// Defer parent failure when this child reaches terminal failure.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fail_parent_on_failure: bool,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            job_id: None,
            priority: DEFAULT_JOB_PRIORITY,
            lifo: false,
            delay: None,
            retry_policy: RetryPolicy::none(),
            timeout: None,
            remove_on_complete: false,
            completion_retention: None,
            remove_on_fail: false,
            failure_retention: None,
            max_stalled_count: 1,
            repeat: None,
            deduplication: None,
            ignore_dependency_on_failure: false,
            remove_dependency_on_failure: false,
            continue_parent_on_failure: false,
            fail_parent_on_failure: false,
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

    /// Configure same-priority waiting order. `true` claims newest ready jobs first.
    pub fn with_lifo(mut self, lifo: bool) -> Self {
        self.lifo = lifo;
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
        self.completion_retention = None;
        self
    }

    /// Configure completed-job retention by age and/or count.
    pub fn with_completion_retention(mut self, retention: JobRetention) -> Self {
        self.remove_on_complete = false;
        self.completion_retention = Some(retention);
        self
    }

    /// Configure whether failed jobs are retained.
    pub fn remove_on_fail(mut self, remove: bool) -> Self {
        self.remove_on_fail = remove;
        self.failure_retention = None;
        self
    }

    /// Configure failed-job retention by age and/or count.
    pub fn with_failure_retention(mut self, retention: JobRetention) -> Self {
        self.remove_on_fail = false;
        self.failure_retention = Some(retention);
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

    /// Configure BullMQ-style `ignoreDependencyOnFailure` for flow children.
    pub fn with_ignore_dependency_on_failure(mut self, ignore: bool) -> Self {
        self.ignore_dependency_on_failure = ignore;
        self
    }

    /// Configure BullMQ-style `removeDependencyOnFailure` for flow children.
    pub fn with_remove_dependency_on_failure(mut self, remove: bool) -> Self {
        self.remove_dependency_on_failure = remove;
        self
    }

    /// Configure BullMQ-style `continueParentOnFailure` for flow children.
    pub fn with_continue_parent_on_failure(mut self, continue_parent: bool) -> Self {
        self.continue_parent_on_failure = continue_parent;
        self
    }

    /// Configure BullMQ-style `failParentOnFailure` for flow children.
    pub fn with_fail_parent_on_failure(mut self, fail_parent: bool) -> Self {
        self.fail_parent_on_failure = fail_parent;
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(job_id) = self.job_id.as_deref() {
            validate_job_id(job_id)?;
        }

        validate_job_priority(self.priority)?;

        if let Some(repeat) = &self.repeat {
            repeat.validate()?;
        }

        if let Some(deduplication) = &self.deduplication {
            deduplication.validate()?;
        }

        if let Some(retention) = &self.completion_retention {
            retention.validate("completion retention")?;
        }

        if let Some(retention) = &self.failure_retention {
            retention.validate("failure retention")?;
        }

        let flow_failure_policies = [
            self.ignore_dependency_on_failure,
            self.remove_dependency_on_failure,
            self.continue_parent_on_failure,
            self.fail_parent_on_failure,
        ]
        .into_iter()
        .filter(|enabled| *enabled)
        .count();
        if flow_failure_policies > 1 {
            return Err(LaneError::ConfigError(
                "flow child failure policies are mutually exclusive".to_string(),
            ));
        }

        Ok(())
    }

    pub(crate) fn removes_completed_immediately(&self) -> bool {
        self.remove_on_complete
            || self
                .completion_retention
                .as_ref()
                .is_some_and(JobRetention::removes_current)
    }

    pub(crate) fn completed_retention(&self) -> Option<&JobRetention> {
        if self.remove_on_complete {
            return None;
        }
        self.completion_retention
            .as_ref()
            .filter(|retention| !retention.removes_current())
    }

    pub(crate) fn removes_failed_immediately(&self) -> bool {
        self.remove_on_fail
            || self
                .failure_retention
                .as_ref()
                .is_some_and(JobRetention::removes_current)
    }

    pub(crate) fn failed_retention(&self) -> Option<&JobRetention> {
        if self.remove_on_fail {
            return None;
        }
        self.failure_retention
            .as_ref()
            .filter(|retention| !retention.removes_current())
    }
}

fn validate_job_id(job_id: &str) -> Result<()> {
    if job_id.trim().is_empty() {
        return Err(LaneError::ConfigError(
            "job id must not be empty".to_string(),
        ));
    }
    if job_id == "0" || job_id.starts_with("0:") {
        return Err(LaneError::ConfigError(
            "job id cannot be `0` or start with `0:`".to_string(),
        ));
    }
    if is_bullmq_integer_job_id(job_id) {
        return Err(LaneError::ConfigError(
            "custom job id cannot be an integer".to_string(),
        ));
    }
    Ok(())
}

fn is_bullmq_integer_job_id(job_id: &str) -> bool {
    let mut chars = job_id.chars();
    match chars.next() {
        Some('-') => matches!(chars.next(), Some('1'..='9')) && chars.all(|ch| ch.is_ascii_digit()),
        Some('1'..='9') => chars.all(|ch| ch.is_ascii_digit()),
        Some('0') => false,
        _ => false,
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
    /// Monotonic sequence assigned each time the job enters the waiting set.
    #[serde(default)]
    pub enqueued_seq: u64,
    pub processed_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub worker_id: Option<JobWorkerId>,
    #[serde(default, skip)]
    pub lock_token: Option<JobLockToken>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_failure: Option<String>,
    pub failed_reason: Option<String>,
    /// Retained failure stack traces, matching BullMQ's JSON stacktrace array shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stacktrace: Vec<String>,
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
            enqueued_seq: 0,
            processed_at: None,
            finished_at: None,
            worker_id: None,
            lock_token: None,
            lease_expires_at: None,
            deferred_failure: None,
            failed_reason: None,
            stacktrace: Vec::new(),
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

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &u64) -> bool {
    *value == 0
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<JobEvent>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub event_sequence: u64,
    pub jobs: Vec<Job>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deduplication_next_jobs: Vec<Job>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deduplication_next_flows: Vec<JobFlow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub released_deduplication_owners: Vec<(String, JobId)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flow_dependency_indexes: Vec<JobFlowDependencyIndex>,
}

/// Serializable parent-scoped flow dependency side index.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobFlowDependencyIndex {
    pub parent_id: JobId,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub processed: BTreeMap<JobId, Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ignored: BTreeMap<JobId, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<JobId>,
}
