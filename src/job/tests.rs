use super::*;
use crate::error::LaneError;
use crate::retry::RetryPolicy;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

fn ts(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).unwrap()
}

fn lock_token(job: &Job) -> &str {
    job.lock_token
        .as_deref()
        .expect("claimed job should carry a lock token")
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
async fn custom_job_ids_make_add_idempotent() {
    let queue = InMemoryJobQueue::new("idempotent");
    let now = ts(1_000);
    let first = queue
        .add_at(
            "sync",
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_job_id("sync:crm:42")
                .with_priority(10),
            now,
        )
        .await
        .unwrap();
    let duplicate = queue
        .add_at(
            "sync-duplicate",
            serde_json::json!({ "version": 2 }),
            JobOptions::new()
                .with_job_id("sync:crm:42")
                .with_priority(1),
            ts(2_000),
        )
        .await
        .unwrap();

    assert_eq!(first.id, "sync:crm:42");
    assert_eq!(duplicate, first);
    let waiting = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Waiting))
        .await
        .unwrap();
    assert_eq!(waiting.total, 1);
    assert_eq!(waiting.jobs[0].name, "sync");
}

#[tokio::test]
async fn simple_deduplication_coalesces_non_terminal_jobs() {
    let queue = InMemoryJobQueue::new("dedup");
    let now = ts(1_000);
    let first = queue
        .add_at(
            "sync",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication_id("account:42"),
            now,
        )
        .await
        .unwrap();
    let duplicate = queue
        .add_at(
            "sync-duplicate",
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication_id("account:42"),
            ts(2_000),
        )
        .await
        .unwrap();

    assert_eq!(duplicate, first);
    assert_eq!(queue.stats().await.unwrap().waiting, 1);

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(3_000))
        .await
        .unwrap()
        .unwrap();
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            ts(4_000),
        )
        .await
        .unwrap();

    let after_terminal = queue
        .add_at(
            "sync-after-terminal",
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication_id("account:42"),
            ts(5_000),
        )
        .await
        .unwrap();

    assert_ne!(after_terminal.id, first.id);
    assert_eq!(after_terminal.name, "sync-after-terminal");
}

#[tokio::test]
async fn retry_reclaims_simple_deduplication() {
    let queue = InMemoryJobQueue::new("dedup-retry");
    let now = ts(1_000);
    let first = queue
        .add_at(
            "sync",
            serde_json::json!({}),
            JobOptions::new().with_deduplication_id("account:retry"),
            now,
        )
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_100))
        .await
        .unwrap()
        .unwrap();
    queue
        .fail_job(
            &claimed.id,
            lock_token(&claimed),
            "boom".to_string(),
            ts(1_200),
        )
        .await
        .unwrap();

    let retried = queue.retry_job(&first.id, ts(1_300)).await.unwrap();
    assert_eq!(retried.state, JobState::Waiting);
    let duplicate = queue
        .add_at(
            "sync-duplicate",
            serde_json::json!({}),
            JobOptions::new().with_deduplication_id("account:retry"),
            ts(1_400),
        )
        .await
        .unwrap();

    assert_eq!(duplicate.id, first.id);
    assert_eq!(queue.stats().await.unwrap().waiting, 1);
}

#[tokio::test]
async fn retry_rejects_active_deduplication_owner() {
    let queue = InMemoryJobQueue::new("dedup-retry-conflict");
    let first = queue
        .add_at(
            "sync",
            serde_json::json!({}),
            JobOptions::new().with_deduplication_id("account:retry-conflict"),
            ts(1_000),
        )
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_100))
        .await
        .unwrap()
        .unwrap();
    queue
        .fail_job(
            &claimed.id,
            lock_token(&claimed),
            "boom".to_string(),
            ts(1_200),
        )
        .await
        .unwrap();
    let second = queue
        .add_at(
            "sync-after-fail",
            serde_json::json!({}),
            JobOptions::new().with_deduplication_id("account:retry-conflict"),
            ts(1_300),
        )
        .await
        .unwrap();

    assert_ne!(second.id, first.id);
    let error = queue.retry_job(&first.id, ts(1_400)).await.unwrap_err();
    assert!(matches!(error, LaneError::JobStateConflict(_)));
}

