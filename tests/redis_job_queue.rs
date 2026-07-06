#![cfg(feature = "redis-backend")]

use a3s_lane::{JobOptions, JobQueueBackend, JobState, RedisJobQueue, RetryPolicy};
use chrono::Utc;
use redis::AsyncCommands;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

    worker
        .update_progress(&first.id, serde_json::json!({ "percent": 50 }))
        .await
        .expect("progress update should succeed");
    worker
        .add_log(&first.id, "accepted".to_string(), 10, Utc::now())
        .await
        .expect("log update should succeed");
    let completed = worker
        .complete_job(&first.id, serde_json::json!({ "ok": true }), Utc::now())
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
        .fail_job(&second.id, "temporary".to_string(), Utc::now())
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
        .fail_job(&retried.id, "terminal".to_string(), Utc::now())
        .await
        .expect("terminal failure should succeed");
    assert_eq!(failed.state, JobState::Failed);

    let delayed = producer
        .add_job(
            "delayed".to_string(),
            serde_json::json!({}),
            JobOptions::new()
                .with_priority(1)
                .with_delay(Duration::from_millis(10)),
        )
        .await
        .expect("delayed job should be added");
    assert_eq!(delayed.state, JobState::Delayed);
    assert!(worker
        .claim_next("worker-d".to_string(), Duration::from_secs(30), Utc::now())
        .await
        .expect("early delayed claim should return")
        .is_none());

    tokio::time::sleep(Duration::from_millis(20)).await;
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
    assert_eq!(stats.completed, 1);
    assert_eq!(stats.failed, 1);
    assert_eq!(stats.active, 1);

    cleanup_namespace(&redis_url, &namespace).await?;
    Ok(())
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
