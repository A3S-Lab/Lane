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
async fn claims_lifo_jobs_newest_first_within_priority() {
    let queue = InMemoryJobQueue::new("lifo");
    let now = ts(1_000);
    let fifo = queue
        .add_at(
            "fifo",
            serde_json::json!({"n": 1}),
            JobOptions::new().with_priority(5),
            now,
        )
        .await
        .unwrap();
    let lifo_old = queue
        .add_at(
            "lifo-old",
            serde_json::json!({"n": 2}),
            JobOptions::new().with_priority(5).with_lifo(true),
            now,
        )
        .await
        .unwrap();
    let lifo_new = queue
        .add_at(
            "lifo-new",
            serde_json::json!({"n": 3}),
            JobOptions::new().with_priority(5).with_lifo(true),
            now,
        )
        .await
        .unwrap();
    let urgent = queue
        .add_at(
            "urgent",
            serde_json::json!({"n": 4}),
            JobOptions::new().with_priority(1),
            now,
        )
        .await
        .unwrap();

    assert!(fifo.enqueued_seq < lifo_old.enqueued_seq);
    assert!(lifo_old.enqueued_seq < lifo_new.enqueued_seq);
    assert!(lifo_new.enqueued_seq < urgent.enqueued_seq);

    for expected in [&urgent, &lifo_new, &lifo_old, &fifo] {
        let claimed = queue
            .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, expected.id);
    }
}

#[tokio::test]
async fn job_state_queries_follow_lifecycle() {
    let queue = InMemoryJobQueue::new("states");
    let now = ts(1_000);
    let waiting = queue
        .add_at("waiting", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    let delayed = queue
        .add_at(
            "delayed",
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(5)),
            now,
        )
        .await
        .unwrap();
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({})),
            vec![JobSpec::new("child", serde_json::json!({}))],
            now,
        )
        .await
        .unwrap();

    assert_eq!(
        queue.get_job_state(&waiting.id).await.unwrap(),
        Some(JobState::Waiting)
    );
    assert_eq!(
        queue.get_job_state(&delayed.id).await.unwrap(),
        Some(JobState::Delayed)
    );
    assert_eq!(
        queue.get_job_state(&flow.parent.id).await.unwrap(),
        Some(JobState::WaitingChildren)
    );

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        queue.get_job_state(&claimed.id).await.unwrap(),
        Some(JobState::Active)
    );
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            ts(1_100),
        )
        .await
        .unwrap();
    assert_eq!(
        queue.get_job_state(&claimed.id).await.unwrap(),
        Some(JobState::Completed)
    );
    assert_eq!(queue.get_job_state("missing").await.unwrap(), None);
}

#[tokio::test]
async fn job_counts_return_selected_and_default_states() {
    let queue = InMemoryJobQueue::new("state-counts");
    let now = ts(1_000);

    let active_job = queue
        .add_at("active", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    let active = queue
        .claim_next("worker-active".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.id, active_job.id);

    let completed_job = queue
        .add_at("completed", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    let completed = queue
        .claim_next("worker-completed".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.id, completed_job.id);
    queue
        .complete_job(
            &completed.id,
            lock_token(&completed),
            serde_json::json!({ "ok": true }),
            now,
        )
        .await
        .unwrap();

    let failed_job = queue
        .add_at("failed", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    let failed = queue
        .claim_next("worker-failed".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.id, failed_job.id);
    queue
        .fail_job(
            &failed.id,
            lock_token(&failed),
            "terminal failure".to_string(),
            now,
        )
        .await
        .unwrap();

    queue
        .add_at("waiting-a", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    queue
        .add_at("waiting-b", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    queue
        .add_at(
            "delayed",
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(30)),
            now,
        )
        .await
        .unwrap();
    queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({})),
            vec![JobSpec::new("child", serde_json::json!({}))],
            now,
        )
        .await
        .unwrap();

    let selected = queue
        .get_job_counts(&[
            JobState::Waiting,
            JobState::Delayed,
            JobState::Waiting,
            JobState::Active,
        ])
        .await
        .unwrap();
    assert_eq!(
        selected,
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

    let all = queue.get_job_counts(&[]).await.unwrap();
    assert_eq!(
        all,
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
        queue
            .get_job_count(&[JobState::Waiting, JobState::Delayed, JobState::Waiting])
            .await
            .unwrap(),
        4
    );
    assert_eq!(queue.get_job_count(&[]).await.unwrap(), 8);
    assert_eq!(queue.count_pending_jobs().await.unwrap(), 5);
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
async fn reschedule_delayed_job_changes_due_time() {
    let queue = InMemoryJobQueue::new("reschedule");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "generate",
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(10)),
            now,
        )
        .await
        .unwrap();

    let rescheduled = queue
        .reschedule_job(&job.id, Duration::from_secs(2), now)
        .await
        .unwrap();
    assert_eq!(rescheduled.state, JobState::Delayed);
    assert_eq!(rescheduled.scheduled_at, ts(3_000));
    assert_eq!(rescheduled.options.delay, Some(Duration::from_secs(2)));

    assert_eq!(queue.promote_due_jobs(ts(2_999)).await.unwrap(), 0);
    assert!(queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(2_999))
        .await
        .unwrap()
        .is_none());

    assert_eq!(queue.promote_due_jobs(ts(3_000)).await.unwrap(), 1);
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(3_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, job.id);

    let waiting = queue
        .add_at("waiting", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    let state_error = queue
        .reschedule_job(&waiting.id, Duration::from_secs(1), now)
        .await
        .unwrap_err();
    assert!(matches!(state_error, LaneError::JobStateConflict(_)));

    let zero_delay_error = queue.reschedule_job(&waiting.id, Duration::ZERO, now).await;
    assert!(matches!(zero_delay_error, Err(LaneError::ConfigError(_))));
}

#[tokio::test]
async fn active_jobs_can_be_moved_back_to_delayed_with_lock() {
    let queue = InMemoryJobQueue::new("active-delay");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "rate-limited",
            serde_json::json!({}),
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

    let wrong_token = queue
        .delay_active_job(&job.id, "wrong-token", Duration::from_secs(1), now)
        .await
        .unwrap_err();
    assert!(matches!(wrong_token, LaneError::JobLeaseConflict(_)));

    let delayed = queue
        .delay_active_job(&job.id, lock_token(&claimed), Duration::from_secs(2), now)
        .await
        .unwrap();
    assert_eq!(delayed.state, JobState::Delayed);
    assert_eq!(delayed.scheduled_at, ts(3_000));
    assert_eq!(delayed.options.delay, Some(Duration::from_secs(2)));
    assert_eq!(delayed.attempts_made, 1);
    assert!(delayed.worker_id.is_none());
    assert!(delayed.lock_token.is_none());
    assert!(delayed.lease_expires_at.is_none());

    let complete_error = queue
        .complete_job(
            &job.id,
            lock_token(&claimed),
            serde_json::json!({}),
            ts(1_100),
        )
        .await
        .unwrap_err();
    assert!(matches!(complete_error, LaneError::JobStateConflict(_)));
    assert!(queue
        .claim_next("worker-b".to_string(), Duration::from_secs(30), ts(2_999))
        .await
        .unwrap()
        .is_none());

    assert_eq!(queue.promote_due_jobs(ts(3_000)).await.unwrap(), 1);
    let reclaimed = queue
        .claim_next("worker-b".to_string(), Duration::from_secs(30), ts(3_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reclaimed.id, job.id);
    assert_eq!(reclaimed.attempts_made, 2);

    let waiting = queue
        .add_at("waiting", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    let state_error = queue
        .delay_active_job(&waiting.id, "missing-token", Duration::ZERO, now)
        .await
        .unwrap_err();
    assert!(matches!(state_error, LaneError::JobStateConflict(_)));
}

#[tokio::test]
async fn active_jobs_can_be_released_back_to_waiting_with_lock() {
    let queue = InMemoryJobQueue::new("active-release");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "yielding",
            serde_json::json!({}),
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

    let wrong_token = queue
        .release_active_job(&job.id, "wrong-token", ts(1_100))
        .await
        .unwrap_err();
    assert!(matches!(wrong_token, LaneError::JobLeaseConflict(_)));

    let released = queue
        .release_active_job(&job.id, lock_token(&claimed), ts(1_100))
        .await
        .unwrap();
    assert_eq!(released.state, JobState::Waiting);
    assert_eq!(released.scheduled_at, ts(1_100));
    assert_eq!(released.attempts_made, 1);
    assert!(released.worker_id.is_none());
    assert!(released.lock_token.is_none());
    assert!(released.lease_expires_at.is_none());

    let complete_error = queue
        .complete_job(
            &job.id,
            lock_token(&claimed),
            serde_json::json!({}),
            ts(1_200),
        )
        .await
        .unwrap_err();
    assert!(matches!(complete_error, LaneError::JobStateConflict(_)));

    let reclaimed = queue
        .claim_next("worker-b".to_string(), Duration::from_secs(30), ts(1_200))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reclaimed.id, job.id);
    assert_eq!(reclaimed.state, JobState::Active);
    assert_eq!(reclaimed.attempts_made, 2);
    assert_eq!(reclaimed.worker_id.as_deref(), Some("worker-b"));

    let waiting = queue
        .add_at("waiting", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    let state_error = queue
        .release_active_job(&waiting.id, "missing-token", ts(1_300))
        .await
        .unwrap_err();
    assert!(matches!(state_error, LaneError::JobStateConflict(_)));
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
async fn remove_deduplication_key_allows_a_new_owner() {
    let queue = InMemoryJobQueue::new("dedup-release");
    let first = queue
        .add_at(
            "sync",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication_id("account:42"),
            ts(1_000),
        )
        .await
        .unwrap();
    let duplicate = queue
        .add_at(
            "sync-duplicate",
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication_id("account:42"),
            ts(1_100),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.id, first.id);
    assert_eq!(
        queue
            .get_deduplication_job_id("account:42")
            .await
            .unwrap()
            .as_deref(),
        Some(first.id.as_str())
    );
    assert!(queue
        .get_deduplication_job_id("missing-account")
        .await
        .unwrap()
        .is_none());
    assert!(queue.get_deduplication_job_id("").await.unwrap().is_none());

    assert!(queue.remove_deduplication_key("account:42").await.unwrap());
    assert!(queue
        .get_deduplication_job_id("account:42")
        .await
        .unwrap()
        .is_none());
    assert!(!queue
        .remove_deduplication_key("missing-account")
        .await
        .unwrap());

    let second = queue
        .add_at(
            "sync-after-release",
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication_id("account:42"),
            ts(1_200),
        )
        .await
        .unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(
        queue
            .get_deduplication_job_id("account:42")
            .await
            .unwrap()
            .as_deref(),
        Some(second.id.as_str())
    );

    let duplicate_second = queue
        .add_at(
            "sync-after-release-duplicate",
            serde_json::json!({ "version": 4 }),
            JobOptions::new().with_deduplication_id("account:42"),
            ts(1_300),
        )
        .await
        .unwrap();
    assert_eq!(duplicate_second.id, second.id);
    assert_eq!(
        queue
            .get_deduplication_job_id("account:42")
            .await
            .unwrap()
            .as_deref(),
        Some(second.id.as_str())
    );
    assert_eq!(queue.stats().await.unwrap().waiting, 2);
}

#[tokio::test]
async fn removed_released_deduplication_owner_does_not_leave_stale_marker() {
    let queue = InMemoryJobQueue::new("dedup-release-cleanup");
    let first = queue
        .add_at(
            "sync",
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_job_id("sync:account:42")
                .with_deduplication_id("account:42"),
            ts(1_000),
        )
        .await
        .unwrap();

    assert!(queue.remove_deduplication_key("account:42").await.unwrap());
    let removed = queue.remove_job(&first.id).await.unwrap().unwrap();
    assert_eq!(removed.id, first.id);

    let reused = queue
        .add_at(
            "sync-reused-id",
            serde_json::json!({ "version": 2 }),
            JobOptions::new()
                .with_job_id("sync:account:42")
                .with_deduplication_id("account:42"),
            ts(1_100),
        )
        .await
        .unwrap();
    assert_eq!(reused.id, first.id);

    let duplicate = queue
        .add_at(
            "sync-duplicate",
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication_id("account:42"),
            ts(1_200),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.id, reused.id);
}

#[tokio::test]
async fn deduplication_ttl_allows_new_non_terminal_owner_after_expiration() {
    let queue = InMemoryJobQueue::new("dedup-ttl");
    let first = queue
        .add_at(
            "sync",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:ttl").with_ttl(Duration::from_secs(1)),
            ),
            ts(1_000),
        )
        .await
        .unwrap();
    assert_eq!(first.deduplication_expires_at, Some(ts(2_000)));

    let duplicate_before_ttl = queue
        .add_at(
            "sync-before-ttl",
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:ttl").with_ttl(Duration::from_secs(1)),
            ),
            ts(1_999),
        )
        .await
        .unwrap();
    assert_eq!(duplicate_before_ttl.id, first.id);

    let after_ttl = queue
        .add_at(
            "sync-after-ttl",
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:ttl").with_ttl(Duration::from_secs(1)),
            ),
            ts(2_000),
        )
        .await
        .unwrap();
    assert_ne!(after_ttl.id, first.id);
    assert_eq!(after_ttl.name, "sync-after-ttl");
    assert_eq!(after_ttl.deduplication_expires_at, Some(ts(3_000)));
    assert_eq!(queue.stats().await.unwrap().waiting, 2);
}

#[tokio::test]
async fn deduplication_extend_ttl_refreshes_owner_window() {
    let queue = InMemoryJobQueue::new("dedup-extend");
    let first = queue
        .add_at(
            "sync",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:extend").with_ttl(Duration::from_secs(1)),
            ),
            ts(1_000),
        )
        .await
        .unwrap();
    assert_eq!(first.deduplication_expires_at, Some(ts(2_000)));

    let duplicate = queue
        .add_at(
            "sync-duplicate",
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:extend")
                    .with_ttl(Duration::from_secs(1))
                    .extend_ttl(true),
            ),
            ts(1_500),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.id, first.id);
    assert_eq!(duplicate.deduplication_expires_at, Some(ts(2_500)));

    let still_owned = queue
        .add_at(
            "sync-still-owned",
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication_id("account:extend"),
            ts(2_250),
        )
        .await
        .unwrap();
    assert_eq!(still_owned.id, first.id);

    let after_extended_ttl = queue
        .add_at(
            "sync-after-extended-ttl",
            serde_json::json!({ "version": 4 }),
            JobOptions::new().with_deduplication_id("account:extend"),
            ts(2_500),
        )
        .await
        .unwrap();
    assert_ne!(after_extended_ttl.id, first.id);
}