#[tokio::test]
async fn add_many_preserves_order_and_idempotent_custom_ids() {
    let queue = InMemoryJobQueue::new("bulk");
    let now = ts(1_000);
    let jobs = queue
        .add_many_at(
            vec![
                JobSpec::new("first", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_job_id("bulk:first")),
                JobSpec::new("second", serde_json::json!({ "n": 2 }))
                    .with_options(JobOptions::new().with_job_id("bulk:second")),
                JobSpec::new("first-duplicate", serde_json::json!({ "n": 3 }))
                    .with_options(JobOptions::new().with_job_id("bulk:first")),
            ],
            now,
        )
        .await
        .unwrap();

    assert_eq!(jobs.len(), 3);
    assert_eq!(jobs[0].id, "bulk:first");
    assert_eq!(jobs[1].id, "bulk:second");
    assert_eq!(jobs[2], jobs[0]);
    assert_eq!(jobs[2].name, "first");
    assert_eq!(queue.stats().await.unwrap().waiting, 2);
}

#[tokio::test]
async fn add_many_rejects_invalid_jobs_without_partial_insert() {
    let queue = InMemoryJobQueue::new("bulk-invalid");
    let error = queue
        .add_many_at(
            vec![
                JobSpec::new("valid", serde_json::json!({}))
                    .with_options(JobOptions::new().with_job_id("bulk:valid")),
                JobSpec::new("invalid", serde_json::json!({}))
                    .with_options(JobOptions::new().with_job_id("  ")),
            ],
            ts(1_000),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, LaneError::ConfigError(_)));
    assert_eq!(queue.stats().await.unwrap().total, 0);
}

#[tokio::test]
async fn priority_updates_reorder_waiting_jobs() {
    let queue = InMemoryJobQueue::new("priority-update");
    let now = ts(1_000);
    let first = queue
        .add_at(
            "first",
            serde_json::json!({}),
            JobOptions::new().with_priority(50),
            now,
        )
        .await
        .unwrap();
    let second = queue
        .add_at(
            "second",
            serde_json::json!({}),
            JobOptions::new().with_priority(60),
            now,
        )
        .await
        .unwrap();

    let updated = queue.update_priority(&second.id, 1).await.unwrap();
    assert_eq!(updated.priority, 1);
    assert_eq!(updated.options.priority, 1);

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, second.id);
    assert_ne!(claimed.id, first.id);
}

#[tokio::test]
async fn priority_updates_reject_terminal_jobs() {
    let queue = InMemoryJobQueue::new("priority-terminal");
    let now = ts(1_000);
    let job = queue
        .add_at("task", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    queue
        .complete_job(&job.id, lock_token(&claimed), serde_json::json!({}), now)
        .await
        .unwrap();

    let error = queue.update_priority(&job.id, 1).await.unwrap_err();
    assert!(matches!(error, LaneError::JobStateConflict(_)));
}

#[tokio::test]
async fn repeatable_jobs_schedule_next_occurrence_after_completion() {
    let queue = InMemoryJobQueue::new("repeat");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "sync",
            serde_json::json!({ "source": "crm" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(5))
                    .with_limit(2)
                    .with_key("crm-sync"),
            ),
            now,
        )
        .await
        .unwrap();
    assert_eq!(job.repeat_key.as_deref(), Some("crm-sync"));
    assert_eq!(job.repeat_count, 0);

    let first = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.id, job.id);
    queue
        .complete_job(
            &first.id,
            lock_token(&first),
            serde_json::json!({ "ok": true }),
            ts(1_100),
        )
        .await
        .unwrap();

    let delayed = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .unwrap();
    assert_eq!(delayed.total, 1);
    let next = &delayed.jobs[0];
    assert_eq!(next.name, "sync");
    assert_eq!(next.repeat_key.as_deref(), Some("crm-sync"));
    assert_eq!(next.repeat_count, 1);
    assert_eq!(next.scheduled_at, ts(6_100));

    assert!(queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(6_000))
        .await
        .unwrap()
        .is_none());
    queue.promote_due_jobs(ts(6_100)).await.unwrap();
    let second = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(6_100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.repeat_count, 1);
    queue
        .complete_job(
            &second.id,
            lock_token(&second),
            serde_json::json!({ "ok": true }),
            ts(6_200),
        )
        .await
        .unwrap();

    let delayed = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .unwrap();
    assert_eq!(delayed.total, 0);
}

#[tokio::test]
async fn cron_repeatable_jobs_schedule_next_matching_occurrence() {
    let queue = InMemoryJobQueue::new("cron-repeat");
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let job = queue
        .add_at(
            "sync",
            serde_json::json!({ "source": "warehouse" }),
            JobOptions::new().with_repeat(
                RepeatOptions::cron("0/5 * * * * * *")
                    .with_limit(2)
                    .with_key("warehouse-sync"),
            ),
            now,
        )
        .await
        .unwrap();
    assert_eq!(job.repeat_key.as_deref(), Some("warehouse-sync"));

    let first = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    queue
        .complete_job(
            &first.id,
            lock_token(&first),
            serde_json::json!({ "ok": true }),
            now,
        )
        .await
        .unwrap();

    let delayed = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .unwrap();
    assert_eq!(delayed.total, 1);
    let next = &delayed.jobs[0];
    assert_eq!(next.repeat_key.as_deref(), Some("warehouse-sync"));
    assert_eq!(next.repeat_count, 1);
    assert_eq!(
        next.scheduled_at,
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 5).unwrap()
    );
}

