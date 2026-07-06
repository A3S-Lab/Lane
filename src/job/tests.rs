use super::*;
use crate::error::LaneError;
use crate::retry::RetryPolicy;
use chrono::{DateTime, TimeZone, Utc};
use std::time::Duration;

fn ts(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).unwrap()
}

#[tokio::test]
async fn claims_waiting_jobs_by_priority_then_fifo() {
    let queue = InMemoryJobQueue::new("email");
    let now = ts(1_000);
    let low = queue
        .add_at(
            "low",
            serde_json::json!({"n": 1}),
            JobOptions::new().with_priority(50),
            now,
        )
        .await
        .unwrap();
    let high = queue
        .add_at(
            "high",
            serde_json::json!({"n": 2}),
            JobOptions::new().with_priority(5),
            now,
        )
        .await
        .unwrap();

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, high.id);
    assert_eq!(claimed.state, JobState::Active);
    assert_eq!(claimed.worker_id.as_deref(), Some("worker-a"));

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, low.id);
}

#[tokio::test]
async fn delayed_jobs_wait_until_due() {
    let queue = InMemoryJobQueue::new("reports");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "generate",
            serde_json::json!({}),
            JobOptions::new()
                .with_priority(1)
                .with_delay(Duration::from_secs(5)),
            now,
        )
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Delayed);

    let early = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(2_000))
        .await
        .unwrap();
    assert!(early.is_none());

    assert_eq!(queue.promote_due_jobs(ts(6_000)).await.unwrap(), 1);
    let due = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(6_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(due.id, job.id);
}

#[tokio::test]
async fn failed_jobs_retry_with_backoff_then_terminal_failure() {
    let queue = InMemoryJobQueue::new("webhooks");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "deliver",
            serde_json::json!({}),
            JobOptions::new().with_retry_policy(RetryPolicy::fixed(1, Duration::from_secs(2))),
            now,
        )
        .await
        .unwrap();

    let first = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.id, job.id);

    let retry = queue
        .fail_job(&job.id, "network".to_string(), ts(1_100))
        .await
        .unwrap();
    assert_eq!(retry.state, JobState::Delayed);
    assert_eq!(retry.scheduled_at, ts(3_100));

    let second = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(3_100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.attempts_made, 2);

    let failed = queue
        .fail_job(&job.id, "still down".to_string(), ts(3_200))
        .await
        .unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.failed_reason.as_deref(), Some("still down"));
}

#[tokio::test]
async fn stalled_jobs_are_recovered_until_limit() {
    let queue = InMemoryJobQueue::new("video");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "transcode",
            serde_json::json!({}),
            JobOptions::new().with_max_stalled_count(1),
            now,
        )
        .await
        .unwrap();
    queue
        .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
        .await
        .unwrap();

    assert_eq!(queue.recover_stalled_jobs(ts(2_001)).await.unwrap(), 1);
    let recovered = queue.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(recovered.state, JobState::Waiting);
    assert_eq!(recovered.stalled_count, 1);

    queue
        .claim_next("worker-b".to_string(), Duration::from_secs(1), ts(2_100))
        .await
        .unwrap();
    assert_eq!(queue.recover_stalled_jobs(ts(3_200)).await.unwrap(), 1);
    let failed = queue.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.stalled_count, 2);
}