#[tokio::test]
async fn deduplication_replace_swaps_delayed_owner() {
    let queue = InMemoryJobQueue::new("dedup-replace");
    let first = queue
        .add_at(
            "sync-old",
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(30))
                .with_deduplication(
                    DeduplicationOptions::new("account:replace")
                        .with_ttl(Duration::from_secs(5))
                        .replace_delayed(true),
                ),
            ts(1_000),
        )
        .await
        .unwrap();
    assert_eq!(first.state, JobState::Delayed);

    let replacement = queue
        .add_at(
            "sync-new",
            serde_json::json!({ "version": 2 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(60))
                .with_deduplication(
                    DeduplicationOptions::new("account:replace")
                        .with_ttl(Duration::from_secs(60))
                        .replace_delayed(true),
                ),
            ts(1_100),
        )
        .await
        .unwrap();

    assert_ne!(replacement.id, first.id);
    assert_eq!(replacement.name, "sync-new");
    assert_eq!(
        replacement.deduplication_expires_at,
        first.deduplication_expires_at
    );
    assert!(queue.get_job(&first.id).await.unwrap().is_none());
    assert_eq!(
        queue
            .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
            .await
            .unwrap()
            .jobs
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>(),
        vec![replacement.id.as_str()]
    );
}

#[tokio::test]
async fn deduplication_replace_does_not_swap_waiting_owner() {
    let queue = InMemoryJobQueue::new("dedup-replace-waiting");
    let first = queue
        .add_at(
            "sync-old",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:replace-waiting").replace_delayed(true),
            ),
            ts(1_000),
        )
        .await
        .unwrap();
    let duplicate = queue
        .add_at(
            "sync-new",
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:replace-waiting").replace_delayed(true),
            ),
            ts(1_100),
        )
        .await
        .unwrap();

    assert_eq!(duplicate.id, first.id);
    assert_eq!(queue.stats().await.unwrap().waiting, 1);
}

#[tokio::test]
async fn deduplication_keep_last_requeues_latest_after_active_owner_finishes() {
    let queue = InMemoryJobQueue::new("dedup-keep-last");
    let owner = queue
        .add_at(
            "sync-owner",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:keep-last")
                    .with_ttl(Duration::from_secs(30))
                    .keep_last_if_active(true),
            ),
            ts(1_000),
        )
        .await
        .unwrap();
    assert!(owner.deduplication_expires_at.is_none());

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(2_000))
        .await
        .unwrap()
        .unwrap();
    let duplicate = queue
        .add_at(
            "sync-stale",
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:keep-last").keep_last_if_active(true),
            ),
            ts(3_000),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.id, claimed.id);

    let latest_duplicate = queue
        .add_at(
            "sync-latest",
            serde_json::json!({ "version": 3 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(5))
                .with_deduplication(
                    DeduplicationOptions::new("account:keep-last").keep_last_if_active(true),
                ),
            ts(4_000),
        )
        .await
        .unwrap();
    assert_eq!(latest_duplicate.id, claimed.id);

    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            ts(6_000),
        )
        .await
        .unwrap();

    let delayed = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .unwrap();
    assert_eq!(delayed.total, 1);
    assert_eq!(delayed.jobs[0].name, "sync-latest");
    assert_eq!(delayed.jobs[0].payload, serde_json::json!({ "version": 3 }));
    assert_eq!(delayed.jobs[0].scheduled_at, ts(11_000));
    assert!(delayed.jobs[0].deduplication_expires_at.is_none());

    queue.promote_due_jobs(ts(11_000)).await.unwrap();
    let next = queue
        .claim_next("worker-b".to_string(), Duration::from_secs(30), ts(11_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.name, "sync-latest");
}

#[tokio::test]
async fn remove_deduplication_key_clears_keep_last_next() {
    let queue = InMemoryJobQueue::new("dedup-keep-last-release");
    let owner = queue
        .add_at(
            "sync-owner",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:keep-last-release").keep_last_if_active(true),
            ),
            ts(1_000),
        )
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(2_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, owner.id);

    let duplicate = queue
        .add_at(
            "sync-stale-next",
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:keep-last-release").keep_last_if_active(true),
            ),
            ts(3_000),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.id, owner.id);

    assert!(queue
        .remove_deduplication_key("account:keep-last-release")
        .await
        .unwrap());
    let replacement = queue
        .add_at(
            "sync-after-release",
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:keep-last-release").keep_last_if_active(true),
            ),
            ts(4_000),
        )
        .await
        .unwrap();
    assert_ne!(replacement.id, owner.id);

    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            ts(5_000),
        )
        .await
        .unwrap();

    let jobs = queue.list_jobs(JobListOptions::new()).await.unwrap();
    assert!(!jobs.jobs.iter().any(|job| job.name == "sync-stale-next"));
    let claimed_replacement = queue
        .claim_next("worker-b".to_string(), Duration::from_secs(30), ts(6_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed_replacement.id, replacement.id);
    assert_eq!(claimed_replacement.name, "sync-after-release");
}

#[tokio::test]
async fn repeat_deduplication_keep_last_replaces_regular_successor() {
    let queue = InMemoryJobQueue::new("repeat-dedup-keep-last");
    let repeat = RepeatOptions::every(Duration::from_secs(60))
        .with_limit(3)
        .with_key("account-sync");
    let deduplication =
        DeduplicationOptions::new("account:repeat-keep-last").keep_last_if_active(true);
    let owner = queue
        .add_at(
            "sync-owner",
            serde_json::json!({ "version": 1 }),
            JobOptions::new()
                .with_repeat(repeat.clone())
                .with_deduplication(deduplication.clone()),
            ts(1_000),
        )
        .await
        .unwrap();
    assert_eq!(owner.repeat_key.as_deref(), Some("account-sync"));

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(2_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, owner.id);

    let stale_duplicate = queue
        .add_at(
            "sync-stale",
            serde_json::json!({ "version": 2 }),
            JobOptions::new()
                .with_repeat(repeat.clone())
                .with_deduplication(deduplication.clone()),
            ts(3_000),
        )
        .await
        .unwrap();
    assert_eq!(stale_duplicate.id, owner.id);

    let latest_duplicate = queue
        .add_at(
            "sync-latest",
            serde_json::json!({ "version": 3 }),
            JobOptions::new()
                .with_delay(Duration::from_secs(5))
                .with_repeat(repeat)
                .with_deduplication(deduplication),
            ts(4_000),
        )
        .await
        .unwrap();
    assert_eq!(latest_duplicate.id, owner.id);

    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            ts(6_000),
        )
        .await
        .unwrap();

    let delayed = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .unwrap();
    assert_eq!(delayed.total, 1);
    let next = &delayed.jobs[0];
    assert_eq!(next.name, "sync-latest");
    assert_eq!(next.payload, serde_json::json!({ "version": 3 }));
    assert_eq!(next.repeat_key.as_deref(), Some("account-sync"));
    assert_eq!(next.repeat_count, 1);
    assert_eq!(next.scheduled_at, ts(11_000));

    let repeats = queue.list_repeats().await.unwrap();
    assert_eq!(repeats.len(), 1);
    assert_eq!(repeats[0].key, "account-sync");
    assert_eq!(repeats[0].job_id, next.id);
    assert_eq!(repeats[0].repeat_count, 1);
}