#[tokio::test]
async fn repeat_key_coalesces_active_series_to_current_owner() {
    let queue = InMemoryJobQueue::new("repeat-owner");
    let now = ts(1_000);
    let first = queue
        .add_at(
            "heartbeat",
            serde_json::json!({ "target": "crm" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(5))
                    .with_limit(3)
                    .with_key("heartbeat-series"),
            ),
            now,
        )
        .await
        .unwrap();

    let duplicate = queue
        .add_at(
            "heartbeat-duplicate",
            serde_json::json!({ "target": "crm", "duplicate": true }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(5))
                    .with_limit(3)
                    .with_key("heartbeat-series"),
            ),
            ts(1_050),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.id, first.id);

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            ts(1_100),
        )
        .await
        .unwrap();

    let delayed = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .unwrap();
    assert_eq!(delayed.total, 1);
    let successor = &delayed.jobs[0];
    assert_eq!(successor.repeat_key.as_deref(), Some("heartbeat-series"));
    assert_eq!(successor.repeat_count, 1);

    let duplicate_during_delay = queue
        .add_at(
            "heartbeat-duplicate-delayed",
            serde_json::json!({ "target": "crm", "duplicate": "delayed" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(5))
                    .with_limit(3)
                    .with_key("heartbeat-series"),
            ),
            ts(1_200),
        )
        .await
        .unwrap();
    assert_eq!(duplicate_during_delay.id, successor.id);

    queue.remove_job(&successor.id).await.unwrap().unwrap();
    let new_series = queue
        .add_at(
            "heartbeat-after-remove",
            serde_json::json!({ "target": "crm", "after": "remove" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(5))
                    .with_limit(3)
                    .with_key("heartbeat-series"),
            ),
            ts(1_300),
        )
        .await
        .unwrap();
    assert_ne!(new_series.id, first.id);
    assert_ne!(new_series.id, successor.id);
    assert_eq!(new_series.repeat_count, 0);
}

#[test]
fn repeat_options_deserialize_legacy_interval_shape() {
    let repeat: RepeatOptions = serde_json::from_value(serde_json::json!({
        "interval": {
            "secs": 5,
            "nanos": 0
        },
        "limit": 2,
        "key": "legacy-sync"
    }))
    .unwrap();

    assert_eq!(repeat.interval(), Some(Duration::from_secs(5)));
    assert_eq!(repeat.cron_expression(), None);
    assert_eq!(repeat.limit, Some(2));
    assert_eq!(repeat.key.as_deref(), Some("legacy-sync"));
}

#[tokio::test]
async fn repeat_successors_do_not_reuse_custom_job_ids() {
    let queue = InMemoryJobQueue::new("repeat-custom-id");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "sync",
            serde_json::json!({}),
            JobOptions::new()
                .with_job_id("sync:first")
                .with_repeat(RepeatOptions::every(Duration::from_secs(5)).with_limit(2)),
            now,
        )
        .await
        .unwrap();
    assert_eq!(job.id, "sync:first");

    let first = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    queue
        .complete_job(
            &first.id,
            lock_token(&first),
            serde_json::json!({}),
            ts(1_100),
        )
        .await
        .unwrap();

    let delayed = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .unwrap();
    assert_eq!(delayed.total, 1);
    assert_ne!(delayed.jobs[0].id, "sync:first");
    assert_eq!(delayed.jobs[0].options.job_id, None);
    assert_eq!(delayed.jobs[0].repeat_count, 1);
}