#[tokio::test]
async fn pause_blocks_claiming_without_rejecting_adds() {
    let queue = InMemoryJobQueue::new("paused");
    let now = ts(1_000);
    queue.pause().await.unwrap();
    queue
        .add_at("task", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    assert!(queue
        .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
        .await
        .unwrap()
        .is_none());

    let stats = queue.stats().await.unwrap();
    assert!(stats.paused);
    assert_eq!(stats.waiting, 1);

    queue.resume().await.unwrap();
    assert!(queue
        .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn remove_on_complete_deletes_record_after_returning_snapshot() {
    let queue = InMemoryJobQueue::new("cleanup");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "task",
            serde_json::json!({}),
            JobOptions::new().remove_on_complete(true),
            now,
        )
        .await
        .unwrap();
    queue
        .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
        .await
        .unwrap();

    let completed = queue
        .complete_job(&job.id, serde_json::json!({"ok": true}), ts(1_100))
        .await
        .unwrap();
    assert_eq!(completed.state, JobState::Completed);
    assert!(queue.get_job(&job.id).await.unwrap().is_none());
}

#[tokio::test]
async fn lease_renewal_requires_active_owner() {
    let queue = InMemoryJobQueue::new("leases");
    let now = ts(1_000);
    let job = queue
        .add_at("task", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();

    let waiting_error = queue
        .renew_lease(&job.id, "worker-a", Duration::from_secs(1), now)
        .await
        .unwrap_err();
    assert!(matches!(waiting_error, LaneError::JobStateConflict(_)));

    queue
        .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
        .await
        .unwrap();

    let wrong_worker = queue
        .renew_lease(&job.id, "worker-b", Duration::from_secs(1), ts(1_500))
        .await
        .unwrap_err();
    assert!(matches!(wrong_worker, LaneError::JobLeaseConflict(_)));

    let renewed = queue
        .renew_lease(&job.id, "worker-a", Duration::from_secs(3), ts(1_500))
        .await
        .unwrap();
    assert_eq!(renewed.lease_expires_at, Some(ts(4_500)));
}

#[tokio::test]
async fn management_api_lists_progress_logs_retries_and_cleans_jobs() {
    let queue = InMemoryJobQueue::new("ops");
    let now = ts(1_000);
    let slower = queue
        .add_at(
            "slow",
            serde_json::json!({}),
            JobOptions::new().with_priority(20),
            now,
        )
        .await
        .unwrap();
    let faster = queue
        .add_at(
            "fast",
            serde_json::json!({}),
            JobOptions::new().with_priority(5),
            now,
        )
        .await
        .unwrap();
    let delayed = queue
        .add_at(
            "later",
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(10)),
            now,
        )
        .await
        .unwrap();

    let first_page = queue
        .list_jobs(
            JobListOptions::new()
                .with_state(JobState::Waiting)
                .with_limit(1),
        )
        .await
        .unwrap();
    assert_eq!(first_page.total, 2);
    assert_eq!(first_page.jobs[0].id, faster.id);

    let second_page = queue
        .list_jobs(
            JobListOptions::new()
                .with_state(JobState::Waiting)
                .with_offset(1)
                .with_limit(1),
        )
        .await
        .unwrap();
    assert_eq!(second_page.jobs[0].id, slower.id);

    let progress = queue
        .update_progress(&slower.id, serde_json::json!({ "percent": 50 }))
        .await
        .unwrap();
    assert_eq!(
        progress.progress,
        Some(serde_json::json!({ "percent": 50 }))
    );

    queue
        .add_log(&slower.id, "first".to_string(), 2, ts(1_100))
        .await
        .unwrap();
    queue
        .add_log(&slower.id, "second".to_string(), 2, ts(1_200))
        .await
        .unwrap();
    let logged = queue
        .add_log(&slower.id, "third".to_string(), 2, ts(1_300))
        .await
        .unwrap();
    assert_eq!(logged.logs.len(), 2);
    assert_eq!(logged.logs[0].line, "second");
    assert_eq!(logged.logs[1].line, "third");

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, faster.id);

    let failed = queue
        .fail_job(&faster.id, "boom".to_string(), ts(1_400))
        .await
        .unwrap();
    assert_eq!(failed.state, JobState::Failed);

    let retried = queue.retry_job(&faster.id, ts(1_500)).await.unwrap();
    assert_eq!(retried.state, JobState::Waiting);
    assert!(retried.failed_reason.is_none());

    queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_500))
        .await
        .unwrap();
    queue
        .complete_job(&faster.id, serde_json::json!({ "ok": true }), ts(1_600))
        .await
        .unwrap();

    let cleaned = queue
        .clean_jobs(
            JobState::Completed,
            Duration::from_millis(100),
            10,
            ts(1_800),
        )
        .await
        .unwrap();
    assert_eq!(cleaned.len(), 1);
    assert_eq!(cleaned[0].id, faster.id);
    assert!(queue.get_job(&faster.id).await.unwrap().is_none());
    assert!(queue.get_job(&delayed.id).await.unwrap().is_some());
}

#[tokio::test]
async fn completing_or_failing_requires_active_job() {
    let queue = InMemoryJobQueue::new("state");
    let now = ts(1_000);
    let job = queue
        .add_at("task", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();

    let complete_error = queue
        .complete_job(&job.id, serde_json::json!({}), now)
        .await
        .unwrap_err();
    assert!(matches!(complete_error, LaneError::JobStateConflict(_)));

    let fail_error = queue
        .fail_job(&job.id, "boom".to_string(), now)
        .await
        .unwrap_err();
    assert!(matches!(fail_error, LaneError::JobStateConflict(_)));
}

#[tokio::test]
async fn local_job_queue_persists_snapshot_across_reopen() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("queue.json");
    let now = ts(1_000);

    let queue = LocalJobQueue::open("durable", &snapshot_path)
        .await
        .unwrap();
    let job = queue
        .add_at(
            "email",
            serde_json::json!({ "to": "ops@example.com" }),
            JobOptions::new().with_priority(7),
            now,
        )
        .await
        .unwrap();
    queue.pause().await.unwrap();

    let reopened = LocalJobQueue::open("durable", &snapshot_path)
        .await
        .unwrap();
    let stats = reopened.stats().await.unwrap();
    assert!(stats.paused);
    assert_eq!(stats.waiting, 1);
    assert_eq!(
        reopened.get_job(&job.id).await.unwrap().unwrap().name,
        "email"
    );

    reopened.resume().await.unwrap();
    let claimed = reopened
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, job.id);
    reopened
        .update_progress(&job.id, serde_json::json!({ "percent": 90 }))
        .await
        .unwrap();
    reopened
        .add_log(&job.id, "almost done".to_string(), 10, ts(1_200))
        .await
        .unwrap();
    reopened
        .complete_job(&job.id, serde_json::json!({ "ok": true }), ts(1_300))
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable", &snapshot_path)
        .await
        .unwrap();
    let restored = reopened.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(restored.state, JobState::Completed);
    assert_eq!(
        restored.progress,
        Some(serde_json::json!({ "percent": 90 }))
    );
    assert_eq!(restored.logs.len(), 1);
    assert_eq!(
        restored.return_value,
        Some(serde_json::json!({ "ok": true }))
    );
}

#[tokio::test]
async fn local_job_queue_rejects_mismatched_snapshot_queue() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("queue.json");

    let queue = LocalJobQueue::open("a", &snapshot_path).await.unwrap();
    queue
        .add_at("task", serde_json::json!({}), JobOptions::new(), ts(1_000))
        .await
        .unwrap();

    let error = LocalJobQueue::open("b", &snapshot_path).await.unwrap_err();
    assert!(matches!(error, LaneError::ConfigError(_)));
}
