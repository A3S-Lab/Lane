#![cfg(feature = "redis-backend")]

use a3s_lane::{
    JobListOptions, JobOptions, JobQueueBackend, JobRateLimit, JobSpec, JobState, LaneError,
    RedisJobQueue, RepeatOptions, RetryPolicy,
};
use chrono::{DateTime, Utc};
use redis::AsyncCommands;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn lock_token(job: &a3s_lane::Job) -> &str {
    job.lock_token
        .as_deref()
        .expect("claimed job should carry a lock token")
}

#[tokio::test]
async fn redis_backend_runs_job_lifecycle_against_real_server() {
    let Some(redis_url) = redis_url() else {
        eprintln!("skipping Redis integration test; set A3S_LANE_REDIS_URL");
        return;
    };
    tokio::time::timeout(Duration::from_secs(20), run_job_lifecycle(redis_url))
        .await
        .expect("Redis integration test timed out")
        .unwrap();
}

async fn run_job_lifecycle(redis_url: String) -> redis::RedisResult<()> {
    let namespace = unique_namespace();
    cleanup_namespace(&redis_url, &namespace).await?;

    let producer = RedisJobQueue::with_namespace(&redis_url, &namespace, "jobs")
        .expect("valid Redis URL should build the producer queue");
    let worker = RedisJobQueue::with_namespace(&redis_url, &namespace, "jobs")
        .expect("valid Redis URL should build the worker queue");

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

    let paused_promote_queue =
        RedisJobQueue::with_namespace(&redis_url, &namespace, "paused-promote")
            .expect("valid Redis URL should build the paused-promote queue");
    paused_promote_queue
        .pause()
        .await
        .expect("paused-promote queue should pause");
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

    let active_limit_queue = RedisJobQueue::with_namespace(&redis_url, &namespace, "active-limit")
        .expect("valid Redis URL should build the active limit queue");
    let zero_active_limit = active_limit_queue
        .set_max_active_jobs(0)
        .await
        .expect_err("zero active limit should be rejected");
    assert!(matches!(zero_active_limit, LaneError::ConfigError(_)));
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
    worker
        .add_log(&first.id, "accepted".to_string(), 10, Utc::now())
        .await
        .expect("log update should succeed");
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
                .with_delay(Duration::from_millis(200)),
        )
        .await
        .expect("delayed job should be added");
    assert_eq!(delayed.state, JobState::Delayed);
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
    assert_eq!(
        producer
            .get_job(&claimed_delayed.id)
            .await
            .expect("active job should load")
            .expect("active job should still exist")
            .state,
        JobState::Active
    );

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
        stored_high.progress,
        Some(serde_json::json!({ "percent": 50 }))
    );
    assert_eq!(stored_high.logs.len(), 1);
    assert_eq!(stored_high.logs[0].line, "accepted");

    let stats = producer.stats().await.expect("stats should load");
    assert_eq!(stats.completed, 3);
    assert_eq!(stats.failed, 2);
    assert_eq!(stats.active, 1);

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
        .find(|job| job.repeat_key.as_deref() == Some("heartbeat"))
        .expect("repeat successor should be delayed");
    assert_eq!(repeat_successor.repeat_count, 1);
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
    let delayed_after_limit = producer
        .list_jobs(JobListOptions::new().with_state(JobState::Delayed))
        .await
        .expect("delayed jobs should list after repeat limit");
    assert!(!delayed_after_limit
        .jobs
        .iter()
        .any(|job| job.repeat_key.as_deref() == Some("heartbeat")));

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
        .find(|job| job.repeat_key.as_deref() == Some("ephemeral-heartbeat"))
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

    cleanup_namespace(&redis_url, &namespace).await?;
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
    format!("a3s:lane:test:{}:{timestamp}", std::process::id())
}

async fn cleanup_namespace(redis_url: &str, namespace: &str) -> redis::RedisResult<()> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_connection_manager().await?;
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{namespace}:*"))
        .query_async(&mut conn)
        .await?;
    if !keys.is_empty() {
        let _: usize = conn.del(keys).await?;
    }
    Ok(())
}