#[tokio::test]
async fn repeat_options_reject_invalid_schedules() {
    let queue = InMemoryJobQueue::new("repeat-invalid");
    let zero_interval = queue
        .add_at(
            "sync",
            serde_json::json!({}),
            JobOptions::new().with_repeat(RepeatOptions::every(Duration::ZERO)),
            ts(1_000),
        )
        .await
        .unwrap_err();
    assert!(matches!(zero_interval, LaneError::ConfigError(_)));

    let zero_limit = queue
        .add_at(
            "sync",
            serde_json::json!({}),
            JobOptions::new()
                .with_repeat(RepeatOptions::every(Duration::from_secs(1)).with_limit(0)),
            ts(1_000),
        )
        .await
        .unwrap_err();
    assert!(matches!(zero_limit, LaneError::ConfigError(_)));

    let invalid_cron = queue
        .add_at(
            "sync",
            serde_json::json!({}),
            JobOptions::new().with_repeat(RepeatOptions::cron("not a cron")),
            ts(1_000),
        )
        .await
        .unwrap_err();
    assert!(matches!(invalid_cron, LaneError::ConfigError(_)));

    let empty_job_id = queue
        .add_at(
            "sync",
            serde_json::json!({}),
            JobOptions::new().with_job_id("  "),
            ts(1_000),
        )
        .await
        .unwrap_err();
    assert!(matches!(empty_job_id, LaneError::ConfigError(_)));
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
        .fail_job(
            &job.id,
            lock_token(&first),
            "network".to_string(),
            ts(1_100),
        )
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
        .fail_job(
            &job.id,
            lock_token(&second),
            "still down".to_string(),
            ts(3_200),
        )
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
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
        .await
        .unwrap()
        .unwrap();

    let completed = queue
        .complete_job(
            &job.id,
            lock_token(&claimed),
            serde_json::json!({"ok": true}),
            ts(1_100),
        )
        .await
        .unwrap();
    assert_eq!(completed.state, JobState::Completed);
    assert!(queue.get_job(&job.id).await.unwrap().is_none());
}

#[tokio::test]
async fn remove_rejects_active_leased_jobs() {
    let queue = InMemoryJobQueue::new("remove-active");
    let now = ts(1_000);
    let job = queue
        .add_at("task", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();

    let error = queue.remove_job(&job.id).await.unwrap_err();
    assert!(matches!(error, LaneError::JobLeaseConflict(_)));
    assert_eq!(
        queue.get_job(&job.id).await.unwrap().unwrap().state,
        JobState::Active
    );

    queue
        .complete_job(
            &job.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            ts(1_100),
        )
        .await
        .unwrap();
    let removed = queue.remove_job(&job.id).await.unwrap().unwrap();
    assert_eq!(removed.id, job.id);
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
        .renew_lease(&job.id, "missing-token", Duration::from_secs(1), now)
        .await
        .unwrap_err();
    assert!(matches!(waiting_error, LaneError::JobStateConflict(_)));

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(1), now)
        .await
        .unwrap()
        .unwrap();

    let wrong_token = queue
        .renew_lease(&job.id, "wrong-token", Duration::from_secs(1), ts(1_500))
        .await
        .unwrap_err();
    assert!(matches!(wrong_token, LaneError::JobLeaseConflict(_)));

    let renewed = queue
        .renew_lease(
            &job.id,
            lock_token(&claimed),
            Duration::from_secs(3),
            ts(1_500),
        )
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
        .fail_job(
            &faster.id,
            lock_token(&claimed),
            "boom".to_string(),
            ts(1_400),
        )
        .await
        .unwrap();
    assert_eq!(failed.state, JobState::Failed);

    let retried = queue.retry_job(&faster.id, ts(1_500)).await.unwrap();
    assert_eq!(retried.state, JobState::Waiting);
    assert!(retried.failed_reason.is_none());

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_500))
        .await
        .unwrap()
        .unwrap();
    queue
        .complete_job(
            &faster.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            ts(1_600),
        )
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
async fn flow_parent_waits_for_children_before_claiming() {
    let queue = InMemoryJobQueue::new("flow");
    let now = ts(1_000);
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("child-a", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(5)),
                JobSpec::new("child-b", serde_json::json!({ "n": 2 }))
                    .with_options(JobOptions::new().with_priority(10)),
            ],
            now,
        )
        .await
        .unwrap();

    assert_eq!(flow.parent.state, JobState::WaitingChildren);
    assert_eq!(flow.parent.child_ids.len(), 2);
    assert!(flow
        .children
        .iter()
        .all(|child| child.parent_id.as_deref() == Some(flow.parent.id.as_str())));

    let stats = queue.stats().await.unwrap();
    assert_eq!(stats.waiting_children, 1);
    assert_eq!(stats.waiting, 2);

    let first_child = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_child.id, flow.children[0].id);
    queue
        .complete_job(
            &first_child.id,
            lock_token(&first_child),
            serde_json::json!({ "ok": 1 }),
            ts(1_200),
        )
        .await
        .unwrap();
    assert_eq!(
        queue.get_job(&flow.parent.id).await.unwrap().unwrap().state,
        JobState::WaitingChildren
    );

    let second_child = queue
        .claim_next("worker-b".to_string(), Duration::from_secs(30), ts(1_300))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second_child.id, flow.children[1].id);
    queue
        .complete_job(
            &second_child.id,
            lock_token(&second_child),
            serde_json::json!({ "ok": 2 }),
            ts(1_400),
        )
        .await
        .unwrap();

    let parent = queue
        .get_job(&flow.parent.id)
        .await
        .unwrap()
        .expect("parent should remain stored");
    assert_eq!(parent.state, JobState::Waiting);

    let claimed_parent = queue
        .claim_next(
            "worker-parent".to_string(),
            Duration::from_secs(30),
            ts(1_500),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed_parent.id, flow.parent.id);
}

