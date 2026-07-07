#![cfg(feature = "redis-backend")]

use a3s_lane::{
    DeduplicationOptions, Job, JobListOptions, JobLogEntry, JobOptions, JobPriorityCount,
    JobQueueBackend, JobRateLimit, JobRetention, JobSpec, JobState, JobStateCount, LaneError,
    RedisJobQueue, RepeatOptions, RetryPolicy,
};
use chrono::{DateTime, TimeZone, Utc};
use redis::AsyncCommands;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static NAMESPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn lock_token(job: &a3s_lane::Job) -> &str {
    job.lock_token
        .as_deref()
        .expect("claimed job should carry a lock token")
}

#[test]
fn redis_backend_runs_job_lifecycle_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };

    std::thread::Builder::new()
        .name("redis-job-lifecycle".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Redis lifecycle runtime should build");
            runtime.block_on(async move {
                tokio::time::timeout(Duration::from_secs(420), run_job_lifecycle(redis_url))
                    .await
                    .expect("Redis job lifecycle integration test timed out")
                    .unwrap();
            });
        })
        .expect("Redis lifecycle test thread should spawn")
        .join()
        .expect("Redis lifecycle test thread should finish");
}

#[tokio::test]
async fn redis_backend_discards_configured_retry_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    tokio::time::timeout(Duration::from_secs(120), run_discard_retry(redis_url))
        .await
        .expect("Redis discard retry integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_counts_states_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    tokio::time::timeout(Duration::from_secs(120), run_state_count_indexes(redis_url))
        .await
        .expect("Redis state-count integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_obliterates_queue_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    tokio::time::timeout(Duration::from_secs(120), run_queue_obliterate(redis_url))
        .await
        .expect("Redis queue obliterate integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_keeps_latest_repeat_duplicate_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    tokio::time::timeout(Duration::from_secs(120), run_repeat_keep_last(redis_url))
        .await
        .expect("Redis repeat keep-last integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_orders_lifo_waiting_jobs_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    tokio::time::timeout(Duration::from_secs(120), run_lifo_waiting_order(redis_url))
        .await
        .expect("Redis lifo waiting-order integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_records_queue_events_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    tokio::time::timeout(Duration::from_secs(120), run_queue_events(redis_url))
        .await
        .expect("Redis queue-events integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_applies_finished_retention_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    tokio::time::timeout(Duration::from_secs(120), run_finished_retention(redis_url))
        .await
        .expect("Redis finished-retention integration test timed out")
        .unwrap();
}

async fn run_finished_retention(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "retention")
        .expect("valid Redis URL should build the retention queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let completed_first = queue
        .add_job(
            "completed-first".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("retention:completed:first")
                .with_completion_retention(JobRetention::count(1)),
        )
        .await
        .expect("first completed job should add");
    queue
        .add_log(
            &completed_first.id,
            "first completed log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("first completed log should append");
    let completed_claim = queue
        .claim_next(
            "worker-retention-complete".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first completed job claim should return")
        .expect("first completed job should be claimable");
    queue
        .complete_job(
            &completed_claim.id,
            lock_token(&completed_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("first completed job should complete");

    let completed_second = queue
        .add_job(
            "completed-second".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("retention:completed:second")
                .with_completion_retention(JobRetention::count(1)),
        )
        .await
        .expect("second completed job should add");
    let completed_claim = queue
        .claim_next(
            "worker-retention-complete".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second completed job claim should return")
        .expect("second completed job should be claimable");
    queue
        .complete_job(
            &completed_claim.id,
            lock_token(&completed_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("second completed job should complete");

    let completed_first_exists: bool = conn
        .hexists(format!("{namespace}:retention:jobs"), &completed_first.id)
        .await?;
    assert!(!completed_first_exists);
    let completed_second_exists: bool = conn
        .hexists(format!("{namespace}:retention:jobs"), &completed_second.id)
        .await?;
    assert!(completed_second_exists);
    let completed_count: usize = conn
        .zcard(format!("{namespace}:retention:completed"))
        .await?;
    assert_eq!(completed_count, 1);
    let completed_first_logs: usize = conn
        .llen(format!("{namespace}:retention:logs:{}", completed_first.id))
        .await?;
    assert_eq!(completed_first_logs, 0);

    let failed_first = queue
        .add_job(
            "failed-first".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("retention:failed:first")
                .with_failure_retention(JobRetention::count(1)),
        )
        .await
        .expect("first failed job should add");
    let failed_claim = queue
        .claim_next(
            "worker-retention-fail".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first failed job claim should return")
        .expect("first failed job should be claimable");
    queue
        .fail_job(
            &failed_claim.id,
            lock_token(&failed_claim),
            "boom".to_string(),
            Utc::now(),
        )
        .await
        .expect("first failed job should fail");

    let failed_second = queue
        .add_job(
            "failed-second".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("retention:failed:second")
                .with_failure_retention(JobRetention::count(1)),
        )
        .await
        .expect("second failed job should add");
    let failed_claim = queue
        .claim_next(
            "worker-retention-fail".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second failed job claim should return")
        .expect("second failed job should be claimable");
    queue
        .fail_job(
            &failed_claim.id,
            lock_token(&failed_claim),
            "boom".to_string(),
            Utc::now(),
        )
        .await
        .expect("second failed job should fail");

    let failed_first_exists: bool = conn
        .hexists(format!("{namespace}:retention:jobs"), &failed_first.id)
        .await?;
    assert!(!failed_first_exists);
    let failed_second_exists: bool = conn
        .hexists(format!("{namespace}:retention:jobs"), &failed_second.id)
        .await?;
    assert!(failed_second_exists);
    let failed_count: usize = conn.zcard(format!("{namespace}:retention:failed")).await?;
    assert_eq!(failed_count, 1);

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    Ok(())
}

async fn run_queue_events(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "events")
        .expect("valid Redis URL should build the events queue");
    let job = queue
        .add_job(
            "task".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_job_id("events:task"),
        )
        .await
        .expect("event test job should add");
    let claimed = queue
        .claim_next(
            "worker-events".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("event test claim should succeed")
        .expect("event test job should be claimable");
    queue
        .update_progress(&job.id, serde_json::json!({ "percent": 50 }))
        .await
        .expect("progress should update");
    queue
        .complete_job(
            &job.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("event test job should complete");
    queue.pause().await.expect("queue should pause");
    queue.resume().await.expect("queue should resume");

    let events = queue
        .read_events("-", "+", 20)
        .await
        .expect("events should read");
    let names = events
        .iter()
        .map(|event| event.event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "added",
            "waiting",
            "active",
            "progress",
            "completed",
            "paused",
            "resumed"
        ]
    );
    assert_eq!(events[0].job_id.as_deref(), Some(job.id.as_str()));
    assert_eq!(
        events[0].fields.get("name"),
        Some(&serde_json::Value::String("task".to_string()))
    );
    assert_eq!(events[2].prev, Some(JobState::Waiting));
    assert_eq!(
        events[3].fields.get("data"),
        Some(&serde_json::json!({ "percent": 50 }))
    );
    assert_eq!(events[4].prev, Some(JobState::Active));
    assert_eq!(
        events[4].fields.get("returnvalue"),
        Some(&serde_json::json!({ "ok": true }))
    );

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_lifo_waiting_order(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "lifo-priority")
        .expect("valid Redis URL should build the lifo priority queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let fifo = queue
        .add_job(
            "fifo".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_job_id("fifo").with_priority(5),
        )
        .await
        .expect("fifo job should be added");
    let lifo_old = queue
        .add_job(
            "lifo-old".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("lifo-old")
                .with_priority(5)
                .with_lifo(true),
        )
        .await
        .expect("old lifo job should be added");
    let lifo_new = queue
        .add_job(
            "lifo-new".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("lifo-new")
                .with_priority(5)
                .with_lifo(true),
        )
        .await
        .expect("new lifo job should be added");
    let urgent = queue
        .add_job(
            "urgent".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_job_id("urgent").with_priority(1),
        )
        .await
        .expect("urgent job should be added");

    assert!(fifo.enqueued_seq < lifo_old.enqueued_seq);
    assert!(lifo_old.enqueued_seq < lifo_new.enqueued_seq);
    assert!(lifo_new.enqueued_seq < urgent.enqueued_seq);

    let waiting_key = format!("{namespace}:lifo-priority:waiting");
    let waiting_ids: Vec<String> = redis::cmd("ZRANGE")
        .arg(&waiting_key)
        .arg(0)
        .arg(-1)
        .query_async(&mut conn)
        .await?;
    assert_eq!(
        waiting_ids,
        vec![
            urgent.id.clone(),
            lifo_new.id.clone(),
            lifo_old.id.clone(),
            fifo.id.clone()
        ]
    );

    for expected in [&urgent, &lifo_new, &lifo_old, &fifo] {
        let claimed = queue
            .claim_next(
                "worker-lifo-priority".to_string(),
                Duration::from_secs(30),
                Utc::now(),
            )
            .await
            .expect("claim should succeed")
            .expect("job should be claimable");
        assert_eq!(claimed.id, expected.id);
    }

    let update_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "lifo-update")
        .expect("valid Redis URL should build the lifo update queue");
    let update_fifo = update_queue
        .add_job(
            "fifo".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("update-fifo")
                .with_priority(5),
        )
        .await
        .expect("fifo update job should be added");
    let update_changed = update_queue
        .add_job(
            "changed".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("update-changed")
                .with_priority(10),
        )
        .await
        .expect("changed update job should be added");
    let updated = update_queue
        .update_priority_with_lifo(&update_changed.id, 5, true)
        .await
        .expect("priority update with lifo should succeed");
    assert_eq!(updated.priority, 5);
    assert!(updated.options.lifo);
    assert!(update_fifo.enqueued_seq < updated.enqueued_seq);

    let update_waiting_key = format!("{namespace}:lifo-update:waiting");
    let update_waiting_ids: Vec<String> = redis::cmd("ZRANGE")
        .arg(&update_waiting_key)
        .arg(0)
        .arg(-1)
        .query_async(&mut conn)
        .await?;
    assert_eq!(
        update_waiting_ids,
        vec![update_changed.id.clone(), update_fifo.id.clone()]
    );

    let update_claim = update_queue
        .claim_next(
            "worker-lifo-update".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("lifo update claim should return")
        .expect("lifo-updated job should be claimable");
    assert_eq!(update_claim.id, update_changed.id);

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_repeat_keep_last(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-keep-last:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-keep-last:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-keep-last")
        .expect("valid Redis URL should build the repeat keep-last queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    trace_stage("repeat-keep-last:queue-created");
    let repeat = RepeatOptions::every(Duration::from_secs(60))
        .with_limit(3)
        .with_key("account-sync");
    let deduplication =
        DeduplicationOptions::new("tenant:repeat-keep-last").keep_last_if_active(true);

    let owner = queue
        .add_job(
            "repeat-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_repeat(repeat.clone())
                .with_deduplication(deduplication.clone()),
        )
        .await
        .expect("repeat owner should be added");
    trace_stage("repeat-keep-last:owner-added");
    assert_eq!(owner.repeat_key.as_deref(), Some("account-sync"));
    let owner_id: Option<String> = conn
        .get(format!("{namespace}:repeat-keep-last:repeat:account-sync"))
        .await?;
    assert_eq!(owner_id.as_deref(), Some(owner.id.as_str()));

    let claimed = queue
        .claim_next(
            "worker-repeat-keep-last".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat owner claim should return")
        .expect("repeat owner should be claimable");
    trace_stage("repeat-keep-last:owner-claimed");
    assert_eq!(claimed.id, owner.id);

    let stale_duplicate = queue
        .add_job(
            "repeat-stale".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new()
                .with_repeat(repeat.clone())
                .with_deduplication(deduplication.clone()),
        )
        .await
        .expect("stale repeat duplicate should return owner");
    trace_stage("repeat-keep-last:stale-duplicate-added");
    assert_eq!(stale_duplicate.id, owner.id);

    let latest_duplicate = queue
        .add_job(
            "repeat-latest".to_string(),
            serde_json::json!({ "version": 3 }),
            JobOptions::new()
                .with_delay(Duration::from_millis(150))
                .with_repeat(repeat)
                .with_deduplication(deduplication),
        )
        .await
        .expect("latest repeat duplicate should return owner");
    trace_stage("repeat-keep-last:latest-duplicate-added");
    assert_eq!(latest_duplicate.id, owner.id);

    let next_key =
        format!("{namespace}:repeat-keep-last:deduplication_next:tenant:repeat-keep-last");
    let next_raw: String = conn.get(&next_key).await?;
    trace_stage("repeat-keep-last:next-record-read");
    let next_proto: Job = serde_json::from_str(&next_raw).expect("stored next job should decode");
    assert_eq!(next_proto.name, "repeat-latest");
    assert_eq!(next_proto.repeat_key.as_deref(), Some("account-sync"));

    let complete_at = Utc::now();
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            complete_at,
        )
        .await
        .expect("repeat owner should complete");
    trace_stage("repeat-keep-last:owner-completed");
    let next_after: Option<String> = conn.get(&next_key).await?;
    assert!(next_after.is_none());

    let repeat_owner_after: Option<String> = conn
        .get(format!("{namespace}:repeat-keep-last:repeat:account-sync"))
        .await?;
    assert_eq!(repeat_owner_after.as_deref(), Some(next_proto.id.as_str()));

    let delayed = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .expect("delayed repeat keep-last jobs should list");
    trace_stage("repeat-keep-last:delayed-listed");
    assert_eq!(delayed.total, 1);
    assert_eq!(delayed.jobs[0].id, next_proto.id);
    assert_eq!(delayed.jobs[0].name, "repeat-latest");
    assert_eq!(delayed.jobs[0].payload, serde_json::json!({ "version": 3 }));
    assert_eq!(delayed.jobs[0].repeat_key.as_deref(), Some("account-sync"));
    assert_eq!(delayed.jobs[0].repeat_count, 1);

    let repeats = queue
        .list_repeats()
        .await
        .expect("repeat keep-last owners should list");
    trace_stage("repeat-keep-last:repeats-listed");
    assert_eq!(repeats.len(), 1);
    assert_eq!(repeats[0].key, "account-sync");
    assert_eq!(repeats[0].job_id, next_proto.id);
    assert_eq!(repeats[0].repeat_count, 1);

    sleep_until_due(delayed.jobs[0].scheduled_at).await;
    trace_stage("repeat-keep-last:due-sleep-finished");
    let next_claim = queue
        .claim_next(
            "worker-repeat-keep-last-next".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat keep-last next claim should return")
        .expect("repeat keep-last next job should be claimable");
    trace_stage("repeat-keep-last:next-claimed");
    assert_eq!(next_claim.id, next_proto.id);
    assert_eq!(next_claim.name, "repeat-latest");

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-keep-last:cleanup-final:done");
    Ok(())
}

async fn run_discard_retry(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("discard-retry:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("discard-retry:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "discard-retry")
        .expect("valid Redis URL should build the discard retry queue");
    let job = queue
        .add_job(
            "discard-retry".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_retry_policy(RetryPolicy::fixed(1, Duration::from_secs(30))),
        )
        .await
        .expect("discard retry job should be added");
    let claimed = queue
        .claim_next(
            "worker-discard-retry".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("discard retry claim should return")
        .expect("discard retry job should be claimable");
    assert_eq!(claimed.id, job.id);

    let failed = queue
        .fail_job_discarding_retry(
            &claimed.id,
            lock_token(&claimed),
            "unrecoverable".to_string(),
            Utc::now(),
        )
        .await
        .expect("discard retry job should fail terminally");
    assert_eq!(failed.state, JobState::Failed);
    assert!(failed.finished_at.is_some());

    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let delayed_score: Option<f64> = conn
        .zscore(format!("{namespace}:discard-retry:delayed"), &job.id)
        .await?;
    assert!(delayed_score.is_none());
    let failed_score: Option<f64> = conn
        .zscore(format!("{namespace}:discard-retry:failed"), &job.id)
        .await?;
    assert!(failed_score.is_some());

    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("discard-retry:done");
    Ok(())
}

async fn run_queue_obliterate(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "obliterate")
        .expect("valid Redis URL should build the obliterate queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let active = queue
        .add_job(
            "active".to_string(),
            serde_json::json!({ "kind": "active" }),
            JobOptions::new().with_priority(1),
        )
        .await
        .expect("active job should be added");
    let active_claim = queue
        .claim_next(
            "worker-active".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("active claim should return")
        .expect("active job should be claimable");
    assert_eq!(active_claim.id, active.id);

    let completed = queue
        .add_job(
            "completed".to_string(),
            serde_json::json!({ "kind": "completed" }),
            JobOptions::new().with_priority(1),
        )
        .await
        .expect("completed job should be added");
    let completed_claim = queue
        .claim_next(
            "worker-completed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("completed claim should return")
        .expect("completed job should be claimable");
    assert_eq!(completed_claim.id, completed.id);
    queue
        .complete_job(
            &completed.id,
            lock_token(&completed_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("completed job should complete");

    let failed = queue
        .add_job(
            "failed".to_string(),
            serde_json::json!({ "kind": "failed" }),
            JobOptions::new().with_priority(1),
        )
        .await
        .expect("failed job should be added");
    let failed_claim = queue
        .claim_next(
            "worker-failed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("failed claim should return")
        .expect("failed job should be claimable");
    assert_eq!(failed_claim.id, failed.id);
    queue
        .fail_job(
            &failed.id,
            lock_token(&failed_claim),
            "boom".to_string(),
            Utc::now(),
        )
        .await
        .expect("failed job should fail terminally");

    let waiting = queue
        .add_job(
            "waiting".to_string(),
            serde_json::json!({ "kind": "waiting" }),
            JobOptions::new()
                .with_priority(50)
                .with_deduplication_id("tenant:one"),
        )
        .await
        .expect("waiting job should be added");
    let duplicate_waiting = queue
        .add_job(
            "waiting-duplicate".to_string(),
            serde_json::json!({ "kind": "duplicate" }),
            JobOptions::new().with_deduplication_id("tenant:one"),
        )
        .await
        .expect("duplicate waiting job should return existing owner");
    assert_eq!(duplicate_waiting.id, waiting.id);
    queue
        .add_log(&waiting.id, "queued".to_string(), 10, Utc::now())
        .await
        .expect("waiting job log should be retained");

    let delayed = queue
        .add_job(
            "delayed".to_string(),
            serde_json::json!({ "kind": "delayed" }),
            JobOptions::new().with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("delayed job should be added");

    let keep_owner = queue
        .add_job(
            "keep-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_priority(2).with_deduplication(
                DeduplicationOptions::new("tenant:keep").keep_last_if_active(true),
            ),
        )
        .await
        .expect("keep-last owner should be added");
    let keep_claim = queue
        .claim_next(
            "worker-keep".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("keep-last claim should return")
        .expect("keep-last owner should be claimable");
    assert_eq!(keep_claim.id, keep_owner.id);
    let keep_duplicate = queue
        .add_job(
            "keep-duplicate".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:keep").keep_last_if_active(true),
            ),
        )
        .await
        .expect("keep-last duplicate should return active owner");
    assert_eq!(keep_duplicate.id, keep_owner.id);

    let before = queue.stats().await.expect("obliterate stats should load");
    assert_eq!(before.total, 6);
    assert_eq!(before.active, 2);

    let error = queue
        .obliterate(false)
        .await
        .expect_err("non-forced obliterate should reject active jobs");
    assert!(matches!(error, LaneError::JobStateConflict(_)));
    let meta_key = format!("{namespace}:obliterate:meta");
    let paused_raw: Option<u8> = conn.hget(&meta_key, "paused").await?;
    assert_eq!(paused_raw, Some(1));
    assert!(queue
        .claim_next(
            "worker-paused".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("paused queue claim should return")
        .is_none());

    let removed = queue
        .obliterate(true)
        .await
        .expect("forced obliterate should remove queue data");
    assert_eq!(removed, before.total);

    let mut cursor = 0_u64;
    let mut remaining_keys = Vec::new();
    loop {
        let (next_cursor, mut keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(format!("{namespace}:obliterate:*"))
            .arg("COUNT")
            .arg(100_u16)
            .query_async(&mut conn)
            .await?;
        remaining_keys.append(&mut keys);
        if next_cursor == 0 {
            break;
        }
        cursor = next_cursor;
    }
    assert!(
        remaining_keys.is_empty(),
        "obliterate should delete queue-prefixed keys: {remaining_keys:?}"
    );

    let stats = queue.stats().await.expect("empty stats should load");
    assert_eq!(stats.total, 0);
    assert_eq!(stats.waiting, 0);
    assert_eq!(stats.delayed, 0);
    assert_eq!(stats.active, 0);
    assert_eq!(stats.completed, 0);
    assert_eq!(stats.failed, 0);
    assert!(!stats.paused);
    for job in [
        &active,
        &completed,
        &failed,
        &waiting,
        &delayed,
        &keep_owner,
    ] {
        assert!(queue
            .get_job(&job.id)
            .await
            .expect("removed job lookup should return")
            .is_none());
    }
    assert!(queue
        .get_deduplication_job_id("tenant:one")
        .await
        .expect("dedup owner lookup should return")
        .is_none());
    let logs = queue
        .get_job_logs(&waiting.id, 0, -1, true)
        .await
        .expect("removed job logs should return empty page");
    assert_eq!(logs.count, 0);
    assert!(logs.logs.is_empty());

    let after = queue
        .add_job(
            "after".to_string(),
            serde_json::json!({ "kind": "after" }),
            JobOptions::new().with_deduplication_id("tenant:one"),
        )
        .await
        .expect("queue should accept jobs after obliterate");
    assert_ne!(after.id, waiting.id);
    let claimed_after = queue
        .claim_next(
            "worker-after".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("after claim should return")
        .expect("after job should be claimable");
    assert_eq!(claimed_after.id, after.id);

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    Ok(())
}

async fn run_job_lifecycle(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("cleanup:done");

    let producer = RedisJobQueue::with_namespace(&redis_url, &namespace, "jobs")
        .expect("valid Redis URL should build the producer queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "jobs")
        .expect("valid Redis URL should build the worker queue");
    trace_stage("queues:created");

    let dedup_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "dedup")
        .expect("valid Redis URL should build the dedup queue");
    let first_dedup = dedup_queue
        .add_job(
            "dedup-sync".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication_id("tenant:42"),
        )
        .await
        .expect("dedup job should be added");
    let duplicate_dedup = dedup_queue
        .add_job(
            "dedup-sync-duplicate".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication_id("tenant:42"),
        )
        .await
        .expect("duplicate dedup job should return existing job");
    assert_eq!(duplicate_dedup, first_dedup);
    let mut dedup_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let dedup_owner: Option<String> = dedup_conn
        .get(format!("{namespace}:dedup:deduplication:tenant:42"))
        .await?;
    assert_eq!(dedup_owner.as_deref(), Some(first_dedup.id.as_str()));

    let first_dedup_claim = dedup_queue
        .claim_next(
            "worker-dedup".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dedup claim should return")
        .expect("dedup job should be claimable");
    dedup_queue
        .complete_job(
            &first_dedup_claim.id,
            lock_token(&first_dedup_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("dedup job should complete");
    let released_dedup_owner: Option<String> = dedup_conn
        .get(format!("{namespace}:dedup:deduplication:tenant:42"))
        .await?;
    assert!(released_dedup_owner.is_none());

    let after_terminal_dedup = dedup_queue
        .add_job(
            "dedup-after-terminal".to_string(),
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication_id("tenant:42"),
        )
        .await
        .expect("dedup id should be reusable after terminal completion");
    assert_ne!(after_terminal_dedup.id, first_dedup.id);
    dedup_queue
        .remove_job(&after_terminal_dedup.id)
        .await
        .expect("dedup waiting job should remove")
        .expect("dedup waiting job should be returned");
    let removed_dedup_owner: Option<String> = dedup_conn
        .get(format!("{namespace}:dedup:deduplication:tenant:42"))
        .await?;
    assert!(removed_dedup_owner.is_none());

    let manual_release_dedup = dedup_queue
        .add_job(
            "dedup-manual-release".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication_id("tenant:manual-release"),
        )
        .await
        .expect("manual-release dedup job should be added");
    let manual_release_duplicate = dedup_queue
        .add_job(
            "dedup-manual-release-duplicate".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication_id("tenant:manual-release"),
        )
        .await
        .expect("manual-release duplicate should return owner");
    assert_eq!(manual_release_duplicate.id, manual_release_dedup.id);
    let manual_release_owner: Option<String> = dedup_conn
        .get(format!(
            "{namespace}:dedup:deduplication:tenant:manual-release"
        ))
        .await?;
    assert_eq!(
        manual_release_owner.as_deref(),
        Some(manual_release_dedup.id.as_str())
    );
    assert_eq!(
        dedup_queue
            .get_deduplication_job_id("tenant:manual-release")
            .await
            .expect("manual-release dedup owner should load")
            .as_deref(),
        Some(manual_release_dedup.id.as_str())
    );
    assert!(dedup_queue
        .get_deduplication_job_id("tenant:missing-manual-release")
        .await
        .expect("missing dedup owner should load")
        .is_none());
    assert!(dedup_queue
        .get_deduplication_job_id("")
        .await
        .expect("empty dedup owner should load")
        .is_none());
    assert!(dedup_queue
        .remove_deduplication_key("tenant:manual-release")
        .await
        .expect("manual-release dedup key removal should return"));
    assert!(dedup_queue
        .get_deduplication_job_id("tenant:manual-release")
        .await
        .expect("manual-release owner should be absent after removal")
        .is_none());
    assert!(!dedup_queue
        .remove_deduplication_key("tenant:missing-manual-release")
        .await
        .expect("missing dedup key removal should return"));
    let manual_release_owner_after_remove: Option<String> = dedup_conn
        .get(format!(
            "{namespace}:dedup:deduplication:tenant:manual-release"
        ))
        .await?;
    assert!(manual_release_owner_after_remove.is_none());
    let manual_release_new_owner = dedup_queue
        .add_job(
            "dedup-manual-release-new-owner".to_string(),
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication_id("tenant:manual-release"),
        )
        .await
        .expect("manual-release new owner should be added");
    assert_ne!(manual_release_new_owner.id, manual_release_dedup.id);
    let manual_release_new_owner_key: Option<String> = dedup_conn
        .get(format!(
            "{namespace}:dedup:deduplication:tenant:manual-release"
        ))
        .await?;
    assert_eq!(
        manual_release_new_owner_key.as_deref(),
        Some(manual_release_new_owner.id.as_str())
    );
    assert_eq!(
        dedup_queue
            .get_deduplication_job_id("tenant:manual-release")
            .await
            .expect("manual-release new owner should load")
            .as_deref(),
        Some(manual_release_new_owner.id.as_str())
    );
    dedup_queue
        .remove_job(&manual_release_dedup.id)
        .await
        .expect("manual-release old owner should remove")
        .expect("manual-release old owner should be returned");
    dedup_queue
        .remove_job(&manual_release_new_owner.id)
        .await
        .expect("manual-release new owner should remove")
        .expect("manual-release new owner should be returned");
    let manual_release_key = format!("{namespace}:dedup:deduplication:tenant:manual-release");
    let _: () = dedup_conn
        .set(&manual_release_key, &manual_release_new_owner.id)
        .await?;
    assert!(dedup_queue
        .get_deduplication_job_id("tenant:manual-release")
        .await
        .expect("stale manual-release owner should load")
        .is_none());
    let stale_manual_release_key: Option<String> = dedup_conn.get(&manual_release_key).await?;
    assert!(stale_manual_release_key.is_none());

    let fail_dedup = dedup_queue
        .add_job(
            "dedup-fail".to_string(),
            serde_json::json!({ "version": 4 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:fail").with_ttl(Duration::from_secs(5)),
            ),
        )
        .await
        .expect("dedup fail job should be added");
    let fail_dedup_claim = dedup_queue
        .claim_next(
            "worker-dedup-fail".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dedup fail claim should return")
        .expect("dedup fail job should be claimable");
    assert_eq!(fail_dedup_claim.id, fail_dedup.id);
    dedup_queue
        .fail_job(
            &fail_dedup_claim.id,
            lock_token(&fail_dedup_claim),
            "terminal failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("terminal failure should release dedup key");
    let failed_dedup_owner: Option<String> = dedup_conn
        .get(format!("{namespace}:dedup:deduplication:tenant:fail"))
        .await?;
    assert!(failed_dedup_owner.is_none());
    let retried_dedup = dedup_queue
        .retry_job(&fail_dedup.id, Utc::now())
        .await
        .expect("dedup retry should move failed job back to waiting");
    assert_eq!(retried_dedup.id, fail_dedup.id);
    let retry_dedup_owner: Option<String> = dedup_conn
        .get(format!("{namespace}:dedup:deduplication:tenant:fail"))
        .await?;
    assert_eq!(retry_dedup_owner.as_deref(), Some(fail_dedup.id.as_str()));
    let retry_dedup_ttl: i64 = redis::cmd("PTTL")
        .arg(format!("{namespace}:dedup:deduplication:tenant:fail"))
        .query_async(&mut dedup_conn)
        .await?;
    assert!(retry_dedup_ttl > 0);
    let retry_duplicate = dedup_queue
        .add_job(
            "dedup-fail-duplicate".to_string(),
            serde_json::json!({ "version": 5 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:fail").with_ttl(Duration::from_secs(5)),
            ),
        )
        .await
        .expect("duplicate add after dedup retry should return retried job");
    assert_eq!(retry_duplicate.id, fail_dedup.id);
    dedup_queue
        .remove_job(&fail_dedup.id)
        .await
        .expect("retried dedup job should remove")
        .expect("retried dedup job should be returned");
    let removed_retry_dedup_owner: Option<String> = dedup_conn
        .get(format!("{namespace}:dedup:deduplication:tenant:fail"))
        .await?;
    assert!(removed_retry_dedup_owner.is_none());

    let retry_conflict_a = dedup_queue
        .add_job(
            "dedup-retry-conflict-a".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_deduplication_id("tenant:retry-conflict"),
        )
        .await
        .expect("retry conflict first job should be added");
    let retry_conflict_claim = dedup_queue
        .claim_next(
            "worker-dedup-retry-conflict".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("retry conflict claim should return")
        .expect("retry conflict job should be claimable");
    assert_eq!(retry_conflict_claim.id, retry_conflict_a.id);
    dedup_queue
        .fail_job(
            &retry_conflict_claim.id,
            lock_token(&retry_conflict_claim),
            "terminal conflict".to_string(),
            Utc::now(),
        )
        .await
        .expect("retry conflict first job should fail");
    let retry_conflict_b = dedup_queue
        .add_job(
            "dedup-retry-conflict-b".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_deduplication_id("tenant:retry-conflict"),
        )
        .await
        .expect("retry conflict second job should be added");
    assert_ne!(retry_conflict_b.id, retry_conflict_a.id);
    let retry_conflict = dedup_queue
        .retry_job(&retry_conflict_a.id, Utc::now())
        .await
        .expect_err("retry should reject a dedup id owned by another non-terminal job");
    assert!(matches!(retry_conflict, LaneError::JobStateConflict(_)));
    let retry_conflict_failed_score: Option<f64> = dedup_conn
        .zscore(format!("{namespace}:dedup:failed"), &retry_conflict_a.id)
        .await?;
    assert!(retry_conflict_failed_score.is_some());
    let retry_conflict_owner: Option<String> = dedup_conn
        .get(format!(
            "{namespace}:dedup:deduplication:tenant:retry-conflict"
        ))
        .await?;
    assert_eq!(
        retry_conflict_owner.as_deref(),
        Some(retry_conflict_b.id.as_str())
    );
    dedup_queue
        .remove_job(&retry_conflict_b.id)
        .await
        .expect("retry conflict second job should remove")
        .expect("retry conflict second job should be returned");

    let clean_dedup = dedup_queue
        .add_job(
            "dedup-clean".to_string(),
            serde_json::json!({ "version": 5 }),
            JobOptions::new().with_deduplication_id("tenant:clean"),
        )
        .await
        .expect("dedup clean job should be added");
    let cleaned_dedup = dedup_queue
        .clean_jobs(JobState::Waiting, Duration::ZERO, 1, Utc::now())
        .await
        .expect("clean should release dedup key");
    assert_eq!(cleaned_dedup.len(), 1);
    assert_eq!(cleaned_dedup[0].id, clean_dedup.id);
    let cleaned_dedup_owner: Option<String> = dedup_conn
        .get(format!("{namespace}:dedup:deduplication:tenant:clean"))
        .await?;
    assert!(cleaned_dedup_owner.is_none());

    let ttl_dedup_key = format!("{namespace}:dedup:deduplication:tenant:ttl");
    let ttl_dedup = dedup_queue
        .add_job(
            "dedup-ttl".to_string(),
            serde_json::json!({ "version": 6 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:ttl").with_ttl(Duration::from_secs(1)),
            ),
        )
        .await
        .expect("ttl dedup job should be added");
    let ttl_duplicate = dedup_queue
        .add_job(
            "dedup-ttl-duplicate".to_string(),
            serde_json::json!({ "version": 7 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:ttl").with_ttl(Duration::from_secs(1)),
            ),
        )
        .await
        .expect("duplicate before ttl should return owner");
    assert_eq!(ttl_duplicate.id, ttl_dedup.id);
    let ttl_dedup_pttl: i64 = redis::cmd("PTTL")
        .arg(&ttl_dedup_key)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(ttl_dedup_pttl > 0);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let ttl_after_expiration = dedup_queue
        .add_job(
            "dedup-ttl-after-expiration".to_string(),
            serde_json::json!({ "version": 8 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:ttl").with_ttl(Duration::from_secs(1)),
            ),
        )
        .await
        .expect("dedup id should be reusable after ttl");
    assert_ne!(ttl_after_expiration.id, ttl_dedup.id);
    let ttl_owner_after_expiration: Option<String> = dedup_conn.get(&ttl_dedup_key).await?;
    assert_eq!(
        ttl_owner_after_expiration.as_deref(),
        Some(ttl_after_expiration.id.as_str())
    );
    dedup_queue
        .remove_job(&ttl_dedup.id)
        .await
        .expect("expired ttl owner should remove")
        .expect("expired ttl owner should be returned");
    let ttl_owner_after_old_remove: Option<String> = dedup_conn.get(&ttl_dedup_key).await?;
    assert_eq!(
        ttl_owner_after_old_remove.as_deref(),
        Some(ttl_after_expiration.id.as_str())
    );
    dedup_queue
        .remove_job(&ttl_after_expiration.id)
        .await
        .expect("current ttl owner should remove")
        .expect("current ttl owner should be returned");
    let ttl_owner_after_current_remove: Option<String> = dedup_conn.get(&ttl_dedup_key).await?;
    assert!(ttl_owner_after_current_remove.is_none());

    let extend_dedup_key = format!("{namespace}:dedup:deduplication:tenant:extend");
    let extend_owner = dedup_queue
        .add_job(
            "dedup-extend".to_string(),
            serde_json::json!({ "version": 9 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:extend")
                    .with_ttl(Duration::from_secs(5))
                    .extend_ttl(true),
            ),
        )
        .await
        .expect("extend dedup job should be added");
    let extend_ttl_shortened: bool = redis::cmd("PEXPIRE")
        .arg(&extend_dedup_key)
        .arg(250)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(extend_ttl_shortened);
    let extend_duplicate = dedup_queue
        .add_job(
            "dedup-extend-duplicate".to_string(),
            serde_json::json!({ "version": 10 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:extend")
                    .with_ttl(Duration::from_secs(5))
                    .extend_ttl(true),
            ),
        )
        .await
        .expect("extend duplicate should return owner");
    assert_eq!(extend_duplicate.id, extend_owner.id);
    let extend_ttl_after_duplicate: i64 = redis::cmd("PTTL")
        .arg(&extend_dedup_key)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(extend_ttl_after_duplicate > 1_000);
    dedup_queue
        .remove_job(&extend_owner.id)
        .await
        .expect("extend owner should remove")
        .expect("extend owner should be returned");

    let replace_dedup_key = format!("{namespace}:dedup:deduplication:tenant:replace");
    let replace_old = dedup_queue
        .add_job(
            "dedup-replace-old".to_string(),
            serde_json::json!({ "version": 11 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(30))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace").replace_delayed(true),
                ),
        )
        .await
        .expect("replace old dedup job should be added");
    let replace_old_score: Option<f64> = dedup_conn
        .zscore(format!("{namespace}:dedup:delayed"), &replace_old.id)
        .await?;
    assert!(replace_old_score.is_some());
    dedup_queue
        .add_log(
            &replace_old.id,
            "old delayed owner log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("replace old owner log should append");
    let replace_old_logs_key = format!("{namespace}:dedup:logs:{}", replace_old.id);
    let replace_old_logs_len: usize = dedup_conn.llen(&replace_old_logs_key).await?;
    assert_eq!(replace_old_logs_len, 1);
    let replace_new = dedup_queue
        .add_job(
            "dedup-replace-new".to_string(),
            serde_json::json!({ "version": 12 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(60))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace").replace_delayed(true),
                ),
        )
        .await
        .expect("replace should insert a new delayed owner");
    assert_ne!(replace_new.id, replace_old.id);
    let replace_owner: Option<String> = dedup_conn.get(&replace_dedup_key).await?;
    assert_eq!(replace_owner.as_deref(), Some(replace_new.id.as_str()));
    let replace_old_hash: Option<String> = dedup_conn
        .hget(format!("{namespace}:dedup:jobs"), &replace_old.id)
        .await?;
    assert!(replace_old_hash.is_none());
    let replace_old_logs_after: usize = dedup_conn.llen(&replace_old_logs_key).await?;
    assert_eq!(replace_old_logs_after, 0);
    let replace_old_score_after: Option<f64> = dedup_conn
        .zscore(format!("{namespace}:dedup:delayed"), &replace_old.id)
        .await?;
    assert!(replace_old_score_after.is_none());
    let replace_new_score: Option<f64> = dedup_conn
        .zscore(format!("{namespace}:dedup:delayed"), &replace_new.id)
        .await?;
    assert!(replace_new_score.is_some());
    dedup_queue
        .remove_job(&replace_new.id)
        .await
        .expect("replace new owner should remove")
        .expect("replace new owner should be returned");
    let replace_owner_after_remove: Option<String> = dedup_conn.get(&replace_dedup_key).await?;
    assert!(replace_owner_after_remove.is_none());

    let replace_ttl_key = format!("{namespace}:dedup:deduplication:tenant:replace-ttl");
    let _replace_ttl_old = dedup_queue
        .add_job(
            "dedup-replace-ttl-old".to_string(),
            serde_json::json!({ "version": 13 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(30))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace-ttl")
                        .with_ttl(Duration::from_secs(5))
                        .replace_delayed(true),
                ),
        )
        .await
        .expect("replace ttl old dedup job should be added");
    let ttl_overridden: bool = redis::cmd("PEXPIRE")
        .arg(&replace_ttl_key)
        .arg(750)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(ttl_overridden);
    let replace_ttl_before: i64 = redis::cmd("PTTL")
        .arg(&replace_ttl_key)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(replace_ttl_before > 0);
    let replace_ttl_new = dedup_queue
        .add_job(
            "dedup-replace-ttl-new".to_string(),
            serde_json::json!({ "version": 14 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(60))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace-ttl")
                        .with_ttl(Duration::from_secs(5))
                        .replace_delayed(true),
                ),
        )
        .await
        .expect("replace ttl should insert a new delayed owner");
    let replace_ttl_after: i64 = redis::cmd("PTTL")
        .arg(&replace_ttl_key)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(replace_ttl_after > 0);
    assert!(
        replace_ttl_after <= 1_000,
        "expected replace to preserve the short deduplication TTL instead of refreshing to the job TTL, before {replace_ttl_before}, after {replace_ttl_after}"
    );
    dedup_queue
        .remove_job(&replace_ttl_new.id)
        .await
        .expect("replace ttl new owner should remove")
        .expect("replace ttl new owner should be returned");

    let replace_extend_key = format!("{namespace}:dedup:deduplication:tenant:replace-extend");
    let replace_extend_old = dedup_queue
        .add_job(
            "dedup-replace-extend-old".to_string(),
            serde_json::json!({ "version": 15 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(30))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace-extend")
                        .with_ttl(Duration::from_secs(5))
                        .replace_delayed(true)
                        .extend_ttl(true),
                ),
        )
        .await
        .expect("replace extend old dedup job should be added");
    let replace_extend_shortened: bool = redis::cmd("PEXPIRE")
        .arg(&replace_extend_key)
        .arg(250)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(replace_extend_shortened);
    let replace_extend_new = dedup_queue
        .add_job(
            "dedup-replace-extend-new".to_string(),
            serde_json::json!({ "version": 16 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(60))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace-extend")
                        .with_ttl(Duration::from_secs(5))
                        .replace_delayed(true)
                        .extend_ttl(true),
                ),
        )
        .await
        .expect("replace extend should insert a new delayed owner");
    assert_ne!(replace_extend_new.id, replace_extend_old.id);
    let replace_extend_ttl: i64 = redis::cmd("PTTL")
        .arg(&replace_extend_key)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(replace_extend_ttl > 1_000);
    dedup_queue
        .remove_job(&replace_extend_new.id)
        .await
        .expect("replace extend new owner should remove")
        .expect("replace extend new owner should be returned");

    let replace_stale_key = format!("{namespace}:dedup:deduplication:tenant:replace-stale");
    let replace_stale_old = dedup_queue
        .add_job(
            "dedup-replace-stale-old".to_string(),
            serde_json::json!({ "version": 17 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(30))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace-stale").replace_delayed(true),
                ),
        )
        .await
        .expect("replace stale old dedup job should be added");
    let stale_removed: usize = dedup_conn
        .zrem(format!("{namespace}:dedup:delayed"), &replace_stale_old.id)
        .await?;
    assert_eq!(stale_removed, 1);
    let replace_stale_duplicate = dedup_queue
        .add_job(
            "dedup-replace-stale-new".to_string(),
            serde_json::json!({ "version": 18 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(60))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace-stale").replace_delayed(true),
                ),
        )
        .await
        .expect("stale replace should return the old owner");
    assert_eq!(replace_stale_duplicate.id, replace_stale_old.id);
    let replace_stale_owner: Option<String> = dedup_conn.get(&replace_stale_key).await?;
    assert_eq!(
        replace_stale_owner.as_deref(),
        Some(replace_stale_old.id.as_str())
    );
    let replace_stale_hash: Option<String> = dedup_conn
        .hget(format!("{namespace}:dedup:jobs"), &replace_stale_old.id)
        .await?;
    assert!(replace_stale_hash.is_some());
    dedup_queue
        .remove_job(&replace_stale_old.id)
        .await
        .expect("stale old owner should remove")
        .expect("stale old owner should be returned");

    let keep_last_key = format!("{namespace}:dedup:deduplication:tenant:keep-last");
    let keep_last_next_key = format!("{namespace}:dedup:deduplication_next:tenant:keep-last");
    let keep_last_owner = dedup_queue
        .add_job(
            "dedup-keep-last-owner".to_string(),
            serde_json::json!({ "version": 19 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:keep-last")
                    .with_ttl(Duration::from_secs(30))
                    .keep_last_if_active(true),
            ),
        )
        .await
        .expect("keep-last owner should be added");
    let keep_last_ttl: i64 = redis::cmd("PTTL")
        .arg(&keep_last_key)
        .query_async(&mut dedup_conn)
        .await?;
    assert_eq!(keep_last_ttl, -1);
    let keep_last_claim = dedup_queue
        .claim_next(
            "worker-keep-last".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("keep-last owner should be claimable")
        .expect("keep-last owner should be returned");
    assert_eq!(keep_last_claim.id, keep_last_owner.id);
    let keep_last_stale = dedup_queue
        .add_job(
            "dedup-keep-last-stale".to_string(),
            serde_json::json!({ "version": 20 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:keep-last").keep_last_if_active(true),
            ),
        )
        .await
        .expect("keep-last stale duplicate should return owner");
    assert_eq!(keep_last_stale.id, keep_last_owner.id);
    let keep_last_latest = dedup_queue
        .add_job(
            "dedup-keep-last-latest".to_string(),
            serde_json::json!({ "version": 21 }),
            JobOptions::new()
                .with_delay(Duration::from_millis(150))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:keep-last").keep_last_if_active(true),
                ),
        )
        .await
        .expect("keep-last latest duplicate should return owner");
    assert_eq!(keep_last_latest.id, keep_last_owner.id);
    let keep_last_next_raw: String = dedup_conn.get(&keep_last_next_key).await?;
    let keep_last_next: Job =
        serde_json::from_str(&keep_last_next_raw).expect("stored next job should decode");
    assert_eq!(keep_last_next.name, "dedup-keep-last-latest");

    let complete_keep_last_at = Utc::now();
    dedup_queue
        .complete_job(
            &keep_last_claim.id,
            lock_token(&keep_last_claim),
            serde_json::json!({ "ok": true }),
            complete_keep_last_at,
        )
        .await
        .expect("keep-last owner should complete");
    let keep_last_next_after: Option<String> = dedup_conn.get(&keep_last_next_key).await?;
    assert!(keep_last_next_after.is_none());
    let keep_last_owner_after: Option<String> = dedup_conn.get(&keep_last_key).await?;
    assert_eq!(
        keep_last_owner_after.as_deref(),
        Some(keep_last_next.id.as_str())
    );
    let keep_last_materialized = dedup_queue
        .get_job(&keep_last_next.id)
        .await
        .expect("keep-last materialized job should load")
        .expect("keep-last materialized job should exist");
    assert_eq!(keep_last_materialized.name, "dedup-keep-last-latest");
    assert_eq!(keep_last_materialized.state, JobState::Delayed);
    assert!(keep_last_materialized.scheduled_at >= complete_keep_last_at);
    let keep_last_delayed_score: Option<f64> = dedup_conn
        .zscore(format!("{namespace}:dedup:delayed"), &keep_last_next.id)
        .await?;
    assert!(keep_last_delayed_score.is_some());
    sleep_until_due(keep_last_materialized.scheduled_at).await;
    let promoted_keep_last = dedup_queue
        .promote_due_jobs(Utc::now())
        .await
        .expect("keep-last delayed next should promote");
    assert_eq!(promoted_keep_last, 1);
    let keep_last_next_claim = dedup_queue
        .claim_next(
            "worker-keep-last-next".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("keep-last next should be claimable")
        .expect("keep-last next should be returned");
    assert_eq!(keep_last_next_claim.id, keep_last_next.id);
    dedup_queue
        .complete_job(
            &keep_last_next_claim.id,
            lock_token(&keep_last_next_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("keep-last next should complete");
    trace_stage("dedup:done");

    let priority_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "priority")
        .expect("valid Redis URL should build the priority queue");
    let first_priority = priority_queue
        .add_job(
            "first-priority".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_priority(50),
        )
        .await
        .expect("first priority job should be added");
    let second_priority = priority_queue
        .add_job(
            "second-priority".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_priority(60),
        )
        .await
        .expect("second priority job should be added");
    priority_queue
        .update_priority(&second_priority.id, 1)
        .await
        .expect("priority should update");
    let priority_counts = priority_queue
        .get_counts_per_priority(&[1, 50, 60, 1])
        .await
        .expect("priority counts should load");
    assert_eq!(
        priority_counts,
        vec![
            JobPriorityCount {
                priority: 1,
                count: 1,
            },
            JobPriorityCount {
                priority: 50,
                count: 1,
            },
            JobPriorityCount {
                priority: 60,
                count: 0,
            },
        ]
    );
    let mut priority_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let priority_one_zcount: usize = redis::cmd("ZCOUNT")
        .arg(format!("{namespace}:priority:waiting"))
        .arg(1_000_000_000_000_f64)
        .arg(1_999_999_999_999_f64)
        .query_async(&mut priority_conn)
        .await?;
    assert_eq!(priority_one_zcount, 1);
    let priority_claim = priority_queue
        .claim_next(
            "worker-priority".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("priority claim should return")
        .expect("updated priority job should be claimable");
    assert_eq!(priority_claim.id, second_priority.id);
    assert_ne!(priority_claim.id, first_priority.id);
    priority_queue
        .complete_job(
            &priority_claim.id,
            lock_token(&priority_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("priority job should complete");
    let _: usize = priority_conn
        .zadd(
            format!("{namespace}:priority:waiting"),
            &second_priority.id,
            0.0,
        )
        .await?;
    let terminal_priority_update = priority_queue
        .update_priority(&second_priority.id, 5)
        .await
        .expect_err("terminal job with stale waiting index should reject priority update");
    assert!(matches!(
        terminal_priority_update,
        LaneError::JobStateConflict(_)
    ));
    let stale_terminal_waiting_score: Option<f64> = priority_conn
        .zscore(format!("{namespace}:priority:waiting"), &second_priority.id)
        .await?;
    assert!(stale_terminal_waiting_score.is_none());
    let _: usize = priority_conn
        .zadd(
            format!("{namespace}:priority:waiting"),
            "missing-priority-job",
            0.0,
        )
        .await?;
    let missing_priority_update = priority_queue
        .update_priority("missing-priority-job", 5)
        .await
        .expect_err("missing job should still be reported as missing");
    assert!(matches!(missing_priority_update, LaneError::JobNotFound(_)));
    let missing_priority_waiting_score: Option<f64> = priority_conn
        .zscore(
            format!("{namespace}:priority:waiting"),
            "missing-priority-job",
        )
        .await?;
    assert!(missing_priority_waiting_score.is_none());
    let delayed_priority_index = priority_queue
        .add_job(
            "priority-stale-delayed".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_priority(90)
                .with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("delayed priority stale-index job should add");
    let _: usize = priority_conn
        .zadd(
            format!("{namespace}:priority:waiting"),
            &delayed_priority_index.id,
            0.0,
        )
        .await?;
    let updated_delayed_priority = priority_queue
        .update_priority(&delayed_priority_index.id, 7)
        .await
        .expect("delayed priority update should update hash and prune stale waiting index");
    assert_eq!(updated_delayed_priority.state, JobState::Delayed);
    assert_eq!(updated_delayed_priority.priority, 7);
    let delayed_priority_waiting_score: Option<f64> = priority_conn
        .zscore(
            format!("{namespace}:priority:waiting"),
            &delayed_priority_index.id,
        )
        .await?;
    assert!(delayed_priority_waiting_score.is_none());
    let delayed_priority_delayed_score: Option<f64> = priority_conn
        .zscore(
            format!("{namespace}:priority:delayed"),
            &delayed_priority_index.id,
        )
        .await?;
    assert!(delayed_priority_delayed_score.is_some());
    let counts_after_delayed_update = priority_queue
        .get_counts_per_priority(&[7, 50])
        .await
        .expect("priority counts after delayed update should load");
    assert_eq!(
        counts_after_delayed_update,
        vec![
            JobPriorityCount {
                priority: 7,
                count: 0,
            },
            JobPriorityCount {
                priority: 50,
                count: 1,
            },
        ]
    );
    trace_stage("priority:done");

    let delayed_priority_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "delayed-priority")
            .expect("valid Redis URL should build the delayed priority queue");
    let delayed_priority_slow = delayed_priority_queue
        .add_job(
            "delayed-priority-slow".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_priority(50)
                .with_delay(Duration::from_millis(120)),
        )
        .await
        .expect("slow delayed priority job should be added");
    let delayed_priority_fast = delayed_priority_queue
        .add_job(
            "delayed-priority-fast".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_priority(60)
                .with_delay(Duration::from_millis(120)),
        )
        .await
        .expect("fast delayed priority job should be added");
    delayed_priority_queue
        .update_priority(&delayed_priority_fast.id, 1)
        .await
        .expect("delayed priority should update in the job hash");
    tokio::time::sleep(Duration::from_millis(160)).await;
    let delayed_priority_claim = delayed_priority_queue
        .claim_next(
            "worker-delayed-priority".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("delayed priority claim should return")
        .expect("updated delayed priority job should be claimable");
    assert_eq!(delayed_priority_claim.id, delayed_priority_fast.id);
    assert_ne!(delayed_priority_claim.id, delayed_priority_slow.id);
    delayed_priority_queue
        .complete_job(
            &delayed_priority_claim.id,
            lock_token(&delayed_priority_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("delayed priority job should complete");
    trace_stage("delayed-priority:done");

    let rate_producer = RedisJobQueue::with_namespace(&redis_url, &namespace, "rate")
        .expect("valid Redis URL should build the rate producer");
    let rate_worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "rate")
        .expect("valid Redis URL should build the rate worker")
        .with_claim_rate_limit(JobRateLimit::new(1, Duration::from_millis(200)))
        .expect("rate limit should be valid");
    let rate_first = rate_producer
        .add_job(
            "rate-first".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("first rate-limited job should be added");
    let rate_second = rate_producer
        .add_job(
            "rate-second".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("second rate-limited job should be added");
    let first_rate_claim = rate_worker
        .claim_next(
            "worker-rate".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first rate claim should return")
        .expect("first rate job should be claimable");
    assert_eq!(first_rate_claim.id, rate_first.id);
    assert!(rate_worker
        .claim_next(
            "worker-rate".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("rate-limited claim should return")
        .is_none());
    rate_worker
        .complete_job(
            &first_rate_claim.id,
            lock_token(&first_rate_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("first rate job should complete");
    tokio::time::sleep(Duration::from_millis(240)).await;
    let second_rate_claim = rate_worker
        .claim_next(
            "worker-rate".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second rate claim should return")
        .expect("second rate job should be claimable after window");
    assert_eq!(second_rate_claim.id, rate_second.id);
    rate_worker
        .complete_job(
            &second_rate_claim.id,
            lock_token(&second_rate_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("second rate job should complete");
    trace_stage("rate:done");

    let global_rate_admin = RedisJobQueue::with_namespace(&redis_url, &namespace, "global-rate")
        .expect("valid Redis URL should build the global rate admin queue");
    let global_rate_worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "global-rate")
        .expect("valid Redis URL should build the global rate worker queue");
    let zero_global_rate = global_rate_admin
        .set_claim_rate_limit(JobRateLimit::new(0, Duration::from_millis(200)))
        .await
        .expect_err("zero global rate max should be rejected");
    assert!(matches!(zero_global_rate, LaneError::ConfigError(_)));
    assert_eq!(
        global_rate_worker
            .get_claim_rate_limit()
            .await
            .expect("unset global rate limit should load"),
        None
    );
    global_rate_admin
        .set_claim_rate_limit(JobRateLimit::new(1, Duration::from_millis(1_000)))
        .await
        .expect("global rate limit should be configured");
    let global_rate_meta_key = format!("{namespace}:global-rate:meta");
    let mut global_rate_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let stored_global_rate: (Option<u64>, Option<u64>) = global_rate_conn
        .hmget(&global_rate_meta_key, &["max", "duration"])
        .await?;
    assert_eq!(stored_global_rate, (Some(1), Some(1_000)));
    assert_eq!(
        global_rate_worker
            .get_claim_rate_limit()
            .await
            .expect("stored global rate limit should load"),
        Some(JobRateLimit::new(1, Duration::from_millis(1_000)))
    );
    let global_rate_first = global_rate_admin
        .add_job(
            "global-rate-first".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("first global-rate job should be added");
    let global_rate_second = global_rate_admin
        .add_job(
            "global-rate-second".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("second global-rate job should be added");
    let global_rate_first_claim = global_rate_worker
        .claim_next(
            "worker-global-rate".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first global-rate claim should return")
        .expect("first global-rate job should be claimable");
    assert_eq!(global_rate_first_claim.id, global_rate_first.id);
    let global_rate_ttl = global_rate_worker
        .get_claim_rate_limit_ttl(None)
        .await
        .expect("global rate-limit TTL should load from meta max");
    assert!(
        (1..=1_000).contains(&global_rate_ttl),
        "expected global rate-limit TTL to be within the configured window, got {global_rate_ttl}"
    );
    assert_eq!(
        global_rate_worker
            .get_claim_rate_limit_ttl(Some(2))
            .await
            .expect("non-exceeded explicit rate-limit TTL should load"),
        0
    );
    let raw_global_rate_ttl: i64 = redis::cmd("PTTL")
        .arg(format!("{namespace}:global-rate:claim_rate_limit"))
        .query_async(&mut global_rate_conn)
        .await?;
    assert!(raw_global_rate_ttl > 0);
    assert!(global_rate_worker
        .claim_next(
            "worker-global-rate".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("global rate-limited claim should return")
        .is_none());
    global_rate_admin
        .clear_claim_rate_limit()
        .await
        .expect("global rate limit should clear");
    let cleared_global_rate: (Option<u64>, Option<u64>) = global_rate_conn
        .hmget(&global_rate_meta_key, &["max", "duration"])
        .await?;
    assert_eq!(cleared_global_rate, (None, None));
    assert_eq!(
        global_rate_worker
            .get_claim_rate_limit()
            .await
            .expect("cleared global rate limit should load"),
        None
    );
    let raw_ttl_after_clear = global_rate_worker
        .get_claim_rate_limit_ttl(None)
        .await
        .expect("raw rate-limit TTL should load after clearing meta config");
    assert!(
        raw_ttl_after_clear > 0,
        "expected raw limiter key TTL to remain after clearing config, got {raw_ttl_after_clear}"
    );
    let global_rate_second_claim = global_rate_worker
        .claim_next(
            "worker-global-rate".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second global-rate claim should return after clear")
        .expect("second global-rate job should be claimable after clearing the limit");
    assert_eq!(global_rate_second_claim.id, global_rate_second.id);
    global_rate_worker
        .complete_job(
            &global_rate_first_claim.id,
            lock_token(&global_rate_first_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("first global-rate job should complete");
    global_rate_worker
        .complete_job(
            &global_rate_second_claim.id,
            lock_token(&global_rate_second_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("second global-rate job should complete");
    trace_stage("global-rate:done");

    let manual_rate_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "manual-rate")
        .expect("valid Redis URL should build the manual rate queue");
    let zero_manual_rate = manual_rate_queue
        .rate_limit_claims_for(Duration::ZERO)
        .await
        .expect_err("zero manual rate-limit duration should be rejected");
    assert!(matches!(zero_manual_rate, LaneError::ConfigError(_)));
    manual_rate_queue
        .set_claim_rate_limit(JobRateLimit::new(1, Duration::from_millis(1_000)))
        .await
        .expect("manual rate queue should configure shared max");
    let manual_rate_job = manual_rate_queue
        .add_job(
            "manual-rate-job".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("manual rate job should add");
    manual_rate_queue
        .rate_limit_claims_for(Duration::from_millis(1_000))
        .await
        .expect("manual rate limit key should be set");
    let mut manual_rate_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let manual_rate_key = format!("{namespace}:manual-rate:claim_rate_limit");
    let manual_rate_value: Option<u64> = manual_rate_conn.get(&manual_rate_key).await?;
    assert_eq!(manual_rate_value, Some(u64::MAX));
    let manual_rate_ttl = manual_rate_queue
        .get_claim_rate_limit_ttl(None)
        .await
        .expect("manual rate TTL should load");
    assert!(
        (1..=1_000).contains(&manual_rate_ttl),
        "expected manual rate TTL to be within the configured window, got {manual_rate_ttl}"
    );
    assert!(manual_rate_queue
        .claim_next(
            "worker-manual-rate".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("manual rate-limited claim should return")
        .is_none());
    manual_rate_queue
        .clear_claim_rate_limit_key()
        .await
        .expect("manual rate limiter key should clear");
    let manual_rate_pttl_after_clear: i64 = redis::cmd("PTTL")
        .arg(&manual_rate_key)
        .query_async(&mut manual_rate_conn)
        .await?;
    assert_eq!(manual_rate_pttl_after_clear, -2);
    let manual_rate_claim = manual_rate_queue
        .claim_next(
            "worker-manual-rate".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("manual rate claim should return after key clear")
        .expect("manual rate job should be claimable after clearing the limiter key");
    assert_eq!(manual_rate_claim.id, manual_rate_job.id);
    manual_rate_queue
        .complete_job(
            &manual_rate_claim.id,
            lock_token(&manual_rate_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("manual rate job should complete");
    trace_stage("manual-rate:done");

    let claim_promote_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "claim-promote")
            .expect("valid Redis URL should build the claim-promote queue");
    let claim_promoted = claim_promote_queue
        .add_job(
            "claim-promoted".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_priority(7)
                .with_delay(Duration::from_millis(500)),
        )
        .await
        .expect("claim-promoted delayed job should be added");
    assert!(claim_promote_queue
        .claim_next(
            "worker-claim-promote-early".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("early claim-promote claim should return")
        .is_none());
    tokio::time::sleep(Duration::from_millis(560)).await;
    let claim_promoted_claim = claim_promote_queue
        .claim_next(
            "worker-claim-promote".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("claim-promote claim should return")
        .expect("due delayed job should be atomically promoted and claimed");
    assert_eq!(claim_promoted_claim.id, claim_promoted.id);
    claim_promote_queue
        .complete_job(
            &claim_promoted_claim.id,
            lock_token(&claim_promoted_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("claim-promoted job should complete");
    trace_stage("claim-promote:done");

    let paused_promote_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "paused-promote")
            .expect("valid Redis URL should build the paused-promote queue");
    assert!(!paused_promote_queue
        .is_paused()
        .await
        .expect("paused-promote pause state should load before pause"));
    paused_promote_queue
        .pause()
        .await
        .expect("paused-promote queue should pause");
    assert!(paused_promote_queue
        .is_paused()
        .await
        .expect("paused-promote pause state should load after pause"));
    let mut paused_promote_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let paused_meta_key = format!("{namespace}:paused-promote:meta");
    let paused_raw: Option<u8> = paused_promote_conn.hget(&paused_meta_key, "paused").await?;
    assert_eq!(paused_raw, Some(1));
    let paused_promoted = paused_promote_queue
        .add_job(
            "paused-promoted".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_millis(500)),
        )
        .await
        .expect("paused-promoted delayed job should be added");
    tokio::time::sleep(Duration::from_millis(560)).await;
    assert!(paused_promote_queue
        .claim_next(
            "worker-paused-promote".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("paused-promote claim should return")
        .is_none());
    let waiting_while_paused = paused_promote_queue
        .list_jobs(JobListOptions::new().with_state(JobState::Waiting))
        .await
        .expect("paused-promote waiting jobs should list");
    assert!(waiting_while_paused
        .jobs
        .iter()
        .any(|job| job.id == paused_promoted.id));
    paused_promote_queue
        .resume()
        .await
        .expect("paused-promote queue should resume");
    assert!(!paused_promote_queue
        .is_paused()
        .await
        .expect("paused-promote pause state should load after resume"));
    let resumed_raw: Option<u8> = paused_promote_conn.hget(&paused_meta_key, "paused").await?;
    assert!(resumed_raw.is_none());
    let _: usize = paused_promote_conn
        .hset(&paused_meta_key, "paused", 0_u8)
        .await?;
    assert!(!paused_promote_queue
        .is_paused()
        .await
        .expect("legacy paused=0 value should load as resumed"));
    let legacy_resumed_raw: Option<u8> =
        paused_promote_conn.hget(&paused_meta_key, "paused").await?;
    assert!(legacy_resumed_raw.is_none());
    let paused_promoted_claim = paused_promote_queue
        .claim_next(
            "worker-paused-promote-resumed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("resumed paused-promote claim should return")
        .expect("paused-promoted job should be claimable after resume");
    assert_eq!(paused_promoted_claim.id, paused_promoted.id);
    paused_promote_queue
        .complete_job(
            &paused_promoted_claim.id,
            lock_token(&paused_promoted_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("paused-promoted job should complete");
    trace_stage("paused-promote:done");

    let single_promote_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "single-promote")
            .expect("valid Redis URL should build the single-promote queue");
    let single_promoted = single_promote_queue
        .add_job(
            "single-promoted".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("single-promoted delayed job should be added");
    let promoted_now = single_promote_queue
        .promote_job(&single_promoted.id, Utc::now())
        .await
        .expect("single delayed job should promote");
    assert_eq!(promoted_now.state, JobState::Waiting);
    let mut single_promote_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let single_promote_delayed_score: Option<f64> = single_promote_conn
        .zscore(
            format!("{namespace}:single-promote:delayed"),
            &single_promoted.id,
        )
        .await?;
    assert!(single_promote_delayed_score.is_none());
    let single_promote_waiting_score: Option<f64> = single_promote_conn
        .zscore(
            format!("{namespace}:single-promote:waiting"),
            &single_promoted.id,
        )
        .await?;
    assert!(single_promote_waiting_score.is_some());
    let single_promote_claim = single_promote_queue
        .claim_next(
            "worker-single-promote".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("single-promote claim should return")
        .expect("single-promoted job should be claimable");
    assert_eq!(single_promote_claim.id, single_promoted.id);
    single_promote_queue
        .complete_job(
            &single_promote_claim.id,
            lock_token(&single_promote_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("single-promoted job should complete");
    let _: usize = single_promote_conn
        .zadd(
            format!("{namespace}:single-promote:delayed"),
            &single_promoted.id,
            0.0,
        )
        .await?;
    let stale_promoted = single_promote_queue
        .promote_job(&single_promoted.id, Utc::now())
        .await
        .expect("completed job with stale delayed index should load");
    assert_eq!(stale_promoted.state, JobState::Completed);
    let stale_completed_delayed_score: Option<f64> = single_promote_conn
        .zscore(
            format!("{namespace}:single-promote:delayed"),
            &single_promoted.id,
        )
        .await?;
    assert!(stale_completed_delayed_score.is_none());
    let _: usize = single_promote_conn
        .zadd(
            format!("{namespace}:single-promote:delayed"),
            "missing-promote-job",
            0.0,
        )
        .await?;
    let missing_promote = single_promote_queue
        .promote_job("missing-promote-job", Utc::now())
        .await
        .expect_err("missing job should still be reported as missing");
    assert!(matches!(missing_promote, LaneError::JobNotFound(_)));
    let missing_delayed_score: Option<f64> = single_promote_conn
        .zscore(
            format!("{namespace}:single-promote:delayed"),
            "missing-promote-job",
        )
        .await?;
    assert!(missing_delayed_score.is_none());
    let missing_index_job = single_promote_queue
        .add_job(
            "missing-delayed-index".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("missing-index delayed job should be added");
    let _: usize = single_promote_conn
        .zrem(
            format!("{namespace}:single-promote:delayed"),
            &missing_index_job.id,
        )
        .await?;
    let missing_index_error = single_promote_queue
        .promote_job(&missing_index_job.id, Utc::now())
        .await
        .expect_err("delayed job without delayed index should reject promote");
    assert!(matches!(
        missing_index_error,
        LaneError::JobStateConflict(_)
    ));
    trace_stage("single-promote:done");

    let active_limit_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "active-limit")
        .expect("valid Redis URL should build the active limit queue");
    let zero_active_limit = active_limit_queue
        .set_max_active_jobs(0)
        .await
        .expect_err("zero active limit should be rejected");
    assert!(matches!(zero_active_limit, LaneError::ConfigError(_)));
    assert_eq!(
        active_limit_queue
            .get_max_active_jobs()
            .await
            .expect("unset active limit should load"),
        None
    );
    active_limit_queue
        .set_max_active_jobs(1)
        .await
        .expect("active limit should be configured");
    let active_limit_meta_key = format!("{namespace}:active-limit:meta");
    let mut active_limit_meta_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let stored_concurrency: Option<usize> = active_limit_meta_conn
        .hget(&active_limit_meta_key, "concurrency")
        .await?;
    assert_eq!(stored_concurrency, Some(1));
    assert_eq!(
        active_limit_queue
            .get_max_active_jobs()
            .await
            .expect("stored active limit should load"),
        Some(1)
    );
    let active_first = active_limit_queue
        .add_job(
            "active-first".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("first active-limit job should be added");
    let active_second = active_limit_queue
        .add_job(
            "active-second".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("second active-limit job should be added");
    let first_active_claim = active_limit_queue
        .claim_next(
            "worker-active-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first active-limit claim should return")
        .expect("first active-limit job should be claimable");
    assert_eq!(first_active_claim.id, active_first.id);
    assert!(active_limit_queue
        .claim_next(
            "worker-active-b".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("maxed active-limit claim should return")
        .is_none());
    active_limit_queue
        .complete_job(
            &first_active_claim.id,
            lock_token(&first_active_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("first active-limit job should complete");
    let second_active_claim = active_limit_queue
        .claim_next(
            "worker-active-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second active-limit claim should return")
        .expect("second active-limit job should be claimable after completion");
    assert_eq!(second_active_claim.id, active_second.id);
    active_limit_queue
        .complete_job(
            &second_active_claim.id,
            lock_token(&second_active_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("second active-limit job should complete");
    active_limit_queue
        .clear_max_active_jobs()
        .await
        .expect("active limit should clear");
    let cleared_concurrency: Option<usize> = active_limit_meta_conn
        .hget(&active_limit_meta_key, "concurrency")
        .await?;
    assert_eq!(cleared_concurrency, None);
    assert_eq!(
        active_limit_queue
            .get_max_active_jobs()
            .await
            .expect("cleared active limit should load"),
        None
    );
    let active_unlimited_first = active_limit_queue
        .add_job(
            "active-unlimited-first".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("first unlimited active-limit job should be added");
    let active_unlimited_second = active_limit_queue
        .add_job(
            "active-unlimited-second".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("second unlimited active-limit job should be added");
    let first_unlimited_claim = active_limit_queue
        .claim_next(
            "worker-active-unlimited-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first unlimited active-limit claim should return")
        .expect("first unlimited active-limit job should be claimable");
    assert_eq!(first_unlimited_claim.id, active_unlimited_first.id);
    let second_unlimited_claim = active_limit_queue
        .claim_next(
            "worker-active-unlimited-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second unlimited active-limit claim should return")
        .expect("second unlimited active-limit job should be claimable");
    assert_eq!(second_unlimited_claim.id, active_unlimited_second.id);
    active_limit_queue
        .complete_job(
            &first_unlimited_claim.id,
            lock_token(&first_unlimited_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("first unlimited active-limit job should complete");
    active_limit_queue
        .complete_job(
            &second_unlimited_claim.id,
            lock_token(&second_unlimited_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("second unlimited active-limit job should complete");
    trace_stage("active-limit:done");

    let manual_retry_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "manual-retry")
        .expect("valid Redis URL should build the manual retry queue");
    let manual_retry = manual_retry_queue
        .add_job(
            "manual-retry".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_priority(10),
        )
        .await
        .expect("manual retry job should be added");
    let manual_retry_claim = manual_retry_queue
        .claim_next(
            "worker-manual-retry-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("manual retry claim should return")
        .expect("manual retry job should be claimable");
    assert_eq!(manual_retry_claim.id, manual_retry.id);
    let manual_retry_failed = manual_retry_queue
        .fail_job(
            &manual_retry_claim.id,
            lock_token(&manual_retry_claim),
            "manual retry terminal failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("manual retry job should fail");
    assert_eq!(manual_retry_failed.state, JobState::Failed);
    let retried_manual = manual_retry_queue
        .retry_job(&manual_retry.id, Utc::now())
        .await
        .expect("manual retry job should move back to waiting");
    assert_eq!(retried_manual.state, JobState::Waiting);
    assert!(retried_manual.failed_reason.is_none());
    let mut manual_retry_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let manual_retry_failed_score: Option<f64> = manual_retry_conn
        .zscore(format!("{namespace}:manual-retry:failed"), &manual_retry.id)
        .await?;
    assert!(manual_retry_failed_score.is_none());
    let manual_retry_waiting_score: Option<f64> = manual_retry_conn
        .zscore(
            format!("{namespace}:manual-retry:waiting"),
            &manual_retry.id,
        )
        .await?;
    assert!(manual_retry_waiting_score.is_some());
    let manual_retry_reclaimed = manual_retry_queue
        .claim_next(
            "worker-manual-retry-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("manual retried claim should return")
        .expect("manual retried job should be claimable");
    assert_eq!(manual_retry_reclaimed.id, manual_retry.id);
    manual_retry_queue
        .complete_job(
            &manual_retry_reclaimed.id,
            lock_token(&manual_retry_reclaimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("manual retried job should complete");
    let _: usize = manual_retry_conn
        .zadd(
            format!("{namespace}:manual-retry:failed"),
            &manual_retry.id,
            0.0,
        )
        .await?;
    let stale_failed_retry = manual_retry_queue
        .retry_job(&manual_retry.id, Utc::now())
        .await
        .expect_err("completed job with stale failed index should reject retry");
    assert!(matches!(stale_failed_retry, LaneError::JobStateConflict(_)));
    let stale_failed_score: Option<f64> = manual_retry_conn
        .zscore(format!("{namespace}:manual-retry:failed"), &manual_retry.id)
        .await?;
    assert!(stale_failed_score.is_none());
    let _: usize = manual_retry_conn
        .zadd(
            format!("{namespace}:manual-retry:failed"),
            "missing-retry-job",
            0.0,
        )
        .await?;
    let missing_retry = manual_retry_queue
        .retry_job("missing-retry-job", Utc::now())
        .await
        .expect_err("missing job should still be reported as missing");
    assert!(matches!(missing_retry, LaneError::JobNotFound(_)));
    let missing_retry_failed_score: Option<f64> = manual_retry_conn
        .zscore(
            format!("{namespace}:manual-retry:failed"),
            "missing-retry-job",
        )
        .await?;
    assert!(missing_retry_failed_score.is_none());
    let missing_failed_index = manual_retry_queue
        .add_job(
            "missing-failed-index".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("missing failed index job should add");
    let missing_failed_index_claim = manual_retry_queue
        .claim_next(
            "worker-manual-retry-missing-index".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("missing failed index claim should return")
        .expect("missing failed index job should claim");
    assert_eq!(missing_failed_index_claim.id, missing_failed_index.id);
    manual_retry_queue
        .fail_job(
            &missing_failed_index_claim.id,
            lock_token(&missing_failed_index_claim),
            "missing failed index terminal failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("missing failed index job should fail");
    let _: usize = manual_retry_conn
        .zrem(
            format!("{namespace}:manual-retry:failed"),
            &missing_failed_index.id,
        )
        .await?;
    let missing_failed_index_error = manual_retry_queue
        .retry_job(&missing_failed_index.id, Utc::now())
        .await
        .expect_err("failed job without failed index should reject retry");
    assert!(matches!(
        missing_failed_index_error,
        LaneError::JobStateConflict(_)
    ));
    let missing_failed_index_after = manual_retry_queue
        .get_job(&missing_failed_index.id)
        .await
        .expect("missing failed index job should load")
        .expect("missing failed index job should still exist");
    assert_eq!(missing_failed_index_after.state, JobState::Failed);
    let missing_failed_index_waiting_score: Option<f64> = manual_retry_conn
        .zscore(
            format!("{namespace}:manual-retry:waiting"),
            &missing_failed_index.id,
        )
        .await?;
    assert!(missing_failed_index_waiting_score.is_none());
    trace_stage("manual-retry:done");

    let state_query_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "state-query")
        .expect("valid Redis URL should build the state-query queue");
    let state_waiting = state_query_queue
        .add_job(
            "state-waiting".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("state waiting job should add");
    let state_delayed = state_query_queue
        .add_job(
            "state-delayed".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(30)),
        )
        .await
        .expect("state delayed job should add");
    let state_flow = state_query_queue
        .add_flow(
            JobSpec::new("state-parent", serde_json::json!({})),
            vec![JobSpec::new("state-child", serde_json::json!({}))],
        )
        .await
        .expect("state flow should add");
    assert_eq!(
        state_query_queue
            .get_job_state(&state_waiting.id)
            .await
            .expect("waiting state should load"),
        Some(JobState::Waiting)
    );
    assert_eq!(
        state_query_queue
            .get_job_state(&state_delayed.id)
            .await
            .expect("delayed state should load"),
        Some(JobState::Delayed)
    );
    assert_eq!(
        state_query_queue
            .get_job_state(&state_flow.parent.id)
            .await
            .expect("waiting-children state should load"),
        Some(JobState::WaitingChildren)
    );
    let state_claim = state_query_queue
        .claim_next(
            "worker-state-query".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("state query claim should return")
        .expect("state query job should be claimable");
    assert_eq!(
        state_query_queue
            .get_job_state(&state_claim.id)
            .await
            .expect("active state should load"),
        Some(JobState::Active)
    );
    state_query_queue
        .complete_job(
            &state_claim.id,
            lock_token(&state_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("state query job should complete");
    assert_eq!(
        state_query_queue
            .get_job_state(&state_claim.id)
            .await
            .expect("completed state should load"),
        Some(JobState::Completed)
    );
    assert_eq!(
        state_query_queue
            .get_job_state("missing-state-job")
            .await
            .expect("missing state should load"),
        None
    );
    let state_index_missing = state_query_queue
        .add_job(
            "state-index-missing".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("state index-missing job should add");
    let state_index_conflict = state_query_queue
        .add_job(
            "state-index-conflict".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("state index-conflict job should add");
    let mut state_query_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let removed_waiting_index: usize = state_query_conn
        .zrem(
            format!("{namespace}:state-query:waiting"),
            &state_index_missing.id,
        )
        .await?;
    assert_eq!(removed_waiting_index, 1);
    assert_eq!(
        state_query_queue
            .get_job(&state_index_missing.id)
            .await
            .expect("state index-missing job should load")
            .expect("state index-missing job should exist")
            .state,
        JobState::Waiting
    );
    assert_eq!(
        state_query_queue
            .get_job_state(&state_index_missing.id)
            .await
            .expect("missing index state should load"),
        None
    );
    let _: usize = state_query_conn
        .zadd(
            format!("{namespace}:state-query:completed"),
            &state_index_conflict.id,
            0.0,
        )
        .await?;
    assert_eq!(
        state_query_queue
            .get_job_state(&state_index_conflict.id)
            .await
            .expect("conflicting index state should load"),
        Some(JobState::Completed)
    );
    trace_stage("state-query:done");

    let list_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "list-ranges")
        .expect("valid Redis URL should build the list-ranges queue");
    let list_slow = list_queue
        .add_job(
            "list-slow".to_string(),
            serde_json::json!({ "n": 1 }),
            JobOptions::new().with_priority(20),
        )
        .await
        .expect("slow list job should add");
    let list_fast = list_queue
        .add_job(
            "list-fast".to_string(),
            serde_json::json!({ "n": 2 }),
            JobOptions::new().with_priority(5),
        )
        .await
        .expect("fast list job should add");
    let list_delayed = list_queue
        .add_job(
            "list-delayed".to_string(),
            serde_json::json!({ "n": 3 }),
            JobOptions::new().with_delay(Duration::from_secs(30)),
        )
        .await
        .expect("delayed list job should add");
    let list_ascending = list_queue
        .list_jobs(
            JobListOptions::new()
                .with_states([JobState::Waiting, JobState::Delayed, JobState::Waiting])
                .with_limit(3),
        )
        .await
        .expect("multi-state ascending list should load");
    assert_eq!(list_ascending.total, 3);
    assert_eq!(
        list_ascending
            .jobs
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            list_fast.id.as_str(),
            list_slow.id.as_str(),
            list_delayed.id.as_str()
        ]
    );
    let list_descending = list_queue
        .list_jobs(
            JobListOptions::new()
                .with_states([JobState::Waiting, JobState::Delayed])
                .descending()
                .with_offset(1)
                .with_limit(2),
        )
        .await
        .expect("multi-state descending list should load");
    assert_eq!(list_descending.total, 3);
    assert_eq!(
        list_descending
            .jobs
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>(),
        vec![list_slow.id.as_str(), list_fast.id.as_str()]
    );
    trace_stage("list-ranges:done");

    producer.pause().await.expect("pause should succeed");
    let high = producer
        .add_job(
            "high".to_string(),
            serde_json::json!({ "n": 1 }),
            JobOptions::new().with_priority(5),
        )
        .await
        .expect("high priority job should be added");
    let low = producer
        .add_job(
            "low".to_string(),
            serde_json::json!({ "n": 2 }),
            JobOptions::new()
                .with_priority(50)
                .with_retry_policy(RetryPolicy::fixed(1, Duration::from_millis(5))),
        )
        .await
        .expect("low priority job should be added");

    assert!(worker
        .claim_next(
            "worker-paused".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("paused claim should return")
        .is_none());
    producer.resume().await.expect("resume should succeed");

    let first = worker
        .claim_next("worker-a".to_string(), Duration::from_secs(30), Utc::now())
        .await
        .expect("first claim should return")
        .expect("first job should be claimable");
    assert_eq!(first.id, high.id);
    assert_eq!(first.state, JobState::Active);
    assert_eq!(first.worker_id.as_deref(), Some("worker-a"));
    assert!(first.lock_token.is_some());
    let wrong_token_complete = worker
        .complete_job(
            &first.id,
            "wrong-token",
            serde_json::json!({ "ok": false }),
            Utc::now(),
        )
        .await
        .expect_err("wrong token must not complete an active job");
    assert!(matches!(
        wrong_token_complete,
        LaneError::JobLeaseConflict(_)
    ));

    worker
        .update_progress(&first.id, serde_json::json!({ "percent": 50 }))
        .await
        .expect("progress update should succeed");
    let updated_data = worker
        .update_data(
            &first.id,
            serde_json::json!({ "n": 1, "stage": "normalized" }),
        )
        .await
        .expect("data update should succeed");
    assert_eq!(
        updated_data.payload,
        serde_json::json!({ "n": 1, "stage": "normalized" })
    );
    worker
        .add_log(&first.id, "accepted".to_string(), 10, Utc::now())
        .await
        .expect("log update should succeed");
    worker
        .add_log(&first.id, "provider accepted".to_string(), 2, Utc::now())
        .await
        .expect("second log update should succeed");
    worker
        .add_log(&first.id, "provider delivered".to_string(), 2, Utc::now())
        .await
        .expect("third log update should trim retained logs");
    let completed = worker
        .complete_job(
            &first.id,
            lock_token(&first),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("complete should succeed");
    assert_eq!(completed.state, JobState::Completed);
    let terminal_progress = worker
        .update_progress(&first.id, serde_json::json!({ "percent": 100 }))
        .await
        .expect_err("terminal completed jobs must reject progress updates");
    assert!(matches!(terminal_progress, LaneError::JobStateConflict(_)));
    let terminal_data = worker
        .update_data(
            &first.id,
            serde_json::json!({ "n": 1, "stage": "archived" }),
        )
        .await
        .expect("terminal retained jobs should allow data updates");
    assert_eq!(
        terminal_data.payload,
        serde_json::json!({ "n": 1, "stage": "archived" })
    );

    let second = worker
        .claim_next("worker-b".to_string(), Duration::from_secs(30), Utc::now())
        .await
        .expect("second claim should return")
        .expect("second job should be claimable");
    assert_eq!(second.id, low.id);

    let retry = worker
        .fail_job(
            &second.id,
            lock_token(&second),
            "temporary".to_string(),
            Utc::now(),
        )
        .await
        .expect("retryable failure should succeed");
    assert_eq!(retry.state, JobState::Delayed);

    tokio::time::sleep(Duration::from_millis(10)).await;
    producer
        .promote_due_jobs(Utc::now())
        .await
        .expect("due retry should promote");
    let retried = worker
        .claim_next("worker-c".to_string(), Duration::from_secs(30), Utc::now())
        .await
        .expect("retry claim should return")
        .expect("retry should be claimable");
    assert_eq!(retried.id, low.id);
    let failed = worker
        .fail_job(
            &retried.id,
            lock_token(&retried),
            "terminal".to_string(),
            Utc::now(),
        )
        .await
        .expect("terminal failure should succeed");
    assert_eq!(failed.state, JobState::Failed);

    let delayed = producer
        .add_job(
            "delayed".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_priority(1)
                .with_delay(Duration::from_secs(1)),
        )
        .await
        .expect("delayed job should be added");
    assert_eq!(delayed.state, JobState::Delayed);
    let rescheduled_delayed = producer
        .reschedule_job(&delayed.id, Duration::from_millis(200), Utc::now())
        .await
        .expect("delayed job should reschedule");
    assert_eq!(rescheduled_delayed.id, delayed.id);
    assert_eq!(rescheduled_delayed.state, JobState::Delayed);
    assert_eq!(
        rescheduled_delayed.options.delay,
        Some(Duration::from_millis(200))
    );
    assert!(worker
        .claim_next("worker-d".to_string(), Duration::from_secs(30), Utc::now())
        .await
        .expect("early delayed claim should return")
        .is_none());

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        producer
            .promote_due_jobs(Utc::now())
            .await
            .expect("delayed job should promote"),
        1
    );
    let claimed_delayed = worker
        .claim_next("worker-d".to_string(), Duration::from_secs(30), Utc::now())
        .await
        .expect("delayed claim should return")
        .expect("delayed job should be claimable");
    assert_eq!(claimed_delayed.id, delayed.id);
    let active_remove = producer
        .remove_job(&claimed_delayed.id)
        .await
        .expect_err("active leased jobs must not be removed");
    assert!(matches!(active_remove, LaneError::JobLeaseConflict(_)));
    let mut remove_index_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let active_after_failed_remove: Option<f64> = remove_index_conn
        .zscore(format!("{namespace}:jobs:active"), &claimed_delayed.id)
        .await?;
    assert!(active_after_failed_remove.is_some());
    assert_eq!(
        producer
            .get_job(&claimed_delayed.id)
            .await
            .expect("active job should load")
            .expect("active job should still exist")
            .state,
        JobState::Active
    );
    let wrong_delay_token = producer
        .delay_active_job(
            &claimed_delayed.id,
            "wrong-token",
            Duration::from_millis(200),
            Utc::now(),
        )
        .await
        .expect_err("wrong token must not delay an active job");
    assert!(matches!(wrong_delay_token, LaneError::JobLeaseConflict(_)));
    let delayed_again = producer
        .delay_active_job(
            &claimed_delayed.id,
            lock_token(&claimed_delayed),
            Duration::from_millis(750),
            Utc::now(),
        )
        .await
        .expect("active job should move back to delayed");
    assert_eq!(delayed_again.state, JobState::Delayed);
    assert_eq!(
        delayed_again.options.delay,
        Some(Duration::from_millis(750))
    );
    assert!(delayed_again.worker_id.is_none());
    assert!(delayed_again.lease_expires_at.is_none());
    let active_after_delay: Option<f64> = remove_index_conn
        .zscore(format!("{namespace}:jobs:active"), &claimed_delayed.id)
        .await?;
    assert!(active_after_delay.is_none());
    let delayed_after_delay: Option<f64> = remove_index_conn
        .zscore(format!("{namespace}:jobs:delayed"), &claimed_delayed.id)
        .await?;
    assert!(delayed_after_delay.is_some());
    let lock_after_delay_exists: usize = remove_index_conn
        .exists(format!("{namespace}:jobs:locks:{}", claimed_delayed.id))
        .await?;
    assert_eq!(lock_after_delay_exists, 0);
    let complete_after_delay = producer
        .complete_job(
            &claimed_delayed.id,
            lock_token(&claimed_delayed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect_err("delayed job must not complete with the old active token");
    assert!(matches!(
        complete_after_delay,
        LaneError::JobStateConflict(_)
    ));
    assert!(worker
        .claim_next(
            "worker-delayed-again-early".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("early delayed-again claim should return")
        .is_none());
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        producer
            .promote_due_jobs(Utc::now())
            .await
            .expect("delayed-again job should promote"),
        1
    );
    let reclaimed_delayed = worker
        .claim_next(
            "worker-delayed-again".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("delayed-again claim should return")
        .expect("delayed-again job should be claimable");
    assert_eq!(reclaimed_delayed.id, claimed_delayed.id);
    worker
        .complete_job(
            &reclaimed_delayed.id,
            lock_token(&reclaimed_delayed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("delayed-again job should complete");

    let release_active = producer
        .add_job(
            "release-active".to_string(),
            serde_json::json!({ "kind": "yield" }),
            JobOptions::new().with_priority(3),
        )
        .await
        .expect("release-active job should be added");
    let claimed_release = worker
        .claim_next(
            "worker-release-active".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("release-active claim should return")
        .expect("release-active job should be claimable");
    assert_eq!(claimed_release.id, release_active.id);
    let wrong_release_token = producer
        .release_active_job(&claimed_release.id, "wrong-token", Utc::now())
        .await
        .expect_err("wrong token must not release an active job");
    assert!(matches!(
        wrong_release_token,
        LaneError::JobLeaseConflict(_)
    ));
    let released_active = producer
        .release_active_job(
            &claimed_release.id,
            lock_token(&claimed_release),
            Utc::now(),
        )
        .await
        .expect("active job should release back to waiting");
    assert_eq!(released_active.state, JobState::Waiting);
    assert_eq!(released_active.attempts_made, claimed_release.attempts_made);
    assert!(released_active.worker_id.is_none());
    assert!(released_active.lock_token.is_none());
    assert!(released_active.lease_expires_at.is_none());
    let release_active_score: Option<f64> = remove_index_conn
        .zscore(format!("{namespace}:jobs:active"), &claimed_release.id)
        .await?;
    assert!(release_active_score.is_none());
    let release_waiting_score: Option<f64> = remove_index_conn
        .zscore(format!("{namespace}:jobs:waiting"), &claimed_release.id)
        .await?;
    assert!(release_waiting_score.is_some());
    let release_lock_exists: usize = remove_index_conn
        .exists(format!("{namespace}:jobs:locks:{}", claimed_release.id))
        .await?;
    assert_eq!(release_lock_exists, 0);
    let complete_after_release = producer
        .complete_job(
            &claimed_release.id,
            lock_token(&claimed_release),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect_err("waiting job must not complete with the old active token");
    assert!(matches!(
        complete_after_release,
        LaneError::JobStateConflict(_)
    ));
    let reclaimed_release = worker
        .claim_next(
            "worker-release-active-again".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("released job claim should return")
        .expect("released job should be claimable again");
    assert_eq!(reclaimed_release.id, claimed_release.id);
    assert_eq!(
        reclaimed_release.attempts_made,
        claimed_release.attempts_made + 1
    );
    worker
        .complete_job(
            &reclaimed_release.id,
            lock_token(&reclaimed_release),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("released job should complete after reclaim");

    let stale_active_delay = producer
        .add_job(
            "stale-active-delay".to_string(),
            serde_json::json!({ "kind": "stale-active-index" }),
            JobOptions::new(),
        )
        .await
        .expect("stale active delay job should be added");
    let stale_active_claim = worker
        .claim_next(
            "worker-stale-active-delay".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("stale active delay claim should return")
        .expect("stale active delay job should be claimable");
    assert_eq!(stale_active_claim.id, stale_active_delay.id);
    let stale_removed_from_active: usize = remove_index_conn
        .zrem(format!("{namespace}:jobs:active"), &stale_active_claim.id)
        .await?;
    assert_eq!(stale_removed_from_active, 1);
    let stale_active_delay_error = producer
        .delay_active_job(
            &stale_active_claim.id,
            lock_token(&stale_active_claim),
            Duration::from_millis(200),
            Utc::now(),
        )
        .await
        .expect_err("missing active zset membership should reject active delay");
    assert!(matches!(
        stale_active_delay_error,
        LaneError::JobStateConflict(_)
    ));
    let stale_active_lock_exists: usize = remove_index_conn
        .exists(format!("{namespace}:jobs:locks:{}", stale_active_claim.id))
        .await?;
    assert_eq!(stale_active_lock_exists, 1);
    worker
        .complete_job(
            &stale_active_claim.id,
            lock_token(&stale_active_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("stale active delay job should still complete with valid lock");

    let stale_reschedule = producer
        .add_job(
            "stale-reschedule".to_string(),
            serde_json::json!({ "kind": "stale-delayed-index" }),
            JobOptions::new().with_delay(Duration::from_secs(30)),
        )
        .await
        .expect("stale reschedule job should be added");
    let stale_removed_from_delayed: usize = remove_index_conn
        .zrem(format!("{namespace}:jobs:delayed"), &stale_reschedule.id)
        .await?;
    assert_eq!(stale_removed_from_delayed, 1);
    let stale_reschedule_error = producer
        .reschedule_job(&stale_reschedule.id, Duration::from_millis(200), Utc::now())
        .await
        .expect_err("missing delayed zset membership should reject reschedule");
    assert!(matches!(
        stale_reschedule_error,
        LaneError::JobStateConflict(_)
    ));

    let removable = producer
        .add_job(
            "removable".to_string(),
            serde_json::json!({ "kind": "cleanup" }),
            JobOptions::new().with_priority(25),
        )
        .await
        .expect("removable job should be added");
    producer
        .add_log(
            &removable.id,
            "queued for removal".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("removable job log should append");
    let removable_logs_key = format!("{namespace}:jobs:logs:{}", removable.id);
    let removable_logs_len: usize = remove_index_conn.llen(&removable_logs_key).await?;
    assert_eq!(removable_logs_len, 1);
    let waiting_reschedule_error = producer
        .reschedule_job(&removable.id, Duration::from_millis(10), Utc::now())
        .await
        .expect_err("waiting jobs should reject reschedule");
    assert!(matches!(
        waiting_reschedule_error,
        LaneError::JobStateConflict(_)
    ));
    let removed = producer
        .remove_job(&removable.id)
        .await
        .expect("removable job should remove")
        .expect("removable job should be returned");
    assert_eq!(removed.id, removable.id);
    assert!(producer
        .get_job(&removable.id)
        .await
        .expect("removed job lookup should return")
        .is_none());
    let removed_waiting_score: Option<f64> = remove_index_conn
        .zscore(format!("{namespace}:jobs:waiting"), &removable.id)
        .await?;
    assert!(removed_waiting_score.is_none());
    let removed_hash: Option<String> = remove_index_conn
        .hget(format!("{namespace}:jobs:jobs"), &removable.id)
        .await?;
    assert!(removed_hash.is_none());
    let removed_logs_len: usize = remove_index_conn.llen(&removable_logs_key).await?;
    assert_eq!(removed_logs_len, 0);
    let removed_logs = producer
        .get_job_logs(&removable.id, 0, -1, true)
        .await
        .expect("removed job logs should return an empty page");
    assert_eq!(removed_logs.count, 0);
    assert!(removed_logs.logs.is_empty());
    let missing_job_id = "missing-job";
    for state in [
        "waiting",
        "delayed",
        "active",
        "waiting_children",
        "completed",
        "failed",
    ] {
        let _: usize = remove_index_conn
            .zadd(format!("{namespace}:jobs:{state}"), missing_job_id, 0.0)
            .await?;
    }
    let _: () = remove_index_conn
        .set(
            format!("{namespace}:jobs:locks:{missing_job_id}"),
            "stale-lock",
        )
        .await?;
    let _: usize = remove_index_conn
        .sadd(
            format!("{namespace}:jobs:dependencies:{missing_job_id}"),
            "stale-child",
        )
        .await?;
    let missing_logs_key = format!("{namespace}:jobs:logs:{missing_job_id}");
    let _: usize = remove_index_conn
        .rpush(&missing_logs_key, "{\"line\":\"stale\"}")
        .await?;
    assert!(producer
        .remove_job(missing_job_id)
        .await
        .expect("missing job remove should return")
        .is_none());
    for state in [
        "waiting",
        "delayed",
        "active",
        "waiting_children",
        "completed",
        "failed",
    ] {
        let orphan_score: Option<f64> = remove_index_conn
            .zscore(format!("{namespace}:jobs:{state}"), missing_job_id)
            .await?;
        assert!(
            orphan_score.is_none(),
            "orphaned {state} index should be pruned for missing remove"
        );
    }
    let missing_lock_exists: usize = remove_index_conn
        .exists(format!("{namespace}:jobs:locks:{missing_job_id}"))
        .await?;
    assert_eq!(missing_lock_exists, 0);
    let missing_dependencies_exist: usize = remove_index_conn
        .exists(format!("{namespace}:jobs:dependencies:{missing_job_id}"))
        .await?;
    assert_eq!(missing_dependencies_exist, 0);
    let missing_logs_len: usize = remove_index_conn.llen(&missing_logs_key).await?;
    assert_eq!(missing_logs_len, 0);

    let auto_remove_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "auto-remove")
        .expect("valid Redis URL should build the auto-remove queue");
    let mut auto_remove_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let remove_on_complete = auto_remove_queue
        .add_job(
            "remove-on-complete".to_string(),
            serde_json::json!({}),
            JobOptions::new().remove_on_complete(true),
        )
        .await
        .expect("remove-on-complete job should add");
    auto_remove_queue
        .add_log(
            &remove_on_complete.id,
            "complete cleanup log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("remove-on-complete log should append");
    let remove_on_complete_claim = auto_remove_queue
        .claim_next(
            "worker-auto-complete".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("remove-on-complete claim should return")
        .expect("remove-on-complete job should be claimable");
    assert_eq!(remove_on_complete_claim.id, remove_on_complete.id);
    let remove_on_complete_snapshot = auto_remove_queue
        .complete_job(
            &remove_on_complete_claim.id,
            lock_token(&remove_on_complete_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("remove-on-complete job should complete");
    assert_eq!(remove_on_complete_snapshot.state, JobState::Completed);
    assert!(auto_remove_queue
        .get_job(&remove_on_complete.id)
        .await
        .expect("remove-on-complete lookup should return")
        .is_none());
    let remove_on_complete_logs_len: usize = auto_remove_conn
        .llen(format!(
            "{namespace}:auto-remove:logs:{}",
            remove_on_complete.id
        ))
        .await?;
    assert_eq!(remove_on_complete_logs_len, 0);

    let remove_on_fail = auto_remove_queue
        .add_job(
            "remove-on-fail".to_string(),
            serde_json::json!({}),
            JobOptions::new().remove_on_fail(true),
        )
        .await
        .expect("remove-on-fail job should add");
    auto_remove_queue
        .add_log(
            &remove_on_fail.id,
            "fail cleanup log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("remove-on-fail log should append");
    let remove_on_fail_claim = auto_remove_queue
        .claim_next(
            "worker-auto-fail".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("remove-on-fail claim should return")
        .expect("remove-on-fail job should be claimable");
    assert_eq!(remove_on_fail_claim.id, remove_on_fail.id);
    let remove_on_fail_snapshot = auto_remove_queue
        .fail_job(
            &remove_on_fail_claim.id,
            lock_token(&remove_on_fail_claim),
            "terminal failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("remove-on-fail job should fail");
    assert_eq!(remove_on_fail_snapshot.state, JobState::Failed);
    assert!(auto_remove_queue
        .get_job(&remove_on_fail.id)
        .await
        .expect("remove-on-fail lookup should return")
        .is_none());
    let remove_on_fail_logs_len: usize = auto_remove_conn
        .llen(format!(
            "{namespace}:auto-remove:logs:{}",
            remove_on_fail.id
        ))
        .await?;
    assert_eq!(remove_on_fail_logs_len, 0);

    let remove_on_stalled_fail = auto_remove_queue
        .add_job(
            "remove-on-stalled-fail".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .remove_on_fail(true)
                .with_max_stalled_count(0),
        )
        .await
        .expect("remove-on-stalled-fail job should add");
    auto_remove_queue
        .add_log(
            &remove_on_stalled_fail.id,
            "stalled cleanup log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("remove-on-stalled-fail log should append");
    let remove_on_stalled_claim = auto_remove_queue
        .claim_next(
            "worker-auto-stalled".to_string(),
            Duration::from_millis(50),
            Utc::now(),
        )
        .await
        .expect("remove-on-stalled-fail claim should return")
        .expect("remove-on-stalled-fail job should be claimable");
    assert_eq!(remove_on_stalled_claim.id, remove_on_stalled_fail.id);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        auto_remove_queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("remove-on-stalled-fail recovery should run"),
        0
    );
    assert_eq!(
        auto_remove_queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("remove-on-stalled-fail recovery should confirm"),
        1
    );
    assert!(auto_remove_queue
        .get_job(&remove_on_stalled_fail.id)
        .await
        .expect("remove-on-stalled-fail lookup should return")
        .is_none());
    let remove_on_stalled_logs_len: usize = auto_remove_conn
        .llen(format!(
            "{namespace}:auto-remove:logs:{}",
            remove_on_stalled_fail.id
        ))
        .await?;
    assert_eq!(remove_on_stalled_logs_len, 0);

    let locked_stalled = producer
        .add_job(
            "locked-stalled".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("locked-stalled job should be added");
    let locked_stalled_claim = worker
        .claim_next(
            "worker-locked-stalled".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("locked-stalled claim should return")
        .expect("locked-stalled job should be claimable");
    assert_eq!(locked_stalled_claim.id, locked_stalled.id);
    let mut stalled_index_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let _: usize = stalled_index_conn
        .zadd(format!("{namespace}:jobs:active"), &locked_stalled.id, 0.0)
        .await?;
    assert_eq!(
        producer
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("locked stalled recovery should run"),
        0
    );
    assert_eq!(
        producer
            .get_job(&locked_stalled.id)
            .await
            .expect("locked-stalled job should load")
            .expect("locked-stalled job should still exist")
            .state,
        JobState::Active
    );
    worker
        .complete_job(
            &locked_stalled_claim.id,
            lock_token(&locked_stalled_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("locked-stalled job should complete with valid token");
    let _: usize = stalled_index_conn
        .zadd(format!("{namespace}:jobs:active"), &locked_stalled.id, 0.0)
        .await?;
    assert_eq!(
        producer
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stale active index recovery should run"),
        0
    );
    assert_eq!(
        producer
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stale active index recovery should confirm"),
        0
    );
    let stale_completed_active_score: Option<f64> = stalled_index_conn
        .zscore(format!("{namespace}:jobs:active"), &locked_stalled.id)
        .await?;
    assert!(stale_completed_active_score.is_none());
    let stale_completed_after_recovery = producer
        .get_job(&locked_stalled.id)
        .await
        .expect("stale completed job should load")
        .expect("stale completed job should still exist");
    assert_eq!(stale_completed_after_recovery.state, JobState::Completed);

    let stalled = producer
        .add_job(
            "stalled".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_max_stalled_count(2),
        )
        .await
        .expect("stalled job should be added");
    let stale_claim = worker
        .claim_next(
            "worker-stale".to_string(),
            Duration::from_millis(50),
            Utc::now(),
        )
        .await
        .expect("stale claim should return")
        .expect("stalled job should be claimable");
    assert_eq!(stale_claim.id, stalled.id);
    let stale_token = lock_token(&stale_claim).to_string();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        producer
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled recovery should run"),
        0
    );
    assert_eq!(
        producer
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled recovery should confirm"),
        1
    );
    let reclaimed = worker
        .claim_next(
            "worker-reclaim".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("reclaim should return")
        .expect("recovered job should be claimable");
    assert_eq!(reclaimed.id, stalled.id);
    let stale_complete = worker
        .complete_job(
            &reclaimed.id,
            &stale_token,
            serde_json::json!({ "ok": false }),
            Utc::now(),
        )
        .await
        .expect_err("stale token must not complete a reclaimed job");
    assert!(matches!(stale_complete, LaneError::JobLeaseConflict(_)));
    worker
        .complete_job(
            &reclaimed.id,
            lock_token(&reclaimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("valid reclaimed token should complete");

    let terminal_stalled = producer
        .add_job(
            "terminal-stalled".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_max_stalled_count(0),
        )
        .await
        .expect("terminal-stalled job should be added");
    let terminal_stalled_claim = worker
        .claim_next(
            "worker-terminal-stalled".to_string(),
            Duration::from_millis(50),
            Utc::now(),
        )
        .await
        .expect("terminal-stalled claim should return")
        .expect("terminal-stalled job should be claimable");
    assert_eq!(terminal_stalled_claim.id, terminal_stalled.id);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        producer
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("terminal stalled recovery should run"),
        0
    );
    assert_eq!(
        producer
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("terminal stalled recovery should confirm"),
        1
    );
    let terminal_failed = producer
        .get_job(&terminal_stalled.id)
        .await
        .expect("terminal-stalled job should load")
        .expect("terminal-stalled job should still exist");
    assert_eq!(terminal_failed.state, JobState::Failed);
    assert_eq!(terminal_failed.stalled_count, 1);
    let terminal_failed_score: Option<f64> = stalled_index_conn
        .zscore(format!("{namespace}:jobs:failed"), &terminal_stalled.id)
        .await?;
    assert!(terminal_failed_score.is_some());
    let terminal_active_score: Option<f64> = stalled_index_conn
        .zscore(format!("{namespace}:jobs:active"), &terminal_stalled.id)
        .await?;
    assert!(terminal_active_score.is_none());

    let stored_high = producer
        .get_job(&high.id)
        .await
        .expect("stored high job should load")
        .expect("stored high job should exist");
    assert_eq!(
        stored_high.payload,
        serde_json::json!({ "n": 1, "stage": "archived" })
    );
    assert_eq!(
        stored_high.progress,
        Some(serde_json::json!({ "percent": 50 }))
    );
    assert_eq!(stored_high.logs.len(), 2);
    assert_eq!(stored_high.logs[0].line, "provider accepted");
    assert_eq!(stored_high.logs[1].line, "provider delivered");
    let high_logs = producer
        .get_job_logs(&high.id, 0, -1, true)
        .await
        .expect("stored high logs should list");
    assert_eq!(high_logs.count, 2);
    assert_eq!(
        high_logs
            .logs
            .iter()
            .map(|entry| entry.line.as_str())
            .collect::<Vec<_>>(),
        vec!["provider accepted", "provider delivered"]
    );
    let newest_high_log = producer
        .get_job_logs(&high.id, 0, 0, false)
        .await
        .expect("stored high logs should list newest first");
    assert_eq!(newest_high_log.count, 2);
    assert_eq!(newest_high_log.logs[0].line, "provider delivered");
    let high_logs_key = format!("{namespace}:jobs:logs:{}", high.id);
    let mut logs_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let raw_high: String = logs_conn
        .hget(format!("{namespace}:jobs:jobs"), &high.id)
        .await?;
    let decoded_high: Job = serde_json::from_str(&raw_high).expect("raw high job should decode");
    assert_eq!(
        decoded_high.payload,
        serde_json::json!({ "n": 1, "stage": "archived" })
    );
    let high_logs_len: usize = logs_conn.llen(&high_logs_key).await?;
    assert_eq!(high_logs_len, 2);
    let high_raw_logs: Vec<String> = logs_conn.lrange(&high_logs_key, 0, -1).await?;
    let high_decoded_logs = high_raw_logs
        .iter()
        .map(|raw| serde_json::from_str::<JobLogEntry>(raw).expect("Redis log JSON should decode"))
        .collect::<Vec<_>>();
    assert_eq!(high_decoded_logs[0].line, "provider accepted");
    assert_eq!(high_decoded_logs[1].line, "provider delivered");
    let kept_high_logs = producer
        .clear_job_logs(&high.id, 1)
        .await
        .expect("stored high logs should trim");
    assert_eq!(kept_high_logs.count, 1);
    assert_eq!(kept_high_logs.logs[0].line, "provider delivered");
    let high_logs_len_after_keep: usize = logs_conn.llen(&high_logs_key).await?;
    assert_eq!(high_logs_len_after_keep, 1);
    let raw_high_after_keep: String = logs_conn
        .hget(format!("{namespace}:jobs:jobs"), &high.id)
        .await?;
    let decoded_high_after_keep: Job =
        serde_json::from_str(&raw_high_after_keep).expect("trimmed high job should decode");
    assert_eq!(decoded_high_after_keep.logs.len(), 1);
    assert_eq!(decoded_high_after_keep.logs[0].line, "provider delivered");
    let cleared_high_logs = producer
        .clear_job_logs(&high.id, 0)
        .await
        .expect("stored high logs should clear");
    assert_eq!(cleared_high_logs.count, 0);
    assert!(cleared_high_logs.logs.is_empty());
    let high_logs_len_after_clear: usize = logs_conn.llen(&high_logs_key).await?;
    assert_eq!(high_logs_len_after_clear, 0);
    let raw_high_after_clear: String = logs_conn
        .hget(format!("{namespace}:jobs:jobs"), &high.id)
        .await?;
    let decoded_high_after_clear: Job =
        serde_json::from_str(&raw_high_after_clear).expect("cleared high job should decode");
    assert!(decoded_high_after_clear.logs.is_empty());

    let stats = producer.stats().await.expect("stats should load");
    assert_eq!(stats.completed, 5);
    assert_eq!(stats.failed, 2);
    assert_eq!(stats.active, 0);
    trace_stage("main-lifecycle:done");

    let stale_claim_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "claim-stale")
        .expect("valid Redis URL should build the claim-stale queue");
    let stale_claim_completed = stale_claim_queue
        .add_job(
            "claim-stale-completed".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("stale completed job should add");
    let stale_claim_waiting = stale_claim_queue
        .add_job(
            "claim-stale-waiting".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("stale waiting job should add");
    let stale_claim_completed_claim = stale_claim_queue
        .claim_next(
            "worker-claim-stale-completed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("stale completed claim should return")
        .expect("stale completed job should claim");
    assert_eq!(stale_claim_completed_claim.id, stale_claim_completed.id);
    stale_claim_queue
        .complete_job(
            &stale_claim_completed_claim.id,
            lock_token(&stale_claim_completed_claim),
            serde_json::json!({}),
            Utc::now(),
        )
        .await
        .expect("stale completed job should complete");
    let mut stale_claim_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let _: usize = stale_claim_conn
        .zadd(
            format!("{namespace}:claim-stale:waiting"),
            &stale_claim_completed.id,
            0.0,
        )
        .await?;
    let claimed_after_stale = stale_claim_queue
        .claim_next(
            "worker-claim-stale-waiting".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("claim should skip stale waiting index")
        .expect("real waiting job should still claim");
    assert_eq!(claimed_after_stale.id, stale_claim_waiting.id);
    let stale_waiting_score: Option<f64> = stale_claim_conn
        .zscore(
            format!("{namespace}:claim-stale:waiting"),
            &stale_claim_completed.id,
        )
        .await?;
    assert!(stale_waiting_score.is_none());
    let stale_completed_after_claim = stale_claim_queue
        .get_job(&stale_claim_completed.id)
        .await
        .expect("stale completed job should load")
        .expect("stale completed job should still exist");
    assert_eq!(stale_completed_after_claim.state, JobState::Completed);
    stale_claim_queue
        .complete_job(
            &claimed_after_stale.id,
            lock_token(&claimed_after_stale),
            serde_json::json!({}),
            Utc::now(),
        )
        .await
        .expect("real waiting job should complete");
    trace_stage("claim-stale:done");

    let clean_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "clean-script")
        .expect("valid Redis URL should build the clean-script queue");
    let clean_old_a = clean_queue
        .add_job(
            "clean-old-a".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("first clean job should be added");
    let clean_old_b = clean_queue
        .add_job(
            "clean-old-b".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("second clean job should be added");
    let clean_new = clean_queue
        .add_job(
            "clean-new".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("new clean job should be added");
    clean_queue
        .add_log(&clean_old_a.id, "clean me".to_string(), 10, Utc::now())
        .await
        .expect("old clean job log should append");
    let clean_old_a_logs_key = format!("{namespace}:clean-script:logs:{}", clean_old_a.id);
    let clean_claim_a = clean_queue
        .claim_next(
            "worker-clean-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first clean claim should return")
        .expect("first clean job should be claimable");
    let clean_claim_b = clean_queue
        .claim_next(
            "worker-clean-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second clean claim should return")
        .expect("second clean job should be claimable");
    let clean_claim_new = clean_queue
        .claim_next(
            "worker-clean-new".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("new clean claim should return")
        .expect("new clean job should be claimable");
    assert_eq!(clean_claim_a.id, clean_old_a.id);
    assert_eq!(clean_claim_b.id, clean_old_b.id);
    assert_eq!(clean_claim_new.id, clean_new.id);
    let clean_now = Utc::now();
    clean_queue
        .complete_job(
            &clean_claim_a.id,
            lock_token(&clean_claim_a),
            serde_json::json!({ "ok": true }),
            clean_now - chrono::Duration::seconds(10),
        )
        .await
        .expect("first old clean job should complete");
    clean_queue
        .complete_job(
            &clean_claim_b.id,
            lock_token(&clean_claim_b),
            serde_json::json!({ "ok": true }),
            clean_now - chrono::Duration::seconds(9),
        )
        .await
        .expect("second old clean job should complete");
    clean_queue
        .complete_job(
            &clean_claim_new.id,
            lock_token(&clean_claim_new),
            serde_json::json!({ "ok": true }),
            clean_now,
        )
        .await
        .expect("new clean job should complete");
    let first_cleaned = clean_queue
        .clean_jobs(JobState::Completed, Duration::from_secs(5), 1, clean_now)
        .await
        .expect("first clean should run");
    assert_eq!(first_cleaned.len(), 1);
    assert_eq!(first_cleaned[0].id, clean_old_a.id);
    let mut clean_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let cleaned_hash: Option<String> = clean_conn
        .hget(format!("{namespace}:clean-script:jobs"), &clean_old_a.id)
        .await?;
    assert!(cleaned_hash.is_none());
    let cleaned_logs_len: usize = clean_conn.llen(&clean_old_a_logs_key).await?;
    assert_eq!(cleaned_logs_len, 0);
    let cleaned_completed_score: Option<f64> = clean_conn
        .zscore(
            format!("{namespace}:clean-script:completed"),
            &clean_old_a.id,
        )
        .await?;
    assert!(cleaned_completed_score.is_none());
    let retained_old_score: Option<f64> = clean_conn
        .zscore(
            format!("{namespace}:clean-script:completed"),
            &clean_old_b.id,
        )
        .await?;
    assert!(retained_old_score.is_some());
    let retained_new_score: Option<f64> = clean_conn
        .zscore(format!("{namespace}:clean-script:completed"), &clean_new.id)
        .await?;
    assert!(retained_new_score.is_some());
    trace_stage("clean-script:done");

    let clean_millis_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "clean-millis")
        .expect("valid Redis URL should build the clean-millis queue");
    let clean_millis_a_id = format!("{namespace}:clean-millis:a");
    let clean_millis_b_id = format!("{namespace}:clean-millis:b");
    clean_millis_queue
        .add_job(
            "clean-millis-a".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_job_id(clean_millis_a_id.clone()),
        )
        .await
        .expect("first clean-millis job should add");
    clean_millis_queue
        .add_job(
            "clean-millis-b".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_job_id(clean_millis_b_id.clone()),
        )
        .await
        .expect("second clean-millis job should add");
    let clean_millis_a = clean_millis_queue
        .claim_next(
            "worker-clean-millis-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first clean-millis claim should return")
        .expect("first clean-millis job should claim");
    let clean_millis_b = clean_millis_queue
        .claim_next(
            "worker-clean-millis-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second clean-millis claim should return")
        .expect("second clean-millis job should claim");
    let same_finished_at = Utc.timestamp_millis_opt(1_100).unwrap();
    clean_millis_queue
        .complete_job(
            &clean_millis_a.id,
            lock_token(&clean_millis_a),
            serde_json::json!({}),
            same_finished_at,
        )
        .await
        .expect("first clean-millis job should complete");
    clean_millis_queue
        .complete_job(
            &clean_millis_b.id,
            lock_token(&clean_millis_b),
            serde_json::json!({}),
            same_finished_at,
        )
        .await
        .expect("second clean-millis job should complete");
    let clean_millis_jobs_key = format!("{namespace}:clean-millis:jobs");
    let raw_a: String = clean_conn
        .hget(&clean_millis_jobs_key, &clean_millis_a_id)
        .await?;
    let raw_b: String = clean_conn
        .hget(&clean_millis_jobs_key, &clean_millis_b_id)
        .await?;
    let mut value_a: serde_json::Value =
        serde_json::from_str(&raw_a).expect("first clean-millis raw should be JSON");
    let mut value_b: serde_json::Value =
        serde_json::from_str(&raw_b).expect("second clean-millis raw should be JSON");
    value_a["finished_at"] = serde_json::Value::String("1970-01-01T00:00:01.100+00:00".into());
    value_b["finished_at"] = serde_json::Value::String("1970-01-01T00:00:01.1+00:00".into());
    let _: usize = clean_conn
        .hset(
            &clean_millis_jobs_key,
            &clean_millis_a_id,
            serde_json::to_string(&value_a).expect("first clean-millis raw should encode"),
        )
        .await?;
    let _: usize = clean_conn
        .hset(
            &clean_millis_jobs_key,
            &clean_millis_b_id,
            serde_json::to_string(&value_b).expect("second clean-millis raw should encode"),
        )
        .await?;
    let first_clean_millis = clean_millis_queue
        .clean_jobs(JobState::Completed, Duration::ZERO, 1, same_finished_at)
        .await
        .expect("clean-millis should use millisecond ordering");
    assert_eq!(first_clean_millis.len(), 1);
    assert_eq!(first_clean_millis[0].id, clean_millis_a_id);
    trace_stage("clean-millis:done");

    let drain_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "drain")
        .expect("valid Redis URL should build the drain queue");
    let mut drain_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let drain_repeat = drain_queue
        .add_job(
            "drain-repeat".to_string(),
            serde_json::json!({ "kind": "repeat" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .with_limit(2)
                    .with_key("drain-heartbeat"),
            ),
        )
        .await
        .expect("drain repeat should add");
    let drain_repeat_claim = drain_queue
        .claim_next(
            "worker-drain-repeat".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("drain repeat claim should return")
        .expect("drain repeat should be claimable");
    assert_eq!(drain_repeat_claim.id, drain_repeat.id);
    drain_queue
        .complete_job(
            &drain_repeat_claim.id,
            lock_token(&drain_repeat_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("drain repeat should complete");
    let drain_repeat_successor = drain_queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .expect("drain repeat delayed jobs should list")
        .jobs
        .into_iter()
        .find(|job| job.repeat_key.as_deref() == Some("drain-heartbeat"))
        .expect("drain repeat successor should be delayed");

    let drain_completed = drain_queue
        .add_job(
            "drain-completed".to_string(),
            serde_json::json!({ "kind": "completed" }),
            JobOptions::new().with_priority(1),
        )
        .await
        .expect("drain completed should add");
    let drain_completed_claim = drain_queue
        .claim_next(
            "worker-drain-completed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("drain completed claim should return")
        .expect("drain completed should be claimable");
    assert_eq!(drain_completed_claim.id, drain_completed.id);
    drain_queue
        .complete_job(
            &drain_completed_claim.id,
            lock_token(&drain_completed_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("drain completed should complete");

    let drain_active = drain_queue
        .add_job(
            "drain-active".to_string(),
            serde_json::json!({ "kind": "active" }),
            JobOptions::new().with_priority(1),
        )
        .await
        .expect("drain active should add");
    let drain_active_claim = drain_queue
        .claim_next(
            "worker-drain-active".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("drain active claim should return")
        .expect("drain active should be claimable");
    assert_eq!(drain_active_claim.id, drain_active.id);

    let drain_waiting = drain_queue
        .add_job(
            "drain-waiting".to_string(),
            serde_json::json!({ "kind": "waiting" }),
            JobOptions::new().with_priority(50),
        )
        .await
        .expect("drain waiting should add");
    let drain_delayed = drain_queue
        .add_job(
            "drain-delayed".to_string(),
            serde_json::json!({ "kind": "delayed" }),
            JobOptions::new().with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("drain delayed should add");
    drain_queue
        .add_log(
            &drain_waiting.id,
            "waiting drain log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("drain waiting log should append");
    drain_queue
        .add_log(
            &drain_delayed.id,
            "delayed drain log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("drain delayed log should append");
    let drain_waiting_logs_key = format!("{namespace}:drain:logs:{}", drain_waiting.id);
    let drain_delayed_logs_key = format!("{namespace}:drain:logs:{}", drain_delayed.id);
    let drain_flow = drain_queue
        .add_flow_at(
            JobSpec::new("drain-parent", serde_json::json!({ "kind": "parent" })),
            vec![JobSpec::new(
                "drain-child",
                serde_json::json!({ "kind": "child" }),
            )],
            Utc::now(),
        )
        .await
        .expect("drain flow should add");

    let drained_waiting = drain_queue
        .drain_jobs(false)
        .await
        .expect("drain waiting should run");
    let drained_waiting_ids = drained_waiting
        .iter()
        .map(|job| job.id.as_str())
        .collect::<Vec<_>>();
    assert!(drained_waiting_ids.contains(&drain_waiting.id.as_str()));
    assert!(drained_waiting_ids.contains(&drain_flow.children[0].id.as_str()));
    assert_eq!(drained_waiting.len(), 2);
    assert!(drain_queue
        .get_job(&drain_waiting.id)
        .await
        .expect("drain waiting lookup should return")
        .is_none());
    let drained_waiting_logs_len: usize = drain_conn.llen(&drain_waiting_logs_key).await?;
    assert_eq!(drained_waiting_logs_len, 0);
    assert!(drain_queue
        .get_job(&drain_flow.children[0].id)
        .await
        .expect("drain child lookup should return")
        .is_none());
    assert_eq!(
        drain_queue
            .get_job(&drain_flow.parent.id)
            .await
            .expect("drain parent lookup should return")
            .expect("drain parent should remain")
            .state,
        JobState::Waiting
    );
    drain_queue
        .remove_job(&drain_flow.parent.id)
        .await
        .expect("released drain parent should remove")
        .expect("released drain parent should be returned");
    assert_eq!(
        drain_queue
            .get_job(&drain_active.id)
            .await
            .expect("drain active lookup should return")
            .expect("drain active should remain")
            .state,
        JobState::Active
    );
    assert_eq!(
        drain_queue
            .get_job(&drain_completed.id)
            .await
            .expect("drain completed lookup should return")
            .expect("drain completed should remain")
            .state,
        JobState::Completed
    );
    assert!(drain_queue
        .get_job(&drain_delayed.id)
        .await
        .expect("drain delayed lookup should return")
        .is_some());

    let drained_delayed = drain_queue
        .drain_jobs(true)
        .await
        .expect("drain delayed should run");
    assert_eq!(drained_delayed.len(), 1);
    assert_eq!(drained_delayed[0].id, drain_delayed.id);
    let drain_delayed_score_after: Option<f64> = drain_conn
        .zscore(format!("{namespace}:drain:delayed"), &drain_delayed.id)
        .await?;
    assert!(drain_delayed_score_after.is_none());
    let drained_delayed_logs_len: usize = drain_conn.llen(&drain_delayed_logs_key).await?;
    assert_eq!(drained_delayed_logs_len, 0);
    let drain_repeat_score_after: Option<f64> = drain_conn
        .zscore(
            format!("{namespace}:drain:delayed"),
            &drain_repeat_successor.id,
        )
        .await?;
    assert!(drain_repeat_score_after.is_some());
    let drain_repeat_owner_after: Option<String> = drain_conn
        .get(format!("{namespace}:drain:repeat:drain-heartbeat"))
        .await?;
    assert_eq!(
        drain_repeat_owner_after.as_deref(),
        Some(drain_repeat_successor.id.as_str())
    );
    trace_stage("drain:done");

    let mut flow_index_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let existing_flow_child_id = format!("{namespace}:flow:existing-child");
    producer
        .add_job(
            "existing-flow-child".to_string(),
            serde_json::json!({ "kind": "existing" }),
            JobOptions::new()
                .with_job_id(existing_flow_child_id.clone())
                .with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("existing flow child id should be added");
    let rejected_flow_parent_id = format!("{namespace}:flow:rejected-parent");
    let rejected_flow_new_child_id = format!("{namespace}:flow:rejected-new-child");
    let rejected_flow = producer
        .add_flow_at(
            JobSpec::new(
                "rejected-flow-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_job_id(rejected_flow_parent_id.clone())),
            vec![
                JobSpec::new("duplicate-flow-child", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_job_id(existing_flow_child_id.clone())),
                JobSpec::new("new-flow-child", serde_json::json!({ "n": 2 })).with_options(
                    JobOptions::new().with_job_id(rejected_flow_new_child_id.clone()),
                ),
            ],
            Utc::now(),
        )
        .await
        .expect_err("flow with an existing Redis job id should be rejected");
    assert!(matches!(rejected_flow, LaneError::ConfigError(_)));
    assert!(producer
        .get_job(&rejected_flow_parent_id)
        .await
        .expect("rejected flow parent lookup should return")
        .is_none());
    assert!(producer
        .get_job(&rejected_flow_new_child_id)
        .await
        .expect("rejected flow child lookup should return")
        .is_none());
    let rejected_parent_waiting_children_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting_children"),
            &rejected_flow_parent_id,
        )
        .await?;
    assert!(rejected_parent_waiting_children_score.is_none());
    let rejected_child_waiting_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting"),
            &rejected_flow_new_child_id,
        )
        .await?;
    assert!(rejected_child_waiting_score.is_none());

    let flow = producer
        .add_flow_at(
            JobSpec::new("flow-parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("flow-child-a", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(2)),
                JobSpec::new("flow-child-b", serde_json::json!({ "n": 2 }))
                    .with_options(JobOptions::new().with_priority(3)),
            ],
            Utc::now(),
        )
        .await
        .expect("flow should be added");
    assert_eq!(flow.parent.state, JobState::WaitingChildren);
    let flow_dependencies_key = format!("{namespace}:jobs:dependencies:{}", flow.parent.id);
    let initial_flow_dependencies: usize = flow_index_conn.scard(&flow_dependencies_key).await?;
    assert_eq!(initial_flow_dependencies, 2);
    let child_a_is_dependency: bool = flow_index_conn
        .sismember(&flow_dependencies_key, &flow.children[0].id)
        .await?;
    let child_b_is_dependency: bool = flow_index_conn
        .sismember(&flow_dependencies_key, &flow.children[1].id)
        .await?;
    assert!(child_a_is_dependency);
    assert!(child_b_is_dependency);
    let flow_dependencies = producer
        .get_flow_dependencies(&flow.parent.id)
        .await
        .expect("flow dependencies should load")
        .expect("flow dependencies should exist");
    assert_eq!(flow_dependencies.parent.id, flow.parent.id);
    assert_eq!(
        flow_dependencies
            .children
            .iter()
            .map(|child| child.id.as_str())
            .collect::<Vec<_>>(),
        vec![flow.children[0].id.as_str(), flow.children[1].id.as_str()]
    );
    assert_eq!(flow_dependencies.pending_child_ids, flow.parent.child_ids);
    assert!(flow_dependencies.missing_child_ids.is_empty());
    let flow_counts = producer
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("flow dependency counts should load")
        .expect("flow dependency counts should exist");
    assert_eq!(flow_counts.processed, 0);
    assert_eq!(flow_counts.unprocessed, 2);
    assert_eq!(flow_counts.failed, 0);
    assert_eq!(flow_counts.missing, 0);

    let child_a = worker
        .claim_next(
            "worker-flow-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first flow child claim should return")
        .expect("first flow child should be claimable");
    assert_eq!(child_a.id, flow.children[0].id);
    worker
        .complete_job(
            &child_a.id,
            lock_token(&child_a),
            serde_json::json!({ "ok": 1 }),
            Utc::now(),
        )
        .await
        .expect("first child should complete");
    let dependencies_after_child_a: usize = flow_index_conn.scard(&flow_dependencies_key).await?;
    assert_eq!(dependencies_after_child_a, 1);
    let child_a_is_dependency: bool = flow_index_conn
        .sismember(&flow_dependencies_key, &flow.children[0].id)
        .await?;
    let child_b_is_dependency: bool = flow_index_conn
        .sismember(&flow_dependencies_key, &flow.children[1].id)
        .await?;
    assert!(!child_a_is_dependency);
    assert!(child_b_is_dependency);
    let flow_dependencies_after_child_a = producer
        .get_flow_dependencies(&flow.parent.id)
        .await
        .expect("flow dependencies after child a should load")
        .expect("flow dependencies after child a should exist");
    assert_eq!(
        flow_dependencies_after_child_a.pending_child_ids,
        vec![flow.children[1].id.clone()]
    );
    assert!(flow_dependencies_after_child_a.missing_child_ids.is_empty());
    let flow_counts_after_child_a = producer
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("flow dependency counts after child a should load")
        .expect("flow dependency counts after child a should exist");
    assert_eq!(flow_counts_after_child_a.processed, 1);
    assert_eq!(flow_counts_after_child_a.unprocessed, 1);
    assert_eq!(flow_counts_after_child_a.failed, 0);
    assert_eq!(flow_counts_after_child_a.missing, 0);
    assert_eq!(
        producer
            .get_job(&flow.parent.id)
            .await
            .expect("parent should load")
            .expect("parent should exist")
            .state,
        JobState::WaitingChildren
    );

    let child_b = worker
        .claim_next(
            "worker-flow-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second flow child claim should return")
        .expect("second flow child should be claimable");
    assert_eq!(child_b.id, flow.children[1].id);
    worker
        .complete_job(
            &child_b.id,
            lock_token(&child_b),
            serde_json::json!({ "ok": 2 }),
            Utc::now(),
        )
        .await
        .expect("second child should complete");

    let parent = producer
        .get_job(&flow.parent.id)
        .await
        .expect("released parent should load")
        .expect("released parent should exist");
    assert_eq!(parent.state, JobState::Waiting);
    let released_parent_waiting_score: Option<f64> = flow_index_conn
        .zscore(format!("{namespace}:jobs:waiting"), &flow.parent.id)
        .await?;
    assert!(released_parent_waiting_score.is_some());
    let released_parent_waiting_children_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting_children"),
            &flow.parent.id,
        )
        .await?;
    assert!(released_parent_waiting_children_score.is_none());
    let dependencies_after_release: usize = flow_index_conn.exists(&flow_dependencies_key).await?;
    assert_eq!(dependencies_after_release, 0);
    let flow_dependencies_after_release = producer
        .get_flow_dependencies(&flow.parent.id)
        .await
        .expect("flow dependencies after release should load")
        .expect("flow dependencies after release should exist");
    assert!(flow_dependencies_after_release.pending_child_ids.is_empty());
    assert!(flow_dependencies_after_release.missing_child_ids.is_empty());
    let flow_counts_after_release = producer
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("flow dependency counts after release should load")
        .expect("flow dependency counts after release should exist");
    assert_eq!(flow_counts_after_release.processed, 2);
    assert_eq!(flow_counts_after_release.unprocessed, 0);
    assert_eq!(flow_counts_after_release.failed, 0);
    assert_eq!(flow_counts_after_release.missing, 0);
    let claimed_parent = worker
        .claim_next(
            "worker-flow-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("flow parent claim should return")
        .expect("flow parent should be claimable");
    assert_eq!(claimed_parent.id, flow.parent.id);

    let remove_release_flow = producer
        .add_flow_at(
            JobSpec::new(
                "remove-release-flow-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("remove-release-flow-child-a", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(1)),
                JobSpec::new("remove-release-flow-child-b", serde_json::json!({ "n": 2 }))
                    .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("remove-release flow should be added");
    let remove_release_child = worker
        .claim_next(
            "worker-remove-release-child-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("remove-release child claim should return")
        .expect("remove-release child should be claimable");
    assert_eq!(remove_release_child.id, remove_release_flow.children[0].id);
    worker
        .complete_job(
            &remove_release_child.id,
            lock_token(&remove_release_child),
            serde_json::json!({ "ok": 1 }),
            Utc::now(),
        )
        .await
        .expect("remove-release child should complete");
    producer
        .remove_job(&remove_release_flow.children[1].id)
        .await
        .expect("remaining flow child should remove")
        .expect("remaining flow child should be returned");
    let remove_released_parent = producer
        .get_job(&remove_release_flow.parent.id)
        .await
        .expect("remove-released parent should load")
        .expect("remove-released parent should exist");
    assert_eq!(remove_released_parent.state, JobState::Waiting);
    let remove_released_dependencies = producer
        .get_flow_dependencies(&remove_release_flow.parent.id)
        .await
        .expect("remove-released dependencies should load")
        .expect("remove-released dependencies should exist");
    assert!(remove_released_dependencies.pending_child_ids.is_empty());
    assert_eq!(
        remove_released_dependencies.missing_child_ids,
        vec![remove_release_flow.children[1].id.clone()]
    );
    let remove_released_counts = producer
        .get_flow_dependency_counts(&remove_release_flow.parent.id)
        .await
        .expect("remove-released dependency counts should load")
        .expect("remove-released dependency counts should exist");
    assert_eq!(remove_released_counts.processed, 1);
    assert_eq!(remove_released_counts.unprocessed, 0);
    assert_eq!(remove_released_counts.failed, 0);
    assert_eq!(remove_released_counts.missing, 1);
    let remove_released_parent_waiting_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting"),
            &remove_release_flow.parent.id,
        )
        .await?;
    assert!(remove_released_parent_waiting_score.is_some());
    let remove_released_parent_waiting_children_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting_children"),
            &remove_release_flow.parent.id,
        )
        .await?;
    assert!(remove_released_parent_waiting_children_score.is_none());
    let removed_flow_child_hash: Option<String> = flow_index_conn
        .hget(
            format!("{namespace}:jobs:jobs"),
            &remove_release_flow.children[1].id,
        )
        .await?;
    assert!(removed_flow_child_hash.is_none());
    let claimed_remove_released_parent = worker
        .claim_next(
            "worker-remove-released-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("remove-released flow parent claim should return")
        .expect("remove-released flow parent should be claimable");
    assert_eq!(
        claimed_remove_released_parent.id,
        remove_release_flow.parent.id
    );

    let remove_unprocessed_flow = producer
        .add_flow_at(
            JobSpec::new(
                "remove-unprocessed-flow-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new(
                    "remove-unprocessed-flow-child-a",
                    serde_json::json!({ "n": 1 }),
                )
                .with_options(JobOptions::new().with_priority(1)),
                JobSpec::new(
                    "remove-unprocessed-flow-child-b",
                    serde_json::json!({ "n": 2 }),
                )
                .with_options(JobOptions::new().with_priority(2)),
                JobSpec::new(
                    "remove-unprocessed-flow-child-c",
                    serde_json::json!({ "n": 3 }),
                )
                .with_options(JobOptions::new().with_priority(3)),
            ],
            Utc::now(),
        )
        .await
        .expect("remove-unprocessed flow should be added");
    let remove_unprocessed_child_a = worker
        .claim_next(
            "worker-remove-unprocessed-child-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("remove-unprocessed child a claim should return")
        .expect("remove-unprocessed child a should be claimable");
    assert_eq!(
        remove_unprocessed_child_a.id,
        remove_unprocessed_flow.children[0].id
    );
    worker
        .complete_job(
            &remove_unprocessed_child_a.id,
            lock_token(&remove_unprocessed_child_a),
            serde_json::json!({ "ok": 1 }),
            Utc::now(),
        )
        .await
        .expect("remove-unprocessed child a should complete");
    let remove_unprocessed_child_b = worker
        .claim_next(
            "worker-remove-unprocessed-child-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("remove-unprocessed child b claim should return")
        .expect("remove-unprocessed child b should be claimable");
    assert_eq!(
        remove_unprocessed_child_b.id,
        remove_unprocessed_flow.children[1].id
    );
    let removed_unprocessed = producer
        .remove_unprocessed_children(&remove_unprocessed_flow.parent.id, Utc::now())
        .await
        .expect("remove-unprocessed children should run")
        .expect("remove-unprocessed parent should exist");
    assert_eq!(removed_unprocessed.len(), 1);
    assert_eq!(
        removed_unprocessed[0].id,
        remove_unprocessed_flow.children[2].id
    );
    let remove_unprocessed_dependency_key = format!(
        "{namespace}:jobs:dependencies:{}",
        remove_unprocessed_flow.parent.id
    );
    let remove_unprocessed_dependencies: usize = flow_index_conn
        .scard(&remove_unprocessed_dependency_key)
        .await?;
    assert_eq!(remove_unprocessed_dependencies, 1);
    let removed_unprocessed_child_hash: Option<String> = flow_index_conn
        .hget(
            format!("{namespace}:jobs:jobs"),
            &remove_unprocessed_flow.children[2].id,
        )
        .await?;
    assert!(removed_unprocessed_child_hash.is_none());
    let remove_unprocessed_parent = producer
        .get_job(&remove_unprocessed_flow.parent.id)
        .await
        .expect("remove-unprocessed parent should load")
        .expect("remove-unprocessed parent should exist");
    assert_eq!(remove_unprocessed_parent.state, JobState::WaitingChildren);
    worker
        .complete_job(
            &remove_unprocessed_child_b.id,
            lock_token(&remove_unprocessed_child_b),
            serde_json::json!({ "ok": 2 }),
            Utc::now(),
        )
        .await
        .expect("remove-unprocessed child b should complete");
    let remove_unprocessed_released_parent = producer
        .get_job(&remove_unprocessed_flow.parent.id)
        .await
        .expect("remove-unprocessed released parent should load")
        .expect("remove-unprocessed released parent should exist");
    assert_eq!(remove_unprocessed_released_parent.state, JobState::Waiting);
    let remove_unprocessed_counts = producer
        .get_flow_dependency_counts(&remove_unprocessed_flow.parent.id)
        .await
        .expect("remove-unprocessed counts should load")
        .expect("remove-unprocessed counts should exist");
    assert_eq!(remove_unprocessed_counts.processed, 2);
    assert_eq!(remove_unprocessed_counts.unprocessed, 0);
    assert_eq!(remove_unprocessed_counts.failed, 0);
    assert_eq!(remove_unprocessed_counts.missing, 1);

    let remove_dependency_flow = producer
        .add_flow_at(
            JobSpec::new(
                "remove-child-dependency-flow-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![JobSpec::new(
                "remove-child-dependency-flow-child",
                serde_json::json!({ "n": 1 }),
            )
            .with_options(JobOptions::new().with_priority(5))],
            Utc::now(),
        )
        .await
        .expect("remove-child-dependency flow should be added");
    assert!(producer
        .remove_child_dependency(&remove_dependency_flow.children[0].id, Utc::now())
        .await
        .expect("child dependency should remove"));
    assert!(!producer
        .remove_child_dependency(&remove_dependency_flow.children[0].id, Utc::now())
        .await
        .expect("removed child dependency should not remove twice"));
    let remove_dependency_parent = producer
        .get_job(&remove_dependency_flow.parent.id)
        .await
        .expect("remove-dependency parent should load")
        .expect("remove-dependency parent should exist");
    assert_eq!(remove_dependency_parent.state, JobState::Waiting);
    assert!(remove_dependency_parent.child_ids.is_empty());
    let remove_dependency_child = producer
        .get_job(&remove_dependency_flow.children[0].id)
        .await
        .expect("remove-dependency child should load")
        .expect("remove-dependency child should exist");
    assert!(remove_dependency_child.parent_id.is_none());
    let remove_dependency_key = format!(
        "{namespace}:jobs:dependencies:{}",
        remove_dependency_flow.parent.id
    );
    let remove_dependency_key_exists: bool = flow_index_conn.exists(&remove_dependency_key).await?;
    assert!(!remove_dependency_key_exists);
    let remove_dependency_parent_waiting_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting"),
            &remove_dependency_flow.parent.id,
        )
        .await?;
    assert!(remove_dependency_parent_waiting_score.is_some());

    let clean_release_flow = producer
        .add_flow_at(
            JobSpec::new(
                "clean-release-flow-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("clean-release-flow-child-a", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(1)),
                JobSpec::new("clean-release-flow-child-b", serde_json::json!({ "n": 2 }))
                    .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("clean-release flow should be added");
    let clean_release_child = worker
        .claim_next(
            "worker-clean-release-child-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("clean-release child claim should return")
        .expect("clean-release child should be claimable");
    assert_eq!(clean_release_child.id, clean_release_flow.children[0].id);
    worker
        .complete_job(
            &clean_release_child.id,
            lock_token(&clean_release_child),
            serde_json::json!({ "ok": 1 }),
            Utc::now(),
        )
        .await
        .expect("clean-release child should complete");
    let clean_released = producer
        .clean_jobs(JobState::Waiting, Duration::from_millis(0), 10, Utc::now())
        .await
        .expect("waiting flow child should clean");
    assert_eq!(clean_released.len(), 1);
    assert_eq!(clean_released[0].id, clean_release_flow.children[1].id);
    let clean_released_parent = producer
        .get_job(&clean_release_flow.parent.id)
        .await
        .expect("clean-released parent should load")
        .expect("clean-released parent should exist");
    assert_eq!(clean_released_parent.state, JobState::Waiting);
    let clean_released_parent_waiting_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting"),
            &clean_release_flow.parent.id,
        )
        .await?;
    assert!(clean_released_parent_waiting_score.is_some());
    let clean_released_parent_waiting_children_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting_children"),
            &clean_release_flow.parent.id,
        )
        .await?;
    assert!(clean_released_parent_waiting_children_score.is_none());
    let cleaned_flow_child_hash: Option<String> = flow_index_conn
        .hget(
            format!("{namespace}:jobs:jobs"),
            &clean_release_flow.children[1].id,
        )
        .await?;
    assert!(cleaned_flow_child_hash.is_none());
    let claimed_clean_released_parent = worker
        .claim_next(
            "worker-clean-released-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("clean-released flow parent claim should return")
        .expect("clean-released flow parent should be claimable");
    assert_eq!(
        claimed_clean_released_parent.id,
        clean_release_flow.parent.id
    );

    let remove_delayed_flow = producer
        .add_flow_at(
            JobSpec::new(
                "remove-delayed-flow-parent",
                serde_json::json!({ "kind": "delayed-aggregate" }),
            )
            .with_options(
                JobOptions::new()
                    .with_priority(1)
                    .with_delay(Duration::from_secs(60)),
            ),
            vec![
                JobSpec::new("remove-delayed-flow-child", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(1)),
            ],
            Utc::now(),
        )
        .await
        .expect("remove-delayed flow should be added");
    producer
        .remove_job(&remove_delayed_flow.children[0].id)
        .await
        .expect("remove-delayed child should remove")
        .expect("remove-delayed child should be returned");
    let remove_delayed_parent = producer
        .get_job(&remove_delayed_flow.parent.id)
        .await
        .expect("remove-delayed parent should load")
        .expect("remove-delayed parent should exist");
    assert_eq!(remove_delayed_parent.state, JobState::Delayed);
    let remove_delayed_parent_delayed_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:delayed"),
            &remove_delayed_flow.parent.id,
        )
        .await?;
    assert!(remove_delayed_parent_delayed_score.is_some());
    let remove_delayed_parent_waiting_children_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting_children"),
            &remove_delayed_flow.parent.id,
        )
        .await?;
    assert!(remove_delayed_parent_waiting_children_score.is_none());
    assert!(worker
        .claim_next(
            "worker-remove-delayed-flow-parent-early".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("early remove-delayed parent claim should return")
        .is_none());

    let delayed_flow = producer
        .add_flow_at(
            JobSpec::new(
                "delayed-flow-parent",
                serde_json::json!({ "kind": "delayed-aggregate" }),
            )
            .with_options(
                JobOptions::new()
                    .with_priority(1)
                    .with_delay(Duration::from_secs(60)),
            ),
            vec![
                JobSpec::new("delayed-flow-child", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(1)),
            ],
            Utc::now(),
        )
        .await
        .expect("delayed flow should be added");
    let delayed_child = worker
        .claim_next(
            "worker-delayed-flow-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("delayed flow child claim should return")
        .expect("delayed flow child should be claimable");
    assert_eq!(delayed_child.id, delayed_flow.children[0].id);
    worker
        .complete_job(
            &delayed_child.id,
            lock_token(&delayed_child),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("delayed flow child should complete");
    let delayed_parent = producer
        .get_job(&delayed_flow.parent.id)
        .await
        .expect("delayed parent should load")
        .expect("delayed parent should exist");
    assert_eq!(delayed_parent.state, JobState::Delayed);
    let delayed_parent_delayed_score: Option<f64> = flow_index_conn
        .zscore(format!("{namespace}:jobs:delayed"), &delayed_flow.parent.id)
        .await?;
    assert!(delayed_parent_delayed_score.is_some());
    let delayed_parent_waiting_children_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting_children"),
            &delayed_flow.parent.id,
        )
        .await?;
    assert!(delayed_parent_waiting_children_score.is_none());
    assert!(worker
        .claim_next(
            "worker-delayed-flow-parent-early".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("early delayed flow parent claim should return")
        .is_none());

    let failed_flow = producer
        .add_flow_at(
            JobSpec::new(
                "failed-flow-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("failed-flow-child", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(1)),
            ],
            Utc::now(),
        )
        .await
        .expect("failed flow should be added");
    let failed_child = worker
        .claim_next(
            "worker-failed-flow-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("failed flow child claim should return")
        .expect("failed flow child should be claimable");
    assert_eq!(failed_child.id, failed_flow.children[0].id);
    worker
        .fail_job(
            &failed_child.id,
            lock_token(&failed_child),
            "terminal child failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("failed flow child should fail");
    let failed_parent = producer
        .get_job(&failed_flow.parent.id)
        .await
        .expect("failed parent should load")
        .expect("failed parent should exist");
    assert_eq!(failed_parent.state, JobState::Failed);
    let expected_failed_reason = format!(
        "child job {} failed: terminal child failure",
        failed_child.id
    );
    assert_eq!(
        failed_parent.failed_reason.as_deref(),
        Some(expected_failed_reason.as_str())
    );
    let failed_parent_failed_score: Option<f64> = flow_index_conn
        .zscore(format!("{namespace}:jobs:failed"), &failed_flow.parent.id)
        .await?;
    assert!(failed_parent_failed_score.is_some());
    let failed_parent_waiting_children_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting_children"),
            &failed_flow.parent.id,
        )
        .await?;
    assert!(failed_parent_waiting_children_score.is_none());
    trace_stage("flow:done");

    let repeat = producer
        .add_job(
            "repeat".to_string(),
            serde_json::json!({ "kind": "heartbeat" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_millis(200))
                    .with_limit(2)
                    .with_key("heartbeat"),
            ),
        )
        .await
        .expect("repeat job should be added");
    let repeat_duplicate = worker
        .add_job(
            "repeat-duplicate".to_string(),
            serde_json::json!({ "kind": "heartbeat", "duplicate": true }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_millis(200))
                    .with_limit(2)
                    .with_key("heartbeat"),
            ),
        )
        .await
        .expect("duplicate repeat job should return the active series owner");
    assert_eq!(repeat_duplicate.id, repeat.id);
    let repeat_owner: Option<String> = flow_index_conn
        .get(format!("{namespace}:jobs:repeat:heartbeat"))
        .await?;
    assert_eq!(repeat_owner.as_deref(), Some(repeat.id.as_str()));
    let repeat_entries = producer
        .list_repeats()
        .await
        .expect("repeat series should list");
    assert!(repeat_entries.iter().any(|entry| {
        entry.key == "heartbeat"
            && entry.job_id == repeat.id
            && entry.name == "repeat"
            && entry.state == JobState::Waiting
            && entry.repeat_count == 0
    }));
    let first_repeat = worker
        .claim_next(
            "worker-repeat-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat claim should return")
        .expect("repeat job should be claimable");
    assert_eq!(first_repeat.id, repeat.id);
    worker
        .complete_job(
            &first_repeat.id,
            lock_token(&first_repeat),
            serde_json::json!({ "tick": 1 }),
            Utc::now(),
        )
        .await
        .expect("first repeat should complete");
    let delayed_repeats = producer
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .expect("delayed repeat should list");
    let repeat_successor = delayed_repeats
        .jobs
        .iter()
        .find(|&job| job.repeat_key.as_deref() == Some("heartbeat"))
        .cloned()
        .expect("repeat successor should be delayed");
    assert_eq!(repeat_successor.repeat_count, 1);
    let repeat_successor_owner: Option<String> = flow_index_conn
        .get(format!("{namespace}:jobs:repeat:heartbeat"))
        .await?;
    assert_eq!(
        repeat_successor_owner.as_deref(),
        Some(repeat_successor.id.as_str())
    );
    let repeat_entries_after_successor = producer
        .list_repeats()
        .await
        .expect("repeat successor series should list");
    let heartbeat_entry = repeat_entries_after_successor
        .iter()
        .find(|entry| entry.key == "heartbeat")
        .expect("heartbeat repeat entry should exist");
    assert_eq!(heartbeat_entry.job_id, repeat_successor.id);
    assert_eq!(heartbeat_entry.state, JobState::Delayed);
    assert_eq!(heartbeat_entry.repeat_count, 1);
    let repeat_duplicate_during_delay = producer
        .add_job(
            "repeat-duplicate-delayed".to_string(),
            serde_json::json!({ "kind": "heartbeat", "duplicate": "delayed" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_millis(200))
                    .with_limit(2)
                    .with_key("heartbeat"),
            ),
        )
        .await
        .expect("duplicate delayed repeat job should return the successor owner");
    assert_eq!(repeat_duplicate_during_delay.id, repeat_successor.id);
    let repeat_successor_delayed_score: Option<f64> = flow_index_conn
        .zscore(format!("{namespace}:jobs:delayed"), &repeat_successor.id)
        .await?;
    assert!(repeat_successor_delayed_score.is_some());

    tokio::time::sleep(Duration::from_millis(250)).await;
    producer
        .promote_due_jobs(Utc::now())
        .await
        .expect("repeat successor should promote");
    let second_repeat = worker
        .claim_next(
            "worker-repeat-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second repeat claim should return")
        .expect("second repeat should be claimable");
    assert_eq!(second_repeat.repeat_key.as_deref(), Some("heartbeat"));
    assert_eq!(second_repeat.repeat_count, 1);
    worker
        .complete_job(
            &second_repeat.id,
            lock_token(&second_repeat),
            serde_json::json!({ "tick": 2 }),
            Utc::now(),
        )
        .await
        .expect("second repeat should complete");
    let repeat_owner_after_limit: Option<String> = flow_index_conn
        .get(format!("{namespace}:jobs:repeat:heartbeat"))
        .await?;
    assert!(repeat_owner_after_limit.is_none());
    let repeat_entries_after_limit = producer
        .list_repeats()
        .await
        .expect("repeat series list after limit should return");
    assert!(!repeat_entries_after_limit
        .iter()
        .any(|entry| entry.key == "heartbeat"));
    let delayed_after_limit = producer
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .expect("delayed jobs should list after repeat limit");
    assert!(!delayed_after_limit
        .jobs
        .iter()
        .any(|job| job.repeat_key.as_deref() == Some("heartbeat")));
    trace_stage("repeat:done");

    let repeat_retry_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-retry")
        .expect("valid Redis URL should build the repeat-retry queue");
    let mut repeat_retry_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let repeat_retry = repeat_retry_queue
        .add_job(
            "repeat-retry".to_string(),
            serde_json::json!({ "kind": "retry-heartbeat" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(30))
                    .with_limit(2)
                    .with_key("retry-heartbeat"),
            ),
        )
        .await
        .expect("repeat-retry job should be added");
    let repeat_retry_claim = repeat_retry_queue
        .claim_next(
            "worker-repeat-retry-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat-retry claim should return")
        .expect("repeat-retry job should be claimable");
    repeat_retry_queue
        .fail_job(
            &repeat_retry_claim.id,
            lock_token(&repeat_retry_claim),
            "terminal repeat retry failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("terminal repeat failure should release owner");
    let repeat_retry_owner_after_fail: Option<String> = repeat_retry_conn
        .get(format!("{namespace}:repeat-retry:repeat:retry-heartbeat"))
        .await?;
    assert!(repeat_retry_owner_after_fail.is_none());
    let repeat_retry_requeued = repeat_retry_queue
        .retry_job(&repeat_retry.id, Utc::now())
        .await
        .expect("repeat retry should reclaim owner");
    assert_eq!(repeat_retry_requeued.state, JobState::Waiting);
    let repeat_retry_owner_after_retry: Option<String> = repeat_retry_conn
        .get(format!("{namespace}:repeat-retry:repeat:retry-heartbeat"))
        .await?;
    assert_eq!(
        repeat_retry_owner_after_retry.as_deref(),
        Some(repeat_retry.id.as_str())
    );
    let repeat_retry_duplicate = repeat_retry_queue
        .add_job(
            "repeat-retry-duplicate".to_string(),
            serde_json::json!({ "kind": "retry-heartbeat", "duplicate": true }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(30))
                    .with_limit(2)
                    .with_key("retry-heartbeat"),
            ),
        )
        .await
        .expect("duplicate after repeat retry should return retried job");
    assert_eq!(repeat_retry_duplicate.id, repeat_retry.id);
    repeat_retry_queue
        .remove_job(&repeat_retry.id)
        .await
        .expect("retried repeat job should remove")
        .expect("retried repeat job should be returned");
    let repeat_retry_owner_after_remove: Option<String> = repeat_retry_conn
        .get(format!("{namespace}:repeat-retry:repeat:retry-heartbeat"))
        .await?;
    assert!(repeat_retry_owner_after_remove.is_none());

    let repeat_retry_conflict_a = repeat_retry_queue
        .add_job(
            "repeat-retry-conflict-a".to_string(),
            serde_json::json!({ "kind": "retry-conflict-a" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(30))
                    .with_limit(2)
                    .with_key("retry-conflict-heartbeat"),
            ),
        )
        .await
        .expect("repeat retry conflict first job should be added");
    let repeat_retry_conflict_claim = repeat_retry_queue
        .claim_next(
            "worker-repeat-retry-conflict".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat retry conflict claim should return")
        .expect("repeat retry conflict job should be claimable");
    assert_eq!(repeat_retry_conflict_claim.id, repeat_retry_conflict_a.id);
    repeat_retry_queue
        .fail_job(
            &repeat_retry_conflict_claim.id,
            lock_token(&repeat_retry_conflict_claim),
            "terminal repeat retry conflict".to_string(),
            Utc::now(),
        )
        .await
        .expect("terminal repeat conflict failure should release owner");
    let repeat_retry_conflict_b = repeat_retry_queue
        .add_job(
            "repeat-retry-conflict-b".to_string(),
            serde_json::json!({ "kind": "retry-conflict-b" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(30))
                    .with_limit(2)
                    .with_key("retry-conflict-heartbeat"),
            ),
        )
        .await
        .expect("repeat retry conflict second job should be added");
    assert_ne!(repeat_retry_conflict_b.id, repeat_retry_conflict_a.id);
    let repeat_retry_conflict = repeat_retry_queue
        .retry_job(&repeat_retry_conflict_a.id, Utc::now())
        .await
        .expect_err("repeat retry should reject another active series owner");
    assert!(matches!(
        repeat_retry_conflict,
        LaneError::JobStateConflict(_)
    ));
    let repeat_retry_conflict_failed_score: Option<f64> = repeat_retry_conn
        .zscore(
            format!("{namespace}:repeat-retry:failed"),
            &repeat_retry_conflict_a.id,
        )
        .await?;
    assert!(repeat_retry_conflict_failed_score.is_some());
    trace_stage("repeat-retry:done");

    let cron_repeat = producer
        .add_job(
            "cron-repeat".to_string(),
            serde_json::json!({ "kind": "cron-heartbeat" }),
            JobOptions::new().with_repeat(
                RepeatOptions::cron("0/1 * * * * * *")
                    .with_limit(2)
                    .with_key("cron-heartbeat"),
            ),
        )
        .await
        .expect("cron repeat job should be added");
    let first_cron_repeat = worker
        .claim_next(
            "worker-cron-repeat-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("cron repeat claim should return")
        .expect("cron repeat job should be claimable");
    assert_eq!(first_cron_repeat.id, cron_repeat.id);
    let cron_completed_at = Utc::now();
    worker
        .complete_job(
            &first_cron_repeat.id,
            lock_token(&first_cron_repeat),
            serde_json::json!({ "tick": 1 }),
            cron_completed_at,
        )
        .await
        .expect("first cron repeat should complete");
    let delayed_cron_repeats = producer
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .expect("delayed cron repeat should list");
    let cron_successor = delayed_cron_repeats
        .jobs
        .iter()
        .find(|job| job.repeat_key.as_deref() == Some("cron-heartbeat"))
        .expect("cron repeat successor should be delayed");
    assert_eq!(cron_successor.repeat_count, 1);
    assert!(cron_successor.scheduled_at > cron_completed_at);

    sleep_until_due(cron_successor.scheduled_at).await;
    producer
        .promote_due_jobs(Utc::now())
        .await
        .expect("cron repeat successor should promote");
    let second_cron_repeat = worker
        .claim_next(
            "worker-cron-repeat-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second cron repeat claim should return")
        .expect("second cron repeat should be claimable");
    assert_eq!(
        second_cron_repeat.repeat_key.as_deref(),
        Some("cron-heartbeat")
    );
    assert_eq!(second_cron_repeat.repeat_count, 1);
    worker
        .complete_job(
            &second_cron_repeat.id,
            lock_token(&second_cron_repeat),
            serde_json::json!({ "tick": 2 }),
            Utc::now(),
        )
        .await
        .expect("second cron repeat should complete");
    let delayed_after_cron_limit = producer
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .expect("delayed jobs should list after cron repeat limit");
    assert!(!delayed_after_cron_limit
        .jobs
        .iter()
        .any(|job| job.repeat_key.as_deref() == Some("cron-heartbeat")));
    trace_stage("cron-repeat:done");

    let repeat_remove_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-remove")
            .expect("valid Redis URL should build the repeat-remove queue");
    let repeat_remove = repeat_remove_queue
        .add_job(
            "repeat-remove".to_string(),
            serde_json::json!({ "kind": "ephemeral-heartbeat" }),
            JobOptions::new().remove_on_complete(true).with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .with_limit(2)
                    .with_key("ephemeral-heartbeat"),
            ),
        )
        .await
        .expect("repeat-remove job should be added");
    let repeat_remove_claim = repeat_remove_queue
        .claim_next(
            "worker-repeat-remove".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat-remove claim should return")
        .expect("repeat-remove job should be claimable");
    assert_eq!(repeat_remove_claim.id, repeat_remove.id);
    repeat_remove_queue
        .complete_job(
            &repeat_remove_claim.id,
            lock_token(&repeat_remove_claim),
            serde_json::json!({ "tick": 1 }),
            Utc::now(),
        )
        .await
        .expect("repeat-remove job should complete");
    assert!(repeat_remove_queue
        .get_job(&repeat_remove.id)
        .await
        .expect("removed repeat job lookup should return")
        .is_none());
    let repeat_remove_delayed = repeat_remove_queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .expect("repeat-remove delayed jobs should list");
    let repeat_remove_successor = repeat_remove_delayed
        .jobs
        .iter()
        .find(|&job| job.repeat_key.as_deref() == Some("ephemeral-heartbeat"))
        .cloned()
        .expect("repeat-remove successor should be delayed");
    assert_eq!(repeat_remove_successor.repeat_count, 1);
    let mut repeat_remove_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let repeat_remove_delayed_score: Option<f64> = repeat_remove_conn
        .zscore(
            format!("{namespace}:repeat-remove:delayed"),
            &repeat_remove_successor.id,
        )
        .await?;
    assert!(repeat_remove_delayed_score.is_some());
    let repeat_remove_owner: Option<String> = repeat_remove_conn
        .get(format!(
            "{namespace}:repeat-remove:repeat:ephemeral-heartbeat"
        ))
        .await?;
    assert_eq!(
        repeat_remove_owner.as_deref(),
        Some(repeat_remove_successor.id.as_str())
    );
    let repeat_removed_by_key = repeat_remove_queue
        .remove_repeat("ephemeral-heartbeat")
        .await
        .expect("repeat-remove successor should remove by repeat key")
        .expect("repeat-remove successor should be returned");
    assert_eq!(repeat_removed_by_key.id, repeat_remove_successor.id);
    let repeat_remove_delayed_score_after: Option<f64> = repeat_remove_conn
        .zscore(
            format!("{namespace}:repeat-remove:delayed"),
            &repeat_remove_successor.id,
        )
        .await?;
    assert!(repeat_remove_delayed_score_after.is_none());
    let repeat_remove_hash_after: Option<String> = repeat_remove_conn
        .hget(
            format!("{namespace}:repeat-remove:jobs"),
            &repeat_remove_successor.id,
        )
        .await?;
    assert!(repeat_remove_hash_after.is_none());
    let repeat_remove_owner_after_remove: Option<String> = repeat_remove_conn
        .get(format!(
            "{namespace}:repeat-remove:repeat:ephemeral-heartbeat"
        ))
        .await?;
    assert!(repeat_remove_owner_after_remove.is_none());
    assert!(repeat_remove_queue
        .remove_repeat("ephemeral-heartbeat")
        .await
        .expect("second repeat-remove by key should return")
        .is_none());
    let _: () = repeat_remove_conn
        .set(
            format!("{namespace}:repeat-remove:repeat:stale-heartbeat"),
            "missing-repeat-owner",
        )
        .await?;
    let repeat_remove_entries_after_stale = repeat_remove_queue
        .list_repeats()
        .await
        .expect("repeat list should prune stale owner keys");
    assert!(!repeat_remove_entries_after_stale
        .iter()
        .any(|entry| entry.key == "stale-heartbeat"));
    let stale_repeat_owner_after_list: Option<String> = repeat_remove_conn
        .get(format!("{namespace}:repeat-remove:repeat:stale-heartbeat"))
        .await?;
    assert!(stale_repeat_owner_after_list.is_none());
    assert!(repeat_remove_queue
        .remove_repeat("stale-heartbeat")
        .await
        .expect("stale repeat owner should return")
        .is_none());
    let stale_repeat_owner_after_remove: Option<String> = repeat_remove_conn
        .get(format!("{namespace}:repeat-remove:repeat:stale-heartbeat"))
        .await?;
    assert!(stale_repeat_owner_after_remove.is_none());
    let repeat_remove_new_series = repeat_remove_queue
        .add_job(
            "repeat-remove-after-key".to_string(),
            serde_json::json!({ "kind": "ephemeral-heartbeat", "after": "remove-key" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .with_limit(2)
                    .with_key("ephemeral-heartbeat"),
            ),
        )
        .await
        .expect("repeat remove key should allow a new series");
    assert_ne!(repeat_remove_new_series.id, repeat_remove_successor.id);
    assert_eq!(repeat_remove_new_series.repeat_count, 0);
    let repeat_remove_active = repeat_remove_queue
        .claim_next(
            "worker-repeat-remove-active".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat remove active claim should return")
        .expect("repeat remove new series should be claimable");
    assert_eq!(repeat_remove_active.id, repeat_remove_new_series.id);
    let repeat_remove_active_error = repeat_remove_queue
        .remove_repeat("ephemeral-heartbeat")
        .await
        .expect_err("active repeat owner should reject remove by key");
    assert!(matches!(
        repeat_remove_active_error,
        LaneError::JobLeaseConflict(_)
    ));
    trace_stage("repeat-remove:done");

    let idempotent_job_id = format!("{namespace}:invoice:42");
    let idempotent = producer
        .add_job(
            "invoice".to_string(),
            serde_json::json!({ "id": 42, "attempt": 1 }),
            JobOptions::new()
                .with_job_id(idempotent_job_id.clone())
                .with_priority(30),
        )
        .await
        .expect("idempotent job should be added");
    let duplicate = worker
        .add_job(
            "invoice-duplicate".to_string(),
            serde_json::json!({ "id": 42, "attempt": 2 }),
            JobOptions::new()
                .with_job_id(idempotent_job_id.clone())
                .with_priority(1),
        )
        .await
        .expect("duplicate idempotent job should return existing");
    assert_eq!(duplicate, idempotent);
    let waiting_jobs = producer
        .list_jobs(JobListOptions::new().with_state(JobState::Waiting))
        .await
        .expect("waiting jobs should list after idempotent add");
    assert_eq!(
        waiting_jobs
            .jobs
            .iter()
            .filter(|job| job.id == idempotent_job_id)
            .count(),
        1
    );

    let bulk_first_id = format!("{namespace}:bulk:first");
    let bulk_second_id = format!("{namespace}:bulk:second");
    let bulk_jobs = producer
        .add_jobs(
            vec![
                JobSpec::new("bulk-first", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_job_id(bulk_first_id.clone())),
                JobSpec::new("bulk-second", serde_json::json!({ "n": 2 }))
                    .with_options(JobOptions::new().with_job_id(bulk_second_id.clone())),
                JobSpec::new("bulk-first-duplicate", serde_json::json!({ "n": 3 }))
                    .with_options(JobOptions::new().with_job_id(bulk_first_id.clone())),
            ],
            Utc::now(),
        )
        .await
        .expect("bulk jobs should be added");
    assert_eq!(bulk_jobs.len(), 3);
    assert_eq!(bulk_jobs[2], bulk_jobs[0]);
    let waiting_after_bulk = producer
        .list_jobs(JobListOptions::new().with_state(JobState::Waiting))
        .await
        .expect("waiting jobs should list after bulk add");
    assert_eq!(
        waiting_after_bulk
            .jobs
            .iter()
            .filter(|job| job.id == bulk_first_id || job.id == bulk_second_id)
            .count(),
        2
    );
    trace_stage("idempotent-bulk:done");

    let atomic_add_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "atomic-add")
        .expect("valid Redis URL should build the atomic-add queue");
    let atomic_add_id = format!("{namespace}:atomic:add");
    let atomic_first = atomic_add_queue
        .add_job(
            "atomic-add".to_string(),
            serde_json::json!({ "attempt": 1 }),
            JobOptions::new()
                .with_job_id(atomic_add_id.clone())
                .with_priority(3),
        )
        .await
        .expect("atomic-add job should be added");
    let atomic_duplicate = atomic_add_queue
        .add_job(
            "atomic-add-duplicate".to_string(),
            serde_json::json!({ "attempt": 2 }),
            JobOptions::new()
                .with_job_id(atomic_add_id.clone())
                .with_priority(1),
        )
        .await
        .expect("duplicate atomic-add job should return existing");
    assert_eq!(atomic_duplicate, atomic_first);
    let mut atomic_add_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let atomic_sequence: Option<u64> = atomic_add_conn
        .get(format!("{namespace}:atomic-add:sequence"))
        .await?;
    assert_eq!(atomic_sequence, Some(1));
    let atomic_waiting_count: usize = atomic_add_conn
        .zcard(format!("{namespace}:atomic-add:waiting"))
        .await?;
    assert_eq!(atomic_waiting_count, 1);
    let atomic_waiting_score: Option<f64> = atomic_add_conn
        .zscore(format!("{namespace}:atomic-add:waiting"), &atomic_add_id)
        .await?;
    assert!(atomic_waiting_score.is_some());

    let atomic_delayed_id = format!("{namespace}:atomic:delayed");
    let atomic_delayed = atomic_add_queue
        .add_job(
            "atomic-delayed".to_string(),
            serde_json::json!({ "attempt": 1 }),
            JobOptions::new()
                .with_job_id(atomic_delayed_id.clone())
                .with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("atomic delayed job should be added");
    let atomic_delayed_duplicate = atomic_add_queue
        .add_job(
            "atomic-delayed-duplicate".to_string(),
            serde_json::json!({ "attempt": 2 }),
            JobOptions::new()
                .with_job_id(atomic_delayed_id.clone())
                .with_delay(Duration::from_secs(30)),
        )
        .await
        .expect("duplicate atomic delayed job should return existing");
    assert_eq!(atomic_delayed_duplicate, atomic_delayed);
    let atomic_sequence_after_delayed: Option<u64> = atomic_add_conn
        .get(format!("{namespace}:atomic-add:sequence"))
        .await?;
    assert_eq!(atomic_sequence_after_delayed, Some(1));
    let atomic_delayed_count: usize = atomic_add_conn
        .zcard(format!("{namespace}:atomic-add:delayed"))
        .await?;
    assert_eq!(atomic_delayed_count, 1);
    let atomic_delayed_score: Option<f64> = atomic_add_conn
        .zscore(
            format!("{namespace}:atomic-add:delayed"),
            &atomic_delayed_id,
        )
        .await?;
    assert!(atomic_delayed_score.is_some());

    trace_stage("cleanup:final:start");
    match tokio::time::timeout(
        Duration::from_secs(5),
        cleanup_namespace_with_conn(&mut atomic_add_conn, &namespace),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("warning: final Redis cleanup failed for {namespace}: {error}");
        }
        Err(_) => {
            eprintln!("warning: final Redis cleanup timed out for {namespace}");
        }
    }
    trace_stage("cleanup:final:done");
    Ok(())
}

async fn run_state_count_indexes(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("state-count:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("state-count:cleanup:done");

    let state_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "state-counts")
        .expect("valid Redis URL should build the state-count queue");
    let active_job = state_queue
        .add_job(
            "state-active".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("active state-count job should add");
    let active = state_queue
        .claim_next(
            "worker-state-active".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("active state-count claim should return")
        .expect("active state-count job should be claimable");
    assert_eq!(active.id, active_job.id);

    let completed_job = state_queue
        .add_job(
            "state-completed".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("completed state-count job should add");
    let completed = state_queue
        .claim_next(
            "worker-state-completed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("completed state-count claim should return")
        .expect("completed state-count job should be claimable");
    assert_eq!(completed.id, completed_job.id);
    state_queue
        .complete_job(
            &completed.id,
            lock_token(&completed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("completed state-count job should complete");

    let failed_job = state_queue
        .add_job(
            "state-failed".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("failed state-count job should add");
    let failed = state_queue
        .claim_next(
            "worker-state-failed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("failed state-count claim should return")
        .expect("failed state-count job should be claimable");
    assert_eq!(failed.id, failed_job.id);
    state_queue
        .fail_job(
            &failed.id,
            lock_token(&failed),
            "terminal failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("failed state-count job should fail");

    state_queue
        .add_job(
            "state-waiting-a".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("first waiting state-count job should add");
    state_queue
        .add_job(
            "state-waiting-b".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("second waiting state-count job should add");
    state_queue
        .add_job(
            "state-delayed".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(30)),
        )
        .await
        .expect("delayed state-count job should add");
    state_queue
        .add_flow(
            JobSpec::new("state-parent", serde_json::json!({})),
            vec![JobSpec::new("state-child", serde_json::json!({}))],
        )
        .await
        .expect("waiting-children state-count flow should add");

    let selected_state_counts = state_queue
        .get_job_counts(&[
            JobState::Waiting,
            JobState::Delayed,
            JobState::Waiting,
            JobState::Active,
        ])
        .await
        .expect("selected state counts should load");
    assert_eq!(
        selected_state_counts,
        vec![
            JobStateCount {
                state: JobState::Waiting,
                count: 3,
            },
            JobStateCount {
                state: JobState::Delayed,
                count: 1,
            },
            JobStateCount {
                state: JobState::Active,
                count: 1,
            },
        ]
    );

    let all_state_counts = state_queue
        .get_job_counts(&[])
        .await
        .expect("default state counts should load");
    assert_eq!(
        all_state_counts,
        vec![
            JobStateCount {
                state: JobState::Waiting,
                count: 3,
            },
            JobStateCount {
                state: JobState::Delayed,
                count: 1,
            },
            JobStateCount {
                state: JobState::Active,
                count: 1,
            },
            JobStateCount {
                state: JobState::WaitingChildren,
                count: 1,
            },
            JobStateCount {
                state: JobState::Completed,
                count: 1,
            },
            JobStateCount {
                state: JobState::Failed,
                count: 1,
            },
        ]
    );
    assert_eq!(
        state_queue
            .get_job_count(&[JobState::Waiting, JobState::Delayed, JobState::Waiting])
            .await
            .expect("selected aggregate state count should load"),
        4
    );
    assert_eq!(
        state_queue
            .get_job_count(&[])
            .await
            .expect("default aggregate state count should load"),
        8
    );
    assert_eq!(
        state_queue
            .count_pending_jobs()
            .await
            .expect("pending state count should load"),
        5
    );

    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let waiting_zcard: usize = conn
        .zcard(format!("{namespace}:state-counts:waiting"))
        .await?;
    let delayed_zcard: usize = conn
        .zcard(format!("{namespace}:state-counts:delayed"))
        .await?;
    let active_zcard: usize = conn
        .zcard(format!("{namespace}:state-counts:active"))
        .await?;
    let waiting_children_zcard: usize = conn
        .zcard(format!("{namespace}:state-counts:waiting_children"))
        .await?;
    let completed_zcard: usize = conn
        .zcard(format!("{namespace}:state-counts:completed"))
        .await?;
    let failed_zcard: usize = conn
        .zcard(format!("{namespace}:state-counts:failed"))
        .await?;
    assert_eq!(waiting_zcard, 3);
    assert_eq!(delayed_zcard, 1);
    assert_eq!(active_zcard, 1);
    assert_eq!(waiting_children_zcard, 1);
    assert_eq!(completed_zcard, 1);
    assert_eq!(failed_zcard, 1);

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("state-count:done");
    Ok(())
}

async fn sleep_until_due(scheduled_at: DateTime<Utc>) {
    let delay = scheduled_at
        .signed_duration_since(Utc::now())
        .to_std()
        .unwrap_or(Duration::ZERO)
        .saturating_add(Duration::from_millis(50))
        .min(Duration::from_secs(2));
    tokio::time::sleep(delay).await;
}

fn redis_url() -> Option<String> {
    std::env::var("A3S_LANE_REDIS_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn unique_namespace() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = NAMESPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "a3s:lane:test:{}:{timestamp}:{sequence}",
        std::process::id()
    )
}

fn trace_stage(stage: &str) {
    if std::env::var_os("A3S_LANE_REDIS_TRACE").is_some() {
        eprintln!("[redis_job_queue] {stage}");
    }
}

async fn cleanup_namespace(redis_url: &str, namespace: &str) -> redis::RedisResult<()> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_connection_manager().await?;
    cleanup_namespace_with_conn(&mut conn, namespace).await
}

async fn cleanup_namespace_with_conn(
    conn: &mut redis::aio::ConnectionManager,
    namespace: &str,
) -> redis::RedisResult<()> {
    let mut cursor = 0_u64;
    loop {
        let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(format!("{namespace}:*"))
            .arg("COUNT")
            .arg(100_u16)
            .query_async(conn)
            .await?;
        if !keys.is_empty() {
            let _: usize = conn.del(keys).await?;
        }
        if next_cursor == 0 {
            break;
        }
        cursor = next_cursor;
    }
    Ok(())
}