#[tokio::test]
async fn retry_resets_deduplication_ttl_owner_window() {
    let queue = InMemoryJobQueue::new("dedup-retry-ttl");
    let first = queue
        .add_at(
            "sync",
            serde_json::json!({}),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:retry-ttl").with_ttl(Duration::from_secs(1)),
            ),
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

    let retried = queue.retry_job(&first.id, ts(3_000)).await.unwrap();
    assert_eq!(retried.state, JobState::Waiting);
    assert_eq!(retried.deduplication_expires_at, Some(ts(4_000)));

    let duplicate = queue
        .add_at(
            "sync-duplicate",
            serde_json::json!({}),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:retry-ttl").with_ttl(Duration::from_secs(1)),
            ),
            ts(3_500),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.id, first.id);
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

    let counts = queue
        .get_counts_per_priority(&[1, 50, 60, 1])
        .await
        .unwrap();
    assert_eq!(
        counts,
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

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, second.id);
    assert_ne!(claimed.id, first.id);
}

#[tokio::test]
async fn priority_updates_can_reinsert_waiting_jobs_as_lifo() {
    let queue = InMemoryJobQueue::new("priority-update-lifo");
    let now = ts(1_000);
    let fifo = queue
        .add_at(
            "fifo",
            serde_json::json!({}),
            JobOptions::new().with_priority(5),
            now,
        )
        .await
        .unwrap();
    let changed = queue
        .add_at(
            "changed",
            serde_json::json!({}),
            JobOptions::new().with_priority(10),
            now,
        )
        .await
        .unwrap();

    let updated = queue
        .update_priority_with_lifo(&changed.id, 5, true)
        .await
        .unwrap();
    assert_eq!(updated.priority, 5);
    assert!(updated.options.lifo);
    assert!(fifo.enqueued_seq < updated.enqueued_seq);

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, changed.id);
}

#[tokio::test]
async fn priority_counts_only_include_waiting_jobs() {
    let queue = InMemoryJobQueue::new("priority-counts");
    let now = ts(1_000);
    queue
        .add_at(
            "waiting-a",
            serde_json::json!({}),
            JobOptions::new().with_priority(5),
            now,
        )
        .await
        .unwrap();
    queue
        .add_at(
            "waiting-b",
            serde_json::json!({}),
            JobOptions::new().with_priority(5),
            now,
        )
        .await
        .unwrap();
    let active = queue
        .add_at(
            "active",
            serde_json::json!({}),
            JobOptions::new().with_priority(1),
            now,
        )
        .await
        .unwrap();
    let _delayed = queue
        .add_at(
            "delayed",
            serde_json::json!({}),
            JobOptions::new()
                .with_priority(5)
                .with_delay(Duration::from_secs(30)),
            now,
        )
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, active.id);

    let counts = queue.get_counts_per_priority(&[5, 1, 9]).await.unwrap();
    assert_eq!(
        counts,
        vec![
            JobPriorityCount {
                priority: 5,
                count: 2,
            },
            JobPriorityCount {
                priority: 1,
                count: 0,
            },
            JobPriorityCount {
                priority: 9,
                count: 0,
            },
        ]
    );
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

#[tokio::test]
async fn list_repeats_returns_current_series_owners() {
    let queue = InMemoryJobQueue::new("repeat-list");
    let now = ts(1_000);
    let slower = queue
        .add_at(
            "slow-heartbeat",
            serde_json::json!({ "target": "crm" }),
            JobOptions::new().with_priority(10).with_repeat(
                RepeatOptions::every(Duration::from_secs(10)).with_key("slow-heartbeat"),
            ),
            now,
        )
        .await
        .unwrap();
    let faster = queue
        .add_at(
            "fast-heartbeat",
            serde_json::json!({ "target": "search" }),
            JobOptions::new().with_priority(1).with_repeat(
                RepeatOptions::every(Duration::from_secs(5)).with_key("fast-heartbeat"),
            ),
            now,
        )
        .await
        .unwrap();

    let repeats = queue.list_repeats().await.unwrap();
    assert_eq!(
        repeats
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["fast-heartbeat", "slow-heartbeat"]
    );
    assert_eq!(repeats[0].job_id, faster.id);
    assert_eq!(repeats[0].name, "fast-heartbeat");
    assert_eq!(repeats[0].state, JobState::Waiting);
    assert_eq!(repeats[0].scheduled_at, now);
    assert_eq!(repeats[0].repeat_count, 0);
    assert_eq!(repeats[0].options.interval(), Some(Duration::from_secs(5)));

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

    let after_successor = queue.list_repeats().await.unwrap();
    let faster_entry = after_successor
        .iter()
        .find(|entry| entry.key == "fast-heartbeat")
        .unwrap();
    assert_ne!(faster_entry.job_id, faster.id);
    assert_eq!(faster_entry.state, JobState::Delayed);
    assert_eq!(faster_entry.repeat_count, 1);

    queue
        .remove_repeat("fast-heartbeat")
        .await
        .unwrap()
        .unwrap();
    let after_remove = queue.list_repeats().await.unwrap();
    assert_eq!(after_remove.len(), 1);
    assert_eq!(after_remove[0].job_id, slower.id);
}

#[tokio::test]
async fn repeat_scheduler_management_paginates_by_next_scheduled_time() {
    let queue = InMemoryJobQueue::new("repeat-page");
    let now = ts(1_000);
    let immediate = queue
        .add_at(
            "immediate",
            serde_json::json!({ "target": "now" }),
            JobOptions::new()
                .with_repeat(RepeatOptions::every(Duration::from_secs(60)).with_key("immediate")),
            now,
        )
        .await
        .unwrap();
    let middle = queue
        .add_at(
            "middle",
            serde_json::json!({ "target": "middle" }),
            JobOptions::new()
                .with_delay(Duration::from_secs(5))
                .with_repeat(RepeatOptions::every(Duration::from_secs(60)).with_key("middle")),
            now,
        )
        .await
        .unwrap();
    let late = queue
        .add_at(
            "late",
            serde_json::json!({ "target": "late" }),
            JobOptions::new()
                .with_delay(Duration::from_secs(10))
                .with_repeat(RepeatOptions::every(Duration::from_secs(60)).with_key("late")),
            now,
        )
        .await
        .unwrap();

    assert_eq!(queue.count_repeats().await.unwrap(), 3);
    assert_eq!(
        queue
            .get_repeat("middle")
            .await
            .unwrap()
            .map(|entry| entry.job_id),
        Some(middle.id.clone())
    );
    assert!(queue.get_repeat("missing").await.unwrap().is_none());

    let descending = queue
        .list_repeats_page(JobRepeatListOptions::new().with_limit(2))
        .await
        .unwrap();
    assert_eq!(descending.total, 3);
    assert_eq!(descending.offset, 0);
    assert_eq!(descending.limit, 2);
    assert_eq!(
        descending
            .repeats
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["late", "middle"]
    );
    assert_eq!(descending.repeats[0].job_id, late.id);

    let ascending = queue
        .list_repeats_page(
            JobRepeatListOptions::new()
                .ascending()
                .with_offset(1)
                .with_limit(1),
        )
        .await
        .unwrap();
    assert_eq!(ascending.total, 3);
    assert_eq!(ascending.repeats[0].key, "middle");
    assert_eq!(ascending.repeats[0].job_id, middle.id);
    assert_eq!(immediate.scheduled_at, now);
}

#[tokio::test]
async fn upsert_repeat_replaces_current_non_active_owner() {
    let queue = InMemoryJobQueue::new("repeat-upsert");
    let now = ts(1_000);
    let first = queue
        .add_at(
            "heartbeat",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(5)).with_key("heartbeat-series"),
            ),
            now,
        )
        .await
        .unwrap();

    let replacement = queue
        .upsert_repeat(
            JobSpec::new("heartbeat-v2", serde_json::json!({ "version": 2 })).with_options(
                JobOptions::new()
                    .with_delay(Duration::from_secs(10))
                    .with_repeat(
                        RepeatOptions::every(Duration::from_secs(30)).with_key("heartbeat-series"),
                    ),
            ),
            ts(1_100),
        )
        .await
        .unwrap();

    assert_ne!(replacement.id, first.id);
    assert_eq!(replacement.name, "heartbeat-v2");
    assert_eq!(replacement.payload, serde_json::json!({ "version": 2 }));
    assert_eq!(replacement.repeat_key.as_deref(), Some("heartbeat-series"));
    assert_eq!(replacement.repeat_count, 0);
    assert_eq!(replacement.state, JobState::Delayed);
    assert!(queue.get_job(&first.id).await.unwrap().is_none());
    assert_eq!(queue.count_repeats().await.unwrap(), 1);
    let entry = queue.get_repeat("heartbeat-series").await.unwrap().unwrap();
    assert_eq!(entry.job_id, replacement.id);
    assert_eq!(entry.name, "heartbeat-v2");
    assert_eq!(entry.options.interval(), Some(Duration::from_secs(30)));
}

#[tokio::test]
async fn upsert_repeat_rejects_active_leased_owner() {
    let queue = InMemoryJobQueue::new("repeat-upsert-active");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "heartbeat",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(5)).with_key("heartbeat-series"),
            ),
            now,
        )
        .await
        .unwrap();

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, job.id);

    let error = queue
        .upsert_repeat(
            JobSpec::new("heartbeat-v2", serde_json::json!({ "version": 2 })).with_options(
                JobOptions::new().with_repeat(
                    RepeatOptions::every(Duration::from_secs(30)).with_key("heartbeat-series"),
                ),
            ),
            ts(1_100),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, LaneError::JobLeaseConflict(_)));
    assert_eq!(
        queue
            .get_repeat("heartbeat-series")
            .await
            .unwrap()
            .unwrap()
            .job_id,
        job.id
    );
    assert_eq!(
        queue.get_job(&job.id).await.unwrap().unwrap().state,
        JobState::Active
    );
}

#[tokio::test]
async fn remove_repeat_removes_current_series_owner_by_key() {
    let queue = InMemoryJobQueue::new("repeat-remove-key");
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
    let successor = delayed
        .jobs
        .iter()
        .find(|job| job.repeat_key.as_deref() == Some("heartbeat-series"))
        .cloned()
        .unwrap();
    assert_eq!(successor.repeat_count, 1);

    let removed = queue
        .remove_repeat("heartbeat-series")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(removed.id, successor.id);
    assert!(queue
        .remove_repeat("heartbeat-series")
        .await
        .unwrap()
        .is_none());

    let new_series = queue
        .add_at(
            "heartbeat-after-remove-repeat",
            serde_json::json!({ "target": "crm", "after": "remove-repeat" }),
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
    assert_ne!(new_series.id, removed.id);
    assert_eq!(new_series.repeat_count, 0);
}

#[tokio::test]
async fn remove_repeat_rejects_active_leased_owner() {
    let queue = InMemoryJobQueue::new("repeat-remove-active");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "heartbeat",
            serde_json::json!({ "target": "crm" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(5)).with_key("heartbeat-series"),
            ),
            now,
        )
        .await
        .unwrap();

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, job.id);

    let error = queue.remove_repeat("heartbeat-series").await.unwrap_err();
    assert!(matches!(error, LaneError::JobLeaseConflict(_)));
    assert_eq!(
        queue.get_job(&job.id).await.unwrap().unwrap().state,
        JobState::Active
    );
}