#[tokio::test]
async fn flow_parent_releases_when_pending_child_is_cleaned() {
    let queue = InMemoryJobQueue::new("flow-clean");
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("child-a", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(5)),
                JobSpec::new("child-b", serde_json::json!({ "n": 2 }))
                    .with_options(JobOptions::new().with_priority(10)),
            ],
            ts(1_000),
        )
        .await
        .unwrap();

    let first_child = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_child.id, flow.children[0].id);
    queue
        .complete_job(
            &first_child.id,
            lock_token(&first_child),
            serde_json::json!({ "ok": 1 }),
            ts(1_200),
        )
        .await
        .unwrap();

    let cleaned = queue
        .clean_jobs(JobState::Waiting, Duration::from_millis(100), 10, ts(1_300))
        .await
        .unwrap();
    assert_eq!(cleaned.len(), 1);
    assert_eq!(cleaned[0].id, flow.children[1].id);
    assert!(queue.get_job(&flow.children[1].id).await.unwrap().is_none());

    let parent = queue
        .get_job(&flow.parent.id)
        .await
        .unwrap()
        .expect("parent should remain stored");
    assert_eq!(parent.state, JobState::Waiting);
    let claimed_parent = queue
        .claim_next(
            "worker-parent".to_string(),
            Duration::from_secs(30),
            ts(1_400),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed_parent.id, flow.parent.id);
}

#[tokio::test]
async fn flow_rejects_duplicate_custom_job_ids() {
    let queue = InMemoryJobQueue::new("flow-ids");
    let error = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({}))
                .with_options(JobOptions::new().with_job_id("flow:duplicate")),
            vec![JobSpec::new("child", serde_json::json!({}))
                .with_options(JobOptions::new().with_job_id("flow:duplicate"))],
            ts(1_000),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, LaneError::ConfigError(_)));
    assert_eq!(queue.stats().await.unwrap().total, 0);
}

#[tokio::test]
async fn flow_parent_fails_when_child_terminally_fails() {
    let queue = InMemoryJobQueue::new("flow-fail");
    let now = ts(1_000);
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({})),
            vec![JobSpec::new("child", serde_json::json!({})).with_options(
                JobOptions::new()
                    .with_retry_policy(RetryPolicy::fixed(1, Duration::from_millis(100))),
            )],
            now,
        )
        .await
        .unwrap();

    let child = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    queue
        .fail_job(
            &child.id,
            lock_token(&child),
            "temporary".to_string(),
            ts(1_100),
        )
        .await
        .unwrap();
    assert_eq!(
        queue.get_job(&flow.parent.id).await.unwrap().unwrap().state,
        JobState::WaitingChildren
    );

    queue.promote_due_jobs(ts(1_200)).await.unwrap();
    let child = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_200))
        .await
        .unwrap()
        .unwrap();
    queue
        .fail_job(
            &child.id,
            lock_token(&child),
            "terminal".to_string(),
            ts(1_300),
        )
        .await
        .unwrap();

    let parent = queue.get_job(&flow.parent.id).await.unwrap().unwrap();
    assert_eq!(parent.state, JobState::Failed);
    assert!(parent
        .failed_reason
        .as_deref()
        .unwrap_or_default()
        .contains("child job"));
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
        .complete_job(&job.id, "missing-token", serde_json::json!({}), now)
        .await
        .unwrap_err();
    assert!(matches!(complete_error, LaneError::JobStateConflict(_)));

    let fail_error = queue
        .fail_job(&job.id, "missing-token", "boom".to_string(), now)
        .await
        .unwrap_err();
    assert!(matches!(fail_error, LaneError::JobStateConflict(_)));
}

