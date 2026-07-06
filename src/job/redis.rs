use super::backend::JobQueueBackend;
use super::types::{
    add_duration, Job, JobListOptions, JobListPage, JobLogEntry, JobOptions, JobPriority,
    JobQueueStats, JobState, JobWorkerId, QueueName,
};
use crate::error::{LaneError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::Value;
use std::time::Duration;

const WAITING_SCORE_BUCKET: f64 = 1_000_000_000_000.0;

const CLAIM_SCRIPT: &str = r#"
local ids = redis.call('ZRANGE', KEYS[1], 0, 0)
if #ids == 0 then
  return nil
end

local id = ids[1]
local raw = redis.call('HGET', KEYS[3], id)
if not raw then
  redis.call('ZREM', KEYS[1], id)
  return nil
end

local job = cjson.decode(raw)
job["state"] = "active"
job["attempts_made"] = (job["attempts_made"] or 0) + 1
job["processed_at"] = ARGV[2]
job["worker_id"] = ARGV[3]
job["lease_expires_at"] = ARGV[4]
job["failed_reason"] = cjson.null

local updated = cjson.encode(job)
redis.call('ZREM', KEYS[1], id)
redis.call('ZADD', KEYS[2], ARGV[1], id)
redis.call('HSET', KEYS[3], id, updated)
return updated
"#;

/// Redis-backed generic job queue.
///
/// Redis stores each job as JSON in a hash and indexes lifecycle states with
/// sorted sets. Claiming a job is atomic: a Lua script moves the next waiting
/// job to the active set and records the worker lease in the same Redis turn.
#[derive(Clone)]
pub struct RedisJobQueue {
    client: redis::Client,
    namespace: String,
    queue: QueueName,
}

impl RedisJobQueue {
    /// Create a Redis queue using the default `a3s:lane` namespace.
    pub fn new(redis_url: &str, queue: impl Into<String>) -> Result<Self> {
        Self::with_namespace(redis_url, "a3s:lane", queue)
    }

    /// Create a Redis queue with a custom key namespace.
    pub fn with_namespace(
        redis_url: &str,
        namespace: impl Into<String>,
        queue: impl Into<String>,
    ) -> Result<Self> {
        let client = redis::Client::open(redis_url)
            .map_err(|error| LaneError::Other(format!("failed to open Redis client: {error}")))?;
        Ok(Self {
            client,
            namespace: namespace.into(),
            queue: queue.into(),
        })
    }

    /// Queue name.
    pub fn queue_name(&self) -> &str {
        &self.queue
    }

    /// Redis key namespace.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    async fn connection(&self) -> Result<ConnectionManager> {
        self.client
            .get_connection_manager()
            .await
            .map_err(redis_error)
    }

    fn key(&self, suffix: &str) -> String {
        format!("{}:{}:{}", self.namespace, self.queue, suffix)
    }

    fn jobs_key(&self) -> String {
        self.key("jobs")
    }

    fn meta_key(&self) -> String {
        self.key("meta")
    }

    fn sequence_key(&self) -> String {
        self.key("sequence")
    }

    fn state_key(&self, state: JobState) -> String {
        self.key(match state {
            JobState::Waiting => "waiting",
            JobState::Delayed => "delayed",
            JobState::Active => "active",
            JobState::WaitingChildren => "waiting_children",
            JobState::Completed => "completed",
            JobState::Failed => "failed",
        })
    }

    fn state_keys(&self) -> [String; 6] {
        [
            self.state_key(JobState::Waiting),
            self.state_key(JobState::Delayed),
            self.state_key(JobState::Active),
            self.state_key(JobState::WaitingChildren),
            self.state_key(JobState::Completed),
            self.state_key(JobState::Failed),
        ]
    }

    async fn next_sequence(&self, conn: &mut ConnectionManager) -> Result<u64> {
        conn.incr(self.sequence_key(), 1_u8)
            .await
            .map_err(redis_error)
    }

    async fn store_job(&self, conn: &mut ConnectionManager, job: &Job) -> Result<()> {
        let encoded = encode_job(job)?;
        conn.hset(self.jobs_key(), &job.id, encoded)
            .await
            .map_err(redis_error)
    }

    async fn load_job(&self, conn: &mut ConnectionManager, job_id: &str) -> Result<Option<Job>> {
        let raw: Option<String> = conn
            .hget(self.jobs_key(), job_id)
            .await
            .map_err(redis_error)?;
        raw.map(|raw| decode_job(&raw)).transpose()
    }

    async fn remove_from_state_sets(
        &self,
        conn: &mut ConnectionManager,
        job_id: &str,
    ) -> Result<()> {
        for key in self.state_keys() {
            let _: usize = conn.zrem(key, job_id).await.map_err(redis_error)?;
        }
        Ok(())
    }

    async fn move_to_state(
        &self,
        conn: &mut ConnectionManager,
        job: &Job,
        state: JobState,
        score: f64,
    ) -> Result<()> {
        self.remove_from_state_sets(conn, &job.id).await?;
        let _: usize = conn
            .zadd(self.state_key(state), &job.id, score)
            .await
            .map_err(redis_error)?;
        self.store_job(conn, job).await
    }

    async fn remove_job_record(
        &self,
        conn: &mut ConnectionManager,
        job_id: &str,
    ) -> Result<Option<Job>> {
        let job = self.load_job(conn, job_id).await?;
        self.remove_from_state_sets(conn, job_id).await?;
        let _: usize = conn
            .hdel(self.jobs_key(), job_id)
            .await
            .map_err(redis_error)?;
        Ok(job)
    }

    async fn is_paused(&self, conn: &mut ConnectionManager) -> Result<bool> {
        let paused: Option<u8> = conn
            .hget(self.meta_key(), "paused")
            .await
            .map_err(redis_error)?;
        Ok(paused.unwrap_or(0) != 0)
    }
}