#[tokio::test]
async fn drain_jobs_removes_waiting_and_optional_delayed_jobs() {
    let queue = InMemoryJobQueue::new("drain");
    let now = ts(1_000);

    let repeat = queue
        .add_at(
            "repeat",
            serde_json::json!({ "kind": "repeat" }),
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .with_limit(2)
                    .with_key("heartbeat"),
            ),
            now,
        )
        .await
        .unwrap();
    let repeat_claim = queue
        .claim_next("worker-repeat".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeat_claim.id, repeat.id);
    queue
        .complete_job(
            &repeat_claim.id,
            lock_token(&repeat_claim),
            serde_json::json!({ "ok": true }),
            ts(1_100),
        )
        .await
        .unwrap();
    let repeat_successor = queue
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .unwrap()
        .jobs
        .into_iter()
        .find(|job| job.repeat_key.as_deref() == Some("heartbeat"))
        .unwrap();

    let completed = queue
        .add_at(
            "completed",
            serde_json::json!({ "kind": "completed" }),
            JobOptions::new().with_priority(1),
            ts(1_200),
        )
        .await
        .unwrap();
    let completed_claim = queue
        .claim_next(
            "worker-completed".to_string(),
            Duration::from_secs(30),
            ts(1_200),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed_claim.id, completed.id);
    queue
        .complete_job(
            &completed_claim.id,
            lock_token(&completed_claim),
            serde_json::json!({ "ok": true }),
            ts(1_250),
        )
        .await
        .unwrap();

    let active = queue
        .add_at(
            "active",
            serde_json::json!({ "kind": "active" }),
            JobOptions::new().with_priority(1),
            ts(1_300),
        )
        .await
        .unwrap();
    let active_claim = queue
        .claim_next(
            "worker-active".to_string(),
            Duration::from_secs(30),
            ts(1_300),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active_claim.id, active.id);

    let waiting = queue
        .add_at(
            "waiting",
            serde_json::json!({ "kind": "waiting" }),
            JobOptions::new().with_priority(50),
            ts(1_400),
        )
        .await
        .unwrap();
    let delayed = queue
        .add_at(
            "delayed",
            serde_json::json!({ "kind": "delayed" }),
            JobOptions::new().with_delay(Duration::from_secs(60)),
            ts(1_400),
        )
        .await
        .unwrap();

    let drained_waiting = queue.drain_jobs(false).await.unwrap();
    assert_eq!(drained_waiting.len(), 1);
    assert_eq!(drained_waiting[0].id, waiting.id);
    assert!(queue.get_job(&waiting.id).await.unwrap().is_none());
    assert!(queue.get_job(&delayed.id).await.unwrap().is_some());
    assert!(queue.get_job(&repeat_successor.id).await.unwrap().is_some());
    assert_eq!(
        queue.get_job(&active.id).await.unwrap().unwrap().state,
        JobState::Active
    );
    assert_eq!(
        queue.get_job(&completed.id).await.unwrap().unwrap().state,
        JobState::Completed
    );

    let drained_delayed = queue.drain_jobs(true).await.unwrap();
    assert_eq!(drained_delayed.len(), 1);
    assert_eq!(drained_delayed[0].id, delayed.id);
    assert!(queue.get_job(&delayed.id).await.unwrap().is_none());
    assert_eq!(
        queue
            .get_job(&repeat_successor.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        JobState::Delayed
    );
}

#[tokio::test]
async fn drain_jobs_releases_flow_parents_after_removing_children() {
    let queue = InMemoryJobQueue::new("drain-flow");
    let now = ts(1_000);
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "parent" })),
            vec![JobSpec::new(
                "child",
                serde_json::json!({ "kind": "child" }),
            )],
            now,
        )
        .await
        .unwrap();
    assert_eq!(flow.parent.state, JobState::WaitingChildren);
    assert_eq!(flow.children[0].state, JobState::Waiting);

    let drained = queue.drain_jobs(false).await.unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].id, flow.children[0].id);
    assert!(queue.get_job(&flow.children[0].id).await.unwrap().is_none());
    assert_eq!(
        queue.get_job(&flow.parent.id).await.unwrap().unwrap().state,
        JobState::Waiting
    );
}

#[tokio::test]
async fn obliterate_pauses_on_active_conflict_and_force_clears_everything() {
    let queue = InMemoryJobQueue::new("obliterate");
    let now = ts(1_000);

    let active = queue
        .add_at(
            "active",
            serde_json::json!({ "kind": "active" }),
            JobOptions::new().with_priority(1),
            now,
        )
        .await
        .unwrap();
    let active_claim = queue
        .claim_next("worker-active".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active_claim.id, active.id);

    let completed = queue
        .add_at(
            "completed",
            serde_json::json!({ "kind": "completed" }),
            JobOptions::new().with_priority(1),
            ts(1_100),
        )
        .await
        .unwrap();
    let completed_claim = queue
        .claim_next(
            "worker-completed".to_string(),
            Duration::from_secs(30),
            ts(1_100),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed_claim.id, completed.id);
    queue
        .complete_job(
            &completed.id,
            lock_token(&completed_claim),
            serde_json::json!({ "ok": true }),
            ts(1_150),
        )
        .await
        .unwrap();

    let failed = queue
        .add_at(
            "failed",
            serde_json::json!({ "kind": "failed" }),
            JobOptions::new().with_priority(1),
            ts(1_200),
        )
        .await
        .unwrap();
    let failed_claim = queue
        .claim_next(
            "worker-failed".to_string(),
            Duration::from_secs(30),
            ts(1_200),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed_claim.id, failed.id);
    queue
        .fail_job(
            &failed.id,
            lock_token(&failed_claim),
            "boom".to_string(),
            ts(1_250),
        )
        .await
        .unwrap();

    let waiting = queue
        .add_at(
            "waiting",
            serde_json::json!({ "kind": "waiting" }),
            JobOptions::new()
                .with_priority(50)
                .with_deduplication_id("tenant:one"),
            ts(1_300),
        )
        .await
        .unwrap();
    let duplicate_waiting = queue
        .add_at(
            "waiting-duplicate",
            serde_json::json!({ "kind": "duplicate" }),
            JobOptions::new().with_deduplication_id("tenant:one"),
            ts(1_310),
        )
        .await
        .unwrap();
    assert_eq!(duplicate_waiting.id, waiting.id);
    queue
        .add_log(&waiting.id, "queued".to_string(), 10, ts(1_320))
        .await
        .unwrap();

    let delayed = queue
        .add_at(
            "delayed",
            serde_json::json!({ "kind": "delayed" }),
            JobOptions::new().with_delay(Duration::from_secs(60)),
            ts(1_300),
        )
        .await
        .unwrap();

    let keep_owner = queue
        .add_at(
            "keep-owner",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_priority(2).with_deduplication(
                DeduplicationOptions::new("tenant:keep").keep_last_if_active(true),
            ),
            ts(1_400),
        )
        .await
        .unwrap();
    let keep_claim = queue
        .claim_next(
            "worker-keep".to_string(),
            Duration::from_secs(30),
            ts(1_400),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(keep_claim.id, keep_owner.id);
    let keep_duplicate = queue
        .add_at(
            "keep-duplicate",
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("tenant:keep").keep_last_if_active(true),
            ),
            ts(1_410),
        )
        .await
        .unwrap();
    assert_eq!(keep_duplicate.id, keep_owner.id);

    let before = queue.stats().await.unwrap();
    assert_eq!(before.total, 6);
    assert_eq!(before.active, 2);

    let error = queue.obliterate(false).await.unwrap_err();
    assert!(matches!(error, LaneError::JobStateConflict(_)));
    assert!(queue.is_paused().await.unwrap());
    assert!(queue
        .claim_next(
            "worker-paused".to_string(),
            Duration::from_secs(30),
            ts(1_500)
        )
        .await
        .unwrap()
        .is_none());

    let removed = queue.obliterate(true).await.unwrap();
    assert_eq!(removed, before.total);
    assert!(!queue.is_paused().await.unwrap());

    let stats = queue.stats().await.unwrap();
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
        assert!(queue.get_job(&job.id).await.unwrap().is_none());
    }
    assert!(queue.list_repeats().await.unwrap().is_empty());
    assert!(queue
        .get_deduplication_job_id("tenant:one")
        .await
        .unwrap()
        .is_none());
    let logs = queue.get_job_logs(&waiting.id, 0, -1, true).await.unwrap();
    assert_eq!(logs.count, 0);
    assert!(logs.logs.is_empty());

    let after = queue
        .add_at(
            "after",
            serde_json::json!({ "kind": "after" }),
            JobOptions::new().with_deduplication_id("tenant:one"),
            ts(1_600),
        )
        .await
        .unwrap();
    assert_ne!(after.id, waiting.id);
    assert!(queue
        .claim_next(
            "worker-after".to_string(),
            Duration::from_secs(30),
            ts(1_600)
        )
        .await
        .unwrap()
        .is_some());
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
async fn failed_jobs_can_discard_configured_retry() {
    let queue = InMemoryJobQueue::new("webhooks-discard");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "deliver",
            serde_json::json!({}),
            JobOptions::new().with_retry_policy(RetryPolicy::fixed(3, Duration::from_secs(2))),
            now,
        )
        .await
        .unwrap();

    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();

    let failed = queue
        .fail_job_discarding_retry(
            &job.id,
            lock_token(&claimed),
            "unrecoverable".to_string(),
            ts(1_100),
        )
        .await
        .unwrap();

    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.attempts_made, 1);
    assert_eq!(failed.finished_at, Some(ts(1_100)));
    assert_eq!(failed.scheduled_at, now);
    assert_eq!(failed.failed_reason.as_deref(), Some("unrecoverable"));
    let stats = queue.stats().await.unwrap();
    assert_eq!(stats.delayed, 0);
    assert_eq!(stats.failed, 1);
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
async fn flow_parent_ignores_configured_stalled_child_terminal_failure() {
    let queue = InMemoryJobQueue::new("flow-ignore-stalled");
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("optional-child", serde_json::json!({ "optional": true }))
                    .with_options(
                        JobOptions::new()
                            .with_priority(1)
                            .with_max_stalled_count(0)
                            .with_ignore_dependency_on_failure(true),
                    ),
            ],
            ts(1_000),
        )
        .await
        .unwrap();
    let child = queue
        .claim_next(
            "worker-stalled".to_string(),
            Duration::from_millis(50),
            ts(1_100),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.id, flow.children[0].id);

    assert_eq!(queue.recover_stalled_jobs(ts(1_200)).await.unwrap(), 1);
    let failed_child = queue.get_job(&child.id).await.unwrap().unwrap();
    assert_eq!(failed_child.state, JobState::Failed);
    let parent = queue.get_job(&flow.parent.id).await.unwrap().unwrap();
    assert_eq!(parent.state, JobState::Waiting);
    assert!(parent.failed_reason.is_none());
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 0,
            unprocessed: 0,
            failed: 0,
            ignored: 1,
            missing: 0,
        }
    );
}