#[tokio::test]
async fn local_job_queue_persists_flow_relationships() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("flow.json");
    let queue = LocalJobQueue::open("durable-flow", &snapshot_path)
        .await
        .unwrap();
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({})),
            vec![JobSpec::new("child", serde_json::json!({ "n": 1 }))],
            ts(1_000),
        )
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable-flow", &snapshot_path)
        .await
        .unwrap();
    let parent = reopened
        .get_job(&flow.parent.id)
        .await
        .unwrap()
        .expect("parent should be restored");
    let child = reopened
        .get_job(&flow.children[0].id)
        .await
        .unwrap()
        .expect("child should be restored");

    assert_eq!(parent.state, JobState::WaitingChildren);
    assert_eq!(parent.child_ids, vec![child.id.clone()]);
    assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
}

#[tokio::test]
async fn local_job_queue_persists_bulk_jobs() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("bulk.json");
    let queue = LocalJobQueue::open("durable-bulk", &snapshot_path)
        .await
        .unwrap();
    let jobs = queue
        .add_many_at(
            vec![
                JobSpec::new("first", serde_json::json!({}))
                    .with_options(JobOptions::new().with_job_id("bulk:first")),
                JobSpec::new("second", serde_json::json!({}))
                    .with_options(JobOptions::new().with_job_id("bulk:second")),
            ],
            ts(1_000),
        )
        .await
        .unwrap();
    assert_eq!(jobs.len(), 2);

    let reopened = LocalJobQueue::open("durable-bulk", &snapshot_path)
        .await
        .unwrap();
    assert!(reopened.get_job("bulk:first").await.unwrap().is_some());
    assert!(reopened.get_job("bulk:second").await.unwrap().is_some());
    assert_eq!(reopened.stats().await.unwrap().waiting, 2);
}

#[tokio::test]
async fn local_job_queue_persists_priority_updates() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("priority.json");
    let queue = LocalJobQueue::open("durable-priority", &snapshot_path)
        .await
        .unwrap();
    let job = queue
        .add_at(
            "task",
            serde_json::json!({}),
            JobOptions::new().with_priority(100),
            ts(1_000),
        )
        .await
        .unwrap();

    queue.update_priority(&job.id, 5).await.unwrap();

    let reopened = LocalJobQueue::open("durable-priority", &snapshot_path)
        .await
        .unwrap();
    let restored = reopened.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(restored.priority, 5);
    assert_eq!(restored.options.priority, 5);
}

#[tokio::test]
async fn local_job_queue_persists_repeat_successors() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("repeat.json");
    let queue = LocalJobQueue::open("durable-repeat", &snapshot_path)
        .await
        .unwrap();
    let job = queue
        .add_at(
            "sync",
            serde_json::json!({}),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(10))
                    .with_limit(2)
                    .with_key("sync"),
            ),
            ts(1_000),
        )
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_000))
        .await
        .unwrap()
        .unwrap();
    queue
        .complete_job(
            &job.id,
            lock_token(&claimed),
            serde_json::json!({}),
            ts(1_500),
        )
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable-repeat", &snapshot_path)
        .await
        .unwrap();
    let delayed = reopened
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .unwrap();
    assert_eq!(delayed.total, 1);
    assert_eq!(delayed.jobs[0].repeat_key.as_deref(), Some("sync"));
    assert_eq!(delayed.jobs[0].repeat_count, 1);
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
        .complete_job(
            &job.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            ts(1_300),
        )
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

#[tokio::test]
async fn worker_completes_claimed_job_and_preserves_context_updates() {
    let backend: Arc<dyn JobQueueBackend> = Arc::new(InMemoryJobQueue::new("worker"));
    let job = backend
        .add_job(
            "send".to_string(),
            serde_json::json!({ "to": "ops@example.com" }),
            JobOptions::new().with_priority(1),
        )
        .await
        .unwrap();

    let processor = Arc::new(job_processor_fn(
        |job: Job, context: JobContext| async move {
            context
                .update_progress(serde_json::json!({ "percent": 50 }))
                .await?;
            context.add_log("accepted by provider").await?;
            Ok(serde_json::json!({ "processed": job.name }))
        },
    ));
    let worker = JobWorker::new(
        Arc::clone(&backend),
        processor,
        JobWorkerConfig::new("worker-a").with_lease_renew_interval(Duration::ZERO),
    );

    let outcome = worker.run_once(ts(1_000)).await.unwrap();
    let completed = match outcome {
        JobRunOutcome::Completed(job) => job,
        other => panic!("expected completed job, got {other:?}"),
    };
    assert_eq!(completed.id, job.id);
    assert_eq!(
        completed.return_value,
        Some(serde_json::json!({ "processed": "send" }))
    );

    let stored = backend.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(stored.state, JobState::Completed);
    assert_eq!(stored.progress, Some(serde_json::json!({ "percent": 50 })));
    assert_eq!(stored.logs.len(), 1);
    assert_eq!(stored.logs[0].line, "accepted by provider");
}