#[async_trait]
impl JobQueueBackend for RedisJobQueue {
    async fn add_job(&self, name: String, payload: Value, options: JobOptions) -> Result<Job> {
        let now = Utc::now();
        let job = Job::new(self.queue.clone(), name, payload, options, now);
        let mut conn = self.connection().await?;
        self.store_job(&mut conn, &job).await?;

        match job.state {
            JobState::Delayed => {
                let _: usize = conn
                    .zadd(
                        self.state_key(JobState::Delayed),
                        &job.id,
                        millis(job.scheduled_at),
                    )
                    .await
                    .map_err(redis_error)?;
            }
            JobState::Waiting => {
                let sequence = self.next_sequence(&mut conn).await?;
                let _: usize = conn
                    .zadd(
                        self.state_key(JobState::Waiting),
                        &job.id,
                        waiting_score(job.priority, sequence),
                    )
                    .await
                    .map_err(redis_error)?;
            }
            _ => {}
        }

        Ok(job)
    }

    async fn claim_next(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>> {
        self.promote_due_jobs(now).await?;
        let mut conn = self.connection().await?;
        if self.is_paused(&mut conn).await? {
            return Ok(None);
        }

        let lease_expires_at = add_duration(now, lease_for);
        let raw: Option<String> = redis::cmd("EVAL")
            .arg(CLAIM_SCRIPT)
            .arg(3)
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Active))
            .arg(self.jobs_key())
            .arg(millis(lease_expires_at))
            .arg(now.to_rfc3339())
            .arg(worker_id)
            .arg(lease_expires_at.to_rfc3339())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;