#[tokio::test]
async fn flow_parent_continues_configured_stalled_child_terminal_failure() {
    let queue = InMemoryJobQueue::new("flow-continue-stalled");
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("optional-child", serde_json::json!({ "optional": true }))
                    .with_options(
                        JobOptions::new()
                            .with_priority(1)
                            .with_max_stalled_count(0)
                            .with_continue_parent_on_failure(true),
                    ),
            ],
            ts(1_000),
        )
        .await
        .unwrap();
    let child = queue
        .claim_next(
            "worker-stalled".to_string(),
            Duration::from_millis(50),
            ts(1_100),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.id, flow.children[0].id);

    assert_eq!(queue.recover_stalled_jobs(ts(1_200)).await.unwrap(), 1);
    let failed_child = queue.get_job(&child.id).await.unwrap().unwrap();
    assert_eq!(failed_child.state, JobState::Failed);
    let parent = queue.get_job(&flow.parent.id).await.unwrap().unwrap();
    assert_eq!(parent.state, JobState::Waiting);
    assert!(parent.failed_reason.is_none());
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 0,
            unprocessed: 0,
            failed: 0,
            ignored: 1,
            missing: 0,
        }
    );
}

#[tokio::test]
async fn flow_parent_removes_configured_stalled_child_terminal_dependency() {
    let queue = InMemoryJobQueue::new("flow-remove-stalled");
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("optional-child", serde_json::json!({ "optional": true }))
                    .with_options(
                        JobOptions::new()
                            .with_priority(1)
                            .with_max_stalled_count(0)
                            .with_remove_dependency_on_failure(true),
                    ),
            ],
            ts(1_000),
        )
        .await
        .unwrap();
    let child = queue
        .claim_next(
            "worker-stalled".to_string(),
            Duration::from_millis(50),
            ts(1_100),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.id, flow.children[0].id);

    assert_eq!(queue.recover_stalled_jobs(ts(1_200)).await.unwrap(), 1);
    let failed_child = queue.get_job(&child.id).await.unwrap().unwrap();
    assert_eq!(failed_child.state, JobState::Failed);
    let parent = queue.get_job(&flow.parent.id).await.unwrap().unwrap();
    assert_eq!(parent.state, JobState::Waiting);
    assert!(parent.failed_reason.is_none());
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 0,
            unprocessed: 0,
            failed: 0,
            ignored: 0,
            missing: 0,
        }
    );
}

#[tokio::test]
async fn repeat_stalled_jobs_are_requeued_after_limit() {
    let queue = InMemoryJobQueue::new("repeat-stalled");
    let now = ts(1_000);
    let job = queue
        .add_at(
            "heartbeat",
            serde_json::json!({}),
            JobOptions::new().with_max_stalled_count(0).with_repeat(
                RepeatOptions::every(Duration::from_secs(60))
                    .with_limit(2)
                    .with_key("heartbeat"),
            ),
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
    assert_eq!(recovered.repeat_key.as_deref(), Some("heartbeat"));

    let repeats = queue.list_repeats().await.unwrap();
    assert_eq!(repeats.len(), 1);
    assert_eq!(repeats[0].job_id, job.id);
    assert_eq!(repeats[0].state, JobState::Waiting);
}

#[tokio::test]
async fn pause_blocks_claiming_without_rejecting_adds() {
    let queue = InMemoryJobQueue::new("paused");
    let now = ts(1_000);
    assert!(!queue.is_paused().await.unwrap());
    queue.pause().await.unwrap();
    assert!(queue.is_paused().await.unwrap());
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
    assert!(!queue.is_paused().await.unwrap());
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
async fn completed_job_retention_keeps_newest_count() {
    let queue = InMemoryJobQueue::new("completion-retention-count");
    let mut ids = Vec::new();

    for index in 0..3 {
        let now = ts(1_000 + index * 100);
        let job = queue
            .add_at(
                format!("task-{index}"),
                serde_json::json!({ "index": index }),
                JobOptions::new().with_completion_retention(JobRetention::count(2)),
                now,
            )
            .await
            .unwrap();
        ids.push(job.id.clone());
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
                ts(1_100 + index * 100),
            )
            .await
            .unwrap();
    }

    assert!(queue.get_job(&ids[0]).await.unwrap().is_none());
    assert_eq!(
        queue.get_job(&ids[1]).await.unwrap().unwrap().state,
        JobState::Completed
    );
    assert_eq!(
        queue.get_job(&ids[2]).await.unwrap().unwrap().state,
        JobState::Completed
    );
    assert_eq!(queue.stats().await.unwrap().completed, 2);
}

#[tokio::test]
async fn completed_job_retention_applies_age_when_another_job_finishes() {
    let queue = InMemoryJobQueue::new("completion-retention-age");
    let first = queue
        .add_at(
            "first",
            serde_json::json!({}),
            JobOptions::new().with_completion_retention(JobRetention::age(Duration::from_secs(1))),
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
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({}),
            ts(1_100),
        )
        .await
        .unwrap();

    assert!(queue.get_job(&first.id).await.unwrap().is_some());

    let second = queue
        .add_at(
            "second",
            serde_json::json!({}),
            JobOptions::new().with_completion_retention(JobRetention::age(Duration::from_secs(1))),
            ts(2_500),
        )
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(2_500))
        .await
        .unwrap()
        .unwrap();
    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({}),
            ts(2_500),
        )
        .await
        .unwrap();

    assert!(queue.get_job(&first.id).await.unwrap().is_none());
    assert_eq!(
        queue.get_job(&second.id).await.unwrap().unwrap().state,
        JobState::Completed
    );
}

#[tokio::test]
async fn failed_job_retention_keeps_newest_count() {
    let queue = InMemoryJobQueue::new("failure-retention-count");
    let mut ids = Vec::new();

    for index in 0..3 {
        let now = ts(1_000 + index * 100);
        let job = queue
            .add_at(
                format!("task-{index}"),
                serde_json::json!({ "index": index }),
                JobOptions::new().with_failure_retention(JobRetention::count(1)),
                now,
            )
            .await
            .unwrap();
        ids.push(job.id.clone());
        let claimed = queue
            .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
            .await
            .unwrap()
            .unwrap();
        queue
            .fail_job(
                &claimed.id,
                lock_token(&claimed),
                "boom".to_string(),
                ts(1_100 + index * 100),
            )
            .await
            .unwrap();
    }

    assert!(queue.get_job(&ids[0]).await.unwrap().is_none());
    assert!(queue.get_job(&ids[1]).await.unwrap().is_none());
    assert_eq!(
        queue.get_job(&ids[2]).await.unwrap().unwrap().state,
        JobState::Failed
    );
    assert_eq!(queue.stats().await.unwrap().failed, 1);
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

    let multi_state_page = queue
        .list_jobs(
            JobListOptions::new()
                .with_states([JobState::Waiting, JobState::Delayed, JobState::Waiting])
                .with_limit(3),
        )
        .await
        .unwrap();
    assert_eq!(multi_state_page.total, 3);
    assert_eq!(
        multi_state_page
            .jobs
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>(),
        vec![faster.id.as_str(), slower.id.as_str(), delayed.id.as_str()]
    );

    let descending_page = queue
        .list_jobs(
            JobListOptions::new()
                .with_states([JobState::Waiting, JobState::Delayed])
                .descending()
                .with_offset(1)
                .with_limit(2),
        )
        .await
        .unwrap();
    assert_eq!(descending_page.total, 3);
    assert_eq!(
        descending_page
            .jobs
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>(),
        vec![slower.id.as_str(), faster.id.as_str()]
    );

    let updated_data = queue
        .update_data(
            &slower.id,
            serde_json::json!({ "stage": "normalized", "attempt": 1 }),
        )
        .await
        .unwrap();
    assert_eq!(
        updated_data.payload,
        serde_json::json!({ "stage": "normalized", "attempt": 1 })
    );
    assert_eq!(
        queue.get_job(&slower.id).await.unwrap().unwrap().payload,
        serde_json::json!({ "stage": "normalized", "attempt": 1 })
    );
    let missing_data_update = queue
        .update_data("missing-data-job", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(missing_data_update, LaneError::JobNotFound(_)));

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
    let ascending_logs = queue.get_job_logs(&slower.id, 0, -1, true).await.unwrap();
    assert_eq!(ascending_logs.count, 2);
    assert_eq!(
        ascending_logs
            .logs
            .iter()
            .map(|entry| entry.line.as_str())
            .collect::<Vec<_>>(),
        vec!["second", "third"]
    );
    let newest_log = queue.get_job_logs(&slower.id, 0, 0, false).await.unwrap();
    assert_eq!(newest_log.count, 2);
    assert_eq!(newest_log.logs[0].line, "third");
    let kept_logs = queue.clear_job_logs(&slower.id, 1).await.unwrap();
    assert_eq!(kept_logs.count, 1);
    assert_eq!(kept_logs.logs[0].line, "third");
    assert_eq!(
        queue
            .get_job(&slower.id)
            .await
            .unwrap()
            .expect("slower job should remain stored")
            .logs
            .len(),
        1
    );
    let cleared_logs = queue.clear_job_logs(&slower.id, 0).await.unwrap();
    assert_eq!(cleared_logs.count, 0);
    assert!(cleared_logs.logs.is_empty());
    let missing_clear = queue.clear_job_logs("missing-log-job", 10).await.unwrap();
    assert_eq!(missing_clear.count, 0);
    assert!(missing_clear.logs.is_empty());

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
    let terminal_data = queue
        .update_data(
            &faster.id,
            serde_json::json!({ "stage": "archived", "terminal": true }),
        )
        .await
        .unwrap();
    assert_eq!(
        terminal_data.payload,
        serde_json::json!({ "stage": "archived", "terminal": true })
    );

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
    queue.remove_job(&slower.id).await.unwrap().unwrap();
    let removed_logs = queue.get_job_logs(&slower.id, 0, -1, true).await.unwrap();
    assert_eq!(removed_logs.count, 0);
    assert!(removed_logs.logs.is_empty());
}

