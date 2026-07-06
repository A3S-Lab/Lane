//! Generic job runtime primitives for distributed priority queues.
//!
//! The existing lane scheduler executes in-process [`Command`](crate::Command)
//! values. This module is the durable job-queue foundation: jobs are plain JSON
//! payloads with bulk submission, explicit lifecycle state, priority plus FIFO/LIFO
//! waiting ordering, delayed scheduling, token-owned worker leases, retries,
//! stalled-job recovery, management APIs, and local durable snapshot persistence.

mod backend;
mod local;
mod memory;
#[cfg(feature = "redis-backend")]
mod redis;
mod types;
mod worker;

pub use backend::JobQueueBackend;
pub use local::LocalJobQueue;
pub use memory::InMemoryJobQueue;
#[cfg(feature = "redis-backend")]
pub use redis::RedisJobQueue;
pub use types::{
    DeduplicationOptions, Job, JobFlow, JobFlowDependencies, JobFlowDependencyCounts, JobId,
    JobListOptions, JobListPage, JobLockToken, JobLogEntry, JobLogPage, JobOptions, JobPriority,
    JobPriorityCount, JobQueueSnapshot, JobQueueStats, JobRateLimit, JobRepeatEntry, JobSpec,
    JobState, JobStateCount, JobWorkerId, QueueName, RepeatOptions, RepeatSchedule,
    DEFAULT_JOB_PRIORITY,
};
pub use worker::{
    job_processor_fn, JobContext, JobProcessor, JobProcessorFn, JobProcessorRouter, JobRunOutcome,
    JobWorker, JobWorkerConfig, JobWorkerHandle,
};

#[cfg(test)]
mod tests;