        raw.map(|raw| decode_job(&raw)).transpose()
    }

    async fn complete_job(&self, job_id: &str, value: Value, now: DateTime<Utc>) -> Result<Job> {
        let mut conn = self.connection().await?;
        let mut job = self.require_job(&mut conn, job_id).await?;
        require_active(&job, "complete")?;
        job.state = JobState::Completed;
        job.finished_at = Some(now);
        job.worker_id = None;
        job.lease_expires_at = None;
        job.return_value = Some(value);
        let completed = job.clone();
        if job.options.remove_on_complete {
            self.remove_job_record(&mut conn, job_id).await?;
        } else {
            self.move_to_state(&mut conn, &job, JobState::Completed, millis(now))
                .await?;
        }
        Ok(completed)
    }

    async fn fail_job(&self, job_id: &str, error: String, now: DateTime<Utc>) -> Result<Job> {
        let mut conn = self.connection().await?;
        let mut job = self.require_job(&mut conn, job_id).await?;
        require_active(&job, "fail")?;
        job.worker_id = None;
        job.lease_expires_at = None;
        job.failed_reason = Some(error);

        if should_retry(&job) {
            let delay = job
                .options
                .retry_policy
                .delay_for_attempt(job.attempts_made);
            job.state = JobState::Delayed;
            job.scheduled_at = add_duration(now, delay);
            job.finished_at = None;
            self.move_to_state(&mut conn, &job, JobState::Delayed, millis(job.scheduled_at))
                .await?;
        } else {
            job.state = JobState::Failed;
            job.finished_at = Some(now);
            if job.options.remove_on_fail {
                let failed = job.clone();
                self.remove_job_record(&mut conn, job_id).await?;
                return Ok(failed);
            }
            self.move_to_state(&mut conn, &job, JobState::Failed, millis(now))
                .await?;
        }

        Ok(job)
    }

    async fn renew_lease(
        &self,
        job_id: &str,
        worker_id: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut conn = self.connection().await?;
        let mut job = self.require_job(&mut conn, job_id).await?;
        require_active(&job, "renew lease")?;
        require_worker(&job, worker_id)?;
        job.lease_expires_at = Some(add_duration(now, lease_for));
        self.move_to_state(
            &mut conn,
            &job,
            JobState::Active,
            millis(job.lease_expires_at.unwrap_or(now)),
        )
        .await?;
        Ok(job)
    }

    async fn promote_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let mut conn = self.connection().await?;
        let mut job = self.require_job(&mut conn, job_id).await?;
        if job.state == JobState::Delayed {
            job.state = JobState::Waiting;
            job.scheduled_at = now;
            let sequence = self.next_sequence(&mut conn).await?;
            self.move_to_state(
                &mut conn,
                &job,
                JobState::Waiting,
                waiting_score(job.priority, sequence),
            )
            .await?;
        }
        Ok(job)
    }

    async fn retry_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let mut conn = self.connection().await?;
        let mut job = self.require_job(&mut conn, job_id).await?;
        if job.state != JobState::Failed {
            return Err(LaneError::JobStateConflict(format!(
                "cannot retry job {} from state {:?}",
                job.id, job.state
            )));
        }
        job.state = JobState::Waiting;
        job.scheduled_at = now;
        job.processed_at = None;
        job.finished_at = None;
        job.worker_id = None;
        job.lease_expires_at = None;
        job.failed_reason = None;
        let sequence = self.next_sequence(&mut conn).await?;
        self.move_to_state(
            &mut conn,
            &job,
            JobState::Waiting,
            waiting_score(job.priority, sequence),
        )
        .await?;
        Ok(job)
    }

    async fn remove_job(&self, job_id: &str) -> Result<Option<Job>> {
        let mut conn = self.connection().await?;
        self.remove_job_record(&mut conn, job_id).await
    }

    async fn clean_jobs(
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
        let mut conn = self.connection().await?;
        let ids = self.ids_for_state(&mut conn, state).await?;
        let mut jobs = Vec::new();
        for id in ids {
            if let Some(job) = self.load_job(&mut conn, &id).await? {
                if job_reference_time(&job) <= cutoff {
                    jobs.push(job);
                }
            }
        }
        jobs.sort_by(|a, b| {
            job_reference_time(a)
                .cmp(&job_reference_time(b))
                .then_with(|| a.id.cmp(&b.id))
        });
        jobs.truncate(limit);

        for job in &jobs {
            self.remove_job_record(&mut conn, &job.id).await?;
        }
        Ok(jobs)
    }

    async fn list_jobs(&self, options: JobListOptions) -> Result<JobListPage> {
        let mut conn = self.connection().await?;
        if let Some(state) = options.state {
            let total: usize = conn
                .zcard(self.state_key(state))
                .await
                .map_err(redis_error)?;
            let end = if options.limit == 0 {
                options.offset
            } else {
                options
                    .offset
                    .saturating_add(options.limit)
                    .saturating_sub(1)
            };
            let ids: Vec<String> = if options.limit == 0 {
                Vec::new()
            } else {
                redis::cmd("ZRANGE")
                    .arg(self.state_key(state))
                    .arg(options.offset)
                    .arg(end)
                    .query_async(&mut conn)
                    .await
                    .map_err(redis_error)?
            };
            let jobs = self.load_jobs(&mut conn, ids).await?;
            return Ok(JobListPage {
                jobs,
                total,
                offset: options.offset,
                limit: options.limit,
            });
        }

        let raw_jobs: Vec<String> = redis::cmd("HVALS")
            .arg(self.jobs_key())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        let mut jobs = raw_jobs
            .into_iter()
            .map(|raw| decode_job(&raw))
            .collect::<Result<Vec<_>>>()?;
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

    async fn update_progress(&self, job_id: &str, progress: Value) -> Result<Job> {
        let mut conn = self.connection().await?;
        let mut job = self.require_job(&mut conn, job_id).await?;
        if job.state.is_terminal() {
            return Err(LaneError::JobStateConflict(format!(
                "cannot update progress for terminal job {}",
                job.id
            )));
        }
        job.progress = Some(progress);
        self.store_job(&mut conn, &job).await?;
        Ok(job)
    }

    async fn add_log(
        &self,
        job_id: &str,
        line: String,
        keep: usize,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut conn = self.connection().await?;
        let mut job = self.require_job(&mut conn, job_id).await?;
        job.logs.push(JobLogEntry {
            timestamp: now,
            line,
        });
        if keep > 0 && job.logs.len() > keep {
            let remove_count = job.logs.len() - keep;
            job.logs.drain(0..remove_count);
        }
        self.store_job(&mut conn, &job).await?;
        Ok(job)
    }

    async fn promote_due_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut conn = self.connection().await?;
        let ids: Vec<String> = redis::cmd("ZRANGEBYSCORE")
            .arg(self.state_key(JobState::Delayed))
            .arg("-inf")
            .arg(millis(now))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;

        let mut promoted = 0;
        for id in ids {
            if let Some(mut job) = self.load_job(&mut conn, &id).await? {
                if job.state == JobState::Delayed && job.scheduled_at <= now {
                    job.state = JobState::Waiting;
                    let sequence = self.next_sequence(&mut conn).await?;
                    self.move_to_state(
                        &mut conn,
                        &job,
                        JobState::Waiting,
                        waiting_score(job.priority, sequence),
                    )
                    .await?;
                    promoted += 1;
                }
            }
        }
        Ok(promoted)
    }

    async fn recover_stalled_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut conn = self.connection().await?;
        let ids: Vec<String> = redis::cmd("ZRANGEBYSCORE")
            .arg(self.state_key(JobState::Active))
            .arg("-inf")
            .arg(millis(now))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;

        let mut recovered = 0;
        for id in ids {
            if let Some(mut job) = self.load_job(&mut conn, &id).await? {
                if job.state != JobState::Active {
                    continue;
                }
                if matches!(job.lease_expires_at, Some(expires_at) if expires_at > now) {
                    continue;
                }
                job.stalled_count = job.stalled_count.saturating_add(1);
                job.worker_id = None;
                job.lease_expires_at = None;
                job.failed_reason = Some("job stalled after worker lease expired".to_string());
                if job.stalled_count > job.options.max_stalled_count {
                    job.state = JobState::Failed;
                    job.finished_at = Some(now);
                    if job.options.remove_on_fail {
                        self.remove_job_record(&mut conn, &job.id).await?;
                    } else {
                        self.move_to_state(&mut conn, &job, JobState::Failed, millis(now))
                            .await?;
                    }
                } else {
                    job.state = JobState::Waiting;
                    job.processed_at = None;
                    let sequence = self.next_sequence(&mut conn).await?;
                    self.move_to_state(
                        &mut conn,
                        &job,
                        JobState::Waiting,
                        waiting_score(job.priority, sequence),
                    )
                    .await?;
                }
                recovered += 1;
            }
        }
        Ok(recovered)
    }

    async fn pause(&self) -> Result<()> {
        let mut conn = self.connection().await?;
        conn.hset(self.meta_key(), "paused", 1_u8)
            .await
            .map_err(redis_error)
    }

    async fn resume(&self) -> Result<()> {
        let mut conn = self.connection().await?;
        conn.hset(self.meta_key(), "paused", 0_u8)
            .await
            .map_err(redis_error)
    }

    async fn get_job(&self, job_id: &str) -> Result<Option<Job>> {
        let mut conn = self.connection().await?;
        self.load_job(&mut conn, job_id).await
    }

    async fn stats(&self) -> Result<JobQueueStats> {
        let mut conn = self.connection().await?;
        let waiting: usize = conn
            .zcard(self.state_key(JobState::Waiting))
            .await
            .map_err(redis_error)?;
        let delayed: usize = conn
            .zcard(self.state_key(JobState::Delayed))
            .await
            .map_err(redis_error)?;
        let active: usize = conn
            .zcard(self.state_key(JobState::Active))
            .await
            .map_err(redis_error)?;
        let waiting_children: usize = conn
            .zcard(self.state_key(JobState::WaitingChildren))
            .await
            .map_err(redis_error)?;
        let completed: usize = conn
            .zcard(self.state_key(JobState::Completed))
            .await
            .map_err(redis_error)?;
        let failed: usize = conn
            .zcard(self.state_key(JobState::Failed))
            .await
            .map_err(redis_error)?;
        let paused = self.is_paused(&mut conn).await?;
        Ok(JobQueueStats {
            total: waiting + delayed + active + waiting_children + completed + failed,
            waiting,
            delayed,
            active,
            waiting_children,
            completed,
            failed,
            paused,
        })
    }
}