#[tokio::test]
async fn flow_parent_waits_for_children_before_claiming() {
    let queue = InMemoryJobQueue::new("flow");
    let now = ts(1_000);
    assert!(queue
        .get_flow_dependencies("missing-parent")
        .await
        .unwrap()
        .is_none());
    assert!(queue
        .get_flow_dependency_counts("missing-parent")
        .await
        .unwrap()
        .is_none());
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
    let dependencies = queue
        .get_flow_dependencies(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dependencies.parent.id, flow.parent.id);
    assert_eq!(
        dependencies
            .children
            .iter()
            .map(|child| child.id.as_str())
            .collect::<Vec<_>>(),
        vec![flow.children[0].id.as_str(), flow.children[1].id.as_str()]
    );
    assert_eq!(dependencies.pending_child_ids, flow.parent.child_ids);
    assert!(dependencies.missing_child_ids.is_empty());
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 0,
            unprocessed: 2,
            failed: 0,
            ignored: 0,
            missing: 0,
        }
    );

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
    let dependencies_after_first = queue
        .get_flow_dependencies(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        dependencies_after_first.pending_child_ids,
        vec![flow.children[1].id.clone()]
    );
    assert!(dependencies_after_first.missing_child_ids.is_empty());
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 1,
            unprocessed: 1,
            failed: 0,
            ignored: 0,
            missing: 0,
        }
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
    let dependencies_after_release = queue
        .get_flow_dependencies(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert!(dependencies_after_release.pending_child_ids.is_empty());
    assert!(dependencies_after_release.missing_child_ids.is_empty());
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 2,
            unprocessed: 0,
            failed: 0,
            ignored: 0,
            missing: 0,
        }
    );

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
    let dependencies = queue
        .get_flow_dependencies(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert!(dependencies.pending_child_ids.is_empty());
    assert_eq!(
        dependencies.missing_child_ids,
        vec![flow.children[1].id.clone()]
    );
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 1,
            unprocessed: 0,
            failed: 0,
            ignored: 0,
            missing: 1,
        }
    );
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
async fn flow_remove_unprocessed_children_skips_active_children() {
    let queue = InMemoryJobQueue::new("flow-remove-unprocessed");
    assert!(queue
        .remove_unprocessed_children("missing-parent", ts(1_000))
        .await
        .unwrap()
        .is_none());
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("child-a", serde_json::json!({ "n": 1 }))
                    .with_options(JobOptions::new().with_priority(1)),
                JobSpec::new("child-b", serde_json::json!({ "n": 2 }))
                    .with_options(JobOptions::new().with_priority(2)),
                JobSpec::new("child-c", serde_json::json!({ "n": 3 }))
                    .with_options(JobOptions::new().with_priority(3)),
            ],
            ts(1_000),
        )
        .await
        .unwrap();

    let completed_child = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed_child.id, flow.children[0].id);
    queue
        .complete_job(
            &completed_child.id,
            lock_token(&completed_child),
            serde_json::json!({ "ok": 1 }),
            ts(1_150),
        )
        .await
        .unwrap();
    let active_child = queue
        .claim_next("worker-b".to_string(), Duration::from_secs(30), ts(1_200))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active_child.id, flow.children[1].id);

    let removed = queue
        .remove_unprocessed_children(&flow.parent.id, ts(1_250))
        .await
        .unwrap()
        .expect("parent should exist");
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].id, flow.children[2].id);
    assert!(queue.get_job(&flow.children[2].id).await.unwrap().is_none());
    assert_eq!(
        queue
            .get_job(&flow.children[1].id)
            .await
            .unwrap()
            .unwrap()
            .state,
        JobState::Active
    );
    assert_eq!(
        queue.get_job(&flow.parent.id).await.unwrap().unwrap().state,
        JobState::WaitingChildren
    );
    let counts_after_remove = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        counts_after_remove,
        JobFlowDependencyCounts {
            processed: 1,
            unprocessed: 1,
            failed: 0,
            ignored: 0,
            missing: 1,
        }
    );

    queue
        .complete_job(
            &active_child.id,
            lock_token(&active_child),
            serde_json::json!({ "ok": 2 }),
            ts(1_300),
        )
        .await
        .unwrap();
    let parent = queue.get_job(&flow.parent.id).await.unwrap().unwrap();
    assert_eq!(parent.state, JobState::Waiting);
    let dependencies = queue
        .get_flow_dependencies(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert!(dependencies.pending_child_ids.is_empty());
    assert_eq!(
        dependencies.missing_child_ids,
        vec![flow.children[2].id.clone()]
    );
}

#[tokio::test]
async fn flow_remove_child_dependency_releases_parent_without_deleting_child() {
    let queue = InMemoryJobQueue::new("flow-remove-child-dependency");
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![JobSpec::new("child", serde_json::json!({ "n": 1 }))
                .with_options(JobOptions::new().with_priority(10))],
            ts(1_000),
        )
        .await
        .unwrap();

    assert!(queue
        .remove_child_dependency(&flow.children[0].id, ts(1_100))
        .await
        .unwrap());
    assert!(!queue
        .remove_child_dependency(&flow.children[0].id, ts(1_200))
        .await
        .unwrap());

    let parent = queue
        .get_job(&flow.parent.id)
        .await
        .unwrap()
        .expect("parent should remain stored");
    assert_eq!(parent.state, JobState::Waiting);
    assert!(parent.child_ids.is_empty());

    let child = queue
        .get_job(&flow.children[0].id)
        .await
        .unwrap()
        .expect("child should remain stored");
    assert_eq!(child.state, JobState::Waiting);
    assert!(child.parent_id.is_none());

    let dependencies = queue
        .get_flow_dependencies(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert!(dependencies.children.is_empty());
    assert!(dependencies.pending_child_ids.is_empty());
    assert!(dependencies.missing_child_ids.is_empty());

    assert!(matches!(
        queue
            .remove_child_dependency("missing-child", ts(1_300))
            .await
            .unwrap_err(),
        LaneError::JobNotFound(_)
    ));
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
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 0,
            unprocessed: 0,
            failed: 1,
            ignored: 0,
            missing: 0,
        }
    );
}

#[tokio::test]
async fn flow_parent_ignores_configured_child_terminal_failure() {
    let queue = InMemoryJobQueue::new("flow-ignore-failure");
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("optional-child", serde_json::json!({ "optional": true }))
                    .with_options(
                        JobOptions::new()
                            .with_priority(1)
                            .with_ignore_dependency_on_failure(true),
                    ),
                JobSpec::new("required-child", serde_json::json!({ "required": true }))
                    .with_options(JobOptions::new().with_priority(2)),
            ],
            ts(1_000),
        )
        .await
        .unwrap();

    let optional_child = queue
        .claim_next(
            "worker-optional".to_string(),
            Duration::from_secs(30),
            ts(1_100),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(optional_child.id, flow.children[0].id);
    queue
        .fail_job(
            &optional_child.id,
            lock_token(&optional_child),
            "optional source failed".to_string(),
            ts(1_200),
        )
        .await
        .unwrap();

    let parent = queue.get_job(&flow.parent.id).await.unwrap().unwrap();
    assert_eq!(parent.state, JobState::WaitingChildren);
    assert!(parent.failed_reason.is_none());
    let counts = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        counts,
        JobFlowDependencyCounts {
            processed: 0,
            unprocessed: 1,
            failed: 0,
            ignored: 1,
            missing: 0,
        }
    );
    let ignored_failures = queue
        .get_flow_ignored_children_failures(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ignored_failures.len(), 1);
    assert_eq!(
        ignored_failures
            .get(&flow.children[0].id)
            .map(String::as_str),
        Some("optional source failed")
    );
    let dependencies = queue
        .get_flow_dependencies(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        dependencies.pending_child_ids,
        vec![flow.children[1].id.clone()]
    );

    let required_child = queue
        .claim_next(
            "worker-required".to_string(),
            Duration::from_secs(30),
            ts(1_300),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(required_child.id, flow.children[1].id);
    queue
        .complete_job(
            &required_child.id,
            lock_token(&required_child),
            serde_json::json!({ "ok": true }),
            ts(1_400),
        )
        .await
        .unwrap();

    let parent = queue.get_job(&flow.parent.id).await.unwrap().unwrap();
    assert_eq!(parent.state, JobState::Waiting);
    let counts = queue
        .get_flow_dependency_counts(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        counts,
        JobFlowDependencyCounts {
            processed: 1,
            unprocessed: 0,
            failed: 0,
            ignored: 1,
            missing: 0,
        }
    );
    let child_values = queue
        .get_flow_children_values(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child_values.len(), 1);
    assert_eq!(
        child_values.get(&flow.children[1].id),
        Some(&serde_json::json!({ "ok": true }))
    );
}

#[tokio::test]
async fn flow_parent_continues_configured_child_terminal_failure() {
    let queue = InMemoryJobQueue::new("flow-continue-failure");
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("optional-child", serde_json::json!({ "optional": true }))
                    .with_options(
                        JobOptions::new()
                            .with_priority(1)
                            .with_continue_parent_on_failure(true),
                    ),
                JobSpec::new("required-child", serde_json::json!({ "required": true }))
                    .with_options(JobOptions::new().with_priority(2)),
            ],
            ts(1_000),
        )
        .await
        .unwrap();

    let optional_child = queue
        .claim_next(
            "worker-optional".to_string(),
            Duration::from_secs(30),
            ts(1_100),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(optional_child.id, flow.children[0].id);
    queue
        .fail_job(
            &optional_child.id,
            lock_token(&optional_child),
            "optional source failed".to_string(),
            ts(1_200),
        )
        .await
        .unwrap();

    let parent = queue.get_job(&flow.parent.id).await.unwrap().unwrap();
    assert_eq!(parent.state, JobState::Waiting);
    assert!(parent.failed_reason.is_none());
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 0,
            unprocessed: 1,
            failed: 0,
            ignored: 1,
            missing: 0,
        }
    );
    let ignored_failures = queue
        .get_flow_ignored_children_failures(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ignored_failures
            .get(&flow.children[0].id)
            .map(String::as_str),
        Some("optional source failed")
    );

    let continued_parent = queue
        .claim_next(
            "worker-parent".to_string(),
            Duration::from_secs(30),
            ts(1_300),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(continued_parent.id, flow.parent.id);

    let complete_error = queue
        .complete_job(
            &continued_parent.id,
            lock_token(&continued_parent),
            serde_json::json!({ "early": true }),
            ts(1_350),
        )
        .await
        .unwrap_err();
    assert!(matches!(complete_error, LaneError::JobStateConflict(_)));

    let required_child = queue
        .claim_next(
            "worker-required".to_string(),
            Duration::from_secs(30),
            ts(1_400),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(required_child.id, flow.children[1].id);
    queue
        .complete_job(
            &required_child.id,
            lock_token(&required_child),
            serde_json::json!({ "ok": true }),
            ts(1_500),
        )
        .await
        .unwrap();

    let completed_parent = queue
        .complete_job(
            &continued_parent.id,
            lock_token(&continued_parent),
            serde_json::json!({ "done": true }),
            ts(1_600),
        )
        .await
        .unwrap();
    assert_eq!(completed_parent.state, JobState::Completed);
    assert_eq!(
        completed_parent.return_value,
        Some(serde_json::json!({ "done": true }))
    );
}

#[tokio::test]
async fn flow_parent_removes_configured_child_terminal_dependency() {
    let queue = InMemoryJobQueue::new("flow-remove-failure");
    let flow = queue
        .add_flow_at(
            JobSpec::new("parent", serde_json::json!({ "kind": "aggregate" }))
                .with_options(JobOptions::new().with_priority(1)),
            vec![
                JobSpec::new("optional-child", serde_json::json!({ "optional": true }))
                    .with_options(
                        JobOptions::new()
                            .with_priority(1)
                            .with_remove_dependency_on_failure(true),
                    ),
                JobSpec::new("required-child", serde_json::json!({ "required": true }))
                    .with_options(JobOptions::new().with_priority(2)),
            ],
            ts(1_000),
        )
        .await
        .unwrap();

    let optional_child = queue
        .claim_next(
            "worker-optional".to_string(),
            Duration::from_secs(30),
            ts(1_100),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(optional_child.id, flow.children[0].id);
    queue
        .fail_job(
            &optional_child.id,
            lock_token(&optional_child),
            "optional source failed".to_string(),
            ts(1_200),
        )
        .await
        .unwrap();

    let parent = queue.get_job(&flow.parent.id).await.unwrap().unwrap();
    assert_eq!(parent.state, JobState::WaitingChildren);
    assert!(parent.failed_reason.is_none());
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 0,
            unprocessed: 1,
            failed: 0,
            ignored: 0,
            missing: 0,
        }
    );
    let ignored_failures = queue
        .get_flow_ignored_children_failures(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert!(ignored_failures.is_empty());

    let required_child = queue
        .claim_next(
            "worker-required".to_string(),
            Duration::from_secs(30),
            ts(1_300),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(required_child.id, flow.children[1].id);
    queue
        .complete_job(
            &required_child.id,
            lock_token(&required_child),
            serde_json::json!({ "ok": true }),
            ts(1_400),
        )
        .await
        .unwrap();

    let parent = queue.get_job(&flow.parent.id).await.unwrap().unwrap();
    assert_eq!(parent.state, JobState::Waiting);
    assert_eq!(
        queue
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 1,
            unprocessed: 0,
            failed: 0,
            ignored: 0,
            missing: 0,
        }
    );
    let child_values = queue
        .get_flow_children_values(&flow.parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child_values.len(), 1);
    assert_eq!(
        child_values.get(&flow.children[1].id),
        Some(&serde_json::json!({ "ok": true }))
    );
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
    let dependencies = reopened
        .get_flow_dependencies(&flow.parent.id)
        .await
        .unwrap()
        .expect("dependencies should be restored");
    assert_eq!(dependencies.parent.id, flow.parent.id);
    assert_eq!(dependencies.children.len(), 1);
    assert_eq!(dependencies.children[0].id, child.id);
    assert_eq!(dependencies.pending_child_ids, vec![child.id.clone()]);
    assert!(dependencies.missing_child_ids.is_empty());
    assert_eq!(
        reopened
            .get_flow_dependency_counts(&flow.parent.id)
            .await
            .unwrap()
            .unwrap(),
        JobFlowDependencyCounts {
            processed: 0,
            unprocessed: 1,
            failed: 0,
            ignored: 0,
            missing: 0,
        }
    );
}

#[tokio::test]
async fn local_job_queue_persists_removed_unprocessed_children() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir
        .path()
        .join("jobs")
        .join("remove-unprocessed-flow.json");
    let queue = LocalJobQueue::open("durable-remove-unprocessed-flow", &snapshot_path)
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

    let removed = queue
        .remove_unprocessed_children(&flow.parent.id, ts(1_100))
        .await
        .unwrap()
        .expect("parent should exist");
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].id, flow.children[0].id);

    let reopened = LocalJobQueue::open("durable-remove-unprocessed-flow", &snapshot_path)
        .await
        .unwrap();
    assert!(reopened
        .get_job(&flow.children[0].id)
        .await
        .unwrap()
        .is_none());
    let parent = reopened
        .get_job(&flow.parent.id)
        .await
        .unwrap()
        .expect("parent should be restored");
    assert_eq!(parent.state, JobState::Waiting);
    let dependencies = reopened
        .get_flow_dependencies(&flow.parent.id)
        .await
        .unwrap()
        .expect("dependencies should be restored");
    assert!(dependencies.pending_child_ids.is_empty());
    assert_eq!(
        dependencies.missing_child_ids,
        vec![flow.children[0].id.clone()]
    );
}