#[tokio::test]
async fn worker_router_dispatches_jobs_by_name() {
    let backend: Arc<dyn JobQueueBackend> = Arc::new(InMemoryJobQueue::new("worker-router"));
    let send = backend
        .add_job(
            "send".to_string(),
            serde_json::json!({ "to": "ops@example.com" }),
            JobOptions::new().with_priority(1),
        )
        .await
        .unwrap();
    let archive = backend
        .add_job(
            "archive".to_string(),
            serde_json::json!({ "path": "/tmp/report.json" }),
            JobOptions::new().with_priority(2),
        )
        .await
        .unwrap();

    let send_processor: Arc<dyn JobProcessor> = Arc::new(job_processor_fn(
        |job: Job, context: JobContext| async move {
            context.add_log("send handler").await?;
            Ok(serde_json::json!({
                "handler": "send",
                "to": job.payload["to"].clone()
            }))
        },
    ));
    let archive_processor: Arc<dyn JobProcessor> = Arc::new(job_processor_fn(
        |job: Job, context: JobContext| async move {
            context.add_log("archive handler").await?;
            Ok(serde_json::json!({
                "handler": "archive",
                "path": job.payload["path"].clone()
            }))
        },
    ));
    let router = JobProcessorRouter::new()
        .with_processor("send", send_processor)
        .with_processor("archive", archive_processor);
    assert_eq!(router.len(), 2);
    assert!(router.contains_processor("send"));

    let processor: Arc<dyn JobProcessor> = Arc::new(router);
    let worker = JobWorker::new(
        Arc::clone(&backend),
        processor,
        JobWorkerConfig::new("worker-a").with_lease_renew_interval(Duration::ZERO),
    );

    assert_eq!(worker.run_until_idle(10).await.unwrap(), 2);
    let stored_send = backend.get_job(&send.id).await.unwrap().unwrap();
    let stored_archive = backend.get_job(&archive.id).await.unwrap().unwrap();
    assert_eq!(stored_send.state, JobState::Completed);
    assert_eq!(stored_archive.state, JobState::Completed);
    assert_eq!(
        stored_send.return_value,
        Some(serde_json::json!({
            "handler": "send",
            "to": "ops@example.com"
        }))
    );
    assert_eq!(
        stored_archive.return_value,
        Some(serde_json::json!({
            "handler": "archive",
            "path": "/tmp/report.json"
        }))
    );
    assert_eq!(stored_send.logs[0].line, "send handler");
    assert_eq!(stored_archive.logs[0].line, "archive handler");
}

#[tokio::test]
async fn worker_router_fails_jobs_without_registered_processor() {
    let backend: Arc<dyn JobQueueBackend> = Arc::new(InMemoryJobQueue::new("worker-router"));
    let job = backend
        .add_job(
            "unregistered".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .unwrap();
    let processor: Arc<dyn JobProcessor> = Arc::new(JobProcessorRouter::new());
    let worker = JobWorker::new(
        Arc::clone(&backend),
        processor,
        JobWorkerConfig::new("worker-a").with_lease_renew_interval(Duration::ZERO),
    );

    let outcome = worker.run_once(ts(1_000)).await.unwrap();
    let failed = match outcome {
        JobRunOutcome::Failed(job) => job,
        other => panic!("expected failed job, got {other:?}"),
    };
    assert_eq!(failed.id, job.id);
    assert_eq!(failed.state, JobState::Failed);
    assert!(failed
        .failed_reason
        .as_deref()
        .unwrap_or_default()
        .contains("no processor registered for job `unregistered`"));
}

#[tokio::test]
async fn worker_failure_marks_job_failed() {
    let backend: Arc<dyn JobQueueBackend> = Arc::new(InMemoryJobQueue::new("worker"));
    let job = backend
        .add_job("fail".to_string(), serde_json::json!({}), JobOptions::new())
        .await
        .unwrap();

    let processor = Arc::new(job_processor_fn(|_, _| async {
        Err::<Value, LaneError>(LaneError::Other("processor failed".to_string()))
    }));
    let worker = JobWorker::new(
        Arc::clone(&backend),
        processor,
        JobWorkerConfig::new("worker-a").with_lease_renew_interval(Duration::ZERO),
    );

    let outcome = worker.run_once(ts(1_000)).await.unwrap();
    let failed = match outcome {
        JobRunOutcome::Failed(job) => job,
        other => panic!("expected failed job, got {other:?}"),
    };
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.failed_reason.as_deref(), Some("processor failed"));
    assert_eq!(
        backend
            .get_job(&job.id)
            .await
            .unwrap()
            .unwrap()
            .failed_reason
            .as_deref(),
        Some("processor failed")
    );
}