impl RedisJobQueue {
    async fn require_job(&self, conn: &mut ConnectionManager, job_id: &str) -> Result<Job> {
        self.load_job(conn, job_id)
            .await?
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))
    }

    async fn ids_for_state(
        &self,
        conn: &mut ConnectionManager,
        state: JobState,
    ) -> Result<Vec<String>> {
        redis::cmd("ZRANGE")
            .arg(self.state_key(state))
            .arg(0)
            .arg(-1)
            .query_async(conn)
            .await
            .map_err(redis_error)
    }

    async fn load_jobs(&self, conn: &mut ConnectionManager, ids: Vec<String>) -> Result<Vec<Job>> {
        let mut jobs = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(job) = self.load_job(conn, &id).await? {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }
}

fn encode_job(job: &Job) -> Result<String> {
    serde_json::to_string(job)
        .map_err(|error| LaneError::Other(format!("failed to encode Redis job: {error}")))
}

fn decode_job(raw: &str) -> Result<Job> {
    serde_json::from_str(raw)
        .map_err(|error| LaneError::Other(format!("failed to decode Redis job: {error}")))
}

fn redis_error(error: redis::RedisError) -> LaneError {
    LaneError::Other(format!("Redis job backend error: {error}"))
}

fn millis(at: DateTime<Utc>) -> f64 {
    at.timestamp_millis() as f64
}