#[tokio::test]
async fn local_job_queue_persists_removed_child_dependency() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir
        .path()
        .join("jobs")
        .join("remove-child-dependency-flow.json");
    let queue = LocalJobQueue::open("durable-remove-child-dependency-flow", &snapshot_path)
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

    assert!(queue
        .remove_child_dependency(&flow.children[0].id, ts(1_100))
        .await
        .unwrap());

    let reopened = LocalJobQueue::open("durable-remove-child-dependency-flow", &snapshot_path)
        .await
        .unwrap();
    let parent = reopened
        .get_job(&flow.parent.id)
        .await
        .unwrap()
        .expect("parent should be restored");
    assert_eq!(parent.state, JobState::Waiting);
    assert!(parent.child_ids.is_empty());
    let child = reopened
        .get_job(&flow.children[0].id)
        .await
        .unwrap()
        .expect("child should be restored");
    assert!(child.parent_id.is_none());
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
async fn local_job_queue_persists_lifo_priority_updates() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("priority-lifo.json");
    let queue = LocalJobQueue::open("durable-priority-lifo", &snapshot_path)
        .await
        .unwrap();
    let fifo = queue
        .add_at(
            "fifo",
            serde_json::json!({}),
            JobOptions::new().with_priority(5),
            ts(1_000),
        )
        .await
        .unwrap();
    let changed = queue
        .add_at(
            "changed",
            serde_json::json!({}),
            JobOptions::new().with_priority(10),
            ts(1_000),
        )
        .await
        .unwrap();

    queue
        .update_priority_with_lifo(&changed.id, 5, true)
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable-priority-lifo", &snapshot_path)
        .await
        .unwrap();
    let restored = reopened.get_job(&changed.id).await.unwrap().unwrap();
    assert_eq!(restored.priority, 5);
    assert!(restored.options.lifo);

    let claimed = reopened
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, changed.id);
    assert_ne!(claimed.id, fifo.id);
}

#[tokio::test]
async fn local_job_queue_persists_lifo_waiting_order() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("lifo.json");
    let queue = LocalJobQueue::open("durable-lifo", &snapshot_path)
        .await
        .unwrap();
    let fifo = queue
        .add_at(
            "fifo",
            serde_json::json!({}),
            JobOptions::new().with_priority(5),
            ts(1_000),
        )
        .await
        .unwrap();
    let lifo_old = queue
        .add_at(
            "lifo-old",
            serde_json::json!({}),
            JobOptions::new().with_priority(5).with_lifo(true),
            ts(1_000),
        )
        .await
        .unwrap();
    let lifo_new = queue
        .add_at(
            "lifo-new",
            serde_json::json!({}),
            JobOptions::new().with_priority(5).with_lifo(true),
            ts(1_000),
        )
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable-lifo", &snapshot_path)
        .await
        .unwrap();
    for expected in [&lifo_new, &lifo_old, &fifo] {
        let claimed = reopened
            .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_000))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, expected.id);
    }
}

#[tokio::test]
async fn local_job_queue_persists_rescheduled_delayed_jobs() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("reschedule.json");
    let queue = LocalJobQueue::open("durable-reschedule", &snapshot_path)
        .await
        .unwrap();
    let job = queue
        .add_at(
            "task",
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(10)),
            ts(1_000),
        )
        .await
        .unwrap();

    queue
        .reschedule_job(&job.id, Duration::from_secs(2), ts(1_000))
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable-reschedule", &snapshot_path)
        .await
        .unwrap();
    let restored = reopened.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(restored.state, JobState::Delayed);
    assert_eq!(restored.scheduled_at, ts(3_000));
    assert_eq!(restored.options.delay, Some(Duration::from_secs(2)));
}

#[tokio::test]
async fn local_job_queue_persists_active_jobs_delayed_by_workers() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("active-delay.json");
    let queue = LocalJobQueue::open("durable-active-delay", &snapshot_path)
        .await
        .unwrap();
    let job = queue
        .add_at("task", serde_json::json!({}), JobOptions::new(), ts(1_000))
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_000))
        .await
        .unwrap()
        .unwrap();

    queue
        .delay_active_job(
            &job.id,
            lock_token(&claimed),
            Duration::from_secs(2),
            ts(1_500),
        )
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable-active-delay", &snapshot_path)
        .await
        .unwrap();
    let restored = reopened.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(restored.state, JobState::Delayed);
    assert_eq!(restored.scheduled_at, ts(3_500));
    assert_eq!(restored.options.delay, Some(Duration::from_secs(2)));
    assert!(restored.worker_id.is_none());
    assert!(restored.lock_token.is_none());
    assert!(restored.lease_expires_at.is_none());
}

#[tokio::test]
async fn local_job_queue_persists_active_jobs_released_by_workers() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("active-release.json");
    let queue = LocalJobQueue::open("durable-active-release", &snapshot_path)
        .await
        .unwrap();
    let job = queue
        .add_at("task", serde_json::json!({}), JobOptions::new(), ts(1_000))
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_000))
        .await
        .unwrap()
        .unwrap();

    queue
        .release_active_job(&job.id, lock_token(&claimed), ts(1_500))
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable-active-release", &snapshot_path)
        .await
        .unwrap();
    let restored = reopened.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(restored.state, JobState::Waiting);
    assert_eq!(restored.scheduled_at, ts(1_500));
    assert_eq!(restored.attempts_made, 1);
    assert!(restored.processed_at.is_none());
    assert!(restored.worker_id.is_none());
    assert!(restored.lock_token.is_none());
    assert!(restored.lease_expires_at.is_none());
}

#[tokio::test]
async fn local_job_queue_persists_discarded_retry_failures() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("discard-retry.json");
    let queue = LocalJobQueue::open("durable-discard-retry", &snapshot_path)
        .await
        .unwrap();
    let job = queue
        .add_at(
            "task",
            serde_json::json!({}),
            JobOptions::new().with_retry_policy(RetryPolicy::fixed(2, Duration::from_secs(10))),
            ts(1_000),
        )
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(1_100))
        .await
        .unwrap()
        .unwrap();

    let failed = queue
        .fail_job_discarding_retry(
            &claimed.id,
            lock_token(&claimed),
            "do not retry".to_string(),
            ts(1_200),
        )
        .await
        .unwrap();
    assert_eq!(failed.state, JobState::Failed);

    let reopened = LocalJobQueue::open("durable-discard-retry", &snapshot_path)
        .await
        .unwrap();
    let restored = reopened.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(restored.state, JobState::Failed);
    assert_eq!(restored.finished_at, Some(ts(1_200)));
    assert_eq!(restored.failed_reason.as_deref(), Some("do not retry"));
}

#[tokio::test]
async fn local_job_queue_restores_job_state_queries() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("state.json");
    let queue = LocalJobQueue::open("durable-state", &snapshot_path)
        .await
        .unwrap();
    let delayed = queue
        .add_at(
            "task",
            serde_json::json!({}),
            JobOptions::new().with_delay(Duration::from_secs(5)),
            ts(1_000),
        )
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable-state", &snapshot_path)
        .await
        .unwrap();
    assert_eq!(
        reopened.get_job_state(&delayed.id).await.unwrap(),
        Some(JobState::Delayed)
    );
    assert_eq!(reopened.get_job_state("missing").await.unwrap(), None);
}

