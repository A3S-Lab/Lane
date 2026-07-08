#![cfg(feature = "redis-backend")]

use a3s_lane::{
    job_processor_fn, DeduplicationOptions, Job, JobContext, JobFinishedResult,
    JobFlowDependencyCountOptions, JobFlowDependencyKind, JobFlowDependencyPageCursor,
    JobFlowDependencyPageItem, JobFlowDependencyPageOptions, JobFlowDependencyPagesOptions,
    JobLeaseRenewal, JobListOptions, JobLogEntry, JobOptions, JobPriorityCount, JobProcessor,
    JobQueueBackend, JobRateLimit, JobRepeatListOptions, JobRetention, JobRunOutcome, JobSpec,
    JobState, JobStateCount, JobWorker, JobWorkerConfig, LaneError, RedisJobQueue, RepeatOptions,
    RetryPolicy, MAX_JOB_PRIORITY,
};
use chrono::{DateTime, TimeZone, Utc};
use redis::{AsyncCommands, ConnectionAddr, IntoConnectionInfo};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, MutexGuard};

static NAMESPACE_COUNTER: AtomicU64 = AtomicU64::new(0);
static REDIS_TEST_LOCK: Mutex<()> = Mutex::const_new(());
static REDIS_TEST_URL: OnceLock<Option<String>> = OnceLock::new();

async fn redis_test_guard() -> MutexGuard<'static, ()> {
    REDIS_TEST_LOCK.lock().await
}

fn lock_token(job: &a3s_lane::Job) -> &str {
    job.lock_token
        .as_deref()
        .expect("claimed job should carry a lock token")
}

fn ts(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).unwrap()
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
                let _guard = redis_test_guard().await;
                tokio::time::timeout(Duration::from_secs(900), run_job_lifecycle(redis_url))
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
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_discard_retry(redis_url))
        .await
        .expect("Redis discard retry integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_preserves_replace_deduplication_ttl_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await.unwrap();

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "dedup-replace-ttl")
        .expect("valid Redis URL should build the queue");
    let mut conn = redis::Client::open(redis_url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let dedup_key = format!("{namespace}:dedup-replace-ttl:deduplication:tenant:replace-ttl");

    let old = queue
        .add_job(
            "dedup-replace-ttl-old".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(30))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace-ttl")
                        .with_ttl(Duration::from_secs(30))
                        .replace_delayed(true),
                ),
        )
        .await
        .expect("old delayed dedup owner should be added");
    let ttl_overridden: bool = redis::cmd("PEXPIRE")
        .arg(&dedup_key)
        .arg(10_000)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(ttl_overridden);
    let ttl_before: i64 = redis::cmd("PTTL")
        .arg(&dedup_key)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(ttl_before > 0 && ttl_before <= 10_000);

    let new = queue
        .add_job(
            "dedup-replace-ttl-new".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(60))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace-ttl")
                        .with_ttl(Duration::from_secs(30))
                        .replace_delayed(true),
                ),
        )
        .await
        .expect("new delayed dedup owner should replace old owner");
    assert_ne!(new.id, old.id);

    let ttl_after: i64 = redis::cmd("PTTL")
        .arg(&dedup_key)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(
        ttl_after > 0 && ttl_after <= ttl_before,
        "replace should preserve the remaining TTL, before {ttl_before}, after {ttl_after}"
    );

    cleanup_namespace(&redis_url, &namespace).await.unwrap();
}

#[tokio::test]
async fn redis_backend_cleans_only_unlocked_active_jobs_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    let namespace = unique_namespace();
    trace_stage("clean-active-focused:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await.unwrap();
    trace_stage("clean-active-focused:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "clean-active")
        .expect("valid Redis URL should build the queue");
    trace_stage("clean-active-focused:queue-created");
    let mut conn = redis::Client::open(redis_url.as_str())
        .unwrap()
        .get_connection_manager()
        .await
        .unwrap();
    trace_stage("clean-active-focused:conn-created");
    let locked = queue
        .add_job(
            "active-locked".to_string(),
            serde_json::json!({ "kind": "locked" }),
            JobOptions::new(),
        )
        .await
        .expect("locked active job should add");
    trace_stage("clean-active-focused:locked-added");
    let unlocked = queue
        .add_job(
            "active-unlocked".to_string(),
            serde_json::json!({ "kind": "unlocked" }),
            JobOptions::new(),
        )
        .await
        .expect("unlocked active job should add");
    trace_stage("clean-active-focused:unlocked-added");
    let locked_claim = queue
        .claim_next(
            "worker-clean-active-locked".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("locked active claim should return")
        .expect("locked active job should claim");
    assert_eq!(locked_claim.id, locked.id);
    trace_stage("clean-active-focused:locked-claimed");
    let unlocked_claim = queue
        .claim_next(
            "worker-clean-active-unlocked".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("unlocked active claim should return")
        .expect("unlocked active job should claim");
    assert_eq!(unlocked_claim.id, unlocked.id);
    trace_stage("clean-active-focused:unlocked-claimed");

    let unlocked_lock_key = format!("{namespace}:clean-active:locks:{}", unlocked.id);
    let removed_unlocked_lock: usize = conn.del(&unlocked_lock_key).await.unwrap();
    assert_eq!(removed_unlocked_lock, 1);
    trace_stage("clean-active-focused:unlocked-lock-removed");
    let clean_now = unlocked_claim
        .lease_expires_at
        .expect("unlocked claim should carry lease expiration")
        + chrono::Duration::seconds(1);

    let cleaned = queue
        .clean_jobs(JobState::Active, Duration::ZERO, 10, clean_now)
        .await
        .expect("active clean should run");
    trace_stage("clean-active-focused:cleaned");
    assert_eq!(
        cleaned
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>(),
        vec![unlocked.id.as_str()]
    );
    assert!(queue
        .get_job(&unlocked.id)
        .await
        .expect("unlocked active lookup should return")
        .is_none());
    assert_eq!(
        queue
            .get_job(&locked.id)
            .await
            .expect("locked active lookup should return")
            .expect("locked active job should remain")
            .state,
        JobState::Active
    );
    let locked_score: Option<f64> = conn
        .zscore(format!("{namespace}:clean-active:active"), &locked.id)
        .await
        .unwrap();
    assert!(locked_score.is_some());
    trace_stage("clean-active-focused:locked-score-checked");

    cleanup_namespace(&redis_url, &namespace).await.unwrap();
    trace_stage("clean-active-focused:cleanup-final:done");
}

#[tokio::test]
async fn redis_backend_counts_states_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_state_count_indexes(redis_url))
        .await
        .expect("Redis state-count integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_reads_job_finished_results_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_job_finished_results(redis_url),
    )
    .await
    .expect("Redis job finished result integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_removes_orphaned_jobs_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_orphaned_job_removal(redis_url),
    )
    .await
    .expect("Redis orphaned-job removal integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_remove_missing_prunes_orphaned_indexes_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_missing_remove_orphan_prune(redis_url),
    )
    .await
    .expect("Redis missing remove orphan-prune integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_saves_stacktrace_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_save_stacktrace(redis_url))
        .await
        .expect("Redis save stacktrace integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_records_job_metrics_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_job_metrics(redis_url))
        .await
        .expect("Redis job metrics integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_renews_leases_in_bulk_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_bulk_lease_renewal(redis_url))
        .await
        .expect("Redis bulk lease renewal integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_reports_maxed_active_limit_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_maxed_active_limit(redis_url))
        .await
        .expect("Redis maxed active-limit integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_job_worker_uses_bulk_lease_renewal_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_worker_bulk_lease_renewal(redis_url),
    )
    .await
    .expect("Redis worker bulk lease renewal integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_matches_bullmq_priority_update_guards_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_priority_update_limit(redis_url),
    )
    .await
    .expect("Redis priority-update guard integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_allows_terminal_progress_updates_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_terminal_progress_update(redis_url),
    )
    .await
    .expect("Redis terminal progress integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_allows_terminal_data_updates_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_terminal_data_update(redis_url),
    )
    .await
    .expect("Redis terminal data integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_emits_reschedule_delayed_events_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_reschedule_delayed_event(redis_url),
    )
    .await
    .expect("Redis reschedule delayed-event integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_emits_delay_active_delayed_events_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_delay_active_delayed_event(redis_url),
    )
    .await
    .expect("Redis delay-active delayed-event integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_release_active_pushes_back_waiting_jobs_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_release_active_waiting_front(redis_url),
    )
    .await
    .expect("Redis release-active waiting-front integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_guards_manual_active_transitions_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_manual_active_transition_guards(redis_url),
    )
    .await
    .expect("Redis manual active-transition guard integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_auto_removes_terminal_jobs_and_logs_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_terminal_auto_remove_cleanup(redis_url),
    )
    .await
    .expect("Redis terminal auto-remove cleanup integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_keeps_log_list_and_snapshot_consistent_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_log_list_snapshot_consistency(redis_url),
    )
    .await
    .expect("Redis log-list snapshot consistency integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_skips_stale_waiting_indexes_while_claiming_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_stale_waiting_claim_cleanup(redis_url),
    )
    .await
    .expect("Redis stale waiting-index claim cleanup integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_cleans_completed_jobs_by_age_limit_and_millis_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_completed_clean_retention_and_millis(redis_url),
    )
    .await
    .expect("Redis completed clean retention integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_retries_completed_jobs_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_completed_retry(redis_url))
        .await
        .expect("Redis completed retry integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_rejects_non_delayed_promote_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_promote_state_gate(redis_url))
        .await
        .expect("Redis promote state-gate integration test timed out")
        .unwrap();
}

async fn run_priority_update_limit(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "priority-update-limit")
        .expect("valid Redis URL should build the priority-update-limit queue");
    let first = queue
        .add_job(
            "first".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_priority(50),
        )
        .await
        .expect("first priority-limit job should add");
    let second = queue
        .add_job(
            "second".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_priority(60),
        )
        .await
        .expect("second priority-limit job should add");

    let error = queue
        .update_priority(&second.id, MAX_JOB_PRIORITY + 1)
        .await
        .expect_err("priority above BullMQ limit should reject");
    assert!(matches!(error, LaneError::ConfigError(_)));

    let stored = queue
        .get_job(&second.id)
        .await
        .expect("stored priority-limit job should load")
        .expect("stored priority-limit job should remain");
    assert_eq!(stored.priority, 60);
    assert_eq!(stored.options.priority, 60);

    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let priority_sixty_zcount: usize = redis::cmd("ZCOUNT")
        .arg(format!("{namespace}:priority-update-limit:waiting"))
        .arg(60_000_000_000_000_f64)
        .arg(60_999_999_999_999_f64)
        .query_async(&mut conn)
        .await?;
    assert_eq!(priority_sixty_zcount, 1);

    let claimed = queue
        .claim_next(
            "worker-priority-update-limit".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("priority-limit claim should return")
        .expect("priority-limit queue should still have waiting work");
    assert_eq!(claimed.id, first.id);
    assert_ne!(claimed.id, second.id);
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("priority-limit claimed job should complete");

    let _: usize = conn
        .zadd(
            format!("{namespace}:priority-update-limit:waiting"),
            &first.id,
            0.0,
        )
        .await?;
    let terminal_update = queue
        .update_priority(&first.id, 5)
        .await
        .expect("terminal priority update should update the stored snapshot");
    assert_eq!(terminal_update.state, JobState::Completed);
    assert_eq!(terminal_update.priority, 5);
    assert_eq!(terminal_update.options.priority, 5);
    let stale_terminal_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:priority-update-limit:waiting"),
            &first.id,
        )
        .await?;
    assert!(stale_terminal_score.is_none());

    let next_claimed = queue
        .claim_next(
            "worker-priority-update-limit-next".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("priority-limit next claim should return")
        .expect("second priority-limit job should remain waiting");
    assert_eq!(next_claimed.id, second.id);
    assert_ne!(next_claimed.id, first.id);

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_reschedule_delayed_event(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "reschedule-event")
        .expect("valid Redis URL should build the reschedule-event queue");
    let job = queue
        .add_job(
            "task".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("reschedule event job should add");
    let rescheduled = queue
        .reschedule_job(&job.id, Duration::from_secs(2), ts(1_000))
        .await
        .expect("delayed job should reschedule");
    assert_eq!(rescheduled.state, JobState::Delayed);
    assert_eq!(rescheduled.scheduled_at, ts(3_000));

    let events = queue
        .read_events("-", "+", 100)
        .await
        .expect("reschedule event stream should load");
    let delayed = events
        .iter()
        .rev()
        .find(|event| event.event == "delayed" && event.job_id.as_deref() == Some(job.id.as_str()))
        .expect("reschedule should emit a delayed event");
    assert_eq!(delayed.fields.get("delay"), Some(&serde_json::json!(3_000)));

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_delay_active_delayed_event(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "delay-active-event")
        .expect("valid Redis URL should build the delay-active-event queue");
    let job = queue
        .add_job("task".to_string(), serde_json::json!({}), JobOptions::new())
        .await
        .expect("delay-active event job should add");
    let claimed = queue
        .claim_next(
            "worker-delay-active-event".to_string(),
            Duration::from_secs(30),
            ts(1_000),
        )
        .await
        .expect("delay-active event claim should return")
        .expect("delay-active event job should be claimable");
    let delayed = queue
        .delay_active_job(
            &job.id,
            lock_token(&claimed),
            Duration::from_secs(2),
            ts(1_000),
        )
        .await
        .expect("active job should delay");
    assert_eq!(delayed.state, JobState::Delayed);
    assert_eq!(delayed.scheduled_at, ts(3_000));

    let events = queue
        .read_events("-", "+", 100)
        .await
        .expect("delay-active event stream should load");
    let delayed_event = events
        .iter()
        .rev()
        .find(|event| event.event == "delayed" && event.job_id.as_deref() == Some(job.id.as_str()))
        .expect("delay_active_job should emit a delayed event");
    assert_eq!(
        delayed_event.fields.get("delay"),
        Some(&serde_json::json!(3_000))
    );
    assert_eq!(delayed_event.prev, Some(JobState::Active));

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_release_active_waiting_front(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "release-active-front")
        .expect("valid Redis URL should build the release-active-front queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let released_b = queue
        .add_job(
            "released-b".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("release-front:b")
                .with_priority(5),
        )
        .await
        .expect("released-b job should add");
    let released_a = queue
        .add_job(
            "released-a".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("release-front:a")
                .with_priority(5),
        )
        .await
        .expect("released-a job should add");
    let waiting = queue
        .add_job(
            "waiting".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("release-front:waiting")
                .with_priority(5),
        )
        .await
        .expect("waiting job should add");

    let claimed_b = queue
        .claim_next(
            "worker-release-front-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("released-b claim should return")
        .expect("released-b job should be claimable");
    assert_eq!(claimed_b.id, released_b.id);
    let claimed_a = queue
        .claim_next(
            "worker-release-front-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("released-a claim should return")
        .expect("released-a job should be claimable");
    assert_eq!(claimed_a.id, released_a.id);

    let released_b = queue
        .release_active_job(&claimed_b.id, lock_token(&claimed_b), Utc::now())
        .await
        .expect("released-b active job should release");
    let released_a = queue
        .release_active_job(&claimed_a.id, lock_token(&claimed_a), Utc::now())
        .await
        .expect("released-a active job should release");
    assert_eq!(released_b.enqueued_seq, 0);
    assert_eq!(released_a.enqueued_seq, 0);

    let waiting_key = format!("{namespace}:release-active-front:waiting");
    let released_b_score: f64 = conn.zscore(&waiting_key, &released_b.id).await?;
    let released_a_score: f64 = conn.zscore(&waiting_key, &released_a.id).await?;
    assert_eq!(released_b_score, 5_000_000_000_000_f64);
    assert_eq!(released_a_score, 5_000_000_000_000_f64);
    let waiting_ids: Vec<String> = redis::cmd("ZRANGE")
        .arg(&waiting_key)
        .arg(0)
        .arg(-1)
        .query_async(&mut conn)
        .await?;
    assert_eq!(
        waiting_ids,
        vec![
            released_a.id.clone(),
            released_b.id.clone(),
            waiting.id.clone()
        ]
    );

    for expected in [&released_a, &released_b, &waiting] {
        let claimed = queue
            .claim_next(
                "worker-release-front-next".to_string(),
                Duration::from_secs(30),
                Utc::now(),
            )
            .await
            .expect("release-front claim should return")
            .expect("release-front job should be claimable");
        assert_eq!(claimed.id, expected.id);
    }

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_manual_active_transition_guards(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "manual-active-guards")
        .expect("valid Redis URL should build the manual-active-guards queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let active_key = format!("{namespace}:manual-active-guards:active");
    let delayed_key = format!("{namespace}:manual-active-guards:delayed");
    let waiting_key = format!("{namespace}:manual-active-guards:waiting");
    let stalled_key = format!("{namespace}:manual-active-guards:stalled");

    let transition = queue
        .add_job(
            "manual-active-transition".to_string(),
            serde_json::json!({ "kind": "delay" }),
            JobOptions::new().with_priority(4),
        )
        .await
        .expect("manual active-transition job should add");
    let claimed_transition = queue
        .claim_next(
            "worker-manual-active-transition".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("manual active-transition claim should return")
        .expect("manual active-transition job should claim");
    assert_eq!(claimed_transition.id, transition.id);

    let active_remove = queue
        .remove_job(&claimed_transition.id)
        .await
        .expect_err("active leased jobs must not be removed");
    assert!(matches!(active_remove, LaneError::JobLeaseConflict(_)));
    let active_after_failed_remove: Option<f64> =
        conn.zscore(&active_key, &claimed_transition.id).await?;
    assert!(active_after_failed_remove.is_some());
    assert_eq!(
        queue
            .get_job(&claimed_transition.id)
            .await
            .expect("active job should load after failed remove")
            .expect("active job should still exist after failed remove")
            .state,
        JobState::Active
    );

    let unlocked_active = queue
        .add_job(
            "manual-active-unlocked-remove".to_string(),
            serde_json::json!({ "kind": "lost-lock" }),
            JobOptions::new().with_priority(5),
        )
        .await
        .expect("unlocked active job should add");
    let unlocked_active_claim = queue
        .claim_next(
            "worker-manual-active-unlocked-remove".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("unlocked active claim should return")
        .expect("unlocked active job should claim");
    assert_eq!(unlocked_active_claim.id, unlocked_active.id);
    queue
        .add_log(
            &unlocked_active.id,
            "active removal log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("unlocked active removal log should append");
    let unlocked_lock_key = format!(
        "{namespace}:manual-active-guards:locks:{}",
        unlocked_active.id
    );
    let unlocked_logs_key = format!(
        "{namespace}:manual-active-guards:logs:{}",
        unlocked_active.id
    );
    let removed_unlocked_lock: usize = conn.del(&unlocked_lock_key).await?;
    assert_eq!(removed_unlocked_lock, 1);
    let stalled_unlocked: usize = conn.sadd(&stalled_key, &unlocked_active.id).await?;
    assert_eq!(stalled_unlocked, 1);

    let removed_unlocked = queue
        .remove_job(&unlocked_active.id)
        .await
        .expect("unlocked active job should remove")
        .expect("unlocked active job should be returned");
    assert_eq!(removed_unlocked.id, unlocked_active.id);
    assert_eq!(removed_unlocked.state, JobState::Active);
    assert!(queue
        .get_job(&unlocked_active.id)
        .await
        .expect("unlocked active lookup should return")
        .is_none());
    let unlocked_active_score_after: Option<f64> =
        conn.zscore(&active_key, &unlocked_active.id).await?;
    assert!(unlocked_active_score_after.is_none());
    let unlocked_logs_len: usize = conn.llen(&unlocked_logs_key).await?;
    assert_eq!(unlocked_logs_len, 0);
    let unlocked_stalled_after: bool = conn.sismember(&stalled_key, &unlocked_active.id).await?;
    assert!(!unlocked_stalled_after);

    let wrong_delay_token = queue
        .delay_active_job(
            &claimed_transition.id,
            "wrong-token",
            Duration::from_millis(50),
            Utc::now(),
        )
        .await
        .expect_err("wrong token must not delay an active job");
    assert!(matches!(wrong_delay_token, LaneError::JobLeaseConflict(_)));
    let delayed_again = queue
        .delay_active_job(
            &claimed_transition.id,
            lock_token(&claimed_transition),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("active job should move back to delayed");
    assert_eq!(delayed_again.state, JobState::Delayed);
    assert_eq!(delayed_again.options.delay, Some(Duration::from_secs(30)));
    assert!(delayed_again.worker_id.is_none());
    assert!(delayed_again.lease_expires_at.is_none());
    let active_after_delay: Option<f64> = conn.zscore(&active_key, &claimed_transition.id).await?;
    assert!(active_after_delay.is_none());
    let delayed_after_delay: Option<f64> =
        conn.zscore(&delayed_key, &claimed_transition.id).await?;
    assert!(delayed_after_delay.is_some());
    let lock_after_delay_exists: usize = conn
        .exists(format!(
            "{namespace}:manual-active-guards:locks:{}",
            claimed_transition.id
        ))
        .await?;
    assert_eq!(lock_after_delay_exists, 0);
    let complete_after_delay = queue
        .complete_job(
            &claimed_transition.id,
            lock_token(&claimed_transition),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect_err("delayed job must not complete with the old active token");
    assert!(matches!(
        complete_after_delay,
        LaneError::JobStateConflict(_)
    ));
    assert!(queue
        .claim_next(
            "worker-manual-active-delayed-early".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("early delayed-again claim should return")
        .is_none());
    assert_eq!(
        queue
            .promote_due_jobs(delayed_again.scheduled_at + chrono::Duration::milliseconds(1))
            .await
            .expect("delayed-again job should promote"),
        1
    );
    let reclaimed_delayed = queue
        .claim_next(
            "worker-manual-active-delayed-again".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("delayed-again claim should return")
        .expect("delayed-again job should be claimable");
    assert_eq!(reclaimed_delayed.id, claimed_transition.id);
    queue
        .complete_job(
            &reclaimed_delayed.id,
            lock_token(&reclaimed_delayed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("delayed-again job should complete");

    let release_active = queue
        .add_job(
            "manual-active-release".to_string(),
            serde_json::json!({ "kind": "yield" }),
            JobOptions::new().with_priority(3),
        )
        .await
        .expect("release-active job should add");
    let claimed_release = queue
        .claim_next(
            "worker-manual-active-release".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("release-active claim should return")
        .expect("release-active job should be claimable");
    assert_eq!(claimed_release.id, release_active.id);
    let wrong_release_token = queue
        .release_active_job(&claimed_release.id, "wrong-token", Utc::now())
        .await
        .expect_err("wrong token must not release an active job");
    assert!(matches!(
        wrong_release_token,
        LaneError::JobLeaseConflict(_)
    ));
    let released_active = queue
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
    let release_active_score: Option<f64> = conn.zscore(&active_key, &claimed_release.id).await?;
    assert!(release_active_score.is_none());
    let release_waiting_score: Option<f64> = conn.zscore(&waiting_key, &claimed_release.id).await?;
    assert!(release_waiting_score.is_some());
    let release_lock_exists: usize = conn
        .exists(format!(
            "{namespace}:manual-active-guards:locks:{}",
            claimed_release.id
        ))
        .await?;
    assert_eq!(release_lock_exists, 0);
    let complete_after_release = queue
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
    let reclaimed_release = queue
        .claim_next(
            "worker-manual-active-release-again".to_string(),
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
    queue
        .complete_job(
            &reclaimed_release.id,
            lock_token(&reclaimed_release),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("released job should complete after reclaim");

    let stale_active_delay = queue
        .add_job(
            "manual-active-stale-delay".to_string(),
            serde_json::json!({ "kind": "stale-active-index" }),
            JobOptions::new(),
        )
        .await
        .expect("stale active delay job should add");
    let stale_active_claim = queue
        .claim_next(
            "worker-manual-active-stale-delay".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("stale active delay claim should return")
        .expect("stale active delay job should be claimable");
    assert_eq!(stale_active_claim.id, stale_active_delay.id);
    let stale_removed_from_active: usize = conn.zrem(&active_key, &stale_active_claim.id).await?;
    assert_eq!(stale_removed_from_active, 1);
    let stale_active_delay_error = queue
        .delay_active_job(
            &stale_active_claim.id,
            lock_token(&stale_active_claim),
            Duration::from_millis(50),
            Utc::now(),
        )
        .await
        .expect_err("missing active zset membership should reject active delay");
    assert!(matches!(
        stale_active_delay_error,
        LaneError::JobStateConflict(_)
    ));
    let stale_active_lock_exists: usize = conn
        .exists(format!(
            "{namespace}:manual-active-guards:locks:{}",
            stale_active_claim.id
        ))
        .await?;
    assert_eq!(stale_active_lock_exists, 1);
    queue
        .complete_job(
            &stale_active_claim.id,
            lock_token(&stale_active_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("stale active delay job should still complete with valid lock");

    let stale_reschedule = queue
        .add_job(
            "manual-active-stale-reschedule".to_string(),
            serde_json::json!({ "kind": "stale-delayed-index" }),
            JobOptions::new().with_delay(Duration::from_secs(30)),
        )
        .await
        .expect("stale reschedule job should add");
    let stale_removed_from_delayed: usize = conn.zrem(&delayed_key, &stale_reschedule.id).await?;
    assert_eq!(stale_removed_from_delayed, 1);
    let stale_reschedule_error = queue
        .reschedule_job(&stale_reschedule.id, Duration::from_millis(50), Utc::now())
        .await
        .expect_err("missing delayed zset membership should reject reschedule");
    assert!(matches!(
        stale_reschedule_error,
        LaneError::JobStateConflict(_)
    ));

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_terminal_auto_remove_cleanup(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue_name = "terminal-auto-remove";
    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, queue_name)
        .expect("valid Redis URL should build the terminal-auto-remove queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let remove_on_complete = queue
        .add_job(
            "remove-on-complete".to_string(),
            serde_json::json!({ "path": "complete" }),
            JobOptions::new().remove_on_complete(true),
        )
        .await
        .expect("remove-on-complete job should add");
    queue
        .add_log(
            &remove_on_complete.id,
            "complete cleanup log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("remove-on-complete log should append");
    let remove_on_complete_claim = queue
        .claim_next(
            "worker-auto-remove-complete".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("remove-on-complete claim should return")
        .expect("remove-on-complete job should be claimable");
    assert_eq!(remove_on_complete_claim.id, remove_on_complete.id);
    let completed_snapshot = queue
        .complete_job(
            &remove_on_complete_claim.id,
            lock_token(&remove_on_complete_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("remove-on-complete job should complete");
    assert_eq!(completed_snapshot.state, JobState::Completed);
    assert_terminal_auto_removed(
        &queue,
        &mut conn,
        &namespace,
        queue_name,
        &remove_on_complete.id,
    )
    .await?;

    let remove_on_fail = queue
        .add_job(
            "remove-on-fail".to_string(),
            serde_json::json!({ "path": "fail" }),
            JobOptions::new().remove_on_fail(true),
        )
        .await
        .expect("remove-on-fail job should add");
    queue
        .add_log(
            &remove_on_fail.id,
            "fail cleanup log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("remove-on-fail log should append");
    let remove_on_fail_claim = queue
        .claim_next(
            "worker-auto-remove-fail".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("remove-on-fail claim should return")
        .expect("remove-on-fail job should be claimable");
    assert_eq!(remove_on_fail_claim.id, remove_on_fail.id);
    let failed_snapshot = queue
        .fail_job(
            &remove_on_fail_claim.id,
            lock_token(&remove_on_fail_claim),
            "terminal failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("remove-on-fail job should fail");
    assert_eq!(failed_snapshot.state, JobState::Failed);
    assert_terminal_auto_removed(
        &queue,
        &mut conn,
        &namespace,
        queue_name,
        &remove_on_fail.id,
    )
    .await?;

    let remove_on_stalled_fail = queue
        .add_job(
            "remove-on-stalled-fail".to_string(),
            serde_json::json!({ "path": "stalled" }),
            JobOptions::new()
                .remove_on_fail(true)
                .with_max_stalled_count(0),
        )
        .await
        .expect("remove-on-stalled-fail job should add");
    queue
        .add_log(
            &remove_on_stalled_fail.id,
            "stalled cleanup log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("remove-on-stalled-fail log should append");
    let remove_on_stalled_claim = queue
        .claim_next(
            "worker-auto-remove-stalled".to_string(),
            Duration::from_millis(80),
            Utc::now(),
        )
        .await
        .expect("remove-on-stalled-fail claim should return")
        .expect("remove-on-stalled-fail job should be claimable");
    assert_eq!(remove_on_stalled_claim.id, remove_on_stalled_fail.id);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("remove-on-stalled-fail recovery should mark candidate"),
        0
    );
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("remove-on-stalled-fail recovery should terminally remove"),
        1
    );
    assert_terminal_auto_removed(
        &queue,
        &mut conn,
        &namespace,
        queue_name,
        &remove_on_stalled_fail.id,
    )
    .await?;

    cleanup_namespace(&redis_url, &namespace).await
}

async fn assert_terminal_auto_removed(
    queue: &RedisJobQueue,
    conn: &mut redis::aio::ConnectionManager,
    namespace: &str,
    queue_name: &str,
    job_id: &str,
) -> redis::RedisResult<()> {
    assert!(queue
        .get_job(job_id)
        .await
        .expect("auto-removed job lookup should return")
        .is_none());

    let stored: Option<String> = conn
        .hget(format!("{namespace}:{queue_name}:jobs"), job_id)
        .await?;
    assert!(stored.is_none());

    for state in [
        "waiting",
        "delayed",
        "active",
        "waiting_children",
        "completed",
        "failed",
    ] {
        let score: Option<f64> = conn
            .zscore(format!("{namespace}:{queue_name}:{state}"), job_id)
            .await?;
        assert!(
            score.is_none(),
            "auto-removed job should not remain in the {state} index"
        );
    }

    let lock_exists: usize = conn
        .exists(format!("{namespace}:{queue_name}:locks:{job_id}"))
        .await?;
    assert_eq!(lock_exists, 0);
    let logs_len: usize = conn
        .llen(format!("{namespace}:{queue_name}:logs:{job_id}"))
        .await?;
    assert_eq!(logs_len, 0);
    let stalled_member: bool = conn
        .sismember(format!("{namespace}:{queue_name}:stalled"), job_id)
        .await?;
    assert!(!stalled_member);

    Ok(())
}

async fn run_log_list_snapshot_consistency(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue_name = "log-consistency";
    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, queue_name)
        .expect("valid Redis URL should build the log-consistency queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let job = queue
        .add_job(
            "log-owner".to_string(),
            serde_json::json!({ "kind": "logs" }),
            JobOptions::new(),
        )
        .await
        .expect("log owner job should add");
    let logs_key = format!("{namespace}:{queue_name}:logs:{}", job.id);
    let jobs_key = format!("{namespace}:{queue_name}:jobs");

    let after_first = queue
        .add_log(&job.id, "first".to_string(), 10, ts(1_000))
        .await
        .expect("first log should append");
    assert_log_lines(&after_first.logs, &["first"]);
    let after_second = queue
        .add_log(&job.id, "second".to_string(), 2, ts(2_000))
        .await
        .expect("second log should append");
    assert_log_lines(&after_second.logs, &["first", "second"]);
    let after_third = queue
        .add_log(&job.id, "third".to_string(), 2, ts(3_000))
        .await
        .expect("third log should append and trim");
    assert_log_lines(&after_third.logs, &["second", "third"]);

    let raw_logs: Vec<String> = conn.lrange(&logs_key, 0, -1).await?;
    assert_log_lines(&decode_raw_log_entries(raw_logs), &["second", "third"]);
    let stored_after_adds = load_raw_job_value(&mut conn, &jobs_key, &job.id).await?;
    assert_log_value_lines(&stored_after_adds, &["second", "third"]);

    let ascending = queue
        .get_job_logs(&job.id, 0, -1, true)
        .await
        .expect("ascending logs should read");
    assert_eq!(ascending.count, 2);
    assert_log_lines(&ascending.logs, &["second", "third"]);
    let newest = queue
        .get_job_logs(&job.id, 0, 0, false)
        .await
        .expect("newest log should read");
    assert_eq!(newest.count, 2);
    assert_log_lines(&newest.logs, &["third"]);

    let kept = queue
        .clear_job_logs(&job.id, 1)
        .await
        .expect("logs should trim to the newest entry");
    assert_eq!(kept.count, 1);
    assert_log_lines(&kept.logs, &["third"]);
    let raw_after_keep: Vec<String> = conn.lrange(&logs_key, 0, -1).await?;
    assert_log_lines(&decode_raw_log_entries(raw_after_keep), &["third"]);
    let stored_after_keep = load_raw_job_value(&mut conn, &jobs_key, &job.id).await?;
    assert_log_value_lines(&stored_after_keep, &["third"]);

    let cleared = queue
        .clear_job_logs(&job.id, 0)
        .await
        .expect("logs should clear");
    assert_eq!(cleared.count, 0);
    assert!(cleared.logs.is_empty());
    let logs_len_after_clear: usize = conn.llen(&logs_key).await?;
    assert_eq!(logs_len_after_clear, 0);
    let stored_after_clear = load_raw_job_value(&mut conn, &jobs_key, &job.id).await?;
    assert_log_value_lines(&stored_after_clear, &[]);

    let missing = queue
        .clear_job_logs("missing-log-owner", 1)
        .await
        .expect("missing log owner clear should return an empty page");
    assert_eq!(missing.count, 0);
    assert!(missing.logs.is_empty());

    cleanup_namespace(&redis_url, &namespace).await
}

fn decode_raw_log_entries(raw_logs: Vec<String>) -> Vec<JobLogEntry> {
    raw_logs
        .iter()
        .map(|raw| serde_json::from_str::<JobLogEntry>(raw).expect("Redis log JSON should decode"))
        .collect()
}

async fn load_raw_job_value(
    conn: &mut redis::aio::ConnectionManager,
    jobs_key: &str,
    job_id: &str,
) -> redis::RedisResult<serde_json::Value> {
    let raw: String = conn.hget(jobs_key, job_id).await?;
    Ok(serde_json::from_str(&raw).expect("raw Redis job JSON should decode"))
}

fn assert_log_lines(logs: &[JobLogEntry], expected: &[&str]) {
    assert_eq!(
        logs.iter()
            .map(|entry| entry.line.as_str())
            .collect::<Vec<_>>(),
        expected
    );
}

fn assert_log_value_lines(job: &serde_json::Value, expected: &[&str]) {
    let raw_logs = job.get("logs").expect("raw Redis job should store logs");
    if expected.is_empty() && raw_logs.as_object().is_some_and(|object| object.is_empty()) {
        return;
    }
    let logs = raw_logs
        .as_array()
        .expect("raw Redis job should store non-empty logs as an array");
    assert_eq!(
        logs.iter()
            .map(|entry| {
                entry
                    .get("line")
                    .and_then(|line| line.as_str())
                    .expect("raw Redis log should store a line")
            })
            .collect::<Vec<_>>(),
        expected
    );
}

async fn run_stale_waiting_claim_cleanup(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue_name = "claim-stale-focused";
    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, queue_name)
        .expect("valid Redis URL should build the claim-stale-focused queue");
    let completed = queue
        .add_job(
            "stale-completed".to_string(),
            serde_json::json!({ "kind": "completed" }),
            JobOptions::new(),
        )
        .await
        .expect("stale completed job should add");
    let waiting = queue
        .add_job(
            "real-waiting".to_string(),
            serde_json::json!({ "kind": "waiting" }),
            JobOptions::new(),
        )
        .await
        .expect("real waiting job should add");

    let completed_claim = queue
        .claim_next(
            "worker-stale-completed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("stale completed claim should return")
        .expect("stale completed job should be claimable");
    assert_eq!(completed_claim.id, completed.id);
    queue
        .complete_job(
            &completed_claim.id,
            lock_token(&completed_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("stale completed job should complete");

    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let waiting_key = format!("{namespace}:{queue_name}:waiting");
    let missing_id = "missing-waiting-index";
    let _: usize = conn.zadd(&waiting_key, &completed.id, 0.0).await?;
    let _: usize = conn.zadd(&waiting_key, missing_id, 0.5).await?;

    let claimed = queue
        .claim_next(
            "worker-real-waiting".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("claim should skip stale waiting indexes")
        .expect("real waiting job should claim");
    assert_eq!(claimed.id, waiting.id);

    let stale_completed_waiting_score: Option<f64> =
        conn.zscore(&waiting_key, &completed.id).await?;
    assert!(stale_completed_waiting_score.is_none());
    let missing_waiting_score: Option<f64> = conn.zscore(&waiting_key, missing_id).await?;
    assert!(missing_waiting_score.is_none());
    let completed_after_claim = queue
        .get_job(&completed.id)
        .await
        .expect("stale completed job should load")
        .expect("stale completed job should still exist");
    assert_eq!(completed_after_claim.state, JobState::Completed);
    assert!(queue
        .get_job(missing_id)
        .await
        .expect("missing stale waiting job lookup should return")
        .is_none());

    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("real waiting job should complete");

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_completed_clean_retention_and_millis(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue_name = "clean-completed-focused";
    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, queue_name)
        .expect("valid Redis URL should build the clean-completed-focused queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let old_a = queue
        .add_job(
            "old-a".to_string(),
            serde_json::json!({ "age": "oldest" }),
            JobOptions::new(),
        )
        .await
        .expect("old-a clean job should add");
    let old_b = queue
        .add_job(
            "old-b".to_string(),
            serde_json::json!({ "age": "old" }),
            JobOptions::new(),
        )
        .await
        .expect("old-b clean job should add");
    let fresh = queue
        .add_job(
            "fresh".to_string(),
            serde_json::json!({ "age": "fresh" }),
            JobOptions::new(),
        )
        .await
        .expect("fresh clean job should add");
    queue
        .add_log(&old_a.id, "clean me".to_string(), 10, Utc::now())
        .await
        .expect("old-a clean log should append");

    let old_a_claim = queue
        .claim_next(
            "worker-clean-old-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("old-a claim should return")
        .expect("old-a should claim");
    let old_b_claim = queue
        .claim_next(
            "worker-clean-old-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("old-b claim should return")
        .expect("old-b should claim");
    let fresh_claim = queue
        .claim_next(
            "worker-clean-fresh".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("fresh claim should return")
        .expect("fresh should claim");
    assert_eq!(old_a_claim.id, old_a.id);
    assert_eq!(old_b_claim.id, old_b.id);
    assert_eq!(fresh_claim.id, fresh.id);

    let clean_now = ts(30_000);
    queue
        .complete_job(
            &old_a_claim.id,
            lock_token(&old_a_claim),
            serde_json::json!({ "ok": true }),
            ts(10_000),
        )
        .await
        .expect("old-a should complete");
    queue
        .complete_job(
            &old_b_claim.id,
            lock_token(&old_b_claim),
            serde_json::json!({ "ok": true }),
            ts(11_000),
        )
        .await
        .expect("old-b should complete");
    queue
        .complete_job(
            &fresh_claim.id,
            lock_token(&fresh_claim),
            serde_json::json!({ "ok": true }),
            clean_now,
        )
        .await
        .expect("fresh should complete");

    let first_cleaned = queue
        .clean_jobs(JobState::Completed, Duration::from_secs(5), 1, clean_now)
        .await
        .expect("first completed clean should run");
    assert_eq!(first_cleaned.len(), 1);
    assert_eq!(first_cleaned[0].id, old_a.id);
    assert_cleaned_job_removed(&mut conn, &namespace, queue_name, &old_a.id).await?;
    assert_completed_index_present(&mut conn, &namespace, queue_name, &old_b.id).await?;
    assert_completed_index_present(&mut conn, &namespace, queue_name, &fresh.id).await?;

    let second_cleaned = queue
        .clean_jobs(JobState::Completed, Duration::from_secs(5), 10, clean_now)
        .await
        .expect("second completed clean should run");
    assert_eq!(
        second_cleaned
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>(),
        vec![old_b.id.as_str()]
    );
    assert_cleaned_job_removed(&mut conn, &namespace, queue_name, &old_b.id).await?;
    assert_completed_index_present(&mut conn, &namespace, queue_name, &fresh.id).await?;

    let millis_queue_name = "clean-millis-focused";
    let millis_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, millis_queue_name)
        .expect("valid Redis URL should build the clean-millis-focused queue");
    let millis_a_id = format!("{namespace}:clean-millis-focused:a");
    let millis_b_id = format!("{namespace}:clean-millis-focused:b");
    millis_queue
        .add_job(
            "millis-a".to_string(),
            serde_json::json!({ "format": "three-digits" }),
            JobOptions::new().with_job_id(millis_a_id.clone()),
        )
        .await
        .expect("millis-a should add");
    millis_queue
        .add_job(
            "millis-b".to_string(),
            serde_json::json!({ "format": "one-digit" }),
            JobOptions::new().with_job_id(millis_b_id.clone()),
        )
        .await
        .expect("millis-b should add");
    let millis_a = millis_queue
        .claim_next(
            "worker-millis-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("millis-a claim should return")
        .expect("millis-a should claim");
    let millis_b = millis_queue
        .claim_next(
            "worker-millis-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("millis-b claim should return")
        .expect("millis-b should claim");
    let same_finished_at = ts(1_100);
    millis_queue
        .complete_job(
            &millis_a.id,
            lock_token(&millis_a),
            serde_json::json!({}),
            same_finished_at,
        )
        .await
        .expect("millis-a should complete");
    millis_queue
        .complete_job(
            &millis_b.id,
            lock_token(&millis_b),
            serde_json::json!({}),
            same_finished_at,
        )
        .await
        .expect("millis-b should complete");

    let millis_jobs_key = format!("{namespace}:{millis_queue_name}:jobs");
    let raw_a: String = conn.hget(&millis_jobs_key, &millis_a_id).await?;
    let raw_b: String = conn.hget(&millis_jobs_key, &millis_b_id).await?;
    let mut value_a: serde_json::Value =
        serde_json::from_str(&raw_a).expect("millis-a raw job should be JSON");
    let mut value_b: serde_json::Value =
        serde_json::from_str(&raw_b).expect("millis-b raw job should be JSON");
    value_a["finished_at"] = serde_json::Value::String("1970-01-01T00:00:01.100+00:00".into());
    value_b["finished_at"] = serde_json::Value::String("1970-01-01T00:00:01.1+00:00".into());
    let _: usize = conn
        .hset(
            &millis_jobs_key,
            &millis_a_id,
            serde_json::to_string(&value_a).expect("millis-a raw job should encode"),
        )
        .await?;
    let _: usize = conn
        .hset(
            &millis_jobs_key,
            &millis_b_id,
            serde_json::to_string(&value_b).expect("millis-b raw job should encode"),
        )
        .await?;

    let first_millis_cleaned = millis_queue
        .clean_jobs(JobState::Completed, Duration::ZERO, 1, same_finished_at)
        .await
        .expect("millisecond clean should run");
    assert_eq!(first_millis_cleaned.len(), 1);
    assert_eq!(first_millis_cleaned[0].id, millis_a_id);
    assert_cleaned_job_removed(&mut conn, &namespace, millis_queue_name, &millis_a_id).await?;
    assert_completed_index_present(&mut conn, &namespace, millis_queue_name, &millis_b_id).await?;

    cleanup_namespace(&redis_url, &namespace).await
}

async fn assert_cleaned_job_removed(
    conn: &mut redis::aio::ConnectionManager,
    namespace: &str,
    queue_name: &str,
    job_id: &str,
) -> redis::RedisResult<()> {
    let stored: Option<String> = conn
        .hget(format!("{namespace}:{queue_name}:jobs"), job_id)
        .await?;
    assert!(stored.is_none());
    let completed_score: Option<f64> = conn
        .zscore(format!("{namespace}:{queue_name}:completed"), job_id)
        .await?;
    assert!(completed_score.is_none());
    let logs_len: usize = conn
        .llen(format!("{namespace}:{queue_name}:logs:{job_id}"))
        .await?;
    assert_eq!(logs_len, 0);
    Ok(())
}

async fn assert_completed_index_present(
    conn: &mut redis::aio::ConnectionManager,
    namespace: &str,
    queue_name: &str,
    job_id: &str,
) -> redis::RedisResult<()> {
    let completed_score: Option<f64> = conn
        .zscore(format!("{namespace}:{queue_name}:completed"), job_id)
        .await?;
    assert!(completed_score.is_some());
    Ok(())
}

async fn run_completed_retry(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "completed-retry")
        .expect("valid Redis URL should build the completed-retry queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let job = queue
        .add_job(
            "completed-retry".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("completed retry job should add");
    let claimed = queue
        .claim_next(
            "worker-completed-retry-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("completed retry claim should return")
        .expect("completed retry job should be claimable");
    assert_eq!(claimed.id, job.id);
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("completed retry job should complete");

    let retried = queue
        .retry_job(&job.id, Utc::now())
        .await
        .expect("completed job should retry");
    assert_eq!(retried.state, JobState::Waiting);
    assert!(retried.return_value.is_none());
    assert!(retried.finished_at.is_none());
    assert_eq!(
        queue
            .get_job_finished_result(&job.id)
            .await
            .expect("completed retry finished result should load"),
        Some(JobFinishedResult::NotFinished)
    );
    let completed_score: Option<f64> = conn
        .zscore(format!("{namespace}:completed-retry:completed"), &job.id)
        .await?;
    assert!(completed_score.is_none());
    let waiting_score: Option<f64> = conn
        .zscore(format!("{namespace}:completed-retry:waiting"), &job.id)
        .await?;
    assert!(waiting_score.is_some());
    let events = queue
        .read_events("-", "+", 100)
        .await
        .expect("completed retry events should load");
    let waiting_event = events
        .iter()
        .rev()
        .find(|event| event.event == "waiting" && event.job_id.as_deref() == Some(job.id.as_str()))
        .expect("completed retry should emit waiting event");
    assert_eq!(waiting_event.prev, Some(JobState::Completed));

    let reclaimed = queue
        .claim_next(
            "worker-completed-retry-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("completed retry reclaim should return")
        .expect("completed retry job should be claimable again");
    assert_eq!(reclaimed.id, job.id);
    queue
        .complete_job(
            &reclaimed.id,
            lock_token(&reclaimed),
            serde_json::json!({ "ok": "again" }),
            Utc::now(),
        )
        .await
        .expect("completed retry reclaimed job should complete");

    let flow_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "completed-flow-retry")
        .expect("valid Redis URL should build the completed-flow-retry queue");
    let flow = flow_queue
        .add_flow(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![JobSpec::new("child", serde_json::json!({ "child": true }))
                .with_options(JobOptions::new().with_priority(1))],
        )
        .await
        .expect("completed flow retry flow should add");
    let flow_child = flow_queue
        .claim_next(
            "worker-completed-flow-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("completed flow child claim should return")
        .expect("completed flow child should be claimable");
    assert_eq!(flow_child.id, flow.children[0].id);
    flow_queue
        .complete_job(
            &flow_child.id,
            lock_token(&flow_child),
            serde_json::json!({ "child": "done" }),
            Utc::now(),
        )
        .await
        .expect("completed flow child should complete");
    assert_eq!(
        flow_queue
            .get_job(&flow.parent.id)
            .await
            .expect("flow parent should load")
            .expect("flow parent should exist")
            .state,
        JobState::Waiting
    );

    let retried_child = flow_queue
        .retry_job(&flow_child.id, Utc::now())
        .await
        .expect("completed flow child should retry");
    assert_eq!(retried_child.state, JobState::Waiting);
    assert!(retried_child.return_value.is_none());
    let parent_after_retry = flow_queue
        .get_job(&flow.parent.id)
        .await
        .expect("flow parent after retry should load")
        .expect("flow parent after retry should exist");
    assert_eq!(parent_after_retry.state, JobState::WaitingChildren);
    assert_eq!(
        flow_queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .expect("flow dependency counts should load")
            .expect("flow dependency counts should exist"),
        a3s_lane::JobFlowDependencyCounts {
            processed: 0,
            unprocessed: 1,
            failed: 0,
            ignored: 0,
            missing: 0,
        }
    );
    assert!(flow_queue
        .get_flow_children_values(&flow.parent.id)
        .await
        .expect("flow child values should load")
        .expect("flow child values should exist")
        .is_empty());
    let parent_waiting_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:completed-flow-retry:waiting"),
            &flow.parent.id,
        )
        .await?;
    assert!(parent_waiting_score.is_none());
    let parent_waiting_children_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:completed-flow-retry:waiting_children"),
            &flow.parent.id,
        )
        .await?;
    assert!(parent_waiting_children_score.is_some());

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_promote_state_gate(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "promote-state")
        .expect("valid Redis URL should build the promote-state queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let waiting = queue
        .add_job(
            "waiting".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("waiting promote-state job should add");
    let waiting_promote = queue
        .promote_job(&waiting.id, Utc::now())
        .await
        .expect_err("waiting job should reject promote");
    assert!(matches!(waiting_promote, LaneError::JobStateConflict(_)));
    assert_eq!(
        queue
            .get_job(&waiting.id)
            .await
            .expect("waiting job should load")
            .expect("waiting job should exist")
            .state,
        JobState::Waiting
    );

    let delayed = queue
        .add_job(
            "delayed".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("delayed promote-state job should add");
    queue
        .promote_job(&delayed.id, Utc::now())
        .await
        .expect("delayed promote-state job should promote");
    let claimed = queue
        .claim_next(
            "worker-promote-state".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("promote-state claim should return")
        .expect("promote-state job should be claimable");
    assert_eq!(claimed.id, waiting.id);
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("promote-state waiting job should complete");

    let delayed_claim = queue
        .claim_next(
            "worker-promote-state-delayed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("promoted delayed claim should return")
        .expect("promoted delayed job should be claimable");
    assert_eq!(delayed_claim.id, delayed.id);
    queue
        .complete_job(
            &delayed_claim.id,
            lock_token(&delayed_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("promoted delayed job should complete");

    let delayed_key = format!("{namespace}:promote-state:delayed");
    let _: usize = conn.zadd(&delayed_key, &delayed.id, 0.0).await?;
    let stale_promote = queue
        .promote_job(&delayed.id, Utc::now())
        .await
        .expect_err("completed job with stale delayed index should reject promote");
    assert!(matches!(stale_promote, LaneError::JobStateConflict(_)));
    let stale_delayed_score: Option<f64> = conn.zscore(&delayed_key, &delayed.id).await?;
    assert!(stale_delayed_score.is_none());

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_terminal_progress_update(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "terminal-progress")
        .expect("valid Redis URL should build the terminal-progress queue");
    let job = queue
        .add_job("task".to_string(), serde_json::json!({}), JobOptions::new())
        .await
        .expect("terminal progress job should add");
    let claimed = queue
        .claim_next(
            "worker-terminal-progress".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("terminal progress claim should return")
        .expect("terminal progress job should be claimable");
    queue
        .complete_job(
            &job.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("terminal progress job should complete");

    let updated = queue
        .update_progress(&job.id, serde_json::json!({ "percent": 100 }))
        .await
        .expect("terminal progress update should succeed");
    assert_eq!(updated.state, JobState::Completed);
    assert_eq!(
        updated.progress,
        Some(serde_json::json!({ "percent": 100 }))
    );

    let stored = queue
        .get_job(&job.id)
        .await
        .expect("terminal progress stored job should load")
        .expect("terminal progress stored job should remain");
    assert_eq!(stored.progress, Some(serde_json::json!({ "percent": 100 })));
    let events = queue
        .read_events("-", "+", 100)
        .await
        .expect("terminal progress events should load");
    let progress = events
        .iter()
        .rev()
        .find(|event| event.event == "progress")
        .expect("terminal progress update should emit an event");
    assert_eq!(progress.job_id.as_deref(), Some(job.id.as_str()));
    assert_eq!(
        progress.fields.get("data"),
        Some(&serde_json::json!({ "percent": 100 }))
    );

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_terminal_data_update(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "terminal-data")
        .expect("valid Redis URL should build the terminal-data queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let jobs_key = format!("{namespace}:terminal-data:jobs");

    let completed = queue
        .add_job(
            "completed".to_string(),
            serde_json::json!({ "stage": "created" }),
            JobOptions::new(),
        )
        .await
        .expect("terminal data completed job should add");
    let completed_claim = queue
        .claim_next(
            "worker-terminal-data-completed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("terminal data completed claim should return")
        .expect("terminal data completed job should claim");
    assert_eq!(completed_claim.id, completed.id);
    queue
        .complete_job(
            &completed.id,
            lock_token(&completed_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("terminal data completed job should complete");

    let updated_completed = queue
        .update_data(
            &completed.id,
            serde_json::json!({ "stage": "archived", "terminal": "completed" }),
        )
        .await
        .expect("completed terminal data update should succeed");
    assert_eq!(updated_completed.state, JobState::Completed);
    assert_eq!(
        updated_completed.payload,
        serde_json::json!({ "stage": "archived", "terminal": "completed" })
    );
    let completed_raw = load_raw_job_value(&mut conn, &jobs_key, &completed.id).await?;
    assert_eq!(
        completed_raw.get("payload"),
        Some(&serde_json::json!({ "stage": "archived", "terminal": "completed" }))
    );

    let failed = queue
        .add_job(
            "failed".to_string(),
            serde_json::json!({ "stage": "created" }),
            JobOptions::new(),
        )
        .await
        .expect("terminal data failed job should add");
    let failed_claim = queue
        .claim_next(
            "worker-terminal-data-failed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("terminal data failed claim should return")
        .expect("terminal data failed job should claim");
    assert_eq!(failed_claim.id, failed.id);
    queue
        .fail_job(
            &failed.id,
            lock_token(&failed_claim),
            "terminal data failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("terminal data failed job should fail");

    let updated_failed = queue
        .update_data(
            &failed.id,
            serde_json::json!({ "stage": "archived", "terminal": "failed" }),
        )
        .await
        .expect("failed terminal data update should succeed");
    assert_eq!(updated_failed.state, JobState::Failed);
    assert_eq!(
        updated_failed.payload,
        serde_json::json!({ "stage": "archived", "terminal": "failed" })
    );
    let failed_raw = load_raw_job_value(&mut conn, &jobs_key, &failed.id).await?;
    assert_eq!(
        failed_raw.get("payload"),
        Some(&serde_json::json!({ "stage": "archived", "terminal": "failed" }))
    );

    let missing = queue
        .update_data(
            "missing-terminal-data-job",
            serde_json::json!({ "stage": "missing" }),
        )
        .await
        .expect_err("missing terminal data update should fail");
    assert!(matches!(missing, LaneError::JobNotFound(_)));

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_maxed_active_limit(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("maxed-active-limit:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("maxed-active-limit:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "maxed-active")
        .expect("valid Redis URL should build the maxed-active queue");
    assert!(!queue
        .is_maxed()
        .await
        .expect("unset max-active queue should not be maxed"));
    queue
        .set_max_active_jobs(1)
        .await
        .expect("max active jobs should configure");
    assert!(!queue
        .is_maxed()
        .await
        .expect("empty max-active queue should not be maxed"));

    let first = queue
        .add_job(
            "maxed-first".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("first maxed-active job should add");
    let second = queue
        .add_job(
            "maxed-second".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("second maxed-active job should add");

    let first_claim = queue
        .claim_next(
            "worker-maxed-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first maxed-active claim should return")
        .expect("first maxed-active job should claim");
    assert_eq!(first_claim.id, first.id);
    assert!(queue
        .is_maxed()
        .await
        .expect("queue should be maxed with one active job"));
    assert!(queue
        .claim_next(
            "worker-maxed-b".to_string(),
            Duration::from_secs(30),
            Utc::now()
        )
        .await
        .expect("maxed queue claim should return")
        .is_none());

    queue
        .complete_job(
            &first_claim.id,
            lock_token(&first_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("first maxed-active job should complete");
    assert!(!queue
        .is_maxed()
        .await
        .expect("queue should not be maxed after completion"));

    let second_claim = queue
        .claim_next(
            "worker-maxed-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("second maxed-active claim should return")
        .expect("second maxed-active job should claim after completion");
    assert_eq!(second_claim.id, second.id);
    assert!(queue
        .is_maxed()
        .await
        .expect("queue should be maxed with the second active job"));
    queue
        .complete_job(
            &second_claim.id,
            lock_token(&second_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("second maxed-active job should complete");
    assert!(!queue
        .is_maxed()
        .await
        .expect("queue should not be maxed after all jobs complete"));

    queue
        .clear_max_active_jobs()
        .await
        .expect("max active jobs should clear");
    assert!(!queue
        .is_maxed()
        .await
        .expect("cleared max-active queue should not be maxed"));

    trace_stage("maxed-active-limit:cleanup-final:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("maxed-active-limit:cleanup-final:done");
    Ok(())
}

async fn run_worker_bulk_lease_renewal(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("worker-bulk-lease:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("worker-bulk-lease:cleanup:done");

    let queue = Arc::new(
        RedisJobQueue::with_namespace(&redis_url, &namespace, "worker-bulk-leases")
            .expect("valid Redis URL should build the worker bulk lease queue"),
    );
    let backend: Arc<dyn JobQueueBackend> = queue.clone();
    for name in ["slow-a", "slow-b"] {
        queue
            .add_job(name.to_string(), serde_json::json!({}), JobOptions::new())
            .await
            .expect("slow worker bulk lease job should add");
    }
    trace_stage("worker-bulk-lease:jobs-added");

    let processor: Arc<dyn JobProcessor> = Arc::new(job_processor_fn(
        |job: Job, context: JobContext| async move {
            trace_stage(&format!("worker-bulk-lease:processor-start:{}", job.name));
            tokio::time::sleep(Duration::from_secs(2)).await;
            context.ensure_lease()?;
            trace_stage(&format!("worker-bulk-lease:processor-finish:{}", job.name));
            Ok(serde_json::json!({ "name": job.name }))
        },
    ));
    let worker = JobWorker::new(
        Arc::clone(&backend),
        processor,
        JobWorkerConfig::new("worker-bulk-leases")
            .with_concurrency(2)
            .with_lease_duration(Duration::from_secs(1))
            .with_lease_renew_interval(Duration::from_millis(100))
            .with_poll_interval(Duration::from_millis(10))
            .with_blocking_claim_timeout(Duration::from_millis(250))
            .with_recover_stalled(false),
    );

    let handle = worker.start();
    trace_stage("worker-bulk-lease:worker-started");
    if tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if queue
                .get_job_count(&[JobState::Completed])
                .await
                .expect("completed count should load")
                == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .is_err()
    {
        let stats = queue.stats().await.expect("timed out stats should load");
        panic!("worker bulk lease jobs should complete, got stats: {stats:?}");
    }
    handle.shutdown().await;
    trace_stage("worker-bulk-lease:worker-shutdown");

    let completed = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Completed))
        .await
        .expect("completed bulk lease jobs should list");
    assert_eq!(completed.total, 2);
    assert!(completed.jobs.iter().all(|job| job
        .return_value
        .as_ref()
        .is_some_and(|value| value["name"].is_string())));

    trace_stage("worker-bulk-lease:cleanup-final:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("worker-bulk-lease:cleanup-final:done");
    Ok(())
}

async fn run_bulk_lease_renewal(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("bulk-lease:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("bulk-lease:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "bulk-leases")
        .expect("valid Redis URL should build the bulk-leases queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    trace_stage("bulk-lease:queue-created");
    let active_key = format!("{namespace}:bulk-leases:active");
    let stalled_key = format!("{namespace}:bulk-leases:stalled");
    let lock_key_prefix = format!("{namespace}:bulk-leases:locks:");

    let first = queue
        .add_job(
            "first".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("first bulk lease job should add");
    trace_stage("bulk-lease:first-added");
    let second = queue
        .add_job(
            "second".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("second bulk lease job should add");
    trace_stage("bulk-lease:second-added");
    let first_claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_000))
        .await
        .expect("first bulk lease claim should return")
        .expect("first bulk lease job should claim");
    trace_stage("bulk-lease:first-claimed");
    let second_claimed = queue
        .claim_next("worker-b".to_string(), Duration::from_secs(30), ts(1_000))
        .await
        .expect("second bulk lease claim should return")
        .expect("second bulk lease job should claim");
    trace_stage("bulk-lease:second-claimed");
    assert_eq!(first_claimed.id, first.id);
    assert_eq!(second_claimed.id, second.id);

    let _: usize = conn.sadd(&stalled_key, &[&first.id, &second.id]).await?;
    trace_stage("bulk-lease:stalled-seeded");
    let failed = queue
        .renew_leases(
            &[
                JobLeaseRenewal::new(&first.id, lock_token(&first_claimed)),
                JobLeaseRenewal::new(&second.id, "wrong-token"),
                JobLeaseRenewal::new("missing-bulk-lease", "missing-token"),
            ],
            Duration::from_secs(5),
            ts(2_000),
        )
        .await
        .expect("bulk lease renewal should run");
    trace_stage("bulk-lease:renewed");
    assert_eq!(
        failed,
        vec![second.id.clone(), "missing-bulk-lease".to_string()]
    );

    let first_after = queue
        .get_job(&first.id)
        .await
        .expect("renewed job should load")
        .expect("renewed job should exist");
    let second_after = queue
        .get_job(&second.id)
        .await
        .expect("failed renewal job should load")
        .expect("failed renewal job should exist");
    assert_eq!(first_after.lease_expires_at, Some(ts(7_000)));
    assert_eq!(second_after.lease_expires_at, Some(ts(31_000)));

    let first_score: f64 = conn.zscore(&active_key, &first.id).await?;
    let second_score: f64 = conn.zscore(&active_key, &second.id).await?;
    assert_eq!(first_score, 7_000.0);
    assert_eq!(second_score, 31_000.0);
    let first_stalled: bool = conn.sismember(&stalled_key, &first.id).await?;
    let second_stalled: bool = conn.sismember(&stalled_key, &second.id).await?;
    assert!(!first_stalled);
    assert!(second_stalled);
    let first_ttl: i64 = conn.pttl(format!("{lock_key_prefix}{}", first.id)).await?;
    assert!(first_ttl > 0);

    trace_stage("bulk-lease:cleanup-final:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("bulk-lease:cleanup-final:done");
    Ok(())
}

async fn run_job_metrics(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "job-metrics")
        .expect("valid Redis URL should build the job-metrics queue");
    let empty = queue
        .get_metrics(JobState::Completed, 0, -1)
        .await
        .expect("empty completed metrics should load");
    assert_eq!(empty.meta.count, 0);
    assert_eq!(empty.meta.previous_timestamp_millis, 0);
    assert_eq!(empty.meta.previous_count, 0);
    assert!(empty.data.is_empty());
    assert_eq!(empty.count, 0);

    let bad_state = queue
        .get_metrics(JobState::Waiting, 0, -1)
        .await
        .expect_err("non-terminal metrics should be rejected");
    assert!(matches!(bad_state, LaneError::ConfigError(_)));

    for (name, finished_at) in [
        ("complete-a", ts(1_000)),
        ("complete-b", ts(2_000)),
        ("complete-c", ts(61_000)),
    ] {
        let job = queue
            .add_job(name.to_string(), serde_json::json!({}), JobOptions::new())
            .await
            .expect("completed metric job should add");
        let claimed = queue
            .claim_next(
                format!("worker-{name}"),
                Duration::from_secs(30),
                finished_at,
            )
            .await
            .expect("completed metric claim should return")
            .expect("completed metric job should claim");
        assert_eq!(claimed.id, job.id);
        queue
            .complete_job(
                &claimed.id,
                lock_token(&claimed),
                serde_json::json!({ "ok": name }),
                finished_at,
            )
            .await
            .expect("completed metric job should complete");
    }

    let completed_metrics = queue
        .get_metrics(JobState::Completed, 0, -1)
        .await
        .expect("completed metrics should load");
    assert_eq!(completed_metrics.meta.count, 3);
    assert_eq!(completed_metrics.meta.previous_timestamp_millis, 61_000);
    assert_eq!(completed_metrics.meta.previous_count, 2);
    assert_eq!(completed_metrics.data, vec![2]);
    assert_eq!(completed_metrics.count, 1);

    for (name, failed_at) in [("fail-a", ts(1_000)), ("fail-b", ts(61_000))] {
        let job = queue
            .add_job(name.to_string(), serde_json::json!({}), JobOptions::new())
            .await
            .expect("failed metric job should add");
        let claimed = queue
            .claim_next(format!("worker-{name}"), Duration::from_secs(30), failed_at)
            .await
            .expect("failed metric claim should return")
            .expect("failed metric job should claim");
        assert_eq!(claimed.id, job.id);
        queue
            .fail_job(
                &claimed.id,
                lock_token(&claimed),
                format!("{name} failed"),
                failed_at,
            )
            .await
            .expect("failed metric job should fail");
    }

    let failed_metrics = queue
        .get_metrics(JobState::Failed, 0, -1)
        .await
        .expect("failed metrics should load");
    assert_eq!(failed_metrics.meta.count, 2);
    assert_eq!(failed_metrics.meta.previous_timestamp_millis, 61_000);
    assert_eq!(failed_metrics.meta.previous_count, 1);
    assert_eq!(failed_metrics.data, vec![1]);
    assert_eq!(failed_metrics.count, 1);

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_save_stacktrace(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "save-stacktrace")
        .expect("valid Redis URL should build the save-stacktrace queue");
    let job = queue
        .add_job(
            "waiting".to_string(),
            serde_json::json!({ "kind": "diagnostic" }),
            JobOptions::new(),
        )
        .await
        .expect("diagnostic job should add");

    let empty_trace = queue
        .save_stacktrace(&job.id, Vec::new(), "empty trace".to_string())
        .await
        .expect("empty stacktrace should save");
    assert!(empty_trace.stacktrace.is_empty());
    assert_eq!(empty_trace.failed_reason.as_deref(), Some("empty trace"));

    let stacktrace = vec![
        "Error: diagnostic failure".to_string(),
        "at worker.rs:42:9".to_string(),
    ];
    let traced = queue
        .save_stacktrace(
            &job.id,
            stacktrace.clone(),
            "diagnostic failure".to_string(),
        )
        .await
        .expect("stacktrace should save");
    assert_eq!(traced.stacktrace, stacktrace);
    assert_eq!(traced.failed_reason.as_deref(), Some("diagnostic failure"));

    let restored = queue
        .get_job(&job.id)
        .await
        .expect("traced job should load")
        .expect("traced job should exist");
    assert_eq!(restored.stacktrace, stacktrace);
    assert_eq!(
        restored.failed_reason.as_deref(),
        Some("diagnostic failure")
    );

    let missing = queue
        .save_stacktrace("missing-stacktrace-job", Vec::new(), "missing".to_string())
        .await
        .expect_err("missing stacktrace job should fail");
    assert!(matches!(missing, LaneError::JobNotFound(_)));

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_job_finished_results(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "finished-result")
        .expect("valid Redis URL should build the finished-result queue");
    let waiting = queue
        .add_job(
            "waiting".to_string(),
            serde_json::json!({ "kind": "waiting" }),
            JobOptions::new(),
        )
        .await
        .expect("waiting job should add");
    assert_eq!(
        queue
            .get_job_finished_result(&waiting.id)
            .await
            .expect("waiting finished status should load"),
        Some(JobFinishedResult::NotFinished)
    );

    let completed = queue
        .claim_next(
            "worker-finished-complete".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("completed claim should return")
        .expect("completed job should be claimable");
    assert_eq!(completed.id, waiting.id);
    queue
        .complete_job(
            &completed.id,
            lock_token(&completed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("completed job should complete");
    assert_eq!(
        queue
            .get_job_finished_result(&completed.id)
            .await
            .expect("completed finished status should load"),
        Some(JobFinishedResult::Completed {
            return_value: Some(serde_json::json!({ "ok": true })),
        })
    );

    let false_value_job = queue
        .add_job(
            "completed-false".to_string(),
            serde_json::json!({ "kind": "completed-false" }),
            JobOptions::new(),
        )
        .await
        .expect("false-value job should add");
    let false_value_claim = queue
        .claim_next(
            "worker-finished-false".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("false-value claim should return")
        .expect("false-value job should be claimable");
    assert_eq!(false_value_claim.id, false_value_job.id);
    queue
        .complete_job(
            &false_value_claim.id,
            lock_token(&false_value_claim),
            serde_json::json!(false),
            Utc::now(),
        )
        .await
        .expect("false-value job should complete");
    assert_eq!(
        queue
            .get_job_finished_result(&false_value_claim.id)
            .await
            .expect("false-value finished status should load"),
        Some(JobFinishedResult::Completed {
            return_value: Some(serde_json::json!(false)),
        })
    );

    let failed_job = queue
        .add_job(
            "failed".to_string(),
            serde_json::json!({ "kind": "failed" }),
            JobOptions::new(),
        )
        .await
        .expect("failed job should add");
    let failed = queue
        .claim_next(
            "worker-finished-fail".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("failed claim should return")
        .expect("failed job should be claimable");
    assert_eq!(failed.id, failed_job.id);
    queue
        .fail_job(
            &failed.id,
            lock_token(&failed),
            "terminal failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("failed job should fail");
    assert_eq!(
        queue
            .get_job_finished_result(&failed.id)
            .await
            .expect("failed finished status should load"),
        Some(JobFinishedResult::Failed {
            failed_reason: Some("terminal failure".to_string()),
        })
    );

    let stale_completed = queue
        .add_job(
            "stale-completed-index".to_string(),
            serde_json::json!({ "kind": "stale-completed" }),
            JobOptions::new(),
        )
        .await
        .expect("stale completed-index job should add");
    let stale_failed = queue
        .add_job(
            "stale-failed-index".to_string(),
            serde_json::json!({ "kind": "stale-failed" }),
            JobOptions::new(),
        )
        .await
        .expect("stale failed-index job should add");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let _: usize = conn
        .zadd(
            format!("{namespace}:finished-result:completed"),
            &stale_completed.id,
            0.0,
        )
        .await?;
    let _: usize = conn
        .zadd(
            format!("{namespace}:finished-result:failed"),
            &stale_failed.id,
            0.0,
        )
        .await?;
    assert_eq!(
        queue
            .get_job_finished_result(&stale_completed.id)
            .await
            .expect("stale completed-index finished status should load"),
        Some(JobFinishedResult::Completed { return_value: None })
    );
    assert_eq!(
        queue
            .get_job_finished_result(&stale_failed.id)
            .await
            .expect("stale failed-index finished status should load"),
        Some(JobFinishedResult::Failed {
            failed_reason: None,
        })
    );

    assert_eq!(
        queue
            .get_job_finished_result("missing-finished-job")
            .await
            .expect("missing finished status should load"),
        None
    );

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_orphaned_job_removal(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "orphan-clean")
        .expect("valid Redis URL should build the orphan-clean queue");
    let referenced = queue
        .add_job(
            "referenced".to_string(),
            serde_json::json!({ "kind": "referenced" }),
            JobOptions::new(),
        )
        .await
        .expect("referenced job should add");
    let stalled = queue
        .add_job(
            "stalled-reference".to_string(),
            serde_json::json!({ "kind": "stalled" }),
            JobOptions::new(),
        )
        .await
        .expect("stalled-reference job should add");
    let orphaned = queue
        .add_job(
            "orphaned".to_string(),
            serde_json::json!({ "kind": "orphaned" }),
            JobOptions::new(),
        )
        .await
        .expect("orphaned job should add");
    queue
        .add_log(&orphaned.id, "orphaned log".to_string(), 10, Utc::now())
        .await
        .expect("orphaned log should append");

    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let waiting_key = format!("{namespace}:orphan-clean:waiting");
    let jobs_key = format!("{namespace}:orphan-clean:jobs");
    let stalled_key = format!("{namespace}:orphan-clean:stalled");
    let orphaned_logs_key = format!("{namespace}:orphan-clean:logs:{}", orphaned.id);
    let orphaned_dependencies_key =
        format!("{namespace}:orphan-clean:dependencies:{}", orphaned.id);
    let orphaned_lock_key = format!("{namespace}:orphan-clean:locks:{}", orphaned.id);

    let _: usize = conn.zrem(&waiting_key, &orphaned.id).await?;
    let _: usize = conn.zrem(&waiting_key, &stalled.id).await?;
    let _: usize = conn.sadd(&stalled_key, &stalled.id).await?;
    let _: usize = conn.sadd(&orphaned_dependencies_key, "stale-child").await?;
    let _: () = conn.set(&orphaned_lock_key, "stale-lock").await?;

    let removed = queue
        .remove_orphaned_jobs(1, 1)
        .await
        .expect("orphaned job cleanup should run");
    assert_eq!(removed, 1);
    let orphaned_hash: Option<String> = conn.hget(&jobs_key, &orphaned.id).await?;
    assert!(orphaned_hash.is_none());
    let orphaned_logs_len: usize = conn.llen(&orphaned_logs_key).await?;
    assert_eq!(orphaned_logs_len, 0);
    let orphaned_dependencies_exists: usize = conn.exists(&orphaned_dependencies_key).await?;
    assert_eq!(orphaned_dependencies_exists, 0);
    let orphaned_lock_exists: usize = conn.exists(&orphaned_lock_key).await?;
    assert_eq!(orphaned_lock_exists, 0);

    assert!(queue
        .get_job(&referenced.id)
        .await
        .expect("referenced job lookup should return")
        .is_some());
    assert!(queue
        .get_job(&stalled.id)
        .await
        .expect("stalled-reference job lookup should return")
        .is_some());

    let no_more = queue
        .remove_orphaned_jobs(1, 0)
        .await
        .expect("second orphaned job cleanup should run");
    assert_eq!(no_more, 0);

    let _: usize = conn.srem(&stalled_key, &stalled.id).await?;
    let removed_after_stalled_clear = queue
        .remove_orphaned_jobs(1, 0)
        .await
        .expect("stalled-clear orphaned job cleanup should run");
    assert_eq!(removed_after_stalled_clear, 1);
    let stalled_hash: Option<String> = conn.hget(&jobs_key, &stalled.id).await?;
    assert!(stalled_hash.is_none());
    assert!(queue
        .get_job(&referenced.id)
        .await
        .expect("referenced job lookup after cleanup should return")
        .is_some());

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_missing_remove_orphan_prune(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue_name = "missing-remove-prune";
    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, queue_name)
        .expect("valid Redis URL should build the missing-remove-prune queue");
    let referenced = queue
        .add_job(
            "referenced".to_string(),
            serde_json::json!({ "kind": "referenced" }),
            JobOptions::new(),
        )
        .await
        .expect("referenced job should add");

    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let missing_job_id = "missing-job-with-stale-redis-state";
    for state in [
        "waiting",
        "delayed",
        "active",
        "waiting_children",
        "completed",
        "failed",
    ] {
        let _: usize = conn
            .zadd(
                format!("{namespace}:{queue_name}:{state}"),
                missing_job_id,
                0.0,
            )
            .await?;
    }
    let _: () = conn
        .set(
            format!("{namespace}:{queue_name}:locks:{missing_job_id}"),
            "stale-lock",
        )
        .await?;
    let _: usize = conn
        .sadd(format!("{namespace}:{queue_name}:stalled"), missing_job_id)
        .await?;
    let _: usize = conn
        .sadd(
            format!("{namespace}:{queue_name}:dependencies:{missing_job_id}"),
            "stale-child",
        )
        .await?;
    let _: usize = conn
        .rpush(
            format!("{namespace}:{queue_name}:logs:{missing_job_id}"),
            "{\"line\":\"stale\"}",
        )
        .await?;

    assert!(queue
        .remove_job(missing_job_id)
        .await
        .expect("missing job remove should return")
        .is_none());

    let stored_missing: Option<String> = conn
        .hget(format!("{namespace}:{queue_name}:jobs"), missing_job_id)
        .await?;
    assert!(stored_missing.is_none());
    for state in [
        "waiting",
        "delayed",
        "active",
        "waiting_children",
        "completed",
        "failed",
    ] {
        let score: Option<f64> = conn
            .zscore(format!("{namespace}:{queue_name}:{state}"), missing_job_id)
            .await?;
        assert!(
            score.is_none(),
            "missing remove should prune the orphaned {state} index"
        );
    }
    let missing_lock_exists: usize = conn
        .exists(format!("{namespace}:{queue_name}:locks:{missing_job_id}"))
        .await?;
    assert_eq!(missing_lock_exists, 0);
    let missing_stalled_member: bool = conn
        .sismember(format!("{namespace}:{queue_name}:stalled"), missing_job_id)
        .await?;
    assert!(!missing_stalled_member);
    let missing_dependencies_exist: usize = conn
        .exists(format!(
            "{namespace}:{queue_name}:dependencies:{missing_job_id}"
        ))
        .await?;
    assert_eq!(missing_dependencies_exist, 0);
    let missing_logs_len: usize = conn
        .llen(format!("{namespace}:{queue_name}:logs:{missing_job_id}"))
        .await?;
    assert_eq!(missing_logs_len, 0);

    assert!(queue
        .get_job(&referenced.id)
        .await
        .expect("referenced job lookup should return")
        .is_some());
    let referenced_waiting_score: Option<f64> = conn
        .zscore(format!("{namespace}:{queue_name}:waiting"), &referenced.id)
        .await?;
    assert!(referenced_waiting_score.is_some());

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_obliterates_queue_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
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
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_repeat_keep_last(redis_url))
        .await
        .expect("Redis repeat keep-last integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_adds_repeat_from_scheduler_metadata_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_add_scheduler_metadata(redis_url),
    )
    .await
    .expect("Redis repeat scheduler metadata add integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_rejects_expired_repeat_end_at_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_expired_end_at_validation(redis_url),
    )
    .await
    .expect("Redis expired repeat end_at integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_rejects_bullmq_reserved_job_ids_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_reserved_job_id_validation(redis_url),
    )
    .await
    .expect("Redis reserved job-id integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_clears_keep_last_next_on_manual_release_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_keep_last_manual_release_cleanup(redis_url),
    )
    .await
    .expect("Redis keep-last manual-release integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_clears_keep_last_next_on_owner_removal_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_keep_last_owner_removal_cleanup(redis_url),
    )
    .await
    .expect("Redis keep-last owner-removal integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_clears_keep_last_next_for_stale_dedup_owner_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_keep_last_stale_owner_cleanup(redis_url),
    )
    .await
    .expect("Redis keep-last stale-owner integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_keeps_ttl_dedup_owner_after_completion_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_ttl_dedup_finalization(redis_url),
    )
    .await
    .expect("Redis TTL dedup finalization integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_materializes_flow_keep_last_after_parent_completion_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_flow_keep_last_parent_completion(redis_url),
    )
    .await
    .expect("Redis flow keep-last completion integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_materializes_flow_keep_last_after_parent_failure_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_flow_keep_last_parent_failure(redis_url),
    )
    .await
    .expect("Redis flow keep-last failure integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_materializes_flow_keep_last_after_parent_stalled_failure_against_real_server(
) {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_flow_keep_last_parent_stalled_failure(redis_url),
    )
    .await
    .expect("Redis flow keep-last stalled integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_upserts_repeat_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_repeat_upsert(redis_url))
        .await
        .expect("Redis repeat upsert integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_upserts_repeat_from_scheduler_metadata_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_upsert_scheduler_metadata(redis_url),
    )
    .await
    .expect("Redis repeat scheduler metadata upsert integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_rejects_repeat_retry_from_scheduler_metadata_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_retry_scheduler_metadata(redis_url),
    )
    .await
    .expect("Redis repeat scheduler metadata retry integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_releases_repeat_scheduler_metadata_without_fast_owner_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_release_scheduler_metadata(redis_url),
    )
    .await
    .expect("Redis repeat scheduler metadata release integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_recovers_repeat_stalled_from_scheduler_metadata_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_stalled_scheduler_metadata(redis_url),
    )
    .await
    .expect("Redis repeat scheduler metadata stalled integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_preserves_drained_repeat_from_scheduler_metadata_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_drain_scheduler_metadata(redis_url),
    )
    .await
    .expect("Redis repeat scheduler metadata drain integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_preserves_cleaned_repeat_from_scheduler_metadata_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_clean_scheduler_metadata(redis_url),
    )
    .await
    .expect("Redis repeat scheduler metadata clean integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_syncs_repeat_scheduler_metadata_on_nonterminal_moves_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_nonterminal_scheduler_metadata(redis_url),
    )
    .await
    .expect("Redis repeat scheduler metadata move integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_removes_repeat_from_scheduler_metadata_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_remove_scheduler_metadata(redis_url),
    )
    .await
    .expect("Redis repeat scheduler metadata remove integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_reads_repeat_from_scheduler_metadata_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_repeat_read_scheduler_metadata(redis_url),
    )
    .await
    .expect("Redis repeat scheduler metadata read integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_orders_lifo_waiting_jobs_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
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
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_queue_events(redis_url))
        .await
        .expect("Redis queue-events integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_trims_queue_events_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_queue_event_trimming(redis_url),
    )
    .await
    .expect("Redis queue-event trimming integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_records_bulk_dedup_events_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_bulk_dedup_events(redis_url))
        .await
        .expect("Redis bulk dedup-events integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_emits_flow_parent_transition_events_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_flow_parent_transition_events(redis_url),
    )
    .await
    .expect("Redis flow parent transition events integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_records_flow_dedup_events_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_flow_dedup_events(redis_url))
        .await
        .expect("Redis flow dedup-events integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_reuses_flow_duplicate_job_ids_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_flow_duplicate_job_ids(redis_url),
    )
    .await
    .expect("Redis flow duplicate job-id integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_emits_retries_exhausted_event_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_retries_exhausted_event(redis_url),
    )
    .await
    .expect("Redis retries-exhausted event integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_emits_stalled_recovery_events_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_stalled_recovery_events(redis_url),
    )
    .await
    .expect("Redis stalled recovery events integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_guards_stalled_recovery_indexes_and_tokens_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_stalled_recovery_guards(redis_url),
    )
    .await
    .expect("Redis stalled recovery guard integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_updates_worker_markers_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_worker_markers(redis_url))
        .await
        .expect("Redis worker-marker integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_suppresses_paused_claim_promotion_marker_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_paused_claim_promotion_marker(redis_url),
    )
    .await
    .expect("Redis paused claim-promotion marker integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_blocks_on_worker_markers_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_blocking_worker_markers(redis_url),
    )
    .await
    .expect("Redis blocking worker-marker integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_job_worker_uses_marker_blocking_claims_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_blocking_job_worker(redis_url))
        .await
        .expect("Redis blocking JobWorker integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_applies_finished_retention_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(Duration::from_secs(120), run_finished_retention(redis_url))
        .await
        .expect("Redis finished-retention integration test timed out")
        .unwrap();
}

#[tokio::test]
async fn redis_backend_ignores_flow_dependency_failure_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_ignored_flow_dependency_failure(redis_url),
    )
    .await
    .expect("Redis ignored flow dependency failure integration test timed out")
    .unwrap();
}

async fn run_ignored_flow_dependency_failure(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-ignore")
        .expect("valid Redis URL should build the flow-ignore queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-ignore")
        .expect("valid Redis URL should build the flow-ignore worker");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let flow = queue
        .add_flow_at(
            JobSpec::new(
                "ignored-failure-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new(
                    "ignored-failure-optional-child",
                    serde_json::json!({ "optional": true }),
                )
                .with_options(
                    JobOptions::new()
                        .with_priority(1)
                        .with_ignore_dependency_on_failure(true),
                ),
                JobSpec::new(
                    "ignored-failure-required-child",
                    serde_json::json!({ "required": true }),
                )
                .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("ignored flow should be added");
    let dependency_key = format!("{namespace}:flow-ignore:dependencies:{}", flow.parent.id);

    let optional_child = worker
        .claim_next(
            "worker-ignored-optional".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("optional child claim should return")
        .expect("optional child should be claimable");
    assert_eq!(optional_child.id, flow.children[0].id);
    worker
        .fail_job(
            &optional_child.id,
            lock_token(&optional_child),
            "optional child failed".to_string(),
            Utc::now(),
        )
        .await
        .expect("optional child should fail");

    let pending_count: usize = conn.scard(&dependency_key).await?;
    assert_eq!(pending_count, 1);
    let optional_pending: bool = conn
        .sismember(&dependency_key, &flow.children[0].id)
        .await?;
    let required_pending: bool = conn
        .sismember(&dependency_key, &flow.children[1].id)
        .await?;
    assert!(!optional_pending);
    assert!(required_pending);
    let parent_after_failure = queue
        .get_job(&flow.parent.id)
        .await
        .expect("parent after ignored failure should load")
        .expect("parent should exist");
    assert_eq!(parent_after_failure.state, JobState::WaitingChildren);
    assert!(parent_after_failure.failed_reason.is_none());
    let counts_after_failure = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("ignored failure counts should load")
        .expect("ignored failure counts should exist");
    assert_eq!(counts_after_failure.processed, 0);
    assert_eq!(counts_after_failure.unprocessed, 1);
    assert_eq!(counts_after_failure.failed, 0);
    assert_eq!(counts_after_failure.ignored, 1);
    assert_eq!(counts_after_failure.missing, 0);
    let ignored_failures = queue
        .get_flow_ignored_children_failures(&flow.parent.id)
        .await
        .expect("ignored failure map should load")
        .expect("ignored failure map should exist");
    assert_eq!(ignored_failures.len(), 1);
    assert_eq!(
        ignored_failures
            .get(&flow.children[0].id)
            .map(String::as_str),
        Some("optional child failed")
    );

    let required_child = worker
        .claim_next(
            "worker-ignored-required".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("required child claim should return")
        .expect("required child should be claimable");
    assert_eq!(required_child.id, flow.children[1].id);
    worker
        .complete_job(
            &required_child.id,
            lock_token(&required_child),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("required child should complete");

    let dependency_key_exists: bool = conn.exists(&dependency_key).await?;
    assert!(!dependency_key_exists);
    let parent_after_release = queue
        .get_job(&flow.parent.id)
        .await
        .expect("parent after release should load")
        .expect("parent should exist");
    assert_eq!(parent_after_release.state, JobState::Waiting);
    let parent_failed_score: Option<f64> = conn
        .zscore(format!("{namespace}:flow-ignore:failed"), &flow.parent.id)
        .await?;
    assert!(parent_failed_score.is_none());
    let parent_waiting_score: Option<f64> = conn
        .zscore(format!("{namespace}:flow-ignore:waiting"), &flow.parent.id)
        .await?;
    assert!(parent_waiting_score.is_some());
    let counts_after_release = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("ignored release counts should load")
        .expect("ignored release counts should exist");
    assert_eq!(counts_after_release.processed, 1);
    assert_eq!(counts_after_release.unprocessed, 0);
    assert_eq!(counts_after_release.failed, 0);
    assert_eq!(counts_after_release.ignored, 1);
    assert_eq!(counts_after_release.missing, 0);
    let child_values = queue
        .get_flow_children_values(&flow.parent.id)
        .await
        .expect("child values should load")
        .expect("child values should exist");
    assert_eq!(child_values.len(), 1);
    assert_eq!(
        child_values.get(&flow.children[1].id),
        Some(&serde_json::json!({ "ok": true }))
    );

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_defers_flow_parent_failure_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_deferred_flow_parent_failure(redis_url),
    )
    .await
    .expect("Redis deferred flow parent failure integration test timed out")
    .unwrap();
}

async fn run_deferred_flow_parent_failure(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-fail-parent")
        .expect("valid Redis URL should build the flow-fail-parent queue");
    let worker_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-fail-parent")
        .expect("valid Redis URL should build the flow-fail-parent worker");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let flow = queue
        .add_flow_at(
            JobSpec::new(
                "deferred-failure-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new(
                    "deferred-failure-required-child",
                    serde_json::json!({ "required": true }),
                )
                .with_options(
                    JobOptions::new()
                        .with_priority(1)
                        .with_fail_parent_on_failure(true),
                ),
                JobSpec::new(
                    "deferred-failure-still-pending-child",
                    serde_json::json!({ "required": true }),
                )
                .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("deferred failure flow should be added");
    let dependency_key = format!(
        "{namespace}:flow-fail-parent:dependencies:{}",
        flow.parent.id
    );

    let failing_child = worker_queue
        .claim_next(
            "worker-deferred-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("failing child claim should return")
        .expect("failing child should be claimable");
    assert_eq!(failing_child.id, flow.children[0].id);
    worker_queue
        .fail_job(
            &failing_child.id,
            lock_token(&failing_child),
            "required child failed".to_string(),
            Utc::now(),
        )
        .await
        .expect("failing child should fail");

    let failed_child_pending: bool = conn
        .sismember(&dependency_key, &flow.children[0].id)
        .await?;
    let required_child_pending: bool = conn
        .sismember(&dependency_key, &flow.children[1].id)
        .await?;
    assert!(!failed_child_pending);
    assert!(required_child_pending);

    let deferred_failure = format!("child job {} failed", flow.children[0].id);
    let parent_after_failure = queue
        .get_job(&flow.parent.id)
        .await
        .expect("parent after deferred failure should load")
        .expect("parent should exist");
    assert_eq!(parent_after_failure.state, JobState::Waiting);
    assert_eq!(
        parent_after_failure.deferred_failure.as_deref(),
        Some(deferred_failure.as_str())
    );
    assert!(parent_after_failure.failed_reason.is_none());
    let counts_after_failure = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("deferred failure counts should load")
        .expect("deferred failure counts should exist");
    assert_eq!(counts_after_failure.processed, 0);
    assert_eq!(counts_after_failure.unprocessed, 1);
    assert_eq!(counts_after_failure.failed, 1);
    assert_eq!(counts_after_failure.ignored, 0);
    assert_eq!(counts_after_failure.missing, 0);

    let processor_called = Arc::new(AtomicBool::new(false));
    let processor_called_for_processor = Arc::clone(&processor_called);
    let processor = Arc::new(job_processor_fn(move |_, _| {
        let processor_called = Arc::clone(&processor_called_for_processor);
        async move {
            processor_called.store(true, Ordering::SeqCst);
            Ok(serde_json::json!({ "unexpected": true }))
        }
    }));
    let backend: Arc<dyn JobQueueBackend> = Arc::new(worker_queue.clone());
    let worker = JobWorker::new(
        backend,
        processor,
        JobWorkerConfig::new("worker-deferred-parent").with_lease_renew_interval(Duration::ZERO),
    );
    let outcome = worker
        .run_once(Utc::now())
        .await
        .expect("deferred parent worker should run once");
    let failed_parent = match outcome {
        JobRunOutcome::Failed(job) => job,
        other => panic!("expected failed parent job, got {other:?}"),
    };
    assert_eq!(failed_parent.id, flow.parent.id);
    assert_eq!(failed_parent.state, JobState::Failed);
    assert_eq!(
        failed_parent.failed_reason.as_deref(),
        Some(deferred_failure.as_str())
    );
    assert!(!processor_called.load(Ordering::SeqCst));

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_paginates_flow_dependencies_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_paginated_flow_dependencies(redis_url),
    )
    .await
    .expect("Redis paginated flow dependency integration test timed out")
    .unwrap();
}

async fn run_paginated_flow_dependencies(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-pages")
        .expect("valid Redis URL should build the flow-pages queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-pages")
        .expect("valid Redis URL should build the flow-pages worker");

    let flow = queue
        .add_flow_at(
            JobSpec::new("page-parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(50)),
            vec![
                JobSpec::new("processed-a", serde_json::json!({ "slot": "processed-a" }))
                    .with_options(JobOptions::new().with_priority(1)),
                JobSpec::new("processed-b", serde_json::json!({ "slot": "processed-b" }))
                    .with_options(JobOptions::new().with_priority(2)),
                JobSpec::new("ignored", serde_json::json!({ "slot": "ignored" })).with_options(
                    JobOptions::new()
                        .with_priority(3)
                        .with_ignore_dependency_on_failure(true),
                ),
                JobSpec::new("failed-a", serde_json::json!({ "slot": "failed-a" })).with_options(
                    JobOptions::new()
                        .with_priority(4)
                        .with_fail_parent_on_failure(true),
                ),
                JobSpec::new("failed-b", serde_json::json!({ "slot": "failed-b" })).with_options(
                    JobOptions::new()
                        .with_priority(5)
                        .with_fail_parent_on_failure(true),
                ),
                JobSpec::new("pending", serde_json::json!({ "slot": "pending" }))
                    .with_options(JobOptions::new().with_priority(6)),
            ],
            ts(1_000),
        )
        .await
        .expect("paginated dependency flow should be added");

    for index in 0..2 {
        let child = worker
            .claim_next(
                "worker-pages".to_string(),
                Duration::from_secs(30),
                ts(1_100 + index),
            )
            .await
            .expect("processed child claim should return")
            .expect("processed child should be claimable");
        assert_eq!(child.id, flow.children[index as usize].id);
        worker
            .complete_job(
                &child.id,
                lock_token(&child),
                serde_json::json!({ "done": index }),
                ts(1_200 + index),
            )
            .await
            .expect("processed child should complete");
    }

    let ignored = worker
        .claim_next(
            "worker-pages".to_string(),
            Duration::from_secs(30),
            ts(1_300),
        )
        .await
        .expect("ignored child claim should return")
        .expect("ignored child should be claimable");
    assert_eq!(ignored.id, flow.children[2].id);
    worker
        .fail_job(
            &ignored.id,
            lock_token(&ignored),
            "optional child failed".to_string(),
            ts(1_400),
        )
        .await
        .expect("ignored child should fail");

    for index in 3..5 {
        let child = worker
            .claim_next(
                "worker-pages".to_string(),
                Duration::from_secs(30),
                ts(1_500 + index),
            )
            .await
            .expect("failed child claim should return")
            .expect("failed child should be claimable");
        assert_eq!(child.id, flow.children[index as usize].id);
        worker
            .fail_job(
                &child.id,
                lock_token(&child),
                format!("required child {index} failed"),
                ts(1_600 + index),
            )
            .await
            .expect("failed child should fail");
    }

    let processed_page = queue
        .get_flow_dependency_page(
            &flow.parent.id,
            JobFlowDependencyPageOptions::new(JobFlowDependencyKind::Processed).with_count(20),
        )
        .await
        .expect("processed dependency page should load")
        .expect("processed dependency page should exist");
    assert_eq!(processed_page.kind, JobFlowDependencyKind::Processed);
    assert_eq!(processed_page.count, 20);
    let processed_items = processed_page
        .items
        .iter()
        .map(|item| match item {
            JobFlowDependencyPageItem::Processed { child_id, value } => {
                (child_id.clone(), value["done"].as_i64().unwrap())
            }
            other => panic!("unexpected processed dependency item: {other:?}"),
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        processed_items,
        BTreeSet::from([
            (flow.children[0].id.clone(), 0),
            (flow.children[1].id.clone(), 1),
        ])
    );

    let unprocessed_page = queue
        .get_flow_dependency_page(
            &flow.parent.id,
            JobFlowDependencyPageOptions::new(JobFlowDependencyKind::Unprocessed).with_count(20),
        )
        .await
        .expect("unprocessed dependency page should load")
        .expect("unprocessed dependency page should exist");
    assert_eq!(unprocessed_page.kind, JobFlowDependencyKind::Unprocessed);
    let unprocessed_items = unprocessed_page
        .items
        .iter()
        .map(|item| match item {
            JobFlowDependencyPageItem::Unprocessed { child_id } => child_id.clone(),
            other => panic!("unexpected unprocessed dependency item: {other:?}"),
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        unprocessed_items,
        BTreeSet::from([flow.children[5].id.clone()])
    );

    let ignored_page = queue
        .get_flow_dependency_page(
            &flow.parent.id,
            JobFlowDependencyPageOptions::new(JobFlowDependencyKind::Ignored).with_count(20),
        )
        .await
        .expect("ignored dependency page should load")
        .expect("ignored dependency page should exist");
    assert_eq!(ignored_page.kind, JobFlowDependencyKind::Ignored);
    assert_eq!(
        ignored_page.items,
        vec![JobFlowDependencyPageItem::Ignored {
            child_id: flow.children[2].id.clone(),
            failed_reason: "optional child failed".to_string(),
        }]
    );

    let values = queue
        .get_flow_dependency_values(&flow.parent.id)
        .await
        .expect("dependency values should load")
        .expect("dependency values should exist");
    assert_eq!(
        values.processed.get(&flow.children[0].id),
        Some(&serde_json::json!({ "done": 0 }))
    );
    assert_eq!(
        values.processed.get(&flow.children[1].id),
        Some(&serde_json::json!({ "done": 1 }))
    );
    let values_unprocessed = values.unprocessed.into_iter().collect::<BTreeSet<_>>();
    assert_eq!(
        values_unprocessed,
        BTreeSet::from([flow.children[5].id.clone()])
    );
    assert_eq!(
        values.ignored.get(&flow.children[2].id).map(String::as_str),
        Some("optional child failed")
    );
    assert_eq!(
        values.failed,
        vec![flow.children[3].id.clone(), flow.children[4].id.clone()]
    );

    let selected_counts = queue
        .get_flow_dependency_selected_counts(
            &flow.parent.id,
            JobFlowDependencyCountOptions::new()
                .with_processed(true)
                .with_failed(true),
        )
        .await
        .expect("selected dependency counts should load")
        .expect("selected dependency counts should exist");
    assert_eq!(selected_counts.processed, Some(2));
    assert_eq!(selected_counts.failed, Some(2));
    assert_eq!(selected_counts.unprocessed, None);
    assert_eq!(selected_counts.ignored, None);

    let failed_first = queue
        .get_flow_dependency_page(
            &flow.parent.id,
            JobFlowDependencyPageOptions::new(JobFlowDependencyKind::Failed).with_count(1),
        )
        .await
        .expect("first failed dependency page should load")
        .expect("first failed dependency page should exist");
    assert_eq!(failed_first.kind, JobFlowDependencyKind::Failed);
    assert_eq!(failed_first.next_cursor, 1);
    assert_eq!(
        failed_first.items,
        vec![JobFlowDependencyPageItem::Failed {
            child_id: flow.children[3].id.clone(),
        }]
    );

    let failed_second = queue
        .get_flow_dependency_page(
            &flow.parent.id,
            JobFlowDependencyPageOptions::new(JobFlowDependencyKind::Failed)
                .with_cursor(failed_first.next_cursor)
                .with_count(1),
        )
        .await
        .expect("second failed dependency page should load")
        .expect("second failed dependency page should exist");
    assert_eq!(failed_second.next_cursor, 0);
    assert_eq!(
        failed_second.items,
        vec![JobFlowDependencyPageItem::Failed {
            child_id: flow.children[4].id.clone(),
        }]
    );

    let pages = queue
        .get_flow_dependency_pages(
            &flow.parent.id,
            JobFlowDependencyPagesOptions::new()
                .with_processed(JobFlowDependencyPageCursor::new().with_count(20))
                .with_unprocessed(JobFlowDependencyPageCursor::new().with_count(20))
                .with_ignored(JobFlowDependencyPageCursor::new().with_count(20))
                .with_failed(JobFlowDependencyPageCursor::new().with_count(1)),
        )
        .await
        .expect("multi dependency pages should load")
        .expect("multi dependency pages should exist");
    let multi_processed = pages
        .get(JobFlowDependencyKind::Processed)
        .expect("processed page should be present")
        .items
        .iter()
        .map(|item| match item {
            JobFlowDependencyPageItem::Processed { child_id, value } => {
                (child_id.clone(), value["done"].as_i64().unwrap())
            }
            other => panic!("unexpected multi processed dependency item: {other:?}"),
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        multi_processed,
        BTreeSet::from([
            (flow.children[0].id.clone(), 0),
            (flow.children[1].id.clone(), 1),
        ])
    );
    assert_eq!(
        pages
            .get(JobFlowDependencyKind::Unprocessed)
            .expect("unprocessed page should be present")
            .items,
        vec![JobFlowDependencyPageItem::Unprocessed {
            child_id: flow.children[5].id.clone(),
        }]
    );
    assert_eq!(
        pages
            .get(JobFlowDependencyKind::Ignored)
            .expect("ignored page should be present")
            .items,
        vec![JobFlowDependencyPageItem::Ignored {
            child_id: flow.children[2].id.clone(),
            failed_reason: "optional child failed".to_string(),
        }]
    );
    let multi_failed = pages
        .get(JobFlowDependencyKind::Failed)
        .expect("failed page should be present");
    assert_eq!(multi_failed.next_cursor, 1);
    assert_eq!(
        multi_failed.items,
        vec![JobFlowDependencyPageItem::Failed {
            child_id: flow.children[3].id.clone(),
        }]
    );

    assert!(queue
        .get_flow_dependency_page(
            "missing-parent",
            JobFlowDependencyPageOptions::new(JobFlowDependencyKind::Processed),
        )
        .await
        .expect("missing parent page lookup should return")
        .is_none());

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_restores_flow_dependency_on_retry_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_retry_restores_flow_dependency(redis_url),
    )
    .await
    .expect("Redis retry flow dependency restoration integration test timed out")
    .unwrap();
}

async fn run_retry_restores_flow_dependency(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-retry-restore")
        .expect("valid Redis URL should build the flow-retry-restore queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-retry-restore")
        .expect("valid Redis URL should build the flow-retry-restore worker");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let flow = queue
        .add_flow_at(
            JobSpec::new(
                "retry-restore-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new(
                    "retry-restore-child",
                    serde_json::json!({ "retryable": true }),
                )
                .with_options(
                    JobOptions::new()
                        .with_priority(2)
                        .with_fail_parent_on_failure(true),
                ),
                JobSpec::new(
                    "retry-restore-required-child",
                    serde_json::json!({ "required": true }),
                )
                .with_options(JobOptions::new().with_priority(1)),
            ],
            Utc::now(),
        )
        .await
        .expect("retry restoration flow should be added");
    let dependency_key = format!(
        "{namespace}:flow-retry-restore:dependencies:{}",
        flow.parent.id
    );

    let required_child = worker
        .claim_next(
            "worker-retry-restore-required".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("required child claim should return")
        .expect("required child should be claimable");
    assert_eq!(required_child.id, flow.children[1].id);
    worker
        .complete_job(
            &required_child.id,
            lock_token(&required_child),
            serde_json::json!({ "required": "done" }),
            Utc::now(),
        )
        .await
        .expect("required child should complete before retryable child fails");
    let required_child_pending: bool = conn
        .sismember(&dependency_key, &flow.children[1].id)
        .await?;
    assert!(!required_child_pending);

    let retryable_child = worker
        .claim_next(
            "worker-retry-restore-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("retryable child claim should return")
        .expect("retryable child should be claimable");
    assert_eq!(retryable_child.id, flow.children[0].id);
    worker
        .fail_job(
            &retryable_child.id,
            lock_token(&retryable_child),
            "retryable child failed".to_string(),
            Utc::now(),
        )
        .await
        .expect("retryable child should fail");

    let failed_child_pending: bool = conn
        .sismember(&dependency_key, &flow.children[0].id)
        .await?;
    let required_child_pending: bool = conn
        .sismember(&dependency_key, &flow.children[1].id)
        .await?;
    assert!(!failed_child_pending);
    assert!(!required_child_pending);
    let parent_after_failure = queue
        .get_job(&flow.parent.id)
        .await
        .expect("parent after deferred failure should load")
        .expect("parent should exist");
    assert_eq!(parent_after_failure.state, JobState::Waiting);
    assert!(parent_after_failure.deferred_failure.is_some());

    let retried_child = queue
        .retry_job(&flow.children[0].id, Utc::now())
        .await
        .expect("failed child should retry");
    assert_eq!(retried_child.state, JobState::Waiting);
    let retried_child_pending: bool = conn
        .sismember(&dependency_key, &flow.children[0].id)
        .await?;
    let required_child_pending: bool = conn
        .sismember(&dependency_key, &flow.children[1].id)
        .await?;
    assert!(retried_child_pending);
    assert!(!required_child_pending);

    let parent_after_retry = queue
        .get_job(&flow.parent.id)
        .await
        .expect("parent after child retry should load")
        .expect("parent should exist");
    assert_eq!(parent_after_retry.state, JobState::WaitingChildren);
    assert!(parent_after_retry.deferred_failure.is_none());
    assert!(parent_after_retry.failed_reason.is_none());
    let parent_waiting_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:flow-retry-restore:waiting"),
            &flow.parent.id,
        )
        .await?;
    assert!(parent_waiting_score.is_none());
    let parent_waiting_children_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:flow-retry-restore:waiting_children"),
            &flow.parent.id,
        )
        .await?;
    assert!(parent_waiting_children_score.is_some());
    let counts_after_retry = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("retry restoration counts should load")
        .expect("retry restoration counts should exist");
    assert_eq!(counts_after_retry.processed, 1);
    assert_eq!(counts_after_retry.unprocessed, 1);
    assert_eq!(counts_after_retry.failed, 0);
    assert_eq!(counts_after_retry.ignored, 0);
    assert_eq!(counts_after_retry.missing, 0);

    let next_job = worker
        .claim_next(
            "worker-retry-restore-after".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("claim after retry should return")
        .expect("retried child should be claimable");
    assert_eq!(next_job.id, flow.children[0].id);
    worker
        .complete_job(
            &next_job.id,
            lock_token(&next_job),
            serde_json::json!({ "retryable": "done" }),
            Utc::now(),
        )
        .await
        .expect("retried child should complete");

    let parent = worker
        .claim_next(
            "worker-retry-restore-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("parent claim after retry restoration should return")
        .expect("parent should be claimable after retried child completes");
    assert_eq!(parent.id, flow.parent.id);

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_guards_flow_parent_completion_with_unsuccessful_index_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_flow_parent_unsuccessful_dependency_index(redis_url),
    )
    .await
    .expect("Redis unsuccessful dependency index integration test timed out")
    .unwrap();
}

async fn run_flow_parent_unsuccessful_dependency_index(
    redis_url: String,
) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let guard_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-unsuccessful-guard")
            .expect("valid Redis URL should build the flow-unsuccessful-guard queue");
    let guard_worker =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-unsuccessful-guard")
            .expect("valid Redis URL should build the flow-unsuccessful-guard worker");
    let retry_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-unsuccessful-retry")
            .expect("valid Redis URL should build the flow-unsuccessful-retry queue");
    let retry_worker =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-unsuccessful-retry")
            .expect("valid Redis URL should build the flow-unsuccessful-retry worker");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let guard_flow = guard_queue
        .add_flow_at(
            JobSpec::new(
                "unsuccessful-guard-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![JobSpec::new(
                "unsuccessful-guard-child",
                serde_json::json!({ "required": true }),
            )
            .with_options(
                JobOptions::new()
                    .with_priority(1)
                    .with_fail_parent_on_failure(true),
            )],
            Utc::now(),
        )
        .await
        .expect("unsuccessful guard flow should be added");
    let guard_unsuccessful_key = format!(
        "{namespace}:flow-unsuccessful-guard:dependencies:{}:unsuccessful",
        guard_flow.parent.id
    );

    let guard_child = guard_worker
        .claim_next(
            "worker-unsuccessful-guard-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("unsuccessful guard child claim should return")
        .expect("unsuccessful guard child should be claimable");
    assert_eq!(guard_child.id, guard_flow.children[0].id);
    guard_worker
        .fail_job(
            &guard_child.id,
            lock_token(&guard_child),
            "required child failed".to_string(),
            Utc::now(),
        )
        .await
        .expect("unsuccessful guard child should fail");

    let guard_unsuccessful_score: Option<f64> = conn
        .zscore(&guard_unsuccessful_key, &guard_flow.children[0].id)
        .await?;
    assert!(guard_unsuccessful_score.is_some());

    let guard_parent = guard_worker
        .claim_next(
            "worker-unsuccessful-guard-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("unsuccessful guard parent claim should return")
        .expect("unsuccessful guard parent should be claimable");
    assert_eq!(guard_parent.id, guard_flow.parent.id);
    let complete_error = guard_worker
        .complete_job(
            &guard_parent.id,
            lock_token(&guard_parent),
            serde_json::json!({ "unexpected": true }),
            Utc::now(),
        )
        .await
        .expect_err("unsuccessful parent should not complete");
    assert!(matches!(
        complete_error,
        LaneError::JobStateConflict(message) if message.contains("failed flow dependencies")
    ));

    let fanout_error = guard_queue
        .add_flow_children_at(
            &guard_parent.id,
            lock_token(&guard_parent),
            vec![
                JobSpec::new("late-child", serde_json::json!({ "unexpected": true }))
                    .with_options(JobOptions::new().with_job_id("unsuccessful-guard-late-child")),
            ],
            Utc::now(),
        )
        .await
        .expect_err("unsuccessful parent should not fan out new children");
    assert!(matches!(
        fanout_error,
        LaneError::JobStateConflict(message) if message.contains("failed flow dependencies")
    ));
    let late_child = guard_queue
        .get_job("unsuccessful-guard-late-child")
        .await
        .expect("late child lookup should load");
    assert!(late_child.is_none());
    let guard_parent_after_fanout = guard_queue
        .get_job(&guard_parent.id)
        .await
        .expect("guard parent after fan-out rejection should load")
        .expect("guard parent after fan-out rejection should exist");
    assert_eq!(guard_parent_after_fanout.state, JobState::Active);
    assert_eq!(
        guard_parent_after_fanout.lock_token.as_deref(),
        Some(lock_token(&guard_parent))
    );
    let guard_parent_active_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:flow-unsuccessful-guard:active"),
            &guard_parent.id,
        )
        .await?;
    assert!(guard_parent_active_score.is_some());
    let guard_parent_waiting_children_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:flow-unsuccessful-guard:waiting_children"),
            &guard_parent.id,
        )
        .await?;
    assert!(guard_parent_waiting_children_score.is_none());
    let guard_parent_lock: Option<String> = conn
        .get(format!(
            "{namespace}:flow-unsuccessful-guard:locks:{}",
            guard_parent.id
        ))
        .await?;
    assert_eq!(
        guard_parent_lock.as_deref(),
        Some(lock_token(&guard_parent))
    );

    let cleared_unsuccessful_entries: usize = conn.del(&guard_unsuccessful_key).await?;
    assert_eq!(cleared_unsuccessful_entries, 1);
    let legacy_fanout_error = guard_queue
        .add_flow_children_at(
            &guard_parent.id,
            lock_token(&guard_parent),
            vec![JobSpec::new(
                "legacy-late-child",
                serde_json::json!({ "unexpected": true }),
            )
            .with_options(JobOptions::new().with_job_id("unsuccessful-guard-legacy-late-child"))],
            Utc::now(),
        )
        .await
        .expect_err("failed child snapshots should block legacy dynamic fan-out");
    assert!(matches!(
        legacy_fanout_error,
        LaneError::JobStateConflict(message) if message.contains("failed flow dependencies")
    ));
    let legacy_late_child = guard_queue
        .get_job("unsuccessful-guard-legacy-late-child")
        .await
        .expect("legacy late child lookup should load");
    assert!(legacy_late_child.is_none());
    let guard_parent_after_legacy_fanout = guard_queue
        .get_job(&guard_parent.id)
        .await
        .expect("guard parent after legacy fan-out rejection should load")
        .expect("guard parent after legacy fan-out rejection should exist");
    assert_eq!(guard_parent_after_legacy_fanout.state, JobState::Active);
    assert_eq!(
        guard_parent_after_legacy_fanout.lock_token.as_deref(),
        Some(lock_token(&guard_parent))
    );

    let retry_flow = retry_queue
        .add_flow_at(
            JobSpec::new(
                "unsuccessful-retry-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![JobSpec::new(
                "unsuccessful-retry-child",
                serde_json::json!({ "retryable": true }),
            )
            .with_options(
                JobOptions::new()
                    .with_priority(1)
                    .with_fail_parent_on_failure(true),
            )],
            Utc::now(),
        )
        .await
        .expect("unsuccessful retry flow should be added");
    let retry_dependency_key = format!(
        "{namespace}:flow-unsuccessful-retry:dependencies:{}",
        retry_flow.parent.id
    );
    let retry_unsuccessful_key = format!("{retry_dependency_key}:unsuccessful");

    let retry_child = retry_worker
        .claim_next(
            "worker-unsuccessful-retry-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("unsuccessful retry child claim should return")
        .expect("unsuccessful retry child should be claimable");
    assert_eq!(retry_child.id, retry_flow.children[0].id);
    retry_worker
        .fail_job(
            &retry_child.id,
            lock_token(&retry_child),
            "retryable child failed".to_string(),
            Utc::now(),
        )
        .await
        .expect("unsuccessful retry child should fail");

    let retry_unsuccessful_score: Option<f64> = conn
        .zscore(&retry_unsuccessful_key, &retry_flow.children[0].id)
        .await?;
    assert!(retry_unsuccessful_score.is_some());

    let retried_child = retry_queue
        .retry_job(&retry_flow.children[0].id, Utc::now())
        .await
        .expect("unsuccessful child should retry");
    assert_eq!(retried_child.state, JobState::Waiting);
    let cleared_unsuccessful_score: Option<f64> = conn
        .zscore(&retry_unsuccessful_key, &retry_flow.children[0].id)
        .await?;
    assert!(cleared_unsuccessful_score.is_none());
    let restored_dependency: bool = conn
        .sismember(&retry_dependency_key, &retry_flow.children[0].id)
        .await?;
    assert!(restored_dependency);
    let retry_parent = retry_queue
        .get_job(&retry_flow.parent.id)
        .await
        .expect("retry parent should load")
        .expect("retry parent should exist");
    assert_eq!(retry_parent.state, JobState::WaitingChildren);
    assert!(retry_parent.deferred_failure.is_none());

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_writes_flow_parent_dependency_side_indexes_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_flow_parent_dependency_side_indexes(redis_url),
    )
    .await
    .expect("Redis flow dependency side-index integration test timed out")
    .unwrap();
}

async fn run_flow_parent_dependency_side_indexes(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-side-indexes")
        .expect("valid Redis URL should build the flow-side-indexes queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-side-indexes")
        .expect("valid Redis URL should build the flow-side-indexes worker");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let flow = queue
        .add_flow_at(
            JobSpec::new(
                "side-index-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("side-index-completed-child", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(1)),
                JobSpec::new("side-index-ignored-child", serde_json::json!({ "n": 2 }))
                    .with_options(
                        JobOptions::new()
                            .with_priority(2)
                            .with_ignore_dependency_on_failure(true),
                    ),
            ],
            Utc::now(),
        )
        .await
        .expect("side-index flow should be added");
    let dependency_key = format!(
        "{namespace}:flow-side-indexes:dependencies:{}",
        flow.parent.id
    );
    let processed_key = format!("{dependency_key}:processed");
    let failed_key = format!("{dependency_key}:failed");

    let completed_child = worker
        .claim_next(
            "worker-side-index-completed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("completed child claim should return")
        .expect("completed child should be claimable");
    assert_eq!(completed_child.id, flow.children[0].id);
    worker
        .complete_job(
            &completed_child.id,
            lock_token(&completed_child),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("completed child should complete");

    let raw_processed: Option<String> = conn.hget(&processed_key, &flow.children[0].id).await?;
    let processed_value = raw_processed
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .expect("processed child value should be JSON");
    assert_eq!(processed_value, Some(serde_json::json!({ "ok": true })));

    let ignored_child = worker
        .claim_next(
            "worker-side-index-ignored".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("ignored child claim should return")
        .expect("ignored child should be claimable");
    assert_eq!(ignored_child.id, flow.children[1].id);
    worker
        .fail_job(
            &ignored_child.id,
            lock_token(&ignored_child),
            "ignored side-index failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("ignored child should fail");

    let raw_ignored: Option<String> = conn.hget(&failed_key, &flow.children[1].id).await?;
    assert_eq!(raw_ignored.as_deref(), Some("ignored side-index failure"));

    let child_values = queue
        .get_flow_children_values(&flow.parent.id)
        .await
        .expect("children values should load")
        .expect("children values should exist");
    assert_eq!(
        child_values.get(&flow.children[0].id),
        Some(&serde_json::json!({ "ok": true }))
    );
    let ignored_failures = queue
        .get_flow_ignored_children_failures(&flow.parent.id)
        .await
        .expect("ignored failures should load")
        .expect("ignored failures should exist");
    assert_eq!(
        ignored_failures
            .get(&flow.children[1].id)
            .map(String::as_str),
        Some("ignored side-index failure")
    );
    let counts = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("dependency counts should load")
        .expect("dependency counts should exist");
    assert_eq!(counts.processed, 1);
    assert_eq!(counts.unprocessed, 0);
    assert_eq!(counts.failed, 0);
    assert_eq!(counts.ignored, 1);
    assert_eq!(counts.missing, 0);

    let retried_child = queue
        .retry_job(&flow.children[0].id, Utc::now())
        .await
        .expect("completed child retry should restore parent dependency");
    assert_eq!(retried_child.id, flow.children[0].id);

    let raw_processed_after_retry: Option<String> =
        conn.hget(&processed_key, &flow.children[0].id).await?;
    assert_eq!(raw_processed_after_retry, None);
    let child_values_after_retry = queue
        .get_flow_children_values(&flow.parent.id)
        .await
        .expect("children values should load after retry")
        .expect("children values should exist after retry");
    assert!(child_values_after_retry.is_empty());

    let counts_after_retry = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("dependency counts should load after retry")
        .expect("dependency counts should exist after retry");
    assert_eq!(counts_after_retry.processed, 0);
    assert_eq!(counts_after_retry.unprocessed, 1);
    assert_eq!(counts_after_retry.failed, 0);
    assert_eq!(counts_after_retry.ignored, 1);
    assert_eq!(counts_after_retry.missing, 0);

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_merges_flow_side_indexes_with_retained_snapshots_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_flow_side_index_snapshot_merge(redis_url),
    )
    .await
    .expect("Redis flow side-index merge integration test timed out")
    .unwrap();
}

async fn run_flow_side_index_snapshot_merge(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-side-index-merge")
        .expect("valid Redis URL should build the flow-side-index-merge queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-side-index-merge")
        .expect("valid Redis URL should build the flow-side-index-merge worker");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let flow = queue
        .add_flow_at(
            JobSpec::new("merge-parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("merge-completed-legacy", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(1)),
                JobSpec::new("merge-ignored-legacy", serde_json::json!({ "n": 2 })).with_options(
                    JobOptions::new()
                        .with_priority(2)
                        .with_ignore_dependency_on_failure(true),
                ),
                JobSpec::new("merge-completed-indexed", serde_json::json!({ "n": 3 }))
                    .with_options(JobOptions::new().with_priority(3)),
                JobSpec::new("merge-ignored-indexed", serde_json::json!({ "n": 4 })).with_options(
                    JobOptions::new()
                        .with_priority(4)
                        .with_ignore_dependency_on_failure(true),
                ),
                JobSpec::new("merge-completed-null-legacy", serde_json::json!({ "n": 5 }))
                    .with_options(JobOptions::new().with_priority(5)),
            ],
            Utc::now(),
        )
        .await
        .expect("merge flow should be added");
    let dependency_key = format!(
        "{namespace}:flow-side-index-merge:dependencies:{}",
        flow.parent.id
    );
    let processed_key = format!("{dependency_key}:processed");
    let failed_key = format!("{dependency_key}:failed");

    let completed_legacy = worker
        .claim_next(
            "worker-merge-completed-legacy".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("legacy completed child claim should return")
        .expect("legacy completed child should be claimable");
    assert_eq!(completed_legacy.id, flow.children[0].id);
    worker
        .complete_job(
            &completed_legacy.id,
            lock_token(&completed_legacy),
            serde_json::json!({ "value": "legacy-completed" }),
            Utc::now(),
        )
        .await
        .expect("legacy completed child should complete");

    let ignored_legacy = worker
        .claim_next(
            "worker-merge-ignored-legacy".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("legacy ignored child claim should return")
        .expect("legacy ignored child should be claimable");
    assert_eq!(ignored_legacy.id, flow.children[1].id);
    worker
        .fail_job(
            &ignored_legacy.id,
            lock_token(&ignored_legacy),
            "legacy ignored failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("legacy ignored child should fail");

    let completed_indexed = worker
        .claim_next(
            "worker-merge-completed-indexed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("indexed completed child claim should return")
        .expect("indexed completed child should be claimable");
    assert_eq!(completed_indexed.id, flow.children[2].id);
    worker
        .complete_job(
            &completed_indexed.id,
            lock_token(&completed_indexed),
            serde_json::json!({ "value": "indexed-completed" }),
            Utc::now(),
        )
        .await
        .expect("indexed completed child should complete");

    let ignored_indexed = worker
        .claim_next(
            "worker-merge-ignored-indexed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("indexed ignored child claim should return")
        .expect("indexed ignored child should be claimable");
    assert_eq!(ignored_indexed.id, flow.children[3].id);
    worker
        .fail_job(
            &ignored_indexed.id,
            lock_token(&ignored_indexed),
            "indexed ignored failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("indexed ignored child should fail");

    let completed_null_legacy = worker
        .claim_next(
            "worker-merge-completed-null-legacy".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("legacy null completed child claim should return")
        .expect("legacy null completed child should be claimable");
    assert_eq!(completed_null_legacy.id, flow.children[4].id);
    worker
        .complete_job(
            &completed_null_legacy.id,
            lock_token(&completed_null_legacy),
            serde_json::Value::Null,
            Utc::now(),
        )
        .await
        .expect("legacy null completed child should complete");

    let removed_processed: usize = conn.hdel(&processed_key, &flow.children[0].id).await?;
    let removed_null_processed: usize = conn.hdel(&processed_key, &flow.children[4].id).await?;
    let removed_failed: usize = conn.hdel(&failed_key, &flow.children[1].id).await?;
    assert_eq!(removed_processed, 1);
    assert_eq!(removed_null_processed, 1);
    assert_eq!(removed_failed, 1);
    let processed_side_index_len: usize = conn.hlen(&processed_key).await?;
    let failed_side_index_len: usize = conn.hlen(&failed_key).await?;
    assert_eq!(processed_side_index_len, 1);
    assert_eq!(failed_side_index_len, 1);

    let child_values = queue
        .get_flow_children_values(&flow.parent.id)
        .await
        .expect("merged child values should load")
        .expect("merged child values should exist");
    assert_eq!(
        child_values.get(&flow.children[0].id),
        Some(&serde_json::json!({ "value": "legacy-completed" }))
    );
    assert_eq!(
        child_values.get(&flow.children[2].id),
        Some(&serde_json::json!({ "value": "indexed-completed" }))
    );
    assert_eq!(
        child_values.get(&flow.children[4].id),
        Some(&serde_json::Value::Null)
    );

    let ignored_failures = queue
        .get_flow_ignored_children_failures(&flow.parent.id)
        .await
        .expect("merged ignored failures should load")
        .expect("merged ignored failures should exist");
    assert_eq!(
        ignored_failures
            .get(&flow.children[1].id)
            .map(String::as_str),
        Some("legacy ignored failure")
    );
    assert_eq!(
        ignored_failures
            .get(&flow.children[3].id)
            .map(String::as_str),
        Some("indexed ignored failure")
    );

    let counts = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("merged dependency counts should load")
        .expect("merged dependency counts should exist");
    assert_eq!(counts.processed, 3);
    assert_eq!(counts.unprocessed, 0);
    assert_eq!(counts.failed, 0);
    assert_eq!(counts.ignored, 2);
    assert_eq!(counts.missing, 0);

    let values = queue
        .get_flow_dependency_values(&flow.parent.id)
        .await
        .expect("merged dependency values should load")
        .expect("merged dependency values should exist");
    assert_eq!(
        values.processed.get(&flow.children[0].id),
        Some(&serde_json::json!({ "value": "legacy-completed" }))
    );
    assert_eq!(
        values.processed.get(&flow.children[2].id),
        Some(&serde_json::json!({ "value": "indexed-completed" }))
    );
    assert_eq!(
        values.processed.get(&flow.children[4].id),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(
        values.ignored.get(&flow.children[1].id).map(String::as_str),
        Some("legacy ignored failure")
    );
    assert_eq!(
        values.ignored.get(&flow.children[3].id).map(String::as_str),
        Some("indexed ignored failure")
    );
    assert!(values.unprocessed.is_empty());
    assert!(values.failed.is_empty());

    let processed_page = queue
        .get_flow_dependency_page(
            &flow.parent.id,
            JobFlowDependencyPageOptions::new(JobFlowDependencyKind::Processed).with_count(20),
        )
        .await
        .expect("merged processed dependency page should load")
        .expect("merged processed dependency page should exist");
    let processed_page_values = processed_page
        .items
        .iter()
        .map(|item| match item {
            JobFlowDependencyPageItem::Processed { child_id, value } => {
                (child_id.clone(), value.clone())
            }
            other => panic!("unexpected processed dependency page item: {other:?}"),
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        processed_page_values,
        BTreeMap::from([
            (
                flow.children[0].id.clone(),
                serde_json::json!({ "value": "legacy-completed" }),
            ),
            (
                flow.children[2].id.clone(),
                serde_json::json!({ "value": "indexed-completed" }),
            ),
            (flow.children[4].id.clone(), serde_json::Value::Null),
        ])
    );

    let ignored_page = queue
        .get_flow_dependency_page(
            &flow.parent.id,
            JobFlowDependencyPageOptions::new(JobFlowDependencyKind::Ignored).with_count(20),
        )
        .await
        .expect("merged ignored dependency page should load")
        .expect("merged ignored dependency page should exist");
    let ignored_page_values = ignored_page
        .items
        .iter()
        .map(|item| match item {
            JobFlowDependencyPageItem::Ignored {
                child_id,
                failed_reason,
            } => (child_id.clone(), failed_reason.clone()),
            other => panic!("unexpected ignored dependency page item: {other:?}"),
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        ignored_page_values,
        BTreeSet::from([
            (
                flow.children[1].id.clone(),
                "legacy ignored failure".to_string()
            ),
            (
                flow.children[3].id.clone(),
                "indexed ignored failure".to_string()
            ),
        ])
    );

    let pages = queue
        .get_flow_dependency_pages(
            &flow.parent.id,
            JobFlowDependencyPagesOptions::new()
                .with_processed(JobFlowDependencyPageCursor::new().with_count(20))
                .with_ignored(JobFlowDependencyPageCursor::new().with_count(20)),
        )
        .await
        .expect("merged dependency pages should load")
        .expect("merged dependency pages should exist");
    let multi_processed_values = pages
        .get(JobFlowDependencyKind::Processed)
        .expect("processed multi page should exist")
        .items
        .iter()
        .map(|item| match item {
            JobFlowDependencyPageItem::Processed { child_id, value } => {
                (child_id.clone(), value.clone())
            }
            other => panic!("unexpected multi processed dependency item: {other:?}"),
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(multi_processed_values, processed_page_values);
    let multi_ignored_values = pages
        .get(JobFlowDependencyKind::Ignored)
        .expect("ignored multi page should exist")
        .items
        .iter()
        .map(|item| match item {
            JobFlowDependencyPageItem::Ignored {
                child_id,
                failed_reason,
            } => (child_id.clone(), failed_reason.clone()),
            other => panic!("unexpected multi ignored dependency item: {other:?}"),
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(multi_ignored_values, ignored_page_values);

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_indexes_reused_completed_flow_children_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_reused_completed_flow_child_side_indexes(redis_url),
    )
    .await
    .expect("Redis reused completed flow-child side-index integration test timed out")
    .unwrap();
}

async fn run_reused_completed_flow_child_side_indexes(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let static_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-reuse-completed-static")
            .expect("valid Redis URL should build the static reuse queue");
    let static_worker =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-reuse-completed-static")
            .expect("valid Redis URL should build the static reuse worker");
    let static_existing = static_queue
        .add_job(
            "completed-static-child".to_string(),
            serde_json::json!({ "kind": "existing" }),
            JobOptions::new().with_job_id("flow-reuse:static:completed"),
        )
        .await
        .expect("static existing child should add");
    let static_existing_claim = static_worker
        .claim_next(
            "worker-static-existing".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("static existing child claim should return")
        .expect("static existing child should be claimable");
    static_worker
        .complete_job(
            &static_existing_claim.id,
            lock_token(&static_existing_claim),
            serde_json::json!({ "reused": "static" }),
            Utc::now(),
        )
        .await
        .expect("static existing child should complete");

    let static_flow = static_queue
        .add_flow_at(
            JobSpec::new("static-parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("candidate-static-child", serde_json::json!({}))
                    .with_options(JobOptions::new().with_job_id(static_existing.id.clone())),
                JobSpec::new("fresh-static-child", serde_json::json!({ "fresh": true }))
                    .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("static flow should reuse a completed child");
    let static_dependency_key = format!(
        "{namespace}:flow-reuse-completed-static:dependencies:{}",
        static_flow.parent.id
    );
    let static_processed_key = format!("{static_dependency_key}:processed");
    let raw_static_reused: Option<String> = conn
        .hget(&static_processed_key, &static_existing.id)
        .await?;
    assert_eq!(
        raw_static_reused
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()
            .expect("static reused child value should be JSON"),
        Some(serde_json::json!({ "reused": "static" }))
    );
    let static_counts = static_queue
        .get_flow_dependency_counts(&static_flow.parent.id)
        .await
        .expect("static dependency counts should load")
        .expect("static dependency counts should exist");
    assert_eq!(static_counts.processed, 1);
    assert_eq!(static_counts.unprocessed, 1);
    let static_initial_values = static_queue
        .get_flow_children_values(&static_flow.parent.id)
        .await
        .expect("static child values should load")
        .expect("static child values should exist");
    assert_eq!(
        static_initial_values.get(&static_existing.id),
        Some(&serde_json::json!({ "reused": "static" }))
    );

    let static_fresh_child = static_worker
        .claim_next(
            "worker-static-fresh".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("static fresh child claim should return")
        .expect("static fresh child should be claimable");
    assert_eq!(static_fresh_child.id, static_flow.children[1].id);
    static_worker
        .complete_job(
            &static_fresh_child.id,
            lock_token(&static_fresh_child),
            serde_json::json!({ "fresh": "static" }),
            Utc::now(),
        )
        .await
        .expect("static fresh child should complete");
    let static_values = static_queue
        .get_flow_children_values(&static_flow.parent.id)
        .await
        .expect("static child values after release should load")
        .expect("static child values after release should exist");
    assert_eq!(
        static_values.get(&static_existing.id),
        Some(&serde_json::json!({ "reused": "static" }))
    );
    assert_eq!(
        static_values.get(&static_fresh_child.id),
        Some(&serde_json::json!({ "fresh": "static" }))
    );

    let dynamic_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-reuse-completed-dynamic")
            .expect("valid Redis URL should build the dynamic reuse queue");
    let dynamic_worker =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-reuse-completed-dynamic")
            .expect("valid Redis URL should build the dynamic reuse worker");
    let dynamic_existing = dynamic_queue
        .add_job(
            "completed-dynamic-child".to_string(),
            serde_json::json!({ "kind": "existing" }),
            JobOptions::new().with_job_id("flow-reuse:dynamic:completed"),
        )
        .await
        .expect("dynamic existing child should add");
    let dynamic_existing_claim = dynamic_worker
        .claim_next(
            "worker-dynamic-existing".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic existing child claim should return")
        .expect("dynamic existing child should be claimable");
    dynamic_worker
        .complete_job(
            &dynamic_existing_claim.id,
            lock_token(&dynamic_existing_claim),
            serde_json::json!({ "reused": "dynamic" }),
            Utc::now(),
        )
        .await
        .expect("dynamic existing child should complete");
    let dynamic_parent = dynamic_queue
        .add_job(
            "dynamic-parent".to_string(),
            serde_json::json!({ "kind": "planner" }),
            JobOptions::new().with_priority(1),
        )
        .await
        .expect("dynamic parent should add");
    let active_dynamic_parent = dynamic_worker
        .claim_next(
            "worker-dynamic-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic parent claim should return")
        .expect("dynamic parent should be claimable");
    assert_eq!(active_dynamic_parent.id, dynamic_parent.id);
    let dynamic_children = dynamic_queue
        .add_flow_children_at(
            &active_dynamic_parent.id,
            lock_token(&active_dynamic_parent),
            vec![
                JobSpec::new("candidate-dynamic-child", serde_json::json!({}))
                    .with_options(JobOptions::new().with_job_id(dynamic_existing.id.clone())),
                JobSpec::new("fresh-dynamic-child", serde_json::json!({ "fresh": true }))
                    .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("dynamic flow children should reuse a completed child");
    let dynamic_dependency_key = format!(
        "{namespace}:flow-reuse-completed-dynamic:dependencies:{}",
        dynamic_parent.id
    );
    let dynamic_processed_key = format!("{dynamic_dependency_key}:processed");
    let raw_dynamic_reused: Option<String> = conn
        .hget(&dynamic_processed_key, &dynamic_existing.id)
        .await?;
    assert_eq!(
        raw_dynamic_reused
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()
            .expect("dynamic reused child value should be JSON"),
        Some(serde_json::json!({ "reused": "dynamic" }))
    );
    let dynamic_counts = dynamic_queue
        .get_flow_dependency_counts(&dynamic_parent.id)
        .await
        .expect("dynamic dependency counts should load")
        .expect("dynamic dependency counts should exist");
    assert_eq!(dynamic_counts.processed, 1);
    assert_eq!(dynamic_counts.unprocessed, 1);
    let dynamic_initial_values = dynamic_queue
        .get_flow_children_values(&dynamic_parent.id)
        .await
        .expect("dynamic child values should load")
        .expect("dynamic child values should exist");
    assert_eq!(
        dynamic_initial_values.get(&dynamic_existing.id),
        Some(&serde_json::json!({ "reused": "dynamic" }))
    );

    let dynamic_fresh_child = dynamic_worker
        .claim_next(
            "worker-dynamic-fresh".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic fresh child claim should return")
        .expect("dynamic fresh child should be claimable");
    assert_eq!(dynamic_fresh_child.id, dynamic_children[1].id);
    dynamic_worker
        .complete_job(
            &dynamic_fresh_child.id,
            lock_token(&dynamic_fresh_child),
            serde_json::json!({ "fresh": "dynamic" }),
            Utc::now(),
        )
        .await
        .expect("dynamic fresh child should complete");
    let dynamic_values = dynamic_queue
        .get_flow_children_values(&dynamic_parent.id)
        .await
        .expect("dynamic child values after release should load")
        .expect("dynamic child values after release should exist");
    assert_eq!(
        dynamic_values.get(&dynamic_existing.id),
        Some(&serde_json::json!({ "reused": "dynamic" }))
    );
    assert_eq!(
        dynamic_values.get(&dynamic_fresh_child.id),
        Some(&serde_json::json!({ "fresh": "dynamic" }))
    );

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_adds_dynamic_flow_children_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_dynamic_flow_children(redis_url),
    )
    .await
    .expect("Redis dynamic flow children integration test timed out")
    .unwrap();
}

async fn run_dynamic_flow_children(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-dynamic")
        .expect("valid Redis URL should build the dynamic flow queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-dynamic")
        .expect("valid Redis URL should build the dynamic flow worker");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let parent = queue
        .add_job(
            "planner".to_string(),
            serde_json::json!({ "kind": "plan" }),
            JobOptions::new().with_priority(1),
        )
        .await
        .expect("dynamic flow parent should be added");
    let active_parent = worker
        .claim_next(
            "worker-dynamic-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic parent claim should return")
        .expect("dynamic parent should be claimable");
    assert_eq!(active_parent.id, parent.id);

    let children = queue
        .add_flow_children_at(
            &active_parent.id,
            lock_token(&active_parent),
            vec![
                JobSpec::new("planned-child-a", serde_json::json!({ "step": 1 }))
                    .with_options(JobOptions::new().with_priority(1)),
                JobSpec::new("planned-child-b", serde_json::json!({ "step": 2 }))
                    .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("dynamic flow children should be added");
    assert_eq!(children.len(), 2);

    let dependency_key = format!("{namespace}:flow-dynamic:dependencies:{}", parent.id);
    let pending_count: usize = conn.scard(&dependency_key).await?;
    assert_eq!(pending_count, 2);
    for child in &children {
        let pending: bool = conn.sismember(&dependency_key, &child.id).await?;
        assert!(pending);
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
    }

    let parent_active_score: Option<f64> = conn
        .zscore(format!("{namespace}:flow-dynamic:active"), &parent.id)
        .await?;
    assert!(parent_active_score.is_none());
    let parent_waiting_children_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:flow-dynamic:waiting_children"),
            &parent.id,
        )
        .await?;
    assert!(parent_waiting_children_score.is_some());
    let parent_lock_exists: usize = conn
        .exists(format!("{namespace}:flow-dynamic:locks:{}", parent.id))
        .await?;
    assert_eq!(parent_lock_exists, 0);

    let waiting_parent = queue
        .get_job(&parent.id)
        .await
        .expect("dynamic parent after fan-out should load")
        .expect("dynamic parent should exist");
    assert_eq!(waiting_parent.state, JobState::WaitingChildren);
    assert!(waiting_parent.worker_id.is_none());
    assert_eq!(
        waiting_parent.child_ids,
        children
            .iter()
            .map(|child| child.id.clone())
            .collect::<Vec<_>>()
    );

    for (index, child) in children.iter().enumerate() {
        let claimed = worker
            .claim_next(
                format!("worker-dynamic-child-{index}"),
                Duration::from_secs(30),
                Utc::now(),
            )
            .await
            .expect("dynamic child claim should return")
            .expect("dynamic child should be claimable");
        assert_eq!(claimed.id, child.id);
        worker
            .complete_job(
                &claimed.id,
                lock_token(&claimed),
                serde_json::json!({ "ok": index }),
                Utc::now(),
            )
            .await
            .expect("dynamic child should complete");
    }

    let dependency_exists: usize = conn.exists(&dependency_key).await?;
    assert_eq!(dependency_exists, 0);
    let released_parent = queue
        .get_job(&parent.id)
        .await
        .expect("dynamic parent after children should load")
        .expect("dynamic parent should exist");
    assert_eq!(released_parent.state, JobState::Waiting);
    let claimed_parent = worker
        .claim_next(
            "worker-dynamic-parent-after".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic parent claim after children should return")
        .expect("dynamic parent should be claimable after children complete");
    assert_eq!(claimed_parent.id, parent.id);

    let reuse_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-dynamic-reuse")
        .expect("valid Redis URL should build the dynamic reuse queue");
    let existing = reuse_queue
        .add_job(
            "existing-child".to_string(),
            serde_json::json!({ "original": true }),
            JobOptions::new().with_job_id("flow-dynamic:existing-child"),
        )
        .await
        .expect("existing dynamic child should add");
    let reuse_parent = reuse_queue
        .add_job(
            "planner".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_priority(1),
        )
        .await
        .expect("dynamic reuse parent should add");
    let active_reuse_parent = reuse_queue
        .claim_next(
            "worker-dynamic-reuse-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic reuse parent claim should return")
        .expect("dynamic reuse parent should be claimable");
    assert_eq!(active_reuse_parent.id, reuse_parent.id);
    let reused_children = reuse_queue
        .add_flow_children_at(
            &active_reuse_parent.id,
            lock_token(&active_reuse_parent),
            vec![
                JobSpec::new("candidate-child", serde_json::json!({ "candidate": true }))
                    .with_options(JobOptions::new().with_job_id(existing.id.clone())),
            ],
            Utc::now(),
        )
        .await
        .expect("dynamic flow should reuse existing child id");
    assert_eq!(reused_children.len(), 1);
    assert_eq!(reused_children[0].id, existing.id);
    assert_eq!(reused_children[0].name, "existing-child");
    assert_eq!(
        reused_children[0].payload,
        serde_json::json!({ "original": true })
    );
    assert_eq!(
        reused_children[0].parent_id.as_deref(),
        Some(reuse_parent.id.as_str())
    );
    let reuse_dependency_key = format!(
        "{namespace}:flow-dynamic-reuse:dependencies:{}",
        reuse_parent.id
    );
    let reuse_pending_count: usize = conn.scard(&reuse_dependency_key).await?;
    assert_eq!(reuse_pending_count, 1);
    let reuse_counts = reuse_queue
        .get_flow_dependency_counts(&reuse_parent.id)
        .await
        .expect("dynamic reuse dependency counts should load")
        .expect("dynamic reuse parent should exist");
    assert_eq!(reuse_counts.unprocessed, 1);
    assert_eq!(reuse_counts.processed, 0);
    let duplicated_events = reuse_queue
        .read_events("-", "+", 20)
        .await
        .expect("dynamic reuse events should read")
        .into_iter()
        .filter(|event| {
            event.event == "duplicated" && event.job_id.as_deref() == Some(existing.id.as_str())
        })
        .count();
    assert_eq!(duplicated_events, 1);
    let reused_claim = reuse_queue
        .claim_next(
            "worker-dynamic-reuse-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic reused child claim should return")
        .expect("dynamic reused child should be claimable");
    assert_eq!(reused_claim.id, existing.id);
    reuse_queue
        .complete_job(
            &reused_claim.id,
            lock_token(&reused_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("dynamic reused child should complete");
    assert_eq!(
        reuse_queue
            .get_job(&reuse_parent.id)
            .await
            .expect("dynamic reuse parent should load")
            .expect("dynamic reuse parent should exist")
            .state,
        JobState::Waiting
    );

    let completed_reuse_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-dynamic-completed")
            .expect("valid Redis URL should build the dynamic completed reuse queue");
    let completed_existing = completed_reuse_queue
        .add_job(
            "completed-child".to_string(),
            serde_json::json!({ "original": true }),
            JobOptions::new().with_job_id("flow-dynamic:completed-child"),
        )
        .await
        .expect("dynamic completed child should add");
    let completed_existing_claim = completed_reuse_queue
        .claim_next(
            "worker-dynamic-completed-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic completed child claim should return")
        .expect("dynamic completed child should be claimable");
    completed_reuse_queue
        .complete_job(
            &completed_existing_claim.id,
            lock_token(&completed_existing_claim),
            serde_json::json!({ "done": true }),
            Utc::now(),
        )
        .await
        .expect("dynamic completed child should finish");
    let completed_parent = completed_reuse_queue
        .add_job(
            "planner".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_priority(1),
        )
        .await
        .expect("dynamic completed parent should add");
    let active_completed_parent = completed_reuse_queue
        .claim_next(
            "worker-dynamic-completed-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic completed parent claim should return")
        .expect("dynamic completed parent should be claimable");
    assert_eq!(active_completed_parent.id, completed_parent.id);
    let completed_reused_children = completed_reuse_queue
        .add_flow_children_at(
            &active_completed_parent.id,
            lock_token(&active_completed_parent),
            vec![JobSpec::new("candidate-child", serde_json::json!({}))
                .with_options(JobOptions::new().with_job_id(completed_existing.id.clone()))],
            Utc::now(),
        )
        .await
        .expect("dynamic flow should reuse completed child id");
    assert_eq!(completed_reused_children.len(), 1);
    assert_eq!(completed_reused_children[0].id, completed_existing.id);
    assert_eq!(completed_reused_children[0].state, JobState::Completed);
    let completed_released_parent = completed_reuse_queue
        .get_job(&completed_parent.id)
        .await
        .expect("dynamic completed parent should load")
        .expect("dynamic completed parent should exist");
    assert_eq!(completed_released_parent.state, JobState::Waiting);
    assert_eq!(
        completed_released_parent.child_ids,
        vec![completed_existing.id.clone()]
    );
    let completed_reuse_counts = completed_reuse_queue
        .get_flow_dependency_counts(&completed_parent.id)
        .await
        .expect("dynamic completed dependency counts should load")
        .expect("dynamic completed parent should exist");
    assert_eq!(completed_reuse_counts.processed, 1);
    assert_eq!(completed_reuse_counts.unprocessed, 0);

    let dedup_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-dynamic-dedup")
        .expect("valid Redis URL should build the dynamic dedup queue");
    let dedup_owner = dedup_queue
        .add_job(
            "existing-child-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_job_id("flow-dynamic-dedup:owner")
                .with_deduplication_id("tenant:dynamic-flow-child"),
        )
        .await
        .expect("dynamic dedup owner should add");
    let dedup_parent = dedup_queue
        .add_job(
            "planner".to_string(),
            serde_json::json!({ "kind": "plan" }),
            JobOptions::new()
                .with_job_id("flow-dynamic-dedup:parent")
                .with_priority(1),
        )
        .await
        .expect("dynamic dedup parent should add");
    let active_dedup_parent = dedup_queue
        .claim_next(
            "worker-dynamic-dedup-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic dedup parent claim should return")
        .expect("dynamic dedup parent should be claimable");
    assert_eq!(active_dedup_parent.id, dedup_parent.id);
    let dedup_children = dedup_queue
        .add_flow_children_at(
            &active_dedup_parent.id,
            lock_token(&active_dedup_parent),
            vec![
                JobSpec::new("candidate-child", serde_json::json!({ "version": 2 })).with_options(
                    JobOptions::new()
                        .with_job_id("flow-dynamic-dedup:candidate")
                        .with_deduplication_id("tenant:dynamic-flow-child"),
                ),
                JobSpec::new("retained-child", serde_json::json!({ "version": 3 }))
                    .with_options(JobOptions::new().with_job_id("flow-dynamic-dedup:retained")),
            ],
            Utc::now(),
        )
        .await
        .expect("dynamic dedup children should add");
    assert_eq!(dedup_children.len(), 1);
    assert_eq!(dedup_children[0].id, "flow-dynamic-dedup:retained");
    assert!(dedup_queue
        .get_job("flow-dynamic-dedup:candidate")
        .await
        .expect("dynamic dedup candidate lookup should return")
        .is_none());
    assert_eq!(
        dedup_queue
            .get_job(&dedup_owner.id)
            .await
            .expect("dynamic dedup owner should load")
            .expect("dynamic dedup owner should exist")
            .parent_id,
        None
    );
    let dedup_dependency_key = format!(
        "{namespace}:flow-dynamic-dedup:dependencies:{}",
        dedup_parent.id
    );
    let dedup_pending_count: usize = conn.scard(&dedup_dependency_key).await?;
    assert_eq!(dedup_pending_count, 1);
    let candidate_is_dependency: bool = conn
        .sismember(&dedup_dependency_key, "flow-dynamic-dedup:candidate")
        .await?;
    assert!(!candidate_is_dependency);
    let dedup_parent_after = dedup_queue
        .get_job(&dedup_parent.id)
        .await
        .expect("dynamic dedup parent should load")
        .expect("dynamic dedup parent should exist");
    assert_eq!(
        dedup_parent_after.child_ids,
        vec!["flow-dynamic-dedup:retained".to_string()]
    );
    let dedup_events = dedup_queue
        .read_events("-", "+", 20)
        .await
        .expect("dynamic dedup events should read");
    assert!(dedup_events.iter().any(|event| {
        event.event == "debounced"
            && event.job_id.as_deref() == Some(dedup_owner.id.as_str())
            && event.fields.get("debounceId")
                == Some(&serde_json::json!("tenant:dynamic-flow-child"))
    }));
    assert!(dedup_events.iter().any(|event| {
        event.event == "deduplicated"
            && event.job_id.as_deref() == Some(dedup_owner.id.as_str())
            && event.fields.get("deduplicatedJobId")
                == Some(&serde_json::json!("flow-dynamic-dedup:candidate"))
    }));

    let keep_last_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-dynamic-keep-last")
            .expect("valid Redis URL should build the dynamic keep-last queue");
    let keep_last_deduplication =
        DeduplicationOptions::new("tenant:dynamic-flow-child-keep-last").keep_last_if_active(true);
    let keep_last_owner = keep_last_queue
        .add_job(
            "existing-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_job_id("flow-dynamic-keep-last:owner")
                .with_priority(0)
                .with_deduplication(keep_last_deduplication.clone()),
        )
        .await
        .expect("dynamic keep-last owner should add");
    let keep_last_owner_claim = keep_last_queue
        .claim_next(
            "worker-dynamic-keep-last-owner".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic keep-last owner claim should return")
        .expect("dynamic keep-last owner should be claimable");
    assert_eq!(keep_last_owner_claim.id, keep_last_owner.id);
    let keep_last_parent = keep_last_queue
        .add_job(
            "planner".to_string(),
            serde_json::json!({ "kind": "plan" }),
            JobOptions::new()
                .with_job_id("flow-dynamic-keep-last:parent")
                .with_priority(1),
        )
        .await
        .expect("dynamic keep-last parent should add");
    let active_keep_last_parent = keep_last_queue
        .claim_next(
            "worker-dynamic-keep-last-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic keep-last parent claim should return")
        .expect("dynamic keep-last parent should be claimable");
    assert_eq!(active_keep_last_parent.id, keep_last_parent.id);
    let keep_last_children = keep_last_queue
        .add_flow_children_at(
            &active_keep_last_parent.id,
            lock_token(&active_keep_last_parent),
            vec![
                JobSpec::new("next-child", serde_json::json!({ "version": 2 })).with_options(
                    JobOptions::new()
                        .with_job_id("flow-dynamic-keep-last:next")
                        .with_priority(0)
                        .with_deduplication(keep_last_deduplication),
                ),
            ],
            Utc::now(),
        )
        .await
        .expect("dynamic keep-last child should be stored as next");
    assert!(keep_last_children.is_empty());
    assert!(keep_last_queue
        .get_job("flow-dynamic-keep-last:next")
        .await
        .expect("dynamic keep-last next lookup should return")
        .is_none());
    let keep_last_parent_after = keep_last_queue
        .get_job(&keep_last_parent.id)
        .await
        .expect("dynamic keep-last parent should load")
        .expect("dynamic keep-last parent should exist");
    assert_eq!(keep_last_parent_after.state, JobState::WaitingChildren);
    assert_eq!(
        keep_last_parent_after.child_ids,
        vec!["flow-dynamic-keep-last:next".to_string()]
    );
    let keep_last_dependency_key = format!(
        "{namespace}:flow-dynamic-keep-last:dependencies:{}",
        keep_last_parent.id
    );
    let keep_last_placeholder_pending: bool = conn
        .sismember(&keep_last_dependency_key, "flow-dynamic-keep-last:next")
        .await?;
    assert!(keep_last_placeholder_pending);
    let keep_last_next_key = format!(
        "{namespace}:flow-dynamic-keep-last:deduplication_next:tenant:dynamic-flow-child-keep-last"
    );
    let keep_last_next_raw: Option<String> = conn.get(&keep_last_next_key).await?;
    assert!(keep_last_next_raw.is_some());
    let keep_last_counts = keep_last_queue
        .get_flow_dependency_counts(&keep_last_parent.id)
        .await
        .expect("dynamic keep-last counts should load")
        .expect("dynamic keep-last parent should exist");
    assert_eq!(keep_last_counts.missing, 1);
    assert_eq!(keep_last_counts.unprocessed, 0);

    keep_last_queue
        .complete_job(
            &keep_last_owner_claim.id,
            lock_token(&keep_last_owner_claim),
            serde_json::json!({ "owner": "done" }),
            Utc::now(),
        )
        .await
        .expect("dynamic keep-last owner should complete");
    let keep_last_next_after: Option<String> = conn.get(&keep_last_next_key).await?;
    assert!(keep_last_next_after.is_none());
    let materialized_keep_last = keep_last_queue
        .get_job("flow-dynamic-keep-last:next")
        .await
        .expect("dynamic keep-last next should load")
        .expect("dynamic keep-last next should exist");
    assert_eq!(
        materialized_keep_last.parent_id.as_deref(),
        Some(keep_last_parent.id.as_str())
    );
    assert_eq!(materialized_keep_last.state, JobState::Waiting);
    let keep_last_counts_after = keep_last_queue
        .get_flow_dependency_counts(&keep_last_parent.id)
        .await
        .expect("dynamic keep-last counts after materialize should load")
        .expect("dynamic keep-last parent should exist");
    assert_eq!(keep_last_counts_after.missing, 0);
    assert_eq!(keep_last_counts_after.unprocessed, 1);
    let keep_last_next_claim = keep_last_queue
        .claim_next(
            "worker-dynamic-keep-last-next".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic keep-last next claim should return")
        .expect("dynamic keep-last next should be claimable");
    assert_eq!(keep_last_next_claim.id, "flow-dynamic-keep-last:next");
    keep_last_queue
        .complete_job(
            &keep_last_next_claim.id,
            lock_token(&keep_last_next_claim),
            serde_json::json!({ "next": "done" }),
            Utc::now(),
        )
        .await
        .expect("dynamic keep-last next should complete");
    assert_eq!(
        keep_last_queue
            .get_job(&keep_last_parent.id)
            .await
            .expect("dynamic keep-last parent after next should load")
            .expect("dynamic keep-last parent should exist")
            .state,
        JobState::Waiting
    );

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_continues_flow_dependency_failure_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_continued_flow_dependency_failure(redis_url),
    )
    .await
    .expect("Redis continued flow dependency failure integration test timed out")
    .unwrap();
}

async fn run_continued_flow_dependency_failure(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-continue")
        .expect("valid Redis URL should build the flow-continue queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-continue")
        .expect("valid Redis URL should build the flow-continue worker");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let flow = queue
        .add_flow_at(
            JobSpec::new(
                "continued-failure-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new(
                    "continued-failure-optional-child",
                    serde_json::json!({ "optional": true }),
                )
                .with_options(
                    JobOptions::new()
                        .with_priority(1)
                        .with_continue_parent_on_failure(true),
                ),
                JobSpec::new(
                    "continued-failure-required-child",
                    serde_json::json!({ "required": true }),
                )
                .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("continued flow should be added");
    let dependency_key = format!("{namespace}:flow-continue:dependencies:{}", flow.parent.id);

    let optional_child = worker
        .claim_next(
            "worker-continued-optional".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("optional child claim should return")
        .expect("optional child should be claimable");
    assert_eq!(optional_child.id, flow.children[0].id);
    worker
        .fail_job(
            &optional_child.id,
            lock_token(&optional_child),
            "optional child failed".to_string(),
            Utc::now(),
        )
        .await
        .expect("optional child should fail");

    let pending_count: usize = conn.scard(&dependency_key).await?;
    assert_eq!(pending_count, 1);
    let optional_pending: bool = conn
        .sismember(&dependency_key, &flow.children[0].id)
        .await?;
    let required_pending: bool = conn
        .sismember(&dependency_key, &flow.children[1].id)
        .await?;
    assert!(!optional_pending);
    assert!(required_pending);
    let parent_after_failure = queue
        .get_job(&flow.parent.id)
        .await
        .expect("parent after continued failure should load")
        .expect("parent should exist");
    assert_eq!(parent_after_failure.state, JobState::Waiting);
    assert!(parent_after_failure.failed_reason.is_none());
    let counts_after_failure = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("continued failure counts should load")
        .expect("continued failure counts should exist");
    assert_eq!(counts_after_failure.processed, 0);
    assert_eq!(counts_after_failure.unprocessed, 1);
    assert_eq!(counts_after_failure.failed, 0);
    assert_eq!(counts_after_failure.ignored, 1);
    assert_eq!(counts_after_failure.missing, 0);
    let ignored_failures = queue
        .get_flow_ignored_children_failures(&flow.parent.id)
        .await
        .expect("continued failure map should load")
        .expect("continued failure map should exist");
    assert_eq!(
        ignored_failures
            .get(&flow.children[0].id)
            .map(String::as_str),
        Some("optional child failed")
    );

    let continued_parent = worker
        .claim_next(
            "worker-continued-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("continued parent claim should return")
        .expect("continued parent should be claimable");
    assert_eq!(continued_parent.id, flow.parent.id);
    let required_still_pending: bool = conn
        .sismember(&dependency_key, &flow.children[1].id)
        .await?;
    assert!(required_still_pending);

    let complete_error = worker
        .complete_job(
            &continued_parent.id,
            lock_token(&continued_parent),
            serde_json::json!({ "early": true }),
            Utc::now(),
        )
        .await
        .expect_err("continued parent should not complete with pending dependencies");
    assert!(matches!(complete_error, LaneError::JobStateConflict(_)));

    let required_child = worker
        .claim_next(
            "worker-continued-required".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("required child claim should return")
        .expect("required child should still be claimable");
    assert_eq!(required_child.id, flow.children[1].id);
    worker
        .complete_job(
            &required_child.id,
            lock_token(&required_child),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("required child should complete");

    let pending_after_required: usize = conn.scard(&dependency_key).await?;
    assert_eq!(pending_after_required, 0);

    let completed_parent = worker
        .complete_job(
            &continued_parent.id,
            lock_token(&continued_parent),
            serde_json::json!({ "done": true }),
            Utc::now(),
        )
        .await
        .expect("continued parent should complete after dependencies resolve");
    assert_eq!(completed_parent.state, JobState::Completed);

    cleanup_namespace(&redis_url, &namespace).await
}

#[tokio::test]
async fn redis_backend_removes_flow_dependency_failure_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_removed_flow_dependency_failure(redis_url),
    )
    .await
    .expect("Redis removed flow dependency failure integration test timed out")
    .unwrap();
}

#[tokio::test]
async fn redis_backend_removes_stale_terminal_child_dependency_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    let _guard = redis_test_guard().await;
    tokio::time::timeout(
        Duration::from_secs(120),
        run_stale_terminal_child_dependency_removal(redis_url),
    )
    .await
    .expect("Redis stale terminal child dependency removal test timed out")
    .unwrap();
}

async fn run_removed_flow_dependency_failure(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-remove")
        .expect("valid Redis URL should build the flow-remove queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-remove")
        .expect("valid Redis URL should build the flow-remove worker");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let flow = queue
        .add_flow_at(
            JobSpec::new(
                "removed-failure-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new(
                    "removed-failure-optional-child",
                    serde_json::json!({ "optional": true }),
                )
                .with_options(
                    JobOptions::new()
                        .with_priority(1)
                        .with_remove_dependency_on_failure(true),
                ),
                JobSpec::new(
                    "removed-failure-required-child",
                    serde_json::json!({ "required": true }),
                )
                .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("removed flow should be added");
    let dependency_key = format!("{namespace}:flow-remove:dependencies:{}", flow.parent.id);

    let optional_child = worker
        .claim_next(
            "worker-removed-optional".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("optional child claim should return")
        .expect("optional child should be claimable");
    assert_eq!(optional_child.id, flow.children[0].id);
    worker
        .fail_job(
            &optional_child.id,
            lock_token(&optional_child),
            "optional child failed".to_string(),
            Utc::now(),
        )
        .await
        .expect("optional child should fail");

    let pending_count: usize = conn.scard(&dependency_key).await?;
    assert_eq!(pending_count, 1);
    let optional_pending: bool = conn
        .sismember(&dependency_key, &flow.children[0].id)
        .await?;
    let required_pending: bool = conn
        .sismember(&dependency_key, &flow.children[1].id)
        .await?;
    assert!(!optional_pending);
    assert!(required_pending);
    let parent_after_failure = queue
        .get_job(&flow.parent.id)
        .await
        .expect("parent after removed failure should load")
        .expect("parent should exist");
    assert_eq!(parent_after_failure.state, JobState::WaitingChildren);
    assert!(parent_after_failure.failed_reason.is_none());
    let counts_after_failure = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("removed failure counts should load")
        .expect("removed failure counts should exist");
    assert_eq!(counts_after_failure.processed, 0);
    assert_eq!(counts_after_failure.unprocessed, 1);
    assert_eq!(counts_after_failure.failed, 0);
    assert_eq!(counts_after_failure.ignored, 0);
    assert_eq!(counts_after_failure.missing, 0);
    let ignored_failures = queue
        .get_flow_ignored_children_failures(&flow.parent.id)
        .await
        .expect("removed failure ignored map should load")
        .expect("removed failure ignored map should exist");
    assert!(ignored_failures.is_empty());

    let required_child = worker
        .claim_next(
            "worker-removed-required".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("required child claim should return")
        .expect("required child should be claimable");
    assert_eq!(required_child.id, flow.children[1].id);
    worker
        .complete_job(
            &required_child.id,
            lock_token(&required_child),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("required child should complete");

    let dependency_key_exists: bool = conn.exists(&dependency_key).await?;
    assert!(!dependency_key_exists);
    let parent_after_release = queue
        .get_job(&flow.parent.id)
        .await
        .expect("parent after release should load")
        .expect("parent should exist");
    assert_eq!(parent_after_release.state, JobState::Waiting);
    let parent_failed_score: Option<f64> = conn
        .zscore(format!("{namespace}:flow-remove:failed"), &flow.parent.id)
        .await?;
    assert!(parent_failed_score.is_none());
    let parent_waiting_score: Option<f64> = conn
        .zscore(format!("{namespace}:flow-remove:waiting"), &flow.parent.id)
        .await?;
    assert!(parent_waiting_score.is_some());
    let counts_after_release = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("removed release counts should load")
        .expect("removed release counts should exist");
    assert_eq!(counts_after_release.processed, 1);
    assert_eq!(counts_after_release.unprocessed, 0);
    assert_eq!(counts_after_release.failed, 0);
    assert_eq!(counts_after_release.ignored, 0);
    assert_eq!(counts_after_release.missing, 0);
    let child_values = queue
        .get_flow_children_values(&flow.parent.id)
        .await
        .expect("removed flow child values should load")
        .expect("removed flow child values should exist");
    assert_eq!(child_values.len(), 1);
    assert_eq!(
        child_values.get(&flow.children[1].id),
        Some(&serde_json::json!({ "ok": true }))
    );

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_stale_terminal_child_dependency_removal(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-stale-dependency")
        .expect("valid Redis URL should build the stale dependency queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-stale-dependency")
        .expect("valid Redis URL should build the stale dependency worker");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let flow = queue
        .add_flow_at(
            JobSpec::new(
                "stale-dependency-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new(
                    "stale-dependency-child-a",
                    serde_json::json!({ "step": "a" }),
                )
                .with_options(JobOptions::new().with_priority(1)),
                JobSpec::new(
                    "stale-dependency-child-b",
                    serde_json::json!({ "step": "b" }),
                )
                .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("stale dependency flow should add");
    let dependency_key = format!(
        "{namespace}:flow-stale-dependency:dependencies:{}",
        flow.parent.id
    );
    let processed_key = format!("{dependency_key}:processed");

    let child_a = worker
        .claim_next(
            "worker-stale-dependency-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("stale dependency child A claim should return")
        .expect("stale dependency child A should be claimable");
    assert_eq!(child_a.id, flow.children[0].id);
    worker
        .complete_job(
            &child_a.id,
            lock_token(&child_a),
            serde_json::json!({ "ok": "a" }),
            Utc::now(),
        )
        .await
        .expect("stale dependency child A should complete");

    let child_a_pending: bool = conn.sismember(&dependency_key, &child_a.id).await?;
    assert!(!child_a_pending);
    let child_a_processed_before_remove: Option<String> =
        conn.hget(&processed_key, &child_a.id).await?;
    assert!(child_a_processed_before_remove.is_some());
    let child_b_pending: bool = conn
        .sismember(&dependency_key, &flow.children[1].id)
        .await?;
    assert!(child_b_pending);
    let stale_inserted: usize = conn.sadd(&dependency_key, &child_a.id).await?;
    assert_eq!(stale_inserted, 1);

    assert!(queue
        .remove_child_dependency(&child_a.id, Utc::now())
        .await
        .expect("stale terminal child dependency should remove"));
    assert!(!queue
        .remove_child_dependency(&child_a.id, Utc::now())
        .await
        .expect("stale terminal child dependency should not remove twice"));

    let child_a_after = queue
        .get_job(&child_a.id)
        .await
        .expect("stale dependency child A lookup should load")
        .expect("stale dependency child A should remain stored");
    assert_eq!(child_a_after.state, JobState::Completed);
    assert!(child_a_after.parent_id.is_none());
    let child_a_pending_after: bool = conn.sismember(&dependency_key, &child_a.id).await?;
    assert!(!child_a_pending_after);
    let child_a_processed_after_remove: Option<String> =
        conn.hget(&processed_key, &child_a.id).await?;
    assert!(child_a_processed_after_remove.is_none());
    let dependency_values_after_remove = queue
        .get_flow_dependency_values(&flow.parent.id)
        .await
        .expect("stale dependency values after remove should load")
        .expect("stale dependency values after remove should exist");
    assert!(!dependency_values_after_remove
        .processed
        .contains_key(&child_a.id));

    let parent_after_remove = queue
        .get_job(&flow.parent.id)
        .await
        .expect("stale dependency parent lookup should load")
        .expect("stale dependency parent should remain stored");
    assert_eq!(parent_after_remove.state, JobState::WaitingChildren);
    assert_eq!(
        parent_after_remove.child_ids,
        vec![flow.children[1].id.clone()]
    );
    let counts_after_remove = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("stale dependency counts should load")
        .expect("stale dependency counts should exist");
    assert_eq!(counts_after_remove.processed, 0);
    assert_eq!(counts_after_remove.unprocessed, 1);
    assert_eq!(counts_after_remove.failed, 0);
    assert_eq!(counts_after_remove.missing, 0);

    let child_b = worker
        .claim_next(
            "worker-stale-dependency-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("stale dependency child B claim should return")
        .expect("stale dependency child B should be claimable");
    assert_eq!(child_b.id, flow.children[1].id);
    worker
        .complete_job(
            &child_b.id,
            lock_token(&child_b),
            serde_json::json!({ "ok": "b" }),
            Utc::now(),
        )
        .await
        .expect("stale dependency child B should complete");

    let parent_after_release = queue
        .get_job(&flow.parent.id)
        .await
        .expect("stale dependency released parent lookup should load")
        .expect("stale dependency released parent should remain stored");
    assert_eq!(parent_after_release.state, JobState::Waiting);

    cleanup_namespace(&redis_url, &namespace).await?;

    let side_bucket_namespace = unique_namespace();
    cleanup_namespace(&redis_url, &side_bucket_namespace).await?;

    let side_bucket_queue = RedisJobQueue::with_namespace(
        &redis_url,
        &side_bucket_namespace,
        "flow-side-bucket-dependency",
    )
    .expect("valid Redis URL should build the side-bucket dependency queue");
    let side_bucket_worker = RedisJobQueue::with_namespace(
        &redis_url,
        &side_bucket_namespace,
        "flow-side-bucket-dependency",
    )
    .expect("valid Redis URL should build the side-bucket dependency worker");

    let side_bucket_flow = side_bucket_queue
        .add_flow_at(
            JobSpec::new(
                "side-bucket-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("side-bucket-child-a", serde_json::json!({ "step": "a" }))
                    .with_options(JobOptions::new().with_priority(1)),
                JobSpec::new("side-bucket-child-b", serde_json::json!({ "step": "b" }))
                    .with_options(JobOptions::new().with_priority(2)),
            ],
            Utc::now(),
        )
        .await
        .expect("side-bucket dependency flow should add");
    let side_dependency_key = format!(
        "{side_bucket_namespace}:flow-side-bucket-dependency:dependencies:{}",
        side_bucket_flow.parent.id
    );
    let side_processed_key = format!("{side_dependency_key}:processed");

    let side_child_a = side_bucket_worker
        .claim_next(
            "worker-side-bucket-dependency-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("side-bucket child A claim should return")
        .expect("side-bucket child A should be claimable");
    assert_eq!(side_child_a.id, side_bucket_flow.children[0].id);
    side_bucket_worker
        .complete_job(
            &side_child_a.id,
            lock_token(&side_child_a),
            serde_json::json!({ "ok": "a" }),
            Utc::now(),
        )
        .await
        .expect("side-bucket child A should complete");

    let side_child_a_pending: bool = conn
        .sismember(&side_dependency_key, &side_child_a.id)
        .await?;
    assert!(!side_child_a_pending);
    let side_child_a_processed_before_remove: Option<String> =
        conn.hget(&side_processed_key, &side_child_a.id).await?;
    assert!(side_child_a_processed_before_remove.is_some());

    assert!(side_bucket_queue
        .remove_child_dependency(&side_child_a.id, Utc::now())
        .await
        .expect("terminal side-bucket-only child dependency should remove"));
    assert!(!side_bucket_queue
        .remove_child_dependency(&side_child_a.id, Utc::now())
        .await
        .expect("terminal side-bucket-only child dependency should not remove twice"));

    let side_child_a_after = side_bucket_queue
        .get_job(&side_child_a.id)
        .await
        .expect("side-bucket child A lookup should load")
        .expect("side-bucket child A should remain stored");
    assert_eq!(side_child_a_after.state, JobState::Completed);
    assert!(side_child_a_after.parent_id.is_none());
    let side_child_a_processed_after_remove: Option<String> =
        conn.hget(&side_processed_key, &side_child_a.id).await?;
    assert!(side_child_a_processed_after_remove.is_none());
    let side_dependency_values_after_remove = side_bucket_queue
        .get_flow_dependency_values(&side_bucket_flow.parent.id)
        .await
        .expect("side-bucket dependency values after remove should load")
        .expect("side-bucket dependency values after remove should exist");
    assert!(!side_dependency_values_after_remove
        .processed
        .contains_key(&side_child_a.id));

    let side_parent_after_remove = side_bucket_queue
        .get_job(&side_bucket_flow.parent.id)
        .await
        .expect("side-bucket parent lookup should load")
        .expect("side-bucket parent should remain stored");
    assert_eq!(side_parent_after_remove.state, JobState::WaitingChildren);
    assert_eq!(
        side_parent_after_remove.child_ids,
        vec![side_bucket_flow.children[1].id.clone()]
    );
    let side_counts_after_remove = side_bucket_queue
        .get_flow_dependency_counts(&side_bucket_flow.parent.id)
        .await
        .expect("side-bucket dependency counts should load")
        .expect("side-bucket dependency counts should exist");
    assert_eq!(side_counts_after_remove.processed, 0);
    assert_eq!(side_counts_after_remove.unprocessed, 1);
    assert_eq!(side_counts_after_remove.failed, 0);
    assert_eq!(side_counts_after_remove.missing, 0);

    cleanup_namespace(&redis_url, &side_bucket_namespace).await
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
    let removed_job = queue
        .add_job(
            "removed-task".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_job_id("events:removed-task"),
        )
        .await
        .expect("removed event test job should add");
    let removed = queue
        .remove_job(&removed_job.id)
        .await
        .expect("removed event test job should remove")
        .expect("removed event test job should exist");
    assert_eq!(removed.id, removed_job.id);
    let cleaned_job = queue
        .add_job(
            "cleaned-task".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_job_id("events:cleaned-task"),
        )
        .await
        .expect("cleaned event test job should add");
    let cleaned = queue
        .clean_jobs(JobState::Waiting, Duration::ZERO, 10, Utc::now())
        .await
        .expect("cleaned event test job should clean");
    assert_eq!(cleaned.len(), 1);
    assert_eq!(cleaned[0].id, cleaned_job.id);
    let dedup_owner = queue
        .add_job(
            "dedup-owner".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("events:dedup-owner")
                .with_deduplication_id("events:dedup"),
        )
        .await
        .expect("deduplicated event owner should add");
    let dedup_duplicate = queue
        .add_job(
            "dedup-duplicate".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("events:dedup-duplicate")
                .with_deduplication_id("events:dedup"),
        )
        .await
        .expect("deduplicated event duplicate should return owner");
    assert_eq!(dedup_duplicate.id, dedup_owner.id);
    let replaced_dedup_owner = queue
        .add_job(
            "dedup-replace-old".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("events:dedup-replace-old")
                .with_delay(Duration::from_secs(60))
                .with_deduplication(
                    DeduplicationOptions::new("events:dedup-replace").replace_delayed(true),
                ),
        )
        .await
        .expect("deduplicated delayed replacement owner should add");
    let dedup_replacement = queue
        .add_job(
            "dedup-replace-new".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("events:dedup-replace-new")
                .with_delay(Duration::from_secs(120))
                .with_deduplication(
                    DeduplicationOptions::new("events:dedup-replace").replace_delayed(true),
                ),
        )
        .await
        .expect("deduplicated delayed replacement should add");
    assert_ne!(dedup_replacement.id, replaced_dedup_owner.id);
    assert!(queue
        .get_job(&replaced_dedup_owner.id)
        .await
        .expect("replaced dedup owner lookup should return")
        .is_none());
    queue.pause().await.expect("queue should pause");
    queue.resume().await.expect("queue should resume");

    let events = queue
        .read_events("-", "+", 40)
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
            "drained",
            "added",
            "waiting",
            "removed",
            "added",
            "waiting",
            "cleaned",
            "added",
            "waiting",
            "debounced",
            "deduplicated",
            "added",
            "delayed",
            "removed",
            "debounced",
            "deduplicated",
            "added",
            "delayed",
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
    assert_eq!(events[5].job_id, None);
    assert_eq!(events[5].prev, None);
    assert_eq!(events[8].job_id.as_deref(), Some(removed_job.id.as_str()));
    assert_eq!(events[8].prev, Some(JobState::Waiting));
    assert_eq!(events[11].job_id, None);
    assert_eq!(events[11].prev, None);
    assert_eq!(events[11].fields.get("count"), Some(&serde_json::json!(1)));
    assert_eq!(events[14].job_id.as_deref(), Some(dedup_owner.id.as_str()));
    assert_eq!(
        events[14].fields.get("debounceId"),
        Some(&serde_json::json!("events:dedup"))
    );
    assert_eq!(events[15].job_id.as_deref(), Some(dedup_owner.id.as_str()));
    assert_eq!(
        events[15].fields.get("deduplicationId"),
        Some(&serde_json::json!("events:dedup"))
    );
    assert_eq!(
        events[15].fields.get("deduplicatedJobId"),
        Some(&serde_json::json!("events:dedup-duplicate"))
    );
    assert_eq!(
        events[18].job_id.as_deref(),
        Some(replaced_dedup_owner.id.as_str())
    );
    assert_eq!(events[18].prev, Some(JobState::Delayed));
    assert_eq!(
        events[19].job_id.as_deref(),
        Some(dedup_replacement.id.as_str())
    );
    assert_eq!(
        events[19].fields.get("debounceId"),
        Some(&serde_json::json!("events:dedup-replace"))
    );
    assert_eq!(
        events[20].job_id.as_deref(),
        Some(dedup_replacement.id.as_str())
    );
    assert_eq!(
        events[20].fields.get("deduplicationId"),
        Some(&serde_json::json!("events:dedup-replace"))
    );
    assert_eq!(
        events[20].fields.get("deduplicatedJobId"),
        Some(&serde_json::json!("events:dedup-replace-old"))
    );

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_queue_event_trimming(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("trim-events:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("trim-events:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "trim-events")
        .expect("valid Redis URL should build the trim-events queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let events_key = format!("{namespace}:trim-events:events");
    trace_stage("trim-events:conn-created");

    let seed_script = r#"
local count = tonumber(ARGV[1])
local first_id = nil
local last_id = nil
for index = 0, count - 1 do
  local job_id = 'trim-events:' .. tostring(index)
  local added_id = redis.call('XADD', KEYS[1], '*', 'event', 'added', 'jobId', job_id, 'name', 'trim-event-' .. tostring(index))
  if not first_id then
    first_id = added_id
  end
  last_id = redis.call('XADD', KEYS[1], '*', 'event', 'waiting', 'jobId', job_id)
end
return {first_id, last_id, tostring(redis.call('XLEN', KEYS[1]))}
"#;
    trace_stage("trim-events:seed-built");
    let seed_result: Vec<String> = redis::cmd("EVAL")
        .arg(seed_script)
        .arg(1)
        .arg(&events_key)
        .arg(120)
        .query_async(&mut conn)
        .await?;
    let first_id = seed_result
        .first()
        .expect("seed script should return the first event id")
        .clone();
    let last_id = seed_result
        .get(1)
        .expect("seed script should return the last event id")
        .clone();
    let seeded_len = seed_result
        .get(2)
        .and_then(|value| value.parse::<usize>().ok())
        .expect("seed script should return the stream length");
    assert_eq!(seeded_len, 240);
    trace_stage("trim-events:seeded");

    let first_before = queue
        .read_events(&first_id, &first_id, 1)
        .await
        .expect("first trim event should read before trimming");
    assert_eq!(first_before.len(), 1);
    assert_eq!(
        first_before.first().map(|event| event.event.as_str()),
        Some("added")
    );
    assert_eq!(
        first_before
            .first()
            .and_then(|event| event.job_id.as_deref()),
        Some("trim-events:0")
    );

    let last_before = queue
        .read_events(&last_id, &last_id, 1)
        .await
        .expect("last trim event should read before trimming");
    assert_eq!(last_before.len(), 1);
    assert_eq!(
        last_before.first().map(|event| event.event.as_str()),
        Some("waiting")
    );
    assert_eq!(
        last_before
            .first()
            .and_then(|event| event.job_id.as_deref()),
        Some("trim-events:119")
    );
    trace_stage("trim-events:before-read");

    let trimmed = queue
        .trim_events(100)
        .await
        .expect("trim event stream should trim");
    assert!(trimmed > 0);
    trace_stage("trim-events:trimmed");

    let after_len: usize = redis::cmd("XLEN")
        .arg(&events_key)
        .query_async(&mut conn)
        .await
        .expect("trim event stream length should read after trimming");
    trace_stage("trim-events:after-read");
    assert_eq!(after_len + trimmed, seeded_len);
    assert!(after_len < seeded_len);

    let removed_first = queue
        .read_events(&first_id, &first_id, 1)
        .await
        .expect("first trim event lookup should return after trimming");
    assert!(removed_first.is_empty());

    let retained_last = queue
        .read_events(&last_id, &last_id, 1)
        .await
        .expect("last trim event should remain after trimming");
    assert_eq!(retained_last.len(), 1);
    assert_eq!(
        retained_last.first().map(|event| event.event.as_str()),
        Some("waiting")
    );
    assert_eq!(
        retained_last
            .first()
            .and_then(|event| event.job_id.as_deref()),
        Some("trim-events:119")
    );

    let limited = queue
        .read_events("-", "+", 5)
        .await
        .expect("trimmed event stream should respect read limits");
    assert_eq!(limited.len(), 5);
    assert_ne!(limited[0].id, first_id);
    assert!(limited.windows(2).all(|pair| pair[0].id <= pair[1].id));

    let cleared = queue
        .trim_events(0)
        .await
        .expect("trim event stream should clear with zero max length");
    assert_eq!(cleared, after_len);
    let cleared_len: usize = redis::cmd("XLEN")
        .arg(&events_key)
        .query_async(&mut conn)
        .await
        .expect("cleared event stream length should read");
    assert_eq!(cleared_len, 0);
    let cleared_events = queue
        .read_events("-", "+", 1)
        .await
        .expect("cleared event stream should read as empty");
    assert!(cleared_events.is_empty());

    cleanup_namespace_with_conn(&mut conn, &namespace).await
}

async fn run_bulk_dedup_events(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "bulk-dedup-events")
        .expect("valid Redis URL should build the bulk dedup events queue");
    let dedup_jobs = queue
        .add_jobs(
            vec![
                JobSpec::new("bulk-dedup-owner", serde_json::json!({ "version": 1 })).with_options(
                    JobOptions::new()
                        .with_job_id("bulk-events:dedup-owner")
                        .with_deduplication_id("bulk-events:dedup"),
                ),
                JobSpec::new("bulk-dedup-duplicate", serde_json::json!({ "version": 2 }))
                    .with_options(
                        JobOptions::new()
                            .with_job_id("bulk-events:dedup-duplicate")
                            .with_deduplication_id("bulk-events:dedup"),
                    ),
            ],
            Utc::now(),
        )
        .await
        .expect("bulk dedup duplicate jobs should add");
    assert_eq!(dedup_jobs.len(), 2);
    assert_eq!(dedup_jobs[1].id, dedup_jobs[0].id);

    let replace_jobs = queue
        .add_jobs(
            vec![
                JobSpec::new(
                    "bulk-dedup-replace-old",
                    serde_json::json!({ "version": 1 }),
                )
                .with_options(
                    JobOptions::new()
                        .with_job_id("bulk-events:replace-old")
                        .with_delay(Duration::from_secs(30))
                        .with_deduplication(
                            DeduplicationOptions::new("bulk-events:replace").replace_delayed(true),
                        ),
                ),
                JobSpec::new(
                    "bulk-dedup-replace-new",
                    serde_json::json!({ "version": 2 }),
                )
                .with_options(
                    JobOptions::new()
                        .with_job_id("bulk-events:replace-new")
                        .with_delay(Duration::from_secs(60))
                        .with_deduplication(
                            DeduplicationOptions::new("bulk-events:replace").replace_delayed(true),
                        ),
                ),
            ],
            Utc::now(),
        )
        .await
        .expect("bulk delayed replacement jobs should add");
    assert_eq!(replace_jobs.len(), 2);
    assert_eq!(replace_jobs[0].id, "bulk-events:replace-old");
    assert_eq!(replace_jobs[1].id, "bulk-events:replace-new");
    assert!(queue
        .get_job(&replace_jobs[0].id)
        .await
        .expect("replaced bulk owner lookup should return")
        .is_none());

    let events = queue
        .read_events("-", "+", 20)
        .await
        .expect("bulk dedup events should read");
    let names = events
        .iter()
        .map(|event| event.event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "added",
            "waiting",
            "debounced",
            "deduplicated",
            "added",
            "delayed",
            "removed",
            "debounced",
            "deduplicated",
            "added",
            "delayed"
        ]
    );
    assert_eq!(events[2].job_id.as_deref(), Some(dedup_jobs[0].id.as_str()));
    assert_eq!(
        events[2].fields.get("debounceId"),
        Some(&serde_json::json!("bulk-events:dedup"))
    );
    assert_eq!(events[3].job_id.as_deref(), Some(dedup_jobs[0].id.as_str()));
    assert_eq!(
        events[3].fields.get("deduplicationId"),
        Some(&serde_json::json!("bulk-events:dedup"))
    );
    assert_eq!(
        events[3].fields.get("deduplicatedJobId"),
        Some(&serde_json::json!("bulk-events:dedup-duplicate"))
    );
    assert_eq!(events[6].job_id.as_deref(), Some("bulk-events:replace-old"));
    assert_eq!(events[6].prev, Some(JobState::Delayed));
    assert_eq!(events[7].job_id.as_deref(), Some("bulk-events:replace-new"));
    assert_eq!(
        events[7].fields.get("debounceId"),
        Some(&serde_json::json!("bulk-events:replace"))
    );
    assert_eq!(events[8].job_id.as_deref(), Some("bulk-events:replace-new"));
    assert_eq!(
        events[8].fields.get("deduplicationId"),
        Some(&serde_json::json!("bulk-events:replace"))
    );
    assert_eq!(
        events[8].fields.get("deduplicatedJobId"),
        Some(&serde_json::json!("bulk-events:replace-old"))
    );

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_flow_duplicate_job_ids(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-duplicate-ids")
        .expect("valid Redis URL should build the flow duplicate-id queue");
    let existing = queue
        .add_job(
            "existing-child".to_string(),
            serde_json::json!({ "original": true }),
            JobOptions::new().with_job_id("flow-duplicate:existing-child"),
        )
        .await
        .expect("existing child should add");
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({}))
                .with_options(JobOptions::new().with_job_id("flow-duplicate:parent")),
            vec![
                JobSpec::new("candidate-child", serde_json::json!({ "candidate": true }))
                    .with_options(JobOptions::new().with_job_id(existing.id.clone())),
            ],
            Utc::now(),
        )
        .await
        .expect("flow should reuse existing child id");
    assert_eq!(flow.parent.state, JobState::WaitingChildren);
    assert_eq!(flow.parent.child_ids, vec![existing.id.clone()]);
    assert_eq!(flow.children.len(), 1);
    assert_eq!(flow.children[0].id, existing.id);
    assert_eq!(flow.children[0].name, "existing-child");
    assert_eq!(
        flow.children[0].payload,
        serde_json::json!({ "original": true })
    );
    assert_eq!(
        flow.children[0].parent_id.as_deref(),
        Some(flow.parent.id.as_str())
    );
    let counts = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .expect("flow dependency counts should load")
        .expect("flow dependency counts should exist");
    assert_eq!(counts.processed, 0);
    assert_eq!(counts.unprocessed, 1);
    assert_eq!(counts.failed, 0);
    assert_eq!(counts.missing, 0);

    let duplicated_events = queue
        .read_events("-", "+", 20)
        .await
        .expect("duplicate-id events should read")
        .into_iter()
        .filter(|event| {
            event.event == "duplicated" && event.job_id.as_deref() == Some(existing.id.as_str())
        })
        .count();
    assert_eq!(duplicated_events, 1);

    let claimed = queue
        .claim_next(
            "worker-existing-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("existing child claim should return")
        .expect("existing child should be claimable");
    assert_eq!(claimed.id, existing.id);
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("existing child should complete");
    assert_eq!(
        queue
            .get_job(&flow.parent.id)
            .await
            .expect("parent should load")
            .expect("parent should exist")
            .state,
        JobState::Waiting
    );

    let completed_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-duplicate-completed")
            .expect("valid Redis URL should build the completed duplicate-id queue");
    let completed_existing = completed_queue
        .add_job(
            "completed-child".to_string(),
            serde_json::json!({ "original": true }),
            JobOptions::new().with_job_id("flow-duplicate:completed-child"),
        )
        .await
        .expect("completed child should add");
    let completed_claim = completed_queue
        .claim_next(
            "worker-completed-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("completed child claim should return")
        .expect("completed child should be claimable");
    completed_queue
        .complete_job(
            &completed_claim.id,
            lock_token(&completed_claim),
            serde_json::json!({ "done": true }),
            Utc::now(),
        )
        .await
        .expect("completed child should finish");
    let completed_flow = completed_queue
        .add_flow_at(
            JobSpec::new("completed-parent", serde_json::json!({}))
                .with_options(JobOptions::new().with_job_id("flow-duplicate:completed-parent")),
            vec![
                JobSpec::new("candidate-completed-child", serde_json::json!({}))
                    .with_options(JobOptions::new().with_job_id(completed_existing.id.clone())),
            ],
            Utc::now(),
        )
        .await
        .expect("flow should reuse completed child id");
    assert_eq!(completed_flow.parent.state, JobState::Waiting);
    assert_eq!(
        completed_flow.parent.child_ids,
        vec![completed_existing.id.clone()]
    );
    let completed_counts = completed_queue
        .get_flow_dependency_counts(&completed_flow.parent.id)
        .await
        .expect("completed duplicate counts should load")
        .expect("completed duplicate counts should exist");
    assert_eq!(completed_counts.processed, 1);
    assert_eq!(completed_counts.unprocessed, 0);
    assert_eq!(completed_counts.failed, 0);
    assert_eq!(completed_counts.missing, 0);
    let completed_values = completed_queue
        .get_flow_children_values(&completed_flow.parent.id)
        .await
        .expect("completed duplicate values should load")
        .expect("completed duplicate values should exist");
    assert_eq!(
        completed_values.get(&completed_existing.id),
        Some(&serde_json::json!({ "done": true }))
    );

    let parent_duplicate_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-duplicate-parent")
            .expect("valid Redis URL should build the duplicate-parent queue");
    let parent_duplicate_first = parent_duplicate_queue
        .add_flow_at(
            JobSpec::new("original-parent", serde_json::json!({ "version": 1 }))
                .with_options(JobOptions::new().with_job_id("flow-duplicate:parent-retry")),
            vec![
                JobSpec::new("original-child", serde_json::json!({ "child": 1 }))
                    .with_options(JobOptions::new().with_job_id("flow-duplicate:parent-child-a")),
            ],
            Utc::now(),
        )
        .await
        .expect("first duplicate-parent flow should add");
    let parent_duplicate_second = parent_duplicate_queue
        .add_flow_at(
            JobSpec::new("candidate-parent", serde_json::json!({ "version": 2 })).with_options(
                JobOptions::new().with_job_id(parent_duplicate_first.parent.id.clone()),
            ),
            vec![
                JobSpec::new("candidate-child", serde_json::json!({ "child": 2 }))
                    .with_options(JobOptions::new().with_job_id("flow-duplicate:parent-child-b")),
            ],
            Utc::now(),
        )
        .await
        .expect("duplicate parent flow should add new child");
    assert_eq!(
        parent_duplicate_second.parent.id,
        parent_duplicate_first.parent.id
    );
    assert_eq!(parent_duplicate_second.parent.name, "original-parent");
    assert_eq!(parent_duplicate_second.children.len(), 1);
    assert_eq!(
        parent_duplicate_second.children[0].id,
        "flow-duplicate:parent-child-b"
    );
    let parent_duplicate_parent = parent_duplicate_queue
        .get_job(&parent_duplicate_first.parent.id)
        .await
        .expect("duplicate parent should load")
        .expect("duplicate parent should exist");
    assert_eq!(
        parent_duplicate_parent.child_ids,
        vec![
            "flow-duplicate:parent-child-a".to_string(),
            "flow-duplicate:parent-child-b".to_string()
        ]
    );
    let parent_duplicate_counts = parent_duplicate_queue
        .get_flow_dependency_counts(&parent_duplicate_first.parent.id)
        .await
        .expect("duplicate parent counts should load")
        .expect("duplicate parent should exist");
    assert_eq!(parent_duplicate_counts.unprocessed, 2);
    assert_eq!(parent_duplicate_counts.processed, 0);
    let parent_duplicate_events = parent_duplicate_queue
        .read_events("-", "+", 20)
        .await
        .expect("duplicate parent events should read");
    assert!(parent_duplicate_events.iter().any(|event| {
        event.event == "duplicated"
            && event.job_id.as_deref() == Some(parent_duplicate_first.parent.id.as_str())
    }));

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_flow_dedup_events(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-dedup-events")
        .expect("valid Redis URL should build the flow dedup events queue");
    let owner = queue
        .add_flow_at(
            JobSpec::new("flow-owner-parent", serde_json::json!({ "version": 1 })).with_options(
                JobOptions::new()
                    .with_job_id("flow-events:owner-parent")
                    .with_deduplication_id("flow-events:dedup"),
            ),
            vec![
                JobSpec::new("flow-owner-child", serde_json::json!({ "version": 1 }))
                    .with_options(JobOptions::new().with_job_id("flow-events:owner-child")),
            ],
            Utc::now(),
        )
        .await
        .expect("flow dedup owner should add");
    let duplicate = queue
        .add_flow_at(
            JobSpec::new("flow-duplicate-parent", serde_json::json!({ "version": 2 }))
                .with_options(
                    JobOptions::new()
                        .with_job_id("flow-events:duplicate-parent")
                        .with_deduplication_id("flow-events:dedup"),
                ),
            vec![
                JobSpec::new("flow-duplicate-child", serde_json::json!({ "version": 2 }))
                    .with_options(JobOptions::new().with_job_id("flow-events:duplicate-child")),
            ],
            Utc::now(),
        )
        .await
        .expect("flow dedup duplicate should return owner");
    assert_eq!(duplicate.parent.id, owner.parent.id);
    assert_eq!(duplicate.children[0].id, owner.children[0].id);
    assert!(queue
        .get_job("flow-events:duplicate-parent")
        .await
        .expect("duplicate flow parent lookup should return")
        .is_none());

    let events = queue
        .read_events("-", "+", 10)
        .await
        .expect("flow dedup events should read");
    let names = events
        .iter()
        .map(|event| event.event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "added",
            "waiting-children",
            "added",
            "waiting",
            "debounced",
            "deduplicated"
        ]
    );
    assert_eq!(events[4].job_id.as_deref(), Some(owner.parent.id.as_str()));
    assert_eq!(
        events[4].fields.get("debounceId"),
        Some(&serde_json::json!("flow-events:dedup"))
    );
    assert_eq!(events[5].job_id.as_deref(), Some(owner.parent.id.as_str()));
    assert_eq!(
        events[5].fields.get("deduplicationId"),
        Some(&serde_json::json!("flow-events:dedup"))
    );
    assert_eq!(
        events[5].fields.get("deduplicatedJobId"),
        Some(&serde_json::json!("flow-events:duplicate-parent"))
    );

    let keep_last_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-keep-last-events")
            .expect("valid Redis URL should build the flow keep-last events queue");
    let keep_last_deduplication =
        DeduplicationOptions::new("flow-events:keep-last").keep_last_if_active(true);
    let keep_last_owner = keep_last_queue
        .add_flow_at(
            JobSpec::new(
                "flow-keep-last-owner-parent",
                serde_json::json!({ "version": 1 }),
            )
            .with_options(JobOptions::new().with_deduplication(keep_last_deduplication.clone())),
            vec![JobSpec::new(
                "flow-keep-last-owner-child",
                serde_json::json!({ "version": 1 }),
            )],
            Utc::now(),
        )
        .await
        .expect("flow keep-last owner should add");
    let keep_last_child = keep_last_queue
        .claim_next(
            "worker-flow-keep-last-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("flow keep-last child claim should return")
        .expect("flow keep-last child should be claimable");
    keep_last_queue
        .complete_job(
            &keep_last_child.id,
            lock_token(&keep_last_child),
            serde_json::json!({ "child": true }),
            Utc::now(),
        )
        .await
        .expect("flow keep-last child should complete");
    let keep_last_parent = keep_last_queue
        .claim_next(
            "worker-flow-keep-last-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("flow keep-last parent claim should return")
        .expect("flow keep-last parent should be claimable");
    assert_eq!(keep_last_parent.id, keep_last_owner.parent.id);
    let keep_last_duplicate = keep_last_queue
        .add_flow_at(
            JobSpec::new(
                "flow-keep-last-latest-parent",
                serde_json::json!({ "version": 2 }),
            )
            .with_options(
                JobOptions::new()
                    .with_job_id("flow-events:keep-last-latest-parent")
                    .with_deduplication(keep_last_deduplication),
            ),
            vec![JobSpec::new(
                "flow-keep-last-latest-child",
                serde_json::json!({ "version": 2 }),
            )
            .with_options(JobOptions::new().with_job_id("flow-events:keep-last-latest-child"))],
            Utc::now(),
        )
        .await
        .expect("flow keep-last duplicate should return owner");
    assert_eq!(keep_last_duplicate.parent.id, keep_last_owner.parent.id);

    let keep_last_events = keep_last_queue
        .read_events("-", "+", 20)
        .await
        .expect("flow keep-last events should read");
    let tail = &keep_last_events[keep_last_events.len() - 2..];
    assert_eq!(tail[0].event, "debounced");
    assert_eq!(
        tail[0].job_id.as_deref(),
        Some(keep_last_owner.parent.id.as_str())
    );
    assert_eq!(
        tail[0].fields.get("debounceId"),
        Some(&serde_json::json!("flow-events:keep-last"))
    );
    assert_eq!(tail[1].event, "deduplicated");
    assert_eq!(
        tail[1].job_id.as_deref(),
        Some(keep_last_owner.parent.id.as_str())
    );
    assert_eq!(
        tail[1].fields.get("deduplicatedJobId"),
        Some(&serde_json::json!("flow-events:keep-last-latest-parent"))
    );

    let child_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-child-dedup-events")
            .expect("valid Redis URL should build the flow child dedup events queue");
    let child_owner = child_queue
        .add_job(
            "existing-child-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_job_id("flow-events:existing-child")
                .with_deduplication_id("flow-events:child-dedup"),
        )
        .await
        .expect("flow child dedup owner should add");
    let child_flow = child_queue
        .add_flow_at(
            JobSpec::new("flow-child-parent", serde_json::json!({ "parent": true }))
                .with_options(JobOptions::new().with_job_id("flow-events:child-parent")),
            vec![
                JobSpec::new("flow-child-candidate", serde_json::json!({ "version": 2 }))
                    .with_options(
                        JobOptions::new()
                            .with_job_id("flow-events:candidate-child")
                            .with_deduplication_id("flow-events:child-dedup"),
                    ),
                JobSpec::new("flow-child-retained", serde_json::json!({ "version": 3 }))
                    .with_options(JobOptions::new().with_job_id("flow-events:retained-child")),
            ],
            Utc::now(),
        )
        .await
        .expect("flow with deduplicated child should add");
    assert_eq!(child_flow.children.len(), 1);
    assert_eq!(child_flow.children[0].id, "flow-events:retained-child");
    assert_eq!(
        child_flow.parent.child_ids,
        vec!["flow-events:retained-child".to_string()]
    );
    assert!(child_queue
        .get_job("flow-events:candidate-child")
        .await
        .expect("deduplicated child candidate lookup should return")
        .is_none());
    assert_eq!(
        child_queue
            .get_job(&child_owner.id)
            .await
            .expect("child dedup owner lookup should return")
            .expect("child dedup owner should exist")
            .parent_id,
        None
    );
    let child_counts = child_queue
        .get_flow_dependency_counts(&child_flow.parent.id)
        .await
        .expect("child dedup flow dependency counts should load")
        .expect("child dedup flow dependency counts should exist");
    assert_eq!(child_counts.unprocessed, 1);
    assert_eq!(child_counts.missing, 0);
    let child_events = child_queue
        .read_events("-", "+", 20)
        .await
        .expect("flow child dedup events should read");
    let child_event_names = child_events
        .iter()
        .map(|event| event.event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        child_event_names,
        vec![
            "added",
            "waiting",
            "added",
            "waiting-children",
            "debounced",
            "deduplicated",
            "added",
            "waiting"
        ]
    );
    assert_eq!(
        child_events[4].job_id.as_deref(),
        Some(child_owner.id.as_str())
    );
    assert_eq!(
        child_events[4].fields.get("debounceId"),
        Some(&serde_json::json!("flow-events:child-dedup"))
    );
    assert_eq!(
        child_events[5].job_id.as_deref(),
        Some(child_owner.id.as_str())
    );
    assert_eq!(
        child_events[5].fields.get("deduplicationId"),
        Some(&serde_json::json!("flow-events:child-dedup"))
    );
    assert_eq!(
        child_events[5].fields.get("deduplicatedJobId"),
        Some(&serde_json::json!("flow-events:candidate-child"))
    );

    let child_keep_last_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-child-keep-last")
            .expect("valid Redis URL should build the flow child keep-last queue");
    let child_keep_last_deduplication =
        DeduplicationOptions::new("flow-events:child-keep-last").keep_last_if_active(true);
    let child_keep_last_owner = child_keep_last_queue
        .add_flow_at(
            JobSpec::new(
                "flow-child-keep-last-owner-parent",
                serde_json::json!({ "version": 1 }),
            )
            .with_options(JobOptions::new().with_priority(1000)),
            vec![JobSpec::new(
                "flow-child-keep-last-owner",
                serde_json::json!({ "version": 1 }),
            )
            .with_options(
                JobOptions::new()
                    .with_job_id("flow-events:child-keep-last-owner")
                    .with_deduplication(child_keep_last_deduplication.clone()),
            )],
            Utc::now(),
        )
        .await
        .expect("flow child keep-last owner should add");
    let child_keep_last_owner_claim = child_keep_last_queue
        .claim_next(
            "worker-flow-child-keep-last-owner".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("flow child keep-last owner claim should return")
        .expect("flow child keep-last owner should be claimable");
    assert_eq!(
        child_keep_last_owner_claim.id,
        child_keep_last_owner.children[0].id
    );
    let child_keep_last_next = child_keep_last_queue
        .add_flow_at(
            JobSpec::new(
                "flow-child-keep-last-next-parent",
                serde_json::json!({ "version": 2 }),
            )
            .with_options(JobOptions::new().with_job_id("flow-events:child-keep-last-parent")),
            vec![JobSpec::new(
                "flow-child-keep-last-next",
                serde_json::json!({ "version": 2 }),
            )
            .with_options(
                JobOptions::new()
                    .with_job_id("flow-events:child-keep-last-next")
                    .with_priority(0)
                    .with_deduplication(child_keep_last_deduplication),
            )],
            Utc::now(),
        )
        .await
        .expect("flow child keep-last next flow should add");
    assert!(child_keep_last_next.children.is_empty());
    assert_eq!(
        child_keep_last_next.parent.child_ids,
        vec!["flow-events:child-keep-last-next".to_string()]
    );
    assert!(child_keep_last_queue
        .get_job("flow-events:child-keep-last-next")
        .await
        .expect("flow child keep-last next lookup should return")
        .is_none());
    child_keep_last_queue
        .complete_job(
            &child_keep_last_owner_claim.id,
            lock_token(&child_keep_last_owner_claim),
            serde_json::json!({ "owner": "done" }),
            Utc::now(),
        )
        .await
        .expect("flow child keep-last owner should complete");
    let materialized_child = child_keep_last_queue
        .get_job("flow-events:child-keep-last-next")
        .await
        .expect("materialized flow child keep-last lookup should return")
        .expect("materialized flow child keep-last should exist");
    assert_eq!(
        materialized_child.parent_id.as_deref(),
        Some(child_keep_last_next.parent.id.as_str())
    );
    assert_eq!(materialized_child.state, JobState::Waiting);
    let child_keep_last_counts = child_keep_last_queue
        .get_flow_dependency_counts(&child_keep_last_next.parent.id)
        .await
        .expect("flow child keep-last dependency counts should load")
        .expect("flow child keep-last parent should exist");
    assert_eq!(child_keep_last_counts.unprocessed, 1);
    assert_eq!(child_keep_last_counts.missing, 0);
    assert_eq!(
        child_keep_last_queue
            .get_job(&child_keep_last_next.parent.id)
            .await
            .expect("flow child keep-last parent lookup should return")
            .expect("flow child keep-last parent should exist")
            .state,
        JobState::WaitingChildren
    );
    let child_keep_last_next_claim = child_keep_last_queue
        .claim_next(
            "worker-flow-child-keep-last-next".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("flow child keep-last next claim should return")
        .expect("flow child keep-last next should be claimable");
    assert_eq!(
        child_keep_last_next_claim.id,
        "flow-events:child-keep-last-next"
    );
    child_keep_last_queue
        .complete_job(
            &child_keep_last_next_claim.id,
            lock_token(&child_keep_last_next_claim),
            serde_json::json!({ "next": "done" }),
            Utc::now(),
        )
        .await
        .expect("flow child keep-last next should complete");
    assert_eq!(
        child_keep_last_queue
            .get_job(&child_keep_last_next.parent.id)
            .await
            .expect("released flow child keep-last parent lookup should return")
            .expect("released flow child keep-last parent should exist")
            .state,
        JobState::Waiting
    );

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_flow_parent_transition_events(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("flow-parent-events:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("flow-parent-events:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-parent-events")
        .expect("valid Redis URL should build the flow parent events queue");

    let complete_flow = queue
        .add_flow_at(
            JobSpec::new(
                "event-complete-parent",
                serde_json::json!({ "kind": "aggregate" }),
            ),
            vec![
                JobSpec::new("event-complete-child-a", serde_json::json!({ "n": 1 })),
                JobSpec::new("event-complete-child-b", serde_json::json!({ "n": 2 })),
            ],
            Utc::now(),
        )
        .await
        .expect("complete flow should add");
    trace_stage("flow-parent-events:complete-flow-added");
    for (index, child) in complete_flow.children.iter().enumerate() {
        let claimed = queue
            .claim_next(
                format!("worker-complete-child-{index}"),
                Duration::from_secs(30),
                Utc::now(),
            )
            .await
            .expect("complete flow child claim should return")
            .expect("complete flow child should be claimable");
        assert_eq!(claimed.id, child.id);
        queue
            .complete_job(
                &claimed.id,
                lock_token(&claimed),
                serde_json::json!({ "ok": index }),
                Utc::now(),
            )
            .await
            .expect("complete flow child should complete");
    }
    trace_stage("flow-parent-events:complete-flow-completed");
    let complete_parent = queue
        .claim_next(
            "worker-complete-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("complete flow parent claim should return")
        .expect("complete flow parent should be claimable");
    assert_eq!(complete_parent.id, complete_flow.parent.id);
    queue
        .complete_job(
            &complete_parent.id,
            lock_token(&complete_parent),
            serde_json::json!({ "done": true }),
            Utc::now(),
        )
        .await
        .expect("complete flow parent should complete");

    let failed_flow = queue
        .add_flow_at(
            JobSpec::new(
                "event-failed-parent",
                serde_json::json!({ "kind": "aggregate" }),
            ),
            vec![JobSpec::new(
                "event-failed-child",
                serde_json::json!({ "critical": true }),
            )],
            Utc::now(),
        )
        .await
        .expect("failed flow should add");
    trace_stage("flow-parent-events:failed-flow-added");
    let failed_child = queue
        .claim_next(
            "worker-failed-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("failed flow child claim should return")
        .expect("failed flow child should be claimable");
    assert_eq!(failed_child.id, failed_flow.children[0].id);
    queue
        .fail_job(
            &failed_child.id,
            lock_token(&failed_child),
            "child exploded".to_string(),
            Utc::now(),
        )
        .await
        .expect("failed flow child should fail terminally");
    trace_stage("flow-parent-events:failed-flow-failed");

    let events = queue
        .read_events("-", "+", 100)
        .await
        .expect("flow parent events should read");
    let complete_parent_waiting = events
        .iter()
        .find(|event| {
            event.job_id.as_deref() == Some(complete_flow.parent.id.as_str())
                && event.event == "waiting"
        })
        .expect("complete flow parent waiting event should be emitted");
    assert_eq!(
        complete_parent_waiting.prev,
        Some(JobState::WaitingChildren)
    );
    let failed_parent_event = events
        .iter()
        .find(|event| {
            event.job_id.as_deref() == Some(failed_flow.parent.id.as_str())
                && event.event == "failed"
        })
        .expect("failed flow parent failed event should be emitted");
    assert_eq!(failed_parent_event.prev, Some(JobState::WaitingChildren));
    assert_eq!(
        failed_parent_event.fields.get("failedReason"),
        Some(&serde_json::Value::String(format!(
            "child job {} failed: child exploded",
            failed_flow.children[0].id
        )))
    );

    trace_stage("flow-parent-events:cleanup-final:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("flow-parent-events:cleanup-final:done");
    Ok(())
}

async fn run_retries_exhausted_event(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "retry-events")
        .expect("valid Redis URL should build the retry-events queue");
    let job = queue
        .add_job(
            "retry-event".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_retry_policy(RetryPolicy::fixed(1, Duration::from_millis(5))),
        )
        .await
        .expect("retry event job should add");
    let first = queue
        .claim_next(
            "worker-retry-event-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("retry event first claim should return")
        .expect("retry event job should be claimable");
    assert_eq!(first.id, job.id);
    queue
        .fail_job(
            &first.id,
            lock_token(&first),
            "temporary".to_string(),
            Utc::now(),
        )
        .await
        .expect("first failure should schedule retry");

    tokio::time::sleep(Duration::from_millis(10)).await;
    let second = queue
        .claim_next(
            "worker-retry-event-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("retry event second claim should return")
        .expect("retry event job should be claimable after delay");
    assert_eq!(second.id, job.id);
    assert_eq!(second.attempts_made, 2);
    queue
        .fail_job(
            &second.id,
            lock_token(&second),
            "terminal".to_string(),
            Utc::now(),
        )
        .await
        .expect("second failure should be terminal");

    let events = queue
        .read_events("-", "+", 20)
        .await
        .expect("retry events should read");
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
            "delayed",
            "waiting",
            "active",
            "failed",
            "retries-exhausted",
            "drained"
        ]
    );
    let exhausted = events
        .iter()
        .find(|event| event.event == "retries-exhausted")
        .expect("retries-exhausted event should be present");
    assert_eq!(exhausted.job_id.as_deref(), Some(job.id.as_str()));
    assert_eq!(
        exhausted.fields.get("attemptsMade"),
        Some(&serde_json::json!(2))
    );
    assert_eq!(events.last().unwrap().event, "drained");

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_stalled_recovery_events(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "stalled-events")
        .expect("valid Redis URL should build the stalled-events queue");
    let job = queue
        .add_job(
            "stalled-event".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_max_stalled_count(1),
        )
        .await
        .expect("stalled event job should add");
    let first = queue
        .claim_next(
            "worker-stalled-event-a".to_string(),
            Duration::from_millis(20),
            Utc::now(),
        )
        .await
        .expect("stalled event first claim should return")
        .expect("stalled event job should be claimable");
    assert_eq!(first.id, job.id);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled event first recovery pass should mark candidates"),
        0
    );
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled event first recovery pass should requeue"),
        1
    );

    let second = queue
        .claim_next(
            "worker-stalled-event-b".to_string(),
            Duration::from_millis(20),
            Utc::now(),
        )
        .await
        .expect("stalled event second claim should return")
        .expect("stalled event job should be claimable after requeue");
    assert_eq!(second.id, job.id);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled event second recovery pass should mark candidates"),
        0
    );
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled event second recovery pass should fail terminally"),
        1
    );

    let events = queue
        .read_events("-", "+", 20)
        .await
        .expect("stalled recovery events should read");
    let job_events = events
        .iter()
        .filter(|event| event.job_id.as_deref() == Some(job.id.as_str()))
        .collect::<Vec<_>>();
    let names = job_events
        .iter()
        .map(|event| event.event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["added", "waiting", "active", "stalled", "waiting", "active", "stalled", "failed"]
    );
    let stalled_events = job_events
        .iter()
        .filter(|event| event.event == "stalled")
        .collect::<Vec<_>>();
    assert_eq!(stalled_events.len(), 2);
    assert!(stalled_events.iter().all(|event| {
        event.fields.get("failedReason")
            == Some(&serde_json::Value::String(
                "job stalled after worker lease expired".to_string(),
            ))
    }));
    let failed_event = job_events
        .last()
        .expect("terminal failed event should be present");
    assert_eq!(failed_event.event, "failed");
    assert_eq!(failed_event.prev, Some(JobState::Active));

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_stalled_recovery_guards(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue_name = "stalled-recovery-guards";
    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, queue_name)
        .expect("valid Redis URL should build the stalled-recovery-guards queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let active_key = format!("{namespace}:{queue_name}:active");
    let waiting_key = format!("{namespace}:{queue_name}:waiting");
    let stalled_key = format!("{namespace}:{queue_name}:stalled");

    let locked = queue
        .add_job(
            "locked-stalled".to_string(),
            serde_json::json!({ "kind": "locked" }),
            JobOptions::new(),
        )
        .await
        .expect("locked stalled job should add");
    let locked_claim = queue
        .claim_next(
            "worker-stalled-locked".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("locked stalled claim should return")
        .expect("locked stalled job should be claimable");
    assert_eq!(locked_claim.id, locked.id);

    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("locked stalled recovery should mark candidates"),
        0
    );
    let locked_candidate: bool = conn.sismember(&stalled_key, &locked.id).await?;
    assert!(locked_candidate);
    assert_eq!(
        queue
            .get_job(&locked.id)
            .await
            .expect("locked stalled job should load")
            .expect("locked stalled job should remain")
            .state,
        JobState::Active
    );

    queue
        .complete_job(
            &locked_claim.id,
            lock_token(&locked_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("locked stalled job should complete with its valid token");
    let locked_candidate_after_complete: bool = conn.sismember(&stalled_key, &locked.id).await?;
    assert!(!locked_candidate_after_complete);

    let _: usize = conn.zadd(&active_key, &locked.id, 0.0).await?;
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stale completed active index recovery should run"),
        0
    );
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stale completed active index recovery should confirm"),
        0
    );
    let stale_completed_active_score: Option<f64> = conn.zscore(&active_key, &locked.id).await?;
    assert!(stale_completed_active_score.is_none());
    assert_eq!(
        queue
            .get_job(&locked.id)
            .await
            .expect("completed stale-index job should load")
            .expect("completed stale-index job should still exist")
            .state,
        JobState::Completed
    );

    let stalled = queue
        .add_job(
            "stalled-requeue".to_string(),
            serde_json::json!({ "kind": "requeue" }),
            JobOptions::new().with_max_stalled_count(2),
        )
        .await
        .expect("stalled requeue job should add");
    let first_claim = queue
        .claim_next(
            "worker-stalled-requeue-a".to_string(),
            Duration::from_millis(100),
            Utc::now(),
        )
        .await
        .expect("stalled requeue first claim should return")
        .expect("stalled requeue job should be claimable");
    assert_eq!(first_claim.id, stalled.id);
    let stale_token = lock_token(&first_claim).to_string();

    tokio::time::sleep(Duration::from_millis(180)).await;
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled requeue first recovery pass should mark candidates"),
        0
    );
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled requeue second recovery pass should requeue"),
        1
    );
    let requeued = queue
        .get_job(&stalled.id)
        .await
        .expect("requeued stalled job should load")
        .expect("requeued stalled job should still exist");
    assert_eq!(requeued.state, JobState::Waiting);
    assert_eq!(requeued.stalled_count, 1);
    assert!(requeued.worker_id.is_none());
    assert!(requeued.lock_token.is_none());
    let requeued_active_score: Option<f64> = conn.zscore(&active_key, &stalled.id).await?;
    assert!(requeued_active_score.is_none());
    let requeued_waiting_score: Option<f64> = conn.zscore(&waiting_key, &stalled.id).await?;
    assert!(requeued_waiting_score.is_some());
    let requeued_candidate: bool = conn.sismember(&stalled_key, &stalled.id).await?;
    assert!(!requeued_candidate);

    let reclaimed = queue
        .claim_next(
            "worker-stalled-requeue-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("reclaimed stalled job claim should return")
        .expect("requeued stalled job should be claimable again");
    assert_eq!(reclaimed.id, stalled.id);
    assert_ne!(lock_token(&reclaimed), stale_token);
    let stale_complete = queue
        .complete_job(
            &reclaimed.id,
            &stale_token,
            serde_json::json!({ "ok": false }),
            Utc::now(),
        )
        .await
        .expect_err("stale token must not complete a reclaimed stalled job");
    assert!(matches!(stale_complete, LaneError::JobLeaseConflict(_)));
    queue
        .complete_job(
            &reclaimed.id,
            lock_token(&reclaimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("reclaimed stalled job should complete with the current token");

    cleanup_namespace(&redis_url, &namespace).await
}

async fn run_worker_markers(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "markers")
        .expect("valid Redis URL should build the marker queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let marker_key = format!("{namespace}:markers:marker");
    let delayed_key = format!("{namespace}:markers:delayed");

    queue
        .add_job(
            "ready".to_string(),
            serde_json::json!({ "kind": "ready" }),
            JobOptions::new(),
        )
        .await
        .expect("waiting job should add");
    let ready_marker: Option<f64> = conn.zscore(&marker_key, "0").await?;
    assert_eq!(ready_marker, Some(0.0));

    let early = queue
        .add_job(
            "early-delayed".to_string(),
            serde_json::json!({ "kind": "early" }),
            JobOptions::new().with_delay(Duration::from_secs(10)),
        )
        .await
        .expect("early delayed job should add");
    let late = queue
        .add_job(
            "late-delayed".to_string(),
            serde_json::json!({ "kind": "late" }),
            JobOptions::new().with_delay(Duration::from_secs(30)),
        )
        .await
        .expect("late delayed job should add");

    let delayed_head: Vec<(String, f64)> = conn.zrange_withscores(&delayed_key, 0, 0).await?;
    assert_eq!(delayed_head.len(), 1);
    assert_eq!(delayed_head[0].0, early.id);
    let delayed_marker: Option<f64> = conn.zscore(&marker_key, "1").await?;
    assert_eq!(delayed_marker, Some(delayed_head[0].1));

    queue
        .promote_job(&early.id, Utc::now())
        .await
        .expect("early delayed job should promote");
    let ready_marker_after_promote: Option<f64> = conn.zscore(&marker_key, "0").await?;
    assert_eq!(ready_marker_after_promote, Some(0.0));
    let delayed_head_after_promote: Vec<(String, f64)> =
        conn.zrange_withscores(&delayed_key, 0, 0).await?;
    assert_eq!(delayed_head_after_promote.len(), 1);
    assert_eq!(delayed_head_after_promote[0].0, late.id);
    let delayed_marker_after_promote: Option<f64> = conn.zscore(&marker_key, "1").await?;
    assert_eq!(
        delayed_marker_after_promote,
        Some(delayed_head_after_promote[0].1)
    );

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    Ok(())
}

async fn run_paused_claim_promotion_marker(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "paused-marker")
        .expect("valid Redis URL should build the paused-marker queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let marker_key = format!("{namespace}:paused-marker:marker");

    queue
        .pause()
        .await
        .expect("paused-marker queue should pause");
    let delayed = queue
        .add_job(
            "paused-delayed".to_string(),
            serde_json::json!({ "kind": "paused" }),
            JobOptions::new().with_delay(Duration::from_millis(80)),
        )
        .await
        .expect("paused delayed job should add");
    tokio::time::sleep(Duration::from_millis(120)).await;

    assert!(queue
        .claim_next(
            "worker-paused-marker".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("paused-marker claim should return")
        .is_none());
    let waiting = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Waiting))
        .await
        .expect("paused-marker waiting jobs should list");
    assert!(waiting.jobs.iter().any(|job| job.id == delayed.id));
    let base_marker: Option<f64> = conn.zscore(&marker_key, "0").await?;
    assert!(base_marker.is_none());
    let delay_marker: Option<f64> = conn.zscore(&marker_key, "1").await?;
    assert!(delay_marker.is_none());

    queue
        .resume()
        .await
        .expect("paused-marker queue should resume");
    let resumed_marker: Option<f64> = conn.zscore(&marker_key, "0").await?;
    assert_eq!(resumed_marker, Some(0.0));
    let claimed = queue
        .claim_next(
            "worker-paused-marker-resumed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("resumed paused-marker claim should return")
        .expect("paused-marker job should claim after resume");
    assert_eq!(claimed.id, delayed.id);

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    Ok(())
}

async fn run_blocking_worker_markers(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking")
        .expect("valid Redis URL should build the blocking queue");
    let ready_worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking")
        .expect("valid Redis URL should build the ready worker queue");
    let ready_waiter = tokio::spawn(async move {
        ready_worker
            .claim_next_blocking(
                "worker-blocking-ready".to_string(),
                Duration::from_secs(30),
                Duration::from_secs(5),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let ready = queue
        .add_job(
            "ready-blocking".to_string(),
            serde_json::json!({ "kind": "ready" }),
            JobOptions::new(),
        )
        .await
        .expect("ready blocking job should add");
    let ready_claim = ready_waiter
        .await
        .expect("ready blocking waiter should join")
        .expect("ready blocking claim should return")
        .expect("ready blocking claim should find a job");
    assert_eq!(ready_claim.id, ready.id);

    let fanout_worker_a = RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking")
        .expect("valid Redis URL should build fanout worker A");
    let fanout_worker_b = RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking")
        .expect("valid Redis URL should build fanout worker B");
    let fanout_waiter_a = tokio::spawn(async move {
        fanout_worker_a
            .claim_next_blocking(
                "worker-blocking-fanout-a".to_string(),
                Duration::from_secs(30),
                Duration::from_secs(5),
            )
            .await
    });
    let fanout_waiter_b = tokio::spawn(async move {
        fanout_worker_b
            .claim_next_blocking(
                "worker-blocking-fanout-b".to_string(),
                Duration::from_secs(30),
                Duration::from_secs(5),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let bulk = queue
        .add_jobs(
            vec![
                JobSpec::new("bulk-blocking-a", serde_json::json!({ "slot": "a" })),
                JobSpec::new("bulk-blocking-b", serde_json::json!({ "slot": "b" })),
            ],
            Utc::now(),
        )
        .await
        .expect("bulk blocking jobs should add");
    let fanout_claim_a = fanout_waiter_a
        .await
        .expect("fanout waiter A should join")
        .expect("fanout claim A should return")
        .expect("fanout claim A should find a job");
    let fanout_claim_b = fanout_waiter_b
        .await
        .expect("fanout waiter B should join")
        .expect("fanout claim B should return")
        .expect("fanout claim B should find a job");
    let expected_ids = bulk
        .iter()
        .map(|job| job.id.as_str())
        .collect::<BTreeSet<_>>();
    let claimed_ids = [fanout_claim_a.id.as_str(), fanout_claim_b.id.as_str()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(claimed_ids, expected_ids);

    let delayed_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking-delayed")
        .expect("valid Redis URL should build the delayed blocking queue");
    let delayed = delayed_queue
        .add_job(
            "delayed-blocking".to_string(),
            serde_json::json!({ "kind": "delayed" }),
            JobOptions::new().with_delay(Duration::from_secs(2)),
        )
        .await
        .expect("delayed blocking job should add");
    let delayed_claim = delayed_queue
        .claim_next_blocking(
            "worker-blocking-delayed".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(5),
        )
        .await
        .expect("delayed blocking claim should return")
        .expect("delayed blocking claim should find a job");
    assert_eq!(delayed_claim.id, delayed.id);

    let paused_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking-paused")
        .expect("valid Redis URL should build the paused blocking queue");
    let paused = paused_queue
        .add_job(
            "paused-blocking".to_string(),
            serde_json::json!({ "kind": "paused" }),
            JobOptions::new(),
        )
        .await
        .expect("paused blocking job should add");
    paused_queue.pause().await.expect("queue should pause");
    let paused_worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking-paused")
        .expect("valid Redis URL should build the paused worker queue");
    let paused_waiter = tokio::spawn(async move {
        paused_worker
            .claim_next_blocking(
                "worker-blocking-paused".to_string(),
                Duration::from_secs(30),
                Duration::from_secs(5),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!paused_waiter.is_finished());
    paused_queue.resume().await.expect("queue should resume");
    let paused_claim = paused_waiter
        .await
        .expect("paused blocking waiter should join")
        .expect("paused blocking claim should return")
        .expect("paused blocking claim should find a job after resume");
    assert_eq!(paused_claim.id, paused.id);

    let max_active_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking-max-active")
            .expect("valid Redis URL should build the max-active blocking queue");
    max_active_queue
        .set_max_active_jobs(1)
        .await
        .expect("max-active blocking queue should configure concurrency");
    let max_active_first = max_active_queue
        .add_job(
            "max-active-first".to_string(),
            serde_json::json!({ "slot": "first" }),
            JobOptions::new(),
        )
        .await
        .expect("first max-active blocking job should add");
    let max_active_second = max_active_queue
        .add_job(
            "max-active-second".to_string(),
            serde_json::json!({ "slot": "second" }),
            JobOptions::new(),
        )
        .await
        .expect("second max-active blocking job should add");
    let max_active_first_claim = max_active_queue
        .claim_next(
            "worker-blocking-max-active-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first max-active blocking claim should return")
        .expect("first max-active blocking job should claim");
    assert_eq!(max_active_first_claim.id, max_active_first.id);

    let max_active_worker =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking-max-active")
            .expect("valid Redis URL should build the max-active blocking worker queue");
    let max_active_waiter = tokio::spawn(async move {
        max_active_worker
            .claim_next_blocking(
                "worker-blocking-max-active-b".to_string(),
                Duration::from_secs(30),
                Duration::from_secs(5),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!max_active_waiter.is_finished());
    max_active_queue
        .complete_job(
            &max_active_first_claim.id,
            lock_token(&max_active_first_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("first max-active blocking job should complete");
    let max_active_second_claim = tokio::time::timeout(Duration::from_secs(10), max_active_waiter)
        .await
        .expect("max-active blocking waiter should wake after active release")
        .expect("max-active blocking waiter should join")
        .expect("max-active blocking claim should return")
        .expect("max-active blocking claim should find the waiting job");
    assert_eq!(max_active_second_claim.id, max_active_second.id);
    max_active_queue
        .complete_job(
            &max_active_second_claim.id,
            lock_token(&max_active_second_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("second max-active blocking job should complete");

    let rate_limit_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking-rate-limit")
            .expect("valid Redis URL should build the rate-limit blocking queue");
    let rate_limit_worker =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking-rate-limit")
            .expect("valid Redis URL should build the rate-limit blocking worker")
            .with_claim_rate_limit(JobRateLimit::new(1, Duration::from_millis(500)))
            .expect("rate-limit blocking worker config should be valid");
    let rate_limit_first = rate_limit_queue
        .add_job(
            "rate-limit-first".to_string(),
            serde_json::json!({ "slot": "first" }),
            JobOptions::new(),
        )
        .await
        .expect("first rate-limit blocking job should add");
    let rate_limit_second = rate_limit_queue
        .add_job(
            "rate-limit-second".to_string(),
            serde_json::json!({ "slot": "second" }),
            JobOptions::new(),
        )
        .await
        .expect("second rate-limit blocking job should add");
    let rate_limit_first_claim = rate_limit_worker
        .claim_next(
            "worker-blocking-rate-limit-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("first rate-limit blocking claim should return")
        .expect("first rate-limit blocking job should claim");
    assert_eq!(rate_limit_first_claim.id, rate_limit_first.id);

    let rate_limit_blocking_worker =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking-rate-limit")
            .expect("valid Redis URL should build the blocking rate-limit worker")
            .with_claim_rate_limit(JobRateLimit::new(1, Duration::from_millis(500)))
            .expect("blocking rate-limit worker config should be valid");
    let rate_limit_waiter = tokio::spawn(async move {
        rate_limit_blocking_worker
            .claim_next_blocking(
                "worker-blocking-rate-limit-b".to_string(),
                Duration::from_secs(30),
                Duration::from_secs(2),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!rate_limit_waiter.is_finished());
    let rate_limit_second_claim = tokio::time::timeout(Duration::from_secs(3), rate_limit_waiter)
        .await
        .expect("rate-limit blocking waiter should wake after limiter TTL")
        .expect("rate-limit blocking waiter should join")
        .expect("rate-limit blocking claim should return")
        .expect("rate-limit blocking claim should find the waiting job");
    assert_eq!(rate_limit_second_claim.id, rate_limit_second.id);
    rate_limit_worker
        .complete_job(
            &rate_limit_first_claim.id,
            lock_token(&rate_limit_first_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("first rate-limit blocking job should complete");
    rate_limit_worker
        .complete_job(
            &rate_limit_second_claim.id,
            lock_token(&rate_limit_second_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("second rate-limit blocking job should complete");

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_blocking_job_worker(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let queue = Arc::new(
        RedisJobQueue::with_namespace(&redis_url, &namespace, "blocking-worker")
            .expect("valid Redis URL should build the blocking worker queue"),
    );
    let backend: Arc<dyn JobQueueBackend> = queue.clone();
    let processor: Arc<dyn JobProcessor> = Arc::new(job_processor_fn(
        |job: Job, context: JobContext| async move {
            context.add_log("processed by blocking worker").await?;
            Ok(serde_json::json!({ "name": job.name }))
        },
    ));
    let worker = JobWorker::new(
        backend,
        processor,
        JobWorkerConfig::new("worker-blocking-runtime")
            .with_lease_renew_interval(Duration::ZERO)
            .with_poll_interval(Duration::from_secs(30))
            .with_blocking_claim_timeout(Duration::from_secs(5)),
    );

    let worker_run =
        tokio::spawn(async move { worker.run_once_blocking(Duration::from_secs(5)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let job = queue
        .add_job(
            "runtime-blocking".to_string(),
            serde_json::json!({ "kind": "runtime" }),
            JobOptions::new(),
        )
        .await
        .expect("runtime blocking job should add");
    let outcome = tokio::time::timeout(Duration::from_secs(15), worker_run)
        .await
        .expect("blocking worker should finish after marker wake-up")
        .expect("blocking worker task should join")
        .expect("blocking worker run should succeed");
    let completed = match outcome {
        JobRunOutcome::Completed(job) => job,
        other => panic!("expected blocking worker to complete a job, got {other:?}"),
    };
    assert_eq!(completed.id, job.id);
    assert_eq!(
        completed.return_value,
        Some(serde_json::json!({ "name": "runtime-blocking" }))
    );

    let stored = queue
        .get_job(&job.id)
        .await
        .expect("completed job lookup should return")
        .expect("completed job should remain stored");
    assert_eq!(stored.state, JobState::Completed);
    assert_eq!(stored.logs.len(), 1);
    assert_eq!(stored.logs[0].line, "processed by blocking worker");

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

async fn run_keep_last_manual_release_cleanup(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("keep-last-release:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("keep-last-release:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "dedup")
        .expect("valid Redis URL should build the dedup queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let key = format!("{namespace}:dedup:deduplication:tenant:keep-last-release");
    let next_key = format!("{namespace}:dedup:deduplication_next:tenant:keep-last-release");

    let owner = queue
        .add_job(
            "dedup-keep-last-release-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:keep-last-release").keep_last_if_active(true),
            ),
        )
        .await
        .expect("keep-last release owner should be added");
    let claimed = queue
        .claim_next(
            "worker-keep-last-release".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("keep-last release owner should be claimable")
        .expect("keep-last release owner should be returned");
    assert_eq!(claimed.id, owner.id);

    let duplicate = queue
        .add_job(
            "dedup-keep-last-release-stale".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:keep-last-release").keep_last_if_active(true),
            ),
        )
        .await
        .expect("keep-last release duplicate should return owner");
    assert_eq!(duplicate.id, owner.id);
    let next_raw: String = conn.get(&next_key).await?;
    let next: Job = serde_json::from_str(&next_raw).expect("stored next job should decode");
    assert_eq!(next.name, "dedup-keep-last-release-stale");

    assert!(queue
        .remove_deduplication_key("tenant:keep-last-release")
        .await
        .expect("keep-last release key should be removable"));
    let next_after_remove: Option<String> = conn.get(&next_key).await?;
    assert!(next_after_remove.is_none());
    let owner_after_remove: Option<String> = conn.get(&key).await?;
    assert!(owner_after_remove.is_none());

    let replacement = queue
        .add_job(
            "dedup-keep-last-release-replacement".to_string(),
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:keep-last-release").keep_last_if_active(true),
            ),
        )
        .await
        .expect("keep-last release replacement should be added");
    assert_ne!(replacement.id, owner.id);
    assert_eq!(
        queue
            .get_deduplication_job_id("tenant:keep-last-release")
            .await
            .expect("keep-last release owner should load")
            .as_deref(),
        Some(replacement.id.as_str())
    );

    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("keep-last release owner should complete");
    assert_eq!(
        queue
            .get_deduplication_job_id("tenant:keep-last-release")
            .await
            .expect("keep-last release owner should remain replacement")
            .as_deref(),
        Some(replacement.id.as_str())
    );
    assert!(queue
        .get_job(&next.id)
        .await
        .expect("stale keep-last next lookup should return")
        .is_none());
    let jobs = queue
        .list_jobs(JobListOptions::new())
        .await
        .expect("keep-last release jobs should list");
    assert!(!jobs.jobs.iter().any(|job| job.id == next.id));

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_keep_last_owner_removal_cleanup(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("keep-last-owner-removal:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("keep-last-owner-removal:cleanup:done");

    let remove_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "dedup-remove")
        .expect("valid Redis URL should build the dedup removal queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let remove_deduplication =
        DeduplicationOptions::new("tenant:keep-last-remove").keep_last_if_active(true);
    let remove_key = format!("{namespace}:dedup-remove:deduplication:tenant:keep-last-remove");
    let remove_next_key =
        format!("{namespace}:dedup-remove:deduplication_next:tenant:keep-last-remove");
    let remove_owner = remove_queue
        .add_job(
            "dedup-keep-last-remove-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(remove_deduplication.clone()),
        )
        .await
        .expect("keep-last remove owner should add");
    let remove_claim = remove_queue
        .claim_next(
            "worker-keep-last-remove".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("keep-last remove claim should return")
        .expect("keep-last remove owner should claim");
    assert_eq!(remove_claim.id, remove_owner.id);
    let remove_duplicate = remove_queue
        .add_job(
            "dedup-keep-last-remove-next".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(remove_deduplication),
        )
        .await
        .expect("keep-last remove duplicate should return owner");
    assert_eq!(remove_duplicate.id, remove_owner.id);
    let remove_next_before: Option<String> = conn.get(&remove_next_key).await?;
    assert!(remove_next_before.is_some());
    remove_queue
        .release_active_job(&remove_claim.id, lock_token(&remove_claim), Utc::now())
        .await
        .expect("keep-last remove owner should release to waiting");
    let remove_next_after_release: Option<String> = conn.get(&remove_next_key).await?;
    assert!(remove_next_after_release.is_some());
    let removed_owner = remove_queue
        .remove_job(&remove_owner.id)
        .await
        .expect("keep-last waiting owner should remove")
        .expect("keep-last waiting owner should be returned");
    assert_eq!(removed_owner.id, remove_owner.id);
    let remove_next_after: Option<String> = conn.get(&remove_next_key).await?;
    assert!(remove_next_after.is_none());
    let remove_owner_after: Option<String> = conn.get(&remove_key).await?;
    assert!(remove_owner_after.is_none());

    let clean_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "dedup-clean")
        .expect("valid Redis URL should build the dedup clean queue");
    let clean_deduplication =
        DeduplicationOptions::new("tenant:keep-last-clean").keep_last_if_active(true);
    let clean_key = format!("{namespace}:dedup-clean:deduplication:tenant:keep-last-clean");
    let clean_next_key =
        format!("{namespace}:dedup-clean:deduplication_next:tenant:keep-last-clean");
    let clean_owner = clean_queue
        .add_job(
            "dedup-keep-last-clean-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(clean_deduplication.clone()),
        )
        .await
        .expect("keep-last clean owner should add");
    let clean_claim = clean_queue
        .claim_next(
            "worker-keep-last-clean".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("keep-last clean claim should return")
        .expect("keep-last clean owner should claim");
    assert_eq!(clean_claim.id, clean_owner.id);
    let clean_duplicate = clean_queue
        .add_job(
            "dedup-keep-last-clean-next".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(clean_deduplication),
        )
        .await
        .expect("keep-last clean duplicate should return owner");
    assert_eq!(clean_duplicate.id, clean_owner.id);
    let clean_next_before: Option<String> = conn.get(&clean_next_key).await?;
    assert!(clean_next_before.is_some());
    clean_queue
        .release_active_job(&clean_claim.id, lock_token(&clean_claim), Utc::now())
        .await
        .expect("keep-last clean owner should release to waiting");
    let cleaned = clean_queue
        .clean_jobs(JobState::Waiting, Duration::from_millis(0), 10, Utc::now())
        .await
        .expect("keep-last waiting owner should clean");
    assert!(cleaned.iter().any(|job| job.id == clean_owner.id));
    let clean_next_after: Option<String> = conn.get(&clean_next_key).await?;
    assert!(clean_next_after.is_none());
    let clean_owner_after: Option<String> = conn.get(&clean_key).await?;
    assert!(clean_owner_after.is_none());

    let drain_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "dedup-drain")
        .expect("valid Redis URL should build the dedup drain queue");
    let drain_deduplication =
        DeduplicationOptions::new("tenant:keep-last-drain").keep_last_if_active(true);
    let drain_key = format!("{namespace}:dedup-drain:deduplication:tenant:keep-last-drain");
    let drain_next_key =
        format!("{namespace}:dedup-drain:deduplication_next:tenant:keep-last-drain");
    let drain_owner = drain_queue
        .add_job(
            "dedup-keep-last-drain-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(drain_deduplication.clone()),
        )
        .await
        .expect("keep-last drain owner should add");
    let drain_claim = drain_queue
        .claim_next(
            "worker-keep-last-drain".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("keep-last drain claim should return")
        .expect("keep-last drain owner should claim");
    assert_eq!(drain_claim.id, drain_owner.id);
    let drain_duplicate = drain_queue
        .add_job(
            "dedup-keep-last-drain-next".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(drain_deduplication),
        )
        .await
        .expect("keep-last drain duplicate should return owner");
    assert_eq!(drain_duplicate.id, drain_owner.id);
    let drain_next_before: Option<String> = conn.get(&drain_next_key).await?;
    assert!(drain_next_before.is_some());
    drain_queue
        .release_active_job(&drain_claim.id, lock_token(&drain_claim), Utc::now())
        .await
        .expect("keep-last drain owner should release to waiting");
    let drained = drain_queue
        .drain_jobs(false)
        .await
        .expect("keep-last waiting owner should drain");
    assert!(drained.iter().any(|job| job.id == drain_owner.id));
    let drain_next_after: Option<String> = conn.get(&drain_next_key).await?;
    assert!(drain_next_after.is_none());
    let drain_owner_after: Option<String> = conn.get(&drain_key).await?;
    assert!(drain_owner_after.is_none());

    let flow_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "dedup-flow-remove")
        .expect("valid Redis URL should build the dedup flow removal queue");
    let flow_deduplication =
        DeduplicationOptions::new("tenant:keep-last-flow-child").keep_last_if_active(true);
    let flow_key =
        format!("{namespace}:dedup-flow-remove:deduplication:tenant:keep-last-flow-child");
    let flow_next_key =
        format!("{namespace}:dedup-flow-remove:deduplication_next:tenant:keep-last-flow-child");
    let flow = flow_queue
        .add_flow_at(
            JobSpec::new(
                "dedup-flow-remove-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_priority(1)),
            vec![JobSpec::new(
                "dedup-flow-remove-child",
                serde_json::json!({ "version": 1 }),
            )
            .with_options(JobOptions::new().with_deduplication(flow_deduplication.clone()))],
            Utc::now(),
        )
        .await
        .expect("keep-last flow should add");
    let flow_child_claim = flow_queue
        .claim_next(
            "worker-keep-last-flow-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("keep-last flow child claim should return")
        .expect("keep-last flow child should claim");
    assert_eq!(flow_child_claim.id, flow.children[0].id);
    let flow_duplicate = flow_queue
        .add_job(
            "dedup-flow-remove-child-next".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(flow_deduplication),
        )
        .await
        .expect("keep-last flow child duplicate should return owner");
    assert_eq!(flow_duplicate.id, flow.children[0].id);
    let flow_next_before: Option<String> = conn.get(&flow_next_key).await?;
    assert!(flow_next_before.is_some());
    flow_queue
        .release_active_job(
            &flow_child_claim.id,
            lock_token(&flow_child_claim),
            Utc::now(),
        )
        .await
        .expect("keep-last flow child should release to waiting");
    let removed_children = flow_queue
        .remove_unprocessed_children(&flow.parent.id, Utc::now())
        .await
        .expect("keep-last flow child removal should run")
        .expect("keep-last flow parent should exist");
    assert!(removed_children
        .iter()
        .any(|job| job.id == flow.children[0].id));
    let flow_next_after: Option<String> = conn.get(&flow_next_key).await?;
    assert!(flow_next_after.is_none());
    let flow_owner_after: Option<String> = conn.get(&flow_key).await?;
    assert!(flow_owner_after.is_none());

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_keep_last_stale_owner_cleanup(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("keep-last-stale-owner:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("keep-last-stale-owner:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "dedup")
        .expect("valid Redis URL should build the dedup queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let key = format!("{namespace}:dedup:deduplication:tenant:keep-last-stale-owner");
    let next_key = format!("{namespace}:dedup:deduplication_next:tenant:keep-last-stale-owner");
    let jobs_key = format!("{namespace}:dedup:jobs");

    let owner = queue
        .add_job(
            "dedup-keep-last-stale-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:keep-last-stale-owner").keep_last_if_active(true),
            ),
        )
        .await
        .expect("keep-last stale owner should be added");
    let claimed = queue
        .claim_next(
            "worker-keep-last-stale-owner".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("keep-last stale owner should be claimable")
        .expect("keep-last stale owner should be returned");
    assert_eq!(claimed.id, owner.id);

    let duplicate = queue
        .add_job(
            "dedup-keep-last-stale-next".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:keep-last-stale-owner").keep_last_if_active(true),
            ),
        )
        .await
        .expect("keep-last stale duplicate should return owner");
    assert_eq!(duplicate.id, owner.id);
    let next_raw: Option<String> = conn.get(&next_key).await?;
    assert!(next_raw.is_some());

    let removed: usize = conn.hdel(&jobs_key, &owner.id).await?;
    assert_eq!(removed, 1);
    assert!(queue
        .get_deduplication_job_id("tenant:keep-last-stale-owner")
        .await
        .expect("stale keep-last owner getter should return")
        .is_none());
    let owner_after_stale_get: Option<String> = conn.get(&key).await?;
    assert!(owner_after_stale_get.is_none());
    let next_after_stale_get: Option<String> = conn.get(&next_key).await?;
    assert!(next_after_stale_get.is_none());

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_ttl_dedup_finalization(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("dedup-ttl-finalization:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("dedup-ttl-finalization:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "dedup-ttl")
        .expect("valid Redis URL should build the TTL dedup queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let dedup_key = format!("{namespace}:dedup-ttl:deduplication:tenant:ttl-finalization");

    let owner = queue
        .add_job(
            "dedup-ttl-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:ttl-finalization")
                    .with_ttl(Duration::from_secs(30)),
            ),
        )
        .await
        .expect("TTL dedup owner should be added");
    let claimed = queue
        .claim_next(
            "worker-ttl-finalization".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("TTL dedup owner should be claimable")
        .expect("TTL dedup owner should be returned");
    assert_eq!(claimed.id, owner.id);
    let completed = queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("TTL dedup owner should complete");
    assert_eq!(completed.state, JobState::Completed);

    let pttl_after_completion: i64 = redis::cmd("PTTL")
        .arg(&dedup_key)
        .query_async(&mut conn)
        .await?;
    assert!(
        pttl_after_completion > 0,
        "TTL dedup key should keep its Redis TTL after completion, got {pttl_after_completion}"
    );
    let owner_after_completion: Option<String> = conn.get(&dedup_key).await?;
    assert_eq!(owner_after_completion.as_deref(), Some(owner.id.as_str()));
    let getter_after_completion = queue
        .get_deduplication_job_id("tenant:ttl-finalization")
        .await
        .expect("TTL dedup getter should return");
    assert_eq!(getter_after_completion.as_deref(), Some(owner.id.as_str()));

    let duplicate = queue
        .add_job(
            "dedup-ttl-duplicate".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:ttl-finalization")
                    .with_ttl(Duration::from_secs(30)),
            ),
        )
        .await
        .expect("duplicate before TTL expiration should return completed owner");
    assert_eq!(duplicate.id, owner.id);
    assert_eq!(duplicate.state, JobState::Completed);

    let shortened: bool = redis::cmd("PEXPIRE")
        .arg(&dedup_key)
        .arg(1_u16)
        .query_async(&mut conn)
        .await?;
    assert!(shortened);
    tokio::time::sleep(Duration::from_millis(20)).await;

    let after_ttl = queue
        .add_job(
            "dedup-ttl-after-expiration".to_string(),
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:ttl-finalization")
                    .with_ttl(Duration::from_secs(30)),
            ),
        )
        .await
        .expect("dedup id should be reusable after TTL expiration");
    assert_ne!(after_ttl.id, owner.id);
    assert_eq!(after_ttl.state, JobState::Waiting);
    let owner_after_ttl: Option<String> = conn.get(&dedup_key).await?;
    assert_eq!(owner_after_ttl.as_deref(), Some(after_ttl.id.as_str()));
    queue
        .remove_job(&after_ttl.id)
        .await
        .expect("post-TTL owner should remove")
        .expect("post-TTL owner should be returned");

    let failed_dedup_key =
        format!("{namespace}:dedup-ttl:deduplication:tenant:ttl-finalization-failed");
    let failed_owner = queue
        .add_job(
            "dedup-ttl-failed-owner".to_string(),
            serde_json::json!({ "version": 4 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:ttl-finalization-failed")
                    .with_ttl(Duration::from_secs(30)),
            ),
        )
        .await
        .expect("failed TTL dedup owner should be added");
    let failed_claim = queue
        .claim_next(
            "worker-ttl-finalization-failed".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("failed TTL dedup owner should be claimable")
        .expect("failed TTL dedup owner should be returned");
    assert_eq!(failed_claim.id, failed_owner.id);
    let failed = queue
        .fail_job(
            &failed_claim.id,
            lock_token(&failed_claim),
            "boom".to_string(),
            Utc::now(),
        )
        .await
        .expect("TTL dedup owner should fail");
    assert_eq!(failed.state, JobState::Failed);
    let failed_pttl: i64 = redis::cmd("PTTL")
        .arg(&failed_dedup_key)
        .query_async(&mut conn)
        .await?;
    assert!(
        failed_pttl > 0,
        "TTL dedup key should keep its Redis TTL after failure, got {failed_pttl}"
    );
    let failed_getter = queue
        .get_deduplication_job_id("tenant:ttl-finalization-failed")
        .await
        .expect("failed TTL dedup getter should return");
    assert_eq!(failed_getter.as_deref(), Some(failed_owner.id.as_str()));
    let failed_duplicate = queue
        .add_job(
            "dedup-ttl-failed-duplicate".to_string(),
            serde_json::json!({ "version": 5 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:ttl-finalization-failed")
                    .with_ttl(Duration::from_secs(30)),
            ),
        )
        .await
        .expect("duplicate before failed TTL expiration should return failed owner");
    assert_eq!(failed_duplicate.id, failed_owner.id);
    assert_eq!(failed_duplicate.state, JobState::Failed);

    let removed_complete_dedup_key =
        format!("{namespace}:dedup-ttl:deduplication:tenant:ttl-finalization-removed-complete");
    let removed_complete_owner = queue
        .add_job(
            "dedup-ttl-removed-complete-owner".to_string(),
            serde_json::json!({ "version": 6 }),
            JobOptions::new()
                .remove_on_complete(true)
                .with_deduplication(
                    DeduplicationOptions::new("tenant:ttl-finalization-removed-complete")
                        .with_ttl(Duration::from_secs(30)),
                ),
        )
        .await
        .expect("remove-on-complete TTL owner should add");
    let removed_complete_claim = queue
        .claim_next(
            "worker-ttl-finalization-removed-complete".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("remove-on-complete TTL owner should be claimable")
        .expect("remove-on-complete TTL owner should be returned");
    assert_eq!(removed_complete_claim.id, removed_complete_owner.id);
    queue
        .complete_job(
            &removed_complete_claim.id,
            lock_token(&removed_complete_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("remove-on-complete TTL owner should complete");
    assert!(queue
        .get_job(&removed_complete_owner.id)
        .await
        .expect("removed completed owner lookup should return")
        .is_none());
    let removed_complete_pttl: i64 = redis::cmd("PTTL")
        .arg(&removed_complete_dedup_key)
        .query_async(&mut conn)
        .await?;
    assert!(
        removed_complete_pttl > 0,
        "remove-on-complete should leave the TTL dedup key until Redis expiration, got {removed_complete_pttl}"
    );
    let removed_complete_key_owner: Option<String> = conn.get(&removed_complete_dedup_key).await?;
    assert_eq!(
        removed_complete_key_owner.as_deref(),
        Some(removed_complete_owner.id.as_str())
    );

    let removed_fail_dedup_key =
        format!("{namespace}:dedup-ttl:deduplication:tenant:ttl-finalization-removed-fail");
    let removed_fail_owner = queue
        .add_job(
            "dedup-ttl-removed-fail-owner".to_string(),
            serde_json::json!({ "version": 7 }),
            JobOptions::new().remove_on_fail(true).with_deduplication(
                DeduplicationOptions::new("tenant:ttl-finalization-removed-fail")
                    .with_ttl(Duration::from_secs(30)),
            ),
        )
        .await
        .expect("remove-on-fail TTL owner should add");
    let removed_fail_claim = queue
        .claim_next(
            "worker-ttl-finalization-removed-fail".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("remove-on-fail TTL owner should be claimable")
        .expect("remove-on-fail TTL owner should be returned");
    assert_eq!(removed_fail_claim.id, removed_fail_owner.id);
    queue
        .fail_job(
            &removed_fail_claim.id,
            lock_token(&removed_fail_claim),
            "boom".to_string(),
            Utc::now(),
        )
        .await
        .expect("remove-on-fail TTL owner should fail");
    assert!(queue
        .get_job(&removed_fail_owner.id)
        .await
        .expect("removed failed owner lookup should return")
        .is_none());
    let removed_fail_pttl: i64 = redis::cmd("PTTL")
        .arg(&removed_fail_dedup_key)
        .query_async(&mut conn)
        .await?;
    assert!(
        removed_fail_pttl > 0,
        "remove-on-fail should leave the TTL dedup key until Redis expiration, got {removed_fail_pttl}"
    );
    let removed_fail_key_owner: Option<String> = conn.get(&removed_fail_dedup_key).await?;
    assert_eq!(
        removed_fail_key_owner.as_deref(),
        Some(removed_fail_owner.id.as_str())
    );

    let stalled_dedup_key =
        format!("{namespace}:dedup-ttl:deduplication:tenant:ttl-finalization-stalled");
    let stalled_owner = queue
        .add_job(
            "dedup-ttl-stalled-owner".to_string(),
            serde_json::json!({ "version": 8 }),
            JobOptions::new()
                .remove_on_fail(true)
                .with_max_stalled_count(0)
                .with_deduplication(
                    DeduplicationOptions::new("tenant:ttl-finalization-stalled")
                        .with_ttl(Duration::from_secs(30)),
                ),
        )
        .await
        .expect("stalled TTL owner should add");
    let stalled_claim = queue
        .claim_next(
            "worker-ttl-finalization-stalled".to_string(),
            Duration::from_millis(20),
            Utc::now(),
        )
        .await
        .expect("stalled TTL owner should be claimable")
        .expect("stalled TTL owner should be returned");
    assert_eq!(stalled_claim.id, stalled_owner.id);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled TTL first recovery pass should run"),
        0
    );
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled TTL terminal recovery pass should run"),
        1
    );
    assert!(queue
        .get_job(&stalled_owner.id)
        .await
        .expect("removed stalled owner lookup should return")
        .is_none());
    let stalled_pttl: i64 = redis::cmd("PTTL")
        .arg(&stalled_dedup_key)
        .query_async(&mut conn)
        .await?;
    assert!(
        stalled_pttl > 0,
        "stalled terminal failure should leave the TTL dedup key until Redis expiration, got {stalled_pttl}"
    );
    let stalled_key_owner: Option<String> = conn.get(&stalled_dedup_key).await?;
    assert_eq!(
        stalled_key_owner.as_deref(),
        Some(stalled_owner.id.as_str())
    );

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
}

async fn run_flow_keep_last_parent_completion(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("flow-keep-last:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("flow-keep-last:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-keep-last")
        .expect("valid Redis URL should build the flow keep-last queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let deduplication =
        DeduplicationOptions::new("tenant:flow-keep-last").keep_last_if_active(true);
    let next_key = format!("{namespace}:flow-keep-last:deduplication_next:tenant:flow-keep-last");

    let owner_flow = queue
        .add_flow_at(
            JobSpec::new("flow-owner-parent", serde_json::json!({ "version": 1 }))
                .with_options(JobOptions::new().with_deduplication(deduplication.clone())),
            vec![JobSpec::new(
                "flow-owner-child",
                serde_json::json!({ "version": 1 }),
            )],
            Utc::now(),
        )
        .await
        .expect("owner flow should be added");
    trace_stage("flow-keep-last:owner-added");

    let owner_child = queue
        .claim_next(
            "worker-flow-owner-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("owner child claim should return")
        .expect("owner child should be claimable");
    assert_eq!(owner_child.id, owner_flow.children[0].id);
    queue
        .complete_job(
            &owner_child.id,
            lock_token(&owner_child),
            serde_json::json!({ "child": true }),
            Utc::now(),
        )
        .await
        .expect("owner child should complete");

    let owner_parent = queue
        .claim_next(
            "worker-flow-owner-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("owner parent claim should return")
        .expect("owner parent should be claimable");
    assert_eq!(owner_parent.id, owner_flow.parent.id);
    trace_stage("flow-keep-last:owner-parent-active");

    let stale_duplicate = queue
        .add_flow_at(
            JobSpec::new("flow-stale-parent", serde_json::json!({ "version": 2 })).with_options(
                JobOptions::new()
                    .with_job_id("flow-stale-parent-id")
                    .with_deduplication(deduplication.clone()),
            ),
            vec![
                JobSpec::new("flow-stale-child", serde_json::json!({ "version": 2 }))
                    .with_options(JobOptions::new().with_job_id("flow-stale-child-id")),
            ],
            Utc::now(),
        )
        .await
        .expect("stale duplicate flow should return owner");
    assert_eq!(stale_duplicate.parent.id, owner_flow.parent.id);

    let latest_duplicate = queue
        .add_flow_at(
            JobSpec::new("flow-latest-parent", serde_json::json!({ "version": 3 })).with_options(
                JobOptions::new()
                    .with_job_id("flow-latest-parent-id")
                    .with_deduplication(deduplication),
            ),
            vec![
                JobSpec::new("flow-latest-child", serde_json::json!({ "version": 3 }))
                    .with_options(JobOptions::new().with_job_id("flow-latest-child-id")),
            ],
            Utc::now(),
        )
        .await
        .expect("latest duplicate flow should return owner");
    assert_eq!(latest_duplicate.parent.id, owner_flow.parent.id);
    trace_stage("flow-keep-last:duplicates-added");

    let next_raw: String = conn.get(&next_key).await?;
    let next_payload: serde_json::Value =
        serde_json::from_str(&next_raw).expect("stored next flow should decode");
    assert_eq!(
        next_payload.get("kind").and_then(|value| value.as_str()),
        Some("flow")
    );
    assert_eq!(
        next_payload
            .get("parent")
            .and_then(|value| value.get("name"))
            .and_then(|value| value.as_str()),
        Some("flow-latest-parent")
    );
    let next_children = next_payload
        .get("children")
        .and_then(|value| value.as_array())
        .expect("stored next flow should include children");
    assert_eq!(next_children.len(), 1);
    assert_eq!(
        next_children[0]
            .get("name")
            .and_then(|value| value.as_str()),
        Some("flow-latest-child")
    );

    queue
        .complete_job(
            &owner_parent.id,
            lock_token(&owner_parent),
            serde_json::json!({ "parent": true }),
            Utc::now(),
        )
        .await
        .expect("owner parent should complete");
    trace_stage("flow-keep-last:owner-parent-completed");

    let next_after: Option<String> = conn.get(&next_key).await?;
    assert!(next_after.is_none());
    assert!(queue
        .get_job("flow-stale-parent-id")
        .await
        .expect("stale parent lookup should return")
        .is_none());
    assert!(queue
        .get_job("flow-stale-child-id")
        .await
        .expect("stale child lookup should return")
        .is_none());

    let latest_parent = queue
        .get_job("flow-latest-parent-id")
        .await
        .expect("latest parent lookup should return")
        .expect("latest parent should exist");
    assert_eq!(latest_parent.name, "flow-latest-parent");
    assert_eq!(latest_parent.state, JobState::WaitingChildren);
    assert_eq!(latest_parent.child_ids, vec!["flow-latest-child-id"]);
    assert!(latest_parent.parent_id.is_none());
    let latest_child = queue
        .get_job("flow-latest-child-id")
        .await
        .expect("latest child lookup should return")
        .expect("latest child should exist");
    assert_eq!(latest_child.name, "flow-latest-child");
    assert_eq!(latest_child.state, JobState::Waiting);
    assert_eq!(
        latest_child.parent_id.as_deref(),
        Some("flow-latest-parent-id")
    );
    assert!(latest_child.child_ids.is_empty());
    assert_eq!(
        queue
            .get_deduplication_job_id("tenant:flow-keep-last")
            .await
            .expect("flow keep-last owner should load")
            .as_deref(),
        Some("flow-latest-parent-id")
    );

    let waiting_children = queue
        .list_jobs(JobListOptions::new().with_state(JobState::WaitingChildren))
        .await
        .expect("waiting-children jobs should list");
    assert_eq!(waiting_children.total, 1);
    assert_eq!(waiting_children.jobs[0].id, "flow-latest-parent-id");
    let waiting = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Waiting))
        .await
        .expect("waiting jobs should list");
    assert_eq!(waiting.total, 1);
    assert_eq!(waiting.jobs[0].id, "flow-latest-child-id");

    let latest_child_claim = queue
        .claim_next(
            "worker-flow-latest-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("latest child claim should return")
        .expect("latest child should be claimable");
    assert_eq!(latest_child_claim.id, "flow-latest-child-id");
    queue
        .complete_job(
            &latest_child_claim.id,
            lock_token(&latest_child_claim),
            serde_json::json!({ "child": true }),
            Utc::now(),
        )
        .await
        .expect("latest child should complete");
    let latest_parent_claim = queue
        .claim_next(
            "worker-flow-latest-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("latest parent claim should return")
        .expect("latest parent should be claimable");
    assert_eq!(latest_parent_claim.id, "flow-latest-parent-id");

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("flow-keep-last:cleanup-final:done");
    Ok(())
}

async fn run_flow_keep_last_parent_failure(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("flow-keep-last-fail:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("flow-keep-last-fail:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-keep-last-fail")
        .expect("valid Redis URL should build the flow keep-last failure queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let deduplication =
        DeduplicationOptions::new("tenant:flow-keep-last-fail").keep_last_if_active(true);
    let next_key =
        format!("{namespace}:flow-keep-last-fail:deduplication_next:tenant:flow-keep-last-fail");

    let owner_flow = queue
        .add_flow_at(
            JobSpec::new(
                "flow-fail-owner-parent",
                serde_json::json!({ "version": 1 }),
            )
            .with_options(JobOptions::new().with_deduplication(deduplication.clone())),
            vec![JobSpec::new(
                "flow-fail-owner-child",
                serde_json::json!({ "version": 1 }),
            )],
            Utc::now(),
        )
        .await
        .expect("owner failure flow should be added");

    let owner_child = queue
        .claim_next(
            "worker-flow-fail-owner-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("owner failure child claim should return")
        .expect("owner failure child should be claimable");
    assert_eq!(owner_child.id, owner_flow.children[0].id);
    queue
        .complete_job(
            &owner_child.id,
            lock_token(&owner_child),
            serde_json::json!({ "child": true }),
            Utc::now(),
        )
        .await
        .expect("owner failure child should complete");

    let owner_parent = queue
        .claim_next(
            "worker-flow-fail-owner-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("owner failure parent claim should return")
        .expect("owner failure parent should be claimable");
    assert_eq!(owner_parent.id, owner_flow.parent.id);

    let stale_duplicate = queue
        .add_flow_at(
            JobSpec::new(
                "flow-fail-stale-parent",
                serde_json::json!({ "version": 2 }),
            )
            .with_options(
                JobOptions::new()
                    .with_job_id("flow-fail-stale-parent-id")
                    .with_deduplication(deduplication.clone()),
            ),
            vec![
                JobSpec::new("flow-fail-stale-child", serde_json::json!({ "version": 2 }))
                    .with_options(JobOptions::new().with_job_id("flow-fail-stale-child-id")),
            ],
            Utc::now(),
        )
        .await
        .expect("stale failure duplicate flow should return owner");
    assert_eq!(stale_duplicate.parent.id, owner_flow.parent.id);

    let latest_duplicate = queue
        .add_flow_at(
            JobSpec::new(
                "flow-fail-latest-parent",
                serde_json::json!({ "version": 3 }),
            )
            .with_options(
                JobOptions::new()
                    .with_job_id("flow-fail-latest-parent-id")
                    .with_deduplication(deduplication),
            ),
            vec![JobSpec::new(
                "flow-fail-latest-child",
                serde_json::json!({ "version": 3 }),
            )
            .with_options(JobOptions::new().with_job_id("flow-fail-latest-child-id"))],
            Utc::now(),
        )
        .await
        .expect("latest failure duplicate flow should return owner");
    assert_eq!(latest_duplicate.parent.id, owner_flow.parent.id);

    let next_raw: String = conn.get(&next_key).await?;
    let next_payload: serde_json::Value =
        serde_json::from_str(&next_raw).expect("stored failure next flow should decode");
    assert_eq!(
        next_payload
            .get("parent")
            .and_then(|value| value.get("name"))
            .and_then(|value| value.as_str()),
        Some("flow-fail-latest-parent")
    );

    queue
        .fail_job(
            &owner_parent.id,
            lock_token(&owner_parent),
            "owner parent failed".to_string(),
            Utc::now(),
        )
        .await
        .expect("owner failure parent should fail terminally");

    let next_after: Option<String> = conn.get(&next_key).await?;
    assert!(next_after.is_none());
    assert!(queue
        .get_job("flow-fail-stale-parent-id")
        .await
        .expect("stale failure parent lookup should return")
        .is_none());
    assert!(queue
        .get_job("flow-fail-stale-child-id")
        .await
        .expect("stale failure child lookup should return")
        .is_none());

    let latest_parent = queue
        .get_job("flow-fail-latest-parent-id")
        .await
        .expect("latest failure parent lookup should return")
        .expect("latest failure parent should exist");
    assert_eq!(latest_parent.name, "flow-fail-latest-parent");
    assert_eq!(latest_parent.state, JobState::WaitingChildren);
    assert_eq!(latest_parent.child_ids, vec!["flow-fail-latest-child-id"]);
    let latest_child = queue
        .get_job("flow-fail-latest-child-id")
        .await
        .expect("latest failure child lookup should return")
        .expect("latest failure child should exist");
    assert_eq!(latest_child.name, "flow-fail-latest-child");
    assert_eq!(latest_child.state, JobState::Waiting);
    assert_eq!(
        latest_child.parent_id.as_deref(),
        Some("flow-fail-latest-parent-id")
    );
    assert_eq!(
        queue
            .get_deduplication_job_id("tenant:flow-keep-last-fail")
            .await
            .expect("failure flow keep-last owner should load")
            .as_deref(),
        Some("flow-fail-latest-parent-id")
    );

    let latest_child_claim = queue
        .claim_next(
            "worker-flow-fail-latest-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("latest failure child claim should return")
        .expect("latest failure child should be claimable");
    assert_eq!(latest_child_claim.id, "flow-fail-latest-child-id");
    queue
        .complete_job(
            &latest_child_claim.id,
            lock_token(&latest_child_claim),
            serde_json::json!({ "child": true }),
            Utc::now(),
        )
        .await
        .expect("latest failure child should complete");
    let latest_parent_claim = queue
        .claim_next(
            "worker-flow-fail-latest-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("latest failure parent claim should return")
        .expect("latest failure parent should be claimable");
    assert_eq!(latest_parent_claim.id, "flow-fail-latest-parent-id");

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("flow-keep-last-fail:cleanup-final:done");
    Ok(())
}

async fn run_flow_keep_last_parent_stalled_failure(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("flow-keep-last-stalled:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("flow-keep-last-stalled:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-keep-last-stalled")
        .expect("valid Redis URL should build the flow keep-last stalled queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let deduplication =
        DeduplicationOptions::new("tenant:flow-keep-last-stalled").keep_last_if_active(true);
    let next_key = format!(
        "{namespace}:flow-keep-last-stalled:deduplication_next:tenant:flow-keep-last-stalled"
    );

    let owner_flow = queue
        .add_flow_at(
            JobSpec::new(
                "flow-stalled-owner-parent",
                serde_json::json!({ "version": 1 }),
            )
            .with_options(
                JobOptions::new()
                    .with_max_stalled_count(0)
                    .with_deduplication(deduplication.clone()),
            ),
            vec![JobSpec::new(
                "flow-stalled-owner-child",
                serde_json::json!({ "version": 1 }),
            )],
            Utc::now(),
        )
        .await
        .expect("owner stalled flow should be added");

    let owner_child = queue
        .claim_next(
            "worker-flow-stalled-owner-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("owner stalled child claim should return")
        .expect("owner stalled child should be claimable");
    assert_eq!(owner_child.id, owner_flow.children[0].id);
    queue
        .complete_job(
            &owner_child.id,
            lock_token(&owner_child),
            serde_json::json!({ "child": true }),
            Utc::now(),
        )
        .await
        .expect("owner stalled child should complete");

    let owner_parent = queue
        .claim_next(
            "worker-flow-stalled-owner-parent".to_string(),
            Duration::from_millis(50),
            Utc::now(),
        )
        .await
        .expect("owner stalled parent claim should return")
        .expect("owner stalled parent should be claimable");
    assert_eq!(owner_parent.id, owner_flow.parent.id);

    let stale_duplicate = queue
        .add_flow_at(
            JobSpec::new(
                "flow-stalled-stale-parent",
                serde_json::json!({ "version": 2 }),
            )
            .with_options(
                JobOptions::new()
                    .with_job_id("flow-stalled-stale-parent-id")
                    .with_deduplication(deduplication.clone()),
            ),
            vec![JobSpec::new(
                "flow-stalled-stale-child",
                serde_json::json!({ "version": 2 }),
            )
            .with_options(JobOptions::new().with_job_id("flow-stalled-stale-child-id"))],
            Utc::now(),
        )
        .await
        .expect("stale stalled duplicate flow should return owner");
    assert_eq!(stale_duplicate.parent.id, owner_flow.parent.id);

    let latest_duplicate = queue
        .add_flow_at(
            JobSpec::new(
                "flow-stalled-latest-parent",
                serde_json::json!({ "version": 3 }),
            )
            .with_options(
                JobOptions::new()
                    .with_job_id("flow-stalled-latest-parent-id")
                    .with_deduplication(deduplication),
            ),
            vec![JobSpec::new(
                "flow-stalled-latest-child",
                serde_json::json!({ "version": 3 }),
            )
            .with_options(JobOptions::new().with_job_id("flow-stalled-latest-child-id"))],
            Utc::now(),
        )
        .await
        .expect("latest stalled duplicate flow should return owner");
    assert_eq!(latest_duplicate.parent.id, owner_flow.parent.id);

    let next_raw: String = conn.get(&next_key).await?;
    let next_payload: serde_json::Value =
        serde_json::from_str(&next_raw).expect("stored stalled next flow should decode");
    assert_eq!(
        next_payload
            .get("parent")
            .and_then(|value| value.get("name"))
            .and_then(|value| value.as_str()),
        Some("flow-stalled-latest-parent")
    );

    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled flow recovery should mark candidates"),
        0
    );
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("stalled flow recovery should terminally fail owner"),
        1
    );

    let next_after: Option<String> = conn.get(&next_key).await?;
    assert!(next_after.is_none());
    assert!(queue
        .get_job("flow-stalled-stale-parent-id")
        .await
        .expect("stale stalled parent lookup should return")
        .is_none());
    assert!(queue
        .get_job("flow-stalled-stale-child-id")
        .await
        .expect("stale stalled child lookup should return")
        .is_none());

    let latest_parent = queue
        .get_job("flow-stalled-latest-parent-id")
        .await
        .expect("latest stalled parent lookup should return")
        .expect("latest stalled parent should exist");
    assert_eq!(latest_parent.name, "flow-stalled-latest-parent");
    assert_eq!(latest_parent.state, JobState::WaitingChildren);
    assert_eq!(
        latest_parent.child_ids,
        vec!["flow-stalled-latest-child-id"]
    );
    let latest_child = queue
        .get_job("flow-stalled-latest-child-id")
        .await
        .expect("latest stalled child lookup should return")
        .expect("latest stalled child should exist");
    assert_eq!(latest_child.name, "flow-stalled-latest-child");
    assert_eq!(latest_child.state, JobState::Waiting);
    assert_eq!(
        latest_child.parent_id.as_deref(),
        Some("flow-stalled-latest-parent-id")
    );
    assert_eq!(
        queue
            .get_deduplication_job_id("tenant:flow-keep-last-stalled")
            .await
            .expect("stalled flow keep-last owner should load")
            .as_deref(),
        Some("flow-stalled-latest-parent-id")
    );

    let latest_child_claim = queue
        .claim_next(
            "worker-flow-stalled-latest-child".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("latest stalled child claim should return")
        .expect("latest stalled child should be claimable");
    assert_eq!(latest_child_claim.id, "flow-stalled-latest-child-id");
    queue
        .complete_job(
            &latest_child_claim.id,
            lock_token(&latest_child_claim),
            serde_json::json!({ "child": true }),
            Utc::now(),
        )
        .await
        .expect("latest stalled child should complete");
    let latest_parent_claim = queue
        .claim_next(
            "worker-flow-stalled-latest-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("latest stalled parent claim should return")
        .expect("latest stalled parent should be claimable");
    assert_eq!(latest_parent_claim.id, "flow-stalled-latest-parent-id");

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("flow-keep-last-stalled:cleanup-final:done");
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

async fn run_repeat_add_scheduler_metadata(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-add-scheduler:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-add-scheduler:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-add-scheduler")
        .expect("valid Redis URL should build the repeat add scheduler queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let repeat_owner = queue
        .add_job(
            "scheduler-repeat-owner".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(60))
                .with_repeat(
                    RepeatOptions::every(Duration::from_secs(60)).with_key("scheduler-add"),
                ),
        )
        .await
        .expect("scheduler-backed repeat owner should add");
    let owner_key = format!("{namespace}:repeat-add-scheduler:repeat:scheduler-add");
    let scheduler_meta_key = format!("{namespace}:repeat-add-scheduler:repeat_meta:scheduler-add");
    let scheduler_owner_id: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(
        scheduler_owner_id.as_deref(),
        Some(repeat_owner.id.as_str())
    );

    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);
    let duplicate = queue
        .add_job(
            "scheduler-repeat-duplicate".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60)).with_key("scheduler-add"),
            ),
        )
        .await
        .expect("repeat duplicate add should recover scheduler metadata owner");
    trace_stage("repeat-add-scheduler:single-duplicate");
    assert_eq!(duplicate.id, repeat_owner.id);
    let restored_owner_after_add: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(
        restored_owner_after_add.as_deref(),
        Some(repeat_owner.id.as_str())
    );

    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);
    let bulk =
        queue
            .add_many(vec![JobSpec::new(
                "scheduler-repeat-bulk-duplicate",
                serde_json::json!({ "version": 3 }),
            )
            .with_options(JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60)).with_key("scheduler-add"),
            ))])
            .await
            .expect("repeat bulk add should recover scheduler metadata owner");
    trace_stage("repeat-add-scheduler:bulk-duplicate");
    assert_eq!(bulk.len(), 1);
    assert_eq!(bulk[0].id, repeat_owner.id);
    let restored_owner_after_bulk: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(
        restored_owner_after_bulk.as_deref(),
        Some(repeat_owner.id.as_str())
    );

    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);
    let flow_error = queue
        .add_flow(
            JobSpec::new("repeat-flow-parent", serde_json::json!({})),
            vec![
                JobSpec::new("repeat-flow-child", serde_json::json!({ "version": 4 }))
                    .with_options(JobOptions::new().with_repeat(
                        RepeatOptions::every(Duration::from_secs(60)).with_key("scheduler-add"),
                    )),
            ],
        )
        .await
        .expect_err("flow repeat duplicate should reject scheduler metadata owner");
    assert!(matches!(flow_error, LaneError::ConfigError(_)));
    let restored_owner_after_flow: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(
        restored_owner_after_flow.as_deref(),
        Some(repeat_owner.id.as_str())
    );

    let parent = queue
        .add_job(
            "dynamic-repeat-parent".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .expect("dynamic repeat parent should add");
    let claimed_parent = queue
        .claim_next(
            "worker-dynamic-repeat-parent".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic repeat parent claim should return")
        .expect("dynamic repeat parent should claim");
    assert_eq!(claimed_parent.id, parent.id);

    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);
    let dynamic_error = queue
        .add_flow_children(
            &claimed_parent.id,
            lock_token(&claimed_parent),
            vec![
                JobSpec::new("dynamic-repeat-child", serde_json::json!({ "version": 5 }))
                    .with_options(JobOptions::new().with_repeat(
                        RepeatOptions::every(Duration::from_secs(60)).with_key("scheduler-add"),
                    )),
            ],
        )
        .await
        .expect_err("dynamic flow repeat duplicate should reject scheduler metadata owner");
    assert!(matches!(dynamic_error, LaneError::ConfigError(_)));
    let restored_owner_after_dynamic: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(
        restored_owner_after_dynamic.as_deref(),
        Some(repeat_owner.id.as_str())
    );

    assert_eq!(
        queue
            .count_repeats()
            .await
            .expect("repeat count should load after scheduler add checks"),
        1
    );

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-add-scheduler:cleanup-final:done");
    Ok(())
}

async fn run_repeat_expired_end_at_validation(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-expired-end-at:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-expired-end-at:cleanup:done");

    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let add_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-expired-add")
        .expect("valid Redis URL should build the expired repeat add queue");
    let add_error = add_queue
        .add_job(
            "expired-add".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .until(Utc::now() - chrono::Duration::seconds(1))
                    .with_key("expired-add"),
            ),
        )
        .await
        .expect_err("expired repeat add should reject before Redis writes");
    assert!(matches!(add_error, LaneError::ConfigError(_)));
    assert_eq!(add_queue.stats().await.unwrap().total, 0);
    assert_eq!(add_queue.count_repeats().await.unwrap(), 0);
    let add_jobs_len: usize = conn
        .hlen(format!("{namespace}:repeat-expired-add:jobs"))
        .await?;
    assert_eq!(add_jobs_len, 0);
    let add_scheduler_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-expired-add:repeat"),
            "expired-add",
        )
        .await?;
    assert!(add_scheduler_score.is_none());

    let bulk_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-expired-bulk")
        .expect("valid Redis URL should build the expired repeat bulk queue");
    let bulk_error = bulk_queue
        .add_many_at(
            vec![
                JobSpec::new("valid-bulk", serde_json::json!({ "ok": true }))
                    .with_options(JobOptions::new().with_job_id("expired-bulk-valid")),
                JobSpec::new("expired-bulk", serde_json::json!({ "ok": false })).with_options(
                    JobOptions::new()
                        .with_job_id("expired-bulk-repeat")
                        .with_repeat(
                            RepeatOptions::every(Duration::from_secs(60))
                                .until(ts(999))
                                .with_key("expired-bulk"),
                        ),
                ),
            ],
            ts(1_000),
        )
        .await
        .expect_err("expired repeat bulk add should reject before partial writes");
    assert!(matches!(bulk_error, LaneError::ConfigError(_)));
    assert_eq!(bulk_queue.stats().await.unwrap().total, 0);
    let bulk_jobs_len: usize = conn
        .hlen(format!("{namespace}:repeat-expired-bulk:jobs"))
        .await?;
    assert_eq!(bulk_jobs_len, 0);
    let bulk_valid_hash: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-expired-bulk:jobs"),
            "expired-bulk-valid",
        )
        .await?;
    assert!(bulk_valid_hash.is_none());

    let flow_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-expired-flow")
        .expect("valid Redis URL should build the expired repeat flow queue");
    let flow_error = flow_queue
        .add_flow_at(
            JobSpec::new("flow-parent", serde_json::json!({}))
                .with_options(JobOptions::new().with_job_id("expired-flow-parent")),
            vec![
                JobSpec::new("flow-child", serde_json::json!({})).with_options(
                    JobOptions::new()
                        .with_job_id("expired-flow-child")
                        .with_repeat(
                            RepeatOptions::every(Duration::from_secs(60))
                                .until(ts(999))
                                .with_key("expired-flow"),
                        ),
                ),
            ],
            ts(1_000),
        )
        .await
        .expect_err("expired repeat flow add should reject before partial writes");
    assert!(matches!(flow_error, LaneError::ConfigError(_)));
    assert_eq!(flow_queue.stats().await.unwrap().total, 0);
    let flow_jobs_len: usize = conn
        .hlen(format!("{namespace}:repeat-expired-flow:jobs"))
        .await?;
    assert_eq!(flow_jobs_len, 0);

    let dynamic_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-expired-dynamic")
            .expect("valid Redis URL should build the expired dynamic flow queue");
    let parent = dynamic_queue
        .add_job(
            "dynamic-parent".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_job_id("expired-dynamic-parent"),
        )
        .await
        .expect("dynamic parent should add");
    let claimed_parent = dynamic_queue
        .claim_next(
            "worker-repeat-expired-dynamic".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dynamic parent claim should return")
        .expect("dynamic parent should be claimable");
    assert_eq!(claimed_parent.id, parent.id);
    let dynamic_error = dynamic_queue
        .add_flow_children_at(
            &claimed_parent.id,
            lock_token(&claimed_parent),
            vec![
                JobSpec::new("dynamic-child", serde_json::json!({})).with_options(
                    JobOptions::new()
                        .with_job_id("expired-dynamic-child")
                        .with_repeat(
                            RepeatOptions::every(Duration::from_secs(60))
                                .until(ts(999))
                                .with_key("expired-dynamic"),
                        ),
                ),
            ],
            ts(1_000),
        )
        .await
        .expect_err("expired dynamic repeat child should reject before parent movement");
    assert!(matches!(dynamic_error, LaneError::ConfigError(_)));
    assert_eq!(dynamic_queue.stats().await.unwrap().total, 1);
    let dynamic_child_hash: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-expired-dynamic:jobs"),
            "expired-dynamic-child",
        )
        .await?;
    assert!(dynamic_child_hash.is_none());
    let dynamic_parent_active_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-expired-dynamic:active"),
            &claimed_parent.id,
        )
        .await?;
    assert!(dynamic_parent_active_score.is_some());
    let dynamic_parent_waiting_children_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-expired-dynamic:waiting_children"),
            &claimed_parent.id,
        )
        .await?;
    assert!(dynamic_parent_waiting_children_score.is_none());

    let upsert_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-expired-upsert")
            .expect("valid Redis URL should build the expired repeat upsert queue");
    let upsert_error = upsert_queue
        .upsert_repeat(
            JobSpec::new("expired-upsert", serde_json::json!({})).with_options(
                JobOptions::new()
                    .with_job_id("expired-upsert-job")
                    .with_repeat(
                        RepeatOptions::every(Duration::from_secs(60))
                            .until(ts(999))
                            .with_key("expired-upsert"),
                    ),
            ),
            ts(1_000),
        )
        .await
        .expect_err("expired repeat upsert should reject before Redis writes");
    assert!(matches!(upsert_error, LaneError::ConfigError(_)));
    assert_eq!(upsert_queue.stats().await.unwrap().total, 0);
    assert_eq!(upsert_queue.count_repeats().await.unwrap(), 0);
    let upsert_jobs_len: usize = conn
        .hlen(format!("{namespace}:repeat-expired-upsert:jobs"))
        .await?;
    assert_eq!(upsert_jobs_len, 0);
    let upsert_owner: Option<String> = conn
        .get(format!(
            "{namespace}:repeat-expired-upsert:repeat:expired-upsert"
        ))
        .await?;
    assert!(upsert_owner.is_none());
    let upsert_meta_exists: bool = conn
        .exists(format!(
            "{namespace}:repeat-expired-upsert:repeat_meta:expired-upsert"
        ))
        .await?;
    assert!(!upsert_meta_exists);

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-expired-end-at:cleanup-final:done");
    Ok(())
}

async fn run_reserved_job_id_validation(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("reserved-job-id:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("reserved-job-id:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "reserved-job-id")
        .expect("valid Redis URL should build the reserved job-id queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let now = ts(1_000);

    let zero_id = queue
        .add_job(
            "reserved-zero".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_job_id("0"),
        )
        .await
        .expect_err("job id 0 should reject before Redis writes");
    assert!(matches!(zero_id, LaneError::ConfigError(_)));
    assert_eq!(queue.stats().await.unwrap().total, 0);

    let integer_id = queue
        .add_job(
            "reserved-integer".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_job_id("42"),
        )
        .await
        .expect_err("integer job id should reject before Redis writes");
    assert!(matches!(integer_id, LaneError::ConfigError(_)));
    assert_eq!(queue.stats().await.unwrap().total, 0);

    let marker_prefix = queue
        .add_many_at(
            vec![
                JobSpec::new("valid", serde_json::json!({}))
                    .with_options(JobOptions::new().with_job_id("valid-id")),
                JobSpec::new("reserved-marker", serde_json::json!({}))
                    .with_options(JobOptions::new().with_job_id("0:delayed")),
            ],
            now,
        )
        .await
        .expect_err("reserved marker-like id should reject before partial bulk writes");
    assert!(matches!(marker_prefix, LaneError::ConfigError(_)));
    assert_eq!(queue.stats().await.unwrap().total, 0);
    let jobs_len: usize = conn
        .hlen(format!("{namespace}:reserved-job-id:jobs"))
        .await?;
    assert_eq!(jobs_len, 0);

    let priority_limit = queue
        .add_many_at(
            vec![
                JobSpec::new("valid-priority", serde_json::json!({}))
                    .with_options(JobOptions::new().with_job_id("priority-valid")),
                JobSpec::new("priority-too-high", serde_json::json!({}))
                    .with_options(JobOptions::new().with_priority(MAX_JOB_PRIORITY + 1)),
            ],
            now,
        )
        .await
        .expect_err("priority above BullMQ limit should reject before partial bulk writes");
    assert!(matches!(priority_limit, LaneError::ConfigError(_)));
    assert_eq!(queue.stats().await.unwrap().total, 0);
    let jobs_len: usize = conn
        .hlen(format!("{namespace}:reserved-job-id:jobs"))
        .await?;
    assert_eq!(jobs_len, 0);

    let flow_error = queue
        .add_flow_at(
            JobSpec::new("reserved-parent", serde_json::json!({}))
                .with_options(JobOptions::new().with_job_id("0:parent")),
            vec![JobSpec::new("child", serde_json::json!({}))],
            now,
        )
        .await
        .expect_err("reserved flow parent id should reject before Redis writes");
    assert!(matches!(flow_error, LaneError::ConfigError(_)));
    assert_eq!(queue.stats().await.unwrap().total, 0);

    let upsert_error = queue
        .upsert_repeat(
            JobSpec::new("reserved-repeat", serde_json::json!({})).with_options(
                JobOptions::new().with_job_id("0:repeat").with_repeat(
                    RepeatOptions::every(Duration::from_secs(60)).with_key("reserved-repeat"),
                ),
            ),
            now,
        )
        .await
        .expect_err("reserved repeat owner id should reject before Redis writes");
    assert!(matches!(upsert_error, LaneError::ConfigError(_)));
    assert_eq!(queue.stats().await.unwrap().total, 0);
    assert_eq!(queue.count_repeats().await.unwrap(), 0);
    let repeat_owner: Option<String> = conn
        .get(format!(
            "{namespace}:reserved-job-id:repeat:reserved-repeat"
        ))
        .await?;
    assert!(repeat_owner.is_none());

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("reserved-job-id:cleanup-final:done");
    Ok(())
}

async fn run_repeat_upsert(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-upsert:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-upsert:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-upsert")
        .expect("valid Redis URL should build the repeat upsert queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let replacement_end_at = Utc::now() + chrono::Duration::days(1);

    let first = queue
        .add_job(
            "heartbeat".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(5)).with_key("heartbeat-series"),
            ),
        )
        .await
        .expect("first repeat owner should add");
    trace_stage("repeat-upsert:first-added");

    let replacement = queue
        .upsert_repeat(
            JobSpec::new("heartbeat-v2", serde_json::json!({ "version": 2 })).with_options(
                JobOptions::new()
                    .with_delay(Duration::from_secs(30))
                    .with_repeat(
                        RepeatOptions::every(Duration::from_secs(30))
                            .with_limit(5)
                            .until(replacement_end_at)
                            .with_key("heartbeat-series"),
                    ),
            ),
            Utc::now(),
        )
        .await
        .expect("repeat upsert should replace non-active owner");
    trace_stage("repeat-upsert:replaced");
    assert_ne!(replacement.id, first.id);
    assert_eq!(replacement.name, "heartbeat-v2");
    assert_eq!(replacement.payload, serde_json::json!({ "version": 2 }));
    assert_eq!(replacement.state, JobState::Delayed);
    assert_eq!(replacement.repeat_key.as_deref(), Some("heartbeat-series"));

    let old_hash: Option<String> = conn
        .hget(format!("{namespace}:repeat-upsert:jobs"), &first.id)
        .await?;
    assert!(old_hash.is_none());
    let old_waiting_score: Option<f64> = conn
        .zscore(format!("{namespace}:repeat-upsert:waiting"), &first.id)
        .await?;
    assert!(old_waiting_score.is_none());
    let replacement_delayed_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-upsert:delayed"),
            &replacement.id,
        )
        .await?;
    assert!(replacement_delayed_score.is_some());
    let owner_id: Option<String> = conn
        .get(format!("{namespace}:repeat-upsert:repeat:heartbeat-series"))
        .await?;
    assert_eq!(owner_id.as_deref(), Some(replacement.id.as_str()));
    let scheduler_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-upsert:repeat"),
            "heartbeat-series",
        )
        .await?;
    assert!(scheduler_score.is_some());
    let scheduler_job_id: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "jid",
        )
        .await?;
    assert_eq!(scheduler_job_id.as_deref(), Some(replacement.id.as_str()));
    let scheduler_name: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "name",
        )
        .await?;
    assert_eq!(scheduler_name.as_deref(), Some("heartbeat-v2"));
    let scheduler_key: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "key",
        )
        .await?;
    assert_eq!(scheduler_key.as_deref(), Some("heartbeat-series"));
    let scheduler_every: Option<u64> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "every",
        )
        .await?;
    assert_eq!(scheduler_every, Some(30_000));
    let scheduler_limit: Option<u32> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "limit",
        )
        .await?;
    assert_eq!(scheduler_limit, Some(5));
    let scheduler_end_date: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "endDate",
        )
        .await?;
    let expected_end_date = serde_json::to_value(replacement_end_at)
        .expect("replacement end timestamp should serialize")
        .as_str()
        .expect("replacement end timestamp should serialize as a string")
        .to_string();
    assert_eq!(
        scheduler_end_date.as_deref(),
        Some(expected_end_date.as_str())
    );

    let cron_replacement = queue
        .upsert_repeat(
            JobSpec::new("heartbeat-cron", serde_json::json!({ "version": 3 })).with_options(
                JobOptions::new().with_repeat(
                    RepeatOptions::cron("0/1 * * * * * *")
                        .with_limit(7)
                        .with_key("heartbeat-series"),
                ),
            ),
            Utc::now(),
        )
        .await
        .expect("repeat upsert should rewrite scheduler metadata");
    trace_stage("repeat-upsert:rewrote-metadata");
    assert_ne!(cron_replacement.id, replacement.id);
    assert_eq!(cron_replacement.name, "heartbeat-cron");
    let replacement_hash: Option<String> = conn
        .hget(format!("{namespace}:repeat-upsert:jobs"), &replacement.id)
        .await?;
    assert!(replacement_hash.is_none());
    let rewritten_scheduler_job_id: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "jid",
        )
        .await?;
    assert_eq!(
        rewritten_scheduler_job_id.as_deref(),
        Some(cron_replacement.id.as_str())
    );
    let rewritten_scheduler_pattern: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "pattern",
        )
        .await?;
    assert_eq!(
        rewritten_scheduler_pattern.as_deref(),
        Some("0/1 * * * * * *")
    );
    let stale_scheduler_every: Option<u64> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "every",
        )
        .await?;
    assert!(stale_scheduler_every.is_none());
    let stale_scheduler_end_date: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "endDate",
        )
        .await?;
    assert!(stale_scheduler_end_date.is_none());
    let rewritten_scheduler_limit: Option<u32> = conn
        .hget(
            format!("{namespace}:repeat-upsert:repeat_meta:heartbeat-series"),
            "limit",
        )
        .await?;
    assert_eq!(rewritten_scheduler_limit, Some(7));

    let entry = queue
        .get_repeat("heartbeat-series")
        .await
        .expect("repeat upsert entry should load")
        .expect("repeat upsert entry should exist");
    assert_eq!(entry.job_id, cron_replacement.id);
    assert_eq!(entry.name, "heartbeat-cron");
    assert_eq!(entry.options.cron_expression(), Some("0/1 * * * * * *"));
    assert_eq!(
        queue
            .count_repeats()
            .await
            .expect("repeat upsert count should load"),
        1
    );

    let active_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-upsert-active")
            .expect("valid Redis URL should build the repeat upsert active queue");
    let active = active_queue
        .add_job(
            "active-heartbeat".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(5)).with_key("active-series"),
            ),
        )
        .await
        .expect("active repeat owner should add");
    let claimed = active_queue
        .claim_next(
            "worker-repeat-upsert-active".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("active repeat owner claim should return")
        .expect("active repeat owner should be claimable");
    assert_eq!(claimed.id, active.id);
    let active_error = active_queue
        .upsert_repeat(
            JobSpec::new("active-heartbeat-v2", serde_json::json!({ "version": 2 })).with_options(
                JobOptions::new().with_repeat(
                    RepeatOptions::every(Duration::from_secs(30)).with_key("active-series"),
                ),
            ),
            Utc::now(),
        )
        .await
        .expect_err("active repeat owner should reject upsert");
    assert!(matches!(active_error, LaneError::JobLeaseConflict(_)));
    let active_owner_id: Option<String> = conn
        .get(format!(
            "{namespace}:repeat-upsert-active:repeat:active-series"
        ))
        .await?;
    assert_eq!(active_owner_id.as_deref(), Some(active.id.as_str()));

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-upsert:cleanup-final:done");
    Ok(())
}

async fn run_repeat_upsert_scheduler_metadata(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-upsert-scheduler:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-upsert-scheduler:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-upsert-scheduler")
        .expect("valid Redis URL should build the repeat upsert scheduler queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let first = queue
        .add_job(
            "scheduler-owned-repeat".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(30))
                .with_repeat(
                    RepeatOptions::every(Duration::from_secs(60)).with_key("scheduler-owned"),
                ),
        )
        .await
        .expect("scheduler-owned repeat should add");
    let owner_key = format!("{namespace}:repeat-upsert-scheduler:repeat:scheduler-owned");
    let scheduler_key = format!("{namespace}:repeat-upsert-scheduler:repeat");
    let scheduler_meta_key =
        format!("{namespace}:repeat-upsert-scheduler:repeat_meta:scheduler-owned");
    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);
    let scheduler_owner_id: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(scheduler_owner_id.as_deref(), Some(first.id.as_str()));

    let replacement = queue
        .upsert_repeat(
            JobSpec::new(
                "scheduler-owned-repeat-v2",
                serde_json::json!({ "version": 2 }),
            )
            .with_options(
                JobOptions::new()
                    .with_delay(Duration::from_secs(60))
                    .with_repeat(
                        RepeatOptions::every(Duration::from_secs(90)).with_key("scheduler-owned"),
                    ),
            ),
            Utc::now(),
        )
        .await
        .expect("repeat upsert should replace scheduler metadata owner");
    trace_stage("repeat-upsert-scheduler:replaced");
    assert_ne!(replacement.id, first.id);
    assert_eq!(replacement.name, "scheduler-owned-repeat-v2");

    let old_hash: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-upsert-scheduler:jobs"),
            &first.id,
        )
        .await?;
    assert!(old_hash.is_none());
    let old_delayed_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-upsert-scheduler:delayed"),
            &first.id,
        )
        .await?;
    assert!(old_delayed_score.is_none());
    let owner_after: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(owner_after.as_deref(), Some(replacement.id.as_str()));
    let scheduler_owner_after: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(
        scheduler_owner_after.as_deref(),
        Some(replacement.id.as_str())
    );
    let scheduler_score_after: Option<f64> = conn.zscore(&scheduler_key, "scheduler-owned").await?;
    assert!(scheduler_score_after.is_some());
    assert_eq!(
        queue
            .count_repeats()
            .await
            .expect("repeat count should load after scheduler upsert"),
        1
    );

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-upsert-scheduler:cleanup-final:done");
    Ok(())
}

async fn run_repeat_retry_scheduler_metadata(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-retry-scheduler:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-retry-scheduler:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-retry-scheduler")
        .expect("valid Redis URL should build the repeat retry scheduler queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let failed_owner = queue
        .add_job(
            "scheduler-retry-old".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60)).with_key("retry-scheduler"),
            ),
        )
        .await
        .expect("old repeat owner should add");
    let claimed = queue
        .claim_next(
            "worker-repeat-retry-scheduler-old".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("old repeat owner claim should return")
        .expect("old repeat owner should claim");
    assert_eq!(claimed.id, failed_owner.id);
    queue
        .fail_job(
            &claimed.id,
            lock_token(&claimed),
            "terminal old repeat failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("old repeat owner should fail terminally");

    let current_owner = queue
        .add_job(
            "scheduler-retry-current".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60)).with_key("retry-scheduler"),
            ),
        )
        .await
        .expect("current repeat owner should add after terminal failure");
    assert_ne!(current_owner.id, failed_owner.id);
    let owner_key = format!("{namespace}:repeat-retry-scheduler:repeat:retry-scheduler");
    let scheduler_meta_key =
        format!("{namespace}:repeat-retry-scheduler:repeat_meta:retry-scheduler");
    let scheduler_owner_id: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(
        scheduler_owner_id.as_deref(),
        Some(current_owner.id.as_str())
    );
    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);

    let retry_error = queue
        .retry_job(&failed_owner.id, Utc::now())
        .await
        .expect_err("retry should reject scheduler metadata repeat owner");
    assert!(matches!(retry_error, LaneError::JobStateConflict(_)));
    trace_stage("repeat-retry-scheduler:rejected");

    let restored_owner: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(restored_owner.as_deref(), Some(current_owner.id.as_str()));
    let failed_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-retry-scheduler:failed"),
            &failed_owner.id,
        )
        .await?;
    assert!(failed_score.is_some());
    let current_hash: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-retry-scheduler:jobs"),
            &current_owner.id,
        )
        .await?;
    assert!(current_hash.is_some());
    assert_eq!(
        queue
            .count_repeats()
            .await
            .expect("repeat count should load after rejected scheduler retry"),
        1
    );

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-retry-scheduler:cleanup-final:done");
    Ok(())
}

async fn run_repeat_release_scheduler_metadata(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-release-scheduler:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-release-scheduler:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-release-scheduler")
        .expect("valid Redis URL should build the repeat release scheduler queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let complete_owner = queue
        .add_job(
            "scheduler-release-complete".to_string(),
            serde_json::json!({ "path": "complete" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .with_limit(1)
                    .with_key("release-complete"),
            ),
        )
        .await
        .expect("complete repeat owner should add");
    let complete_owner_key =
        format!("{namespace}:repeat-release-scheduler:repeat:release-complete");
    let complete_scheduler_meta_key =
        format!("{namespace}:repeat-release-scheduler:repeat_meta:release-complete");
    let complete_scheduler_key = format!("{namespace}:repeat-release-scheduler:repeat");
    let complete_scheduler_owner_id: Option<String> =
        conn.hget(&complete_scheduler_meta_key, "jid").await?;
    assert_eq!(
        complete_scheduler_owner_id.as_deref(),
        Some(complete_owner.id.as_str())
    );
    let removed_complete_owner_key: usize = conn.del(&complete_owner_key).await?;
    assert_eq!(removed_complete_owner_key, 1);

    let complete_claim = queue
        .claim_next(
            "worker-repeat-release-complete".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("complete repeat owner claim should return")
        .expect("complete repeat owner should claim");
    assert_eq!(complete_claim.id, complete_owner.id);
    queue
        .complete_job(
            &complete_claim.id,
            lock_token(&complete_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("complete repeat owner should complete");
    trace_stage("repeat-release-scheduler:completed");

    let complete_owner_after: Option<String> = conn.get(&complete_owner_key).await?;
    assert!(complete_owner_after.is_none());
    let complete_scheduler_owner_after: Option<String> =
        conn.hget(&complete_scheduler_meta_key, "jid").await?;
    assert!(complete_scheduler_owner_after.is_none());
    let complete_scheduler_score_after: Option<f64> = conn
        .zscore(&complete_scheduler_key, "release-complete")
        .await?;
    assert!(complete_scheduler_score_after.is_none());

    let fail_owner = queue
        .add_job(
            "scheduler-release-fail".to_string(),
            serde_json::json!({ "path": "fail" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60)).with_key("release-fail"),
            ),
        )
        .await
        .expect("failed repeat owner should add");
    let fail_owner_key = format!("{namespace}:repeat-release-scheduler:repeat:release-fail");
    let fail_scheduler_meta_key =
        format!("{namespace}:repeat-release-scheduler:repeat_meta:release-fail");
    let fail_scheduler_owner_id: Option<String> =
        conn.hget(&fail_scheduler_meta_key, "jid").await?;
    assert_eq!(
        fail_scheduler_owner_id.as_deref(),
        Some(fail_owner.id.as_str())
    );
    let removed_fail_owner_key: usize = conn.del(&fail_owner_key).await?;
    assert_eq!(removed_fail_owner_key, 1);

    let fail_claim = queue
        .claim_next(
            "worker-repeat-release-fail".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("failed repeat owner claim should return")
        .expect("failed repeat owner should claim");
    assert_eq!(fail_claim.id, fail_owner.id);
    queue
        .fail_job(
            &fail_claim.id,
            lock_token(&fail_claim),
            "terminal repeat release failure".to_string(),
            Utc::now(),
        )
        .await
        .expect("failed repeat owner should fail terminally");
    trace_stage("repeat-release-scheduler:failed");

    let fail_owner_after: Option<String> = conn.get(&fail_owner_key).await?;
    assert!(fail_owner_after.is_none());
    let fail_scheduler_owner_after: Option<String> =
        conn.hget(&fail_scheduler_meta_key, "jid").await?;
    assert!(fail_scheduler_owner_after.is_none());
    let fail_scheduler_score_after: Option<f64> =
        conn.zscore(&complete_scheduler_key, "release-fail").await?;
    assert!(fail_scheduler_score_after.is_none());
    assert_eq!(
        queue
            .count_repeats()
            .await
            .expect("repeat count should load after release recovery"),
        0
    );

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-release-scheduler:cleanup-final:done");
    Ok(())
}

async fn run_repeat_stalled_scheduler_metadata(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-stalled-scheduler:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-stalled-scheduler:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-stalled-scheduler")
        .expect("valid Redis URL should build the repeat stalled scheduler queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let stalled_owner = queue
        .add_job(
            "scheduler-stalled-owner".to_string(),
            serde_json::json!({ "path": "stalled" }),
            JobOptions::new().with_max_stalled_count(0).with_repeat(
                RepeatOptions::every(Duration::from_secs(60)).with_key("stalled-scheduler"),
            ),
        )
        .await
        .expect("stalled repeat owner should add");
    let owner_key = format!("{namespace}:repeat-stalled-scheduler:repeat:stalled-scheduler");
    let scheduler_meta_key =
        format!("{namespace}:repeat-stalled-scheduler:repeat_meta:stalled-scheduler");
    let scheduler_owner_id: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(
        scheduler_owner_id.as_deref(),
        Some(stalled_owner.id.as_str())
    );

    let claimed = queue
        .claim_next(
            "worker-repeat-stalled-scheduler".to_string(),
            Duration::from_millis(50),
            Utc::now(),
        )
        .await
        .expect("stalled repeat owner claim should return")
        .expect("stalled repeat owner should claim");
    assert_eq!(claimed.id, stalled_owner.id);
    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);
    let owner_after_delete: Option<String> = conn.get(&owner_key).await?;
    assert!(owner_after_delete.is_none());

    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("repeat stalled scheduler recovery should mark candidates"),
        0
    );
    assert_eq!(
        queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("repeat stalled scheduler recovery should requeue"),
        1
    );
    trace_stage("repeat-stalled-scheduler:requeued");

    let recovered = queue
        .get_job(&stalled_owner.id)
        .await
        .expect("stalled repeat owner lookup should return")
        .expect("stalled repeat owner should still exist");
    assert_eq!(recovered.state, JobState::Waiting);
    assert_eq!(recovered.stalled_count, 1);
    let failed_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-stalled-scheduler:failed"),
            &stalled_owner.id,
        )
        .await?;
    assert!(failed_score.is_none());
    let waiting_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-stalled-scheduler:waiting"),
            &stalled_owner.id,
        )
        .await?;
    assert!(waiting_score.is_some());
    let restored_owner: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(restored_owner.as_deref(), Some(stalled_owner.id.as_str()));
    let scheduler_owner_after: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(
        scheduler_owner_after.as_deref(),
        Some(stalled_owner.id.as_str())
    );
    assert_eq!(
        queue
            .count_repeats()
            .await
            .expect("repeat count should load after stalled recovery"),
        1
    );

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-stalled-scheduler:cleanup-final:done");
    Ok(())
}

async fn run_repeat_drain_scheduler_metadata(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-drain-scheduler:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-drain-scheduler:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-drain-scheduler")
        .expect("valid Redis URL should build the repeat drain scheduler queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let repeat_owner = queue
        .add_job(
            "scheduler-drain-owner".to_string(),
            serde_json::json!({ "path": "drain" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .with_limit(2)
                    .with_key("drain-scheduler"),
            ),
        )
        .await
        .expect("repeat drain owner should add");
    let claimed = queue
        .claim_next(
            "worker-repeat-drain-scheduler".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat drain owner claim should return")
        .expect("repeat drain owner should claim");
    assert_eq!(claimed.id, repeat_owner.id);
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("repeat drain owner should complete");

    let repeat_successor = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .expect("repeat drain delayed jobs should list")
        .jobs
        .into_iter()
        .find(|job| job.repeat_key.as_deref() == Some("drain-scheduler"))
        .expect("repeat drain successor should be delayed");
    let owner_key = format!("{namespace}:repeat-drain-scheduler:repeat:drain-scheduler");
    let scheduler_meta_key =
        format!("{namespace}:repeat-drain-scheduler:repeat_meta:drain-scheduler");
    let scheduler_owner_id: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(
        scheduler_owner_id.as_deref(),
        Some(repeat_successor.id.as_str())
    );
    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);

    let ordinary_delayed = queue
        .add_job(
            "ordinary-delayed-drain".to_string(),
            serde_json::json!({ "path": "ordinary" }),
            JobOptions::new().with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("ordinary delayed job should add");
    let drained = queue
        .drain_jobs(true)
        .await
        .expect("drain should preserve scheduler metadata repeat owner");
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].id, ordinary_delayed.id);
    trace_stage("repeat-drain-scheduler:drained");

    let repeat_successor_after = queue
        .get_job(&repeat_successor.id)
        .await
        .expect("repeat drain successor lookup should return")
        .expect("repeat drain successor should remain");
    assert_eq!(repeat_successor_after.state, JobState::Delayed);
    let repeat_delayed_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-drain-scheduler:delayed"),
            &repeat_successor.id,
        )
        .await?;
    assert!(repeat_delayed_score.is_some());
    let ordinary_hash: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-drain-scheduler:jobs"),
            &ordinary_delayed.id,
        )
        .await?;
    assert!(ordinary_hash.is_none());
    let restored_owner: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(
        restored_owner.as_deref(),
        Some(repeat_successor.id.as_str())
    );
    let scheduler_owner_after: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(
        scheduler_owner_after.as_deref(),
        Some(repeat_successor.id.as_str())
    );
    assert_eq!(
        queue
            .count_repeats()
            .await
            .expect("repeat count should load after drain recovery"),
        1
    );

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-drain-scheduler:cleanup-final:done");
    Ok(())
}

async fn run_repeat_clean_scheduler_metadata(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-clean-scheduler:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-clean-scheduler:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-clean-scheduler")
        .expect("valid Redis URL should build the repeat clean scheduler queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let repeat_owner = queue
        .add_job(
            "scheduler-clean-owner".to_string(),
            serde_json::json!({ "path": "clean" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .with_limit(2)
                    .with_key("clean-scheduler"),
            ),
        )
        .await
        .expect("repeat clean owner should add");
    let claimed = queue
        .claim_next(
            "worker-repeat-clean-scheduler".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat clean owner claim should return")
        .expect("repeat clean owner should claim");
    assert_eq!(claimed.id, repeat_owner.id);
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("repeat clean owner should complete");

    let repeat_successor = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .expect("repeat clean delayed jobs should list")
        .jobs
        .into_iter()
        .find(|job| job.repeat_key.as_deref() == Some("clean-scheduler"))
        .expect("repeat clean successor should be delayed");
    let owner_key = format!("{namespace}:repeat-clean-scheduler:repeat:clean-scheduler");
    let scheduler_meta_key =
        format!("{namespace}:repeat-clean-scheduler:repeat_meta:clean-scheduler");
    let scheduler_owner_id: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(
        scheduler_owner_id.as_deref(),
        Some(repeat_successor.id.as_str())
    );
    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);

    let ordinary_delayed = queue
        .add_job(
            "ordinary-delayed-clean".to_string(),
            serde_json::json!({ "path": "ordinary" }),
            JobOptions::new().with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("ordinary delayed job should add");
    let clean_now = Utc::now() + chrono::Duration::seconds(120);
    let cleaned = queue
        .clean_jobs(JobState::Delayed, Duration::ZERO, 10, clean_now)
        .await
        .expect("clean should preserve scheduler metadata repeat owner");
    assert_eq!(cleaned.len(), 1);
    assert_eq!(cleaned[0].id, ordinary_delayed.id);
    trace_stage("repeat-clean-scheduler:cleaned");

    let repeat_successor_after = queue
        .get_job(&repeat_successor.id)
        .await
        .expect("repeat clean successor lookup should return")
        .expect("repeat clean successor should remain");
    assert_eq!(repeat_successor_after.state, JobState::Delayed);
    let repeat_delayed_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-clean-scheduler:delayed"),
            &repeat_successor.id,
        )
        .await?;
    assert!(repeat_delayed_score.is_some());
    let ordinary_hash: Option<String> = conn
        .hget(
            format!("{namespace}:repeat-clean-scheduler:jobs"),
            &ordinary_delayed.id,
        )
        .await?;
    assert!(ordinary_hash.is_none());
    let restored_owner: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(
        restored_owner.as_deref(),
        Some(repeat_successor.id.as_str())
    );
    let scheduler_owner_after: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(
        scheduler_owner_after.as_deref(),
        Some(repeat_successor.id.as_str())
    );
    assert_eq!(
        queue
            .count_repeats()
            .await
            .expect("repeat count should load after clean recovery"),
        1
    );

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-clean-scheduler:cleanup-final:done");
    Ok(())
}

async fn run_repeat_nonterminal_scheduler_metadata(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-metadata-moves:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-metadata-moves:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-metadata-moves")
        .expect("valid Redis URL should build the repeat metadata move queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let owner_key = format!("{namespace}:repeat-metadata-moves:repeat:metadata-moves");
    let scheduler_key = format!("{namespace}:repeat-metadata-moves:repeat");
    let scheduler_meta_key =
        format!("{namespace}:repeat-metadata-moves:repeat_meta:metadata-moves");

    let added = queue
        .add_job(
            "scheduler-metadata-owner".to_string(),
            serde_json::json!({ "path": "metadata" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .with_limit(3)
                    .with_key("metadata-moves"),
            ),
        )
        .await
        .expect("repeat metadata owner should add");
    assert_repeat_scheduler_metadata(
        &mut conn,
        &namespace,
        "repeat-metadata-moves",
        "metadata-moves",
        &added,
        "waiting",
    )
    .await?;

    let removed_owner: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner, 1);
    let removed_scheduler_hash: usize = conn.del(&scheduler_meta_key).await?;
    assert_eq!(removed_scheduler_hash, 1);
    let removed_scheduler_score: usize = conn.zrem(&scheduler_key, "metadata-moves").await?;
    assert_eq!(removed_scheduler_score, 1);
    let first_claim = queue
        .claim_next(
            "worker-repeat-metadata-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat metadata claim should return")
        .expect("repeat metadata owner should claim");
    assert_repeat_scheduler_metadata(
        &mut conn,
        &namespace,
        "repeat-metadata-moves",
        "metadata-moves",
        &first_claim,
        "active",
    )
    .await?;

    let removed_owner: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner, 1);
    let delayed = queue
        .delay_active_job(
            &first_claim.id,
            lock_token(&first_claim),
            Duration::from_secs(60),
            Utc::now(),
        )
        .await
        .expect("repeat metadata active owner should delay");
    assert_repeat_scheduler_metadata(
        &mut conn,
        &namespace,
        "repeat-metadata-moves",
        "metadata-moves",
        &delayed,
        "delayed",
    )
    .await?;

    let removed_owner: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner, 1);
    let promoted = queue
        .promote_job(&delayed.id, Utc::now())
        .await
        .expect("repeat metadata delayed owner should promote");
    assert_repeat_scheduler_metadata(
        &mut conn,
        &namespace,
        "repeat-metadata-moves",
        "metadata-moves",
        &promoted,
        "waiting",
    )
    .await?;

    let second_claim = queue
        .claim_next(
            "worker-repeat-metadata-b".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat metadata second claim should return")
        .expect("repeat metadata owner should claim again");
    assert_repeat_scheduler_metadata(
        &mut conn,
        &namespace,
        "repeat-metadata-moves",
        "metadata-moves",
        &second_claim,
        "active",
    )
    .await?;

    let removed_owner: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner, 1);
    let released = queue
        .release_active_job(&second_claim.id, lock_token(&second_claim), Utc::now())
        .await
        .expect("repeat metadata active owner should release");
    assert_repeat_scheduler_metadata(
        &mut conn,
        &namespace,
        "repeat-metadata-moves",
        "metadata-moves",
        &released,
        "waiting",
    )
    .await?;

    let third_claim = queue
        .claim_next(
            "worker-repeat-metadata-c".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("repeat metadata third claim should return")
        .expect("repeat metadata owner should claim after release");
    let delayed_again = queue
        .delay_active_job(
            &third_claim.id,
            lock_token(&third_claim),
            Duration::from_secs(60),
            Utc::now(),
        )
        .await
        .expect("repeat metadata owner should delay again");
    assert_repeat_scheduler_metadata(
        &mut conn,
        &namespace,
        "repeat-metadata-moves",
        "metadata-moves",
        &delayed_again,
        "delayed",
    )
    .await?;

    let removed_owner: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner, 1);
    let rescheduled = queue
        .reschedule_job(&delayed_again.id, Duration::from_secs(120), Utc::now())
        .await
        .expect("repeat metadata delayed owner should reschedule");
    assert_repeat_scheduler_metadata(
        &mut conn,
        &namespace,
        "repeat-metadata-moves",
        "metadata-moves",
        &rescheduled,
        "delayed",
    )
    .await?;

    let removed_owner: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner, 1);
    let promoted_due = queue
        .promote_due_jobs(rescheduled.scheduled_at + chrono::Duration::seconds(1))
        .await
        .expect("repeat metadata due owner should promote");
    assert_eq!(promoted_due, 1);
    let promoted_due_job = queue
        .get_job(&rescheduled.id)
        .await
        .expect("repeat metadata due-promoted owner lookup should return")
        .expect("repeat metadata due-promoted owner should remain");
    assert_eq!(promoted_due_job.state, JobState::Waiting);
    assert_repeat_scheduler_metadata(
        &mut conn,
        &namespace,
        "repeat-metadata-moves",
        "metadata-moves",
        &promoted_due_job,
        "waiting",
    )
    .await?;

    let conflicting = queue
        .add_job(
            "scheduler-metadata-conflict".to_string(),
            serde_json::json!({ "path": "conflict" }),
            JobOptions::new().with_delay(Duration::from_secs(60)),
        )
        .await
        .expect("repeat metadata conflict job should add");
    let jobs_key = format!("{namespace}:repeat-metadata-moves:jobs");
    let conflicting_raw: String = conn.hget(&jobs_key, &conflicting.id).await?;
    let mut conflicting_json: serde_json::Value =
        serde_json::from_str(&conflicting_raw).expect("conflict job JSON should decode");
    conflicting_json["repeat_key"] = serde_json::Value::String("metadata-moves".to_string());
    let conflicting_raw =
        serde_json::to_string(&conflicting_json).expect("conflict job JSON should encode");
    let _: usize = conn
        .hset(&jobs_key, &conflicting.id, conflicting_raw)
        .await?;

    let removed_owner: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner, 1);
    let moved_conflict = queue
        .promote_job(&conflicting.id, Utc::now())
        .await
        .expect("conflicting repeat-keyed stale job should still move");
    assert_eq!(moved_conflict.state, JobState::Waiting);
    assert_eq!(moved_conflict.repeat_key.as_deref(), Some("metadata-moves"));
    let scheduler_owner_after_conflict: Option<String> =
        conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(
        scheduler_owner_after_conflict.as_deref(),
        Some(promoted_due_job.id.as_str())
    );
    let owner_after_conflict: Option<String> = conn.get(&owner_key).await?;
    assert!(owner_after_conflict.is_none());
    let repeats_after_conflict = queue
        .list_repeats()
        .await
        .expect("repeat metadata conflict list should repair owner key");
    assert!(repeats_after_conflict
        .iter()
        .any(|entry| { entry.key == "metadata-moves" && entry.job_id == promoted_due_job.id }));
    let restored_owner_after_list: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(
        restored_owner_after_list.as_deref(),
        Some(promoted_due_job.id.as_str())
    );

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-metadata-moves:cleanup-final:done");
    Ok(())
}

async fn assert_repeat_scheduler_metadata(
    conn: &mut redis::aio::ConnectionManager,
    namespace: &str,
    queue: &str,
    repeat_key: &str,
    job: &Job,
    expected_state: &str,
) -> redis::RedisResult<()> {
    let owner_key = format!("{namespace}:{queue}:repeat:{repeat_key}");
    let scheduler_key = format!("{namespace}:{queue}:repeat");
    let scheduler_meta_key = format!("{namespace}:{queue}:repeat_meta:{repeat_key}");
    let expected_next = job.scheduled_at.timestamp_millis();
    let repeat_options = job
        .options
        .repeat
        .as_ref()
        .expect("repeat metadata test jobs should carry repeat options");

    let owner_id: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(owner_id.as_deref(), Some(job.id.as_str()));
    let meta_owner_id: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(meta_owner_id.as_deref(), Some(job.id.as_str()));
    let meta_state: Option<String> = conn.hget(&scheduler_meta_key, "state").await?;
    assert_eq!(meta_state.as_deref(), Some(expected_state));
    let meta_next: Option<String> = conn.hget(&scheduler_meta_key, "next").await?;
    let meta_next = meta_next
        .as_deref()
        .and_then(|value| value.parse::<f64>().ok());
    assert_eq!(meta_next, Some(expected_next as f64));
    let scheduler_score: Option<f64> = conn.zscore(&scheduler_key, repeat_key).await?;
    assert_eq!(scheduler_score, Some(expected_next as f64));
    let meta_opts: Option<String> = conn.hget(&scheduler_meta_key, "opts").await?;
    assert!(meta_opts.as_deref().is_some_and(|opts| !opts.is_empty()));
    let meta_limit: Option<u32> = conn.hget(&scheduler_meta_key, "limit").await?;
    assert_eq!(meta_limit, repeat_options.limit);
    let meta_end_at: Option<String> = conn.hget(&scheduler_meta_key, "endDate").await?;
    assert_eq!(
        meta_end_at.as_deref(),
        repeat_options
            .end_at
            .as_ref()
            .map(DateTime::to_rfc3339)
            .as_deref()
    );
    let meta_every: Option<u64> = conn.hget(&scheduler_meta_key, "every").await?;
    assert_eq!(
        meta_every,
        repeat_options
            .interval()
            .and_then(|interval| u64::try_from(interval.as_millis()).ok())
    );
    let meta_pattern: Option<String> = conn.hget(&scheduler_meta_key, "pattern").await?;
    assert_eq!(meta_pattern.as_deref(), repeat_options.cron_expression());
    Ok(())
}

async fn run_repeat_remove_scheduler_metadata(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-remove-scheduler:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-remove-scheduler:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-remove-scheduler")
        .expect("valid Redis URL should build the repeat remove scheduler queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let job = queue
        .add_job(
            "orphanable-repeat".to_string(),
            serde_json::json!({ "kind": "scheduler-meta-owner" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60)).with_key("orphanable-repeat"),
            ),
        )
        .await
        .expect("repeat owner should add");
    trace_stage("repeat-remove-scheduler:added");

    let owner_key = format!("{namespace}:repeat-remove-scheduler:repeat:orphanable-repeat");
    let scheduler_key = format!("{namespace}:repeat-remove-scheduler:repeat");
    let scheduler_meta_key =
        format!("{namespace}:repeat-remove-scheduler:repeat_meta:orphanable-repeat");
    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);
    let scheduler_owner_id: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(scheduler_owner_id.as_deref(), Some(job.id.as_str()));

    let removed = queue
        .remove_repeat("orphanable-repeat")
        .await
        .expect("repeat removal should use scheduler metadata")
        .expect("repeat removal should return the scheduler-owned job");
    trace_stage("repeat-remove-scheduler:removed");
    assert_eq!(removed.id, job.id);

    let removed_hash: Option<String> = conn
        .hget(format!("{namespace}:repeat-remove-scheduler:jobs"), &job.id)
        .await?;
    assert!(removed_hash.is_none());
    let waiting_score: Option<f64> = conn
        .zscore(
            format!("{namespace}:repeat-remove-scheduler:waiting"),
            &job.id,
        )
        .await?;
    assert!(waiting_score.is_none());
    let scheduler_score: Option<f64> = conn.zscore(&scheduler_key, "orphanable-repeat").await?;
    assert!(scheduler_score.is_none());
    let scheduler_meta_owner_after: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert!(scheduler_meta_owner_after.is_none());
    let owner_after: Option<String> = conn.get(&owner_key).await?;
    assert!(owner_after.is_none());
    assert_eq!(
        queue
            .count_repeats()
            .await
            .expect("repeat count should load after scheduler removal"),
        0
    );

    let _: usize = conn
        .zadd(&scheduler_key, "missing-orphanable-repeat", 1_i64)
        .await?;
    let missing_scheduler_meta_key =
        format!("{namespace}:repeat-remove-scheduler:repeat_meta:missing-orphanable-repeat");
    let _: usize = conn
        .hset(
            &missing_scheduler_meta_key,
            "jid",
            "missing-scheduler-owner",
        )
        .await?;
    assert!(queue
        .remove_repeat("missing-orphanable-repeat")
        .await
        .expect("stale scheduler metadata removal should return")
        .is_none());
    let stale_scheduler_score: Option<f64> = conn
        .zscore(&scheduler_key, "missing-orphanable-repeat")
        .await?;
    assert!(stale_scheduler_score.is_none());
    let stale_scheduler_owner: Option<String> =
        conn.hget(&missing_scheduler_meta_key, "jid").await?;
    assert!(stale_scheduler_owner.is_none());

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-remove-scheduler:cleanup-final:done");
    Ok(())
}

async fn run_repeat_read_scheduler_metadata(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    trace_stage("repeat-read-scheduler:cleanup:start");
    cleanup_namespace(&redis_url, &namespace).await?;
    trace_stage("repeat-read-scheduler:cleanup:done");

    let queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-read-scheduler")
        .expect("valid Redis URL should build the repeat read scheduler queue");
    let mut conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let job = queue
        .add_job(
            "recoverable-repeat".to_string(),
            serde_json::json!({ "kind": "scheduler-meta-reader" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60)).with_key("recoverable-repeat"),
            ),
        )
        .await
        .expect("repeat owner should add");
    trace_stage("repeat-read-scheduler:added");

    let owner_key = format!("{namespace}:repeat-read-scheduler:repeat:recoverable-repeat");
    let scheduler_meta_key =
        format!("{namespace}:repeat-read-scheduler:repeat_meta:recoverable-repeat");
    let scheduler_owner_id: Option<String> = conn.hget(&scheduler_meta_key, "jid").await?;
    assert_eq!(scheduler_owner_id.as_deref(), Some(job.id.as_str()));

    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);
    let entries = queue
        .list_repeats()
        .await
        .expect("repeat list should recover scheduler metadata owner");
    assert!(entries
        .iter()
        .any(|entry| entry.key == "recoverable-repeat" && entry.job_id == job.id));
    let restored_owner_after_list: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(restored_owner_after_list.as_deref(), Some(job.id.as_str()));

    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);
    let entry = queue
        .get_repeat("recoverable-repeat")
        .await
        .expect("repeat getter should recover scheduler metadata owner")
        .expect("repeat getter should return scheduler metadata owner");
    assert_eq!(entry.job_id, job.id);
    let restored_owner_after_get: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(restored_owner_after_get.as_deref(), Some(job.id.as_str()));

    let removed_owner_key: usize = conn.del(&owner_key).await?;
    assert_eq!(removed_owner_key, 1);
    assert_eq!(
        queue
            .count_repeats()
            .await
            .expect("repeat count should recover scheduler metadata owner"),
        1
    );
    let restored_owner_after_count: Option<String> = conn.get(&owner_key).await?;
    assert_eq!(restored_owner_after_count.as_deref(), Some(job.id.as_str()));

    cleanup_namespace_with_conn(&mut conn, &namespace).await?;
    trace_stage("repeat-read-scheduler:cleanup-final:done");
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
    trace_stage("dedup:queue-created");
    let first_dedup = dedup_queue
        .add_job(
            "dedup-sync".to_string(),
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication_id("tenant:42"),
        )
        .await
        .expect("dedup job should be added");
    trace_stage("dedup:first-added");
    let duplicate_dedup = dedup_queue
        .add_job(
            "dedup-sync-duplicate".to_string(),
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication_id("tenant:42"),
        )
        .await
        .expect("duplicate dedup job should return existing job");
    trace_stage("dedup:duplicate-added");
    assert_eq!(duplicate_dedup, first_dedup);
    let mut dedup_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    trace_stage("dedup:conn-created");
    let dedup_owner: Option<String> = dedup_conn
        .get(format!("{namespace}:dedup:deduplication:tenant:42"))
        .await?;
    assert_eq!(dedup_owner.as_deref(), Some(first_dedup.id.as_str()));
    trace_stage("dedup:owner-read");

    let first_dedup_claim = dedup_queue
        .claim_next(
            "worker-dedup".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("dedup claim should return")
        .expect("dedup job should be claimable");
    trace_stage("dedup:first-claimed");
    dedup_queue
        .complete_job(
            &first_dedup_claim.id,
            lock_token(&first_dedup_claim),
            serde_json::json!({ "ok": true }),
            Utc::now(),
        )
        .await
        .expect("dedup job should complete");
    trace_stage("dedup:first-completed");
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
                DeduplicationOptions::new("tenant:fail").with_ttl(Duration::from_secs(30)),
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
        .expect("terminal failure should keep TTL dedup key expiring");
    let failed_dedup_owner: Option<String> = dedup_conn
        .get(format!("{namespace}:dedup:deduplication:tenant:fail"))
        .await?;
    assert_eq!(failed_dedup_owner.as_deref(), Some(fail_dedup.id.as_str()));
    let failed_dedup_ttl: i64 = redis::cmd("PTTL")
        .arg(format!("{namespace}:dedup:deduplication:tenant:fail"))
        .query_async(&mut dedup_conn)
        .await?;
    assert!(
        failed_dedup_ttl > 0,
        "failed TTL dedup key should keep expiring after terminal failure, got {failed_dedup_ttl}"
    );
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
                DeduplicationOptions::new("tenant:fail").with_ttl(Duration::from_secs(30)),
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
                DeduplicationOptions::new("tenant:ttl").with_ttl(Duration::from_secs(30)),
            ),
        )
        .await
        .expect("ttl dedup job should be added");
    let ttl_duplicate = dedup_queue
        .add_job(
            "dedup-ttl-duplicate".to_string(),
            serde_json::json!({ "version": 7 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:ttl").with_ttl(Duration::from_secs(30)),
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
    let ttl_shortened: bool = redis::cmd("PEXPIRE")
        .arg(&ttl_dedup_key)
        .arg(1_u16)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(ttl_shortened);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let ttl_after_expiration = dedup_queue
        .add_job(
            "dedup-ttl-after-expiration".to_string(),
            serde_json::json!({ "version": 8 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:ttl").with_ttl(Duration::from_secs(5)),
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
                    .with_ttl(Duration::from_secs(10))
                    .extend_ttl(true),
            ),
        )
        .await
        .expect("extend dedup job should be added");
    let extend_ttl_shortened: bool = redis::cmd("PEXPIRE")
        .arg(&extend_dedup_key)
        .arg(5_000)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(extend_ttl_shortened);
    let extend_duplicate = dedup_queue
        .add_job(
            "dedup-extend-duplicate".to_string(),
            serde_json::json!({ "version": 10 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:extend")
                    .with_ttl(Duration::from_secs(10))
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
    assert!(extend_ttl_after_duplicate > 7_000);
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
                        .with_ttl(Duration::from_secs(30))
                        .replace_delayed(true),
                ),
        )
        .await
        .expect("replace ttl old dedup job should be added");
    let ttl_overridden: bool = redis::cmd("PEXPIRE")
        .arg(&replace_ttl_key)
        .arg(10_000)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(ttl_overridden);
    let replace_ttl_before: i64 = redis::cmd("PTTL")
        .arg(&replace_ttl_key)
        .query_async(&mut dedup_conn)
        .await?;
    assert!(replace_ttl_before > 0 && replace_ttl_before <= 10_000);
    let replace_ttl_new = dedup_queue
        .add_job(
            "dedup-replace-ttl-new".to_string(),
            serde_json::json!({ "version": 14 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(60))
                .with_deduplication(
                    DeduplicationOptions::new("tenant:replace-ttl")
                        .with_ttl(Duration::from_secs(30))
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
        replace_ttl_after <= replace_ttl_before + 100,
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
    assert!(promoted_keep_last >= 1);
    let mut keep_last_next_claim = None;
    for _ in 0..promoted_keep_last {
        let claimed = dedup_queue
            .claim_next(
                "worker-keep-last-next".to_string(),
                Duration::from_secs(30),
                Utc::now(),
            )
            .await
            .expect("keep-last next claim should return")
            .expect("promoted keep-last batch should have a claimable job");
        if claimed.id == keep_last_next.id {
            keep_last_next_claim = Some(claimed);
            break;
        }
        dedup_queue
            .complete_job(
                &claimed.id,
                lock_token(&claimed),
                serde_json::json!({ "drained": true }),
                Utc::now(),
            )
            .await
            .expect("non-target promoted job should drain");
    }
    let keep_last_next_claim =
        keep_last_next_claim.expect("keep-last materialized job should be promoted");
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
        .expect("terminal job with stale waiting index should update priority");
    assert_eq!(terminal_priority_update.state, JobState::Completed);
    assert_eq!(terminal_priority_update.priority, 5);
    assert_eq!(terminal_priority_update.options.priority, 5);
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
        .with_claim_rate_limit(JobRateLimit::new(1, Duration::from_secs(2)))
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
    tokio::time::sleep(Duration::from_millis(2_100)).await;
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
        .set_claim_rate_limit(JobRateLimit::new(1, Duration::from_secs(5)))
        .await
        .expect("global rate limit should be configured");
    let global_rate_meta_key = format!("{namespace}:global-rate:meta");
    let mut global_rate_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let stored_global_rate: (Option<u64>, Option<u64>) = global_rate_conn
        .hmget(&global_rate_meta_key, &["max", "duration"])
        .await?;
    assert_eq!(stored_global_rate, (Some(1), Some(5_000)));
    assert_eq!(
        global_rate_worker
            .get_claim_rate_limit()
            .await
            .expect("stored global rate limit should load"),
        Some(JobRateLimit::new(1, Duration::from_secs(5)))
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
        (1..=5_000).contains(&global_rate_ttl),
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
        .set_claim_rate_limit(JobRateLimit::new(1, Duration::from_secs(5)))
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
        .rate_limit_claims_for(Duration::from_secs(5))
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
        (1..=5_000).contains(&manual_rate_ttl),
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
                .with_delay(Duration::from_secs(10)),
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
    tokio::time::sleep(Duration::from_millis(10_100)).await;
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
    let paused_marker_key = format!("{namespace}:paused-promote:marker");
    let paused_base_marker: Option<f64> =
        paused_promote_conn.zscore(&paused_marker_key, "0").await?;
    assert!(
        paused_base_marker.is_none(),
        "claim-time delayed promotion should not wake workers with a base marker while paused"
    );
    let paused_delay_marker: Option<f64> =
        paused_promote_conn.zscore(&paused_marker_key, "1").await?;
    assert!(
        paused_delay_marker.is_none(),
        "claim-time delayed promotion should clear the consumed delay marker"
    );
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
    let stale_promote = single_promote_queue
        .promote_job(&single_promoted.id, Utc::now())
        .await
        .expect_err("completed job with stale delayed index should reject promote");
    assert!(matches!(stale_promote, LaneError::JobStateConflict(_)));
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
    trace_stage("main-lifecycle:pause-resume:done");

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
        .expect("terminal retained jobs should allow progress updates");
    assert_eq!(terminal_progress.state, JobState::Completed);
    assert_eq!(
        terminal_progress.progress,
        Some(serde_json::json!({ "percent": 100 }))
    );
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
    trace_stage("main-lifecycle:complete-fail:done");

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
        .reschedule_job(&delayed.id, Duration::from_secs(2), Utc::now())
        .await
        .expect("delayed job should reschedule");
    assert_eq!(rescheduled_delayed.id, delayed.id);
    assert_eq!(rescheduled_delayed.state, JobState::Delayed);
    assert_eq!(
        rescheduled_delayed.options.delay,
        Some(Duration::from_secs(2))
    );
    assert!(worker
        .claim_next("worker-d".to_string(), Duration::from_secs(30), Utc::now())
        .await
        .expect("early delayed claim should return")
        .is_none());

    tokio::time::sleep(Duration::from_millis(2_100)).await;
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
    trace_stage("main-lifecycle:delayed-claimed");
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

    let unlocked_active = producer
        .add_job(
            "remove-unlocked-active".to_string(),
            serde_json::json!({ "kind": "lost-lock" }),
            JobOptions::new().with_priority(4),
        )
        .await
        .expect("unlocked active remove job should add");
    let unlocked_active_claim = worker
        .claim_next(
            "worker-remove-unlocked-active".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("unlocked active remove claim should return")
        .expect("unlocked active remove job should claim");
    assert_eq!(unlocked_active_claim.id, unlocked_active.id);
    producer
        .add_log(
            &unlocked_active.id,
            "active removal log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("unlocked active removal log should append");
    let unlocked_active_lock_key = format!("{namespace}:jobs:locks:{}", unlocked_active.id);
    let unlocked_active_logs_key = format!("{namespace}:jobs:logs:{}", unlocked_active.id);
    let jobs_stalled_key = format!("{namespace}:jobs:stalled");
    let removed_unlocked_active_lock: usize =
        remove_index_conn.del(&unlocked_active_lock_key).await?;
    assert_eq!(removed_unlocked_active_lock, 1);
    let stalled_unlocked_active: usize = remove_index_conn
        .sadd(&jobs_stalled_key, &unlocked_active.id)
        .await?;
    assert_eq!(stalled_unlocked_active, 1);
    let removed_unlocked_active = producer
        .remove_job(&unlocked_active.id)
        .await
        .expect("unlocked active job should remove")
        .expect("unlocked active job should be returned");
    assert_eq!(removed_unlocked_active.id, unlocked_active.id);
    assert_eq!(removed_unlocked_active.state, JobState::Active);
    assert!(producer
        .get_job(&unlocked_active.id)
        .await
        .expect("unlocked active lookup should return")
        .is_none());
    let unlocked_active_score_after: Option<f64> = remove_index_conn
        .zscore(format!("{namespace}:jobs:active"), &unlocked_active.id)
        .await?;
    assert!(unlocked_active_score_after.is_none());
    let unlocked_active_logs_len: usize = remove_index_conn.llen(&unlocked_active_logs_key).await?;
    assert_eq!(unlocked_active_logs_len, 0);
    let unlocked_active_stalled_after: bool = remove_index_conn
        .sismember(&jobs_stalled_key, &unlocked_active.id)
        .await?;
    assert!(!unlocked_active_stalled_after);

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
            Duration::from_secs(10),
            Utc::now(),
        )
        .await
        .expect("active job should move back to delayed");
    assert_eq!(delayed_again.state, JobState::Delayed);
    assert_eq!(delayed_again.options.delay, Some(Duration::from_secs(10)));
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
    tokio::time::sleep(Duration::from_millis(10_100)).await;
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
    trace_stage("main-lifecycle:delay-active:done");

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
    trace_stage("main-lifecycle:release-active:done");

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
    trace_stage("main-lifecycle:remove-prune:done");

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
    trace_stage("main-lifecycle:auto-remove:done");

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
    trace_stage("main-lifecycle:stalled:done");

    let repeat_stalled_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "repeat-stalled")
            .expect("valid Redis URL should build the repeat-stalled queue");
    let mut repeat_stalled_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let repeat_stalled = repeat_stalled_queue
        .add_job(
            "repeat-stalled".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_max_stalled_count(0).with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .with_limit(2)
                    .with_key("repeat-stalled"),
            ),
        )
        .await
        .expect("repeat-stalled job should be added");
    let repeat_stalled_claim = repeat_stalled_queue
        .claim_next(
            "worker-repeat-stalled".to_string(),
            Duration::from_millis(50),
            Utc::now(),
        )
        .await
        .expect("repeat-stalled claim should return")
        .expect("repeat-stalled job should be claimable");
    assert_eq!(repeat_stalled_claim.id, repeat_stalled.id);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        repeat_stalled_queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("repeat stalled recovery should mark candidates"),
        0
    );
    assert_eq!(
        repeat_stalled_queue
            .recover_stalled_jobs(Utc::now())
            .await
            .expect("repeat stalled recovery should requeue"),
        1
    );
    let repeat_stalled_recovered = repeat_stalled_queue
        .get_job(&repeat_stalled.id)
        .await
        .expect("repeat-stalled job should load")
        .expect("repeat-stalled job should still exist");
    assert_eq!(repeat_stalled_recovered.state, JobState::Waiting);
    assert_eq!(repeat_stalled_recovered.stalled_count, 1);
    let repeat_stalled_failed_score: Option<f64> = repeat_stalled_conn
        .zscore(
            format!("{namespace}:repeat-stalled:failed"),
            &repeat_stalled.id,
        )
        .await?;
    assert!(repeat_stalled_failed_score.is_none());
    let repeat_stalled_waiting_score: Option<f64> = repeat_stalled_conn
        .zscore(
            format!("{namespace}:repeat-stalled:waiting"),
            &repeat_stalled.id,
        )
        .await?;
    assert!(repeat_stalled_waiting_score.is_some());
    let repeat_stalled_owner: Option<String> = repeat_stalled_conn
        .get(format!("{namespace}:repeat-stalled:repeat:repeat-stalled"))
        .await?;
    assert_eq!(
        repeat_stalled_owner.as_deref(),
        Some(repeat_stalled.id.as_str())
    );
    let repeat_stalled_scheduler_owner: Option<String> = repeat_stalled_conn
        .hget(
            format!("{namespace}:repeat-stalled:repeat_meta:repeat-stalled"),
            "jid",
        )
        .await?;
    assert_eq!(
        repeat_stalled_scheduler_owner.as_deref(),
        Some(repeat_stalled.id.as_str())
    );
    trace_stage("main-lifecycle:repeat-stalled:done");

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
        Some(serde_json::json!({ "percent": 100 }))
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
    let decoded_high: serde_json::Value =
        serde_json::from_str(&raw_high).expect("raw high job should decode");
    assert_eq!(
        decoded_high.get("payload"),
        Some(&serde_json::json!({ "n": 1, "stage": "archived" }))
    );
    assert_eq!(
        producer
            .get_job(&high.id)
            .await
            .expect("high job should load")
            .expect("high job should still exist")
            .payload,
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
    let decoded_high_after_keep = producer
        .get_job(&high.id)
        .await
        .expect("trimmed high job should load")
        .expect("trimmed high job should still exist");
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
    let decoded_high_after_clear = producer
        .get_job(&high.id)
        .await
        .expect("cleared high job should load")
        .expect("cleared high job should still exist");
    assert!(decoded_high_after_clear.logs.is_empty());
    trace_stage("main-lifecycle:logs:done");

    let stats = producer.stats().await.expect("stats should load");
    assert_eq!(stats.completed, 6);
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

    let clean_active_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "clean-active")
        .expect("valid Redis URL should build the clean-active queue");
    let mut clean_active_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let active_locked = clean_active_queue
        .add_job(
            "active-locked".to_string(),
            serde_json::json!({ "kind": "locked" }),
            JobOptions::new().with_priority(1),
        )
        .await
        .expect("locked active clean job should add");
    let active_unlocked = clean_active_queue
        .add_job(
            "active-unlocked".to_string(),
            serde_json::json!({ "kind": "unlocked" }),
            JobOptions::new().with_priority(2),
        )
        .await
        .expect("unlocked active clean job should add");
    let active_locked_claim = clean_active_queue
        .claim_next(
            "worker-clean-active-locked".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("locked active clean claim should return")
        .expect("locked active clean job should claim");
    assert_eq!(active_locked_claim.id, active_locked.id);
    let active_unlocked_claim = clean_active_queue
        .claim_next(
            "worker-clean-active-unlocked".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("unlocked active clean claim should return")
        .expect("unlocked active clean job should claim");
    assert_eq!(active_unlocked_claim.id, active_unlocked.id);
    clean_active_queue
        .add_log(
            &active_unlocked.id,
            "active cleanup log".to_string(),
            10,
            Utc::now(),
        )
        .await
        .expect("unlocked active clean log should append");
    let active_unlocked_lock_key = format!("{namespace}:clean-active:locks:{}", active_unlocked.id);
    let active_unlocked_logs_key = format!("{namespace}:clean-active:logs:{}", active_unlocked.id);
    let active_stalled_key = format!("{namespace}:clean-active:stalled");
    let removed_unlocked_lock: usize = clean_active_conn.del(&active_unlocked_lock_key).await?;
    assert_eq!(removed_unlocked_lock, 1);
    let stalled_inserted: usize = clean_active_conn
        .sadd(&active_stalled_key, &active_unlocked.id)
        .await?;
    assert_eq!(stalled_inserted, 1);
    let active_clean_now = active_unlocked_claim
        .lease_expires_at
        .expect("unlocked active claim should carry a lease expiration")
        + chrono::Duration::seconds(1);

    let active_cleaned = clean_active_queue
        .clean_jobs(JobState::Active, Duration::ZERO, 10, active_clean_now)
        .await
        .expect("active clean should run");
    assert_eq!(active_cleaned.len(), 1);
    assert_eq!(active_cleaned[0].id, active_unlocked.id);
    assert!(clean_active_queue
        .get_job(&active_unlocked.id)
        .await
        .expect("unlocked active lookup should return")
        .is_none());
    let active_unlocked_score: Option<f64> = clean_active_conn
        .zscore(
            format!("{namespace}:clean-active:active"),
            &active_unlocked.id,
        )
        .await?;
    assert!(active_unlocked_score.is_none());
    let active_unlocked_logs_len: usize = clean_active_conn.llen(&active_unlocked_logs_key).await?;
    assert_eq!(active_unlocked_logs_len, 0);
    let active_unlocked_stalled: bool = clean_active_conn
        .sismember(&active_stalled_key, &active_unlocked.id)
        .await?;
    assert!(!active_unlocked_stalled);
    let active_locked_after_clean = clean_active_queue
        .get_job(&active_locked.id)
        .await
        .expect("locked active lookup should return")
        .expect("locked active job should remain");
    assert_eq!(active_locked_after_clean.state, JobState::Active);
    let active_locked_score: Option<f64> = clean_active_conn
        .zscore(
            format!("{namespace}:clean-active:active"),
            &active_locked.id,
        )
        .await?;
    assert!(active_locked_score.is_some());
    trace_stage("clean-active:done");

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

    producer
        .drain_jobs(true)
        .await
        .expect("pre-flow waiting and delayed leftovers should drain");

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
                .with_delay(Duration::from_secs(3_600)),
        )
        .await
        .expect("existing flow child id should be added");
    let reused_flow_parent_id = format!("{namespace}:flow:reused-parent");
    let reused_flow_new_child_id = format!("{namespace}:flow:reused-new-child");
    let reused_flow = producer
        .add_flow_at(
            JobSpec::new(
                "reused-flow-parent",
                serde_json::json!({ "kind": "aggregate" }),
            )
            .with_options(JobOptions::new().with_job_id(reused_flow_parent_id.clone())),
            vec![
                JobSpec::new("duplicate-flow-child", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_job_id(existing_flow_child_id.clone())),
                JobSpec::new("new-flow-child", serde_json::json!({ "n": 2 }))
                    .with_options(JobOptions::new().with_job_id(reused_flow_new_child_id.clone())),
            ],
            Utc::now(),
        )
        .await
        .expect("flow should reuse an existing Redis child id");
    assert_eq!(reused_flow.parent.state, JobState::WaitingChildren);
    assert_eq!(
        reused_flow.parent.child_ids,
        vec![
            existing_flow_child_id.clone(),
            reused_flow_new_child_id.clone()
        ]
    );
    assert_eq!(reused_flow.children.len(), 2);
    assert_eq!(reused_flow.children[0].id, existing_flow_child_id);
    assert_eq!(reused_flow.children[0].name, "existing-flow-child");
    assert_eq!(
        reused_flow.children[0].parent_id.as_deref(),
        Some(reused_flow.parent.id.as_str())
    );
    let reused_flow_dependencies_key =
        format!("{namespace}:jobs:dependencies:{}", reused_flow.parent.id);
    let reused_flow_dependencies: usize =
        flow_index_conn.scard(&reused_flow_dependencies_key).await?;
    assert_eq!(reused_flow_dependencies, 2);
    let reused_parent_waiting_children_score: Option<f64> = flow_index_conn
        .zscore(
            format!("{namespace}:jobs:waiting_children"),
            &reused_flow_parent_id,
        )
        .await?;
    assert!(reused_parent_waiting_children_score.is_some());
    let removed_reused_children = producer
        .remove_unprocessed_children(&reused_flow.parent.id, Utc::now())
        .await
        .expect("reused flow children should be removable")
        .expect("reused flow parent should exist");
    assert_eq!(removed_reused_children.len(), 2);
    producer
        .remove_job(&reused_flow.parent.id)
        .await
        .expect("reused flow parent should be removable");

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
    let remove_unprocessed_events = producer
        .read_events("-", "+", 200)
        .await
        .expect("remove-unprocessed events should read");
    assert!(remove_unprocessed_events.iter().any(|event| {
        event.event == "removed"
            && event.job_id.as_deref() == Some(remove_unprocessed_flow.children[2].id.as_str())
            && event.prev == Some(JobState::Waiting)
    }));
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

    let clean_release_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-clean-release")
            .expect("valid Redis URL should build the clean-release flow queue");
    let clean_release_worker =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "flow-clean-release")
            .expect("valid Redis URL should build the clean-release flow worker");
    let mut clean_release_conn = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;
    let clean_release_flow = clean_release_queue
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
    let clean_release_child = clean_release_worker
        .claim_next(
            "worker-clean-release-child-a".to_string(),
            Duration::from_secs(30),
            Utc::now(),
        )
        .await
        .expect("clean-release child claim should return")
        .expect("clean-release child should be claimable");
    assert!(clean_release_flow
        .children
        .iter()
        .any(|child| child.id == clean_release_child.id));
    let clean_release_remaining_child = clean_release_flow
        .children
        .iter()
        .find(|child| child.id != clean_release_child.id)
        .expect("clean-release flow should have one unclaimed child");
    clean_release_worker
        .complete_job(
            &clean_release_child.id,
            lock_token(&clean_release_child),
            serde_json::json!({ "ok": 1 }),
            Utc::now(),
        )
        .await
        .expect("clean-release child should complete");
    let clean_released = clean_release_queue
        .clean_jobs(JobState::Waiting, Duration::from_millis(0), 10, Utc::now())
        .await
        .expect("waiting flow child should clean");
    assert_eq!(clean_released.len(), 1);
    assert_eq!(clean_released[0].id, clean_release_remaining_child.id);
    let clean_released_parent = clean_release_queue
        .get_job(&clean_release_flow.parent.id)
        .await
        .expect("clean-released parent should load")
        .expect("clean-released parent should exist");
    assert_eq!(clean_released_parent.state, JobState::Waiting);
    let clean_released_parent_waiting_score: Option<f64> = clean_release_conn
        .zscore(
            format!("{namespace}:flow-clean-release:waiting"),
            &clean_release_flow.parent.id,
        )
        .await?;
    assert!(clean_released_parent_waiting_score.is_some());
    let clean_released_parent_waiting_children_score: Option<f64> = clean_release_conn
        .zscore(
            format!("{namespace}:flow-clean-release:waiting_children"),
            &clean_release_flow.parent.id,
        )
        .await?;
    assert!(clean_released_parent_waiting_children_score.is_none());
    let cleaned_flow_child_hash: Option<String> = clean_release_conn
        .hget(
            format!("{namespace}:flow-clean-release:jobs"),
            &clean_release_remaining_child.id,
        )
        .await?;
    assert!(cleaned_flow_child_hash.is_none());
    let claimed_clean_released_parent = clean_release_worker
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

    producer
        .drain_jobs(true)
        .await
        .expect("pre-delayed-flow leftovers should drain");

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
    let repeat_late = producer
        .add_job(
            "repeat-late".to_string(),
            serde_json::json!({ "kind": "heartbeat", "late": true }),
            JobOptions::new()
                .with_delay(Duration::from_millis(500))
                .with_repeat(
                    RepeatOptions::every(Duration::from_millis(200))
                        .with_limit(2)
                        .with_key("heartbeat-late"),
                ),
        )
        .await
        .expect("late repeat job should be added");
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
    assert_eq!(
        producer
            .count_repeats()
            .await
            .expect("repeat scheduler count should load"),
        2
    );
    assert_eq!(
        producer
            .get_repeat("heartbeat")
            .await
            .expect("repeat scheduler should load")
            .map(|entry| entry.job_id),
        Some(repeat.id.clone())
    );
    let repeat_page = producer
        .list_repeats_page(JobRepeatListOptions::new().with_limit(2))
        .await
        .expect("repeat scheduler page should load");
    assert_eq!(repeat_page.total, 2);
    assert_eq!(
        repeat_page
            .repeats
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["heartbeat-late", "heartbeat"]
    );
    assert_eq!(repeat_page.repeats[0].job_id, repeat_late.id);
    let repeat_page_asc = producer
        .list_repeats_page(
            JobRepeatListOptions::new()
                .ascending()
                .with_offset(1)
                .with_limit(1),
        )
        .await
        .expect("ascending repeat scheduler page should load");
    assert_eq!(repeat_page_asc.total, 2);
    assert_eq!(repeat_page_asc.repeats[0].key, "heartbeat-late");
    assert_eq!(
        producer
            .remove_repeat("heartbeat-late")
            .await
            .expect("late repeat scheduler should remove")
            .map(|job| job.id),
        Some(repeat_late.id)
    );
    assert_eq!(
        producer
            .count_repeats()
            .await
            .expect("repeat scheduler count after remove should load"),
        1
    );
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
    let repeat_scheduler_owner: Option<String> = flow_index_conn
        .hget(format!("{namespace}:jobs:repeat_meta:heartbeat"), "jid")
        .await?;
    assert_eq!(
        repeat_scheduler_owner.as_deref(),
        Some(repeat_successor.id.as_str())
    );
    let repeat_scheduler_key: Option<String> = flow_index_conn
        .hget(format!("{namespace}:jobs:repeat_meta:heartbeat"), "key")
        .await?;
    assert_eq!(repeat_scheduler_key.as_deref(), Some("heartbeat"));
    let repeat_scheduler_every: Option<u64> = flow_index_conn
        .hget(format!("{namespace}:jobs:repeat_meta:heartbeat"), "every")
        .await?;
    assert_eq!(repeat_scheduler_every, Some(200));
    let repeat_scheduler_limit: Option<u32> = flow_index_conn
        .hget(format!("{namespace}:jobs:repeat_meta:heartbeat"), "limit")
        .await?;
    assert_eq!(repeat_scheduler_limit, Some(2));
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
    let repeat_scheduler_after_limit: Option<String> = flow_index_conn
        .hget(format!("{namespace}:jobs:repeat_meta:heartbeat"), "jid")
        .await?;
    assert!(repeat_scheduler_after_limit.is_none());
    let repeat_scheduler_score_after_limit: Option<f64> = flow_index_conn
        .zscore(format!("{namespace}:jobs:repeat"), "heartbeat")
        .await?;
    assert!(repeat_scheduler_score_after_limit.is_none());
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
    let cron_scheduler_pattern: Option<String> = flow_index_conn
        .hget(
            format!("{namespace}:jobs:repeat_meta:cron-heartbeat"),
            "pattern",
        )
        .await?;
    assert_eq!(cron_scheduler_pattern.as_deref(), Some("0/1 * * * * * *"));
    let cron_scheduler_every: Option<u64> = flow_index_conn
        .hget(
            format!("{namespace}:jobs:repeat_meta:cron-heartbeat"),
            "every",
        )
        .await?;
    assert!(cron_scheduler_every.is_none());

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
    let repeat_remove_scheduler_owner: Option<String> = repeat_remove_conn
        .hget(
            format!("{namespace}:repeat-remove:repeat_meta:ephemeral-heartbeat"),
            "jid",
        )
        .await?;
    assert_eq!(
        repeat_remove_scheduler_owner.as_deref(),
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
    let repeat_remove_scheduler_after_remove: Option<String> = repeat_remove_conn
        .hget(
            format!("{namespace}:repeat-remove:repeat_meta:ephemeral-heartbeat"),
            "jid",
        )
        .await?;
    assert!(repeat_remove_scheduler_after_remove.is_none());
    let repeat_remove_scheduler_score_after_remove: Option<f64> = repeat_remove_conn
        .zscore(
            format!("{namespace}:repeat-remove:repeat"),
            "ephemeral-heartbeat",
        )
        .await?;
    assert!(repeat_remove_scheduler_score_after_remove.is_none());
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
    let _: usize = repeat_remove_conn
        .zadd(
            format!("{namespace}:repeat-remove:repeat"),
            "stale-heartbeat",
            1_i64,
        )
        .await?;
    let _: usize = repeat_remove_conn
        .hset(
            format!("{namespace}:repeat-remove:repeat_meta:stale-heartbeat"),
            "jid",
            "missing-repeat-owner",
        )
        .await?;
    let repeat_remove_entries_after_stale = repeat_remove_queue
        .list_repeats()
        .await
        .expect("repeat list should prune stale scheduler and owner keys");
    assert!(!repeat_remove_entries_after_stale
        .iter()
        .any(|entry| entry.key == "stale-heartbeat"));
    let stale_repeat_owner_after_list: Option<String> = repeat_remove_conn
        .get(format!("{namespace}:repeat-remove:repeat:stale-heartbeat"))
        .await?;
    assert!(stale_repeat_owner_after_list.is_none());
    let stale_repeat_scheduler_after_list: Option<f64> = repeat_remove_conn
        .zscore(
            format!("{namespace}:repeat-remove:repeat"),
            "stale-heartbeat",
        )
        .await?;
    assert!(stale_repeat_scheduler_after_list.is_none());
    let stale_repeat_meta_after_list: Option<String> = repeat_remove_conn
        .hget(
            format!("{namespace}:repeat-remove:repeat_meta:stale-heartbeat"),
            "jid",
        )
        .await?;
    assert!(stale_repeat_meta_after_list.is_none());
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

    // This lifecycle scenario intentionally exercises large cleanup/drain paths.
    // The namespace is unique per run, and a blocking final namespace scan can
    // make the shared Redis integration instance unavailable for following
    // tests, so leave final disposal to the test Redis process lifecycle.
    trace_stage("cleanup:final:skipped");
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
    REDIS_TEST_URL
        .get_or_init(|| {
            let value = std::env::var("A3S_LANE_REDIS_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())?;

            match redis_preflight(&value) {
                Ok(()) => Some(value),
                Err(error) => {
                    eprintln!(
                        "skipping Redis integration test; A3S_LANE_REDIS_URL is not usable: {error}"
                    );
                    None
                }
            }
        })
        .clone()
}

fn redis_preflight(redis_url: &str) -> std::result::Result<(), String> {
    let info = redis_url
        .into_connection_info()
        .map_err(|error| format!("failed to parse Redis URL: {error}"))?;
    match info.addr {
        ConnectionAddr::Tcp(host, port) => tcp_preflight(&host, port),
        ConnectionAddr::TcpTls { host, port, .. } => tcp_preflight(&host, port),
        ConnectionAddr::Unix(path) => {
            if path.exists() {
                Ok(())
            } else {
                Err(format!("Redis unix socket does not exist: {path:?}"))
            }
        }
    }
}

fn tcp_preflight(host: &str, port: u16) -> std::result::Result<(), String> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("failed to resolve Redis address {host}:{port}: {error}"))?
        .collect::<Vec<_>>();
    if addrs.is_empty() {
        return Err(format!("failed to resolve Redis address {host}:{port}"));
    }

    let mut last_error = None;
    for _ in 0..5 {
        for addr in &addrs {
            match TcpStream::connect_timeout(addr, Duration::from_secs(1)) {
                Ok(_) => return Ok(()),
                Err(error) => {
                    last_error = Some(format!(
                        "failed to connect to Redis at {addr} within 1s: {error}"
                    ));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    Err(last_error.unwrap_or_else(|| format!("Redis preflight failed for {host}:{port}")))
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
    tokio::time::timeout(Duration::from_secs(30), async {
        let client = redis::Client::open(redis_url)?;
        let mut conn = client.get_connection_manager().await?;
        cleanup_namespace_with_conn(&mut conn, namespace).await
    })
    .await
    .map_err(|_| redis_timeout_error("Redis namespace cleanup timed out"))?
}

async fn cleanup_namespace_with_conn(
    conn: &mut redis::aio::ConnectionManager,
    namespace: &str,
) -> redis::RedisResult<()> {
    let mut cursor = 0_u64;
    loop {
        let mut scan_cmd = redis::cmd("SCAN");
        scan_cmd
            .arg(cursor)
            .arg("MATCH")
            .arg(format!("{namespace}:*"))
            .arg("COUNT")
            .arg(1000_u16);
        let scan = scan_cmd.query_async(conn);
        let (next_cursor, keys): (u64, Vec<String>) =
            tokio::time::timeout(Duration::from_secs(30), scan)
                .await
                .map_err(|_| redis_timeout_error("Redis namespace scan timed out"))??;
        if !keys.is_empty() {
            let mut unlink_cmd = redis::cmd("UNLINK");
            unlink_cmd.arg(&keys);
            let removed: redis::RedisResult<usize> =
                tokio::time::timeout(Duration::from_secs(30), unlink_cmd.query_async(conn))
                    .await
                    .map_err(|_| redis_timeout_error("Redis namespace unlink timed out"))?;
            if removed.is_err() {
                let _: usize = tokio::time::timeout(Duration::from_secs(30), conn.del(keys))
                    .await
                    .map_err(|_| redis_timeout_error("Redis namespace delete timed out"))??;
            }
        }
        if next_cursor == 0 {
            break;
        }
        cursor = next_cursor;
    }
    Ok(())
}

fn redis_timeout_error(message: &'static str) -> redis::RedisError {
    redis::RedisError::from((redis::ErrorKind::IoError, message))
}