fn subtract_duration(at: DateTime<Utc>, duration: Duration) -> DateTime<Utc> {
    match chrono::Duration::from_std(duration) {
        Ok(delta) => at.checked_sub_signed(delta).unwrap_or(at),
        Err(_) => at,
    }
}

fn waiting_score(priority: JobPriority, sequence: u64) -> f64 {
    (priority as f64 * WAITING_SCORE_BUCKET) + sequence as f64
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

fn require_worker(job: &Job, worker_id: &str) -> Result<()> {
    if job.worker_id.as_deref() == Some(worker_id) {
        Ok(())
    } else {
        Err(LaneError::JobLeaseConflict(format!(
            "worker {worker_id} does not own job {}",
            job.id
        )))
    }
}

fn should_retry(job: &Job) -> bool {
    job.options.retry_policy.max_retries > 0
        && job.attempts_made <= job.options.retry_policy.max_retries
}

fn compare_list_order(a: &Job, b: &Job) -> std::cmp::Ordering {
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

fn job_reference_time(job: &Job) -> DateTime<Utc> {
    job.finished_at
        .or(job.processed_at)
        .unwrap_or(job.scheduled_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn constructor_preserves_namespace_and_queue() {
        let queue = RedisJobQueue::with_namespace("redis://127.0.0.1/", "test:lane", "email")
            .expect("valid Redis URL should build a queue client");

        assert_eq!(queue.namespace(), "test:lane");
        assert_eq!(queue.queue_name(), "email");
        assert_eq!(queue.jobs_key(), "test:lane:email:jobs");
        assert_eq!(
            queue.state_key(JobState::Waiting),
            "test:lane:email:waiting"
        );
    }

    #[test]
    fn waiting_score_preserves_priority_before_fifo_sequence() {
        assert!(waiting_score(1, 99) < waiting_score(2, 1));
        assert!(waiting_score(5, 1) < waiting_score(5, 2));
    }

    #[test]
    fn subtract_duration_saturates_on_invalid_duration() {
        let now = Utc.timestamp_millis_opt(10_000).unwrap();
        assert_eq!(
            subtract_duration(now, Duration::from_millis(250)),
            Utc.timestamp_millis_opt(9_750).unwrap()
        );
    }
}