#[tokio::test]
async fn worker_timeout_fails_job() {
    let backend: Arc<dyn JobQueueBackend> = Arc::new(InMemoryJobQueue::new("worker"));
    let job = backend
        .add_job(
            "slow".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_timeout(Duration::from_millis(10)),
        )
        .await
        .unwrap();

    let processor = Arc::new(job_processor_fn(|_, _| async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(serde_json::json!({ "late": true }))
    }));
    let worker = JobWorker::new(
        Arc::clone(&backend),
        processor,
        JobWorkerConfig::new("worker-a").with_lease_renew_interval(Duration::ZERO),
    );

    let outcome = worker.run_once(ts(1_000)).await.unwrap();
    let failed = match outcome {
        JobRunOutcome::Failed(job) => job,
        other => panic!("expected failed job, got {other:?}"),
    };
    assert_eq!(failed.state, JobState::Failed);
    assert!(failed
        .failed_reason
        .as_deref()
        .unwrap_or_default()
        .contains("timed out"));
    assert_eq!(
        backend.get_job(&job.id).await.unwrap().unwrap().state,
        JobState::Failed
    );
}

#[tokio::test]
async fn worker_context_reports_lost_lease_after_renewal_failure() {
    let queue = Arc::new(InMemoryJobQueue::new("worker-lease-lost"));
    let backend: Arc<dyn JobQueueBackend> = queue.clone();
    let job = backend
        .add_job(
            "lease-sensitive".to_string(),
            serde_json::json!({}),
            JobOptions::new(),
        )
        .await
        .unwrap();

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let (lost_tx, mut lost_rx) = tokio::sync::mpsc::unbounded_channel();
    let processor = Arc::new(job_processor_fn(move |_job: Job, context: JobContext| {
        let started_tx = started_tx.clone();
        let lost_tx = lost_tx.clone();
        async move {
            let _ = started_tx.send(());
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            loop {
                if context.has_lost_lease() {
                    let progress_error = context
                        .update_progress(serde_json::json!({ "stale": true }))
                        .await
                        .unwrap_err();
                    assert!(matches!(progress_error, LaneError::JobLeaseConflict(_)));
                    let _ = lost_tx.send(());
                    return Ok(serde_json::json!({ "stale": true }));
                }

                if tokio::time::Instant::now() >= deadline {
                    return Err(LaneError::Other(
                        "lease loss was not reported to the processor".to_string(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }));
    let worker = JobWorker::new(
        Arc::clone(&backend),
        processor,
        JobWorkerConfig::new("worker-a")
            .with_lease_duration(Duration::from_secs(30))
            .with_lease_renew_interval(Duration::from_millis(10)),
    );

    let run = tokio::spawn(async move { worker.run_once(ts(1_000)).await });
    tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
        .await
        .expect("processor should start")
        .expect("processor should send start signal");

    assert_eq!(queue.recover_stalled_jobs(ts(40_000)).await.unwrap(), 1);
    tokio::time::timeout(Duration::from_secs(1), lost_rx.recv())
        .await
        .expect("processor should observe lost lease")
        .expect("processor should send lost lease signal");

    let error = run
        .await
        .expect("worker task should join")
        .expect_err("worker should not finalize a job after lease loss");
    assert!(matches!(error, LaneError::JobLeaseConflict(_)));
    assert_eq!(
        backend.get_job(&job.id).await.unwrap().unwrap().state,
        JobState::Waiting
    );
}

#[tokio::test]
async fn worker_run_until_idle_processes_ready_jobs() {
    let backend: Arc<dyn JobQueueBackend> = Arc::new(InMemoryJobQueue::new("worker"));
    for name in ["a", "b"] {
        backend
            .add_job(name.to_string(), serde_json::json!({}), JobOptions::new())
            .await
            .unwrap();
    }

    let processor = Arc::new(job_processor_fn(
        |job: Job, _context: JobContext| async move { Ok(serde_json::json!({ "name": job.name })) },
    ));
    let worker = JobWorker::new(
        Arc::clone(&backend),
        processor,
        JobWorkerConfig::new("worker-a").with_lease_renew_interval(Duration::ZERO),
    );

    assert_eq!(worker.run_until_idle(10).await.unwrap(), 2);
    let stats = backend.stats().await.unwrap();
    assert_eq!(stats.completed, 2);
    assert_eq!(stats.waiting, 0);
}
