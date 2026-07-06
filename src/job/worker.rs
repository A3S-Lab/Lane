use super::backend::JobQueueBackend;
use super::types::{Job, JobId, JobWorkerId};
use crate::error::{LaneError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// User-provided processor for generic queue jobs.
#[async_trait]
pub trait JobProcessor: Send + Sync {
    /// Process a claimed job and return the value stored on completion.
    async fn process(&self, job: Job, context: JobContext) -> Result<Value>;
}

/// Adapter for async closures used as job processors.
pub struct JobProcessorFn<F> {
    f: F,
}

impl<F> JobProcessorFn<F> {
    /// Create a processor from an async closure.
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

#[async_trait]
impl<F, Fut> JobProcessor for JobProcessorFn<F>
where
    F: Fn(Job, JobContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value>> + Send + 'static,
{
    async fn process(&self, job: Job, context: JobContext) -> Result<Value> {
        (self.f)(job, context).await
    }
}

/// Routes jobs to processors by job name.
#[derive(Clone, Default)]
pub struct JobProcessorRouter {
    processors: HashMap<String, Arc<dyn JobProcessor>>,
    default_processor: Option<Arc<dyn JobProcessor>>,
}

impl JobProcessorRouter {
    /// Create an empty processor router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a processor for a job name.
    pub fn register_processor(
        &mut self,
        name: impl Into<String>,
        processor: Arc<dyn JobProcessor>,
    ) {
        self.processors.insert(name.into(), processor);
    }

    /// Add a named processor with builder-style chaining.
    pub fn with_processor(
        mut self,
        name: impl Into<String>,
        processor: Arc<dyn JobProcessor>,
    ) -> Self {
        self.register_processor(name, processor);
        self
    }

    /// Register a fallback processor used when a job name is not registered.
    pub fn register_default_processor(&mut self, processor: Arc<dyn JobProcessor>) {
        self.default_processor = Some(processor);
    }

    /// Add a fallback processor with builder-style chaining.
    pub fn with_default_processor(mut self, processor: Arc<dyn JobProcessor>) -> Self {
        self.register_default_processor(processor);
        self
    }

    /// Whether a processor is registered for the job name.
    pub fn contains_processor(&self, name: &str) -> bool {
        self.processors.contains_key(name)
    }

    /// Number of named processors.
    pub fn len(&self) -> usize {
        self.processors.len()
    }

    /// Whether no named processors are registered.
    pub fn is_empty(&self) -> bool {
        self.processors.is_empty()
    }
}

#[async_trait]
impl JobProcessor for JobProcessorRouter {
    async fn process(&self, job: Job, context: JobContext) -> Result<Value> {
        let processor = self
            .processors
            .get(&job.name)
            .or(self.default_processor.as_ref())
            .cloned()
            .ok_or_else(|| {
                LaneError::ConfigError(format!("no processor registered for job `{}`", job.name))
            })?;

        processor.process(job, context).await
    }
}

/// Context passed to a [`JobProcessor`] for progress, logs, and lease renewal.
#[derive(Clone)]
pub struct JobContext {
    backend: Arc<dyn JobQueueBackend>,
    job_id: JobId,
    worker_id: JobWorkerId,
    lease_duration: Duration,
    log_retention: usize,
}

impl JobContext {
    fn new(
        backend: Arc<dyn JobQueueBackend>,
        job_id: JobId,
        worker_id: JobWorkerId,
        lease_duration: Duration,
        log_retention: usize,
    ) -> Self {
        Self {
            backend,
            job_id,
            worker_id,
            lease_duration,
            log_retention,
        }
    }

    /// Current job ID.
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    /// Current worker ID.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Store a progress value for the current job.
    pub async fn update_progress(&self, progress: Value) -> Result<Job> {
        self.backend.update_progress(&self.job_id, progress).await
    }

    /// Append a retained log line for the current job.
    pub async fn add_log(&self, line: impl Into<String>) -> Result<Job> {
        self.backend
            .add_log(&self.job_id, line.into(), self.log_retention, Utc::now())
            .await
    }

    /// Renew the current job lease using the worker's configured lease duration.
    pub async fn renew_lease(&self) -> Result<Job> {
        self.backend
            .renew_lease(
                &self.job_id,
                &self.worker_id,
                self.lease_duration,
                Utc::now(),
            )
            .await
    }
}

/// Configuration for a generic job worker.
#[derive(Debug, Clone)]
pub struct JobWorkerConfig {
    pub worker_id: JobWorkerId,
    pub concurrency: usize,
    pub lease_duration: Duration,
    pub lease_renew_interval: Duration,
    pub poll_interval: Duration,
    pub stalled_check_interval: Duration,
    pub recover_stalled: bool,
    pub log_retention: usize,
}

impl JobWorkerConfig {
    /// Create a worker configuration with conservative defaults.
    pub fn new(worker_id: impl Into<String>) -> Self {
        Self {
            worker_id: worker_id.into(),
            concurrency: 1,
            lease_duration: Duration::from_secs(30),
            lease_renew_interval: Duration::from_secs(10),
            poll_interval: Duration::from_millis(250),
            stalled_check_interval: Duration::from_secs(30),
            recover_stalled: true,
            log_retention: 1_000,
        }
    }

    /// Configure worker concurrency. Values below 1 are clamped to 1.
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    /// Configure worker lease duration.
    pub fn with_lease_duration(mut self, lease_duration: Duration) -> Self {
        self.lease_duration = lease_duration;
        self
    }

    /// Configure periodic lease renewal interval.
    pub fn with_lease_renew_interval(mut self, interval: Duration) -> Self {
        self.lease_renew_interval = interval;
        self
    }

    /// Configure polling interval when no job is ready.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Configure stalled-job recovery interval.
    pub fn with_stalled_check_interval(mut self, interval: Duration) -> Self {
        self.stalled_check_interval = interval;
        self
    }

    /// Enable or disable stalled-job recovery from the worker loop.
    pub fn with_recover_stalled(mut self, recover: bool) -> Self {
        self.recover_stalled = recover;
        self
    }

    /// Configure retained job log lines. `0` keeps all lines.
    pub fn with_log_retention(mut self, keep: usize) -> Self {
        self.log_retention = keep;
        self
    }
}

impl Default for JobWorkerConfig {
    fn default() -> Self {
        Self::new(format!("worker-{}", uuid::Uuid::new_v4()))
    }
}

/// Outcome from processing at most one job.
#[derive(Debug, Clone, PartialEq)]
pub enum JobRunOutcome {
    /// A job was completed successfully.
    Completed(Job),
    /// Processing failed. The returned job may be terminal failed or delayed for retry.
    Failed(Job),
    /// No job was claimable.
    NoJob,
}

/// Backend-agnostic worker runtime for generic queue jobs.
#[derive(Clone)]
pub struct JobWorker {
    backend: Arc<dyn JobQueueBackend>,
    processor: Arc<dyn JobProcessor>,
    config: JobWorkerConfig,
    shutdown: Arc<AtomicBool>,
}

impl JobWorker {
    /// Create a worker from a backend, processor, and config.
    pub fn new(
        backend: Arc<dyn JobQueueBackend>,
        processor: Arc<dyn JobProcessor>,
        config: JobWorkerConfig,
    ) -> Self {
        Self {
            backend,
            processor,
            config,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Worker ID.
    pub fn worker_id(&self) -> &str {
        &self.config.worker_id
    }

    /// Process at most one job immediately.
    pub async fn run_once(&self, now: DateTime<Utc>) -> Result<JobRunOutcome> {
        self.backend.promote_due_jobs(now).await?;
        let Some(job) = self
            .backend
            .claim_next(
                self.config.worker_id.clone(),
                self.config.lease_duration,
                now,
            )
            .await?
        else {
            return Ok(JobRunOutcome::NoJob);
        };

        self.process_claimed(job).await
    }

    /// Recover stalled jobs immediately.
    pub async fn recover_stalled(&self, now: DateTime<Utc>) -> Result<usize> {
        self.backend.recover_stalled_jobs(now).await
    }

    /// Run jobs until the queue is idle or `max_jobs` have been processed.
    pub async fn run_until_idle(&self, max_jobs: usize) -> Result<usize> {
        let mut processed = 0;
        while processed < max_jobs {
            match self.run_once(Utc::now()).await? {
                JobRunOutcome::NoJob => break,
                JobRunOutcome::Completed(_) | JobRunOutcome::Failed(_) => processed += 1,
            }
        }
        Ok(processed)
    }

    /// Start background worker loops.
    pub fn start(&self) -> JobWorkerHandle {
        self.shutdown.store(false, Ordering::Relaxed);
        let mut handles = Vec::with_capacity(self.config.concurrency + 1);
        for _ in 0..self.config.concurrency {
            let worker = self.clone();
            handles.push(tokio::spawn(async move {
                worker.run_loop().await;
            }));
        }

        if self.config.recover_stalled {
            let worker = self.clone();
            handles.push(tokio::spawn(async move {
                worker.recovery_loop().await;
            }));
        }

        JobWorkerHandle {
            shutdown: Arc::clone(&self.shutdown),
            handles,
        }
    }

    async fn run_loop(self) {
        while !self.shutdown.load(Ordering::Relaxed) {
            match self.run_once(Utc::now()).await {
                Ok(JobRunOutcome::NoJob) => tokio::time::sleep(self.config.poll_interval).await,
                Ok(JobRunOutcome::Completed(_)) | Ok(JobRunOutcome::Failed(_)) => {}
                Err(error) => {
                    tracing::warn!(error = %error, worker_id = %self.config.worker_id, "job worker iteration failed");
                    tokio::time::sleep(self.config.poll_interval).await;
                }
            }
        }
    }

    async fn recovery_loop(self) {
        while !self.shutdown.load(Ordering::Relaxed) {
            if let Err(error) = self.recover_stalled(Utc::now()).await {
                tracing::warn!(error = %error, worker_id = %self.config.worker_id, "job stalled recovery failed");
            }
            tokio::time::sleep(self.config.stalled_check_interval).await;
        }
    }

    async fn process_claimed(&self, job: Job) -> Result<JobRunOutcome> {
        let context = JobContext::new(
            Arc::clone(&self.backend),
            job.id.clone(),
            self.config.worker_id.clone(),
            self.config.lease_duration,
            self.config.log_retention,
        );

        let lease_shutdown = Arc::new(AtomicBool::new(false));
        let renew_handle = self.spawn_lease_renewer(context.clone(), Arc::clone(&lease_shutdown));
        let job_id = job.id.clone();
        let timeout = job.options.timeout;
        let result = self.process_with_timeout(job, context, timeout).await;
        lease_shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = renew_handle {
            handle.abort();
            let _ = handle.await;
        }

        match result {
            Ok(value) => {
                let completed = self
                    .backend
                    .complete_job(&job_id, value, Utc::now())
                    .await?;
                Ok(JobRunOutcome::Completed(completed))
            }
            Err(error) => {
                let failed = self
                    .backend
                    .fail_job(&job_id, error.to_string(), Utc::now())
                    .await?;
                Ok(JobRunOutcome::Failed(failed))
            }
        }
    }

    async fn process_with_timeout(
        &self,
        job: Job,
        context: JobContext,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let processor = Arc::clone(&self.processor);
        match timeout {
            Some(timeout) => {
                match tokio::time::timeout(timeout, processor.process(job, context)).await {
                    Ok(result) => result,
                    Err(_) => Err(LaneError::Timeout(timeout)),
                }
            }
            None => processor.process(job, context).await,
        }
    }

    fn spawn_lease_renewer(
        &self,
        context: JobContext,
        shutdown: Arc<AtomicBool>,
    ) -> Option<JoinHandle<()>> {
        if self.config.lease_renew_interval.is_zero()
            || self.config.lease_renew_interval >= self.config.lease_duration
        {
            return None;
        }

        let interval = self.config.lease_renew_interval;
        let worker_id = self.config.worker_id.clone();
        Some(tokio::spawn(async move {
            while !shutdown.load(Ordering::Relaxed) {
                tokio::time::sleep(interval).await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(error) = context.renew_lease().await {
                    tracing::warn!(error = %error, worker_id = %worker_id, job_id = %context.job_id(), "job lease renewal failed");
                    break;
                }
            }
        }))
    }
}

/// Handle for shutting down background worker loops.
pub struct JobWorkerHandle {
    shutdown: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl JobWorkerHandle {
    /// Request shutdown without waiting for loops to exit.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Request shutdown and wait for worker tasks to finish.
    pub async fn shutdown(self) {
        self.shutdown.store(true, Ordering::Relaxed);
        for handle in self.handles {
            let _ = handle.await;
        }
    }
}

/// Helper to create an async-closure job processor.
pub fn job_processor_fn<F>(f: F) -> JobProcessorFn<F> {
    JobProcessorFn::new(f)
}