#[tokio::test]
async fn local_job_queue_persists_removed_deduplication_keys() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("dedup-release.json");
    let queue = LocalJobQueue::open("durable-dedup-release", &snapshot_path)
        .await
        .unwrap();
    let first = queue
        .add_at(
            "sync",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication_id("account:42"),
            ts(1_000),
        )
        .await
        .unwrap();
    assert_eq!(
        queue
            .get_deduplication_job_id("account:42")
            .await
            .unwrap()
            .as_deref(),
        Some(first.id.as_str())
    );
    assert!(queue.remove_deduplication_key("account:42").await.unwrap());
    assert!(queue
        .get_deduplication_job_id("account:42")
        .await
        .unwrap()
        .is_none());

    let reopened = LocalJobQueue::open("durable-dedup-release", &snapshot_path)
        .await
        .unwrap();
    assert!(reopened
        .get_deduplication_job_id("account:42")
        .await
        .unwrap()
        .is_none());
    let second = reopened
        .add_at(
            "sync-after-reopen",
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication_id("account:42"),
            ts(1_100),
        )
        .await
        .unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(
        reopened
            .get_deduplication_job_id("account:42")
            .await
            .unwrap()
            .as_deref(),
        Some(second.id.as_str())
    );

    let duplicate_second = reopened
        .add_at(
            "sync-after-reopen-duplicate",
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication_id("account:42"),
            ts(1_200),
        )
        .await
        .unwrap();
    assert_eq!(duplicate_second.id, second.id);
    assert_eq!(
        reopened
            .get_deduplication_job_id("account:42")
            .await
            .unwrap()
            .as_deref(),
        Some(second.id.as_str())
    );
}

#[tokio::test]
async fn local_job_queue_persists_removed_keep_last_next() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("dedup-keep-last.json");
    let queue = LocalJobQueue::open("durable-dedup-keep-last", &snapshot_path)
        .await
        .unwrap();
    let owner = queue
        .add_at(
            "sync-owner",
            serde_json::json!({ "version": 1 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:keep-last-release").keep_last_if_active(true),
            ),
            ts(1_000),
        )
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), ts(2_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, owner.id);

    let duplicate = queue
        .add_at(
            "sync-stale-next",
            serde_json::json!({ "version": 2 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:keep-last-release").keep_last_if_active(true),
            ),
            ts(3_000),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.id, owner.id);
    assert!(queue
        .remove_deduplication_key("account:keep-last-release")
        .await
        .unwrap());

    queue
        .complete_job(
            &claimed.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            ts(4_000),
        )
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable-dedup-keep-last", &snapshot_path)
        .await
        .unwrap();
    let jobs = reopened.list_jobs(JobListOptions::new()).await.unwrap();
    assert!(!jobs.jobs.iter().any(|job| job.name == "sync-stale-next"));
    let after_reopen = reopened
        .add_at(
            "sync-after-reopen",
            serde_json::json!({ "version": 3 }),
            JobOptions::new().with_deduplication(
                DeduplicationOptions::new("account:keep-last-release").keep_last_if_active(true),
            ),
            ts(5_000),
        )
        .await
        .unwrap();
    assert_ne!(after_reopen.id, owner.id);
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

    let repeats = reopened.list_repeats().await.unwrap();
    assert_eq!(repeats.len(), 1);
    assert_eq!(repeats[0].key, "sync");
    assert_eq!(repeats[0].job_id, delayed.jobs[0].id);
    assert_eq!(repeats[0].state, JobState::Delayed);
    assert_eq!(repeats[0].repeat_count, 1);
}

#[tokio::test]
async fn local_job_queue_persists_snapshot_across_reopen() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("queue.json");
    let now = ts(1_000);

    let queue = LocalJobQueue::open("durable", &snapshot_path)
        .await
        .unwrap();
    assert!(!queue.is_paused().await.unwrap());
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
    assert!(queue.is_paused().await.unwrap());

    let reopened = LocalJobQueue::open("durable", &snapshot_path)
        .await
        .unwrap();
    assert!(reopened.is_paused().await.unwrap());
    let stats = reopened.stats().await.unwrap();
    assert!(stats.paused);
    assert_eq!(stats.waiting, 1);
    assert_eq!(
        reopened.get_job(&job.id).await.unwrap().unwrap().name,
        "email"
    );

    reopened.resume().await.unwrap();
    assert!(!reopened.is_paused().await.unwrap());
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
    let restored_logs = reopened.get_job_logs(&job.id, 0, -1, true).await.unwrap();
    assert_eq!(restored_logs.count, 1);
    assert_eq!(restored_logs.logs[0].line, "almost done");
    let cleared_logs = reopened.clear_job_logs(&job.id, 0).await.unwrap();
    assert_eq!(cleared_logs.count, 0);
    assert!(cleared_logs.logs.is_empty());
    let reopened = LocalJobQueue::open("durable", &snapshot_path)
        .await
        .unwrap();
    let restored_after_clear = reopened.get_job(&job.id).await.unwrap().unwrap();
    assert!(restored_after_clear.logs.is_empty());
    assert_eq!(
        restored.return_value,
        Some(serde_json::json!({ "ok": true }))
    );
}

#[tokio::test]
async fn local_job_queue_persists_failed_obliterate_pause_and_forced_clear() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("obliterate.json");
    let now = ts(1_000);

    let queue = LocalJobQueue::open("durable-obliterate", &snapshot_path)
        .await
        .unwrap();
    let job = queue
        .add_at("active", serde_json::json!({}), JobOptions::new(), now)
        .await
        .unwrap();
    let claimed = queue
        .claim_next("worker-a".to_string(), Duration::from_secs(30), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, job.id);

    let error = queue.obliterate(false).await.unwrap_err();
    assert!(matches!(error, LaneError::JobStateConflict(_)));

    let reopened = LocalJobQueue::open("durable-obliterate", &snapshot_path)
        .await
        .unwrap();
    assert!(reopened.is_paused().await.unwrap());
    let paused_stats = reopened.stats().await.unwrap();
    assert!(paused_stats.paused);
    assert_eq!(paused_stats.active, 1);
    assert!(reopened
        .claim_next(
            "worker-paused".to_string(),
            Duration::from_secs(30),
            ts(1_100)
        )
        .await
        .unwrap()
        .is_none());

    let removed = reopened.obliterate(true).await.unwrap();
    assert_eq!(removed, 1);

    let reopened = LocalJobQueue::open("durable-obliterate", &snapshot_path)
        .await
        .unwrap();
    let stats = reopened.stats().await.unwrap();
    assert_eq!(stats.total, 0);
    assert_eq!(stats.active, 0);
    assert!(!stats.paused);
    assert!(!reopened.is_paused().await.unwrap());

    let after = reopened
        .add_at("after", serde_json::json!({}), JobOptions::new(), ts(1_200))
        .await
        .unwrap();
    let claimed_after = reopened
        .claim_next(
            "worker-after".to_string(),
            Duration::from_secs(30),
            ts(1_200),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed_after.id, after.id);
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
                .update_data(serde_json::json!({
                    "to": job.payload["to"].clone(),
                    "normalized": true
                }))
                .await?;
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
    assert_eq!(
        stored.payload,
        serde_json::json!({ "to": "ops@example.com", "normalized": true })
    );
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
async fn worker_context_can_discard_configured_retry() {
    let backend: Arc<dyn JobQueueBackend> = Arc::new(InMemoryJobQueue::new("worker-discard"));
    let job = backend
        .add_job(
            "fail-once".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_retry_policy(RetryPolicy::fixed(1, Duration::from_secs(30))),
        )
        .await
        .unwrap();

    let processor = Arc::new(job_processor_fn(|_, context: JobContext| async move {
        context.discard_retry();
        Err::<Value, LaneError>(LaneError::Other("unrecoverable".to_string()))
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
    assert_eq!(failed.failed_reason.as_deref(), Some("unrecoverable"));

    let stored = backend.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(stored.state, JobState::Failed);
    assert_eq!(stored.finished_at, failed.finished_at);
    assert_eq!(backend.stats().await.unwrap().delayed, 0);
}

#[tokio::test]
async fn worker_unrecoverable_error_discards_configured_retry() {
    let backend: Arc<dyn JobQueueBackend> = Arc::new(InMemoryJobQueue::new("worker-unrecoverable"));
    let job = backend
        .add_job(
            "fail-terminal".to_string(),
            serde_json::json!({}),
            JobOptions::new().with_retry_policy(RetryPolicy::fixed(2, Duration::from_secs(30))),
        )
        .await
        .unwrap();

    let processor = Arc::new(job_processor_fn(|_, _| async {
        Err::<Value, LaneError>(LaneError::unrecoverable_job(
            "schema is permanently invalid",
        ))
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
    assert_eq!(
        failed.failed_reason.as_deref(),
        Some("schema is permanently invalid")
    );

    let stored = backend.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(stored.state, JobState::Failed);
    assert_eq!(stored.attempts_made, 1);
    assert_eq!(backend.stats().await.unwrap().delayed, 0);
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

#[tokio::test]
async fn job_event_stream_records_core_lifecycle() {
    let queue = InMemoryJobQueue::new("events");
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
        .update_progress(&job.id, serde_json::json!({ "percent": 50 }))
        .await
        .unwrap();
    queue
        .complete_job(
            &job.id,
            lock_token(&claimed),
            serde_json::json!({ "ok": true }),
            now,
        )
        .await
        .unwrap();

    let events = queue.read_events("-", "+", 20).await.unwrap();
    let names = events
        .iter()
        .map(|event| event.event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["added", "waiting", "active", "progress", "completed"]
    );
    assert_eq!(events[0].job_id.as_deref(), Some(job.id.as_str()));
    assert_eq!(
        events[0].fields.get("name"),
        Some(&Value::String("task".to_string()))
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

    assert_eq!(queue.trim_events(2).await.unwrap(), 3);
    let trimmed = queue.read_events("-", "+", 20).await.unwrap();
    let names = trimmed
        .iter()
        .map(|event| event.event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["progress", "completed"]);
}

#[tokio::test]
async fn local_job_queue_persists_event_stream() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("events.json");
    let queue = LocalJobQueue::open("durable-events", &snapshot_path)
        .await
        .unwrap();
    let job = queue
        .add_at("task", serde_json::json!({}), JobOptions::new(), ts(1_000))
        .await
        .unwrap();
    queue
        .update_progress(&job.id, serde_json::json!({ "percent": 25 }))
        .await
        .unwrap();

    let reopened = LocalJobQueue::open("durable-events", &snapshot_path)
        .await
        .unwrap();
    let events = reopened.read_events("-", "+", 10).await.unwrap();
    let names = events
        .iter()
        .map(|event| event.event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["added", "waiting", "progress"]);
    assert_eq!(
        events[2].fields.get("data"),
        Some(&serde_json::json!({ "percent": 25 }))
    );
}

#[tokio::test]
async fn local_job_queue_persists_finished_retention_cleanup() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let snapshot_path = temp_dir.path().join("jobs").join("retention.json");
    let queue = LocalJobQueue::open("durable-retention", &snapshot_path)
        .await
        .unwrap();
    let mut ids = Vec::new();

    for index in 0..2 {
        let now = ts(1_000 + index * 100);
        let job = queue
            .add_at(
                format!("task-{index}"),
                serde_json::json!({}),
                JobOptions::new().with_completion_retention(JobRetention::count(1)),
                now,
            )
            .await
            .unwrap();
        ids.push(job.id.clone());
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
                ts(1_100 + index * 100),
            )
            .await
            .unwrap();
    }

    let reopened = LocalJobQueue::open("durable-retention", &snapshot_path)
        .await
        .unwrap();
    assert!(reopened.get_job(&ids[0]).await.unwrap().is_none());
    assert_eq!(
        reopened.get_job(&ids[1]).await.unwrap().unwrap().state,
        JobState::Completed
    );
}
