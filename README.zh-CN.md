# a3s 车道

<p>
  <strong>Language / 语言:</strong>
  <a href="README.md">English</a> ·
  <a href="README.zh-CN.md">中文</a>
</p>

用于并发异步任务的基于通道的优先级队列。命令可以组织到具有可配置并发性和优先级的命名通道中，或者保留为键入的主机拥有的值，直到主机准备好执行它们。

优先级控制接下来允许哪个待处理项目。它不会打断已经运行的未来；主动工作仍需要明确的取消和结算合同。

[![crates.io](https://img.shields.io/crates/v/a3s-lane.svg)](https://crates.io/crates/a3s-lane)

## 安装

```toml
[dependencies]
a3s-lane = "0.5"
```

默认情况下，所有四个功能（`distributed`、`metrics`、`monitoring`、`telemetry`）均处于启用状态。仅核心队列：

```toml
a3s-lane = { version = "0.5", default-features = false }
# or pick selectively:
a3s-lane = { version = "0.5", default-features = false, features = ["metrics", "distributed"] }
```

为多进程工作人员启用可选的 Redis 通用作业后端：

```toml
a3s-lane = { version = "0.5", features = ["redis-backend"] }
```

## 用法

为每个任务类型实现 `Command` 特征：

```rust
#[async_trait]
pub trait Command: Send + Sync {
    async fn execute(&self) -> Result<serde_json::Value>;
    fn command_type(&self) -> &str;
}
```

然后构建一个管理器，启动调度器，并提交：

```rust
use a3s_lane::{QueueManagerBuilder, EventEmitter, Command, Result};
use async_trait::async_trait;
use std::time::Duration;

struct FetchCommand { url: String }

#[async_trait]
impl Command for FetchCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({ "url": self.url }))
    }
    fn command_type(&self) -> &str { "fetch" }
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let emitter = EventEmitter::new(100);
    let manager = QueueManagerBuilder::new(emitter)
        .with_default_lanes()
        .build().await?;

    manager.start().await?;

    let rx = manager.submit("query", Box::new(FetchCommand { url: "...".into() })).await?;
    let result = rx.await??;
    println!("{result}");

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;
    Ok(())
}
```

`submit()` 返回 `oneshot::Receiver<Result<Value>>` — `??` 解包通道发送和命令结果。

## 车道模型

|车道 |优先|最大并发数 |使用案例|
|------|----------|-----------------|----------|
| `system` | 0（最高）| 5 |系统级操作 |
| `control` | 1 | 3 |暂停/取消 |
| `query` | 2 | 10 | 10只读查询 |
| `session` | 3 | 5 |会话管理 |
| `skill` | 4 | 3 |工具执行 |
| `prompt` | 5（最低）| 2 |法学硕士一代|

自定义通道替换或扩展默认通道：

```rust
QueueManagerBuilder::new(emitter)
    .with_lane("high",  LaneConfig::new(1, 4), 0)
    .with_lane("low",   LaneConfig::new(1, 2), 1)
    .build().await?;
```

## 主机拥有的类型化队列

当主机必须保留类型化状态的所有权并且
决定执行何时开始，就像终端或 Web 事件循环一样。较低
数值首先运行，同等优先级的项目保持 FIFO：

```rust
use a3s_lane::{PriorityItem, PriorityQueue};

let mut turns = PriorityQueue::new();
turns.push(1, "automatic continuation");
turns.push(0, "first user turn");
turns.push(0, "second user turn");

let claimed = turns.pop().expect("queued turn");
assert_eq!(claimed.value(), &"first user turn");

// If admission fails before execution starts, preserve its original FIFO slot.
turns.restore(claimed);
let order = turns
    .ordered()
    .into_iter()
    .map(PriorityItem::value)
    .copied()
    .collect::<Vec<_>>();
assert_eq!(
    order,
    ["first user turn", "second user turn", "automatic continuation"]
);
```

`ordered()` 是队列 UI 的非变异投影。已认领的物品会保留
它的优先级和插入顺序，因此`restore()`可以放置失败的接纳
返回而不将其移到新的工作后面。

## 车道配置

所有选项都使用构建器模式并且可以链接：

```rust
LaneConfig::new(min_concurrency, max_concurrency)
    .with_timeout(Duration::from_secs(30))
    .with_retry_policy(RetryPolicy::exponential(3))     // 100ms initial, 2× backoff, 30s cap
    .with_pressure_threshold(50)                        // emit queue.lane.pressure / queue.lane.idle
    .with_rate_limit(RateLimitConfig::per_second(100))  // requires `distributed` feature
    .with_priority_boost(PriorityBoostConfig::standard( // requires `distributed` feature
        Duration::from_secs(300),
    ))
```

**重试策略**：`exponential(max_retries)`、`fixed(max_retries, delay)`、`none()`。

**速率限制配置**：`per_second(n)`、`per_minute(n)`、`per_hour(n)`、`unlimited()`。

**PriorityBoostConfig**：`standard(deadline)`（以剩余截止日期的 75/50/25% 提升）、`aggressive(deadline)`、`disabled()`。

## 活动

`EventStream` 实现 `futures_core::Stream` — 通过 `StreamExt` 或 `.recv()` 便捷方法使用 `.next().await`。直接从管理器订阅，无需手动线程`EventEmitter`：

```rust
use tokio_stream::StreamExt;

// All events
let mut stream = manager.subscribe();

// Filtered — only failures
let mut failures = manager.subscribe_filtered(|e| {
    e.key == "queue.command.failed" || e.key == "queue.command.timeout"
});

tokio::spawn(async move {
    while let Some(event) = stream.next().await {
        println!("[{}] {}", event.timestamp, event.key);
    }
});
```

在每个队列阶段自动发出事件：

|事件键|当 |有效负载字段 |
|------------|------|----------------|
| `queue.command.submitted` | `submit()` 已接受 | `lane_id` |
| `queue.command.started` |调度程序已调度 | `lane_id`、`command_id`、`command_type` |
| `queue.command.completed` |已退货`Ok` | `lane_id`、`command_id` |
| `queue.command.retry` |失败，将重试 | `lane_id`、`command_id`、`attempt` |
| `queue.command.dead_lettered` |移至 DLQ | `lane_id`、`command_id`、`command_type` |
| `queue.command.failed` |终端故障| `lane_id`、`command_id`、`error` |
| `queue.command.timeout` |超时 | `lane_id`、`command_id`、`error` |
| `queue.shutdown.started` | `shutdown()` 称为 | — |
| `queue.lane.pressure` | `pending >= threshold`，第一次穿越| `lane_id` |
| `queue.lane.idle` | `pending == 0` 受压后 | `lane_id` |

`queue.lane.pressure` 和 `queue.lane.idle` 在通道配置上需要 `with_pressure_threshold(n)`。

## 可靠性

### 死信队列

```rust
let dlq = DeadLetterQueue::new(1000);
let queue = CommandQueue::with_dlq(emitter, dlq.clone());

// Inspect failed commands after running
for letter in dlq.list().await {
    println!("{}: {}", letter.command_type, letter.error);
}
```

### 持久存储

```rust
let storage = Arc::new(LocalStorage::new(PathBuf::from("./queue_data")).await?);
let manager = QueueManagerBuilder::new(emitter)
    .with_storage(storage)
    .with_default_lanes()
    .build().await?;
```

自定义后端：实现 `Storage` 特征（`save_command`、`load_commands`、`remove_command`、`save_dead_letter`、`load_dead_letters`、`clear_all`）。

### 优雅关闭

```rust
manager.shutdown().await;                           // stop accepting new commands
manager.drain(Duration::from_secs(30)).await?;      // wait for in-flight to finish
```

## 可观察性

### 指标

```rust
let metrics = QueueMetrics::local();  // in-memory; or bring your own MetricsBackend
let manager = QueueManagerBuilder::new(emitter)
    .with_metrics(metrics.clone())
    .build().await?;

let snap = metrics.snapshot().await;
// snap.counters  →  submit/complete/fail/timeout/retry/dead-letter counts per lane
// snap.histograms →  latency p50/p90/p95/p99 per lane
```

OpenTelemetry OTLP 导出：使用`OtelMetricsBackend`（需要`telemetry` 功能）。

自定义后端：实现`MetricsBackend`（`increment_counter`、`set_gauge`、`record_histogram`、`snapshot`、`reset`）。

### 警报和监控

```rust
let alerts = Arc::new(AlertManager::with_queue_depth_alerts(
    100,  // warning threshold
    200,  // critical threshold
));
alerts.add_callback(|a| eprintln!("[{:?}] {}: {}", a.level, a.lane_id, a.message)).await;

let manager = QueueManagerBuilder::new(emitter)
    .with_alerts(alerts)
    .build().await?;
```

后台监视器（按时间间隔轮询）：

```rust
let monitor = Arc::new(QueueMonitor::with_config(manager.queue(), MonitorConfig {
    interval: Duration::from_secs(5),
    pending_warning_threshold: 50,
    active_warning_threshold: 25,
}));
monitor.clone().start().await;

let stats = monitor.stats().await;
println!("pending={} active={}", stats.total_pending, stats.total_active);
```

## 可扩展性（`distributed` 功能）

```rust
// Rate limiting — enforced at dequeue time, not submit time
LaneConfig::new(1, 10).with_rate_limit(RateLimitConfig::per_second(100))

// Priority boost — commands approaching their deadline get elevated priority
LaneConfig::new(1, 10).with_priority_boost(
    PriorityBoostConfig::standard(Duration::from_secs(300))
)

// Multi-core partitioning — auto-detects CPU cores
let queue = Arc::new(LocalDistributedQueue::auto());
```

自定义分布式队列：实现`DistributedQueue`（`enqueue`、`dequeue`、`complete`、`num_partitions`、`worker_id`）。

## 发展

```bash
just test       # 420 library tests, --all-features
just ci         # fmt + clippy + test
just bench      # Criterion benchmarks → target/criterion/report/index.html
just cov        # coverage report (requires cargo-llvm-cov)
just doc        # generate and open rustdoc
```

可选：`cargo install cargo-llvm-cov`、`brew install lcov`（HTML 覆盖范围）。

## 在A3S生态系统中

a3s-lane是A3S Agent OS的调度层。 A3S 代码使用键入的
主机拥有的待处理轮次的队列，并且可以选择创建每个会话通道
工具执行管理器。会话执行本身仍然存在
单次航班：取消在下一个排队之前解决活动工作人员的问题
轮流被承认。

```
a3s-gateway → a3s-box (MicroVM) → SafeClaw → a3s-code → a3s-lane
                                                          ↑ here
```

对于任何基于优先级的异步调度独立工作：Web 服务器、后台作业处理器、速率受限的 API 客户端。

## 通用作业队列路线图

A3S Lane 正在从进程内通道调度程序演变成通用的
分布式优先级作业队列。该方向类似于 BullMQ，但原生于
A3S 堆栈和 Rust API。

|相|状态 |范围 |
| ---| ---| ---|
|车道调度器|完成 |通道优先级、每通道并发性、命令重试、超时、DLQ、事件、指标、监控。 |
|通用作业运行时 |进行中 | JSON 作业、Lua 支持的 Redis 批量提交、幂等自定义作业 ID、带可选 TTL 的简单重复数据删除、反跳 TTL 扩展、延迟所有者替换、保留最后活动重新排队、重复密钥所有权和更新插入、显式作业状态、优先级加上 FIFO/LIFO 相同优先级排序、按年龄/计数/限制保留已完成作业、保留队列事件流、延迟作业、令牌拥有的工作器租赁、主动等待/延迟移动、完成/失败快照、重试退避、Redis 共享速率限制和主动并发控制、BullMQ 式两阶段停滞恢复（具有重复调度程序重新排队处理）、暂停/恢复。 ||作业管理 API |进行中 |添加/获取/获取状态/获取作业完成结果/获取作业计数/获取作业计数/计数待处理/删除/删除重复/upsert-重复/删除重复数据删除密钥/获取重复数据删除作业 ID/列表-重复/get-repeat/count-repeats/list-repeats-page/add-flow-children/get-flow-dependency/get-flow-dependency-counts/get-flow-dependency-selected-counts/get-flow-dependency-va lues/get-flow-dependency-page/get-flow-dependency-pages/get-flow-children-values/get-flow-ignored-children-failures/remove-unprocessed-children/remove-child-dependency/promote/重新安排/延迟活动/释放活动/重试/更新优先级/更新优先级with-lifo/更新数据/保存堆栈跟踪/暂停/恢复/已暂停/排空/清理/删除/删除孤立Redis 维护 API、多状态分页、升序/降序列表、等待优先级计数、添加日志/获取日志/清除作业日志、读取事件/修剪事件、进度更新、单个和批量租约续订、Redis 终端指标。 |
|工人运行时 |进行中 | `JobWorker`从任何`JobQueueBackend`声明作业，在可用时使用后端本机阻塞声明挂钩，通过`JobProcessorRouter`按名称路由作业，运行异步处理器，完成/失败作业，支持处理器进度/日志更新，协作租约丢失检查，超时，后台循环的共享批量租约续订以及停滞的恢复循环。 ||耐用的后端 |进行中 | `LocalJobQueue` JSON快照持久化可用，包括父级范围的流依赖侧索引； `RedisJobQueue`可在`redis-backend`后面使用，具有Lua支持的添加、批量添加、先进先出/后进先出等待分数排序、BullMQ风格的Redis工作标记zset更新、Redis标记支持的阻塞声明、Redis流队列事件、使用TTL的简单重复数据删除、反跳TTL扩展、延迟所有者替换、保留最后一个活动重新排队、重复数据删除键删除、重复键所有权、Redis 支持的重复调度程序 zset/hash 元数据、列表/删除/更新插入/分页、静态流提交、动态流子扇出、流依赖项检查、BullMQ 样式选定/完整依赖项存储桶计数和读取、单/多存储桶分页依赖项读取、流子值和忽略失败读取、动态流子级重复数据删除跳过和保持最后实现、流父级和活动子级保持最后实现、延迟升级和重新安排、活动等待/延迟移动、单作业提升、状态索引和完成结果查询、作业计数快照、终端指标、手动重试、优先级更新、进度更新、堆栈跟踪更新、日志追加、列表/统计快照、完成/失败/停顿脚本期间的完成作业年龄/计数保留、排出、清理、孤立作业清理、删除、声明、Redis 共享速率限制、最大活动、流父级释放/失败事件、重复后继队列、完成、失败、更新、删除和停止候选集恢复语义。 Postgres/NATS 后端仍在计划中。 |
|流动工作 |进行中 |父子依赖、等待子状态、依赖检查、BullMQ 风格的选定/完整依赖桶计数和读取、单/多桶分页依赖检查、子返回值检查、忽略、删除、继续和失败父子失败释放、静态和动态扇出、扇入释放、流父级重复数据删除事件、静态和动态普通流子级重复数据删除跳过语义、活动子流保持最后重复数据删除实现、BullMQ 风格现有父级和可以使用带有 `duplicated` 事件的子自定义作业 ID 附件、内存/本地流父级 keep-last 重复数据删除以及活动父级完成、终端故障或停滞终端故障时的 Redis 流父级 keep-last 实现。 |
|重复工作 |进行中 |具有重复键、限制、结束时间戳、重复键删除、更新插入、单键查找、计数和 BullMQ 样式的下次分页的固定间隔和 UTC cron 可重复作业可跨内存、本地持久和 Redis 后端使用。 Redis 另外还在 Lua 中维护调度程序 zset/hash 元数据，因此分布式读取器和写入器共享一个重复系列状态机。 |
|框架集成|计划| NestJS 模块和来自 BullMQ 兼容概念的迁移指南。 |

通用作业运行时通过 `JobQueueBackend` 特征公开。
`InMemoryJobQueue` 是进程本地的，用于测试、嵌入式运行时、
和参考语义：

```rust
use a3s_lane::{InMemoryJobQueue, JobListOptions, JobOptions, JobQueueBackend, JobState, RetryPolicy};
use std::time::Duration;

# async fn example() -> a3s_lane::Result<()> {
let queue = InMemoryJobQueue::new("email");

let job = queue
    .add(
        "send",
        serde_json::json!({ "to": "ops@example.com" }),
        JobOptions::new()
            .with_job_id("email:ops@example.com:welcome")
            .with_priority(10)
            .with_lifo(false)
            .with_delay(Duration::from_secs(5))
            .with_retry_policy(RetryPolicy::fixed(3, Duration::from_secs(1))),
    )
    .await?;

let bulk = queue
    .add_jobs(
        vec![
            a3s_lane::JobSpec::new("index", serde_json::json!({ "id": 1 })),
            a3s_lane::JobSpec::new("index", serde_json::json!({ "id": 2 })),
        ],
        chrono::Utc::now(),
    )
    .await?;
assert_eq!(bulk.len(), 2);

let recent_pending = queue
    .list_jobs(
        JobListOptions::new()
            .with_states([JobState::Waiting, JobState::Delayed])
            .descending()
            .with_limit(20),
    )
    .await?;
assert_eq!(recent_pending.total, 3);

queue.promote_due_jobs(chrono::Utc::now()).await?;
let claimed = queue
    .claim_next("worker-1".to_string(), Duration::from_secs(30), chrono::Utc::now())
    .await?;

if let Some(claimed) = claimed {
    let lock_token = claimed
        .lock_token
        .as_deref()
        .expect("claimed jobs include a lock token");
    queue
        .update_data(&claimed.id, serde_json::json!({ "to": "ops@example.com", "normalized": true }))
        .await?;
    queue
        .update_progress(&claimed.id, serde_json::json!({ "percent": 50 }))
        .await?;
    queue
        .add_log(&claimed.id, "smtp accepted message".to_string(), 100, chrono::Utc::now())
        .await?;
    queue
        .complete_job(
            &claimed.id,
            lock_token,
            serde_json::json!({ "ok": true }),
            chrono::Utc::now(),
        )
        .await?;
}
# Ok(())
# }
```

管理 API 是后端合约的一部分：`list_jobs()` 返回
分页 `JobListPage` 值，具有单状态、多状态、升序和
降序范围选项，`add_jobs()` 提交具有相同的批次
幂等性语义为 `add_job()`、`promote_job()` 将延迟作业移至
等待，`reschedule_job()` 更改延迟作业相对于作业的到期时间
当前时钟，`delay_active_job()` 将代币拥有的活动作业移回到
延迟，`release_active_job()`将代币拥有的活动作业移回等待状态，
`get_job_state()` 返回作业 ID 的当前生命周期状态，`retry_job()`
手动重新排队保留的失败或已完成的作业，`fail_job_discarding_retry()`
在不应用剩余自动重试的情况下使活动的令牌拥有的作业失败，`update_priority()`
更改存储的作业优先级，`update_priority_with_lifo()` 还选择
相同优先级等待重新插入侧，`renew_lease()`扩展了一个活跃的worker
使用索赔代币进行租赁，`renew_leases()`续订多个索赔
租用并返回续订失败的作业 ID，
`remove_job()` 删除不受活动工作锁保护的作业，
`remove_repeat()` 删除重复密钥的当前非活动所有者，并且，
Redis，当所有者密钥过时或时可以回退到调度程序元数据
失踪,
`upsert_repeat()` 创建或替换当前非活动所有者以进行重复
钥匙，
`remove_deduplication_key()` 清除重复数据删除 ID 的活动所有者，
`get_deduplication_job_id()` 返回当前所有者的作业 ID
重复数据删除 ID，`list_repeats()` 列出当前非终端重复序列
所有者，`get_repeat()` 通过键返回一个当前重复所有者，
`count_repeats()` 返回当前重复序列计数，并且`list_repeats_page()` 返回按下一个计划时间排序的重复系列
BullMQ 风格的默认降序分页，
`get_flow_dependencies()` 返回流父级的子级快照以及待处理的快照
并且缺少子 ID，`get_flow_dependency_counts()` 返回已处理，
未处理、失败、忽略和丢失的子项计数，
`get_flow_dependency_selected_counts()` 仅返回请求的 BullMQ 风格
已处理、未处理、忽略和失败的计数桶，
`get_flow_dependency_values()` 返回 BullMQ 风格的已处理、未处理、
被忽略并且依赖项存储桶失败，
`get_flow_dependency_page()` 从一个返回一个 BullMQ 风格的游标页
已处理、未处理、忽略或失败的流依赖性存储桶，以及
`get_flow_dependency_pages()` 返回多个请求的依赖桶页面
在一次后端调用中，
流父级无法完成，同时阻止子级依赖仍然存在，
`add_flow_children()` 让活跃的、拥有代币的父级添加新的或现有的
自定义 id 子作业并将其自身移动到 `waiting_children`，镜像 BullMQ
动态`moveToWaitingChildren()`扇出路径，
`remove_unprocessed_children()`
删除仍未处理且不活跃的子项，
`remove_child_dependency()` 将一个子依赖与其父依赖分离
在不删除子作业的情况下，
`drain_jobs(false)` 删除等待作业，`drain_jobs(true)` 也删除
普通延迟的工作，同时保留当前延迟的回头客，
`clean_jobs()` 按状态删除旧记录，`obliterate(false)` 暂停
仅当不存在活动作业时才排队并删除所有队列数据，
即使有活动作业，`obliterate(true)` 也会强制删除，`get_job_counts()`
返回每个状态的计数，`get_job_count()`返回聚合计数
选择状态，`count_pending_jobs()`返回等待、延迟和等待子进程工作，`get_counts_per_priority()` 返回等待作业计数
对于选定的优先级，`get_job_finished_result()`返回`NotFinished`，
已完成的返回值，或保留作业的终端失败原因，
`RedisJobQueue::get_metrics()` 返回 BullMQ 风格的已完成/失败
每分钟终端指标，`update_data()`取代保留的作业负载，
`save_stacktrace()` 存储保留的失败堆栈跟踪和失败原因，`add_log()` 附加保留的作业日志，以及
`get_job_logs()` 返回具有 Redis/BullMQ 风格范围语义的 `JobLogPage`。
`clear_job_logs(job_id, 0)` 清除作业保留的日志，同时为正
值保留最新条目。 `read_events("-", "+", limit)` 读取保留
按 Redis 流 ID 顺序对事件进行队列，并且 `trim_events(max_len)` 修剪
使用后端的保留事件机制对事件流进行队列。
`pause()`、`resume()`和`is_paused()`提供队列级调度控制。
当待处理的子级被删除时，清理路径可以解锁流父级。
当生产者需要幂等提交时设置`JobOptions::with_job_id()`：
再次添加相同的作业 ID 将返回现有作业，而不是排队
重复。自定义作业 ID 不得为 `0` 或以 `0:` 开头，因为 BullMQ
为内部等待列表标记保留该形状，以及纯整数自定义
id 被拒绝匹配 BullMQ 的 `Job.validateOptions()` 守卫。
`JobOptions::with_lifo(true)` 更改了就绪作业插入语义
具有相同优先级的作业：较新的就绪作业先于较旧的就绪作业被声明
作业，而较低优先级值仍然首先运行。优先级遵循 BullMQ 的
整数范围且不得超过`2^21`；两个添加时间选项和`update_priority()`/`update_priority_with_lifo()` 之前强制执行该限制
改变后端状态。
默认情况下保留已完成的作业。 `remove_on_complete(true)` 和
`remove_on_fail(true)` 保留删除当前的兼容性简写
立即终端作业，匹配 BullMQ 的 `removeOnComplete: true` 和
`removeOnFail: true`。使用 `JobRetention` 进行 BullMQ 样式的 `KeepJobs` 保留：
通过 TTL 支持的重复数据删除，Redis 仍然保留原始重复数据删除所有者密钥
直到其 TTL 过期，即使已完成的作业记录立即被删除。
`count` 保留最新的 N 个已完成或失败的作业，`age` 驱逐较旧的作业
比另一个作业达到相同终止状态的持续时间，并且 `limit`
限制每个年龄清理过程。

```rust
# use a3s_lane::{JobOptions, JobRetention};
# use std::time::Duration;
let options = JobOptions::new()
    .with_completion_retention(JobRetention::count(1_000))
    .with_failure_retention(
        JobRetention::age_and_count(Duration::from_secs(7 * 24 * 60 * 60), 10_000)
            .with_limit(1_000),
    );
```

```rust
# use a3s_lane::{InMemoryJobQueue, JobOptions, JobQueueBackend};
# async fn events_example() -> a3s_lane::Result<()> {
let queue = InMemoryJobQueue::new("email");
let job = queue
    .add_job("send".to_string(), serde_json::json!({ "to": "ops@example.com" }), JobOptions::new())
    .await?;

let events = queue.read_events("-", "+", 100).await?;
assert_eq!(events[0].event, "added");
assert_eq!(events[0].job_id.as_deref(), Some(job.id.as_str()));

queue.trim_events(10_000).await?;
# Ok(())
# }
```

每一份声称的工作都带有一个不透明的`lock_token`。工人必须传递该令牌
至 `complete_job()`、`fail_job()`、`fail_job_discarding_retry()`，以及
`renew_lease()`。这可以防止过时的工作人员完成或失败工作
租约到期后，另一名工人收回了它。活跃的租赁职位
无法通过普通管理API删除；首先运行停滞恢复
当工人租约到期时。

流作业在单个操作中创建一个父作业和一个或多个子作业。
父级从`waiting_children`开始，子级通常被认领，并且
在每个剩余的子进程完成或完成后，父进程被释放到`waiting`
已删除。默认情况下，终端子失败会导致父失败；可重试的孩子
失败会使父进程阻塞，直到子进程重试并到达终端
结果。活动的父作业也可以使用其锁调用 `add_flow_children()`
原子添加子项并将其自身移动到 `waiting_children` 的令牌；这个
是 BullMQ 的 `moveToWaitingChildren()` 背后的动态规划器/扇出形状。
当提交的流程父级使用现有的自定义作业 ID 时，Lane 如下
BullMQ的`addParentJob`重复路径：保留存储的父数据，
为父 id 发出`duplicated`，提交的子项仍然是
根据正常的子规则添加、附加、删除重复或跳过。
当动态添加的子项使用现有的自定义作业 ID 时，Lane 会保留
现有子数据，发出`duplicated`，更新`parent_id`，记录待处理
为未完成的孩子提供依赖，并让完成的孩子满足
立即产生依赖性。
动态子项遵循与静态流相同的 BullMQ 重复数据删除路径
孩子们。与现有重复数据删除所有者匹配的子候选者是
跳过，在所有者 ID 上发出 `debounced` 和 `deduplicated`，并且不是
附加到活动父级。如果匹配的所有者处于活动状态且候选者
使用`keep_last_if_active(true)`，Lane将最新的候选者存储为下一个该父母的孩子；父母留在`waiting_children`直到主人
最终确定，下一个孩子实现。
可选儿童可以使用
`JobOptions::new().with_ignore_dependency_on_failure(true)` 镜像 BullMQ
`ignoreDependencyOnFailure`：终端故障将该子节点从
父级仍然阻塞的依赖集，将其视为已忽略，并释放
一旦剩余的依赖项完成，父级。
`JobOptions::new().with_remove_dependency_on_failure(true)` 镜像 BullMQ
`removeDependencyOnFailure`：终端故障也会将子进程从
仍然阻塞的依赖项集，但不会将其添加到忽略的依赖项中
计数。
`JobOptions::new().with_continue_parent_on_failure(true)` 镜像 BullMQ
`continueParentOnFailure`：终端故障将子进程从
仍然阻塞的依赖集，记录父检查的失败，以及
立即将父级移动到`waiting`或`delayed`，而不是等待
剩余的依赖项。
`JobOptions::new().with_fail_parent_on_failure(true)` 镜像 BullMQ
`failParentOnFailure`：终端故障将子进程从
仍然阻塞的依赖集，提前释放父级并延迟失败，
并让工作进程在运行父处理器之前使父进程失败。
家长可在扇入释放后拨打`get_flow_children_values()`取回
完成的子返回值，镜像 BullMQ 的 `getChildrenValues()`。
`get_flow_ignored_children_failures()` 镜像 BullMQ
`getIgnoredChildrenFailures()` 并返回配置了子项的失败
`ignoreDependencyOnFailure`或`continueParentOnFailure`；删除了依赖
故意忽略失败。

```rust
use a3s_lane::{
    InMemoryJobQueue, JobFlowDependencyCountOptions, JobFlowDependencyKind,
    JobFlowDependencyPageCursor, JobFlowDependencyPageOptions, JobFlowDependencyPagesOptions,
    JobOptions, JobSpec, JobState,
};

# async fn flow_example() -> a3s_lane::Result<()> {
let queue = InMemoryJobQueue::new("reports");

let flow = queue
    .add_flow(
        JobSpec::new("aggregate", serde_json::json!({ "report": "daily" }))
            .with_options(JobOptions::new().with_priority(1)),
        vec![
            JobSpec::new("fetch-us", serde_json::json!({ "region": "us" })),
            JobSpec::new("fetch-eu", serde_json::json!({ "region": "eu" })),
        ],
    )
    .await?;

assert_eq!(flow.parent.state, JobState::WaitingChildren);

let dependencies = queue.get_flow_dependencies(&flow.parent.id).await?.unwrap();
assert_eq!(dependencies.pending_child_ids.len(), 2);
assert!(dependencies.missing_child_ids.is_empty());

let counts = queue
    .get_flow_dependency_counts(&flow.parent.id)
    .await?
    .unwrap();
assert_eq!(counts.unprocessed, 2);

let selected_counts = queue
    .get_flow_dependency_selected_counts(
        &flow.parent.id,
        JobFlowDependencyCountOptions::new().with_unprocessed(true),
    )
    .await?
    .unwrap();
assert_eq!(selected_counts.unprocessed, Some(2));
assert_eq!(selected_counts.processed, None);

let dependency_values = queue
    .get_flow_dependency_values(&flow.parent.id)
    .await?
    .unwrap();
assert_eq!(dependency_values.unprocessed.len(), 2);

let pending_page = queue
    .get_flow_dependency_page(
        &flow.parent.id,
        JobFlowDependencyPageOptions::new(JobFlowDependencyKind::Unprocessed).with_count(20),
    )
    .await?
    .unwrap();
assert_eq!(pending_page.items.len(), 2);
assert_eq!(pending_page.next_cursor, 0);

let dependency_pages = queue
    .get_flow_dependency_pages(
        &flow.parent.id,
        JobFlowDependencyPagesOptions::new()
            .with_unprocessed(JobFlowDependencyPageCursor::new().with_count(20)),
    )
    .await?
    .unwrap();
assert_eq!(
    dependency_pages
        .get(JobFlowDependencyKind::Unprocessed)
        .unwrap()
        .items
        .len(),
    2
);

let removed = queue
    .remove_unprocessed_children(&flow.parent.id, chrono::Utc::now())
    .await?
    .unwrap();
assert_eq!(removed.len(), 2);

let detached_flow = queue
    .add_flow(
        JobSpec::new("aggregate-detach", serde_json::json!({})),
        vec![JobSpec::new("optional-child", serde_json::json!({}))],
    )
    .await?;
assert!(queue
    .remove_child_dependency(&detached_flow.children[0].id, chrono::Utc::now())
    .await?);
# Ok(())
# }
```

重复作业在成功完成后安排下一次发生。使用
`RepeatOptions::every()` 用于固定间隔或 `RepeatOptions::cron()` 用于固定间隔
七字段 UTC cron 表达式。重复`limit`计算总执行次数，
包括第一份工作。自定义重复键还充当系列所有者：而
存在具有相同重复键的非终结符，重复添加返回
该所有者而不是创建并行重复链。在Redis中，重复
重复添加可以通过验证从丢失的快速所有者密钥中恢复
`repeat_meta:<key>.jid` 并在返回之前恢复 `repeat:<key>`
当前所有者。添加、批量添加、流添加、动态流子级和重复
upserts 拒绝 `end_at` 早于添加时间戳的重复选项，
匹配 BullMQ 的 `endDate` 添加时间保护并避免部分写入：

```rust
use a3s_lane::{InMemoryJobQueue, JobOptions, JobSpec, RepeatOptions};
use std::time::Duration;

# async fn repeat_example() -> a3s_lane::Result<()> {
let queue = InMemoryJobQueue::new("sync");

let job = queue
    .add(
        "heartbeat",
        serde_json::json!({ "target": "crm" }),
        JobOptions::new().with_repeat(
            RepeatOptions::every(Duration::from_secs(60))
                .with_limit(10)
                .with_key("crm-heartbeat"),
        ),
    )
    .await?;

assert_eq!(job.repeat_key.as_deref(), Some("crm-heartbeat"));

let cron_job = queue
    .add(
        "nightly-import",
        serde_json::json!({ "target": "warehouse" }),
        JobOptions::new().with_repeat(
            RepeatOptions::cron("0 0 2 * * * *")
                .with_limit(30)
                .with_key("warehouse-nightly-import"),
        ),
    )
    .await?;

assert_eq!(
    cron_job.repeat_key.as_deref(),
    Some("warehouse-nightly-import")
);

let duplicate = queue
    .add(
        "heartbeat",
        serde_json::json!({ "target": "crm", "duplicate": true }),
        JobOptions::new().with_repeat(
            RepeatOptions::every(Duration::from_secs(60))
                .with_limit(10)
                .with_key("crm-heartbeat"),
        ),
    )
    .await?;

assert_eq!(duplicate.id, job.id);

let replacement = queue
    .upsert_repeat(
        JobSpec::new(
            "heartbeat-v2",
            serde_json::json!({ "target": "crm", "template": "v2" }),
        )
        .with_options(
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(30))
                    .with_limit(10)
                    .with_key("crm-heartbeat"),
            ),
        ),
        chrono::Utc::now(),
    )
    .await?;

assert_ne!(replacement.id, job.id);

let repeats = queue.list_repeats().await?;
assert_eq!(repeats[0].key, "crm-heartbeat");
assert_eq!(repeats[0].job_id, replacement.id);

let removed = queue.remove_repeat("crm-heartbeat").await?;
assert_eq!(
    removed.as_ref().map(|job| job.id.as_str()),
    Some(replacement.id.as_str())
);
# Ok(())
# }
```

简单的重复数据删除在第一次匹配时合并重复的提交
作业拥有其重复数据删除 ID。可选的 TTL 限制所有者密钥的长度
阻止重复项，包括当所有者已经完成或失败时：

```rust
use a3s_lane::{DeduplicationOptions, InMemoryJobQueue, JobOptions, JobQueueBackend};
use chrono::Utc;
use std::time::Duration;

# async fn dedup_example() -> a3s_lane::Result<()> {
let queue = InMemoryJobQueue::new("billing");

let first = queue
    .add(
        "recalculate-account",
        serde_json::json!({ "account_id": "acct_42" }),
        JobOptions::new().with_deduplication_id("account:acct_42"),
    )
    .await?;

let duplicate = queue
    .add(
        "recalculate-account",
        serde_json::json!({ "account_id": "acct_42", "duplicate": true }),
        JobOptions::new().with_deduplication_id("account:acct_42"),
    )
    .await?;

assert_eq!(duplicate.id, first.id);

let ttl_owner = queue
    .add(
        "refresh-account",
        serde_json::json!({ "account_id": "acct_42" }),
        JobOptions::new().with_deduplication(
            DeduplicationOptions::new("account-refresh:acct_42")
                .with_ttl(Duration::from_secs(30))
                .extend_ttl(true),
        ),
    )
    .await?;

assert!(ttl_owner.deduplication_expires_at.is_some());

let delayed_owner = queue
    .add(
        "refresh-account",
        serde_json::json!({ "account_id": "acct_42", "version": 1 }),
        JobOptions::new()
            .with_delay(Duration::from_secs(60))
            .with_deduplication(
                DeduplicationOptions::new("account-refresh:acct_42")
                    .replace_delayed(true),
            ),
    )
    .await?;

let replacement = queue
    .add(
        "refresh-account",
        serde_json::json!({ "account_id": "acct_42", "version": 2 }),
        JobOptions::new()
            .with_delay(Duration::from_secs(60))
            .with_deduplication(
                DeduplicationOptions::new("account-refresh:acct_42")
                    .replace_delayed(true),
            ),
    )
    .await?;

assert_ne!(replacement.id, delayed_owner.id);

let active_owner = queue
    .add(
        "sync-account",
        serde_json::json!({ "account_id": "acct_42", "version": 1 }),
        JobOptions::new().with_deduplication(
            DeduplicationOptions::new("account-sync:acct_42")
                .keep_last_if_active(true),
        ),
    )
    .await?;
let claimed = queue
    .claim_next("worker-a".to_string(), Duration::from_secs(30), Utc::now())
    .await?
    .expect("job should be claimable");

let duplicate = queue
    .add(
        "sync-account",
        serde_json::json!({ "account_id": "acct_42", "version": 2 }),
        JobOptions::new().with_deduplication(
            DeduplicationOptions::new("account-sync:acct_42")
                .keep_last_if_active(true),
        ),
    )
    .await?;

assert_eq!(duplicate.id, active_owner.id);
queue
    .complete_job(
        &claimed.id,
        claimed.lock_token.as_deref().expect("claimed jobs have locks"),
        serde_json::json!({ "ok": true }),
        Utc::now(),
    )
    .await?;
# Ok(())
# }
```

当前的重复数据删除模式有意覆盖BullMQ的简单模式。一个
没有 TTL 的重复数据删除 ID 会阻止重复添加，直到所属作业为止
完成、最终失败、被删除或被清理。 TTL支持的
重复数据删除 id 遵循 BullMQ 的 Redis 终结规则：完成和
终端故障保留所有者密钥，同时其 Redis TTL 仍为正，因此
重复项继续返回保留的终端所有者，直到 TTL 过期
当保留该终端作业记录时。当`remove_on_complete(true)`时，
`remove_on_fail(true)`，或完成作业保留删除作业记录
同样的移动到完成回合，Redis 重复数据删除密钥仍然会过期
类似于 BullMQ 的 Lua 路径，但 Lane 的高级添加/获取 API 需要可用的作业
快照，并可能在接受以后的替换之前删除丢失的所有者。
移除式路径，例如显式移除、清理、排水和手动
`remove_deduplication_key()`立即清除所有者。
`extend_ttl(true)` 涵盖 BullMQ 的去抖扩展路径：重复添加
返回当前所有者并刷新重复数据删除 TTL，而不是允许
所有者密钥将在原定截止日期到期。
`replace_delayed(true)` 还涵盖了 BullMQ 的延迟所有者替换路径：一个新的
重复数据删除添加可能会删除延迟的独立所有者并将新作业插入
当旧所有者仍然存在于延迟索引中时进行相同的操作。
对于 TTL 支持的延迟更换，更换保留了现有所有者
默认key的剩余TTL；当`extend_ttl(true)`也被设置时，替换而是刷新 TTL。
`keep_last_if_active(true)` 涵盖 BullMQ 的活动所有者保留最后路径
独立和重复系列作业：当前所有者在时添加的重复项
主动返回该所有者，覆盖队列本地下一个作业记录，以及
当所有者完成时，仅实现最新的副本，最终失败，
或耗尽停滞的作业恢复。如果最新的副本有延迟，则延迟
从所有者最终确定时间戳开始。对于重复系列，最新
重复成为相同重复键的下一个出现并替换
该最终轮次的常规继任者。对于流动父母来说
内存中/本地运行时，当父所有者处于运行状态时提交的重复流
active 存储最新的替换父项和子项，然后实现
当活动父进程完成时流动。 Redis Lua 现在覆盖活动父级
流程的完成、终端故障和停滞的终端故障路径
保持最后。
普通流程子级去重遵循 BullMQ 的子级添加路径
静态 `add_flow()` 和动态 `add_flow_children()`：如果是子候选
匹配现有的重复数据删除所有者，Lane 返回并发出事件
所有者，跳过存储候选子项，使所有者与新的子项分离
父级，并且仅将未跳过的子级记录为新父级的待处理
依赖关系。当该子重复数据删除使用`keep_last_if_active`并且
所有者处于活动状态，Lane 将最新的候选者存储为下一个孩子。这活动所有者仍然拥有重复数据删除 ID，并且所有者最终确定得以实现
最新的孩子并将其注册为候选父母的依赖者。
重试失败的重复数据删除作业会在作业执行期间回收重复数据删除 ID。
等待或再次活动；如果有另一个实时重复数据删除所有者，则重试会被拒绝，
包括保留的终端 TTL 所有者，已经拥有该 ID。
`remove_deduplication_key()` 清除队列的当前所有者
最终确定之前或保留的终端 TTL 窗口期间的重复数据删除 ID，
匹配 BullMQ 的队列级别 `removeDeduplicationKey()` 删除行为
Redis 重复数据删除键。原来的工作仍保持当前状态，但是
具有相同重复数据删除 ID 的后续提交可以成为新的所有者。
`get_deduplication_job_id()` 返回当前可用的所有者作业 ID
重复数据删除 ID； Redis 后端验证所有者作业快照而不是
盲目地暴露孤立的原始密钥。

当进程本地运行时需要持久重启时使用`LocalJobQueue`
恢复。它的JSON快照存储作业、事件、重复数据删除后续作业、
已发布的重复数据删除所有者和父级范围的流依赖项索引
因此终端子返回值和忽略/失败父失败标记仍然存在
普通子进程清理和进程重启：

```rust
use a3s_lane::{JobOptions, JobQueueBackend, LocalJobQueue};
use std::path::PathBuf;

# async fn durable_example() -> a3s_lane::Result<()> {
let queue = LocalJobQueue::open("email", PathBuf::from("./lane-jobs/email.json")).await?;
let job = queue
    .add(
        "send",
        serde_json::json!({ "to": "ops@example.com" }),
        JobOptions::new().with_priority(10),
    )
    .await?;

let claimed = queue
    .claim_next("worker-1".to_string(), std::time::Duration::from_secs(30), chrono::Utc::now())
    .await?;

if let Some(claimed) = claimed {
    let lock_token = claimed
        .lock_token
        .as_deref()
        .expect("claimed jobs include a lock token");
    queue
        .complete_job(
            &claimed.id,
            lock_token,
            serde_json::json!({ "ok": true }),
            chrono::Utc::now(),
        )
        .await?;
}
# Ok(())
# }
```

当多个工人或进程需要向
相同的持久优先级队列。它将作业以 JSON 形式存储在 Redis 哈希、索引中
具有排序集的状态，将保留的作业日志存储在每个作业的 Redis 列表中，以及
使用 Lua 脚本自动添加工作、提升因延迟而产生的工作、领取工作、
和过渡租赁工作。 Redis 后端遵循核心 BullMQ 锁定
机制：声明为作业创建一个独立的 TTL 锁定密钥，并且
完成、失败、发布、延迟和更新操作必须证明所有权
在脚本改变之前匹配锁定令牌
活动/已完成/失败/延迟索引。活动 `get_job()` 快照读取到
将密钥锁定回来，以便管理调用者可以检查当前的租赁令牌。
`renew_leases()` 镜像 BullMQ
`extendLocks` 形状：Redis 在 Lua 一轮中检查每个令牌，更新有效锁
键，更新活动租赁分数和保留的作业快照，删除成功的
`stalled` 候选集中的作业，并仅返回失败的作业 ID。
停滞恢复使用 BullMQ 的两阶段候选集形状：恢复通道
将活动作业记录在`stalled`集中，用于下一次传递，成功
更新/最终化脚本从该集合中删除该作业，并且仅删除其稍后通过的作业
候选人没有 TTL 锁可以重新排队或失败作业：

```rust
use a3s_lane::{JobOptions, JobQueueBackend, JobRateLimit, RedisJobQueue, RetryPolicy};
use std::time::Duration;

# async fn redis_example() -> a3s_lane::Result<()> {
let queue = RedisJobQueue::with_namespace(
    "redis://127.0.0.1/",
    "a3s:lane",
    "email",
)?;
queue.set_claim_rate_limit(JobRateLimit::new(100, Duration::from_secs(60))).await?;
assert_eq!(
    queue.get_claim_rate_limit().await?,
    Some(JobRateLimit::new(100, Duration::from_secs(60)))
);
let _rate_limit_ttl_ms = queue.get_claim_rate_limit_ttl(None).await?;
queue.rate_limit_claims_for(Duration::from_millis(500)).await?;
queue.clear_claim_rate_limit_key().await?;
queue.set_max_active_jobs(32).await?;
assert_eq!(queue.get_max_active_jobs().await?, Some(32));
assert!(!queue.is_maxed().await?);

let job = queue
    .add_job(
        "send".to_string(),
        serde_json::json!({ "to": "ops@example.com" }),
        JobOptions::new()
            .with_priority(10)
            .with_retry_policy(RetryPolicy::fixed(3, Duration::from_secs(1))),
    )
    .await?;

if let Some(claimed) = queue
    .claim_next_blocking(
        "worker-1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(10),
    )
    .await?
{
    let lock_token = claimed
        .lock_token
        .as_deref()
        .expect("claimed jobs include a lock token");
    queue
        .complete_job(
            &claimed.id,
            lock_token,
            serde_json::json!({ "ok": true }),
            chrono::Utc::now(),
        )
        .await?;
}

assert_eq!(queue.get_job(&job.id).await?.map(|job| job.name), Some("send".to_string()));
# Ok(())
# }
```

`with_claim_rate_limit()` 配置工人本地索赔率限制，同时
通过 Redis 为使用相同命名空间的工作人员共享计数器密钥
和队列。 `set_claim_rate_limit()` 将共享配置存储在队列中
元哈希为 `max` 和 `duration`，匹配 BullMQ 的全局速率限制
机制。 `get_claim_rate_limit()` 使用 `HMGET` 读取这些字段，并且
`get_claim_rate_limit_ttl()` 遵循 BullMQ 的 `getRateLimitTtl` 脚本形状：
具有明确的最大值，仅在限制器计数器达到后才返回 TTL
该阈值，否则它会使用 Redis 共享 `meta.max` 存在并下降
回到原始的`PTTL`作为限制器键。 `rate_limit_claims_for()`镜子
BullMQ的手动`rateLimit()`路径通过将限制器键设置为非常大
具有毫秒 TTL 的计数器； `clear_claim_rate_limit_key()`镜子
`removeRateLimitKey()` 通过删除该限制器密钥而不更改共享
配置。 `clear_claim_rate_limit()` 删除共享配置字段。的
Lua 声明脚本更喜欢明确的本地工人限制，否则读取
检查速率限制计数器之前的 Redis 元值。当窗户是
耗尽，`claim_next()`返回`None`并且作业仍在等待稍后
民意调查。 `claim_next_blocking()` 反映 BullMQ 的工作端限制器延迟
在空声明后检查活动限制器 TTL，并休眠直到
限制器窗口可以接纳另一项工作，但以工作人员的阻塞期限为上限。

`set_max_active_jobs()` 配置 Redis 共享的活动作业上限
队列。它将队列元哈希中的值存储为`concurrency`，匹配
BullMQ 的队列最大机制。 `get_max_active_jobs()` 读取相同的元数据
字段，镜像 BullMQ 的全局并发 getter。 `is_maxed()`镜子
BullMQ 的 `isMaxed()` 队列 getter 通过读取 `meta.concurrency` 和活动的
Lua 一轮中的有序集计数。 Lua声明脚本读取元值，
检查同一个 Redis 轮中的活动排序集计数，并返回 `None`
当队列已经存在时，无需移动作业或消耗速率限制容量
最大化。 `clear_max_active_jobs()` 移除共享天花板。

与 BullMQ 的 `moveToActive` 脚本一样，Redis 声称也会促进由于延迟的作业
在检查暂停、速率限制、最大活动和之前，在同一个 Lua 脚本中
下一个索赔。暂停或已满的队列仍可以由于延迟的作业而移回
`waiting`；它只是返回 `None` 而不是租赁工作。在那暂停或
maxed 分支，Lane 会像 BullMQ 一样抑制基础工作标记
`addBaseMarkerIfNeeded(markerKey, isPausedOrMaxed)`帮手，所以延迟推广
在队列恢复或活动槽打开之前不会唤醒其他工作人员。
在移动等待索引条目之前，声明还会验证存储的作业状态
到`active`，修剪陈旧的等待排序集条目而不是重新激活
已经转移到其他地方的工作岗位。

Redis还在同一个Lua中维护了一个BullMQ风格的队列`marker`zset
将作业移动到 `waiting` 或 `delayed` 的状态转换。等待写入添加
成员`0`，得分为`0`；延迟写入和延迟删除刷新成员`1`
到最早的延迟分数，反映 BullMQ 的 `addBaseMarkerIfNeeded` 和
`addDelayMarkerIfNeeded`唤醒机制。 `claim_next_blocking()` 使用
专用 Redis 连接到标记集的`BZPOPMIN`，处理弹出的内容
标记仅作为唤醒信号，然后重新运行正常的 Lua 声明路径，以便
暂停、速率限制、最大活跃、延迟升级和锁定所有权检查保留
原子的。成功的声明会重写基本标记以扇出多个被阻止的
工作人员处理批量添加的作业，并暂停/恢复更新标记集，以便恢复
队列唤醒沉睡的 Redis 工作人员。活动工作完成路径也刷新
每当等待工作剩余时，基本标记，因此完成，最终
失败、重试延迟或手动延迟租用作业会唤醒阻塞的 Redis
`set_max_active_jobs()` 插槽可用后的工作人员。
`JobQueueBackend::claim_next_blocking()` 将该等待路径暴露给
与后端无关`JobWorker`；非阻塞后端使用默认的立即数
`claim_next()` 后备，而 Redis 工作线程使用标记支持的 `BZPOPMIN`
当队列当前不受速率限制时的路径。

Redis 添加也由 Lua 支持。添加脚本写入作业 JSON 和
在同一个 Redis 轮次中等待、延迟或等待子索引。如果定制
作业 ID 已存在，脚本返回现有作业而不推进
等待序列或写入重复的状态索引。 Lane 拒绝自定义作业 ID
等于`0`，以`0:`为前缀，或脚本执行前的纯整数，
匹配 BullMQ 的保留标记命名空间和整数 ID 保护。 Redis 脚本
在申请、列出和晋升工作时使用特殊情况的类似标记的值，因此
Lane 将这些 ID 保留在用户作业 ID 命名空间之外。批量添加同样如此
一个脚本调用中的机制，同时保留调用者的输入顺序，包括
与 BullMQ 的管道式 `addBulk()` 发出的重复数据删除流事件相同
对于每项工作。
对于简单的重复数据删除，相同的添加脚本使用独立的
`deduplication:<id>` key，相当于BullMQ的`de:<id>`作用，返回
当前所有者在写入副本之前。如果`DeduplicationOptions`有TTL，
Lua 脚本使用 `PX` 写入所有者密钥，以便 Redis 过期
重复数据删除窗口，即使原始作业稍后完成或之前失败
TTL 确实如此。完成、终端失败和停滞的终端失败脚本
镜像 BullMQ 的 `removeDeduplicationKeyIfNeededOnFinalization`：他们删除了一个
仅当 Redis 报告没有 TTL (`PTTL == -1`) 或过期时才匹配所有者密钥
TTL 为零，并保留具有正 TTL 的密钥。 keep-last-if-active 模式故意省略 TTL，匹配 BullMQ 的主动所有者行为，因此关键
当工作仍在租赁期间时不能过期。如果
`extend_ttl(true)`已设置，重复添加之前用`PX`刷新所有者密钥
返回当前所有者，匹配 BullMQ 的 debounce 扩展分支。
如果设置了`replace_delayed(true)`并且当前所有者是独立的延迟
作业中，添加脚本首先删除旧的延迟 zset 成员，然后删除
旧作业哈希并仅在延迟删除成功时插入新所有者，
镜像 BullMQ 的延迟替换分支。通过 TTL 支持的重复数据删除，
脚本使用 Redis `KEEPTTL` 更新所有者 ID，因此替换不会扩展
剩余的重复数据删除窗口，除非还设置了 `extend_ttl(true)`。那
同一分支发出 BullMQ 风格的 `removed prev=delayed`、`debounced` 和
替换作业自己的添加/状态事件之前的`deduplicated` 事件。如果
`keep_last_if_active(true)` 已设置且当前所有者存在于
活动排序集，重复添加覆盖 a
`deduplication_next:<id>` 原始作业记录和 `PERSIST` 所有者密钥。对于
独立和重复作业、完成、终端失败和停滞的终端失败
然后脚本自动删除旧的所有者密钥，实现最新的密钥
proto-job 进入等待或延迟状态，并将重复数据删除所有者设置为
新工作。当所有者和最新的重复时
共享相同的重复密钥，终结脚本也会递增
`repeat_count`，将 `repeat:<key>` 所有者设置为具体化的最新作业，并且
抑制该回合的常规重复后继者。这保留了单所有者重复不变性，同时匹配 BullMQ 的 keep-last 重新排队
机制，其中 dedup-next 记录在作业完成期间消耗，而不是
而不是通过稍后的客户端传递。 Flow keep-last 使用相同的
`deduplication_next:<id>` 带流量包络的密钥； Redis目前已实现
活动父完成、终端故障或终端停滞的信封
失败。活动流子级保留最后重复数据删除存储下一个子级及其
父关系并将物化子项注册到父依赖项中
当活动所有者完成时设置。流父级重复数据删除遵循 BullMQ 的
`addParentJob` 路径：
重复的父提交返回当前所有者流程并写入
所有者父 ID 上的 `debounced` 和 `deduplicated` 事件；主动保持最后
流程重复写入相同的事件，同时替换挂起的事件
`deduplication_next:<id>` 流量包络线。 Redis 删除路径镜像 BullMQ 的
删除助手：删除时，
clean、drain、repeat upsert 或 flow 未处理子项删除会删除作业
仍然拥有 `deduplication:<id>`，它也会清除 `deduplication_next:<id>` 所以
以前活跃的所有者不能留下陈旧的影子工作。

等待顺序是模仿 BullMQ 的 Redis 级别机制而不仅仅是
匹配其选项名称。在 BullMQ 5.79.3 中，标准作业使用 Redis 列表：
`opts.lifo`选择`RPUSH`，FIFO使用`LPUSH`，worker从
尾部有`RPOPLPUSH`；优先作业使用一个排序集，其分数为
`priority * 0x100000000 + counter` 和 `changePriority(..., lifo: true)` 放置
位于其相同优先级分数范围前面的作业。巷子里的商店都在等待
作业在一个排序集中，因此每个将作业移动到 `waiting` 的 Lua 脚本
增加队列序列，将该值写入 `job.enqueued_seq`，并且
计算优先级分桶分数。每个优先级桶的下半部分是
为具有相反顺序的 LIFO 条目保留，上半部分是
为具有正向序列顺序的 FIFO 条目保留。这保持了`ZRANGE`
要求优先级第一，最新的 LIFO 在较旧的 LIFO 之前，LIFO 在 FIFO 之前
相同的优先级，最旧的 FIFO 在较新的 FIFO 之前，同时保留
`get_counts_per_priority()` 作为同一优先级存储桶上的 `ZCOUNT`。
`release_active_job()` 将返回的作业写入其优先级的开头
存储桶，镜像 BullMQ 的 `pushBackJobWithPriority()` 优先级分数
工作和标准等候名单工作的`RPUSH`前端消费行为；
如果多个已发布的作业共享该确切分数，Redis 将按作业 ID 对它们进行排序。

完成的作业保留遵循 BullMQ 的底层 `moveToFinished` 机制
而不是仅匹配 `removeOnComplete` 和 `removeOnFail` 选项
名称。在 BullMQ 5.79.3 中，这些选项被标准化为`keepJobs`； `true`
变成`{ count: 0 }`，`false`变成无限保留，一个数字变成
`{ count: number }`，并且一个对象可以携带`age`、`count`和`limit`。的
`moveToFinished-14.lua` 脚本将当前作业写入已完成或失败
zset 以完成时间戳作为分数，然后调用
`removeJobsByMaxAge(timestamp, maxAge, targetSet, prefix, maxLimit)` 和
`removeJobsByMaxCount(maxCount, targetSet, prefix)` 在同一个 Lua 回合中。
Lane 镜像存储级行为：Redis 完成、终端失败、
停滞的终端故障和首先使父级失败的流清理脚本
完成工作，然后应用年龄清理，然后根据年龄进行清理
终端 zset，同时删除作业哈希、日志列表和依赖项集
删除已完成的作业。就像 BullMQ 的 `moveToFinished` 脚本一样，这个完成的工作
记录清理不会删除重复数据删除所有者密钥； TTL 支持的所有者
继续存在，直到 Redis 过期，而无 TTL 所有者已经存在
在定稿期间发布。
内存中和本地持久队列对 `finished_at` 使用相同的顺序
时间戳。年龄清理是尽力而为，就像 BullMQ 一样：没有背景
计时器，因此只有当后续作业出现时，才会删除超龄完成或失败的作业
进入相同的终止状态。

队列事件遵循 BullMQ 的 Redis 流机制。 BullMQ的Lua脚本编写
`XADD <queue>:events`的全局队列事件，常用
`MAXLEN ~ maxEvents`，默认保留 10,000 个条目； `QueueEvents`
然后通过事件 ID 从该流中读取。车道镜那储物形状为
Redis 后端每个队列有一个 `events` 流。 Lua状态转换写的是
与作业突变相同的 Redis 回合中的事件：add 写入 `added` 后跟
`waiting`、`delayed`、或`waiting-children`；索赔写`active prev=waiting`；
补全将 `completed prev=active` 与 `returnvalue` 写入；失败写入
`failed` 或使用 `failedReason` 重试 `delayed`，以及终端故障
尝试计数已耗尽，还可以编写 BullMQ 风格的 `retries-exhausted` ，
`attemptsMade`；已完成和终端失败的移动到完成的路径写入
当没有等待或活动作业剩余时，队列级`drained`事件；流子
完成、终端故障和停滞的终端故障路径也会发出父级
`waiting`、`delayed` 或 `failed` 事件与 `prev=waiting-children` 时
同一个 Lua 回合释放父级或使父级失败；
显式删除为删除的作业写入`removed prev=<state>`； `clean_jobs()`
删除老化作业后写入队列级`cleaned count=<n>`事件；
去重添加，包括批量添加，编写 BullMQ 风格 `debounced` 和
`deduplicated` 具有所有者作业 ID、重复数据删除 ID 并已跳过的事件
候选人职位 ID；流父重复数据删除写入相同的事件对
所有者父 ID 和跳过的候选父 ID；普通流子
重复数据删除将事件对写入现有子所有者，同时忽略从新的父依赖集中跳过候选；流程子自定义作业 ID
当重复项在保留的子 id 上写入 BullMQ 样式 `duplicated` 时
现有的孩子依附于新的父母；延迟业主更换也
为旧所有者写入 `removed prev=delayed`，然后是 `debounced` 和
`deduplicated` 替换作业 ID 上的事件；进度写道
`progress data=<json>`；暂停/恢复写入队列级事件。
`read_events()` 在流 ID 上使用 `XRANGE`，并且
`trim_events()` 使用 BullMQ 风格的 `XTRIM MAXLEN ~`。内存中和本地
持久后端在其快照中保留相同的保留事件条目，因此
测试和嵌入式运行时在没有 Redis 的情况下公开相同的合约。喜欢
BullMQ的`addLog`脚本，Lane作业日志保留保留日志列表并且不
发出队列事件；进度更新确实如此。

完成、终端故障和停滞的终端故障脚本使用
BullMQ 风格的重复数据删除键最终确定语义：匹配的所有者键
没有 TTL 的密钥被释放，而具有正 TTL 的匹配密钥将保留，直到
Redis 会使其过期，即使 `remove_on_complete(true)`、`remove_on_fail(true)` 或
完成作业保留立即删除完成的作业记录。删除，
clean、drain、repeat upsert 和 flow 子项删除路径使用删除语义
相反：他们释放匹配的所有者密钥并清除配对的密钥
`deduplication_next:<id>` 影子记录，匹配 BullMQ 的删除清理
保留最后的重复数据删除。
手动重试回收重试脚本内的密钥，重新应用 TTL，并且
如果有更新的非终端作业，则拒绝将失败的作业移回等待状态
已拥有相同的重复数据删除 ID。
`remove_deduplication_key()`直接删除`deduplication:<id>`，所以稍后
即使旧所有者仍然是非终端，add 也可以声明相同的 id。当一个
keep-last 所有者有一个待定的继任者，释放也清除
`deduplication_next:<id>` 所以旧的活跃所有者无法实现陈旧的
手动释放id后重复。内存中和本地持久
后端通过跟踪发布的所有者 ID 来保持相同的逻辑发布
他们的快照，而不是仅仅依赖客户端扫描。
`get_deduplication_job_id()` 查阅相同的 `deduplication:<id>` 密钥。不像
BullMQ 的原始 `GET de:<id>` getter，Lane 验证所有者仍然可以
作为作业返回 API 表面的作业快照加载；如果关键点在作业缺失或不匹配，或者终端作业没有明确的 TTL 所有者
密钥，它会清除过时的所有者密钥和任何孤立的密钥
`deduplication_next:<id>` 举报无主前记录。终端作业
正的 TTL 和保留的作业记录仍然是有效的重复数据删除所有者，直到
Redis 使密钥过期。

Redis流提交是全有或全无：流添加脚本写入父级，
新子项、现有父项和现有子项附件、状态索引、
队列事件，以及父级的挂起依赖项在一个 Redis 回合中设置。
同一提交流程中的重复 ID 将被拒绝。现有家长
自定义作业 ID 遵循 BullMQ 的 `addParentJob` 重复路径：Lane 保留
存储的父快照，发出 `duplicated`，保留当前依赖集，
并仅添加提交的新子项、与该父项重复的子项，或者
重复数据删除保留最后的占位符。现有的子自定义作业 ID 如下
当 BullMQ 没有冲突的保留父级时的 `handleDuplicatedJob` 路径：
Lane 保留原始子数据，更新其 `parent_id`，发出 `duplicated`，
将未完成的子项添加到新的父项依赖项集中，并让
已经完成的孩子立即满足依赖性，以便父母可以
在同一回合留下`waiting_children`。如果现有的孩子仍然属于
对于不同的保留父级，流添加返回父级冲突错误
无需创建部分记录。如果儿童候选人
针对现有所有者进行重复数据删除，添加脚本会处理之前的情况
依赖项插入：跳过普通候选者，不跳过现有所有者
附加到新的父级，并且返回的流仅包含以下子级：
实际上被存储了。活跃的保留最后子候选者存储在
`deduplication_next:<id>` 及其父 ID；当所有者最终确定时，在同一 Redis 回合中，物化子级将添加到父级依赖项集中。
`get_flow_dependencies()` 使用 Redis 端读取脚本来加载父级和
一轮中作业哈希中每个保留的子快照，并返回
仍待保留或未保留的子 ID。
`get_flow_dependency_counts()` 遵循 BullMQ 的 `getDependencyCounts` Redis/Lua
机制而不是仅仅复制 API 名称。 BullMQ 5.79.3 计数
父级范围 `:processed`、`:dependencies`、`:failed` 和 `:unsuccessful`
具有 `HLEN`、`SCARD`、`HLEN` 和 `ZCARD` 的结构，忽略、删除和
由故障策略路径处理的持续故障。莱恩现在写同样的
`dependencies:<parent_id>:processed` 下的父范围 Redis 侧索引，
`dependencies:<parent_id>:failed`，以及
`dependencies:<parent_id>:unsuccessful`，同时在内存中且本地持久
队列保留等效的`JobQueueSnapshot.flow_dependency_indexes`条目。
子快照仍然可用于审核和兼容性回退。的
Redis count脚本一轮读取这些边索引并返回处理后，
未处理、失败、忽略和缺失的总数，而不返回每个子项
快照到客户端；内存/本地读者使用相同的权威
side-index-first 查看并回退到仅针对子级保留的子级快照
id 未被侧面索引覆盖。
删除的失败依赖项被故意从失败中省略并忽略
总计，匹配 BullMQ 的 `removeDependencyOnFailure` 行为。
`get_flow_dependency_selected_counts(parent_id, options)` 镜像 BullMQ
`Job.getDependenciesCount(opts)` 选择器语义。空选项默认为
四个 BullMQ 存储桶，而显式选项仅返回 `Some(count)`请求已处理、未处理、忽略和失败的存储桶。 Redis 读取
与 `HLEN`、`SCARD`、`HLEN` 和 `ZCARD` 相同的父范围侧索引，
匹配BullMQ的`getDependencyCounts-4.lua`机制并避免快照
当调用者只需要计数时扇出。巷子保持`get_flow_dependency_counts()`
作为具有 `missing` 和兼容性的扩展队列级计数快照
后备支持。
`get_flow_dependency_values(parent_id)` 镜像 BullMQ 的无选项
`Job.getDependencies()`路径：Redis读取相同的父级范围`:processed`，
`:dependencies`、`:failed` 和 `:unsuccessful` 结构以及 `HGETALL`，
`SMEMBERS`、`HGETALL` 和 `ZRANGE 0 -1`，将处理后的值解析为 JSON 和
忽略作为失败原因字符串的值。然后它合并保留的子项
那些未包含在这些辅助索引中的任何父子 ID 的快照，保留
在侧索引字段之前创建的流的全桶兼容性
可用。
`get_flow_dependency_page(parent_id, options)` 和
`get_flow_dependency_pages(parent_id, options)` 镜像 BullMQ 的分页
用于大扇出检查的`Job.getDependencies(opts)`路径。 Redis读取
`processed` 与 `HSCAN dependencies:<parent_id>:processed`, `unprocessed` 与
`SSCAN dependencies:<parent_id>`、`ignored` 与
`HSCAN dependencies:<parent_id>:failed` 和 `failed` 与
`ZRANGE dependencies:<parent_id>:unsuccessful`。多桶吸气剂保持
BullMQ 结果顺序并在一个 Lua 回合中读取所有请求的存储桶
将多个客户端往返缝合在一起。 `count`选项是Redis
扫描 hash 提示并设置桶，就像 BullMQ 一样；来电者应继续阅读
与返回的光标一起直到它变成`0`。对于混合升级数据，
初始`cursor = 0`页面还附加保留的子快照后备条目未包含在辅助索引中的；后面的游标页面仍然是纯Redis
光标扫描。
当孩子完成时，失败并显示 `ignore_dependency_on_failure` 或
`continue_parent_on_failure`，失败并显示`fail_parent_on_failure`，或者当
静态流或主动父扇出按自定义重用现有的已完成子项
id, Lane 镜像 BullMQ 的父级范围的侧索引路径，而不是仅依赖
关于子快照回退。与重复使用的已完成子项的混合流，
因此，新完成的子项、忽略的失败以及父项失败
读取跨 Redis、内存中和本地的权威依赖关系视图
耐用的后端。
完成流程父级会检查 Redis 依赖项集和
`dependencies:<parent_id>:unsuccessful` 离开活动状态前，匹配
BullMQ 的 `moveToFinished` 防护会拒绝具有挂起依赖项的作业或
不成功的子依赖项。当`continueParentOnFailure`释放
父母早，晚子完成仍然会将该孩子从依赖关系中删除
设置为父级只能在剩余所需的扇入完成后才能完成
解决了。
`get_flow_children_values()` 和 `get_flow_ignored_children_failures()` 关注
BullMQ 的 `getChildrenValues()` 和 `getIgnoredChildrenFailures()` 扇入
语义。 BullMQ 读取父级范围的 `:processed` 和 `:failed` 哈希值；莱恩的
Redis 读取脚本现在也更喜欢这些哈希值，然后合并保留的子项
侧面索引未涵盖的任何子 ID 的快照。这保留了
混合升级数据的兼容性，其中一些孩子在
存在辅助索引，后来的子级写入了新的父级范围的哈希值。返回值为 JSON `null` 的已完成子项在两者中均保持可见
侧面索引读取和保留快照回退读取。
`remove_unprocessed_children()` 遵循 BullMQ 的 `removeUnprocessedChildren`
依赖集级别的脚本形状：它删除仍在的子项
父级的挂起依赖项集，跳过已完成、失败、活动或锁定
子项，删除已删除的子项记录和每个子项元数据，发出
BullMQ 风格的 `removed` 事件针对同一个 Redis 回合中每个被移除的子节点，然后
检查家长是否可以离开`waiting_children`。巷返回删除的
子快照用于可审核性，同时保留父快照`child_ids`，因此
后来的依赖性检查报告将儿童列为失踪。
`remove_child_dependency()` 遵循 BullMQ 的 `removeChildDependency` 路径：
当存在时，从父级的挂起依赖集中删除一个子级，清除
孩子的父母参考，保留孩子的工作本身，并释放
当没有悬而未决的依赖关系时，父级。 Redis 处理挂起的依赖关系
设置、父级 `child_ids` 和父级范围 `:processed`、`:failed` 和
`:unsuccessful` 桶作为关系证据，因此具有
已经离开挂起的集合仍然可以分离而不会留下幽灵
后面的依赖值。内存中队列和本地持久队列暴露相同的
通过允许保留已完成或失败的子级来实现可见的分离语义
要从父级的 `child_ids` 中删除快照而不删除子级
工作。已终端子项的过时依赖项仍会被删除就像 BullMQ 的 `SREM` 路径一样。普通作业拆除、清洁、排水路径
与显式依赖分离保持分离：它们遵循 BullMQ 的
通过释放挂起的父级来实现`removeJob`、`cleanJobsInSet`和`drain`脚本
依赖项并删除已删除作业自己的元数据，同时保留
父级范围的终端依赖结果索引不被视为清理
显式依赖分离之外的目标。本地持久快照持久化
同样的区别，因此在完成或忽略的子作业后重新打开队列是
删除仍然保留父级已处理或忽略的依赖项存储桶，
而`remove_child_dependency()`则故意清理桶。

流扇入在 Redis 转换中也受到保护。 Redis流提交写入
为父级和子级完成、删除和设置挂起的依赖项
清理脚本在检查是否存在之前从该集合中删除子 ID
家长可以被释放到`waiting`，停在`delayed`直到它自己的时间表
是由于孩子达到了极限失败而失败。这如下
BullMQ 的依赖删除机制：删除子项的清理也会更新
父依赖状态而不是依赖于稍后的客户端清理
通过。
动态流扇出也是 Redis 原子的：`add_flow_children()` 检查
父级锁定并拒绝 `dependencies:<parent_id>:unsuccessful` 的父级
在插入新的依赖项之前 zset 是非空的，与 BullMQ 的匹配
`moveToWaitingChildren` 失败的儿童防护罩。它还会回退到保留的孩子
混合升级数据的快照，其中侧面索引丢失但失败
依赖性仍然被记录。当守卫通过时，它会插入新的孩子或
附加现有的自定义 id 子项，跳过普通的重复数据删除子项
候选者，将活跃的保留最后子候选者存储在`deduplication_next:<id>`中，
更新 `dependencies:<parent_id>`，从 `active` 中删除父级，删除
它的锁，将父级写入
`waiting_children`，当所有附加的孩子都被释放后立即释放它
已经在一个Lua脚本中完成了。 Keep-last 占位符保留父级
被阻止，直到所有者最终确定脚本实现最新的子项。
`ignore_dependency_on_failure`、`remove_dependency_on_failure`、
`continue_parent_on_failure`和`fail_parent_on_failure`使用Redis端终端`fail_job()`和停滞的终端故障的故障策略路径。
忽略并删除的失败将失败的子项从
`dependencies:<parent_id>` 并仅在以下情况下释放或延迟父进程：
剩余的依赖集为空。持续失败会删除失败的子项并
立即将父级移动到`waiting`或`delayed`，留下其他待处理的
依赖项可检查。 Fail-parent 失败删除失败的孩子，写入
child id 放入`dependencies:<parent_id>:unsuccessful`，保留剩余的
依赖项可检查，在父级上存储延迟故障，并让
工作线程在处理器执行之前使父进程失败，匹配 BullMQ 的 `fpof` plus
`defa` 路径。重试该子项会删除不成功的条目并恢复
父依赖集。如果失败策略已经释放了父策略，
后来的终端子故障仍然会删除其挂起的依赖关系并更新
父级范围的故障索引，而不是保持未处理状态可见。
不合格的孩子仍被保留以供检查。被忽视和持续的失败
通过忽略的依赖项计数来报告；删除的故障被保留
但从失败和忽略的依赖项计数中省略，而失败父失败
保留在失败的依赖项计数中。

重复后继者也会在 Redis 完成脚本期间创建。的
worker计算`RepeatOptions`的下一个出现，然后是Lua脚本
完成当前作业并将下一个延迟或等待发生的事件写入
同样的Redis转。 Redis 保留轻量级 `repeat:<key>` 所有者密钥
快速冲突检查和由队列级`repeat`组成的调度程序索引
zset 加上 `repeat_meta:<key>` 哈希值。添加脚本检查所有者密钥并
在插入新的重复作业之前回退到调度程序元数据，
完成脚本将所有权和调度程序元数据转移给后继者
在释放已完成的事件和终端故障之前，删除、清理、
耗尽和停滞的终端故障仅在它们仍然存在时才释放两条记录
指向正在完成或删除的作业。那些发布助手还会检查
`repeat_meta:<key>.jid`，因此终端脚本会清除调度程序元数据，即使
快速`repeat:<key>`所有者密钥已经消失。
手动重试回收重试中的重复键和调度程序元数据
脚本并拒绝重试，如果另一个非终端事件已经拥有该
系列。 `list_repeats()` 首先读取调度器zset，加载每个所有者作业
来自作业哈希的快照，仅返回非终端匹配所有者，恢复
来自 `repeat_meta:<key>.jid` 的快速 `repeat:<key>` 所有者密钥
调度程序所有者仍然有效，清除该点的陈旧调度程序/所有者记录
丢失、终止或不匹配的作业，并扫描旧版 `repeat:<key>` 所有者
键作为迁移后备。`remove_repeat()` 解析当前 `repeat:<key>` 所有者，回退到
`repeat_meta:<key>` 当快速所有者密钥丢失时的调度程序所有者 ID，以及
然后运行与`remove_job()`相同的Redis端删除路径，因此它拒绝
活跃的租用所有者，删除作业哈希和状态索引，释放重复
和重复数据删除所有权，并且可以解锁流父级。读者重复使用
相同的调度程序元数据回退：如果快速所有者密钥丢失但是
`repeat_meta:<key>.jid` 仍然指向有效的非终端重复所有者 Redis
返回该所有者并使用 `SET NX` 恢复 `repeat:<key>`。如果车主钥匙
或调度程序元数据指向丢失的作业，Redis 清除过时的所有者密钥，
仅当 zset 条目和元数据散列仍然描述丢失的所有者时。
`upsert_repeat()` 遵循 BullMQ 的
Lane当前的`upsertJobScheduler(..., override: true)`机制
重复所有者层：Redis 脚本解析当前 `repeat:<key>` 所有者，
当快速所有者密钥丢失时，回退到`repeat_meta:<key>.jid`，
当调度程序所有者仍然有效时修复该所有者密钥，拒绝活动
租用业主，拒绝流程拥有的事件，以避免腐败父母
依赖关系，检查作业 ID 和重复数据删除所有者冲突，删除旧的
作业哈希和状态索引中的非活动所有者，清除其锁、日志，
仅当依赖键、重复数据删除所有者和重复所有者仍然指向时
在该作业中，然后写入替换作业、其等待/延迟索引、事件、
重复数据删除密钥、`repeat:<key>` 所有者和调度程序元数据位于同一目录中Redis转。 Lane 在调用 Redis 脚本之前验证重复`end_at`；
如果结束时间戳已经早于添加/更新插入时间戳，则
操作返回配置错误并留下作业哈希、状态索引、
重复所有者和调度程序元数据未受影响。

这是特意设置的脚本级机制，而不仅仅是 API 字段奇偶校验。它是
受到 BullMQ 使用 Lua 脚本来维护重复调度程序记录的启发，
以原子方式删除重复键、锁和状态索引。在 BullMQ 5.79.3 中，
`addJobScheduler-11.lua` 将调度程序元数据存储在重复 zset/hash 中，并且，
覆盖时，删除先前的延迟、优先、等待或暂停
创建新的计划作业之前的下一个作业；活动/已完成/失败
碰撞不会被盲目覆盖。莱恩现在保留了现有的
`repeat:<key>` 用于快速碰撞检查的所有者密钥，还写入
BullMQ 风格的调度程序 zset 在队列的 `repeat` 键加上
`repeat_meta:<key>` 包含当前所有者 ID、名称、下一个的哈希值
时间戳、状态、计数、重复选项和面向计划的字段`key`，
当 Rust 重复选项提供时，`every`、`pattern`、`limit` 和 `endDate`
他们。调度程序在`HSET`之前写入删除并重建元数据哈希，因此
从间隔计划到 cron 计划的覆盖不能保持陈旧
后面的 `every` 或 `endDate` 字段。添加、批量添加、流添加、重复插入、
重复后继队列，索赔时间到期促销，`promote_due_jobs()`，
手动提升、重新安排、主动延迟/释放、重试、删除、清理、排空、
并停止终端清理更新同一 Redis 脚本内的这些记录
这会改变工作状态。非终端移动脚本重建
`repeat_meta:<key>` 已移动作业快照的哈希值，包括面向计划的`opts`、`every`、`pattern`、`limit` 和 `endDate` 等字段；他们还
更新调度程序 zset 分数并恢复丢失的快速 `repeat:<key>` 所有者
当移动的作业仍然拥有该系列时，请使用 `SET NX` 键。如果快速所有者是
缺少但调度程序元数据已经指定了不同的所有者，即运动
脚本保持调度程序记录不变，而不是窃取系列。
`get_repeat()`、`count_repeats()` 和 `list_repeats_page()` 通读
调度程序 zset，验证所有者作业快照，修复丢失的快速所有者密钥
来自调度程序元数据、修剪陈旧元数据和镜像 BullMQ
`getJobScheduler`、`getJobSchedulersCount` 和 `getJobSchedulers(start, end,
asc)` 读取端：条目按下一个计划时间排序，默认为
降序排列。莱恩仍然模型重复工作
Rust 原生的重复系列所有者和后继队列流，而不是完整的
BullMQ JS 模板引擎，因此精确的 BullMQ 调度程序字段对字段奇偶校验
仍然是稍后运行时功能奇偶校验项。

手动生命周期管理遵循相同的Redis端状态移动规则：
`promote_job()` 从延迟 zset 中删除延迟作业并将其插入到
在一个脚本中等待，将延迟的 zset 视为 Redis 移动门，
拒绝其存储状态不再延迟的保留作业，并修剪
孤儿或陈旧的延迟成员同时保留了状态冲突的结果。
`reschedule_job()` 遵循 BullMQ 的 `changeDelay` 机制：
脚本从延迟的 zset 中删除作业，如果该 zset 则拒绝更改
缺少成员资格，更新存储的延迟和计划的时间戳，以及
将作业添加回延迟的 zset，并在同一 Redis 回合中使用新分数。
它还会发出带有新延迟时间戳的 BullMQ 的 `delayed` 事件。
`delay_active_job()` 遵循 BullMQ 的 `moveToDelayed` 租用机制
jobs：脚本验证锁定令牌，将活动 zset 视为移动
门，如果缺少活动索引成员资格，则拒绝移动，清除
锁，更新存储的延迟和计划时间戳，并写入延迟
zset成员在同一个Redis轮中。它发出与以下相同的延迟时间戳字段
BullMQ 的 `moveToDelayed` 脚本。 `release_active_job()` 遵循 BullMQ 的
`moveJobFromActiveToWait`状态移动：脚本验证锁定令牌，
将活动的 zset 视为移动门，清除锁和活动租约
字段，重置`processed_at`，并将作业写回等待的 zset
其在同一 Redis 回合中的优先级分数。与普通的添加和重试不同重新排队，主动释放将作业写入其优先级存储桶的开头，因此
它在具有相同优先级的旧 FIFO 或 LIFO 条目之前声明，匹配
BullMQ 的主动等待脚本。当移动的作业是当前作业时
重复系列所有者，索赔，索赔时间延迟促销，`promote_due_jobs()`，
手动升级、重新安排、主动延迟和主动发布也会重建
同一脚本中的调度程序 hash/zset 并修复丢失的快速所有者密钥
而不是让重复序列分布在过时的 Redis 键上。他们不
覆盖已经指向另一个所有者的调度程序记录。
`retry_job()` 遵循 BullMQ 的 `reprocessJob` 形状，用于保留失败和
已完成的作业：它将匹配的终端 zset 视为 Redis 移动门，
在修剪过时的一侧后拒绝不一致的已完成/失败索引漂移，
清除终端元数据（`failed_reason`对于失败的作业，`return_value`对于
已完成的作业，加上已处理/已完成的时间戳），发出 `waiting`
`prev=failed` 或 `prev=completed`，并将作业移回其中等待
脚本。对于重复数据删除失败的作业，同一脚本会回收所有者密钥并
在将作业返回等待状态之前重新应用重复数据删除 TTL。对于
重复键入失败的作业，重试首先检查快速 `repeat:<key>` 所有者
密钥和调度程序`repeat_meta:<key>.jid`所有者；如果其中一个指向另一个
非终止发生时，Redis 在需要时恢复快速所有者密钥，并且
拒绝重试。只有无争议的失败所有者才能收回重复密钥并调度程序元数据。当重试的作业是保留的流程子级时，重试会恢复
将子级放入父级的挂起依赖集中，清除陈旧的延迟父级
失败元数据，并将非终端父级移回`waiting_children`，
匹配 BullMQ 失败和完成的依赖关系恢复路径
孩子们。
当处理失败由于其配置而达到终端失败状态时
重试次数耗尽，Lane 在 `failed` 之后发出 `retries-exhausted`，
匹配 BullMQ 的 `moveToFinished` 事件顺序。仅手动重试-丢弃路径
如果作业实际上已达到配置的重试限制，则发出该事件。
BullMQ 已弃用的 `job.discard()` 有意建模为当前的
失败路径决策而不是存储的作业元数据：BullMQ 设置内存中
`discarded` 标志，`shouldRetryJob()` 在 `moveToFailed()` 之前检查该标志，
然后 Redis 转换使用终端失败路径而不是延迟路径
或立即重试。莱恩将该机制公开为
`fail_job_discarding_retry()` 和 `JobContext::discard_retry()`。巷也
镜像 BullMQ 的首选 `UnrecoverableError` 路径
`LaneError::unrecoverable_job()`：当处理器返回该错误时，工作线程
使用与 `discard_retry()` 相同的重试绕过最终路径。雷迪斯
后端重用与`fail_job()`相同的活动到失败的Lua脚本，但通过
重试标志被禁用，因此脚本写入失败的 zset，释放
重复数据删除/重复所有权，并自动更新流父级。
`update_priority()`
重写作业哈希，对于等待作业，替换等待 zset 分数相同的脚本；对于不再等待的作业，它会修剪陈旧的等待
成员，同时保留存储的状态。保留的终端作业可以更新
它们存储的优先级无需重新排队，与 BullMQ 匹配
`changePriority-7.lua` 只存在的守卫。对于等待作业，脚本还
刷新 `enqueued_seq` 并重新计算 FIFO/LIFO 分数。
`update_priority_with_lifo()` 暴露 BullMQ
`changePriority({ priority, lifo })` 直接形状：可选的 LIFO 标志是
在重新计算等待分数之前存储在 `job.options.lifo` 上，因此
Redis 索引随序列化作业快照一起更改。这是
有意与 BullMQ 通过 Redis 移动作业状态的机制保持一致
脚本而不是协调多个客户端 Redis 命令。巷也
在进入该脚本之前应用 BullMQ 的 `2^21` 优先级上限，因此
无效更新无法部分重写作业哈希或等待索引。

Redis 作业管理突变也是由脚本支持的。 `update_data()` 关注
BullMQ的`updateData`存在检查和写入形状，适配Lane的Redis
哈希布局，通过解码存储的作业 JSON，替换 `payload`，并写入
Lua 回合中返回作业快照。 `update_progress()` 镜像 BullMQ
`updateProgress-3.lua` 仅存守卫：任何保留的工作，包括
终端作业，可以接收新的进度值，并且脚本写入该值
加上一个 Redis 回合中的 `XADD event=progress` 条目。 `save_stacktrace()`
镜像 BullMQ 的 `saveStacktrace` 存储行为：
Lua脚本验证保留的作业是否存在，解码Lane存储的作业
JSON，将 stacktrace 数组和失败原因一起替换，并写入
在一个 Redis 回合中更新快照。 `add_log()` 在关键级别遵循 BullMQ 的 `addLog` 形状：脚本
验证作业是否存在，`RPUSH`es 结构化 JSON 条目
`logs:<jobId>`，在提供保留计数时应用 `LTRIM`，并且镜像
作业 JSON 快照中保留的条目以实现车道兼容性，无需
发出队列事件。 `clean_jobs()` 通过解析的过滤保留记录
毫秒参考时间，`clean_jobs(JobState::Active, ...)` 现在镜像
BullMQ 的 `clean(..., "active")` 通过仅清理其工人的活动作业来进行保护
锁已经消失了。锁定的活动作业仍必须完成、失败、释放，
或经历停滞的复苏。 Redis clean 删除锁键、哈希条目、
状态索引、停滞候选条目、依赖集和日志列表以原子方式更新已删除的子作业的流程父级，并返回已删除的
快照。对于非终端重复所有者，干净的脚本反映了 BullMQ 的
调度程序作业防护：它检查两者
`repeat:<key>` 和 `repeat_meta:<key>.jid`，从 恢复快速所有者密钥
有效的调度程序元数据，并跳过当前系列所有者而不是删除
通过广泛的清理。
`RedisJobQueue::remove_orphaned_jobs(count, limit)` 是仅 Redis 维护
相当于 Lane 存储布局的 BullMQ 的 `removeOrphanedJobs()` 助手：
它使用 `HSCAN` 扫描中央作业哈希，检查等待、延迟、活动、
Lua 中的等待子级、已完成、失败和停滞的 Redis 索引，并且仅
当这些键都没有引用作业 ID 时，删除作业哈希字段。已删除
孤儿也会失去保留的 `logs:<jobId>` 列表、`dependencies:<jobId>` 集，
和 `locks:<jobId>` key 在同一个 Redis 回合中。通过 `count = 0` 使用
默认扫描计数为 1000，并且 `limit = 0` 删除发现的所有孤儿
扫描。

队列排出遵循相同的规则。 `drain_jobs(false)` 删除等待作业
并且 `drain_jobs(true)` 还可以在一个 Redis 回合中删除普通的延迟作业，
同时删除每个已删除作业的保留日志列表并保持活动状态，
已完成、失败和正在等待的儿童作业已就位。就像 BullMQ 的 `drain`
脚本，Lane保护当前延迟重复
发生：BullMQ 从作业调度程序记录中派生该集合，而 Lane
检查`repeat:<key>`所有者密钥，回退到`repeat_meta:<key>.jid`，并且
当调度程序元数据仍然命名延迟时恢复快速所有者密钥
业主。删除的子项会在同一脚本中更新其父项依赖项集，
因此家长可以从 `waiting_children` 移动到 `waiting`、`delayed` 或 `failed`
没有后续客户通行证。

队列删除遵循 BullMQ 的底层暂停优先机制，而不是
而不是仅匹配公共方法名称。 BullMQ 的公共 `obliterate()` 调用
`pause()` 在调用其 Lua 命令之前；该命令检查`meta.paused`，
拒绝活动作业，除非设置了`force`，然后删除队列的状态，
作业、锁定、重复、指标和元数据键。 Lane 将相同的生命周期折叠成
一个 Redis 脚本：它写入 `meta.paused`，检查活动排序集索引，
当存在活动作业且 `force` 为 false 时，返回作业状态冲突，计数
当前作业哈希，并批量扫描队列前缀，直到每个匹配
key被删除，包括作业哈希，生命周期索引，锁，保留日志，
重复数据删除所有者、保留最后活动影子作业、重复所有者、依赖性
集、速率限制计数器、序列键和暂停元数据本身。失败了
非强制擦除故意将 `meta.paused` 留在原地，因此没有工人
可以要求额外的工作，直到队列恢复或强制删除。一个
成功的强制删除也会删除暂停标记，留下一个空的，
未暂停的队列，可以通过干净的重复数据删除和重复接受新作业
所有权。

队列读取使用相同的 Redis 端快照方法。 `get_job_state()` 关注
BullMQ的`getState`机制通过在一个脚本中检查Redis状态索引，
而不是信任序列化作业 JSON 状态字段。车道检查完成，
失败、延迟、活动、等待和等待子级排序集和返回
`None` 当作业 ID 不存在于任何状态索引中时。
`get_job_finished_result()` 遵循 BullMQ 的 `isFinished(..., returnValue=true)`
shape：Redis 检查已完成和失败的索引以及保留的作业哈希
在一个 Lua 脚本中，即使保留了一个索引，也会将这些索引视为权威索引
快照仍然带有较旧的状态，并返回`NotFinished`，一个已完成的状态
`return_value`、失败的 `failed_reason` 或 `None`（缺少保留记录）。
`RedisJobQueue::get_metrics(JobState::Completed | JobState::Failed, start, end)`
遵循 BullMQ 的 `getMetrics` 存储形状。完成、失败和停滞
终端脚本递增 `metrics:<state>` 并关闭一分钟窗口
`metrics:<state>:data` 与 `LPUSH`/`LTRIM`；读取使用相同的`HMGET计数，
prevTS，prevCount`, `LRANGE`, and `LLEN`脚本形状。车道记录终端
默认情况下，指标带有 `DEFAULT_JOB_METRICS_RETENTION` 保留的数据点。
`get_job_counts()` 遵循 BullMQ 的 `getCounts` 脚本形状：空状态输入
默认为所有生命周期状态，在第一个状态之后重复的状态将被忽略
发生次数，Redis 统计一个 Lua 脚本中请求的状态索引。巷
将每个生命周期状态存储为排序集，因此脚本使用 `ZCARD`
等待、延迟、活动、等待子级、完成和失败而不是
客户端加载作业快照。`get_job_count()` 镜像 BullMQ 的 `getJobCountByTypes()` getter 层
将每个状态的计数相加，因此它继承了相同的默认所有和
重复状态语义。 `count_pending_jobs()` 镜像 BullMQ 的 `count()`
含义：等待、延迟和等待子作业被视为待处理
工作，而活动的、已完成的和失败的作业被排除在外。
`get_counts_per_priority()` 遵循 BullMQ 的 `getCountsPerPriority` 形状
优先级队列：重复请求的优先级在第一个优先级之后将被忽略
发生，并且 Redis 将使用 `ZCOUNT` 计算优先级编码的等待作业
等待 zset 分数范围而不是在客户端加载作业快照。
`get_job_logs()` 读取带有 `LRANGE` 和 `LLEN` 的 `logs:<jobId>` 列表，
包括 BullMQ 使用负索引的降序窗口约定以及
扭转结果。丢失或已删除的日志列表将返回空页。
`clear_job_logs()` 遵循 BullMQ 的 `Job.clearLogs()` 存储行为：积极
保留使用`LTRIM logs:<jobId> -keep -1`，零保留删除日志
列表。 Lane 还修剪了作业快照中嵌入的 `logs` 数组
Redis Lua 开启，因此保留的作业记录和 Redis 日志列表不会发生漂移。
`list_jobs()`在Redis上遵循BullMQ的`getRanges`/`getJobs`机制
索引层：调用者可以请求一个或多个生命周期状态并选择
升序或降序范围顺序。 Lane 使该机制适应其排序
通过收集选定的状态成员、修剪陈旧索引来建立状态索引
保留作业状态不再匹配的条目，按 Lane 排序快照稳定状态/优先级/时间/id顺序，并在一个Lua中返回请求的页面
转。
`stats()` 评估一个读取暂停标志和所有等待的 Lua 脚本，
延迟、活动、等待子项、已完成和失败的排序集计数
单转Redis，镜像BullMQ的`getCounts`风格而不是拼接
一起进行几个客户端读取。 Redis 暂停状态遵循 BullMQ 的
`meta.paused`机制：`pause()`写入字段，`resume()`删除字段，以及
`is_paused()` 读取相同的字段。旧的 `paused = 0` 值被视为
恢复并清理干净。

停滞的恢复也是 Lua 支持的。恢复脚本遵循 BullMQ 的
`moveStalledJobsToWait` 形状：它消耗之前的 `stalled` 候选者
设置，验证每个候选者的独立锁定密钥是否丢失，递增
停滞的计数，并且要么重新排队作业，要么在同一个 Redis 中失败
转。在脚本的末尾，它标记了当前活跃的索引成员
`stalled` 设置为下一个恢复通道。成功`renew_lease()`，
`complete_job()`、`fail_job()`、`delay_active_job()`，以及
`release_active_job()` 脚本从候选集中删除作业，镜像
BullMQ 的 `extendLock` 和 `removeLock` 助手。如果一个活跃的排序集成员
指向已经转移到不同状态的作业，稍后恢复
传递陈旧的活动索引的修剪，而不是将其视为可恢复的工作。
当候选对象实际恢复时，Redis 会写入一个 `stalled` 事件，其中包含
失败原因，然后写入结果`waiting prev=active`或
`failed prev=active` 在同一个 Lua 回合中转换，匹配内存中和
本地事件合约，同时保留 BullMQ 的显式 `stalled` 通知。
BullMQ 5.79.3 还提供了特殊情况下的可重复调度程序作业
`moveStalledJobsToWait-9.lua`：如果调度程序记录仍然存在，则停止
即使超出普通停顿限制后，事件也会重新排队。巷
镜像活跃重复所有者的分支：非重复作业在之后仍然失败
`max_stalled_count`，但是一个停滞的重复所有者，其所有者密钥或调度程序
元数据仍然指向作业移回`waiting`并保留其重复所有权。如果快速 `repeat:<key>` 所有者密钥丢失，但
`repeat_meta:<key>.jid` 仍将停滞的事件命名为恢复脚本
在重新排队之前恢复快速所有者密钥。

`remove_job()` 使用 Redis 脚本仅在其工作线程时拒绝活动作业
锁定密钥仍然存在，与 BullMQ 的 `removeJob` `isLocked` 防护相匹配。一个活跃的
锁已经消失的作业可以作为过时的工作被删除；脚本
删除作业哈希、锁定密钥、所有状态索引、停滞的候选条目，
保留的日志列表，以及在一个 Redis 回合中设置的任何子依赖项。一个删除
请求丢失的作业仍然会修剪孤立的索引、锁、依赖集，
以及该 ID 的日志列表。如果删除的作业是流程子项，则相同的脚本
更新父级的依赖集并自动将父级从
`waiting_children` 至 `waiting`、`delayed` 或 `failed`（视情况而定）。

针对任何可访问的 Redis 服务器运行 Redis 集成测试：

```bash
A3S_LANE_REDIS_URL=redis://127.0.0.1:6379/ \
  cargo test --features redis-backend --test redis_job_queue
```

集成工具在执行之前执行短暂的 TCP 可达性预检
测试机构运行。报告并跳过丢失或无法访问的 Redis 端点
快速，而不是让每个异步测试等待更长的每次测试超时。
命名空间清理还具有有限的 Redis 命令超时，因此测试过时
连接明显失败，而不是隐藏套件背后的实际失败
外部超时。

使用 `JobWorker` 针对任何后端运行异步处理器：

```rust
use a3s_lane::{
    job_processor_fn, InMemoryJobQueue, JobOptions, JobProcessor, JobProcessorRouter,
    JobQueueBackend, JobWorker, JobWorkerConfig,
};
use std::{sync::Arc, time::Duration};

# async fn worker_example() -> a3s_lane::Result<()> {
let backend: Arc<dyn JobQueueBackend> = Arc::new(InMemoryJobQueue::new("email"));
backend
    .add_job(
        "send".to_string(),
        serde_json::json!({ "to": "ops@example.com" }),
        JobOptions::new().with_timeout(Duration::from_secs(30)),
    )
    .await?;

let send_processor: Arc<dyn JobProcessor> = Arc::new(job_processor_fn(|job, context| async move {
    context.ensure_lease()?;
    context.update_data(serde_json::json!({ "to": job.payload["to"], "normalized": true })).await?;
    context.update_progress(serde_json::json!({ "phase": "sending" })).await?;
    context.add_log("provider accepted message").await?;
    Ok(serde_json::json!({ "sent": job.payload["to"] }))
}));
let processor = Arc::new(JobProcessorRouter::new().with_processor("send", send_processor));

let worker = JobWorker::new(
    backend,
    processor,
    JobWorkerConfig::new("worker-1")
        .with_concurrency(4)
        .with_blocking_claim_timeout(Duration::from_secs(5)),
);

worker.run_until_idle(100).await?;
# Ok(())
# }
```

后台`JobWorker`循环调用`JobQueueBackend::claim_next_blocking()`。
Redis 后端使用队列标记 zset 休眠，直到准备好或延迟工作
唤醒他们，而内存中和本地持久后端保持立即声明
后备。 `run_once()` 对于确定性手动工作保持非阻塞；使用
`run_once_blocking()` 当单个工作迭代应该等待新工作时。
从`start()`开始的工作人员在并发中共享一个租约续订循环
处理器并批量调用`renew_leases()`；直接`run_once()`通话保持
每个作业的更新路径，因此确定性手动运行不需要背景
工人手柄。

`JobContext::has_lost_lease()`和`JobContext::ensure_lease()`让长时间运行
在工作人员观察到某个情况后，处理器会在执行更多外部工作之前停止
续租失败。上下文进度和日志助手也拒绝写入一次
设置了租赁损失标志。 `JobContext::discard_retry()`让处理器标记
当前失败的最终确定为终端，即使作业的重试策略仍然存在
尚有剩余尝试；标记仅存在于工人上下文中，而不是
存储在作业中。从 a 返回 `LaneError::unrecoverable_job(message)`
处理器是首选的类型错误等效项，用于处理永远不应该发生的故障
会自动重试。

## 基准测试

Apple Silicon（M 系列），发布版本，带预热管理器的稳态吞吐量：

|工作量|吞吐量|
|----------|------------|
| 100 个命令，10 个通道 | ~33,000–50,000 次操作/秒 |
| 100 个命令，1 个通道 | ~6,600–10,000 次操作/秒 |
|指标开销| ~3–5% |

完整生命周期基准（包括管理器创建/启动/关闭）以约 85-93 操作/秒的速度运行 — 主要由启动成本而非调度决定。

```bash
cargo bench
open target/criterion/report/index.html
```

## 社区

加入我们的 [Discord](https://discord.gg/XVg6Hu6H)，了解问题、讨论和更新。

## 许可证

麻省理工学院