//! Generic job runtime primitives for distributed priority queues.
//!
//! The existing lane scheduler executes in-process [`Command`](crate::Command)
//! values. This module is the durable job-queue foundation: jobs are plain JSON
//! payloads with explicit lifecycle state, priority ordering, delayed
//! scheduling, worker leases, retries, stalled-job recovery, management APIs,
//! and local durable snapshot persistence.

mod backend;
mod local;
mod memory;
mod types;
mod worker;

pub use backend::JobQueueBackend;
pub use local::LocalJobQueue;
pub use memory::InMemoryJobQueue;
pub use types::{
    Job, JobId, JobListOptions, JobListPage, JobLogEntry, JobOptions, JobPriority,
    JobQueueSnapshot, JobQueueStats, JobState, JobWorkerId, QueueName, DEFAULT_JOB_PRIORITY,
};
pub use worker::{
    job_processor_fn, JobContext, JobProcessor, JobProcessorFn, JobRunOutcome, JobWorker,
    JobWorkerConfig, JobWorkerHandle,
};

#[cfg(test)]
mod tests;
