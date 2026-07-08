use super::backend::JobQueueBackend;
use super::types::{
    add_duration, deduplication_expiration, page_repeat_entries, validate_job_priority, Job,
    JobEvent, JobFinishedResult, JobFlow, JobFlowChildValues, JobFlowDependencies,
    JobFlowDependencyCountOptions, JobFlowDependencyCounts, JobFlowDependencyKind,
    JobFlowDependencyPage, JobFlowDependencyPageItem, JobFlowDependencyPageOptions,
    JobFlowDependencyPages, JobFlowDependencyPagesOptions, JobFlowDependencySelectedCounts,
    JobFlowDependencyValues, JobFlowIgnoredFailures, JobId, JobLeaseRenewal, JobListOptions,
    JobListPage, JobLogEntry, JobLogPage, JobMetrics, JobMetricsMeta, JobOptions, JobPriority,
    JobPriorityCount, JobQueueStats, JobRateLimit, JobRepeatEntry, JobRepeatListOptions,
    JobRepeatPage, JobSpec, JobState, JobStateCount, JobWorkerId, QueueName,
    DEFAULT_JOB_EVENT_RETENTION, DEFAULT_JOB_METRICS_RETENTION,
};
use crate::error::{LaneError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use redis::aio::ConnectionManager;
use redis::streams::StreamRangeReply;
use redis::AsyncCommands;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};
use uuid::Uuid;

const WAITING_SCORE_BUCKET: f64 = 1_000_000_000_000.0;
const MAX_MARKER_BLOCK_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_MARKER_BLOCK_TIMEOUT: Duration = Duration::from_millis(1);

const ADD_JOB_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function active_deduplicated_raw(jobs_key, deduplication_prefix, deduplication_id)
  if not deduplication_id or deduplication_id == '' then
    return nil
  end

  local deduplication_key = deduplication_prefix .. deduplication_id
  local existing_id = redis.call('GET', deduplication_key)
  if not existing_id then
    return nil
  end

  local existing_raw = redis.call('HGET', jobs_key, existing_id)
  if not existing_raw then
    redis.call('DEL', deduplication_key)
    return nil
  end

  local existing_job = cjson.decode(existing_raw)
  if existing_job["state"] == "completed" or existing_job["state"] == "failed" then
    local pttl = redis.call('PTTL', deduplication_key)
    if pttl > 0 then
      return existing_raw
    end
    redis.call('DEL', deduplication_key)
    return nil
  end

  return existing_raw
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function deduplication_ttl_millis(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  if deduplication["keep_last_if_active"] == true then
    return nil
  end
  return duration_millis(deduplication["ttl"])
end

local function set_deduplication_key(job, job_id, deduplication_prefix, keep_existing_ttl)
  local ttl = deduplication_ttl_millis(job)
  local deduplication = job["options"]["deduplication"]
  local id = deduplication["id"]
  if keep_existing_ttl and ttl and deduplication["extend"] ~= true then
    redis.call('SET', deduplication_prefix .. id, job_id, 'KEEPTTL')
  elseif ttl then
    redis.call('SET', deduplication_prefix .. id, job_id, 'PX', ttl)
  else
    redis.call('SET', deduplication_prefix .. id, job_id)
  end
end

local function can_replace_deduplicated_owner(candidate_job, existing_job)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return false
  end
  local deduplication = candidate_job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null or deduplication["replace"] ~= true then
    return false
  end
  if existing_job["state"] ~= "delayed" then
    return false
  end
  if candidate_job["repeat_key"] and candidate_job["repeat_key"] ~= cjson.null then
    return false
  end
  if existing_job["parent_id"] and existing_job["parent_id"] ~= cjson.null then
    return false
  end
  if existing_job["repeat_key"] and existing_job["repeat_key"] ~= cjson.null then
    return false
  end
  if existing_job["child_ids"] and existing_job["child_ids"] ~= cjson.null and #existing_job["child_ids"] > 0 then
    return false
  end
  return true
end

local function can_store_deduplicated_next(candidate_job, existing_job, active_key)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return false
  end
  local deduplication = candidate_job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null or deduplication["keep_last_if_active"] ~= true then
    return false
  end
  if existing_job["state"] ~= "active" then
    return false
  end
  if not redis.call('ZSCORE', active_key, existing_job["id"]) then
    return false
  end
  if candidate_job["parent_id"] and candidate_job["parent_id"] ~= cjson.null then
    return false
  end
  local candidate_repeat_key = candidate_job["repeat_key"]
  local existing_repeat_key = existing_job["repeat_key"]
  local candidate_has_repeat = candidate_repeat_key ~= nil and candidate_repeat_key ~= cjson.null
  local existing_has_repeat = existing_repeat_key ~= nil and existing_repeat_key ~= cjson.null
  if candidate_has_repeat ~= existing_has_repeat then
    return false
  end
  if candidate_has_repeat and candidate_repeat_key ~= existing_repeat_key then
    return false
  end
  if candidate_job["child_ids"] and candidate_job["child_ids"] ~= cjson.null and #candidate_job["child_ids"] > 0 then
    return false
  end
  return true
end

local function store_deduplicated_next(candidate_raw, candidate_job, existing_job, deduplication_prefix, deduplication_next_prefix)
  local deduplication_id = candidate_job["options"]["deduplication"]["id"]
  redis.call('SET', deduplication_next_prefix .. deduplication_id, candidate_raw)
  redis.call('PERSIST', deduplication_prefix .. deduplication_id)
end

local function extend_deduplicated_owner(candidate_job, existing_job, deduplication_prefix)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return
  end
  local deduplication = candidate_job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return
  end
  if deduplication["replace"] == true or deduplication["extend"] ~= true or deduplication["keep_last_if_active"] == true then
    return
  end
  local ttl = duration_millis(deduplication["ttl"])
  if ttl then
    redis.call('SET', deduplication_prefix .. deduplication["id"], existing_job["id"], 'PX', ttl)
  end
end

local function deduplication_replace(candidate_job)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return false
  end
  local deduplication = candidate_job["options"]["deduplication"]
  return deduplication and deduplication ~= cjson.null and deduplication["replace"] == true
end

local function emit_deduplication_events(events_key, max_events, event_job_id, deduplication_id, deduplicated_job_id)
  redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'debounced', 'jobId', event_job_id, 'debounceId', deduplication_id)
  redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'deduplicated', 'jobId', event_job_id, 'deduplicationId', deduplication_id, 'deduplicatedJobId', deduplicated_job_id)
end

local function emit_deduplicated_events(events_key, max_events, owner_job, candidate_job)
  local deduplication = candidate_job["options"]["deduplication"]
  local deduplication_id = deduplication["id"]
  emit_deduplication_events(events_key, max_events, owner_job["id"], deduplication_id, candidate_job["id"])
end

local function emit_replaced_deduplicated_owner_events(events_key, max_events, candidate_job, existing_job)
  local deduplication = candidate_job["options"]["deduplication"]
  local deduplication_id = deduplication["id"]
  redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'removed', 'jobId', existing_job["id"], 'prev', 'delayed')
  emit_deduplication_events(events_key, max_events, candidate_job["id"], deduplication_id, existing_job["id"])
end

local function active_repeat_raw(jobs_key, repeat_prefix, repeat_key)
  if not repeat_key or repeat_key == '' then
    return nil
  end

  local owner_key = repeat_prefix .. repeat_key
  local existing_id = redis.call('GET', owner_key)
  if existing_id then
    local existing_raw = redis.call('HGET', jobs_key, existing_id)
    if existing_raw then
      local existing_job = cjson.decode(existing_raw)
      if existing_job["state"] ~= "completed"
        and existing_job["state"] ~= "failed"
        and existing_job["repeat_key"] == repeat_key then
        return existing_raw
      end
    end
    redis.call('DEL', owner_key)
  end

  local scheduler_key = string.sub(repeat_prefix, 1, -2)
  local scheduler_meta_key = string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key
  local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
  if not scheduler_owner_id then
    return nil
  end

  local scheduler_owner_raw = redis.call('HGET', jobs_key, scheduler_owner_id)
  if not scheduler_owner_raw then
    redis.call('ZREM', scheduler_key, repeat_key)
    redis.call('DEL', scheduler_meta_key)
    return nil
  end

  local scheduler_owner_job = cjson.decode(scheduler_owner_raw)
  if scheduler_owner_job["state"] == "completed"
    or scheduler_owner_job["state"] == "failed"
    or scheduler_owner_job["repeat_key"] ~= repeat_key then
    redis.call('ZREM', scheduler_key, repeat_key)
    redis.call('DEL', scheduler_meta_key)
    return nil
  end

  redis.call('SET', owner_key, scheduler_owner_id, 'NX')
  return scheduler_owner_raw
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key
end

local function store_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local existing = redis.call('HGET', KEYS[1], ARGV[1])
if existing then
  return {'existing', existing}
end

local deduplicated = active_deduplicated_raw(KEYS[1], ARGV[8], ARGV[7])
local replaced_deduplicated_owner = false
if deduplicated then
  local candidate_job = cjson.decode(ARGV[2])
  local existing_job = cjson.decode(deduplicated)
  if can_replace_deduplicated_owner(candidate_job, existing_job) then
    local removed = redis.call('ZREM', KEYS[3], existing_job["id"])
    if removed > 0 then
      redis.call('HDEL', KEYS[1], existing_job["id"])
      redis.call('DEL', ARGV[12] .. existing_job["id"])
      emit_replaced_deduplicated_owner_events(KEYS[7], ARGV[13], candidate_job, existing_job)
      replaced_deduplicated_owner = true
    else
      return {'deduplicated', deduplicated}
    end
  elseif can_store_deduplicated_next(candidate_job, existing_job, KEYS[6]) then
    store_deduplicated_next(ARGV[2], candidate_job, existing_job, ARGV[8], ARGV[11])
    emit_deduplicated_events(KEYS[7], ARGV[13], existing_job, candidate_job)
    return {'deduplicated', deduplicated}
  else
    extend_deduplicated_owner(candidate_job, existing_job, ARGV[8])
    if not deduplication_replace(candidate_job) then
      emit_deduplicated_events(KEYS[7], ARGV[13], existing_job, candidate_job)
    end
    return {'deduplicated', deduplicated}
  end
end

local repeat_owner = active_repeat_raw(KEYS[1], ARGV[10], ARGV[9])
if repeat_owner then
  return {'repeat', repeat_owner}
end

local inserted = redis.call('HSETNX', KEYS[1], ARGV[1], ARGV[2])
if inserted == 0 then
  local current = redis.call('HGET', KEYS[1], ARGV[1])
  if current then
    return {'existing', current}
  end
  return {'missing'}
end

local state = ARGV[3]
local inserted_raw = ARGV[2]
if state == 'waiting' then
  local job = cjson.decode(ARGV[2])
  inserted_raw = enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[5], job, ARGV[1], tonumber(ARGV[5]), ARGV[6], KEYS[#KEYS])
elseif state == 'delayed' then
  redis.call('ZADD', KEYS[3], ARGV[4], ARGV[1])
  refresh_delay_marker(KEYS[#KEYS], KEYS[3])
elseif state == 'waiting_children' then
  redis.call('ZADD', KEYS[4], ARGV[4], ARGV[1])
end

if ARGV[7] ~= '' then
  set_deduplication_key(cjson.decode(ARGV[2]), ARGV[1], ARGV[8], replaced_deduplicated_owner)
end
if ARGV[9] ~= '' then
  redis.call('SET', ARGV[10] .. ARGV[9], ARGV[1])
  store_repeat_scheduler(cjson.decode(inserted_raw), ARGV[1], ARGV[10], ARGV[4])
end

local event_job = cjson.decode(inserted_raw)
redis.call('XADD', KEYS[7], 'MAXLEN', '~', ARGV[13], '*', 'event', 'added', 'jobId', ARGV[1], 'name', event_job["name"] or '')
local event_state = state
if event_state == 'waiting_children' then
  event_state = 'waiting-children'
end
redis.call('XADD', KEYS[7], 'MAXLEN', '~', ARGV[13], '*', 'event', event_state, 'jobId', ARGV[1])

return {'inserted', inserted_raw}
"#;

const UPSERT_REPEAT_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function deduplication_id(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  local id = deduplication["id"]
  if not id or id == cjson.null or id == '' then
    return nil
  end
  return id
end

local function deduplication_ttl_millis(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  if deduplication["keep_last_if_active"] == true then
    return nil
  end
  return duration_millis(deduplication["ttl"])
end

local function set_deduplication_key(job, job_id, deduplication_prefix)
  local deduplication = job["options"]["deduplication"]
  local id = deduplication["id"]
  local ttl = deduplication_ttl_millis(job)
  if ttl then
    redis.call('SET', deduplication_prefix .. id, job_id, 'PX', ttl)
  else
    redis.call('SET', deduplication_prefix .. id, job_id)
  end
end

local function release_deduplication_key(job, job_id, deduplication_prefix, deduplication_next_prefix)
  local id = deduplication_id(job)
  if id then
    local key = deduplication_prefix .. id
    if redis.call('GET', key) == job_id then
      redis.call('DEL', key)
      if deduplication_next_prefix and deduplication_next_prefix ~= '' then
        redis.call('DEL', deduplication_next_prefix .. id)
      end
    end
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function store_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = repeat_key(job)
  if not key then
    return
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local function release_repeat_key(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if key then
    local owner_key = repeat_prefix .. key
    local owner_matches = redis.call('GET', owner_key) == job_id
    if owner_matches then
      redis.call('DEL', owner_key)
    end

    local scheduler_meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
    local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
    if scheduler_owner_id == job_id or (owner_matches and not scheduler_owner_id) then
      redis.call('ZREM', repeat_scheduler_key(repeat_prefix), key)
      redis.call('DEL', scheduler_meta_key)
    end
  end
end

local function active_deduplicated_owner_id(jobs_key, deduplication_prefix, deduplication_id_value, ignored_job_id)
  if not deduplication_id_value or deduplication_id_value == '' then
    return nil
  end

  local deduplication_key = deduplication_prefix .. deduplication_id_value
  local existing_id = redis.call('GET', deduplication_key)
  if not existing_id then
    return nil
  end
  if ignored_job_id and ignored_job_id ~= '' and existing_id == ignored_job_id then
    return nil
  end

  local existing_raw = redis.call('HGET', jobs_key, existing_id)
  if not existing_raw then
    redis.call('DEL', deduplication_key)
    return nil
  end

  local existing_job = cjson.decode(existing_raw)
  if existing_job["state"] == "completed" or existing_job["state"] == "failed" then
    local pttl = redis.call('PTTL', deduplication_key)
    if pttl > 0 then
      return existing_id
    end
    redis.call('DEL', deduplication_key)
    return nil
  end

  return existing_id
end

local function remove_state_indexes(job_id)
  redis.call('ZREM', KEYS[2], job_id)
  redis.call('ZREM', KEYS[3], job_id)
  redis.call('ZREM', KEYS[4], job_id)
  redis.call('ZREM', KEYS[6], job_id)
  redis.call('ZREM', KEYS[7], job_id)
  redis.call('ZREM', KEYS[8], job_id)
end

local function remove_non_active_job(job, job_id)
  release_deduplication_key(job, job_id, ARGV[8], ARGV[11])
  release_repeat_key(job, job_id, ARGV[10])
  remove_state_indexes(job_id)
  redis.call('HDEL', KEYS[1], job_id)
  redis.call('DEL', ARGV[12] .. job_id)
  redis.call('DEL', ARGV[13] .. job_id)
  redis.call('DEL', ARGV[14] .. job_id)
end

if ARGV[9] == '' then
  return {'config', 'repeat key must not be empty'}
end

local owner_key = ARGV[10] .. ARGV[9]
local scheduler_meta_key = repeat_scheduler_meta_key(ARGV[10], ARGV[9])
local current_owner_id = redis.call('GET', owner_key)
local current_owner_job = nil
if current_owner_id then
  local current_owner_raw = redis.call('HGET', KEYS[1], current_owner_id)
  if not current_owner_raw then
    redis.call('DEL', owner_key)
    current_owner_id = nil
  else
    current_owner_job = cjson.decode(current_owner_raw)
    if current_owner_job["state"] == "completed" or current_owner_job["state"] == "failed" then
      redis.call('DEL', owner_key)
      current_owner_id = nil
      current_owner_job = nil
    elseif current_owner_job["state"] == "active" then
      return {'active', current_owner_id}
    elseif (current_owner_job["parent_id"] and current_owner_job["parent_id"] ~= cjson.null) or
      (current_owner_job["child_ids"] and current_owner_job["child_ids"] ~= cjson.null and #current_owner_job["child_ids"] > 0) then
      return {'flow', current_owner_id}
    end
  end
end

if not current_owner_id then
  local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
  if scheduler_owner_id then
    local scheduler_owner_raw = redis.call('HGET', KEYS[1], scheduler_owner_id)
    if not scheduler_owner_raw then
      redis.call('ZREM', repeat_scheduler_key(ARGV[10]), ARGV[9])
      redis.call('DEL', scheduler_meta_key)
    else
      local scheduler_owner_job = cjson.decode(scheduler_owner_raw)
      if scheduler_owner_job["state"] == "completed"
        or scheduler_owner_job["state"] == "failed"
        or repeat_key(scheduler_owner_job) ~= ARGV[9] then
        redis.call('ZREM', repeat_scheduler_key(ARGV[10]), ARGV[9])
        redis.call('DEL', scheduler_meta_key)
      elseif scheduler_owner_job["state"] == "active" then
        redis.call('SET', owner_key, scheduler_owner_id, 'NX')
        return {'active', scheduler_owner_id}
      elseif (scheduler_owner_job["parent_id"] and scheduler_owner_job["parent_id"] ~= cjson.null) or
        (scheduler_owner_job["child_ids"] and scheduler_owner_job["child_ids"] ~= cjson.null and #scheduler_owner_job["child_ids"] > 0) then
        redis.call('SET', owner_key, scheduler_owner_id, 'NX')
        return {'flow', scheduler_owner_id}
      else
        current_owner_id = scheduler_owner_id
        current_owner_job = scheduler_owner_job
        redis.call('SET', owner_key, scheduler_owner_id, 'NX')
      end
    end
  end
end

local existing_raw = redis.call('HGET', KEYS[1], ARGV[1])
if existing_raw and current_owner_id ~= ARGV[1] then
  return {'exists', existing_raw}
end

local deduplicated_owner_id = active_deduplicated_owner_id(KEYS[1], ARGV[8], ARGV[7], current_owner_id or '')
if deduplicated_owner_id then
  return {'deduplicated', deduplicated_owner_id}
end

if current_owner_id and current_owner_job then
  remove_non_active_job(current_owner_job, current_owner_id)
end

local inserted = redis.call('HSETNX', KEYS[1], ARGV[1], ARGV[2])
if inserted == 0 then
  local current = redis.call('HGET', KEYS[1], ARGV[1])
  if current then
    return {'exists', current}
  end
  return {'missing'}
end

local state = ARGV[3]
local inserted_raw = ARGV[2]
if state == 'waiting' then
  local job = cjson.decode(ARGV[2])
  inserted_raw = enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[5], job, ARGV[1], tonumber(ARGV[5]), ARGV[6], KEYS[#KEYS])
elseif state == 'delayed' then
  redis.call('ZADD', KEYS[3], ARGV[4], ARGV[1])
  refresh_delay_marker(KEYS[#KEYS], KEYS[3])
elseif state == 'waiting_children' then
  redis.call('ZADD', KEYS[4], ARGV[4], ARGV[1])
end

if ARGV[7] ~= '' then
  set_deduplication_key(cjson.decode(ARGV[2]), ARGV[1], ARGV[8])
end
redis.call('SET', owner_key, ARGV[1])
store_repeat_scheduler(cjson.decode(inserted_raw), ARGV[1], ARGV[10], ARGV[4])

local event_job = cjson.decode(inserted_raw)
redis.call('XADD', KEYS[9], 'MAXLEN', '~', ARGV[15], '*', 'event', 'added', 'jobId', ARGV[1], 'name', event_job["name"] or '')
local event_state = state
if event_state == 'waiting_children' then
  event_state = 'waiting-children'
end
redis.call('XADD', KEYS[9], 'MAXLEN', '~', ARGV[15], '*', 'event', event_state, 'jobId', ARGV[1])

return {'inserted', inserted_raw}
"#;

const ADD_JOBS_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function active_deduplicated_raw(jobs_key, deduplication_prefix, deduplication_id)
  if not deduplication_id or deduplication_id == '' then
    return nil
  end

  local deduplication_key = deduplication_prefix .. deduplication_id
  local existing_id = redis.call('GET', deduplication_key)
  if not existing_id then
    return nil
  end

  local existing_raw = redis.call('HGET', jobs_key, existing_id)
  if not existing_raw then
    redis.call('DEL', deduplication_key)
    return nil
  end

  local existing_job = cjson.decode(existing_raw)
  if existing_job["state"] == "completed" or existing_job["state"] == "failed" then
    local pttl = redis.call('PTTL', deduplication_key)
    if pttl > 0 then
      return existing_raw
    end
    redis.call('DEL', deduplication_key)
    return nil
  end

  return existing_raw
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function deduplication_ttl_millis(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  if deduplication["keep_last_if_active"] == true then
    return nil
  end
  return duration_millis(deduplication["ttl"])
end

local function set_deduplication_key(job, job_id, deduplication_prefix, keep_existing_ttl)
  local ttl = deduplication_ttl_millis(job)
  local deduplication = job["options"]["deduplication"]
  local id = deduplication["id"]
  if keep_existing_ttl and ttl and deduplication["extend"] ~= true then
    redis.call('SET', deduplication_prefix .. id, job_id, 'KEEPTTL')
  elseif ttl then
    redis.call('SET', deduplication_prefix .. id, job_id, 'PX', ttl)
  else
    redis.call('SET', deduplication_prefix .. id, job_id)
  end
end

local function can_replace_deduplicated_owner(candidate_job, existing_job)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return false
  end
  local deduplication = candidate_job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null or deduplication["replace"] ~= true then
    return false
  end
  if existing_job["state"] ~= "delayed" then
    return false
  end
  if candidate_job["repeat_key"] and candidate_job["repeat_key"] ~= cjson.null then
    return false
  end
  if existing_job["parent_id"] and existing_job["parent_id"] ~= cjson.null then
    return false
  end
  if existing_job["repeat_key"] and existing_job["repeat_key"] ~= cjson.null then
    return false
  end
  if existing_job["child_ids"] and existing_job["child_ids"] ~= cjson.null and #existing_job["child_ids"] > 0 then
    return false
  end
  return true
end

local function can_store_deduplicated_next(candidate_job, existing_job, active_key)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return false
  end
  local deduplication = candidate_job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null or deduplication["keep_last_if_active"] ~= true then
    return false
  end
  if existing_job["state"] ~= "active" then
    return false
  end
  if not redis.call('ZSCORE', active_key, existing_job["id"]) then
    return false
  end
  if candidate_job["parent_id"] and candidate_job["parent_id"] ~= cjson.null then
    return false
  end
  local candidate_repeat_key = candidate_job["repeat_key"]
  local existing_repeat_key = existing_job["repeat_key"]
  local candidate_has_repeat = candidate_repeat_key ~= nil and candidate_repeat_key ~= cjson.null
  local existing_has_repeat = existing_repeat_key ~= nil and existing_repeat_key ~= cjson.null
  if candidate_has_repeat ~= existing_has_repeat then
    return false
  end
  if candidate_has_repeat and candidate_repeat_key ~= existing_repeat_key then
    return false
  end
  if candidate_job["child_ids"] and candidate_job["child_ids"] ~= cjson.null and #candidate_job["child_ids"] > 0 then
    return false
  end
  return true
end

local function store_deduplicated_next(candidate_raw, candidate_job, deduplication_prefix, deduplication_next_prefix)
  local deduplication_id = candidate_job["options"]["deduplication"]["id"]
  redis.call('SET', deduplication_next_prefix .. deduplication_id, candidate_raw)
  redis.call('PERSIST', deduplication_prefix .. deduplication_id)
end

local function extend_deduplicated_owner(candidate_job, existing_job, deduplication_prefix)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return
  end
  local deduplication = candidate_job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return
  end
  if deduplication["replace"] == true or deduplication["extend"] ~= true or deduplication["keep_last_if_active"] == true then
    return
  end
  local ttl = duration_millis(deduplication["ttl"])
  if ttl then
    redis.call('SET', deduplication_prefix .. deduplication["id"], existing_job["id"], 'PX', ttl)
  end
end

local function deduplication_replace(candidate_job)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return false
  end
  local deduplication = candidate_job["options"]["deduplication"]
  return deduplication and deduplication ~= cjson.null and deduplication["replace"] == true
end

local function emit_deduplication_events(events_key, max_events, event_job_id, deduplication_id, deduplicated_job_id)
  redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'debounced', 'jobId', event_job_id, 'debounceId', deduplication_id)
  redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'deduplicated', 'jobId', event_job_id, 'deduplicationId', deduplication_id, 'deduplicatedJobId', deduplicated_job_id)
end

local function emit_deduplicated_events(events_key, max_events, owner_job, candidate_job)
  local deduplication = candidate_job["options"]["deduplication"]
  local deduplication_id = deduplication["id"]
  emit_deduplication_events(events_key, max_events, owner_job["id"], deduplication_id, candidate_job["id"])
end

local function emit_replaced_deduplicated_owner_events(events_key, max_events, candidate_job, existing_job)
  local deduplication = candidate_job["options"]["deduplication"]
  local deduplication_id = deduplication["id"]
  redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'removed', 'jobId', existing_job["id"], 'prev', 'delayed')
  emit_deduplication_events(events_key, max_events, candidate_job["id"], deduplication_id, existing_job["id"])
end

local function active_repeat_raw(jobs_key, repeat_prefix, repeat_key)
  if not repeat_key or repeat_key == '' then
    return nil
  end

  local owner_key = repeat_prefix .. repeat_key
  local existing_id = redis.call('GET', owner_key)
  if existing_id then
    local existing_raw = redis.call('HGET', jobs_key, existing_id)
    if existing_raw then
      local existing_job = cjson.decode(existing_raw)
      if existing_job["state"] ~= "completed"
        and existing_job["state"] ~= "failed"
        and existing_job["repeat_key"] == repeat_key then
        return existing_raw
      end
    end
    redis.call('DEL', owner_key)
  end

  local scheduler_key = string.sub(repeat_prefix, 1, -2)
  local scheduler_meta_key = string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key
  local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
  if not scheduler_owner_id then
    return nil
  end

  local scheduler_owner_raw = redis.call('HGET', jobs_key, scheduler_owner_id)
  if not scheduler_owner_raw then
    redis.call('ZREM', scheduler_key, repeat_key)
    redis.call('DEL', scheduler_meta_key)
    return nil
  end

  local scheduler_owner_job = cjson.decode(scheduler_owner_raw)
  if scheduler_owner_job["state"] == "completed"
    or scheduler_owner_job["state"] == "failed"
    or scheduler_owner_job["repeat_key"] ~= repeat_key then
    redis.call('ZREM', scheduler_key, repeat_key)
    redis.call('DEL', scheduler_meta_key)
    return nil
  end

  redis.call('SET', owner_key, scheduler_owner_id, 'NX')
  return scheduler_owner_raw
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key
end

local function store_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local count = tonumber(ARGV[1])
local offset = 2
local per_job_args = 7
local waiting_score_bucket = tonumber(ARGV[2 + count * per_job_args])
local deduplication_prefix = ARGV[3 + count * per_job_args]
local repeat_prefix = ARGV[4 + count * per_job_args]
local deduplication_next_prefix = ARGV[5 + count * per_job_args]
local logs_prefix = ARGV[6 + count * per_job_args]
local max_events = ARGV[7 + count * per_job_args]
local added = {}

for index = 1, count do
  local id = ARGV[offset]
  local raw = ARGV[offset + 1]
  local state = ARGV[offset + 2]
  local scheduled_score = ARGV[offset + 3]
  local priority = tonumber(ARGV[offset + 4])
  local deduplication_id = ARGV[offset + 5]
  local repeat_key = ARGV[offset + 6]
  local existing = redis.call('HGET', KEYS[1], id)

  if existing then
    added[index] = existing
  else
    local should_insert = true
    local replaced_deduplicated_owner = false
    local deduplicated = active_deduplicated_raw(KEYS[1], deduplication_prefix, deduplication_id)
    if deduplicated then
      local candidate_job = cjson.decode(raw)
      local existing_job = cjson.decode(deduplicated)
      if can_replace_deduplicated_owner(candidate_job, existing_job) then
        local removed = redis.call('ZREM', KEYS[3], existing_job["id"])
        if removed > 0 then
          redis.call('HDEL', KEYS[1], existing_job["id"])
          redis.call('DEL', logs_prefix .. existing_job["id"])
          emit_replaced_deduplicated_owner_events(KEYS[7], max_events, candidate_job, existing_job)
          replaced_deduplicated_owner = true
        else
          added[index] = deduplicated
          should_insert = false
        end
      elseif can_store_deduplicated_next(candidate_job, existing_job, KEYS[6]) then
        store_deduplicated_next(raw, candidate_job, deduplication_prefix, deduplication_next_prefix)
        emit_deduplicated_events(KEYS[7], max_events, existing_job, candidate_job)
        added[index] = deduplicated
        should_insert = false
      else
        extend_deduplicated_owner(candidate_job, existing_job, deduplication_prefix)
        if not deduplication_replace(candidate_job) then
          emit_deduplicated_events(KEYS[7], max_events, existing_job, candidate_job)
        end
        added[index] = deduplicated
        should_insert = false
      end
    end
    if should_insert then
      local repeat_owner = active_repeat_raw(KEYS[1], repeat_prefix, repeat_key)
      if repeat_owner then
        added[index] = repeat_owner
        should_insert = false
      end
    end
    if should_insert then
      local inserted = redis.call('HSETNX', KEYS[1], id, raw)
      if inserted == 0 then
        local current = redis.call('HGET', KEYS[1], id)
        if current then
          added[index] = current
        else
          return {'missing', id}
        end
      else
        local inserted_raw = raw
        if state == 'waiting' then
          inserted_raw = enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[5], cjson.decode(raw), id, priority, waiting_score_bucket, KEYS[#KEYS])
        elseif state == 'delayed' then
          redis.call('ZADD', KEYS[3], scheduled_score, id)
          refresh_delay_marker(KEYS[#KEYS], KEYS[3])
        elseif state == 'waiting_children' then
          redis.call('ZADD', KEYS[4], scheduled_score, id)
        end
        if deduplication_id ~= '' then
          set_deduplication_key(cjson.decode(raw), id, deduplication_prefix, replaced_deduplicated_owner)
        end
        if repeat_key ~= '' then
          redis.call('SET', repeat_prefix .. repeat_key, id)
          store_repeat_scheduler(cjson.decode(inserted_raw), id, repeat_prefix, scheduled_score)
        end
        local event_job = cjson.decode(inserted_raw)
        redis.call('XADD', KEYS[7], 'MAXLEN', '~', max_events, '*', 'event', 'added', 'jobId', id, 'name', event_job["name"] or '')
        local event_state = state
        if event_state == 'waiting_children' then
          event_state = 'waiting-children'
        end
        redis.call('XADD', KEYS[7], 'MAXLEN', '~', max_events, '*', 'event', event_state, 'jobId', id)
        added[index] = inserted_raw
      end
    end
  end

  offset = offset + per_job_args
end

return added
"#;

const ADD_FLOW_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function release_parent_if_no_pending_dependencies(parent_id, dependency_key, scheduled_score, now_millis, priority, waiting_score_bucket, max_events)
  if redis.call('SCARD', dependency_key) ~= 0 then
    return
  end

  local parent_raw = redis.call('HGET', KEYS[1], parent_id)
  if not parent_raw then
    return
  end

  local parent = cjson.decode(parent_raw)
  if parent["state"] ~= "waiting_children" then
    return
  end

  redis.call('DEL', dependency_key)
  redis.call('ZREM', KEYS[4], parent_id)
  parent["processed_at"] = cjson.null
  parent["finished_at"] = cjson.null
  parent["worker_id"] = cjson.null
  parent["lock_token"] = cjson.null
  parent["lease_expires_at"] = cjson.null
  parent["deferred_failure"] = cjson.null
  parent["failed_reason"] = cjson.null

  if tonumber(scheduled_score) > tonumber(now_millis) then
    parent["state"] = "delayed"
    redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
    redis.call('ZADD', KEYS[3], scheduled_score, parent_id)
    refresh_delay_marker(KEYS[#KEYS], KEYS[3])
    redis.call('XADD', KEYS[6], 'MAXLEN', '~', max_events, '*', 'event', 'delayed', 'jobId', parent_id, 'prev', 'waiting-children')
  else
    parent["state"] = "waiting"
    enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[5], parent, parent_id, priority, waiting_score_bucket, KEYS[#KEYS])
    redis.call('XADD', KEYS[6], 'MAXLEN', '~', max_events, '*', 'event', 'waiting', 'jobId', parent_id, 'prev', 'waiting-children')
  end
end

local function record_processed_child_dependency(dependency_key, child_id, child)
  local return_value = child["return_value"]
  if return_value == nil then
    return_value = cjson.null
  end
  redis.call('HSET', dependency_key .. ':processed', child_id, cjson.encode(return_value))
end

local function active_deduplication_id(jobs_key, deduplication_prefix, deduplication_id)
  if not deduplication_id or deduplication_id == '' then
    return nil
  end

  local deduplication_key = deduplication_prefix .. deduplication_id
  local existing_id = redis.call('GET', deduplication_key)
  if not existing_id then
    return nil
  end

  local existing_raw = redis.call('HGET', jobs_key, existing_id)
  if not existing_raw then
    redis.call('DEL', deduplication_key)
    return nil
  end

  local existing_job = cjson.decode(existing_raw)
  if existing_job["state"] == "completed" or existing_job["state"] == "failed" then
    local pttl = redis.call('PTTL', deduplication_key)
    if pttl > 0 then
      return existing_id
    end
    redis.call('DEL', deduplication_key)
    return nil
  end

  return existing_id
end

local function can_store_deduplicated_next_flow(parent_job, child_jobs, existing_job, active_key)
  if not parent_job["options"] or parent_job["options"] == cjson.null then
    return false
  end
  local deduplication = parent_job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null or deduplication["keep_last_if_active"] ~= true then
    return false
  end
  if existing_job["state"] ~= "active" then
    return false
  end
  if not redis.call('ZSCORE', active_key, existing_job["id"]) then
    return false
  end
  if parent_job["parent_id"] and parent_job["parent_id"] ~= cjson.null then
    return false
  end
  if parent_job["repeat_key"] and parent_job["repeat_key"] ~= cjson.null then
    return false
  end
  if existing_job["repeat_key"] and existing_job["repeat_key"] ~= cjson.null then
    return false
  end
  for _, child in ipairs(child_jobs) do
    if child["repeat_key"] and child["repeat_key"] ~= cjson.null then
      return false
    end
  end
  return true
end

local function store_deduplicated_next_flow(parent_raw, child_raws, deduplication_prefix, deduplication_next_prefix)
  local parent = cjson.decode(parent_raw)
  local children = {}
  for index, child_raw in ipairs(child_raws) do
    children[index] = cjson.decode(child_raw)
  end
  local deduplication_id = parent["options"]["deduplication"]["id"]
  redis.call('SET', deduplication_next_prefix .. deduplication_id, cjson.encode({
    kind = 'flow',
    parent = parent,
    children = children
  }))
  redis.call('PERSIST', deduplication_prefix .. deduplication_id)
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function deduplication_ttl_millis(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  if deduplication["keep_last_if_active"] == true then
    return nil
  end
  return duration_millis(deduplication["ttl"])
end

local function set_deduplication_key(job, job_id, deduplication_prefix)
  local ttl = deduplication_ttl_millis(job)
  local id = job["options"]["deduplication"]["id"]
  if ttl then
    redis.call('SET', deduplication_prefix .. id, job_id, 'PX', ttl)
  else
    redis.call('SET', deduplication_prefix .. id, job_id)
  end
end

local function extend_deduplicated_owner(candidate_job, existing_job, deduplication_prefix)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return
  end
  local deduplication = candidate_job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return
  end
  if deduplication["extend"] ~= true or deduplication["keep_last_if_active"] == true then
    return
  end
  local ttl = duration_millis(deduplication["ttl"])
  if ttl then
    redis.call('SET', deduplication_prefix .. deduplication["id"], existing_job["id"], 'PX', ttl)
  end
end

local function emit_deduplicated_events(events_key, max_events, owner_job, candidate_job)
  local deduplication = candidate_job["options"]["deduplication"]
  local deduplication_id = deduplication["id"]
  redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'debounced', 'jobId', owner_job["id"], 'debounceId', deduplication_id)
  redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'deduplicated', 'jobId', owner_job["id"], 'deduplicationId', deduplication_id, 'deduplicatedJobId', candidate_job["id"])
end

local function flow_child_deduplication_requires_next(candidate_job, existing_job)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return false
  end
  local deduplication = candidate_job["options"]["deduplication"]
  return deduplication
    and deduplication ~= cjson.null
    and deduplication["keep_last_if_active"] == true
    and existing_job["state"] == "active"
end

local function active_repeat_id(jobs_key, repeat_prefix, repeat_key)
  if not repeat_key or repeat_key == '' then
    return nil
  end

  local owner_key = repeat_prefix .. repeat_key
  local existing_id = redis.call('GET', owner_key)
  if existing_id then
    local existing_raw = redis.call('HGET', jobs_key, existing_id)
    if existing_raw then
      local existing_job = cjson.decode(existing_raw)
      if existing_job["state"] ~= "completed"
        and existing_job["state"] ~= "failed"
        and existing_job["repeat_key"] == repeat_key then
        return existing_id
      end
    end
    redis.call('DEL', owner_key)
  end

  local scheduler_key = string.sub(repeat_prefix, 1, -2)
  local scheduler_meta_key = string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key
  local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
  if not scheduler_owner_id then
    return nil
  end

  local scheduler_owner_raw = redis.call('HGET', jobs_key, scheduler_owner_id)
  if not scheduler_owner_raw then
    redis.call('ZREM', scheduler_key, repeat_key)
    redis.call('DEL', scheduler_meta_key)
    return nil
  end

  local scheduler_owner_job = cjson.decode(scheduler_owner_raw)
  if scheduler_owner_job["state"] == "completed"
    or scheduler_owner_job["state"] == "failed"
    or scheduler_owner_job["repeat_key"] ~= repeat_key then
    redis.call('ZREM', scheduler_key, repeat_key)
    redis.call('DEL', scheduler_meta_key)
    return nil
  end

  redis.call('SET', owner_key, scheduler_owner_id, 'NX')
  return scheduler_owner_id
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key
end

local function store_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local count = tonumber(ARGV[1])
local offset = 2
local per_job_args = 7
local waiting_score_bucket = tonumber(ARGV[2 + count * per_job_args])
local dependency_prefix = ARGV[3 + count * per_job_args]
local deduplication_prefix = ARGV[4 + count * per_job_args]
local repeat_prefix = ARGV[5 + count * per_job_args]
local deduplication_next_prefix = ARGV[6 + count * per_job_args]
local max_events = ARGV[7 + count * per_job_args]
local now_millis = ARGV[8 + count * per_job_args]
local staged_deduplication_ids = {}
local staged_repeat_keys = {}
local skipped_job_ids = {}
local duplicated_child_ids = {}
local deduplicated_child_owner_ids = {}
local deduplicated_next_child_ids = {}
local deduplicated_next_child_id_by_deduplication = {}
local duplicated_parent_id = nil
local parent_raw = ARGV[3]
local child_raws = {}
local child_jobs = {}
local child_offset = 2 + per_job_args
for index = 2, count do
  child_raws[#child_raws + 1] = ARGV[child_offset + 1]
  child_jobs[#child_jobs + 1] = cjson.decode(ARGV[child_offset + 1])
  child_offset = child_offset + per_job_args
end

for index = 1, count do
  local id = ARGV[offset]
  local raw = ARGV[offset + 1]
  local deduplication_id = ARGV[offset + 5]
  local repeat_key = ARGV[offset + 6]
  local existing_raw_by_id = redis.call('HGET', KEYS[1], id)
  if existing_raw_by_id then
    if index == 1 then
      duplicated_parent_id = id
    else
      local existing_child = cjson.decode(existing_raw_by_id)
      local existing_parent_id = existing_child["parent_id"]
      if existing_parent_id and existing_parent_id ~= cjson.null and existing_parent_id ~= ARGV[2] and redis.call('HGET', KEYS[1], existing_parent_id) then
        return {'parent_conflict', id, existing_parent_id}
      end
      duplicated_child_ids[id] = true
    end
  end
  local deduplicated_id = nil
  if not duplicated_child_ids[id] and not (index == 1 and duplicated_parent_id) then
    deduplicated_id = active_deduplication_id(KEYS[1], deduplication_prefix, deduplication_id)
  end
  if deduplicated_id then
    if index == 1 then
      local existing_raw = redis.call('HGET', KEYS[1], deduplicated_id)
      if existing_raw then
        local parent_job = cjson.decode(raw)
        local existing_job = cjson.decode(existing_raw)
        if can_store_deduplicated_next_flow(parent_job, child_jobs, existing_job, KEYS[7]) then
          store_deduplicated_next_flow(parent_raw, child_raws, deduplication_prefix, deduplication_next_prefix)
          emit_deduplicated_events(KEYS[6], max_events, existing_job, parent_job)
          return {'deduplicated_parent', deduplicated_id}
        end
        extend_deduplicated_owner(parent_job, existing_job, deduplication_prefix)
        emit_deduplicated_events(KEYS[6], max_events, existing_job, parent_job)
        return {'deduplicated_parent', deduplicated_id}
      end
    else
      local existing_raw = redis.call('HGET', KEYS[1], deduplicated_id)
      if existing_raw then
        local child_job = cjson.decode(raw)
        local existing_job = cjson.decode(existing_raw)
        if flow_child_deduplication_requires_next(child_job, existing_job) then
          redis.call('SET', deduplication_next_prefix .. deduplication_id, raw)
          redis.call('PERSIST', deduplication_prefix .. deduplication_id)
          skipped_job_ids[id] = true
          deduplicated_child_owner_ids[id] = deduplicated_id
          local previous_next_child_id = deduplicated_next_child_id_by_deduplication[deduplication_id]
          if previous_next_child_id then
            deduplicated_next_child_ids[previous_next_child_id] = nil
          end
          deduplicated_next_child_id_by_deduplication[deduplication_id] = id
          deduplicated_next_child_ids[id] = true
        else
          extend_deduplicated_owner(child_job, existing_job, deduplication_prefix)
          skipped_job_ids[id] = true
          deduplicated_child_owner_ids[id] = deduplicated_id
        end
      else
        return {'deduplicated', deduplicated_id}
      end
    end
  end
  if not skipped_job_ids[id] and not duplicated_child_ids[id] and not (index == 1 and duplicated_parent_id) then
    if deduplication_id ~= '' then
      if staged_deduplication_ids[deduplication_id] then
        return {'deduplicated', staged_deduplication_ids[deduplication_id]}
      end
      staged_deduplication_ids[deduplication_id] = id
    end
    local repeat_owner_id = active_repeat_id(KEYS[1], repeat_prefix, repeat_key)
    if repeat_owner_id then
      return {'repeat', repeat_owner_id}
    end
    if repeat_key ~= '' then
      if staged_repeat_keys[repeat_key] then
        return {'repeat', staged_repeat_keys[repeat_key]}
      end
      staged_repeat_keys[repeat_key] = id
    end
  end
  offset = offset + per_job_args
end

offset = 2
for index = 1, count do
  local id = ARGV[offset]
  local raw = ARGV[offset + 1]
  local state = ARGV[offset + 2]
  local scheduled_score = ARGV[offset + 3]
  local priority = tonumber(ARGV[offset + 4])
  local deduplication_id = ARGV[offset + 5]
  local repeat_key = ARGV[offset + 6]

  if index == 1 and duplicated_parent_id then
    local existing_parent_raw = redis.call('HGET', KEYS[1], id)
    if existing_parent_raw then
      redis.call('XADD', KEYS[6], 'MAXLEN', '~', max_events, '*', 'event', 'duplicated', 'jobId', id)
    end
  elseif skipped_job_ids[id] then
    local owner_raw = redis.call('HGET', KEYS[1], deduplicated_child_owner_ids[id])
    if owner_raw then
      emit_deduplicated_events(KEYS[6], max_events, cjson.decode(owner_raw), cjson.decode(raw))
    end
  elseif duplicated_child_ids[id] then
    local existing_raw = redis.call('HGET', KEYS[1], id)
    if existing_raw then
      local existing_child = cjson.decode(existing_raw)
      existing_child["parent_id"] = ARGV[2]
      redis.call('HSET', KEYS[1], id, cjson.encode(existing_child))
      redis.call('XADD', KEYS[6], 'MAXLEN', '~', max_events, '*', 'event', 'duplicated', 'jobId', id)
    end
  else
    if index == 1 then
      local parent_job = cjson.decode(raw)
      if parent_job["child_ids"] and parent_job["child_ids"] ~= cjson.null then
        local retained_child_ids = {}
        for _, child_id in ipairs(parent_job["child_ids"]) do
          if not skipped_job_ids[child_id] or deduplicated_next_child_ids[child_id] then
            retained_child_ids[#retained_child_ids + 1] = child_id
          end
        end
        parent_job["child_ids"] = retained_child_ids
        raw = cjson.encode(parent_job)
      end
    end
    redis.call('HSET', KEYS[1], id, raw)
    if state == 'waiting' then
      enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[5], cjson.decode(raw), id, priority, waiting_score_bucket, KEYS[#KEYS])
    elseif state == 'delayed' then
      redis.call('ZADD', KEYS[3], scheduled_score, id)
      refresh_delay_marker(KEYS[#KEYS], KEYS[3])
    elseif state == 'waiting_children' then
      redis.call('ZADD', KEYS[4], scheduled_score, id)
    end
    if deduplication_id ~= '' then
      set_deduplication_key(cjson.decode(raw), id, deduplication_prefix)
    end
    if repeat_key ~= '' then
      redis.call('SET', repeat_prefix .. repeat_key, id)
      store_repeat_scheduler(cjson.decode(raw), id, repeat_prefix, scheduled_score)
    end
    local event_job = cjson.decode(raw)
    redis.call('XADD', KEYS[6], 'MAXLEN', '~', max_events, '*', 'event', 'added', 'jobId', id, 'name', event_job["name"] or '')
    local event_state = state
    if event_state == 'waiting_children' then
      event_state = 'waiting-children'
    end
    redis.call('XADD', KEYS[6], 'MAXLEN', '~', max_events, '*', 'event', event_state, 'jobId', id)
  end

  offset = offset + per_job_args
end

if duplicated_parent_id then
  local existing_parent_raw = redis.call('HGET', KEYS[1], ARGV[2])
  if existing_parent_raw then
    local existing_parent = cjson.decode(existing_parent_raw)
    if not existing_parent["child_ids"] or existing_parent["child_ids"] == cjson.null then
      existing_parent["child_ids"] = {}
    end
    local existing_child_ids = {}
    for _, child_id in ipairs(existing_parent["child_ids"]) do
      existing_child_ids[child_id] = true
    end
    local child_offset = 2 + per_job_args
    for index = 2, count do
      local child_id = ARGV[child_offset]
      if (not skipped_job_ids[child_id] or deduplicated_next_child_ids[child_id]) and not existing_child_ids[child_id] then
        existing_parent["child_ids"][#existing_parent["child_ids"] + 1] = child_id
        existing_child_ids[child_id] = true
      end
      child_offset = child_offset + per_job_args
    end
    redis.call('HSET', KEYS[1], ARGV[2], cjson.encode(existing_parent))
  end
end

if count > 1 and (ARGV[4] == 'waiting_children' or duplicated_parent_id) then
  local parent_id = ARGV[2]
  local dependency_key = dependency_prefix .. parent_id
  if not duplicated_parent_id then
    redis.call('DEL', dependency_key)
  end

  local child_offset = 2 + per_job_args
  for index = 2, count do
    if not skipped_job_ids[ARGV[child_offset]] then
      local child_id = ARGV[child_offset]
      if duplicated_child_ids[child_id] then
        local existing_child_raw = redis.call('HGET', KEYS[1], child_id)
        if existing_child_raw then
          local existing_child = cjson.decode(existing_child_raw)
          if existing_child["state"] == "completed" then
            record_processed_child_dependency(dependency_key, child_id, existing_child)
          else
            redis.call('SADD', dependency_key, child_id)
          end
        end
      else
        redis.call('SADD', dependency_key, child_id)
      end
    end
    child_offset = child_offset + per_job_args
  end

  local has_deduplicated_next_child = false
  for _, _ in pairs(deduplicated_next_child_ids) do
    has_deduplicated_next_child = true
    break
  end
  if not has_deduplicated_next_child then
    release_parent_if_no_pending_dependencies(parent_id, dependency_key, ARGV[5], now_millis, tonumber(ARGV[6]), waiting_score_bucket, max_events)
  end
end

local result = {'ok'}
local skipped_offset = 2 + per_job_args
for index = 2, count do
  local child_id = ARGV[skipped_offset]
  if skipped_job_ids[child_id] then
    result[#result + 1] = child_id
  end
  skipped_offset = skipped_offset + per_job_args
end

return result
"#;

const ADD_FLOW_CHILDREN_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function release_parent_if_no_pending_dependencies(parent_id, dependency_key, waiting_score_bucket, max_events)
  if redis.call('SCARD', dependency_key) ~= 0 then
    return
  end

  local parent_raw = redis.call('HGET', KEYS[1], parent_id)
  if not parent_raw then
    return
  end

  local parent = cjson.decode(parent_raw)
  if parent["state"] ~= "waiting_children" then
    return
  end

  redis.call('DEL', dependency_key)
  redis.call('ZREM', KEYS[4], parent_id)
  parent["processed_at"] = cjson.null
  parent["finished_at"] = cjson.null
  parent["worker_id"] = cjson.null
  parent["lock_token"] = cjson.null
  parent["lease_expires_at"] = cjson.null
  parent["deferred_failure"] = cjson.null
  parent["failed_reason"] = cjson.null
  parent["state"] = "waiting"

  local priority = tonumber(parent["priority"] or '1000') or 1000
  enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[6], parent, parent_id, priority, waiting_score_bucket, KEYS[9])
  redis.call('XADD', KEYS[7], 'MAXLEN', '~', max_events, '*', 'event', 'waiting', 'jobId', parent_id, 'prev', 'waiting-children')
end

local function record_processed_child_dependency(dependency_key, child_id, child)
  local return_value = child["return_value"]
  if return_value == nil then
    return_value = cjson.null
  end
  redis.call('HSET', dependency_key .. ':processed', child_id, cjson.encode(return_value))
end

local function active_deduplication_id(jobs_key, deduplication_prefix, deduplication_id)
  if not deduplication_id or deduplication_id == '' then
    return nil
  end

  local deduplication_key = deduplication_prefix .. deduplication_id
  local existing_id = redis.call('GET', deduplication_key)
  if not existing_id then
    return nil
  end

  local existing_raw = redis.call('HGET', jobs_key, existing_id)
  if not existing_raw then
    redis.call('DEL', deduplication_key)
    return nil
  end

  local existing_job = cjson.decode(existing_raw)
  if existing_job["state"] == "completed" or existing_job["state"] == "failed" then
    local pttl = redis.call('PTTL', deduplication_key)
    if pttl > 0 then
      return existing_id
    end
    redis.call('DEL', deduplication_key)
    return nil
  end

  return existing_id
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function deduplication_ttl_millis(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  if deduplication["keep_last_if_active"] == true then
    return nil
  end
  return duration_millis(deduplication["ttl"])
end

local function set_deduplication_key(job, job_id, deduplication_prefix)
  local ttl = deduplication_ttl_millis(job)
  local id = job["options"]["deduplication"]["id"]
  if ttl then
    redis.call('SET', deduplication_prefix .. id, job_id, 'PX', ttl)
  else
    redis.call('SET', deduplication_prefix .. id, job_id)
  end
end

local function extend_deduplicated_owner(candidate_job, existing_job, deduplication_prefix)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return
  end
  local deduplication = candidate_job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return
  end
  if deduplication["extend"] ~= true or deduplication["keep_last_if_active"] == true then
    return
  end
  local ttl = duration_millis(deduplication["ttl"])
  if ttl then
    redis.call('SET', deduplication_prefix .. deduplication["id"], existing_job["id"], 'PX', ttl)
  end
end

local function emit_deduplicated_events(events_key, max_events, owner_job, candidate_job)
  local deduplication = candidate_job["options"]["deduplication"]
  local deduplication_id = deduplication["id"]
  redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'debounced', 'jobId', owner_job["id"], 'debounceId', deduplication_id)
  redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'deduplicated', 'jobId', owner_job["id"], 'deduplicationId', deduplication_id, 'deduplicatedJobId', candidate_job["id"])
end

local function flow_child_deduplication_requires_next(candidate_job, existing_job)
  if not candidate_job["options"] or candidate_job["options"] == cjson.null then
    return false
  end
  local deduplication = candidate_job["options"]["deduplication"]
  return deduplication
    and deduplication ~= cjson.null
    and deduplication["keep_last_if_active"] == true
    and existing_job["state"] == "active"
end

local function active_repeat_id(jobs_key, repeat_prefix, repeat_key)
  if not repeat_key or repeat_key == '' then
    return nil
  end

  local owner_key = repeat_prefix .. repeat_key
  local existing_id = redis.call('GET', owner_key)
  if existing_id then
    local existing_raw = redis.call('HGET', jobs_key, existing_id)
    if existing_raw then
      local existing_job = cjson.decode(existing_raw)
      if existing_job["state"] ~= "completed"
        and existing_job["state"] ~= "failed"
        and existing_job["repeat_key"] == repeat_key then
        return existing_id
      end
    end
    redis.call('DEL', owner_key)
  end

  local scheduler_key = string.sub(repeat_prefix, 1, -2)
  local scheduler_meta_key = string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key
  local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
  if not scheduler_owner_id then
    return nil
  end

  local scheduler_owner_raw = redis.call('HGET', jobs_key, scheduler_owner_id)
  if not scheduler_owner_raw then
    redis.call('ZREM', scheduler_key, repeat_key)
    redis.call('DEL', scheduler_meta_key)
    return nil
  end

  local scheduler_owner_job = cjson.decode(scheduler_owner_raw)
  if scheduler_owner_job["state"] == "completed"
    or scheduler_owner_job["state"] == "failed"
    or scheduler_owner_job["repeat_key"] ~= repeat_key then
    redis.call('ZREM', scheduler_key, repeat_key)
    redis.call('DEL', scheduler_meta_key)
    return nil
  end

  redis.call('SET', owner_key, scheduler_owner_id, 'NX')
  return scheduler_owner_id
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key
end

local function store_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local parent_id = ARGV[1]
local token = ARGV[2]
local count = tonumber(ARGV[3])
if not count or count < 1 then
  return {'empty'}
end

local parent_raw = redis.call('HGET', KEYS[1], parent_id)
if not parent_raw then
  return {'missing', parent_id}
end
local parent = cjson.decode(parent_raw)
if parent["state"] ~= "active" then
  return {'state', parent["state"] or ''}
end

local lock_token = redis.call('GET', KEYS[10])
if not lock_token then
  return {'lock_missing'}
end
if lock_token ~= token then
  return {'lock_mismatch'}
end

local offset = 4
local per_job_args = 7
local waiting_score_bucket = tonumber(ARGV[4 + count * per_job_args])
local dependency_prefix = ARGV[5 + count * per_job_args]
local deduplication_prefix = ARGV[6 + count * per_job_args]
local repeat_prefix = ARGV[7 + count * per_job_args]
local deduplication_next_prefix = ARGV[8 + count * per_job_args]
local max_events = ARGV[9 + count * per_job_args]
local now_millis = ARGV[10 + count * per_job_args]
local dependency_key = dependency_prefix .. parent_id
if redis.call('ZCARD', dependency_key .. ':unsuccessful') ~= 0 then
  return {'failed_dependencies'}
end
for _, child_id in ipairs(parent["child_ids"] or {}) do
  local child_raw = redis.call('HGET', KEYS[1], child_id)
  if child_raw then
    local child = cjson.decode(child_raw)
    if child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true)) then
      return {'failed_dependencies'}
    end
  end
end
local staged_ids = {}
local staged_deduplication_ids = {}
local staged_repeat_keys = {}
local skipped_job_ids = {}
local duplicated_child_ids = {}
local deduplicated_child_owner_ids = {}
local deduplicated_next_child_ids = {}
local deduplicated_next_child_id_by_deduplication = {}
local existing_child_ids = {}

for _, child_id in ipairs(parent["child_ids"] or {}) do
  existing_child_ids[child_id] = true
end

for index = 1, count do
  local id = ARGV[offset]
  local raw = ARGV[offset + 1]
  local deduplication_id = ARGV[offset + 5]
  local repeat_key = ARGV[offset + 6]
  local existing_raw_by_id = redis.call('HGET', KEYS[1], id)
  if id == parent_id or staged_ids[id] then
    return {'exists', id}
  end
  if existing_raw_by_id then
    local existing_child = cjson.decode(existing_raw_by_id)
    local existing_parent_id = existing_child["parent_id"]
    if existing_parent_id and existing_parent_id ~= cjson.null and existing_parent_id ~= parent_id and redis.call('HGET', KEYS[1], existing_parent_id) then
      return {'parent_conflict', id, existing_parent_id}
    end
    duplicated_child_ids[id] = true
  elseif existing_child_ids[id] then
    return {'exists', id}
  end
  staged_ids[id] = true

  if not duplicated_child_ids[id] then
    local deduplicated_id = active_deduplication_id(KEYS[1], deduplication_prefix, deduplication_id)
    if deduplicated_id then
      local existing_raw = redis.call('HGET', KEYS[1], deduplicated_id)
      if existing_raw then
        local child_job = cjson.decode(raw)
        local existing_job = cjson.decode(existing_raw)
        if flow_child_deduplication_requires_next(child_job, existing_job) then
          redis.call('SET', deduplication_next_prefix .. deduplication_id, raw)
          redis.call('PERSIST', deduplication_prefix .. deduplication_id)
          skipped_job_ids[id] = true
          deduplicated_child_owner_ids[id] = deduplicated_id
          local previous_next_child_id = deduplicated_next_child_id_by_deduplication[deduplication_id]
          if previous_next_child_id then
            deduplicated_next_child_ids[previous_next_child_id] = nil
          end
          deduplicated_next_child_id_by_deduplication[deduplication_id] = id
          deduplicated_next_child_ids[id] = true
        else
          extend_deduplicated_owner(child_job, existing_job, deduplication_prefix)
          skipped_job_ids[id] = true
          deduplicated_child_owner_ids[id] = deduplicated_id
        end
      else
        return {'deduplicated', deduplicated_id}
      end
    end

    if not skipped_job_ids[id] then
      if deduplication_id ~= '' then
        if staged_deduplication_ids[deduplication_id] then
          return {'deduplicated', staged_deduplication_ids[deduplication_id]}
        end
        staged_deduplication_ids[deduplication_id] = id
      end

      local repeat_owner_id = active_repeat_id(KEYS[1], repeat_prefix, repeat_key)
      if repeat_owner_id then
        return {'repeat', repeat_owner_id}
      end
      if repeat_key ~= '' then
        if staged_repeat_keys[repeat_key] then
          return {'repeat', staged_repeat_keys[repeat_key]}
        end
        staged_repeat_keys[repeat_key] = id
      end
    end
  end

  offset = offset + per_job_args
end

redis.call('DEL', KEYS[10])
redis.call('SREM', KEYS[8], parent_id)
redis.call('ZREM', KEYS[5], parent_id)

parent["state"] = "waiting_children"
parent["processed_at"] = cjson.null
parent["finished_at"] = cjson.null
parent["worker_id"] = cjson.null
parent["lock_token"] = cjson.null
parent["lease_expires_at"] = cjson.null
parent["deferred_failure"] = cjson.null
parent["failed_reason"] = cjson.null
if not parent["child_ids"] or parent["child_ids"] == cjson.null then
  parent["child_ids"] = {}
end

offset = 4
for index = 1, count do
  local id = ARGV[offset]
  local raw = ARGV[offset + 1]
  local state = ARGV[offset + 2]
  local scheduled_score = ARGV[offset + 3]
  local priority = tonumber(ARGV[offset + 4])
  local deduplication_id = ARGV[offset + 5]
  local repeat_key = ARGV[offset + 6]
  local child = cjson.decode(raw)

  if skipped_job_ids[id] then
    local owner_raw = redis.call('HGET', KEYS[1], deduplicated_child_owner_ids[id])
    if owner_raw then
      emit_deduplicated_events(KEYS[7], max_events, cjson.decode(owner_raw), child)
    end
    if deduplicated_next_child_ids[id] then
      if not existing_child_ids[id] then
        table.insert(parent["child_ids"], id)
        existing_child_ids[id] = true
      end
      redis.call('SADD', dependency_key, id)
    end
  elseif duplicated_child_ids[id] then
    if not existing_child_ids[id] then
      table.insert(parent["child_ids"], id)
      existing_child_ids[id] = true
    end
    local existing_raw = redis.call('HGET', KEYS[1], id)
    if existing_raw then
      local existing_child = cjson.decode(existing_raw)
      existing_child["parent_id"] = parent_id
      redis.call('HSET', KEYS[1], id, cjson.encode(existing_child))
      if existing_child["state"] == "completed" then
        record_processed_child_dependency(dependency_key, id, existing_child)
      else
        redis.call('SADD', dependency_key, id)
      end
      redis.call('XADD', KEYS[7], 'MAXLEN', '~', max_events, '*', 'event', 'duplicated', 'jobId', id)
    end
  else
    if not existing_child_ids[id] then
      table.insert(parent["child_ids"], id)
      existing_child_ids[id] = true
    end
    redis.call('HSET', KEYS[1], id, raw)
    redis.call('SADD', dependency_key, id)
    if state == 'waiting' then
      enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[6], child, id, priority, waiting_score_bucket, KEYS[9])
    elseif state == 'delayed' then
      redis.call('ZADD', KEYS[3], scheduled_score, id)
      refresh_delay_marker(KEYS[9], KEYS[3])
    elseif state == 'waiting_children' then
      redis.call('ZADD', KEYS[4], scheduled_score, id)
    end
    if deduplication_id ~= '' then
      set_deduplication_key(child, id, deduplication_prefix)
    end
    if repeat_key ~= '' then
      redis.call('SET', repeat_prefix .. repeat_key, id)
      store_repeat_scheduler(child, id, repeat_prefix, scheduled_score)
    end
    redis.call('XADD', KEYS[7], 'MAXLEN', '~', max_events, '*', 'event', 'added', 'jobId', id, 'name', child["name"] or '')
    local event_state = state
    if event_state == 'waiting_children' then
      event_state = 'waiting-children'
    end
    redis.call('XADD', KEYS[7], 'MAXLEN', '~', max_events, '*', 'event', event_state, 'jobId', id)
  end

  offset = offset + per_job_args
end

redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
redis.call('ZADD', KEYS[4], now_millis, parent_id)
redis.call('XADD', KEYS[7], 'MAXLEN', '~', max_events, '*', 'event', 'waiting-children', 'jobId', parent_id, 'prev', 'active')
release_parent_if_no_pending_dependencies(parent_id, dependency_key, waiting_score_bucket, max_events)

local result = {'ok'}
offset = 4
for index = 1, count do
  local child_id = ARGV[offset]
  if skipped_job_ids[child_id] then
    result[#result + 1] = child_id
  end
  offset = offset + per_job_args
end

return result
"#;

const CLAIM_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function days_from_civil(year, month, day)
  if month <= 2 then
    year = year - 1
  end
  local era = math.floor(year / 400)
  local yoe = year - era * 400
  local shifted_month = month
  if month > 2 then
    shifted_month = month - 3
  else
    shifted_month = month + 9
  end
  local doy = math.floor((153 * shifted_month + 2) / 5) + day - 1
  local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
  return era * 146097 + doe - 719468
end

local function iso_to_millis(value)
  local year = tonumber(string.sub(value, 1, 4))
  local month = tonumber(string.sub(value, 6, 7))
  local day = tonumber(string.sub(value, 9, 10))
  local hour = tonumber(string.sub(value, 12, 13))
  local minute = tonumber(string.sub(value, 15, 16))
  local second = tonumber(string.sub(value, 18, 19))
  if not year or not month or not day or not hour or not minute or not second then
    return 0
  end

  local millis = 0
  if string.sub(value, 20, 20) == "." then
    local digits = string.match(string.sub(value, 21), "^(%d+)")
    if digits then
      digits = string.sub(digits .. "000", 1, 3)
      millis = tonumber(digits) or 0
    end
  end

  local days = days_from_civil(year, month, day)
  return (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function sync_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = repeat_key(job)
  if not key or not repeat_prefix or repeat_prefix == '' then
    return
  end

  local owner_key = repeat_prefix .. key
  local owner_id = redis.call('GET', owner_key)
  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local scheduler_owner_id = redis.call('HGET', meta_key, 'jid')
  if owner_id and owner_id ~= job_id then
    return
  end
  if not owner_id and scheduler_owner_id and scheduler_owner_id ~= job_id then
    return
  end
  if not owner_id then
    redis.call('SET', owner_key, job_id, 'NX')
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function queue_is_paused_or_maxed(meta_key, active_key)
  local paused = redis.call('HGET', meta_key, 'paused')
  if paused == '0' then
    redis.call('HDEL', meta_key, 'paused')
  elseif paused then
    return true
  end

  local max_concurrency = tonumber(redis.call('HGET', meta_key, 'concurrency') or '0')
  if max_concurrency and max_concurrency > 0 then
    local active_count = redis.call('ZCARD', active_key)
    if active_count >= max_concurrency then
      return true
    end
  end

  return false
end

local is_paused_or_maxed = queue_is_paused_or_maxed(KEYS[5], KEYS[2])
local promotion_marker_key = KEYS[#KEYS]
if is_paused_or_maxed then
  promotion_marker_key = ''
end

local due_ids = redis.call('ZRANGEBYSCORE', KEYS[6], '-inf', ARGV[10], 'LIMIT', 0, ARGV[12])
for _, due_id in ipairs(due_ids) do
  local delayed_raw = redis.call('HGET', KEYS[3], due_id)
  redis.call('ZREM', KEYS[6], due_id)
  if delayed_raw then
    local delayed_job = cjson.decode(delayed_raw)
    if delayed_job["state"] == "delayed" then
      delayed_job["state"] = "waiting"
      local priority = tonumber(delayed_job["priority"] or '1000') or 1000
      enqueue_waiting_job(KEYS[3], KEYS[1], KEYS[7], delayed_job, due_id, priority, ARGV[11], promotion_marker_key)
      sync_repeat_scheduler(delayed_job, due_id, ARGV[15], iso_to_millis(delayed_job["scheduled_at"]))
      redis.call('XADD', KEYS[8], 'MAXLEN', '~', ARGV[14], '*', 'event', 'waiting', 'jobId', due_id, 'prev', 'delayed')
    end
  end
end
refresh_delay_marker(KEYS[#KEYS], KEYS[6])

local rate_limit_max = tonumber(ARGV[8])
local rate_limit_duration = tonumber(ARGV[9])
if not rate_limit_max or rate_limit_max <= 0 then
  rate_limit_max = tonumber(redis.call('HGET', KEYS[5], 'max') or '0')
  rate_limit_duration = tonumber(redis.call('HGET', KEYS[5], 'duration') or '0')
end
if not rate_limit_duration or rate_limit_duration <= 0 then
  rate_limit_max = 0
end
if rate_limit_max and rate_limit_max > 0 then
  local current_claims = tonumber(redis.call('GET', KEYS[4]) or '0')
  if current_claims >= rate_limit_max then
    return nil
  end
end

if is_paused_or_maxed then
  return nil
end

local candidate_limit = tonumber(ARGV[13]) or 1
local ids = redis.call('ZRANGE', KEYS[1], 0, candidate_limit - 1)
if #ids == 0 then
  return nil
end

for _, id in ipairs(ids) do
  local raw = redis.call('HGET', KEYS[3], id)
  if not raw then
    redis.call('ZREM', KEYS[1], id)
  else
    local job = cjson.decode(raw)
    if job["state"] ~= "waiting" then
      redis.call('ZREM', KEYS[1], id)
    else
      local lock_key = ARGV[7] .. id
      redis.call('SET', lock_key, ARGV[5], 'PX', ARGV[6])
      if rate_limit_max and rate_limit_max > 0 then
        local counter = redis.call('INCR', KEYS[4])
        if counter == 1 then
          redis.call('PEXPIRE', KEYS[4], rate_limit_duration)
        end
      end

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
      sync_repeat_scheduler(job, id, ARGV[15], iso_to_millis(job["scheduled_at"]))
      redis.call('XADD', KEYS[8], 'MAXLEN', '~', ARGV[14], '*', 'event', 'active', 'jobId', id, 'prev', 'waiting')
      redis.call('ZADD', KEYS[#KEYS], 0, '0')
      return updated
    end
  end
end

return nil
"#;

const CLAIM_RATE_LIMIT_TTL_SCRIPT: &str = r#"
local function rate_limit_ttl(max_jobs, rate_limit_key)
  if max_jobs and max_jobs <= tonumber(redis.call('GET', rate_limit_key) or '0') then
    local ttl = redis.call('PTTL', rate_limit_key)
    if ttl == 0 then
      redis.call('DEL', rate_limit_key)
    end
    if ttl > 0 then
      return ttl
    end
  end
  return 0
end

local max_jobs = tonumber(ARGV[1])
if max_jobs and max_jobs > 0 then
  return rate_limit_ttl(max_jobs, KEYS[1])
end

local configured_max = redis.call('HGET', KEYS[2], 'max')
if configured_max then
  return rate_limit_ttl(tonumber(configured_max), KEYS[1])
end

return redis.call('PTTL', KEYS[1])
"#;

const ACTIVE_CLAIM_RATE_LIMIT_TTL_SCRIPT: &str = r#"
local function rate_limit_ttl(max_jobs, rate_limit_key)
  if max_jobs and max_jobs <= tonumber(redis.call('GET', rate_limit_key) or '0') then
    local ttl = redis.call('PTTL', rate_limit_key)
    if ttl == 0 then
      redis.call('DEL', rate_limit_key)
    end
    if ttl > 0 then
      return ttl
    end
  end
  return 0
end

local max_jobs = tonumber(ARGV[1])
if max_jobs and max_jobs > 0 then
  return rate_limit_ttl(max_jobs, KEYS[1])
end

local configured_max = redis.call('HGET', KEYS[2], 'max')
if configured_max then
  return rate_limit_ttl(tonumber(configured_max), KEYS[1])
end

return 0
"#;

const IS_MAXED_SCRIPT: &str = r#"
local concurrency = tonumber(redis.call('HGET', KEYS[1], 'concurrency') or '0')
if not concurrency or concurrency <= 0 then
  return 0
end

local active_count = redis.call('ZCARD', KEYS[2])
if active_count >= concurrency then
  return 1
end

return 0
"#;

const COMPLETE_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function add_base_marker_if_waiting(marker_key, waiting_key)
  if marker_key and marker_key ~= '' and redis.call('ZCARD', waiting_key) > 0 then
    redis.call('ZADD', marker_key, 0, '0')
  end
end

local function days_from_civil(year, month, day)
  if month <= 2 then
    year = year - 1
  end
  local era = math.floor(year / 400)
  local yoe = year - era * 400
  local shifted_month = month
  if month > 2 then
    shifted_month = month - 3
  else
    shifted_month = month + 9
  end
  local doy = math.floor((153 * shifted_month + 2) / 5) + day - 1
  local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
  return era * 146097 + doe - 719468
end

local function iso_to_millis(value)
  local year = tonumber(string.sub(value, 1, 4))
  local month = tonumber(string.sub(value, 6, 7))
  local day = tonumber(string.sub(value, 9, 10))
  local hour = tonumber(string.sub(value, 12, 13))
  local minute = tonumber(string.sub(value, 15, 16))
  local second = tonumber(string.sub(value, 18, 19))
  if not year or not month or not day or not hour or not minute or not second then
    return 0
  end

  local millis = 0
  if string.sub(value, 20, 20) == "." then
    local digits = string.match(string.sub(value, 21), "^(%d+)")
    if digits then
      digits = string.sub(digits .. "000", 1, 3)
      millis = tonumber(digits) or 0
    end
  end

  local days = days_from_civil(year, month, day)
  return (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis
end

local function civil_from_days(days)
  local z = days + 719468
  local era = math.floor(z / 146097)
  local doe = z - era * 146097
  local yoe = math.floor((doe - math.floor(doe / 1460) + math.floor(doe / 36524) - math.floor(doe / 146096)) / 365)
  local year = yoe + era * 400
  local doy = doe - (365 * yoe + math.floor(yoe / 4) - math.floor(yoe / 100))
  local mp = math.floor((5 * doy + 2) / 153)
  local day = doy - math.floor((153 * mp + 2) / 5) + 1
  local month = mp + 3
  if mp >= 10 then
    month = mp - 9
  end
  if month <= 2 then
    year = year + 1
  end
  return year, month, day
end

local function millis_to_iso(value)
  local day_millis = 86400000
  local days = math.floor(value / day_millis)
  local remainder = value - (days * day_millis)
  if remainder < 0 then
    days = days - 1
    remainder = remainder + day_millis
  end
  local hour = math.floor(remainder / 3600000)
  remainder = remainder - (hour * 3600000)
  local minute = math.floor(remainder / 60000)
  remainder = remainder - (minute * 60000)
  local second = math.floor(remainder / 1000)
  local millisecond = remainder - (second * 1000)
  local year, month, day = civil_from_days(days)
  return string.format("%04d-%02d-%02dT%02d:%02d:%02d.%03d+00:00", year, month, day, hour, minute, second, millisecond)
end

local function deduplication_id(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  local id = deduplication["id"]
  if not id or id == cjson.null or id == '' then
    return nil
  end
  return id
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function deduplication_ttl_millis(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  if deduplication["keep_last_if_active"] == true then
    return nil
  end
  return duration_millis(deduplication["ttl"])
end

local function release_deduplication_key(job, job_id, deduplication_prefix, deduplication_next_prefix)
  local id = deduplication_id(job)
  if id then
    local key = deduplication_prefix .. id
    if redis.call('GET', key) == job_id then
      redis.call('DEL', key)
      if deduplication_next_prefix and deduplication_next_prefix ~= '' then
        redis.call('DEL', deduplication_next_prefix .. id)
      end
    end
  end
end

local function release_deduplication_key_on_finalization(job, job_id, deduplication_prefix)
  local id = deduplication_id(job)
  if id then
    local key = deduplication_prefix .. id
    local pttl = redis.call('PTTL', key)
    if pttl == 0 then
      redis.call('DEL', key)
    elseif pttl == -1 and redis.call('GET', key) == job_id then
      redis.call('DEL', key)
    end
  end
end

local function set_deduplication_key(job, job_id, deduplication_prefix)
  local id = deduplication_id(job)
  if id then
    local ttl = deduplication_ttl_millis(job)
    if ttl then
      redis.call('SET', deduplication_prefix .. id, job_id, 'PX', ttl)
    else
      redis.call('SET', deduplication_prefix .. id, job_id)
    end
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function store_repeat_scheduler(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if not key then
    return
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end
  local scheduled_millis = iso_to_millis(job["scheduled_at"])

  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local function release_repeat_key(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if key then
    local owner_key = repeat_prefix .. key
    local owner_matches = redis.call('GET', owner_key) == job_id
    if owner_matches then
      redis.call('DEL', owner_key)
    end

    local scheduler_meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
    local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
    if scheduler_owner_id == job_id or (owner_matches and not scheduler_owner_id) then
      redis.call('ZREM', repeat_scheduler_key(repeat_prefix), key)
      redis.call('DEL', scheduler_meta_key)
    end
  end
end

local function retention_options(job, remove_field, retention_field, legacy_remove)
  if legacy_remove == true then
    return { count = 0 }
  end
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  if job["options"][remove_field] == true then
    return { count = 0 }
  end
  local retention = job["options"][retention_field]
  if retention and retention ~= cjson.null then
    return retention
  end
  return nil
end

local function retention_count(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local count = retention["count"]
  if count == nil or count == cjson.null then
    return nil
  end
  return tonumber(count)
end

local function retention_age_millis(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local age = duration_millis(retention["age"])
  if age and age > 0 then
    return age
  end
  return nil
end

local function retention_limit(retention)
  if not retention or retention == cjson.null then
    return 1000
  end
  local limit = tonumber(retention["limit"] or '1000') or 1000
  if limit <= 0 then
    return 1000
  end
  return limit
end

local function remove_dependency_keys(dependency_prefix, job_id)
  redis.call(
    'DEL',
    dependency_prefix .. job_id,
    dependency_prefix .. job_id .. ':processed',
    dependency_prefix .. job_id .. ':failed',
    dependency_prefix .. job_id .. ':unsuccessful'
  )
end

local function remove_finished_job(jobs_key, job_id, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  local raw = redis.call('HGET', jobs_key, job_id)
  if raw then
    local removed = cjson.decode(raw)
    release_repeat_key(removed, job_id, repeat_prefix)
  end
  redis.call('HDEL', jobs_key, job_id)
  remove_dependency_keys(dependency_prefix, job_id)
  redis.call('DEL', logs_prefix .. job_id)
end

local function remove_finished_jobs_by_max_age(timestamp, retention, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  local max_age = retention_age_millis(retention)
  if not max_age then
    return
  end
  local max_limit = retention_limit(retention)
  local start = tonumber(timestamp) - max_age
  local job_ids = redis.call('ZREVRANGEBYSCORE', target_set, start, '-inf', 'LIMIT', 0, max_limit)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(jobs_key, job_id, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  end
  if #job_ids > 0 then
    if #job_ids < max_limit then
      redis.call('ZREMRANGEBYSCORE', target_set, '-inf', start)
    else
      for _, job_id in ipairs(job_ids) do
        redis.call('ZREM', target_set, job_id)
      end
    end
  end
end

local function remove_finished_jobs_by_max_count(max_count, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  if not max_count or max_count <= 0 then
    return
  end
  local job_ids = redis.call('ZREVRANGE', target_set, max_count, -1)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(jobs_key, job_id, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  end
  redis.call('ZREMRANGEBYRANK', target_set, 0, -(max_count + 1))
end

local function apply_finished_retention(timestamp, retention, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  if not retention or retention == cjson.null then
    return
  end
  remove_finished_jobs_by_max_age(timestamp, retention, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  remove_finished_jobs_by_max_count(retention_count(retention), target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
end

local function set_repeat_key(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if key then
    redis.call('SET', repeat_prefix .. key, job_id)
    store_repeat_scheduler(job, job_id, repeat_prefix)
  end
end

local function repeat_keep_last_next_count(owner_job, next_job)
  local next_repeat_key = repeat_key(next_job)
  local owner_repeat_key = repeat_key(owner_job)
  if not next_repeat_key then
    if owner_repeat_key then
      return false
    end
    return nil
  end
  if not owner_repeat_key or owner_repeat_key ~= next_repeat_key then
    return false
  end

  local next_count = (tonumber(owner_job["repeat_count"] or '0') or 0) + 1
  local repeat_options = nil
  if owner_job["options"] and owner_job["options"] ~= cjson.null then
    repeat_options = owner_job["options"]["repeat"]
  end
  if repeat_options and repeat_options ~= cjson.null then
    local limit = tonumber(repeat_options["limit"] or '0') or 0
    if limit > 0 and next_count >= limit then
      return false
    end
  end
  return next_count
end

local function prepare_deduplicated_next_job(job, now_iso, now_millis)
  local now_value = tonumber(now_millis)
  local delay = nil
  if job["options"] and job["options"] ~= cjson.null then
    delay = duration_millis(job["options"]["delay"])
  end
  local scheduled_millis = now_value
  if delay then
    scheduled_millis = scheduled_millis + delay
  end
  job["created_at"] = now_iso
  job["scheduled_at"] = millis_to_iso(scheduled_millis)
  if scheduled_millis > now_value then
    job["state"] = "delayed"
  else
    job["state"] = "waiting"
  end
  job["attempts_made"] = 0
  job["stalled_count"] = 0
  job["processed_at"] = cjson.null
  job["finished_at"] = cjson.null
  job["worker_id"] = cjson.null
  job["lock_token"] = cjson.null
  job["lease_expires_at"] = cjson.null
  job["deferred_failure"] = cjson.null
  job["failed_reason"] = cjson.null
  job["return_value"] = cjson.null
  job["progress"] = cjson.null
  job["logs"] = {}

  local deduplication_ttl = deduplication_ttl_millis(job)
  if deduplication_ttl then
    job["deduplication_expires_at"] = millis_to_iso(now_value + deduplication_ttl)
  else
    job["deduplication_expires_at"] = cjson.null
  end
  return scheduled_millis
end

local function enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, job, scheduled_millis, waiting_score_bucket, marker_key)
  if job["state"] == "waiting" then
    local priority = tonumber(job["priority"] or '1000') or 1000
    enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job["id"], priority, waiting_score_bucket, marker_key)
  elseif job["state"] == "delayed" then
    redis.call('HSET', jobs_key, job["id"], cjson.encode(job))
    redis.call('ZADD', delayed_key, scheduled_millis, job["id"])
    refresh_delay_marker(marker_key, delayed_key)
  elseif job["state"] == "waiting_children" then
    redis.call('HSET', jobs_key, job["id"], cjson.encode(job))
    redis.call('ZADD', waiting_children_key, scheduled_millis, job["id"])
  else
    redis.call('HSET', jobs_key, job["id"], cjson.encode(job))
  end
end

local function enqueue_deduplicated_next_flow(next_key, next_payload, jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, deduplication_prefix, dependency_prefix, now_iso, now_millis, waiting_score_bucket, marker_key)
  local parent = next_payload["parent"]
  if not parent or parent == cjson.null then
    redis.call('DEL', next_key)
    return false
  end
  local children = next_payload["children"] or {}
  if redis.call('HEXISTS', jobs_key, parent["id"]) == 1 then
    redis.call('DEL', next_key)
    return false
  end
  for _, child in ipairs(children) do
    if redis.call('HEXISTS', jobs_key, child["id"]) == 1 then
      redis.call('DEL', next_key)
      return false
    end
  end

  local child_ids = {}
  for _, child in ipairs(children) do
    child_ids[#child_ids + 1] = child["id"]
  end

  local parent_scheduled_millis = prepare_deduplicated_next_job(parent, now_iso, now_millis)
  parent["parent_id"] = cjson.null
  parent["child_ids"] = child_ids
  if #children > 0 then
    parent["state"] = "waiting_children"
  end
  set_deduplication_key(parent, parent["id"], deduplication_prefix)
  enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, parent, parent_scheduled_millis, waiting_score_bucket, marker_key)

  if #children > 0 then
    local dependency_key = dependency_prefix .. parent["id"]
    redis.call('DEL', dependency_key)
    for _, child in ipairs(children) do
      local child_scheduled_millis = prepare_deduplicated_next_job(child, now_iso, now_millis)
      child["parent_id"] = parent["id"]
      child["child_ids"] = {}
      redis.call('SADD', dependency_key, child["id"])
      set_deduplication_key(child, child["id"], deduplication_prefix)
      enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, child, child_scheduled_millis, waiting_score_bucket, marker_key)
    end
  end

  redis.call('DEL', next_key)
  return true
end

local function enqueue_deduplicated_next(owner_job, jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, deduplication_prefix, deduplication_next_prefix, repeat_prefix, dependency_prefix, now_iso, now_millis, waiting_score_bucket, marker_key)
  local id = deduplication_id(owner_job)
  if not id then
    return false
  end
  local next_key = deduplication_next_prefix .. id
  local next_raw = redis.call('GET', next_key)
  if not next_raw then
    return false
  end
  local next_job = cjson.decode(next_raw)
  if next_job["kind"] == "flow" then
    return enqueue_deduplicated_next_flow(next_key, next_job, jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, deduplication_prefix, dependency_prefix, now_iso, now_millis, waiting_score_bucket, marker_key)
  end
  if redis.call('HEXISTS', jobs_key, next_job["id"]) == 1 then
    redis.call('DEL', next_key)
    return false
  end

  local repeat_next_count = repeat_keep_last_next_count(owner_job, next_job)
  if repeat_next_count == false then
    redis.call('DEL', next_key)
    return false
  end

  local scheduled_millis = prepare_deduplicated_next_job(next_job, now_iso, now_millis)
  if repeat_next_count then
    next_job["repeat_key"] = owner_job["repeat_key"]
    next_job["repeat_count"] = repeat_next_count
  end

  set_deduplication_key(next_job, next_job["id"], deduplication_prefix)
  if repeat_next_count then
    set_repeat_key(next_job, next_job["id"], repeat_prefix)
  end
  enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, next_job, scheduled_millis, waiting_score_bucket, marker_key)
  local parent_id = next_job["parent_id"]
  if parent_id and parent_id ~= cjson.null then
    local parent_raw = redis.call('HGET', jobs_key, parent_id)
    if parent_raw then
      local parent = cjson.decode(parent_raw)
      local child_ids = parent["child_ids"] or {}
      local found_child = false
      for _, child_id in ipairs(child_ids) do
        if child_id == next_job["id"] then
          found_child = true
          break
        end
      end
      if not found_child then
        child_ids[#child_ids + 1] = next_job["id"]
        parent["child_ids"] = child_ids
        redis.call('HSET', jobs_key, parent_id, cjson.encode(parent))
      end
      redis.call('SADD', dependency_prefix .. parent_id, next_job["id"])
    end
  end
  redis.call('DEL', next_key)
  return true
end

local function collect_metrics(meta_key, data_key, max_points, timestamp)
  local max_data_points = tonumber(max_points)
  if not max_data_points or max_data_points <= 0 then
    return
  end

  local count = redis.call('HINCRBY', meta_key, 'count', 1) - 1
  local prev_ts = redis.call('HGET', meta_key, 'prevTS')
  if not prev_ts then
    redis.call('HSET', meta_key, 'prevTS', timestamp, 'prevCount', 0)
    return
  end

  local n = math.min(math.floor(tonumber(timestamp) / 60000) - math.floor(tonumber(prev_ts) / 60000), max_data_points)
  if n > 0 then
    local prev_count = tonumber(redis.call('HGET', meta_key, 'prevCount') or '0') or 0
    local delta = count - prev_count
    if n > 1 then
      local points = {}
      points[1] = delta
      for i = 2, n do
        points[i] = 0
      end
      for i = 1, #points, 7000 do
        local last = math.min(i + 6999, #points)
        redis.call('LPUSH', data_key, unpack(points, i, last))
      end
    else
      redis.call('LPUSH', data_key, delta)
    end
    redis.call('LTRIM', data_key, 0, max_data_points - 1)
    redis.call('HSET', meta_key, 'prevCount', count, 'prevTS', timestamp)
  end
end

local function emit_parent_waiting_children_transition_event(events_key, max_events, event, job_id, failed_reason)
  if event == 'failed' then
    redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'failed', 'jobId', job_id, 'failedReason', failed_reason, 'prev', 'waiting-children')
  else
    redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', event, 'jobId', job_id, 'prev', 'waiting-children')
  end
end

local function emit_drained_event_if_needed(waiting_key, active_key, events_key, max_events)
  if redis.call('ZCARD', waiting_key) == 0 and redis.call('ZCARD', active_key) == 0 then
    redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'drained')
  end
end

local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] ~= "active" then
  return {'state', job["state"] or ''}
end

local lock_token = redis.call('GET', KEYS[4])
if not lock_token then
  return {'lock_missing'}
end
if lock_token ~= ARGV[2] then
  return {'lock_mismatch'}
end

if redis.call('SCARD', ARGV[13] .. ARGV[1]) > 0 then
  return {'pending_dependencies'}
end

if redis.call('ZCARD', ARGV[13] .. ARGV[1] .. ':unsuccessful') > 0 then
  return {'failed_dependencies'}
end

for _, child_id in ipairs(job["child_ids"] or {}) do
  local child_raw = redis.call('HGET', KEYS[1], child_id)
  if child_raw then
    local child = cjson.decode(child_raw)
    if child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true)) then
      return {'failed_dependencies'}
    end
  end
end

redis.call('DEL', KEYS[4])
redis.call('SREM', KEYS[11], ARGV[1])
redis.call('ZREM', KEYS[2], ARGV[1])

job["state"] = "completed"
job["finished_at"] = ARGV[3]
job["worker_id"] = cjson.null
job["lease_expires_at"] = cjson.null
job["deferred_failure"] = cjson.null
job["return_value"] = cjson.decode(ARGV[4])

release_deduplication_key_on_finalization(job, ARGV[1], ARGV[14])
local enqueued_deduplicated_next = enqueue_deduplicated_next(job, KEYS[1], KEYS[5], KEYS[6], KEYS[9], KEYS[7], ARGV[14], ARGV[16], ARGV[15], ARGV[13], ARGV[3], ARGV[5], ARGV[7], KEYS[#KEYS])

local updated = cjson.encode(job)
local completion_retention = retention_options(job, 'remove_on_complete', 'completion_retention', ARGV[6] == '1')
local completion_max_count = retention_count(completion_retention)
if completion_max_count == 0 then
  remove_finished_job(KEYS[1], ARGV[1], ARGV[13], ARGV[14], ARGV[15], ARGV[17])
else
  redis.call('HSET', KEYS[1], ARGV[1], updated)
  redis.call('ZADD', KEYS[3], ARGV[5], ARGV[1])
  apply_finished_retention(ARGV[5], completion_retention, KEYS[3], KEYS[1], ARGV[13], ARGV[14], ARGV[15], ARGV[17])
end
collect_metrics(KEYS[12], KEYS[13], ARGV[19], ARGV[5])

local parent_id = job["parent_id"]
if parent_id and parent_id ~= cjson.null then
  local parent_raw = redis.call('HGET', KEYS[1], parent_id)
  if parent_raw then
	    local parent = cjson.decode(parent_raw)
	    local dependency_key = ARGV[13] .. parent_id
	    local had_dependency_set = redis.call('EXISTS', dependency_key) == 1
	    local removed_dependency = 0
	    if had_dependency_set then
	      removed_dependency = redis.call('SREM', dependency_key, ARGV[1])
	      if removed_dependency == 1 then
	        redis.call('HSET', dependency_key .. ':processed', ARGV[1], ARGV[4])
	      end
	    end
    if parent["state"] == "waiting_children" then
      local all_done = true
      local failed_child_id = nil
      local failed_reason = nil

      if had_dependency_set then
        if redis.call('SCARD', dependency_key) > 0 then
          all_done = false
        end
      else
        for _, child_id in ipairs(parent["child_ids"] or {}) do
          local child_raw = nil
          if child_id == ARGV[1] then
            child_raw = updated
          else
            child_raw = redis.call('HGET', KEYS[1], child_id)
          end
          if child_raw then
            local child = cjson.decode(child_raw)
            if child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true)) then
              failed_child_id = child_id
              failed_reason = child["failed_reason"] or "unknown error"
              break
            elseif child["state"] ~= "completed" and child["state"] ~= "failed" then
              all_done = false
              break
            end
          end
        end
      end

      if failed_child_id then
        redis.call('DEL', dependency_key)
        redis.call('ZREM', KEYS[6], parent_id)
        parent["state"] = "failed"
        parent["finished_at"] = ARGV[3]
        parent["worker_id"] = cjson.null
        parent["lock_token"] = cjson.null
        parent["lease_expires_at"] = cjson.null
        parent["deferred_failure"] = cjson.null
        parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
        release_deduplication_key_on_finalization(parent, parent_id, ARGV[14])
        release_repeat_key(parent, parent_id, ARGV[15])
        local parent_failure_retention = retention_options(parent, 'remove_on_fail', 'failure_retention', false)
        local parent_failure_max_count = retention_count(parent_failure_retention)
        if parent_failure_max_count == 0 then
          remove_finished_job(KEYS[1], parent_id, ARGV[13], ARGV[14], ARGV[15], ARGV[17])
        else
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[8], ARGV[5], parent_id)
          apply_finished_retention(ARGV[5], parent_failure_retention, KEYS[8], KEYS[1], ARGV[13], ARGV[14], ARGV[15], ARGV[17])
        end
        collect_metrics(KEYS[14], KEYS[15], ARGV[19], ARGV[5])
        emit_parent_waiting_children_transition_event(KEYS[10], ARGV[18], 'failed', parent_id, parent["failed_reason"])
      elseif all_done then
        redis.call('DEL', dependency_key)
        redis.call('ZREM', KEYS[6], parent_id)
        parent["processed_at"] = cjson.null
        parent["finished_at"] = cjson.null
        parent["worker_id"] = cjson.null
        parent["lock_token"] = cjson.null
        parent["lease_expires_at"] = cjson.null
        parent["deferred_failure"] = cjson.null
        parent["failed_reason"] = cjson.null

        local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
        if parent_scheduled_millis <= tonumber(ARGV[5]) then
          parent["state"] = "waiting"
          local priority = tonumber(parent["priority"] or '1000') or 1000
          enqueue_waiting_job(KEYS[1], KEYS[5], KEYS[7], parent, parent_id, priority, ARGV[7], KEYS[#KEYS])
        else
          parent["state"] = "delayed"
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[9], parent_scheduled_millis, parent_id)
          refresh_delay_marker(KEYS[#KEYS], KEYS[9])
        end
        emit_parent_waiting_children_transition_event(KEYS[10], ARGV[18], parent["state"], parent_id)
      end
    end
  end
end

local repeat_next_id = ARGV[8]
if not enqueued_deduplicated_next and repeat_next_id and repeat_next_id ~= '' then
  local inserted = redis.call('HSETNX', KEYS[1], repeat_next_id, ARGV[9])
  if inserted == 1 then
    local repeat_next = cjson.decode(ARGV[9])
    set_deduplication_key(repeat_next, repeat_next_id, ARGV[14])
    set_repeat_key(repeat_next, repeat_next_id, ARGV[15])
    local repeat_next_state = ARGV[10]
    if repeat_next_state == 'waiting' then
      local repeat_priority = tonumber(ARGV[12])
      enqueue_waiting_job(KEYS[1], KEYS[5], KEYS[7], repeat_next, repeat_next_id, repeat_priority, ARGV[7], KEYS[#KEYS])
    elseif repeat_next_state == 'delayed' then
      redis.call('ZADD', KEYS[9], ARGV[11], repeat_next_id)
      refresh_delay_marker(KEYS[#KEYS], KEYS[9])
    elseif repeat_next_state == 'waiting_children' then
      redis.call('ZADD', KEYS[6], ARGV[11], repeat_next_id)
    end
  end
end
release_repeat_key(job, ARGV[1], ARGV[15])

add_base_marker_if_waiting(KEYS[#KEYS], KEYS[5])
redis.call('XADD', KEYS[10], 'MAXLEN', '~', ARGV[18], '*', 'event', 'completed', 'jobId', ARGV[1], 'returnvalue', ARGV[4], 'prev', 'active')
emit_drained_event_if_needed(KEYS[5], KEYS[2], KEYS[10], ARGV[18])

return {'ok', updated}
"#;

const FAIL_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function add_base_marker_if_waiting(marker_key, waiting_key)
  if marker_key and marker_key ~= '' and redis.call('ZCARD', waiting_key) > 0 then
    redis.call('ZADD', marker_key, 0, '0')
  end
end

local function days_from_civil(year, month, day)
  if month <= 2 then
    year = year - 1
  end
  local era = math.floor(year / 400)
  local yoe = year - era * 400
  local shifted_month = month
  if month > 2 then
    shifted_month = month - 3
  else
    shifted_month = month + 9
  end
  local doy = math.floor((153 * shifted_month + 2) / 5) + day - 1
  local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
  return era * 146097 + doe - 719468
end

local function iso_to_millis(value)
  local year = tonumber(string.sub(value, 1, 4))
  local month = tonumber(string.sub(value, 6, 7))
  local day = tonumber(string.sub(value, 9, 10))
  local hour = tonumber(string.sub(value, 12, 13))
  local minute = tonumber(string.sub(value, 15, 16))
  local second = tonumber(string.sub(value, 18, 19))
  if not year or not month or not day or not hour or not minute or not second then
    return 0
  end

  local millis = 0
  if string.sub(value, 20, 20) == "." then
    local digits = string.match(string.sub(value, 21), "^(%d+)")
    if digits then
      digits = string.sub(digits .. "000", 1, 3)
      millis = tonumber(digits) or 0
    end
  end

  local days = days_from_civil(year, month, day)
  return (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis
end

local function deduplication_id(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  local id = deduplication["id"]
  if not id or id == cjson.null or id == '' then
    return nil
  end
  return id
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function civil_from_days(days)
  local z = days + 719468
  local era = math.floor(z / 146097)
  local doe = z - era * 146097
  local yoe = math.floor((doe - math.floor(doe / 1460) + math.floor(doe / 36524) - math.floor(doe / 146096)) / 365)
  local year = yoe + era * 400
  local doy = doe - (365 * yoe + math.floor(yoe / 4) - math.floor(yoe / 100))
  local mp = math.floor((5 * doy + 2) / 153)
  local day = doy - math.floor((153 * mp + 2) / 5) + 1
  local month = mp + 3
  if mp >= 10 then
    month = mp - 9
  end
  if month <= 2 then
    year = year + 1
  end
  return year, month, day
end

local function millis_to_iso(value)
  local day_millis = 86400000
  local days = math.floor(value / day_millis)
  local remainder = value - (days * day_millis)
  if remainder < 0 then
    days = days - 1
    remainder = remainder + day_millis
  end
  local hour = math.floor(remainder / 3600000)
  remainder = remainder - (hour * 3600000)
  local minute = math.floor(remainder / 60000)
  remainder = remainder - (minute * 60000)
  local second = math.floor(remainder / 1000)
  local millisecond = remainder - (second * 1000)
  local year, month, day = civil_from_days(days)
  return string.format("%04d-%02d-%02dT%02d:%02d:%02d.%03d+00:00", year, month, day, hour, minute, second, millisecond)
end

local function deduplication_ttl_millis(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  if deduplication["keep_last_if_active"] == true then
    return nil
  end
  return duration_millis(deduplication["ttl"])
end

local function release_deduplication_key(job, job_id, deduplication_prefix, deduplication_next_prefix)
  local id = deduplication_id(job)
  if id then
    local key = deduplication_prefix .. id
    if redis.call('GET', key) == job_id then
      redis.call('DEL', key)
      if deduplication_next_prefix and deduplication_next_prefix ~= '' then
        redis.call('DEL', deduplication_next_prefix .. id)
      end
    end
  end
end

local function set_deduplication_key(job, job_id, deduplication_prefix)
  local id = deduplication_id(job)
  if id then
    local ttl = deduplication_ttl_millis(job)
    if ttl then
      redis.call('SET', deduplication_prefix .. id, job_id, 'PX', ttl)
    else
      redis.call('SET', deduplication_prefix .. id, job_id)
    end
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function store_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = repeat_key(job)
  if not key then
    return
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local function release_repeat_key(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if key then
    local owner_key = repeat_prefix .. key
    local owner_matches = redis.call('GET', owner_key) == job_id
    if owner_matches then
      redis.call('DEL', owner_key)
    end

    local scheduler_meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
    local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
    if scheduler_owner_id == job_id or (owner_matches and not scheduler_owner_id) then
      redis.call('ZREM', repeat_scheduler_key(repeat_prefix), key)
      redis.call('DEL', scheduler_meta_key)
    end
  end
end

local function retention_options(job, remove_field, retention_field, legacy_remove)
  if legacy_remove == true then
    return { count = 0 }
  end
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  if job["options"][remove_field] == true then
    return { count = 0 }
  end
  local retention = job["options"][retention_field]
  if retention and retention ~= cjson.null then
    return retention
  end
  return nil
end

local function retention_count(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local count = retention["count"]
  if count == nil or count == cjson.null then
    return nil
  end
  return tonumber(count)
end

local function retention_age_millis(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local age = duration_millis(retention["age"])
  if age and age > 0 then
    return age
  end
  return nil
end

local function retention_limit(retention)
  if not retention or retention == cjson.null then
    return 1000
  end
  local limit = tonumber(retention["limit"] or '1000') or 1000
  if limit <= 0 then
    return 1000
  end
  return limit
end

local function remove_dependency_keys(dependency_prefix, job_id)
  redis.call(
    'DEL',
    dependency_prefix .. job_id,
    dependency_prefix .. job_id .. ':processed',
    dependency_prefix .. job_id .. ':failed',
    dependency_prefix .. job_id .. ':unsuccessful'
  )
end

local function remove_finished_job(jobs_key, job_id, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  local raw = redis.call('HGET', jobs_key, job_id)
  if raw then
    local removed = cjson.decode(raw)
    release_repeat_key(removed, job_id, repeat_prefix)
  end
  redis.call('HDEL', jobs_key, job_id)
  remove_dependency_keys(dependency_prefix, job_id)
  redis.call('DEL', logs_prefix .. job_id)
end

local function remove_finished_jobs_by_max_age(timestamp, retention, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  local max_age = retention_age_millis(retention)
  if not max_age then
    return
  end
  local max_limit = retention_limit(retention)
  local start = tonumber(timestamp) - max_age
  local job_ids = redis.call('ZREVRANGEBYSCORE', target_set, start, '-inf', 'LIMIT', 0, max_limit)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(jobs_key, job_id, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  end
  if #job_ids > 0 then
    if #job_ids < max_limit then
      redis.call('ZREMRANGEBYSCORE', target_set, '-inf', start)
    else
      for _, job_id in ipairs(job_ids) do
        redis.call('ZREM', target_set, job_id)
      end
    end
  end
end

local function remove_finished_jobs_by_max_count(max_count, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  if not max_count or max_count <= 0 then
    return
  end
  local job_ids = redis.call('ZREVRANGE', target_set, max_count, -1)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(jobs_key, job_id, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  end
  redis.call('ZREMRANGEBYRANK', target_set, 0, -(max_count + 1))
end

local function apply_finished_retention(timestamp, retention, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  if not retention or retention == cjson.null then
    return
  end
  remove_finished_jobs_by_max_age(timestamp, retention, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  remove_finished_jobs_by_max_count(retention_count(retention), target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
end

local function set_repeat_key(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if key then
    redis.call('SET', repeat_prefix .. key, job_id)
  end
end

local function repeat_keep_last_next_count(owner_job, next_job)
  local next_repeat_key = repeat_key(next_job)
  local owner_repeat_key = repeat_key(owner_job)
  if not next_repeat_key then
    if owner_repeat_key then
      return false
    end
    return nil
  end
  if not owner_repeat_key or owner_repeat_key ~= next_repeat_key then
    return false
  end

  local next_count = (tonumber(owner_job["repeat_count"] or '0') or 0) + 1
  local repeat_options = nil
  if owner_job["options"] and owner_job["options"] ~= cjson.null then
    repeat_options = owner_job["options"]["repeat"]
  end
  if repeat_options and repeat_options ~= cjson.null then
    local limit = tonumber(repeat_options["limit"] or '0') or 0
    if limit > 0 and next_count >= limit then
      return false
    end
  end
  return next_count
end

local function prepare_deduplicated_next_job(job, now_iso, now_millis)
  local now_value = tonumber(now_millis)
  local delay = nil
  if job["options"] and job["options"] ~= cjson.null then
    delay = duration_millis(job["options"]["delay"])
  end
  local scheduled_millis = now_value
  if delay then
    scheduled_millis = scheduled_millis + delay
  end
  job["created_at"] = now_iso
  job["scheduled_at"] = millis_to_iso(scheduled_millis)
  if scheduled_millis > now_value then
    job["state"] = "delayed"
  else
    job["state"] = "waiting"
  end
  job["attempts_made"] = 0
  job["stalled_count"] = 0
  job["processed_at"] = cjson.null
  job["finished_at"] = cjson.null
  job["worker_id"] = cjson.null
  job["lock_token"] = cjson.null
  job["lease_expires_at"] = cjson.null
  job["deferred_failure"] = cjson.null
  job["failed_reason"] = cjson.null
  job["return_value"] = cjson.null
  job["progress"] = cjson.null
  job["logs"] = {}

  local deduplication_ttl = deduplication_ttl_millis(job)
  if deduplication_ttl then
    job["deduplication_expires_at"] = millis_to_iso(now_value + deduplication_ttl)
  else
    job["deduplication_expires_at"] = cjson.null
  end
  return scheduled_millis
end

local function enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, job, scheduled_millis, waiting_score_bucket, marker_key)
  if job["state"] == "waiting" then
    local priority = tonumber(job["priority"] or '1000') or 1000
    enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job["id"], priority, waiting_score_bucket, marker_key)
  elseif job["state"] == "delayed" then
    redis.call('HSET', jobs_key, job["id"], cjson.encode(job))
    redis.call('ZADD', delayed_key, scheduled_millis, job["id"])
    refresh_delay_marker(marker_key, delayed_key)
  elseif job["state"] == "waiting_children" then
    redis.call('HSET', jobs_key, job["id"], cjson.encode(job))
    redis.call('ZADD', waiting_children_key, scheduled_millis, job["id"])
  else
    redis.call('HSET', jobs_key, job["id"], cjson.encode(job))
  end
end

local function enqueue_deduplicated_next_flow(next_key, next_payload, jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, deduplication_prefix, dependency_prefix, now_iso, now_millis, waiting_score_bucket, marker_key)
  local parent = next_payload["parent"]
  if not parent or parent == cjson.null then
    redis.call('DEL', next_key)
    return false
  end
  local children = next_payload["children"] or {}
  if redis.call('HEXISTS', jobs_key, parent["id"]) == 1 then
    redis.call('DEL', next_key)
    return false
  end
  for _, child in ipairs(children) do
    if redis.call('HEXISTS', jobs_key, child["id"]) == 1 then
      redis.call('DEL', next_key)
      return false
    end
  end

  local child_ids = {}
  for _, child in ipairs(children) do
    child_ids[#child_ids + 1] = child["id"]
  end

  local parent_scheduled_millis = prepare_deduplicated_next_job(parent, now_iso, now_millis)
  parent["parent_id"] = cjson.null
  parent["child_ids"] = child_ids
  if #children > 0 then
    parent["state"] = "waiting_children"
  end
  set_deduplication_key(parent, parent["id"], deduplication_prefix)
  enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, parent, parent_scheduled_millis, waiting_score_bucket, marker_key)

  if #children > 0 then
    local dependency_key = dependency_prefix .. parent["id"]
    redis.call('DEL', dependency_key)
    for _, child in ipairs(children) do
      local child_scheduled_millis = prepare_deduplicated_next_job(child, now_iso, now_millis)
      child["parent_id"] = parent["id"]
      child["child_ids"] = {}
      redis.call('SADD', dependency_key, child["id"])
      set_deduplication_key(child, child["id"], deduplication_prefix)
      enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, child, child_scheduled_millis, waiting_score_bucket, marker_key)
    end
  end

  redis.call('DEL', next_key)
  return true
end

local function enqueue_deduplicated_next(owner_job, jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, deduplication_prefix, deduplication_next_prefix, repeat_prefix, dependency_prefix, now_iso, now_millis, waiting_score_bucket, marker_key)
  local id = deduplication_id(owner_job)
  if not id then
    return false
  end
  local next_key = deduplication_next_prefix .. id
  local next_raw = redis.call('GET', next_key)
  if not next_raw then
    return false
  end
  local next_job = cjson.decode(next_raw)
  if next_job["kind"] == "flow" then
    return enqueue_deduplicated_next_flow(next_key, next_job, jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, deduplication_prefix, dependency_prefix, now_iso, now_millis, waiting_score_bucket, marker_key)
  end
  if redis.call('HEXISTS', jobs_key, next_job["id"]) == 1 then
    redis.call('DEL', next_key)
    return false
  end

  local repeat_next_count = repeat_keep_last_next_count(owner_job, next_job)
  if repeat_next_count == false then
    redis.call('DEL', next_key)
    return false
  end

  local scheduled_millis = prepare_deduplicated_next_job(next_job, now_iso, now_millis)
  if repeat_next_count then
    next_job["repeat_key"] = owner_job["repeat_key"]
    next_job["repeat_count"] = repeat_next_count
  end

  set_deduplication_key(next_job, next_job["id"], deduplication_prefix)
  if repeat_next_count then
    set_repeat_key(next_job, next_job["id"], repeat_prefix)
    store_repeat_scheduler(next_job, next_job["id"], repeat_prefix, scheduled_millis)
  end
  enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, next_job, scheduled_millis, waiting_score_bucket, marker_key)
  local parent_id = next_job["parent_id"]
  if parent_id and parent_id ~= cjson.null then
    local parent_raw = redis.call('HGET', jobs_key, parent_id)
    if parent_raw then
      local parent = cjson.decode(parent_raw)
      local child_ids = parent["child_ids"] or {}
      local found_child = false
      for _, child_id in ipairs(child_ids) do
        if child_id == next_job["id"] then
          found_child = true
          break
        end
      end
      if not found_child then
        child_ids[#child_ids + 1] = next_job["id"]
        parent["child_ids"] = child_ids
        redis.call('HSET', jobs_key, parent_id, cjson.encode(parent))
      end
      redis.call('SADD', dependency_prefix .. parent_id, next_job["id"])
    end
  end
  redis.call('DEL', next_key)
  return true
end

local function collect_metrics(meta_key, data_key, max_points, timestamp)
  local max_data_points = tonumber(max_points)
  if not max_data_points or max_data_points <= 0 then
    return
  end

  local count = redis.call('HINCRBY', meta_key, 'count', 1) - 1
  local prev_ts = redis.call('HGET', meta_key, 'prevTS')
  if not prev_ts then
    redis.call('HSET', meta_key, 'prevTS', timestamp, 'prevCount', 0)
    return
  end

  local n = math.min(math.floor(tonumber(timestamp) / 60000) - math.floor(tonumber(prev_ts) / 60000), max_data_points)
  if n > 0 then
    local prev_count = tonumber(redis.call('HGET', meta_key, 'prevCount') or '0') or 0
    local delta = count - prev_count
    if n > 1 then
      local points = {}
      points[1] = delta
      for i = 2, n do
        points[i] = 0
      end
      for i = 1, #points, 7000 do
        local last = math.min(i + 6999, #points)
        redis.call('LPUSH', data_key, unpack(points, i, last))
      end
    else
      redis.call('LPUSH', data_key, delta)
    end
    redis.call('LTRIM', data_key, 0, max_data_points - 1)
    redis.call('HSET', meta_key, 'prevCount', count, 'prevTS', timestamp)
  end
end

local function emit_parent_waiting_children_transition_event(events_key, max_events, event, job_id, failed_reason)
  if event == 'failed' then
    redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'failed', 'jobId', job_id, 'failedReason', failed_reason, 'prev', 'waiting-children')
  else
    redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', event, 'jobId', job_id, 'prev', 'waiting-children')
  end
end

local function emit_drained_event_if_needed(waiting_key, active_key, events_key, max_events)
  if redis.call('ZCARD', waiting_key) == 0 and redis.call('ZCARD', active_key) == 0 then
    redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'drained')
  end
end

local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] ~= "active" then
  return {'state', job["state"] or ''}
end

local lock_token = redis.call('GET', KEYS[5])
if not lock_token then
  return {'lock_missing'}
end
if lock_token ~= ARGV[2] then
  return {'lock_mismatch'}
end

redis.call('DEL', KEYS[5])
redis.call('SREM', KEYS[10], ARGV[1])
redis.call('ZREM', KEYS[2], ARGV[1])

job["worker_id"] = cjson.null
job["lease_expires_at"] = cjson.null
job["deferred_failure"] = cjson.null
job["failed_reason"] = ARGV[4]

if ARGV[5] == '1' then
  job["state"] = "delayed"
  job["scheduled_at"] = ARGV[6]
  job["finished_at"] = cjson.null
  local updated = cjson.encode(job)
  redis.call('HSET', KEYS[1], ARGV[1], updated)
  redis.call('ZADD', KEYS[3], ARGV[7], ARGV[1])
  refresh_delay_marker(KEYS[#KEYS], KEYS[3])
  add_base_marker_if_waiting(KEYS[#KEYS], KEYS[7])
  redis.call('XADD', KEYS[9], 'MAXLEN', '~', ARGV[16], '*', 'event', 'delayed', 'jobId', ARGV[1], 'failedReason', ARGV[4], 'prev', 'active')
  return {'ok', updated}
end

job["state"] = "failed"
job["finished_at"] = ARGV[3]

local function release_deduplication_key_on_finalization(job, job_id, deduplication_prefix)
  local id = deduplication_id(job)
  if id then
    local key = deduplication_prefix .. id
    local pttl = redis.call('PTTL', key)
    if pttl == 0 then
      redis.call('DEL', key)
    elseif pttl == -1 and redis.call('GET', key) == job_id then
      redis.call('DEL', key)
    end
  end
end

release_deduplication_key_on_finalization(job, ARGV[1], ARGV[11])
release_repeat_key(job, ARGV[1], ARGV[12])
enqueue_deduplicated_next(job, KEYS[1], KEYS[7], KEYS[6], KEYS[3], KEYS[8], ARGV[11], ARGV[14], ARGV[12], ARGV[10], ARGV[3], ARGV[8], ARGV[13], KEYS[#KEYS])
local updated = cjson.encode(job)
local failure_retention = retention_options(job, 'remove_on_fail', 'failure_retention', ARGV[9] == '1')
local failure_max_count = retention_count(failure_retention)
if failure_max_count == 0 then
  remove_finished_job(KEYS[1], ARGV[1], ARGV[10], ARGV[11], ARGV[12], ARGV[15])
else
  redis.call('HSET', KEYS[1], ARGV[1], updated)
  redis.call('ZADD', KEYS[4], ARGV[8], ARGV[1])
  apply_finished_retention(ARGV[8], failure_retention, KEYS[4], KEYS[1], ARGV[10], ARGV[11], ARGV[12], ARGV[15])
end
collect_metrics(KEYS[11], KEYS[12], ARGV[17], ARGV[8])

local parent_id = job["parent_id"]
if parent_id and parent_id ~= cjson.null then
  local parent_raw = redis.call('HGET', KEYS[1], parent_id)
  if parent_raw then
	    local parent = cjson.decode(parent_raw)
      local dependency_key = ARGV[10] .. parent_id
      local releases_dependency_after_parent_release = job["options"] and job["options"] ~= cjson.null and (job["options"]["ignore_dependency_on_failure"] == true or job["options"]["remove_dependency_on_failure"] == true or job["options"]["continue_parent_on_failure"] == true or job["options"]["fail_parent_on_failure"] == true)
      if releases_dependency_after_parent_release and redis.call('EXISTS', dependency_key) == 1 then
        local removed_dependency = redis.call('SREM', dependency_key, ARGV[1]) == 1
        if removed_dependency then
          if job["options"]["fail_parent_on_failure"] == true then
            redis.call('ZADD', dependency_key .. ':unsuccessful', ARGV[8], ARGV[1])
          elseif job["options"]["ignore_dependency_on_failure"] == true or job["options"]["continue_parent_on_failure"] == true then
            redis.call('HSET', dependency_key .. ':failed', ARGV[1], ARGV[4])
          end
        end
      end
	    if parent["state"] == "waiting_children" then
	      local ignore_parent_failure = job["options"] and job["options"] ~= cjson.null and job["options"]["ignore_dependency_on_failure"] == true
	      local continue_parent_failure = job["options"] and job["options"] ~= cjson.null and job["options"]["continue_parent_on_failure"] == true
	      local fail_parent_failure = job["options"] and job["options"] ~= cjson.null and job["options"]["fail_parent_on_failure"] == true
	      local dependency_failure_releases_parent = job["options"] and job["options"] ~= cjson.null and (job["options"]["ignore_dependency_on_failure"] == true or job["options"]["remove_dependency_on_failure"] == true or job["options"]["continue_parent_on_failure"] == true or job["options"]["fail_parent_on_failure"] == true)
      local dependency_key = ARGV[10] .. parent_id
      local all_done = true
      local continue_parent = false
      local fail_parent = false
      local failed_child_id = nil
      local failed_reason = nil

      if dependency_failure_releases_parent then
        if fail_parent_failure then
          if redis.call('EXISTS', dependency_key) == 1 then
            redis.call('SREM', dependency_key, ARGV[1])
          end
          redis.call('ZADD', dependency_key .. ':unsuccessful', ARGV[8], ARGV[1])
          fail_parent = true
	        elseif continue_parent_failure then
	          if redis.call('EXISTS', dependency_key) == 1 then
	            redis.call('SREM', dependency_key, ARGV[1])
	          end
	          redis.call('HSET', dependency_key .. ':failed', ARGV[1], ARGV[4])
	          continue_parent = true
	        else
	          local had_dependency_set = redis.call('EXISTS', dependency_key) == 1
	          if had_dependency_set then
	            redis.call('SREM', dependency_key, ARGV[1])
	            if ignore_parent_failure then
	              redis.call('HSET', dependency_key .. ':failed', ARGV[1], ARGV[4])
	            end
	            if redis.call('SCARD', dependency_key) > 0 then
	              all_done = false
	            end
          else
            for _, child_id in ipairs(parent["child_ids"] or {}) do
              local child_raw = nil
              if child_id == ARGV[1] then
                child_raw = updated
              else
                child_raw = redis.call('HGET', KEYS[1], child_id)
              end
              if child_raw then
                local child = cjson.decode(child_raw)
                if child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true)) then
                  failed_child_id = child_id
                  failed_reason = child["failed_reason"] or "unknown error"
                  break
                elseif child["state"] ~= "completed" and child["state"] ~= "failed" then
                  all_done = false
                  break
                end
              end
            end
          end
        end
      else
        failed_child_id = ARGV[1]
        failed_reason = ARGV[4]
      end

      if failed_child_id then
        redis.call('DEL', dependency_key)
        redis.call('ZREM', KEYS[6], parent_id)
        parent["state"] = "failed"
        parent["finished_at"] = ARGV[3]
        parent["worker_id"] = cjson.null
        parent["lock_token"] = cjson.null
        parent["lease_expires_at"] = cjson.null
        parent["deferred_failure"] = cjson.null
        parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
        release_deduplication_key_on_finalization(parent, parent_id, ARGV[11])
        release_repeat_key(parent, parent_id, ARGV[12])
        local parent_failure_retention = retention_options(parent, 'remove_on_fail', 'failure_retention', false)
        local parent_failure_max_count = retention_count(parent_failure_retention)
        if parent_failure_max_count == 0 then
          remove_finished_job(KEYS[1], parent_id, ARGV[10], ARGV[11], ARGV[12], ARGV[15])
        else
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[4], ARGV[8], parent_id)
          apply_finished_retention(ARGV[8], parent_failure_retention, KEYS[4], KEYS[1], ARGV[10], ARGV[11], ARGV[12], ARGV[15])
        end
        collect_metrics(KEYS[11], KEYS[12], ARGV[17], ARGV[8])
        emit_parent_waiting_children_transition_event(KEYS[9], ARGV[16], 'failed', parent_id, parent["failed_reason"])
      elseif fail_parent then
        redis.call('ZREM', KEYS[6], parent_id)
        parent["processed_at"] = cjson.null
        parent["finished_at"] = cjson.null
        parent["worker_id"] = cjson.null
        parent["lock_token"] = cjson.null
        parent["lease_expires_at"] = cjson.null
        parent["deferred_failure"] = "child job " .. ARGV[1] .. " failed"
        parent["failed_reason"] = cjson.null

        local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
        if parent_scheduled_millis <= tonumber(ARGV[8]) then
          parent["state"] = "waiting"
          local priority = tonumber(parent["priority"] or '1000') or 1000
          enqueue_waiting_job(KEYS[1], KEYS[7], KEYS[8], parent, parent_id, priority, ARGV[13], KEYS[#KEYS])
        else
          parent["state"] = "delayed"
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[3], parent_scheduled_millis, parent_id)
          refresh_delay_marker(KEYS[#KEYS], KEYS[3])
        end
        emit_parent_waiting_children_transition_event(KEYS[9], ARGV[16], parent["state"], parent_id)
      elseif continue_parent then
        redis.call('ZREM', KEYS[6], parent_id)
        parent["processed_at"] = cjson.null
        parent["finished_at"] = cjson.null
        parent["worker_id"] = cjson.null
        parent["lock_token"] = cjson.null
        parent["lease_expires_at"] = cjson.null
        parent["deferred_failure"] = cjson.null
        parent["failed_reason"] = cjson.null

        local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
        if parent_scheduled_millis <= tonumber(ARGV[8]) then
          parent["state"] = "waiting"
          local priority = tonumber(parent["priority"] or '1000') or 1000
          enqueue_waiting_job(KEYS[1], KEYS[7], KEYS[8], parent, parent_id, priority, ARGV[13], KEYS[#KEYS])
        else
          parent["state"] = "delayed"
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[3], parent_scheduled_millis, parent_id)
          refresh_delay_marker(KEYS[#KEYS], KEYS[3])
        end
        emit_parent_waiting_children_transition_event(KEYS[9], ARGV[16], parent["state"], parent_id)
      elseif all_done then
        redis.call('DEL', dependency_key)
        redis.call('ZREM', KEYS[6], parent_id)
        parent["processed_at"] = cjson.null
        parent["finished_at"] = cjson.null
        parent["worker_id"] = cjson.null
        parent["lock_token"] = cjson.null
        parent["lease_expires_at"] = cjson.null
        parent["deferred_failure"] = cjson.null
        parent["failed_reason"] = cjson.null

        local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
        if parent_scheduled_millis <= tonumber(ARGV[8]) then
          parent["state"] = "waiting"
          local priority = tonumber(parent["priority"] or '1000') or 1000
          enqueue_waiting_job(KEYS[1], KEYS[7], KEYS[8], parent, parent_id, priority, ARGV[13], KEYS[#KEYS])
        else
          parent["state"] = "delayed"
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[3], parent_scheduled_millis, parent_id)
          refresh_delay_marker(KEYS[#KEYS], KEYS[3])
        end
        emit_parent_waiting_children_transition_event(KEYS[9], ARGV[16], parent["state"], parent_id)
      end
    end
  end
end

add_base_marker_if_waiting(KEYS[#KEYS], KEYS[7])
redis.call('XADD', KEYS[9], 'MAXLEN', '~', ARGV[16], '*', 'event', 'failed', 'jobId', ARGV[1], 'failedReason', ARGV[4], 'prev', 'active')
local max_retries = 0
if job["options"] and job["options"] ~= cjson.null and job["options"]["retry_policy"] and job["options"]["retry_policy"] ~= cjson.null then
  max_retries = tonumber(job["options"]["retry_policy"]["max_retries"] or '0') or 0
end
local attempts_made = tonumber(job["attempts_made"] or '0') or 0
if attempts_made > max_retries then
  redis.call(
    'XADD',
    KEYS[9],
    'MAXLEN',
    '~',
    ARGV[16],
    '*',
    'event',
    'retries-exhausted',
    'jobId',
    ARGV[1],
    'attemptsMade',
    attempts_made
  )
end
emit_drained_event_if_needed(KEYS[7], KEYS[2], KEYS[9], ARGV[16])

return {'ok', updated}
"#;

const RENEW_LEASE_SCRIPT: &str = r#"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] ~= "active" then
  return {'state', job["state"] or ''}
end

local lock_token = redis.call('GET', KEYS[3])
if not lock_token then
  return {'lock_missing'}
end
if lock_token ~= ARGV[2] then
  return {'lock_mismatch'}
end

redis.call('SET', KEYS[3], ARGV[2], 'PX', ARGV[5])
redis.call('SREM', KEYS[4], ARGV[1])
job["lease_expires_at"] = ARGV[3]
local updated = cjson.encode(job)
redis.call('HSET', KEYS[1], ARGV[1], updated)
redis.call('ZADD', KEYS[2], ARGV[4], ARGV[1])
return {'ok', updated}
"#;

const RENEW_LEASES_SCRIPT: &str = r#"
local jobs_key = KEYS[1]
local active_key = KEYS[2]
local stalled_key = KEYS[3]
local lock_prefix = ARGV[1]
local lease_expires_at = ARGV[2]
local lease_score = ARGV[3]
local lock_duration = ARGV[4]
local count = tonumber(ARGV[5]) or 0
local failed = {}
local offset = 6

for index = 1, count do
  local job_id = ARGV[offset]
  local token = ARGV[offset + 1]
  local raw = redis.call('HGET', jobs_key, job_id)
  local ok = false

  if raw then
    local job = cjson.decode(raw)
    if job["state"] == "active" and redis.call('ZSCORE', active_key, job_id) then
      local lock_key = lock_prefix .. job_id
      local current_token = redis.call('GET', lock_key)
      if current_token and current_token == token then
        redis.call('SET', lock_key, token, 'PX', lock_duration)
        redis.call('SREM', stalled_key, job_id)
        job["lease_expires_at"] = lease_expires_at
        redis.call('HSET', jobs_key, job_id, cjson.encode(job))
        redis.call('ZADD', active_key, lease_score, job_id)
        ok = true
      end
    end
  end

  if not ok then
    failed[#failed + 1] = job_id
  end
  offset = offset + 2
end

return failed
"#;

const DELAY_ACTIVE_JOB_SCRIPT: &str = r#"
local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function add_base_marker_if_waiting(marker_key, waiting_key)
  if marker_key and marker_key ~= '' and redis.call('ZCARD', waiting_key) > 0 then
    redis.call('ZADD', marker_key, 0, '0')
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function sync_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = repeat_key(job)
  if not key or not repeat_prefix or repeat_prefix == '' then
    return
  end

  local owner_key = repeat_prefix .. key
  local owner_id = redis.call('GET', owner_key)
  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local scheduler_owner_id = redis.call('HGET', meta_key, 'jid')
  if owner_id and owner_id ~= job_id then
    return
  end
  if not owner_id and scheduler_owner_id and scheduler_owner_id ~= job_id then
    return
  end
  if not owner_id then
    redis.call('SET', owner_key, job_id, 'NX')
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  redis.call('ZREM', KEYS[2], ARGV[1])
  redis.call('DEL', KEYS[4])
  redis.call('SREM', KEYS[6], ARGV[1])
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] ~= "active" then
  redis.call('ZREM', KEYS[2], ARGV[1])
  return {'state', job["state"] or ''}
end

local lock_token = redis.call('GET', KEYS[4])
if not lock_token then
  return {'lock_missing'}
end
if lock_token ~= ARGV[2] then
  return {'lock_mismatch'}
end

local removed_from_active = redis.call('ZREM', KEYS[2], ARGV[1])
if removed_from_active == 0 then
  return {'state', 'active_index_missing'}
end

redis.call('DEL', KEYS[4])
redis.call('SREM', KEYS[6], ARGV[1])

job["state"] = "delayed"
job["scheduled_at"] = ARGV[3]
job["processed_at"] = cjson.null
job["worker_id"] = cjson.null
job["lock_token"] = cjson.null
job["lease_expires_at"] = cjson.null
job["deferred_failure"] = cjson.null
job["failed_reason"] = cjson.null
if not job["options"] or job["options"] == cjson.null then
  job["options"] = {}
end
job["options"]["delay"] = cjson.decode(ARGV[5])

local updated = cjson.encode(job)
redis.call('HSET', KEYS[1], ARGV[1], updated)
redis.call('ZADD', KEYS[3], ARGV[4], ARGV[1])
refresh_delay_marker(KEYS[#KEYS], KEYS[3])
add_base_marker_if_waiting(KEYS[#KEYS], KEYS[7])
sync_repeat_scheduler(job, ARGV[1], ARGV[7], ARGV[4])
redis.call('XADD', KEYS[5], 'MAXLEN', '~', ARGV[6], '*', 'event', 'delayed', 'jobId', ARGV[1], 'delay', ARGV[4], 'prev', 'active')

return {'ok', updated}
"#;

const RELEASE_ACTIVE_JOB_SCRIPT: &str = r#"
local function push_back_waiting_job(jobs_key, waiting_key, job, job_id, priority, bucket, marker_key)
  job["enqueued_seq"] = 0
  local waiting_score = priority * tonumber(bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function sync_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = repeat_key(job)
  if not key or not repeat_prefix or repeat_prefix == '' then
    return
  end

  local owner_key = repeat_prefix .. key
  local owner_id = redis.call('GET', owner_key)
  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local scheduler_owner_id = redis.call('HGET', meta_key, 'jid')
  if owner_id and owner_id ~= job_id then
    return
  end
  if not owner_id and scheduler_owner_id and scheduler_owner_id ~= job_id then
    return
  end
  if not owner_id then
    redis.call('SET', owner_key, job_id, 'NX')
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  redis.call('ZREM', KEYS[2], ARGV[1])
  redis.call('DEL', KEYS[5])
  redis.call('SREM', KEYS[7], ARGV[1])
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] ~= "active" then
  redis.call('ZREM', KEYS[2], ARGV[1])
  return {'state', job["state"] or ''}
end

local lock_token = redis.call('GET', KEYS[5])
if not lock_token then
  return {'lock_missing'}
end
if lock_token ~= ARGV[2] then
  return {'lock_mismatch'}
end

local removed_from_active = redis.call('ZREM', KEYS[2], ARGV[1])
if removed_from_active == 0 then
  return {'state', 'active_index_missing'}
end

redis.call('DEL', KEYS[5])
redis.call('SREM', KEYS[7], ARGV[1])

job["state"] = "waiting"
job["scheduled_at"] = ARGV[3]
job["processed_at"] = cjson.null
job["worker_id"] = cjson.null
job["lock_token"] = cjson.null
job["lease_expires_at"] = cjson.null
job["deferred_failure"] = cjson.null
job["failed_reason"] = cjson.null

local priority = tonumber(job["priority"] or '1000') or 1000
local updated = push_back_waiting_job(KEYS[1], KEYS[3], job, ARGV[1], priority, ARGV[5], KEYS[#KEYS])
sync_repeat_scheduler(job, ARGV[1], ARGV[7], ARGV[4])
redis.call('XADD', KEYS[6], 'MAXLEN', '~', ARGV[6], '*', 'event', 'waiting', 'jobId', ARGV[1], 'prev', 'active')

return {'ok', updated}
"#;

const RECOVER_STALLED_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function deduplication_id(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  local id = deduplication["id"]
  if not id or id == cjson.null or id == '' then
    return nil
  end
  return id
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function civil_from_days(days)
  local z = days + 719468
  local era = math.floor(z / 146097)
  local doe = z - era * 146097
  local yoe = math.floor((doe - math.floor(doe / 1460) + math.floor(doe / 36524) - math.floor(doe / 146096)) / 365)
  local year = yoe + era * 400
  local doy = doe - (365 * yoe + math.floor(yoe / 4) - math.floor(yoe / 100))
  local mp = math.floor((5 * doy + 2) / 153)
  local day = doy - math.floor((153 * mp + 2) / 5) + 1
  local month = mp + 3
  if mp >= 10 then
    month = mp - 9
  end
  if month <= 2 then
    year = year + 1
  end
  return year, month, day
end

local function millis_to_iso(value)
  local day_millis = 86400000
  local days = math.floor(value / day_millis)
  local remainder = value - (days * day_millis)
  if remainder < 0 then
    days = days - 1
    remainder = remainder + day_millis
  end
  local hour = math.floor(remainder / 3600000)
  remainder = remainder - (hour * 3600000)
  local minute = math.floor(remainder / 60000)
  remainder = remainder - (minute * 60000)
  local second = math.floor(remainder / 1000)
  local millisecond = remainder - (second * 1000)
  local year, month, day = civil_from_days(days)
  return string.format("%04d-%02d-%02dT%02d:%02d:%02d.%03d+00:00", year, month, day, hour, minute, second, millisecond)
end

local function deduplication_ttl_millis(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  if deduplication["keep_last_if_active"] == true then
    return nil
  end
  return duration_millis(deduplication["ttl"])
end

local function release_deduplication_key(job, job_id, deduplication_prefix, deduplication_next_prefix)
  local id = deduplication_id(job)
  if id then
    local key = deduplication_prefix .. id
    if redis.call('GET', key) == job_id then
      redis.call('DEL', key)
      if deduplication_next_prefix and deduplication_next_prefix ~= '' then
        redis.call('DEL', deduplication_next_prefix .. id)
      end
    end
  end
end

local function set_deduplication_key(job, job_id, deduplication_prefix)
  local id = deduplication_id(job)
  if id then
    local ttl = deduplication_ttl_millis(job)
    if ttl then
      redis.call('SET', deduplication_prefix .. id, job_id, 'PX', ttl)
    else
      redis.call('SET', deduplication_prefix .. id, job_id)
    end
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function store_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = repeat_key(job)
  if not key then
    return
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local function release_repeat_key(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if key then
    local owner_key = repeat_prefix .. key
    local owner_matches = redis.call('GET', owner_key) == job_id
    if owner_matches then
      redis.call('DEL', owner_key)
    end

    local scheduler_meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
    local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
    if scheduler_owner_id == job_id or (owner_matches and not scheduler_owner_id) then
      redis.call('ZREM', repeat_scheduler_key(repeat_prefix), key)
      redis.call('DEL', scheduler_meta_key)
    end
  end
end

local function set_repeat_key(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if key then
    redis.call('SET', repeat_prefix .. key, job_id)
  end
end

local function is_current_repeat_scheduler_job(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if not key then
    return false
  end
  local owner_key = repeat_prefix .. key
  local owner_id = redis.call('GET', owner_key)
  if owner_id == job_id then
    return true
  end
  if redis.call('HGET', repeat_scheduler_meta_key(repeat_prefix, key), 'jid') == job_id then
    if not owner_id then
      redis.call('SET', owner_key, job_id, 'NX')
    end
    return true
  end
  return false
end

local function retention_options(job, remove_field, retention_field)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  if job["options"][remove_field] == true then
    return { count = 0 }
  end
  local retention = job["options"][retention_field]
  if retention and retention ~= cjson.null then
    return retention
  end
  return nil
end

local function retention_count(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local count = retention["count"]
  if count == nil or count == cjson.null then
    return nil
  end
  return tonumber(count)
end

local function retention_age_millis(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local age = duration_millis(retention["age"])
  if age and age > 0 then
    return age
  end
  return nil
end

local function retention_limit(retention)
  if not retention or retention == cjson.null then
    return 1000
  end
  local limit = tonumber(retention["limit"] or '1000') or 1000
  if limit <= 0 then
    return 1000
  end
  return limit
end

local function remove_dependency_keys(dependency_prefix, job_id)
  redis.call(
    'DEL',
    dependency_prefix .. job_id,
    dependency_prefix .. job_id .. ':processed',
    dependency_prefix .. job_id .. ':failed',
    dependency_prefix .. job_id .. ':unsuccessful'
  )
end

local function remove_finished_job(jobs_key, job_id, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  local raw = redis.call('HGET', jobs_key, job_id)
  if raw then
    local removed = cjson.decode(raw)
    release_repeat_key(removed, job_id, repeat_prefix)
  end
  redis.call('HDEL', jobs_key, job_id)
  remove_dependency_keys(dependency_prefix, job_id)
  redis.call('DEL', logs_prefix .. job_id)
end

local function remove_finished_jobs_by_max_age(timestamp, retention, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  local max_age = retention_age_millis(retention)
  if not max_age then
    return
  end
  local max_limit = retention_limit(retention)
  local start = tonumber(timestamp) - max_age
  local job_ids = redis.call('ZREVRANGEBYSCORE', target_set, start, '-inf', 'LIMIT', 0, max_limit)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(jobs_key, job_id, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  end
  if #job_ids > 0 then
    if #job_ids < max_limit then
      redis.call('ZREMRANGEBYSCORE', target_set, '-inf', start)
    else
      for _, job_id in ipairs(job_ids) do
        redis.call('ZREM', target_set, job_id)
      end
    end
  end
end

local function remove_finished_jobs_by_max_count(max_count, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  if not max_count or max_count <= 0 then
    return
  end
  local job_ids = redis.call('ZREVRANGE', target_set, max_count, -1)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(jobs_key, job_id, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  end
  redis.call('ZREMRANGEBYRANK', target_set, 0, -(max_count + 1))
end

local function apply_finished_retention(timestamp, retention, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  if not retention or retention == cjson.null then
    return
  end
  remove_finished_jobs_by_max_age(timestamp, retention, target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
  remove_finished_jobs_by_max_count(retention_count(retention), target_set, jobs_key, dependency_prefix, deduplication_prefix, repeat_prefix, logs_prefix)
end

local function repeat_keep_last_next_count(owner_job, next_job)
  local next_repeat_key = repeat_key(next_job)
  local owner_repeat_key = repeat_key(owner_job)
  if not next_repeat_key then
    if owner_repeat_key then
      return false
    end
    return nil
  end
  if not owner_repeat_key or owner_repeat_key ~= next_repeat_key then
    return false
  end

  local next_count = (tonumber(owner_job["repeat_count"] or '0') or 0) + 1
  local repeat_options = nil
  if owner_job["options"] and owner_job["options"] ~= cjson.null then
    repeat_options = owner_job["options"]["repeat"]
  end
  if repeat_options and repeat_options ~= cjson.null then
    local limit = tonumber(repeat_options["limit"] or '0') or 0
    if limit > 0 and next_count >= limit then
      return false
    end
  end
  return next_count
end

local function prepare_deduplicated_next_job(job, now_iso, now_millis)
  local now_value = tonumber(now_millis)
  local delay = nil
  if job["options"] and job["options"] ~= cjson.null then
    delay = duration_millis(job["options"]["delay"])
  end
  local scheduled_millis = now_value
  if delay then
    scheduled_millis = scheduled_millis + delay
  end
  job["created_at"] = now_iso
  job["scheduled_at"] = millis_to_iso(scheduled_millis)
  if scheduled_millis > now_value then
    job["state"] = "delayed"
  else
    job["state"] = "waiting"
  end
  job["attempts_made"] = 0
  job["stalled_count"] = 0
  job["processed_at"] = cjson.null
  job["finished_at"] = cjson.null
  job["worker_id"] = cjson.null
  job["lock_token"] = cjson.null
  job["lease_expires_at"] = cjson.null
  job["deferred_failure"] = cjson.null
  job["failed_reason"] = cjson.null
  job["return_value"] = cjson.null
  job["progress"] = cjson.null
  job["logs"] = {}

  local deduplication_ttl = deduplication_ttl_millis(job)
  if deduplication_ttl then
    job["deduplication_expires_at"] = millis_to_iso(now_value + deduplication_ttl)
  else
    job["deduplication_expires_at"] = cjson.null
  end
  return scheduled_millis
end

local function enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, job, scheduled_millis, waiting_score_bucket, marker_key)
  if job["state"] == "waiting" then
    local priority = tonumber(job["priority"] or '1000') or 1000
    enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job["id"], priority, waiting_score_bucket, marker_key)
  elseif job["state"] == "delayed" then
    redis.call('HSET', jobs_key, job["id"], cjson.encode(job))
    redis.call('ZADD', delayed_key, scheduled_millis, job["id"])
    refresh_delay_marker(marker_key, delayed_key)
  elseif job["state"] == "waiting_children" then
    redis.call('HSET', jobs_key, job["id"], cjson.encode(job))
    redis.call('ZADD', waiting_children_key, scheduled_millis, job["id"])
  else
    redis.call('HSET', jobs_key, job["id"], cjson.encode(job))
  end
end

local function enqueue_deduplicated_next_flow(next_key, next_payload, jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, deduplication_prefix, dependency_prefix, now_iso, now_millis, waiting_score_bucket, marker_key)
  local parent = next_payload["parent"]
  if not parent or parent == cjson.null then
    redis.call('DEL', next_key)
    return false
  end
  local children = next_payload["children"] or {}
  if redis.call('HEXISTS', jobs_key, parent["id"]) == 1 then
    redis.call('DEL', next_key)
    return false
  end
  for _, child in ipairs(children) do
    if redis.call('HEXISTS', jobs_key, child["id"]) == 1 then
      redis.call('DEL', next_key)
      return false
    end
  end

  local child_ids = {}
  for _, child in ipairs(children) do
    child_ids[#child_ids + 1] = child["id"]
  end

  local parent_scheduled_millis = prepare_deduplicated_next_job(parent, now_iso, now_millis)
  parent["parent_id"] = cjson.null
  parent["child_ids"] = child_ids
  if #children > 0 then
    parent["state"] = "waiting_children"
  end
  set_deduplication_key(parent, parent["id"], deduplication_prefix)
  enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, parent, parent_scheduled_millis, waiting_score_bucket, marker_key)

  if #children > 0 then
    local dependency_key = dependency_prefix .. parent["id"]
    redis.call('DEL', dependency_key)
    for _, child in ipairs(children) do
      local child_scheduled_millis = prepare_deduplicated_next_job(child, now_iso, now_millis)
      child["parent_id"] = parent["id"]
      child["child_ids"] = {}
      redis.call('SADD', dependency_key, child["id"])
      set_deduplication_key(child, child["id"], deduplication_prefix)
      enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, child, child_scheduled_millis, waiting_score_bucket, marker_key)
    end
  end

  redis.call('DEL', next_key)
  return true
end

local function enqueue_deduplicated_next(owner_job, jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, deduplication_prefix, deduplication_next_prefix, repeat_prefix, dependency_prefix, now_iso, now_millis, waiting_score_bucket, marker_key)
  local id = deduplication_id(owner_job)
  if not id then
    return false
  end
  local next_key = deduplication_next_prefix .. id
  local next_raw = redis.call('GET', next_key)
  if not next_raw then
    return false
  end
  local next_job = cjson.decode(next_raw)
  if next_job["kind"] == "flow" then
    return enqueue_deduplicated_next_flow(next_key, next_job, jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, deduplication_prefix, dependency_prefix, now_iso, now_millis, waiting_score_bucket, marker_key)
  end
  if redis.call('HEXISTS', jobs_key, next_job["id"]) == 1 then
    redis.call('DEL', next_key)
    return false
  end

  local repeat_next_count = repeat_keep_last_next_count(owner_job, next_job)
  if repeat_next_count == false then
    redis.call('DEL', next_key)
    return false
  end

  local scheduled_millis = prepare_deduplicated_next_job(next_job, now_iso, now_millis)
  if repeat_next_count then
    next_job["repeat_key"] = owner_job["repeat_key"]
    next_job["repeat_count"] = repeat_next_count
  end

  set_deduplication_key(next_job, next_job["id"], deduplication_prefix)
  if repeat_next_count then
    set_repeat_key(next_job, next_job["id"], repeat_prefix)
    store_repeat_scheduler(next_job, next_job["id"], repeat_prefix, scheduled_millis)
  end
  enqueue_prepared_job(jobs_key, waiting_key, waiting_children_key, delayed_key, sequence_key, next_job, scheduled_millis, waiting_score_bucket, marker_key)
  local parent_id = next_job["parent_id"]
  if parent_id and parent_id ~= cjson.null then
    local parent_raw = redis.call('HGET', jobs_key, parent_id)
    if parent_raw then
      local parent = cjson.decode(parent_raw)
      local child_ids = parent["child_ids"] or {}
      local found_child = false
      for _, child_id in ipairs(child_ids) do
        if child_id == next_job["id"] then
          found_child = true
          break
        end
      end
      if not found_child then
        child_ids[#child_ids + 1] = next_job["id"]
        parent["child_ids"] = child_ids
        redis.call('HSET', jobs_key, parent_id, cjson.encode(parent))
      end
      redis.call('SADD', dependency_prefix .. parent_id, next_job["id"])
    end
  end
  redis.call('DEL', next_key)
  return true
end

local function mark_active_jobs_stalled(active_key, stalled_key)
  local active_ids = redis.call('ZRANGE', active_key, 0, -1)
  for i = 1, #active_ids, 7000 do
    local last = math.min(i + 6999, #active_ids)
    redis.call('SADD', stalled_key, unpack(active_ids, i, last))
  end
end

local function collect_metrics(meta_key, data_key, max_points, timestamp)
  local max_data_points = tonumber(max_points)
  if not max_data_points or max_data_points <= 0 then
    return
  end

  local count = redis.call('HINCRBY', meta_key, 'count', 1) - 1
  local prev_ts = redis.call('HGET', meta_key, 'prevTS')
  if not prev_ts then
    redis.call('HSET', meta_key, 'prevTS', timestamp, 'prevCount', 0)
    return
  end

  local n = math.min(math.floor(tonumber(timestamp) / 60000) - math.floor(tonumber(prev_ts) / 60000), max_data_points)
  if n > 0 then
    local prev_count = tonumber(redis.call('HGET', meta_key, 'prevCount') or '0') or 0
    local delta = count - prev_count
    if n > 1 then
      local points = {}
      points[1] = delta
      for i = 2, n do
        points[i] = 0
      end
      for i = 1, #points, 7000 do
        local last = math.min(i + 6999, #points)
        redis.call('LPUSH', data_key, unpack(points, i, last))
      end
    else
      redis.call('LPUSH', data_key, delta)
    end
    redis.call('LTRIM', data_key, 0, max_data_points - 1)
    redis.call('HSET', meta_key, 'prevCount', count, 'prevTS', timestamp)
  end
end

local function emit_stalled_event(events_key, max_events, job_id, failed_reason)
  redis.call(
    'XADD',
    events_key,
    'MAXLEN',
    '~',
    max_events,
    '*',
    'event',
    'stalled',
    'jobId',
    job_id,
    'failedReason',
    failed_reason
  )
end

local function emit_waiting_event(events_key, max_events, job_id)
  redis.call(
    'XADD',
    events_key,
    'MAXLEN',
    '~',
    max_events,
    '*',
    'event',
    'waiting',
    'jobId',
    job_id,
    'prev',
    'active'
  )
end

local function emit_failed_event(events_key, max_events, job_id, failed_reason)
  redis.call(
    'XADD',
    events_key,
    'MAXLEN',
    '~',
    max_events,
    '*',
    'event',
    'failed',
    'jobId',
    job_id,
    'failedReason',
    failed_reason,
    'prev',
    'active'
  )
end

local function emit_parent_waiting_children_transition_event(events_key, max_events, event, job_id, failed_reason)
  if event == 'failed' then
    redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'failed', 'jobId', job_id, 'failedReason', failed_reason, 'prev', 'waiting-children')
  else
    redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', event, 'jobId', job_id, 'prev', 'waiting-children')
  end
end

local candidate_limit = tonumber(ARGV[5]) or 1000
local ids = redis.call('SPOP', KEYS[8], candidate_limit)
if not ids then
  ids = {}
end
local recovered = 0

for _, id in ipairs(ids) do
  local raw = redis.call('HGET', KEYS[1], id)
  if not raw then
    redis.call('ZREM', KEYS[2], id)
  else
    local job = cjson.decode(raw)
    if job["state"] == "active" then
      local lock_key = ARGV[3] .. id
      if redis.call('EXISTS', lock_key) == 0 then
        job["stalled_count"] = (job["stalled_count"] or 0) + 1
        job["worker_id"] = cjson.null
        job["lock_token"] = cjson.null
        job["lease_expires_at"] = cjson.null
        job["deferred_failure"] = cjson.null
        job["failed_reason"] = "job stalled after worker lease expired"
        redis.call('ZREM', KEYS[2], id)

        local max_stalled = 1
        if job["options"] and job["options"]["max_stalled_count"] ~= nil then
          max_stalled = tonumber(job["options"]["max_stalled_count"])
        end

        local function release_deduplication_key_on_finalization(job, job_id, deduplication_prefix)
          local id = deduplication_id(job)
          if id then
            local key = deduplication_prefix .. id
            local pttl = redis.call('PTTL', key)
            if pttl == 0 then
              redis.call('DEL', key)
            elseif pttl == -1 and redis.call('GET', key) == job_id then
              redis.call('DEL', key)
            end
          end
        end

        if job["stalled_count"] > max_stalled and not is_current_repeat_scheduler_job(job, id, ARGV[8]) then
          job["state"] = "failed"
          job["finished_at"] = ARGV[2]
          redis.call('DEL', ARGV[6] .. id)
          release_deduplication_key_on_finalization(job, id, ARGV[7])
          release_repeat_key(job, id, ARGV[8])
          enqueue_deduplicated_next(job, KEYS[1], KEYS[3], KEYS[6], KEYS[7], KEYS[5], ARGV[7], ARGV[9], ARGV[8], ARGV[6], ARGV[2], ARGV[1], ARGV[4], KEYS[11])

          local failure_retention = retention_options(job, 'remove_on_fail', 'failure_retention')
          local failure_max_count = retention_count(failure_retention)
          if failure_max_count == 0 then
            remove_finished_job(KEYS[1], id, ARGV[6], ARGV[7], ARGV[8], ARGV[10])
          else
            redis.call('HSET', KEYS[1], id, cjson.encode(job))
            redis.call('ZADD', KEYS[4], ARGV[1], id)
            apply_finished_retention(ARGV[1], failure_retention, KEYS[4], KEYS[1], ARGV[6], ARGV[7], ARGV[8], ARGV[10])
          end
          collect_metrics(KEYS[9], KEYS[10], ARGV[11], ARGV[1])
          emit_stalled_event(KEYS[12], ARGV[12], id, job["failed_reason"])
          emit_failed_event(KEYS[12], ARGV[12], id, job["failed_reason"])

          local parent_id = job["parent_id"]
          if parent_id and parent_id ~= cjson.null then
            local parent_raw = redis.call('HGET', KEYS[1], parent_id)
            if parent_raw then
	              local parent = cjson.decode(parent_raw)
                local dependency_key = ARGV[6] .. parent_id
                local releases_dependency_after_parent_release = job["options"] and job["options"] ~= cjson.null and (job["options"]["ignore_dependency_on_failure"] == true or job["options"]["remove_dependency_on_failure"] == true or job["options"]["continue_parent_on_failure"] == true or job["options"]["fail_parent_on_failure"] == true)
                if releases_dependency_after_parent_release and redis.call('EXISTS', dependency_key) == 1 then
                  local removed_dependency = redis.call('SREM', dependency_key, id) == 1
                  if removed_dependency then
                    if job["options"]["fail_parent_on_failure"] == true then
                      redis.call('ZADD', dependency_key .. ':unsuccessful', ARGV[1], id)
                    elseif job["options"]["ignore_dependency_on_failure"] == true or job["options"]["continue_parent_on_failure"] == true then
                      redis.call('HSET', dependency_key .. ':failed', id, job["failed_reason"])
                    end
                  end
                end
	              if parent["state"] == "waiting_children" then
	                local ignore_parent_failure = job["options"] and job["options"] ~= cjson.null and job["options"]["ignore_dependency_on_failure"] == true
	                local continue_parent_failure = job["options"] and job["options"] ~= cjson.null and job["options"]["continue_parent_on_failure"] == true
	                local fail_parent_failure = job["options"] and job["options"] ~= cjson.null and job["options"]["fail_parent_on_failure"] == true
	                local dependency_failure_releases_parent = job["options"] and job["options"] ~= cjson.null and (job["options"]["ignore_dependency_on_failure"] == true or job["options"]["remove_dependency_on_failure"] == true or job["options"]["continue_parent_on_failure"] == true or job["options"]["fail_parent_on_failure"] == true)
                local dependency_key = ARGV[6] .. parent_id
                local all_done = true
                local continue_parent = false
                local fail_parent = false
                local failed_child_id = nil
                local failed_reason = nil

                if dependency_failure_releases_parent then
                  if fail_parent_failure then
                    if redis.call('EXISTS', dependency_key) == 1 then
                      redis.call('SREM', dependency_key, id)
                    end
                    redis.call('ZADD', dependency_key .. ':unsuccessful', ARGV[1], id)
                    fail_parent = true
	                  elseif continue_parent_failure then
	                    if redis.call('EXISTS', dependency_key) == 1 then
	                      redis.call('SREM', dependency_key, id)
	                    end
	                    redis.call('HSET', dependency_key .. ':failed', id, job["failed_reason"])
	                    continue_parent = true
	                  else
	                    local had_dependency_set = redis.call('EXISTS', dependency_key) == 1
	                    if had_dependency_set then
	                      redis.call('SREM', dependency_key, id)
	                      if ignore_parent_failure then
	                        redis.call('HSET', dependency_key .. ':failed', id, job["failed_reason"])
	                      end
	                      if redis.call('SCARD', dependency_key) > 0 then
	                        all_done = false
	                      end
                    else
                      local updated = cjson.encode(job)
                      for _, child_id in ipairs(parent["child_ids"] or {}) do
                        local child_raw = nil
                        if child_id == id then
                          child_raw = updated
                        else
                          child_raw = redis.call('HGET', KEYS[1], child_id)
                        end
                        if child_raw then
                          local child = cjson.decode(child_raw)
                          if child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true)) then
                            failed_child_id = child_id
                            failed_reason = child["failed_reason"] or "unknown error"
                            break
                          elseif child["state"] ~= "completed" and child["state"] ~= "failed" then
                            all_done = false
                            break
                          end
                        end
                      end
                    end
                  end
                else
                  failed_child_id = id
                  failed_reason = job["failed_reason"]
                end

                if failed_child_id then
                  redis.call('DEL', dependency_key)
                  redis.call('ZREM', KEYS[6], parent_id)
                  parent["state"] = "failed"
                  parent["finished_at"] = ARGV[2]
                  parent["worker_id"] = cjson.null
                  parent["lock_token"] = cjson.null
                  parent["lease_expires_at"] = cjson.null
                  parent["deferred_failure"] = cjson.null
                  parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
                  release_deduplication_key_on_finalization(parent, parent_id, ARGV[7])
                  release_repeat_key(parent, parent_id, ARGV[8])
                  local parent_failure_retention = retention_options(parent, 'remove_on_fail', 'failure_retention')
                  local parent_failure_max_count = retention_count(parent_failure_retention)
                  if parent_failure_max_count == 0 then
                    remove_finished_job(KEYS[1], parent_id, ARGV[6], ARGV[7], ARGV[8], ARGV[10])
                  else
                    redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
                    redis.call('ZADD', KEYS[4], ARGV[1], parent_id)
                    apply_finished_retention(ARGV[1], parent_failure_retention, KEYS[4], KEYS[1], ARGV[6], ARGV[7], ARGV[8], ARGV[10])
                  end
                  collect_metrics(KEYS[9], KEYS[10], ARGV[11], ARGV[1])
                  emit_parent_waiting_children_transition_event(KEYS[12], ARGV[12], 'failed', parent_id, parent["failed_reason"])
                elseif fail_parent then
                  redis.call('ZREM', KEYS[6], parent_id)
                  parent["processed_at"] = cjson.null
                  parent["finished_at"] = cjson.null
                  parent["worker_id"] = cjson.null
                  parent["lock_token"] = cjson.null
                  parent["lease_expires_at"] = cjson.null
                  parent["deferred_failure"] = "child job " .. id .. " failed"
                  parent["failed_reason"] = cjson.null

                  local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
                  if parent_scheduled_millis <= tonumber(ARGV[1]) then
                    parent["state"] = "waiting"
                    local priority = tonumber(parent["priority"] or '1000') or 1000
                    enqueue_waiting_job(KEYS[1], KEYS[3], KEYS[5], parent, parent_id, priority, ARGV[4], KEYS[11])
                  else
                    parent["state"] = "delayed"
                    redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
                    redis.call('ZADD', KEYS[7], parent_scheduled_millis, parent_id)
                    refresh_delay_marker(KEYS[11], KEYS[7])
                  end
                  emit_parent_waiting_children_transition_event(KEYS[12], ARGV[12], parent["state"], parent_id)
                elseif continue_parent then
                  redis.call('ZREM', KEYS[6], parent_id)
                  parent["processed_at"] = cjson.null
                  parent["finished_at"] = cjson.null
                  parent["worker_id"] = cjson.null
                  parent["lock_token"] = cjson.null
                  parent["lease_expires_at"] = cjson.null
                  parent["deferred_failure"] = cjson.null
                  parent["failed_reason"] = cjson.null

                  local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
                  if parent_scheduled_millis <= tonumber(ARGV[1]) then
                    parent["state"] = "waiting"
                    local priority = tonumber(parent["priority"] or '1000') or 1000
                    enqueue_waiting_job(KEYS[1], KEYS[3], KEYS[5], parent, parent_id, priority, ARGV[4], KEYS[11])
                  else
                    parent["state"] = "delayed"
                    redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
                    redis.call('ZADD', KEYS[7], parent_scheduled_millis, parent_id)
                    refresh_delay_marker(KEYS[11], KEYS[7])
                  end
                  emit_parent_waiting_children_transition_event(KEYS[12], ARGV[12], parent["state"], parent_id)
                elseif all_done then
                  redis.call('DEL', dependency_key)
                  redis.call('ZREM', KEYS[6], parent_id)
                  parent["processed_at"] = cjson.null
                  parent["finished_at"] = cjson.null
                  parent["worker_id"] = cjson.null
                  parent["lock_token"] = cjson.null
                  parent["lease_expires_at"] = cjson.null
                  parent["deferred_failure"] = cjson.null
                  parent["failed_reason"] = cjson.null

                  local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
                  if parent_scheduled_millis <= tonumber(ARGV[1]) then
                    parent["state"] = "waiting"
                    local priority = tonumber(parent["priority"] or '1000') or 1000
                    enqueue_waiting_job(KEYS[1], KEYS[3], KEYS[5], parent, parent_id, priority, ARGV[4], KEYS[11])
                  else
                    parent["state"] = "delayed"
                    redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
                    redis.call('ZADD', KEYS[7], parent_scheduled_millis, parent_id)
                    refresh_delay_marker(KEYS[11], KEYS[7])
                  end
                  emit_parent_waiting_children_transition_event(KEYS[12], ARGV[12], parent["state"], parent_id)
                end
              end
            end
          end
        else
          job["state"] = "waiting"
          job["processed_at"] = cjson.null
          local priority = tonumber(job["priority"] or '1000') or 1000
          enqueue_waiting_job(KEYS[1], KEYS[3], KEYS[5], job, id, priority, ARGV[4], KEYS[11])
          emit_stalled_event(KEYS[12], ARGV[12], id, job["failed_reason"])
          emit_waiting_event(KEYS[12], ARGV[12], id)
        end

        recovered = recovered + 1
      end
    else
      redis.call('ZREM', KEYS[2], id)
    end
  end
end

mark_active_jobs_stalled(KEYS[2], KEYS[8])

return recovered
"#;

const PROMOTE_JOB_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function sync_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = repeat_key(job)
  if not key or not repeat_prefix or repeat_prefix == '' then
    return
  end

  local owner_key = repeat_prefix .. key
  local owner_id = redis.call('GET', owner_key)
  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local scheduler_owner_id = redis.call('HGET', meta_key, 'jid')
  if owner_id and owner_id ~= job_id then
    return
  end
  if not owner_id and scheduler_owner_id and scheduler_owner_id ~= job_id then
    return
  end
  if not owner_id then
    redis.call('SET', owner_key, job_id, 'NX')
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  redis.call('ZREM', KEYS[3], ARGV[1])
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] ~= "delayed" then
  redis.call('ZREM', KEYS[3], ARGV[1])
  refresh_delay_marker(KEYS[#KEYS], KEYS[3])
  return {'state', job["state"] or ''}
end

local removed_from_delayed = redis.call('ZREM', KEYS[3], ARGV[1])
refresh_delay_marker(KEYS[#KEYS], KEYS[3])

if removed_from_delayed == 0 then
  return {'state', 'delayed_index_missing'}
end

job["state"] = "waiting"
job["scheduled_at"] = ARGV[2]
local priority = tonumber(job["priority"] or '1000') or 1000
local updated = enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[4], job, ARGV[1], priority, ARGV[3], KEYS[#KEYS])
sync_repeat_scheduler(job, ARGV[1], ARGV[5], ARGV[6])
redis.call('XADD', KEYS[5], 'MAXLEN', '~', ARGV[4], '*', 'event', 'waiting', 'jobId', ARGV[1], 'prev', 'delayed')

return {'ok', updated}
"#;

const RESCHEDULE_JOB_SCRIPT: &str = r#"
local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function sync_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = repeat_key(job)
  if not key or not repeat_prefix or repeat_prefix == '' then
    return
  end

  local owner_key = repeat_prefix .. key
  local owner_id = redis.call('GET', owner_key)
  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local scheduler_owner_id = redis.call('HGET', meta_key, 'jid')
  if owner_id and owner_id ~= job_id then
    return
  end
  if not owner_id and scheduler_owner_id and scheduler_owner_id ~= job_id then
    return
  end
  if not owner_id then
    redis.call('SET', owner_key, job_id, 'NX')
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  redis.call('ZREM', KEYS[2], ARGV[1])
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] ~= "delayed" then
  redis.call('ZREM', KEYS[2], ARGV[1])
  return {'state', job["state"] or 'unknown'}
end

local removed_from_delayed = redis.call('ZREM', KEYS[2], ARGV[1])
if removed_from_delayed == 0 then
  return {'state', 'delayed_index_missing'}
end

job["scheduled_at"] = ARGV[2]
if not job["options"] or job["options"] == cjson.null then
  job["options"] = {}
end
job["options"]["delay"] = cjson.decode(ARGV[4])

local updated = cjson.encode(job)
redis.call('ZADD', KEYS[2], ARGV[3], ARGV[1])
refresh_delay_marker(KEYS[3], KEYS[2])
redis.call('HSET', KEYS[1], ARGV[1], updated)
redis.call('XADD', KEYS[4], 'MAXLEN', '~', ARGV[6], '*', 'event', 'delayed', 'jobId', ARGV[1], 'delay', ARGV[3])
sync_repeat_scheduler(job, ARGV[1], ARGV[5], ARGV[3])

return {'ok', updated}
"#;

const RETRY_JOB_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function deduplication_id(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  local id = deduplication["id"]
  if not id or id == cjson.null or id == '' then
    return nil
  end
  return id
end

local function active_deduplication_owner(jobs_key, deduplication_prefix, deduplication_id, job_id)
  if not deduplication_id then
    return nil
  end

  local deduplication_key = deduplication_prefix .. deduplication_id
  local existing_id = redis.call('GET', deduplication_key)
  if not existing_id or existing_id == job_id then
    return nil
  end

  local existing_raw = redis.call('HGET', jobs_key, existing_id)
  if not existing_raw then
    redis.call('DEL', deduplication_key)
    return nil
  end

  local existing_job = cjson.decode(existing_raw)
  if existing_job["state"] == "completed" or existing_job["state"] == "failed" then
    local pttl = redis.call('PTTL', deduplication_key)
    if pttl > 0 then
      return existing_id
    end
    redis.call('DEL', deduplication_key)
    return nil
  end

  return existing_id
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function store_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = repeat_key(job)
  if not key then
    return
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local function active_repeat_owner(jobs_key, repeat_prefix, repeat_key_value, job_id)
  if not repeat_key_value then
    return nil
  end

  local owner_key = repeat_prefix .. repeat_key_value
  local existing_id = redis.call('GET', owner_key)
  if existing_id == job_id then
    return nil
  end
  if existing_id then
    local existing_raw = redis.call('HGET', jobs_key, existing_id)
    if existing_raw then
      local existing_job = cjson.decode(existing_raw)
      if existing_job["state"] ~= "completed"
        and existing_job["state"] ~= "failed"
        and repeat_key(existing_job) == repeat_key_value then
        return existing_id
      end
    end
    redis.call('DEL', owner_key)
  end

  local scheduler_key = repeat_scheduler_key(repeat_prefix)
  local scheduler_meta_key = repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
  if not scheduler_owner_id or scheduler_owner_id == job_id then
    return nil
  end

  local scheduler_owner_raw = redis.call('HGET', jobs_key, scheduler_owner_id)
  if not scheduler_owner_raw then
    redis.call('ZREM', scheduler_key, repeat_key_value)
    redis.call('DEL', scheduler_meta_key)
    return nil
  end

  local scheduler_owner_job = cjson.decode(scheduler_owner_raw)
  if scheduler_owner_job["state"] == "completed"
    or scheduler_owner_job["state"] == "failed"
    or repeat_key(scheduler_owner_job) ~= repeat_key_value then
    redis.call('ZREM', scheduler_key, repeat_key_value)
    redis.call('DEL', scheduler_meta_key)
    return nil
  end

  redis.call('SET', owner_key, scheduler_owner_id, 'NX')
  return scheduler_owner_id
end

local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  redis.call('ZREM', KEYS[2], ARGV[1])
  redis.call('ZREM', KEYS[3], ARGV[1])
  return {'missing'}
end

local job = cjson.decode(raw)
local previous_state = job["state"] or ''
if previous_state ~= "failed" and previous_state ~= "completed" then
  redis.call('ZREM', KEYS[2], ARGV[1])
  redis.call('ZREM', KEYS[3], ARGV[1])
  return {'state', previous_state}
end

local completed_indexed = redis.call('ZSCORE', KEYS[2], ARGV[1])
local failed_indexed = redis.call('ZSCORE', KEYS[3], ARGV[1])
if previous_state == "completed" then
  if failed_indexed then
    redis.call('ZREM', KEYS[3], ARGV[1])
    return {'state', 'failed'}
  end
  if not completed_indexed then
    return {'state', 'completed_index_missing'}
  end
elseif previous_state == "failed" then
  if completed_indexed then
    redis.call('ZREM', KEYS[2], ARGV[1])
    return {'state', 'completed'}
  end
  if not failed_indexed then
    return {'state', 'failed_index_missing'}
  end
end

local retry_deduplication_id = deduplication_id(job)
local deduplication_owner =
  active_deduplication_owner(KEYS[1], ARGV[4], retry_deduplication_id, ARGV[1])
if deduplication_owner then
  return {'deduplicated', deduplication_owner}
end
local retry_repeat_key = repeat_key(job)
local repeat_owner = active_repeat_owner(KEYS[1], ARGV[5], retry_repeat_key, ARGV[1])
if repeat_owner then
  return {'repeat', repeat_owner}
end

local terminal_key = KEYS[3]
local terminal_index_missing = 'failed_index_missing'
if previous_state == "completed" then
  terminal_key = KEYS[2]
  terminal_index_missing = 'completed_index_missing'
end

local removed_from_terminal = redis.call('ZREM', terminal_key, ARGV[1])
if removed_from_terminal == 0 then
  return {'state', terminal_index_missing}
end

job["state"] = "waiting"
job["scheduled_at"] = ARGV[2]
job["processed_at"] = cjson.null
job["finished_at"] = cjson.null
job["worker_id"] = cjson.null
job["lock_token"] = cjson.null
job["lease_expires_at"] = cjson.null
job["deferred_failure"] = cjson.null
job["failed_reason"] = cjson.null
job["return_value"] = cjson.null
if retry_deduplication_id then
  if ARGV[6] ~= '' then
    job["deduplication_expires_at"] = ARGV[6]
  else
    job["deduplication_expires_at"] = cjson.null
  end
end

local priority = tonumber(job["priority"] or '1000') or 1000
local updated = enqueue_waiting_job(KEYS[1], KEYS[4], KEYS[5], job, ARGV[1], priority, ARGV[3], KEYS[#KEYS])
redis.call('XADD', KEYS[6], 'MAXLEN', '~', ARGV[8], '*', 'event', 'waiting', 'jobId', ARGV[1], 'prev', previous_state)

local parent_id = job["parent_id"]
if parent_id and parent_id ~= cjson.null then
  local parent_raw = redis.call('HGET', KEYS[1], parent_id)
  if parent_raw then
    local parent = cjson.decode(parent_raw)
    local owns_child = false
    for _, child_id in ipairs(parent["child_ids"] or {}) do
      if child_id == ARGV[1] then
        owns_child = true
        break
      end
    end

    if owns_child and parent["state"] ~= "completed" and parent["state"] ~= "failed" and parent["state"] ~= "active" then
      local parent_previous_state = parent["state"] or ""
      redis.call('SADD', ARGV[10] .. parent_id, ARGV[1])
      redis.call('HDEL', ARGV[10] .. parent_id .. ':processed', ARGV[1])
      redis.call('ZREM', ARGV[10] .. parent_id .. ':unsuccessful', ARGV[1])
      redis.call('HDEL', ARGV[10] .. parent_id .. ':failed', ARGV[1])
      redis.call('ZREM', KEYS[4], parent_id)
      redis.call('ZREM', KEYS[7], parent_id)
      redis.call('ZREM', KEYS[8], parent_id)
      if parent_previous_state == "delayed" then
        refresh_delay_marker(KEYS[#KEYS], KEYS[8])
      end
      parent["state"] = "waiting_children"
      parent["processed_at"] = cjson.null
      parent["finished_at"] = cjson.null
      parent["worker_id"] = cjson.null
      parent["lock_token"] = cjson.null
      parent["lease_expires_at"] = cjson.null
      parent["deferred_failure"] = cjson.null
      parent["failed_reason"] = cjson.null
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[7], ARGV[9], parent_id)
      if parent_previous_state ~= "waiting_children" then
        local prev_event_state = parent_previous_state
        if prev_event_state == "waiting_children" then
          prev_event_state = "waiting-children"
        end
        redis.call('XADD', KEYS[6], 'MAXLEN', '~', ARGV[8], '*', 'event', 'waiting-children', 'jobId', parent_id, 'prev', prev_event_state)
      end
    end
  end
end

if retry_deduplication_id then
  if ARGV[7] ~= '' then
    redis.call('SET', ARGV[4] .. retry_deduplication_id, ARGV[1], 'PX', ARGV[7])
  else
    redis.call('SET', ARGV[4] .. retry_deduplication_id, ARGV[1])
  end
end
if retry_repeat_key then
  redis.call('SET', ARGV[5] .. retry_repeat_key, ARGV[1])
  store_repeat_scheduler(job, ARGV[1], ARGV[5], ARGV[9])
end

return {'ok', updated}
"#;

const UPDATE_PRIORITY_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  redis.call('ZREM', KEYS[2], ARGV[1])
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] ~= "waiting" then
  redis.call('ZREM', KEYS[2], ARGV[1])
end

local priority = tonumber(ARGV[2])
job["priority"] = priority
if not job["options"] or job["options"] == cjson.null then
  job["options"] = {}
end
job["options"]["priority"] = priority
if ARGV[4] == '1' then
  job["options"]["lifo"] = true
elseif ARGV[4] == '0' then
  job["options"]["lifo"] = false
end

if job["state"] == "waiting" then
  enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[3], job, ARGV[1], priority, ARGV[3], KEYS[#KEYS])
end

local updated = cjson.encode(job)
redis.call('HSET', KEYS[1], ARGV[1], updated)

return {'ok', updated}
"#;

const PROMOTE_DUE_JOBS_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function days_from_civil(year, month, day)
  if month <= 2 then
    year = year - 1
  end
  local era = math.floor(year / 400)
  local yoe = year - era * 400
  local shifted_month = month
  if month > 2 then
    shifted_month = month - 3
  else
    shifted_month = month + 9
  end
  local doy = math.floor((153 * shifted_month + 2) / 5) + day - 1
  local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
  return era * 146097 + doe - 719468
end

local function iso_to_millis(value)
  local year = tonumber(string.sub(value, 1, 4))
  local month = tonumber(string.sub(value, 6, 7))
  local day = tonumber(string.sub(value, 9, 10))
  local hour = tonumber(string.sub(value, 12, 13))
  local minute = tonumber(string.sub(value, 15, 16))
  local second = tonumber(string.sub(value, 18, 19))
  if not year or not month or not day or not hour or not minute or not second then
    return 0
  end

  local millis = 0
  if string.sub(value, 20, 20) == "." then
    local digits = string.match(string.sub(value, 21), "^(%d+)")
    if digits then
      digits = string.sub(digits .. "000", 1, 3)
      millis = tonumber(digits) or 0
    end
  end

  local days = days_from_civil(year, month, day)
  return (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function sync_repeat_scheduler(job, job_id, repeat_prefix, scheduled_millis)
  local key = repeat_key(job)
  if not key or not repeat_prefix or repeat_prefix == '' then
    return
  end

  local owner_key = repeat_prefix .. key
  local owner_id = redis.call('GET', owner_key)
  local meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
  local scheduler_owner_id = redis.call('HGET', meta_key, 'jid')
  if owner_id and owner_id ~= job_id then
    return
  end
  if not owner_id and scheduler_owner_id and scheduler_owner_id ~= job_id then
    return
  end
  if not owner_id then
    redis.call('SET', owner_key, job_id, 'NX')
  end

  local repeat_options = nil
  if job["options"] and job["options"] ~= cjson.null then
    repeat_options = job["options"]["repeat"]
  end

  local encoded_repeat_options = ''
  if repeat_options and repeat_options ~= cjson.null then
    encoded_repeat_options = cjson.encode(repeat_options)
  end

  redis.call('ZADD', repeat_scheduler_key(repeat_prefix), scheduled_millis, key)
  redis.call('DEL', meta_key)
  redis.call(
    'HSET',
    meta_key,
    'jid',
    job_id,
    'name',
    job["name"] or '',
    'next',
    scheduled_millis,
    'state',
    job["state"] or '',
    'count',
    job["repeat_count"] or 0,
    'opts',
    encoded_repeat_options,
    'key',
    key
  )

  if repeat_options and repeat_options ~= cjson.null then
    if repeat_options["limit"] and repeat_options["limit"] ~= cjson.null then
      redis.call('HSET', meta_key, 'limit', repeat_options["limit"])
    end
    if repeat_options["end_at"] and repeat_options["end_at"] ~= cjson.null then
      redis.call('HSET', meta_key, 'endDate', repeat_options["end_at"])
    end

    local interval = repeat_options["interval"]
    if interval and interval ~= cjson.null then
      local every = nil
      if type(interval) == 'number' then
        every = math.floor(interval)
      else
        local secs = tonumber(interval["secs"] or interval["seconds"] or 0) or 0
        local nanos = tonumber(interval["nanos"] or interval["subsec_nanos"] or 0) or 0
        every = (secs * 1000) + math.floor(nanos / 1000000)
      end
      if every and every > 0 then
        redis.call('HSET', meta_key, 'every', every)
      end
    end

    local pattern = repeat_options["cron"]
    if pattern and pattern ~= cjson.null then
      redis.call('HSET', meta_key, 'pattern', pattern)
    end
  end
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local ids = redis.call('ZRANGEBYSCORE', KEYS[2], '-inf', ARGV[1], 'LIMIT', 0, ARGV[3])
local promoted = 0

for _, id in ipairs(ids) do
  local raw = redis.call('HGET', KEYS[1], id)
  redis.call('ZREM', KEYS[2], id)
  if raw then
    local job = cjson.decode(raw)
    if job["state"] == "delayed" then
      job["state"] = "waiting"
      local priority = tonumber(job["priority"] or '1000') or 1000
      enqueue_waiting_job(KEYS[1], KEYS[3], KEYS[4], job, id, priority, ARGV[2], KEYS[#KEYS])
      sync_repeat_scheduler(job, id, ARGV[5], iso_to_millis(job["scheduled_at"]))
      redis.call('XADD', KEYS[5], 'MAXLEN', '~', ARGV[4], '*', 'event', 'waiting', 'jobId', id, 'prev', 'delayed')
      promoted = promoted + 1
    end
  end
end
refresh_delay_marker(KEYS[#KEYS], KEYS[2])

return promoted
"#;

const REMOVE_DEDUPLICATION_KEY_SCRIPT: &str = r#"
local removed = redis.call('DEL', KEYS[1])
if removed > 0 then
  redis.call('DEL', KEYS[2])
end
return removed
"#;

const GET_DEDUPLICATION_JOB_ID_SCRIPT: &str = r#"
local job_id = redis.call('GET', KEYS[2])
if not job_id then
  return nil
end

local raw = redis.call('HGET', KEYS[1], job_id)
if not raw then
  redis.call('DEL', KEYS[2], KEYS[3])
  return nil
end

local job = cjson.decode(raw)
local deduplication = nil
if job["options"] and job["options"] ~= cjson.null then
  deduplication = job["options"]["deduplication"]
end
if not deduplication
  or deduplication == cjson.null
  or deduplication["id"] ~= ARGV[1] then
  redis.call('DEL', KEYS[2], KEYS[3])
  return nil
end
if job["state"] == "completed" or job["state"] == "failed" then
  local pttl = redis.call('PTTL', KEYS[2])
  if pttl > 0 then
    return job_id
  end
  redis.call('DEL', KEYS[2], KEYS[3])
  return nil
end

return job_id
"#;

const REMOVE_JOB_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function days_from_civil(year, month, day)
  if month <= 2 then
    year = year - 1
  end
  local era = math.floor(year / 400)
  local yoe = year - era * 400
  local shifted_month = month
  if month > 2 then
    shifted_month = month - 3
  else
    shifted_month = month + 9
  end
  local doy = math.floor((153 * shifted_month + 2) / 5) + day - 1
  local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
  return era * 146097 + doe - 719468
end

local function iso_to_millis(value)
  local year = tonumber(string.sub(value, 1, 4))
  local month = tonumber(string.sub(value, 6, 7))
  local day = tonumber(string.sub(value, 9, 10))
  local hour = tonumber(string.sub(value, 12, 13))
  local minute = tonumber(string.sub(value, 15, 16))
  local second = tonumber(string.sub(value, 18, 19))
  if not year or not month or not day or not hour or not minute or not second then
    return 0
  end

  local millis = 0
  if string.sub(value, 20, 20) == "." then
    local digits = string.match(string.sub(value, 21), "^(%d+)")
    if digits then
      digits = string.sub(digits .. "000", 1, 3)
      millis = tonumber(digits) or 0
    end
  end

  local days = days_from_civil(year, month, day)
  return (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis
end

local function deduplication_id(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  local id = deduplication["id"]
  if not id or id == cjson.null or id == '' then
    return nil
  end
  return id
end

local function release_deduplication_key(job, job_id, deduplication_prefix, deduplication_next_prefix)
  local id = deduplication_id(job)
  if id then
    local key = deduplication_prefix .. id
    if redis.call('GET', key) == job_id then
      redis.call('DEL', key)
      if deduplication_next_prefix and deduplication_next_prefix ~= '' then
        redis.call('DEL', deduplication_next_prefix .. id)
      end
    end
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function release_repeat_key(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if key then
    local owner_key = repeat_prefix .. key
    local owner_matches = redis.call('GET', owner_key) == job_id
    if owner_matches then
      redis.call('DEL', owner_key)
    end

    local scheduler_meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
    local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
    if scheduler_owner_id == job_id or (owner_matches and not scheduler_owner_id) then
      redis.call('ZREM', repeat_scheduler_key(repeat_prefix), key)
      redis.call('DEL', scheduler_meta_key)
    end
  end
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function retention_options(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  if job["options"]["remove_on_fail"] == true then
    return { count = 0 }
  end
  local retention = job["options"]["failure_retention"]
  if retention and retention ~= cjson.null then
    return retention
  end
  return nil
end

local function retention_count(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local count = retention["count"]
  if count == nil or count == cjson.null then
    return nil
  end
  return tonumber(count)
end

local function retention_age_millis(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  return duration_millis(retention["age"])
end

local function retention_limit(retention)
  if not retention or retention == cjson.null then
    return 1000
  end
  local limit = tonumber(retention["limit"] or '1000') or 1000
  if limit <= 0 then
    return 1000
  end
  return limit
end

local function remove_finished_job(job_id)
  local removed_raw = redis.call('HGET', KEYS[1], job_id)
  if removed_raw then
    local removed = cjson.decode(removed_raw)
    release_deduplication_key(removed, job_id, ARGV[6], ARGV[9])
    release_repeat_key(removed, job_id, ARGV[7])
  end
  for index = 3, 8 do
    redis.call('ZREM', KEYS[index], job_id)
  end
  redis.call('SREM', KEYS[12], job_id)
  redis.call('HDEL', KEYS[1], job_id)
  redis.call(
    'DEL',
    ARGV[5] .. job_id,
    ARGV[5] .. job_id .. ':processed',
    ARGV[5] .. job_id .. ':failed',
    ARGV[5] .. job_id .. ':unsuccessful'
  )
  redis.call('DEL', ARGV[8] .. job_id)
end

local function remove_finished_jobs_by_max_age(timestamp, retention)
  local max_age = retention_age_millis(retention)
  if not max_age then
    return
  end
  local max_limit = retention_limit(retention)
  local start = tonumber(timestamp) - max_age
  local job_ids = redis.call('ZREVRANGEBYSCORE', KEYS[8], start, '-inf', 'LIMIT', 0, max_limit)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(job_id)
  end
  if #job_ids > 0 then
    if #job_ids < max_limit then
      redis.call('ZREMRANGEBYSCORE', KEYS[8], '-inf', start)
    else
      for _, job_id in ipairs(job_ids) do
        redis.call('ZREM', KEYS[8], job_id)
      end
    end
  end
end

local function remove_finished_jobs_by_max_count(max_count)
  if not max_count or max_count <= 0 then
    return
  end
  local job_ids = redis.call('ZREVRANGE', KEYS[8], max_count, -1)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(job_id)
  end
  redis.call('ZREMRANGEBYRANK', KEYS[8], 0, -(max_count + 1))
end

local function apply_failed_retention(timestamp, retention)
  if not retention or retention == cjson.null then
    return
  end
  remove_finished_jobs_by_max_age(timestamp, retention)
  remove_finished_jobs_by_max_count(retention_count(retention))
end

local function event_state_name(state)
  if state == 'waiting_children' then
    return 'waiting-children'
  end
  return state or ''
end

local function emit_parent_waiting_children_transition_event(events_key, max_events, event, job_id, failed_reason)
  if event == 'failed' then
    redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', 'failed', 'jobId', job_id, 'failedReason', failed_reason, 'prev', 'waiting-children')
  else
    redis.call('XADD', events_key, 'MAXLEN', '~', max_events, '*', 'event', event, 'jobId', job_id, 'prev', 'waiting-children')
  end
end

local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  redis.call('DEL', KEYS[2])
  redis.call('SREM', KEYS[12], ARGV[1])
  for index = 3, 8 do
    redis.call('ZREM', KEYS[index], ARGV[1])
  end
  redis.call(
    'DEL',
    ARGV[5] .. ARGV[1],
    ARGV[5] .. ARGV[1] .. ':processed',
    ARGV[5] .. ARGV[1] .. ':failed',
    ARGV[5] .. ARGV[1] .. ':unsuccessful'
  )
  redis.call('DEL', ARGV[8] .. ARGV[1])
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] == "active" and redis.call('EXISTS', KEYS[2]) == 1 then
  return {'active'}
end

redis.call('DEL', KEYS[2])
redis.call('SREM', KEYS[12], ARGV[1])
release_deduplication_key(job, ARGV[1], ARGV[6], ARGV[9])
release_repeat_key(job, ARGV[1], ARGV[7])
for index = 3, 8 do
  redis.call('ZREM', KEYS[index], ARGV[1])
end
redis.call('HDEL', KEYS[1], ARGV[1])
redis.call(
  'DEL',
  ARGV[5] .. ARGV[1],
  ARGV[5] .. ARGV[1] .. ':processed',
  ARGV[5] .. ARGV[1] .. ':failed',
  ARGV[5] .. ARGV[1] .. ':unsuccessful'
)
redis.call('DEL', ARGV[8] .. ARGV[1])

local parent_id = job["parent_id"]
if parent_id and parent_id ~= cjson.null then
  local parent_raw = redis.call('HGET', KEYS[1], parent_id)
  if parent_raw then
    local parent = cjson.decode(parent_raw)
    if parent["state"] == "waiting_children" then
      local all_done = true
      local failed_child_id = nil
      local failed_reason = nil
      local dependency_key = ARGV[5] .. parent_id
      local had_dependency_set = redis.call('EXISTS', dependency_key) == 1

      if had_dependency_set then
        redis.call('SREM', dependency_key, ARGV[1])
        if redis.call('SCARD', dependency_key) > 0 then
          all_done = false
        end
      else
        for _, child_id in ipairs(parent["child_ids"] or {}) do
          local child_raw = nil
          if child_id ~= ARGV[1] then
            child_raw = redis.call('HGET', KEYS[1], child_id)
          end

          if child_raw then
            local child = cjson.decode(child_raw)
            if child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true)) then
              failed_child_id = child_id
              failed_reason = child["failed_reason"] or "unknown error"
              break
            elseif child["state"] ~= "completed" and child["state"] ~= "failed" then
              all_done = false
              break
            end
          end
        end
      end

      if failed_child_id then
        redis.call('DEL', dependency_key)
        redis.call('ZREM', KEYS[6], parent_id)
        parent["state"] = "failed"
        parent["finished_at"] = ARGV[2]
        parent["worker_id"] = cjson.null
        parent["lock_token"] = cjson.null
        parent["lease_expires_at"] = cjson.null
        parent["deferred_failure"] = cjson.null
        parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
        release_deduplication_key(parent, parent_id, ARGV[6], ARGV[9])
        release_repeat_key(parent, parent_id, ARGV[7])
        local parent_failure_retention = retention_options(parent)
        local parent_failure_max_count = retention_count(parent_failure_retention)
        if parent_failure_max_count == 0 then
          remove_finished_job(parent_id)
        else
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[8], ARGV[3], parent_id)
          apply_failed_retention(ARGV[3], parent_failure_retention)
        end
        emit_parent_waiting_children_transition_event(KEYS[11], ARGV[10], 'failed', parent_id, parent["failed_reason"])
      elseif all_done then
        redis.call('DEL', dependency_key)
        redis.call('ZREM', KEYS[6], parent_id)
        parent["processed_at"] = cjson.null
        parent["finished_at"] = cjson.null
        parent["worker_id"] = cjson.null
        parent["lock_token"] = cjson.null
        parent["lease_expires_at"] = cjson.null
        parent["deferred_failure"] = cjson.null
        parent["failed_reason"] = cjson.null

        local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
        if parent_scheduled_millis <= tonumber(ARGV[3]) then
          parent["state"] = "waiting"
          local priority = tonumber(parent["priority"] or '1000') or 1000
          enqueue_waiting_job(KEYS[1], KEYS[3], KEYS[9], parent, parent_id, priority, ARGV[4], KEYS[10])
        else
          parent["state"] = "delayed"
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[4], parent_scheduled_millis, parent_id)
          refresh_delay_marker(KEYS[10], KEYS[4])
        end
        emit_parent_waiting_children_transition_event(KEYS[11], ARGV[10], parent["state"], parent_id)
      end
    end
  end
end

redis.call('XADD', KEYS[11], 'MAXLEN', '~', ARGV[10], '*', 'event', 'removed', 'jobId', ARGV[1], 'prev', event_state_name(job["state"]))

return {'ok', raw}
"#;

const CLEAN_JOBS_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function days_from_civil(year, month, day)
  if month <= 2 then
    year = year - 1
  end
  local era = math.floor(year / 400)
  local yoe = year - era * 400
  local shifted_month = month
  if month > 2 then
    shifted_month = month - 3
  else
    shifted_month = month + 9
  end
  local doy = math.floor((153 * shifted_month + 2) / 5) + day - 1
  local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
  return era * 146097 + doe - 719468
end

local function iso_to_millis(value)
  local year = tonumber(string.sub(value, 1, 4))
  local month = tonumber(string.sub(value, 6, 7))
  local day = tonumber(string.sub(value, 9, 10))
  local hour = tonumber(string.sub(value, 12, 13))
  local minute = tonumber(string.sub(value, 15, 16))
  local second = tonumber(string.sub(value, 18, 19))
  if not year or not month or not day or not hour or not minute or not second then
    return 0
  end

  local millis = 0
  if string.sub(value, 20, 20) == "." then
    local digits = string.match(string.sub(value, 21), "^(%d+)")
    if digits then
      digits = string.sub(digits .. "000", 1, 3)
      millis = tonumber(digits) or 0
    end
  end

  local days = days_from_civil(year, month, day)
  return (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis
end

local function deduplication_id(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  local id = deduplication["id"]
  if not id or id == cjson.null or id == '' then
    return nil
  end
  return id
end

local function release_deduplication_key(job, job_id, deduplication_prefix, deduplication_next_prefix)
  local id = deduplication_id(job)
  if id then
    local key = deduplication_prefix .. id
    if redis.call('GET', key) == job_id then
      redis.call('DEL', key)
      if deduplication_next_prefix and deduplication_next_prefix ~= '' then
        redis.call('DEL', deduplication_next_prefix .. id)
      end
    end
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function release_repeat_key(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if key then
    local owner_key = repeat_prefix .. key
    local owner_matches = redis.call('GET', owner_key) == job_id
    if owner_matches then
      redis.call('DEL', owner_key)
    end

    local scheduler_meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
    local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
    if scheduler_owner_id == job_id or (owner_matches and not scheduler_owner_id) then
      redis.call('ZREM', repeat_scheduler_key(repeat_prefix), key)
      redis.call('DEL', scheduler_meta_key)
    end
  end
end

local function is_current_repeat_owner(job, job_id, repeat_prefix)
  if job["state"] == "completed" or job["state"] == "failed" then
    return false
  end
  local key = repeat_key(job)
  if not key then
    return false
  end

  local owner_key = repeat_prefix .. key
  local owner_id = redis.call('GET', owner_key)
  if owner_id == job_id then
    return true
  end
  if redis.call('HGET', repeat_scheduler_meta_key(repeat_prefix, key), 'jid') == job_id then
    if not owner_id then
      redis.call('SET', owner_key, job_id, 'NX')
    end
    return true
  end
  return false
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function retention_options(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  if job["options"]["remove_on_fail"] == true then
    return { count = 0 }
  end
  local retention = job["options"]["failure_retention"]
  if retention and retention ~= cjson.null then
    return retention
  end
  return nil
end

local function retention_count(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local count = retention["count"]
  if count == nil or count == cjson.null then
    return nil
  end
  return tonumber(count)
end

local function retention_age_millis(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  return duration_millis(retention["age"])
end

local function retention_limit(retention)
  if not retention or retention == cjson.null then
    return 1000
  end
  local limit = tonumber(retention["limit"] or '1000') or 1000
  if limit <= 0 then
    return 1000
  end
  return limit
end

local function remove_state_indexes(job_id)
  for key_index = 2, 7 do
    redis.call('ZREM', KEYS[key_index], job_id)
  end
end

local function remove_finished_job(job_id)
  local removed_raw = redis.call('HGET', KEYS[1], job_id)
  if removed_raw then
    local removed = cjson.decode(removed_raw)
    release_deduplication_key(removed, job_id, ARGV[10], ARGV[13])
    release_repeat_key(removed, job_id, ARGV[11])
  end
  remove_state_indexes(job_id)
  redis.call('SREM', KEYS[11], job_id)
  redis.call('HDEL', KEYS[1], job_id)
  redis.call(
    'DEL',
    ARGV[8] .. job_id,
    ARGV[8] .. job_id .. ':processed',
    ARGV[8] .. job_id .. ':failed',
    ARGV[8] .. job_id .. ':unsuccessful'
  )
  redis.call('DEL', ARGV[12] .. job_id)
end

local function remove_finished_jobs_by_max_age(timestamp, retention)
  local max_age = retention_age_millis(retention)
  if not max_age then
    return
  end
  local max_limit = retention_limit(retention)
  local start = tonumber(timestamp) - max_age
  local job_ids = redis.call('ZREVRANGEBYSCORE', KEYS[7], start, '-inf', 'LIMIT', 0, max_limit)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(job_id)
  end
  if #job_ids > 0 then
    if #job_ids < max_limit then
      redis.call('ZREMRANGEBYSCORE', KEYS[7], '-inf', start)
    else
      for _, job_id in ipairs(job_ids) do
        redis.call('ZREM', KEYS[7], job_id)
      end
    end
  end
end

local function remove_finished_jobs_by_max_count(max_count)
  if not max_count or max_count <= 0 then
    return
  end
  local job_ids = redis.call('ZREVRANGE', KEYS[7], max_count, -1)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(job_id)
  end
  redis.call('ZREMRANGEBYRANK', KEYS[7], 0, -(max_count + 1))
end

local function apply_failed_retention(timestamp, retention)
  if not retention or retention == cjson.null then
    return
  end
  remove_finished_jobs_by_max_age(timestamp, retention)
  remove_finished_jobs_by_max_count(retention_count(retention))
end

local function release_parent_after_removed_child(job, removed_id)
  local parent_id = job["parent_id"]
  if not parent_id or parent_id == cjson.null then
    return
  end

  local parent_raw = redis.call('HGET', KEYS[1], parent_id)
  if not parent_raw then
    return
  end

  local parent = cjson.decode(parent_raw)
  if parent["state"] ~= "waiting_children" then
    return
  end

  local all_done = true
  local failed_child_id = nil
  local failed_reason = nil
  local dependency_key = ARGV[8] .. parent_id
  local had_dependency_set = redis.call('EXISTS', dependency_key) == 1

  if had_dependency_set then
    redis.call('SREM', dependency_key, removed_id)
    if redis.call('SCARD', dependency_key) > 0 then
      all_done = false
    end
  else
    for _, child_id in ipairs(parent["child_ids"] or {}) do
      local child_raw = nil
      if child_id ~= removed_id then
        child_raw = redis.call('HGET', KEYS[1], child_id)
      end

      if child_raw then
        local child = cjson.decode(child_raw)
        if child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true)) then
          failed_child_id = child_id
          failed_reason = child["failed_reason"]
          if not failed_reason or failed_reason == cjson.null then
            failed_reason = "unknown error"
          end
          break
        elseif child["state"] ~= "completed" and child["state"] ~= "failed" then
          all_done = false
          break
        end
      end
    end
  end

  if failed_child_id then
    redis.call('DEL', dependency_key)
    redis.call('ZREM', KEYS[5], parent_id)
    parent["state"] = "failed"
    parent["finished_at"] = ARGV[5]
    parent["worker_id"] = cjson.null
    parent["lock_token"] = cjson.null
    parent["lease_expires_at"] = cjson.null
    parent["deferred_failure"] = cjson.null
    parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
    release_deduplication_key(parent, parent_id, ARGV[10], ARGV[13])
    release_repeat_key(parent, parent_id, ARGV[11])
    local parent_failure_retention = retention_options(parent)
    local parent_failure_max_count = retention_count(parent_failure_retention)
    if parent_failure_max_count == 0 then
      remove_finished_job(parent_id)
    else
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[7], ARGV[6], parent_id)
      apply_failed_retention(ARGV[6], parent_failure_retention)
    end
  elseif all_done then
    redis.call('DEL', dependency_key)
    redis.call('ZREM', KEYS[5], parent_id)
    parent["processed_at"] = cjson.null
    parent["finished_at"] = cjson.null
    parent["worker_id"] = cjson.null
    parent["lock_token"] = cjson.null
    parent["lease_expires_at"] = cjson.null
    parent["deferred_failure"] = cjson.null
    parent["failed_reason"] = cjson.null

    local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
    if parent_scheduled_millis <= tonumber(ARGV[6]) then
      parent["state"] = "waiting"
      local priority = tonumber(parent["priority"] or '1000') or 1000
      enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[8], parent, parent_id, priority, ARGV[7], KEYS[9])
    else
      parent["state"] = "delayed"
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[3], parent_scheduled_millis, parent_id)
      refresh_delay_marker(KEYS[9], KEYS[3])
    end
  end
end

local state = ARGV[1]
local cutoff = ARGV[2]
local limit = tonumber(ARGV[3])
local lock_prefix = ARGV[4]
local cutoff_millis = tonumber(ARGV[9])
local state_key = nil

if state == 'waiting' then
  state_key = KEYS[2]
elseif state == 'delayed' then
  state_key = KEYS[3]
elseif state == 'active' then
  state_key = KEYS[4]
elseif state == 'waiting_children' then
  state_key = KEYS[5]
elseif state == 'completed' then
  state_key = KEYS[6]
elseif state == 'failed' then
  state_key = KEYS[7]
else
  return {'bad_state'}
end

if limit <= 0 then
  return {}
end

local ids = redis.call('ZRANGE', state_key, 0, -1)
local candidates = {}

for _, id in ipairs(ids) do
  local raw = redis.call('HGET', KEYS[1], id)
  if raw then
    local job = cjson.decode(raw)
    if job["state"] == state then
      local active_locked = state == 'active' and redis.call('EXISTS', lock_prefix .. id) == 1
      if not active_locked then
        local reference = job["finished_at"]
        if not reference or reference == cjson.null then
          reference = job["processed_at"]
        end
        if not reference or reference == cjson.null then
          reference = job["scheduled_at"]
        end

        if reference and reference ~= cjson.null then
          local reference_millis = iso_to_millis(reference)
          if reference_millis <= cutoff_millis
            and not is_current_repeat_owner(job, id, ARGV[11]) then
            table.insert(candidates, { id = id, reference_millis = reference_millis, raw = raw, job = job })
          end
        end
      end
    else
      redis.call('ZREM', state_key, id)
    end
  else
    redis.call('ZREM', state_key, id)
  end
end

table.sort(candidates, function(left, right)
  if left.reference_millis == right.reference_millis then
    return left.id < right.id
  end
  return left.reference_millis < right.reference_millis
end)

local removed = {}
local count = math.min(limit, #candidates)
for index = 1, count do
  local candidate = candidates[index]
  redis.call('DEL', lock_prefix .. candidate.id)
  redis.call('SREM', KEYS[11], candidate.id)
  for key_index = 2, 7 do
    redis.call('ZREM', KEYS[key_index], candidate.id)
  end
  redis.call(
    'DEL',
    ARGV[8] .. candidate.id,
    ARGV[8] .. candidate.id .. ':processed',
    ARGV[8] .. candidate.id .. ':failed',
    ARGV[8] .. candidate.id .. ':unsuccessful'
  )
  redis.call('DEL', ARGV[12] .. candidate.id)
  release_deduplication_key(candidate.job, candidate.id, ARGV[10], ARGV[13])
  release_repeat_key(candidate.job, candidate.id, ARGV[11])
  redis.call('HDEL', KEYS[1], candidate.id)
  release_parent_after_removed_child(candidate.job, candidate.id)
  removed[index] = candidate.raw
end

redis.call('XADD', KEYS[10], 'MAXLEN', '~', ARGV[14], '*', 'event', 'cleaned', 'count', count)

return removed
"#;

const DRAIN_JOBS_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function days_from_civil(year, month, day)
  if month <= 2 then
    year = year - 1
  end
  local era = math.floor(year / 400)
  local yoe = year - era * 400
  local shifted_month = month
  if month > 2 then
    shifted_month = month - 3
  else
    shifted_month = month + 9
  end
  local doy = math.floor((153 * shifted_month + 2) / 5) + day - 1
  local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
  return era * 146097 + doe - 719468
end

local function iso_to_millis(value)
  local year = tonumber(string.sub(value, 1, 4))
  local month = tonumber(string.sub(value, 6, 7))
  local day = tonumber(string.sub(value, 9, 10))
  local hour = tonumber(string.sub(value, 12, 13))
  local minute = tonumber(string.sub(value, 15, 16))
  local second = tonumber(string.sub(value, 18, 19))
  local millis = 0
  if string.sub(value, 20, 20) == "." then
    local digits = string.match(string.sub(value, 21), "^(%d+)")
    if digits then
      digits = string.sub(digits .. "000", 1, 3)
      millis = tonumber(digits) or 0
    end
  end

  local days = days_from_civil(year, month, day)
  return (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis
end

local function deduplication_id(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  local id = deduplication["id"]
  if not id or id == cjson.null or id == '' then
    return nil
  end
  return id
end

local function release_deduplication_key(job, job_id, deduplication_prefix, deduplication_next_prefix)
  local id = deduplication_id(job)
  if id then
    local key = deduplication_prefix .. id
    if redis.call('GET', key) == job_id then
      redis.call('DEL', key)
      if deduplication_next_prefix and deduplication_next_prefix ~= '' then
        redis.call('DEL', deduplication_next_prefix .. id)
      end
    end
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function release_repeat_key(job, job_id, repeat_prefix)
  local key = repeat_key(job)
  if key then
    local owner_key = repeat_prefix .. key
    local owner_matches = redis.call('GET', owner_key) == job_id
    if owner_matches then
      redis.call('DEL', owner_key)
    end

    local scheduler_meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
    local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
    if scheduler_owner_id == job_id or (owner_matches and not scheduler_owner_id) then
      redis.call('ZREM', repeat_scheduler_key(repeat_prefix), key)
      redis.call('DEL', scheduler_meta_key)
    end
  end
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function retention_options(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  if job["options"]["remove_on_fail"] == true then
    return { count = 0 }
  end
  local retention = job["options"]["failure_retention"]
  if retention and retention ~= cjson.null then
    return retention
  end
  return nil
end

local function retention_count(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local count = retention["count"]
  if count == nil or count == cjson.null then
    return nil
  end
  return tonumber(count)
end

local function retention_age_millis(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  return duration_millis(retention["age"])
end

local function retention_limit(retention)
  if not retention or retention == cjson.null then
    return 1000
  end
  local limit = tonumber(retention["limit"] or '1000') or 1000
  if limit <= 0 then
    return 1000
  end
  return limit
end

local function remove_state_indexes(job_id)
  for key_index = 2, 7 do
    redis.call('ZREM', KEYS[key_index], job_id)
  end
end

local function remove_finished_job(job_id)
  local removed_raw = redis.call('HGET', KEYS[1], job_id)
  if removed_raw then
    local removed = cjson.decode(removed_raw)
    release_deduplication_key(removed, job_id, ARGV[7], ARGV[10])
    release_repeat_key(removed, job_id, ARGV[8])
  end
  remove_state_indexes(job_id)
  redis.call('HDEL', KEYS[1], job_id)
  redis.call(
    'DEL',
    ARGV[6] .. job_id,
    ARGV[6] .. job_id .. ':processed',
    ARGV[6] .. job_id .. ':failed',
    ARGV[6] .. job_id .. ':unsuccessful'
  )
  redis.call('DEL', ARGV[9] .. job_id)
end

local function remove_finished_jobs_by_max_age(timestamp, retention)
  local max_age = retention_age_millis(retention)
  if not max_age then
    return
  end
  local max_limit = retention_limit(retention)
  local start = tonumber(timestamp) - max_age
  local job_ids = redis.call('ZREVRANGEBYSCORE', KEYS[7], start, '-inf', 'LIMIT', 0, max_limit)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(job_id)
  end
  if #job_ids > 0 then
    if #job_ids < max_limit then
      redis.call('ZREMRANGEBYSCORE', KEYS[7], '-inf', start)
    else
      for _, job_id in ipairs(job_ids) do
        redis.call('ZREM', KEYS[7], job_id)
      end
    end
  end
end

local function remove_finished_jobs_by_max_count(max_count)
  if not max_count or max_count <= 0 then
    return
  end
  local job_ids = redis.call('ZREVRANGE', KEYS[7], max_count, -1)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(job_id)
  end
  redis.call('ZREMRANGEBYRANK', KEYS[7], 0, -(max_count + 1))
end

local function apply_failed_retention(timestamp, retention)
  if not retention or retention == cjson.null then
    return
  end
  remove_finished_jobs_by_max_age(timestamp, retention)
  remove_finished_jobs_by_max_count(retention_count(retention))
end

local function is_current_delayed_repeat_owner(job, job_id)
  if job["state"] ~= "delayed" then
    return false
  end
  local key = repeat_key(job)
  if not key then
    return false
  end
  local owner_key = ARGV[8] .. key
  local owner_id = redis.call('GET', owner_key)
  if owner_id == job_id then
    return true
  end
  if redis.call('HGET', repeat_scheduler_meta_key(ARGV[8], key), 'jid') == job_id then
    if not owner_id then
      redis.call('SET', owner_key, job_id, 'NX')
    end
    return true
  end
  return false
end

local function release_parent_after_removed_child(job, removed_id)
  local parent_id = job["parent_id"]
  if not parent_id or parent_id == cjson.null then
    return
  end

  local parent_raw = redis.call('HGET', KEYS[1], parent_id)
  if not parent_raw then
    return
  end

  local parent = cjson.decode(parent_raw)
  if parent["state"] ~= "waiting_children" then
    return
  end

  local all_done = true
  local failed_child_id = nil
  local failed_reason = nil
  local dependency_key = ARGV[6] .. parent_id
  local had_dependency_set = redis.call('EXISTS', dependency_key) == 1

  if had_dependency_set then
    redis.call('SREM', dependency_key, removed_id)
    if redis.call('SCARD', dependency_key) > 0 then
      all_done = false
    end
  else
    for _, child_id in ipairs(parent["child_ids"] or {}) do
      local child_raw = nil
      if child_id ~= removed_id then
        child_raw = redis.call('HGET', KEYS[1], child_id)
      end

      if child_raw then
        local child = cjson.decode(child_raw)
        if child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true)) then
          failed_child_id = child_id
          failed_reason = child["failed_reason"]
          if not failed_reason or failed_reason == cjson.null then
            failed_reason = "unknown error"
          end
          break
        elseif child["state"] ~= "completed" and child["state"] ~= "failed" then
          all_done = false
          break
        end
      end
    end
  end

  if failed_child_id then
    redis.call('DEL', dependency_key)
    redis.call('ZREM', KEYS[5], parent_id)
    parent["state"] = "failed"
    parent["finished_at"] = ARGV[2]
    parent["worker_id"] = cjson.null
    parent["lock_token"] = cjson.null
    parent["lease_expires_at"] = cjson.null
    parent["deferred_failure"] = cjson.null
    parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
    release_deduplication_key(parent, parent_id, ARGV[7], ARGV[10])
    release_repeat_key(parent, parent_id, ARGV[8])
    local parent_failure_retention = retention_options(parent)
    local parent_failure_max_count = retention_count(parent_failure_retention)
    if parent_failure_max_count == 0 then
      remove_finished_job(parent_id)
    else
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[7], ARGV[3], parent_id)
      apply_failed_retention(ARGV[3], parent_failure_retention)
    end
  elseif all_done then
    redis.call('DEL', dependency_key)
    redis.call('ZREM', KEYS[5], parent_id)
    parent["processed_at"] = cjson.null
    parent["finished_at"] = cjson.null
    parent["worker_id"] = cjson.null
    parent["lock_token"] = cjson.null
    parent["lease_expires_at"] = cjson.null
    parent["deferred_failure"] = cjson.null
    parent["failed_reason"] = cjson.null

    local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
    if parent_scheduled_millis <= tonumber(ARGV[3]) then
      parent["state"] = "waiting"
      local priority = tonumber(parent["priority"] or '1000') or 1000
      enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[8], parent, parent_id, priority, ARGV[4], KEYS[#KEYS])
    else
      parent["state"] = "delayed"
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[3], parent_scheduled_millis, parent_id)
      refresh_delay_marker(KEYS[#KEYS], KEYS[3])
    end
  end
end

local function collect_jobs(state_key, state, candidates)
  local ids = redis.call('ZRANGE', state_key, 0, -1)
  for _, id in ipairs(ids) do
    local raw = redis.call('HGET', KEYS[1], id)
    if raw then
      local job = cjson.decode(raw)
      if job["state"] == state then
        if not is_current_delayed_repeat_owner(job, id) then
          table.insert(candidates, { id = id, raw = raw, job = job })
        end
      else
        redis.call('ZREM', state_key, id)
      end
    else
      redis.call('ZREM', state_key, id)
    end
  end
end

local candidates = {}
collect_jobs(KEYS[2], "waiting", candidates)
if ARGV[1] == "1" then
  collect_jobs(KEYS[3], "delayed", candidates)
end

local removed = {}
for index, candidate in ipairs(candidates) do
  redis.call('DEL', ARGV[5] .. candidate.id)
  remove_state_indexes(candidate.id)
  redis.call(
    'DEL',
    ARGV[6] .. candidate.id,
    ARGV[6] .. candidate.id .. ':processed',
    ARGV[6] .. candidate.id .. ':failed',
    ARGV[6] .. candidate.id .. ':unsuccessful'
  )
  redis.call('DEL', ARGV[9] .. candidate.id)
  release_deduplication_key(candidate.job, candidate.id, ARGV[7], ARGV[10])
  release_repeat_key(candidate.job, candidate.id, ARGV[8])
  redis.call('HDEL', KEYS[1], candidate.id)
  release_parent_after_removed_child(candidate.job, candidate.id)
  removed[index] = candidate.raw
end

return removed
"#;

const OBLITERATE_SCRIPT: &str = r#"
redis.call('HSET', KEYS[1], 'paused', 1)
if redis.call('ZCARD', KEYS[2]) > 0 and ARGV[2] ~= '1' then
  return -2
end

local removed_jobs = redis.call('HLEN', KEYS[3])
local scan_count = tonumber(ARGV[3]) or 1000
if scan_count <= 0 then
  scan_count = 1000
end

local found = true
while found do
  found = false
  local cursor = '0'
  repeat
    local page = redis.call('SCAN', cursor, 'MATCH', ARGV[1] .. '*', 'COUNT', scan_count)
    cursor = page[1]
    local keys = page[2]
    if #keys > 0 then
      found = true
      for i = 1, #keys, 5000 do
        redis.call('DEL', unpack(keys, i, math.min(i + 4999, #keys)))
      end
    end
  until cursor == '0'
end
return removed_jobs
"#;

const REMOVE_ORPHANED_JOBS_SCRIPT: &str = r#"
local jobs_key = KEYS[1]
local cursor = ARGV[1]
local scan_count = tonumber(ARGV[2]) or 1000
local limit = tonumber(ARGV[3]) or 0
local dependencies_prefix = ARGV[4]
local logs_prefix = ARGV[5]
local lock_prefix = ARGV[6]

if scan_count <= 0 then
  scan_count = 1000
end

local reference_key_types = {}
for index = 2, #KEYS do
  reference_key_types[index] = redis.call('TYPE', KEYS[index])['ok']
end

local page = redis.call('HSCAN', jobs_key, cursor, 'COUNT', scan_count)
local next_cursor = page[1]
local entries = page[2]
local removed_count = 0

for index = 1, #entries, 2 do
  if limit > 0 and removed_count >= limit then
    break
  end

  local job_id = entries[index]
  local referenced = false

  for key_index = 2, #KEYS do
    local key_type = reference_key_types[key_index]
    if key_type == 'zset' then
      if redis.call('ZSCORE', KEYS[key_index], job_id) then
        referenced = true
        break
      end
    elseif key_type == 'set' then
      if redis.call('SISMEMBER', KEYS[key_index], job_id) == 1 then
        referenced = true
        break
      end
    elseif key_type == 'list' then
      if redis.call('LPOS', KEYS[key_index], job_id) then
        referenced = true
        break
      end
    end
  end

  if not referenced then
    redis.call('HDEL', jobs_key, job_id)
    redis.call(
      'DEL',
      dependencies_prefix .. job_id,
      dependencies_prefix .. job_id .. ':processed',
      dependencies_prefix .. job_id .. ':failed',
      dependencies_prefix .. job_id .. ':unsuccessful',
      logs_prefix .. job_id,
      lock_prefix .. job_id
    )
    removed_count = removed_count + 1
  end
end

return {next_cursor, removed_count}
"#;

const LIST_JOBS_SCRIPT: &str = r#"
local function iso_sort_key(value)
  if not value or value == cjson.null then
    return ''
  end

  local base = string.sub(value, 1, 19)
  local digits = ''
  if string.sub(value, 20, 20) == "." then
    digits = string.match(string.sub(value, 21), "^(%d+)") or ''
  end
  digits = string.sub(digits .. "000000000", 1, 9)
  return base .. "." .. digits
end

local function state_rank(state)
  if state == 'waiting' then
    return 0
  elseif state == 'delayed' then
    return 1
  elseif state == 'active' then
    return 2
  elseif state == 'waiting_children' then
    return 3
  elseif state == 'completed' then
    return 4
  elseif state == 'failed' then
    return 5
  end
  return 6
end

local offset = tonumber(ARGV[1])
local limit = tonumber(ARGV[2])
local ascending = ARGV[3] == '1'
local result = {}

local function state_key_for(state)
  if state == 'waiting' then
    return KEYS[2]
  elseif state == 'delayed' then
    return KEYS[3]
  elseif state == 'active' then
    return KEYS[4]
  elseif state == 'waiting_children' then
    return KEYS[5]
  elseif state == 'completed' then
    return KEYS[6]
  elseif state == 'failed' then
    return KEYS[7]
  end
  return nil
end

local function collect_state_jobs(state, state_key, jobs, seen_jobs)
  local ids = redis.call('ZRANGE', state_key, 0, -1)
  for _, id in ipairs(ids) do
    local raw = redis.call('HGET', KEYS[1], id)
    if raw then
      local job = cjson.decode(raw)
      if job["state"] == state then
        if not seen_jobs[id] then
          seen_jobs[id] = true
          table.insert(jobs, {
            raw = raw,
            state_rank = state_rank(job["state"]),
            priority = tonumber(job["priority"] or '1000') or 1000,
            scheduled_at = iso_sort_key(job["scheduled_at"]),
            created_at = iso_sort_key(job["created_at"]),
            id = job["id"] or ''
          })
        end
      else
        redis.call('ZREM', state_key, id)
      end
    else
      redis.call('ZREM', state_key, id)
    end
  end
end

local requested_states = {}
local seen_states = {}
if #ARGV > 3 then
  for index = 4, #ARGV do
    local state = ARGV[index]
    local state_key = state_key_for(state)
    if not state_key then
      return {'bad_state'}
    end
    if not seen_states[state] then
      seen_states[state] = true
      requested_states[#requested_states + 1] = state
    end
  end
else
  requested_states = {'waiting', 'delayed', 'active', 'waiting_children', 'completed', 'failed'}
end

local jobs = {}
local seen_jobs = {}
for _, state in ipairs(requested_states) do
  collect_state_jobs(state, state_key_for(state), jobs, seen_jobs)
end

table.sort(jobs, function(left, right)
  if left.state_rank ~= right.state_rank then
    return left.state_rank < right.state_rank
  elseif left.priority ~= right.priority then
    return left.priority < right.priority
  elseif left.scheduled_at ~= right.scheduled_at then
    return left.scheduled_at < right.scheduled_at
  elseif left.created_at ~= right.created_at then
    return left.created_at < right.created_at
  end
  return left.id < right.id
end)

result[1] = #jobs
if limit > 0 then
  local out = 2
  if ascending then
    local first = offset + 1
    local last = math.min(offset + limit, #jobs)
    for index = first, last do
      result[out] = jobs[index].raw
      out = out + 1
    end
  else
    local first = #jobs - offset
    local last = math.max(first - limit + 1, 1)
    for index = first, last, -1 do
      result[out] = jobs[index].raw
      out = out + 1
    end
  end
end

return result
"#;

const UPDATE_DATA_SCRIPT: &str = r#"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
job["payload"] = cjson.decode(ARGV[2])
local updated = cjson.encode(job)
redis.call('HSET', KEYS[1], ARGV[1], updated)

return {'ok', updated}
"#;

const UPDATE_PROGRESS_SCRIPT: &str = r#"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
job["progress"] = cjson.decode(ARGV[2])
local updated = cjson.encode(job)
redis.call('HSET', KEYS[1], ARGV[1], updated)
redis.call('XADD', KEYS[2], 'MAXLEN', '~', ARGV[3], '*', 'event', 'progress', 'jobId', ARGV[1], 'data', ARGV[2])

return {'ok', updated}
"#;

const SAVE_STACKTRACE_SCRIPT: &str = r#"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
job["stacktrace"] = cjson.decode(ARGV[2])
job["failed_reason"] = ARGV[3]
local updated = cjson.encode(job)
redis.call('HSET', KEYS[1], ARGV[1], updated)

return {'ok', updated}
"#;

const ADD_LOG_SCRIPT: &str = r#"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
local logs = job["logs"]
if not logs or logs == cjson.null or type(logs) ~= "table" then
  logs = {}
end

table.insert(logs, { timestamp = ARGV[2], line = ARGV[3] })

local keep = tonumber(ARGV[4])
if keep and keep > 0 then
  while #logs > keep do
    table.remove(logs, 1)
  end
end

job["logs"] = logs
local updated = cjson.encode(job)
redis.call('HSET', KEYS[1], ARGV[1], updated)
redis.call('RPUSH', KEYS[2], ARGV[5])
if keep and keep > 0 then
  redis.call('LTRIM', KEYS[2], -keep, -1)
end

return {'ok', updated}
"#;

const CLEAR_LOGS_SCRIPT: &str = r#"
local keep = tonumber(ARGV[2]) or 0
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if raw then
  local job = cjson.decode(raw)
  local logs = job["logs"]
  if not logs or logs == cjson.null or type(logs) ~= "table" then
    logs = {}
  end

  if keep <= 0 then
    logs = {}
  else
    while #logs > keep do
      table.remove(logs, 1)
    end
  end

  job["logs"] = logs
  redis.call('HSET', KEYS[1], ARGV[1], cjson.encode(job))
end

if keep > 0 then
  redis.call('LTRIM', KEYS[2], -keep, -1)
else
  redis.call('DEL', KEYS[2])
end

local retained = redis.call('LRANGE', KEYS[2], 0, -1)
local result = { tostring(redis.call('LLEN', KEYS[2])) }
for _, entry in ipairs(retained) do
  result[#result + 1] = entry
end
return result
"#;

const GET_JOB_STATE_SCRIPT: &str = r#"
if redis.call('ZSCORE', KEYS[1], ARGV[1]) then
  return 'completed'
end
if redis.call('ZSCORE', KEYS[2], ARGV[1]) then
  return 'failed'
end
if redis.call('ZSCORE', KEYS[3], ARGV[1]) then
  return 'delayed'
end
if redis.call('ZSCORE', KEYS[4], ARGV[1]) then
  return 'active'
end
if redis.call('ZSCORE', KEYS[5], ARGV[1]) then
  return 'waiting'
end
if redis.call('ZSCORE', KEYS[6], ARGV[1]) then
  return 'waiting_children'
end
return 'unknown'
"#;

const GET_JOB_FINISHED_RESULT_SCRIPT: &str = r#"
local raw = redis.call('HGET', KEYS[3], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
if redis.call('ZSCORE', KEYS[1], ARGV[1]) then
  local return_value = job["return_value"]
  if return_value ~= nil and return_value ~= cjson.null then
    return {'completed', cjson.encode(return_value)}
  end
  return {'completed'}
end

if redis.call('ZSCORE', KEYS[2], ARGV[1]) then
  local failed_reason = job["failed_reason"]
  if failed_reason and failed_reason ~= cjson.null then
    return {'failed', failed_reason}
  end
  return {'failed'}
end

return {'not_finished'}
"#;

const GET_METRICS_SCRIPT: &str = r#"
local metrics = redis.call('HMGET', KEYS[1], 'count', 'prevTS', 'prevCount')
local data = redis.call('LRANGE', KEYS[2], tonumber(ARGV[1]), tonumber(ARGV[2]))
local num_points = redis.call('LLEN', KEYS[2])
return {metrics, data, num_points}
"#;

const STATS_SCRIPT: &str = r#"
local paused_value = 0
local paused = redis.call('HGET', KEYS[1], 'paused')
if paused == '0' then
  redis.call('HDEL', KEYS[1], 'paused')
elseif paused then
  paused_value = 1
end

return {
  redis.call('ZCARD', KEYS[2]),
  redis.call('ZCARD', KEYS[3]),
  redis.call('ZCARD', KEYS[4]),
  redis.call('ZCARD', KEYS[5]),
  redis.call('ZCARD', KEYS[6]),
  redis.call('ZCARD', KEYS[7]),
  paused_value
}
"#;

const JOB_COUNTS_SCRIPT: &str = r#"
local results = {}

for index = 1, #KEYS do
  results[#results + 1] = redis.call('ZCARD', KEYS[index])
end

return results
"#;

const COUNTS_PER_PRIORITY_SCRIPT: &str = r#"
local bucket = tonumber(ARGV[1])
local results = {}

for index = 2, #ARGV do
  local priority = tonumber(ARGV[index])
  local min_score = priority * bucket
  local max_score = ((priority + 1) * bucket) - 1
  results[#results + 1] = redis.call('ZCOUNT', KEYS[1], min_score, max_score)
end

return results
"#;

const FLOW_DEPENDENCY_COUNTS_SCRIPT: &str = r#"
local parent_raw = redis.call('HGET', KEYS[1], ARGV[1])
if not parent_raw then
  return {'missing'}
end

local parent = cjson.decode(parent_raw)
local has_dependency_set = redis.call('EXISTS', KEYS[2]) == 1
local pending_child_ids = {}
for _, pending_child_id in ipairs(redis.call('SMEMBERS', KEYS[2])) do
  pending_child_ids[pending_child_id] = true
end

local processed_child_ids = {}
for _, processed_child_id in ipairs(redis.call('HKEYS', KEYS[3])) do
  processed_child_ids[processed_child_id] = true
end

local ignored_child_ids = {}
for _, ignored_child_id in ipairs(redis.call('HKEYS', KEYS[4])) do
  ignored_child_ids[ignored_child_id] = true
end

local unsuccessful_child_ids = {}
for _, unsuccessful_child_id in ipairs(redis.call('ZRANGE', KEYS[5], 0, -1)) do
  unsuccessful_child_ids[unsuccessful_child_id] = true
end

local processed = redis.call('HLEN', KEYS[3])
local unprocessed = 0
local failed = redis.call('ZCARD', KEYS[5])
local ignored = redis.call('HLEN', KEYS[4])
local missing = 0

for _, child_id in ipairs(parent['child_ids'] or {}) do
  local is_pending = pending_child_ids[child_id] == true
  local is_indexed =
    processed_child_ids[child_id] == true
    or ignored_child_ids[child_id] == true
    or unsuccessful_child_ids[child_id] == true
  local child_raw = redis.call('HGET', KEYS[1], child_id)
  if is_pending then
    if child_raw then
      unprocessed = unprocessed + 1
    else
      missing = missing + 1
    end
  elseif is_indexed then
    -- Parent-scoped dependency indexes are authoritative when present.
  elseif not child_raw then
    missing = missing + 1
  else
    local child = cjson.decode(child_raw)
    if child["state"] == "completed" then
      processed = processed + 1
    elseif child["state"] == "failed" then
      if child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true) then
        ignored = ignored + 1
      elseif child["options"] and child["options"] ~= cjson.null and child["options"]["remove_dependency_on_failure"] == true then
        -- Removed dependencies are intentionally omitted from dependency counts.
      else
        failed = failed + 1
      end
    elseif not has_dependency_set then
      unprocessed = unprocessed + 1
    end
  end
end

return {'ok', tostring(processed), tostring(unprocessed), tostring(failed), tostring(ignored), tostring(missing)}
"#;

const FLOW_DEPENDENCY_SELECTED_COUNTS_SCRIPT: &str = r#"
local parent_raw = redis.call('HGET', KEYS[1], ARGV[1])
if not parent_raw then
  return {'missing'}
end

local result = {'ok'}
for index = 2, #ARGV do
  local kind = ARGV[index]
  result[#result + 1] = kind
  if kind == 'processed' then
    result[#result + 1] = tostring(redis.call('HLEN', KEYS[3]))
  elseif kind == 'unprocessed' then
    result[#result + 1] = tostring(redis.call('SCARD', KEYS[2]))
  elseif kind == 'ignored' then
    result[#result + 1] = tostring(redis.call('HLEN', KEYS[4]))
  elseif kind == 'failed' then
    result[#result + 1] = tostring(redis.call('ZCARD', KEYS[5]))
  else
    return {'invalid_kind', kind}
  end
end

return result
"#;

const FLOW_DEPENDENCY_VALUES_SCRIPT: &str = r#"
local parent_raw = redis.call('HGET', KEYS[1], ARGV[1])
if not parent_raw then
  return {'missing'}
end

local parent = cjson.decode(parent_raw)
local result = {'ok'}
local has_dependency_set = redis.call('EXISTS', KEYS[2]) == 1
local pending_child_ids = {}
local processed_child_ids = {}
local ignored_child_ids = {}
local unsuccessful_child_ids = {}

local processed = redis.call('HGETALL', KEYS[3])
for index = 1, #processed, 2 do
  processed_child_ids[processed[index]] = true
end

local unprocessed = redis.call('SMEMBERS', KEYS[2])
for _, entry in ipairs(unprocessed) do
  pending_child_ids[entry] = true
end

local ignored = redis.call('HGETALL', KEYS[4])
for index = 1, #ignored, 2 do
  ignored_child_ids[ignored[index]] = true
end

local failed = redis.call('ZRANGE', KEYS[5], 0, -1)
for _, entry in ipairs(failed) do
  unsuccessful_child_ids[entry] = true
end

for _, child_id in ipairs(parent['child_ids'] or {}) do
  local is_pending = pending_child_ids[child_id] == true
  local is_indexed =
    processed_child_ids[child_id] == true
    or ignored_child_ids[child_id] == true
    or unsuccessful_child_ids[child_id] == true

  if not is_pending and not is_indexed then
    local child_raw = redis.call('HGET', KEYS[1], child_id)
    if child_raw then
      local child = cjson.decode(child_raw)
      if child["state"] == "completed" then
        local return_value = child["return_value"]
        if return_value == nil then
          return_value = cjson.null
        end
        processed[#processed + 1] = child_id
        processed[#processed + 1] = cjson.encode(return_value)
      elseif child["state"] == "failed" then
        if child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true) then
          ignored[#ignored + 1] = child_id
          ignored[#ignored + 1] = child["failed_reason"] or ""
        elseif child["options"] and child["options"] ~= cjson.null and child["options"]["remove_dependency_on_failure"] == true then
          -- Removed dependencies are intentionally omitted from dependency values.
        else
          failed[#failed + 1] = child_id
        end
      elseif not has_dependency_set then
        unprocessed[#unprocessed + 1] = child_id
      end
    end
  end
end

result[#result + 1] = tostring(#processed)
for _, entry in ipairs(processed) do
  result[#result + 1] = entry
end

result[#result + 1] = tostring(#unprocessed)
for _, entry in ipairs(unprocessed) do
  result[#result + 1] = entry
end

result[#result + 1] = tostring(#ignored)
for _, entry in ipairs(ignored) do
  result[#result + 1] = entry
end

result[#result + 1] = tostring(#failed)
for _, entry in ipairs(failed) do
  result[#result + 1] = entry
end

return result
"#;

const FLOW_DEPENDENCY_PAGE_SCRIPT: &str = r#"
local parent_raw = redis.call('HGET', KEYS[1], ARGV[1])
if not parent_raw then
  return {'missing'}
end

local kind = ARGV[2]
local cursor = tonumber(ARGV[3]) or 0
local count = tonumber(ARGV[4]) or 20
if count < 1 then
  count = 1
end

local parent = cjson.decode(parent_raw)

local function append_retained_dependency_fallback(entries, kind)
  if cursor ~= 0 then
    return entries
  end

  local has_dependency_set = redis.call('EXISTS', KEYS[2]) == 1
  for _, child_id in ipairs(parent['child_ids'] or {}) do
    local is_pending = redis.call('SISMEMBER', KEYS[2], child_id) == 1
    local is_indexed =
      is_pending
      or redis.call('HEXISTS', KEYS[3], child_id) == 1
      or redis.call('HEXISTS', KEYS[4], child_id) == 1
      or redis.call('ZSCORE', KEYS[5], child_id)

    if not is_indexed then
      local child_raw = redis.call('HGET', KEYS[1], child_id)
      if child_raw then
        local child = cjson.decode(child_raw)
        if kind == 'processed' and child["state"] == "completed" then
          local return_value = child["return_value"]
          if return_value == nil then
            return_value = cjson.null
          end
          entries[#entries + 1] = child_id
          entries[#entries + 1] = cjson.encode(return_value)
        elseif kind == 'unprocessed' and not has_dependency_set and child["state"] ~= "completed" and child["state"] ~= "failed" then
          entries[#entries + 1] = child_id
        elseif kind == 'ignored' and child["state"] == "failed" and child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true) then
          entries[#entries + 1] = child_id
          entries[#entries + 1] = child["failed_reason"] or ""
        elseif kind == 'failed' and child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true)) then
          entries[#entries + 1] = child_id
        end
      end
    end
  end

  return entries
end

local result = {'ok', kind}
if kind == 'processed' then
  local scanned = redis.call('HSCAN', KEYS[3], cursor, 'COUNT', count)
  result[#result + 1] = tostring(scanned[1])
  local entries = append_retained_dependency_fallback(scanned[2], kind)
  for _, entry in ipairs(entries) do
    result[#result + 1] = entry
  end
elseif kind == 'unprocessed' then
  local scanned = redis.call('SSCAN', KEYS[2], cursor, 'COUNT', count)
  result[#result + 1] = tostring(scanned[1])
  local entries = append_retained_dependency_fallback(scanned[2], kind)
  for _, entry in ipairs(entries) do
    result[#result + 1] = entry
  end
elseif kind == 'ignored' then
  local scanned = redis.call('HSCAN', KEYS[4], cursor, 'COUNT', count)
  result[#result + 1] = tostring(scanned[1])
  local entries = append_retained_dependency_fallback(scanned[2], kind)
  for _, entry in ipairs(entries) do
    result[#result + 1] = entry
  end
elseif kind == 'failed' then
  local total = redis.call('ZCARD', KEYS[5])
  local start = cursor
  if start < 0 then
    start = 0
  end

  local entries = {}
  if start < total then
    entries = redis.call('ZRANGE', KEYS[5], start, start + count - 1)
  end

  local next_cursor = 0
  if start + #entries < total then
    next_cursor = start + #entries
  end

  result[#result + 1] = tostring(next_cursor)
  entries = append_retained_dependency_fallback(entries, kind)
  for _, entry in ipairs(entries) do
    result[#result + 1] = entry
  end
else
  return {'invalid_kind', kind}
end

return result
"#;

const FLOW_DEPENDENCY_PAGES_SCRIPT: &str = r#"
local parent_raw = redis.call('HGET', KEYS[1], ARGV[1])
if not parent_raw then
  return {'missing'}
end

local result = {'ok'}
local request_count = math.floor((#ARGV - 1) / 3)
result[#result + 1] = tostring(request_count)
local parent = cjson.decode(parent_raw)

local function append_retained_dependency_fallback(entries, kind, cursor)
  if cursor ~= 0 then
    return entries
  end

  local has_dependency_set = redis.call('EXISTS', KEYS[2]) == 1
  for _, child_id in ipairs(parent['child_ids'] or {}) do
    local is_pending = redis.call('SISMEMBER', KEYS[2], child_id) == 1
    local is_indexed =
      is_pending
      or redis.call('HEXISTS', KEYS[3], child_id) == 1
      or redis.call('HEXISTS', KEYS[4], child_id) == 1
      or redis.call('ZSCORE', KEYS[5], child_id)

    if not is_indexed then
      local child_raw = redis.call('HGET', KEYS[1], child_id)
      if child_raw then
        local child = cjson.decode(child_raw)
        if kind == 'processed' and child["state"] == "completed" then
          local return_value = child["return_value"]
          if return_value == nil then
            return_value = cjson.null
          end
          entries[#entries + 1] = child_id
          entries[#entries + 1] = cjson.encode(return_value)
        elseif kind == 'unprocessed' and not has_dependency_set and child["state"] ~= "completed" and child["state"] ~= "failed" then
          entries[#entries + 1] = child_id
        elseif kind == 'ignored' and child["state"] == "failed" and child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true) then
          entries[#entries + 1] = child_id
          entries[#entries + 1] = child["failed_reason"] or ""
        elseif kind == 'failed' and child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true)) then
          entries[#entries + 1] = child_id
        end
      end
    end
  end

  return entries
end

local function append_scan_page(kind, next_cursor, entries, count)
  result[#result + 1] = kind
  result[#result + 1] = tostring(next_cursor)
  result[#result + 1] = tostring(count)
  result[#result + 1] = tostring(#entries)
  for _, entry in ipairs(entries) do
    result[#result + 1] = entry
  end
end

for request_index = 0, request_count - 1 do
  local base = 2 + (request_index * 3)
  local kind = ARGV[base]
  local cursor = tonumber(ARGV[base + 1]) or 0
  local count = tonumber(ARGV[base + 2]) or 20
  if count < 1 then
    count = 1
  end

  if kind == 'processed' then
    local scanned = redis.call('HSCAN', KEYS[3], cursor, 'COUNT', count)
    append_scan_page(kind, scanned[1], append_retained_dependency_fallback(scanned[2], kind, cursor), count)
  elseif kind == 'unprocessed' then
    local scanned = redis.call('SSCAN', KEYS[2], cursor, 'COUNT', count)
    append_scan_page(kind, scanned[1], append_retained_dependency_fallback(scanned[2], kind, cursor), count)
  elseif kind == 'ignored' then
    local scanned = redis.call('HSCAN', KEYS[4], cursor, 'COUNT', count)
    append_scan_page(kind, scanned[1], append_retained_dependency_fallback(scanned[2], kind, cursor), count)
  elseif kind == 'failed' then
    local total = redis.call('ZCARD', KEYS[5])
    local start = cursor
    if start < 0 then
      start = 0
    end

    local entries = {}
    if start < total then
      entries = redis.call('ZRANGE', KEYS[5], start, start + count - 1)
    end

    local next_cursor = 0
    if start + #entries < total then
      next_cursor = start + #entries
    end

    append_scan_page(kind, next_cursor, append_retained_dependency_fallback(entries, kind, cursor), count)
  else
    return {'invalid_kind', kind}
  end
end

return result
"#;

const FLOW_CHILDREN_VALUES_SCRIPT: &str = r#"
local parent_raw = redis.call('HGET', KEYS[1], ARGV[1])
if not parent_raw then
  return {'missing'}
end

local parent = cjson.decode(parent_raw)
local result = {'ok'}
local indexed_child_ids = {}

local processed_values = redis.call('HGETALL', KEYS[2])
for index = 1, #processed_values, 2 do
  local child_id = processed_values[index]
  indexed_child_ids[child_id] = true
  table.insert(result, child_id)
  table.insert(result, processed_values[index + 1])
end

for _, child_id in ipairs(parent['child_ids'] or {}) do
  if indexed_child_ids[child_id] ~= true then
    local child_raw = redis.call('HGET', KEYS[1], child_id)
    if child_raw then
      local child = cjson.decode(child_raw)
      if child["state"] == "completed" and child["return_value"] ~= nil then
        table.insert(result, child_id)
        table.insert(result, cjson.encode(child["return_value"]))
      end
    end
  end
end

return result
"#;

const FLOW_IGNORED_CHILDREN_FAILURES_SCRIPT: &str = r#"
local parent_raw = redis.call('HGET', KEYS[1], ARGV[1])
if not parent_raw then
  return {'missing'}
end

local parent = cjson.decode(parent_raw)
local result = {'ok'}
local indexed_child_ids = {}

local failed_values = redis.call('HGETALL', KEYS[2])
for index = 1, #failed_values, 2 do
  local child_id = failed_values[index]
  indexed_child_ids[child_id] = true
  table.insert(result, child_id)
  table.insert(result, failed_values[index + 1])
end

for _, child_id in ipairs(parent['child_ids'] or {}) do
  if indexed_child_ids[child_id] ~= true then
    local child_raw = redis.call('HGET', KEYS[1], child_id)
    if child_raw then
      local child = cjson.decode(child_raw)
      if child["state"] == "failed" and child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true) then
        table.insert(result, child_id)
        table.insert(result, child["failed_reason"] or "")
      end
    end
  end
end

return result
"#;

const REMOVE_UNPROCESSED_CHILDREN_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function days_from_civil(year, month, day)
  year = year - (month <= 2 and 1 or 0)
  local era = math.floor(year / 400)
  local yoe = year - era * 400
  local shifted_month = month
  if month > 2 then
    shifted_month = month - 3
  else
    shifted_month = month + 9
  end
  local doy = math.floor((153 * shifted_month + 2) / 5) + day - 1
  local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
  return era * 146097 + doe - 719468
end

local function iso_to_millis(value)
  local year = tonumber(string.sub(value, 1, 4))
  local month = tonumber(string.sub(value, 6, 7))
  local day = tonumber(string.sub(value, 9, 10))
  local hour = tonumber(string.sub(value, 12, 13))
  local minute = tonumber(string.sub(value, 15, 16))
  local second = tonumber(string.sub(value, 18, 19))
  if not year or not month or not day or not hour or not minute or not second then
    return 0
  end

  local millis = 0
  if string.sub(value, 20, 20) == "." then
    local digits = string.match(string.sub(value, 21), "^(%d+)")
    if digits then
      digits = string.sub(digits .. "000", 1, 3)
      millis = tonumber(digits) or 0
    end
  end

  local days = days_from_civil(year, month, day)
  return (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis
end

local function deduplication_id(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  local id = deduplication["id"]
  if not id or id == cjson.null or id == '' then
    return nil
  end
  return id
end

local function release_deduplication_key(job, job_id)
  local id = deduplication_id(job)
  if id then
    local key = ARGV[7] .. id
    if redis.call('GET', key) == job_id then
      redis.call('DEL', key)
      if ARGV[10] and ARGV[10] ~= '' then
        redis.call('DEL', ARGV[10] .. id)
      end
    end
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function release_repeat_key(job, job_id)
  local key = repeat_key(job)
  if key then
    local repeat_prefix = ARGV[8]
    local owner_key = repeat_prefix .. key
    local owner_matches = redis.call('GET', owner_key) == job_id
    if owner_matches then
      redis.call('DEL', owner_key)
    end

    local scheduler_meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
    local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
    if scheduler_owner_id == job_id or (owner_matches and not scheduler_owner_id) then
      redis.call('ZREM', repeat_scheduler_key(repeat_prefix), key)
      redis.call('DEL', scheduler_meta_key)
    end
  end
end

local function remove_state_indexes(job_id)
  for index = 2, 7 do
    redis.call('ZREM', KEYS[index], job_id)
  end
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function retention_options(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  if job["options"]["remove_on_fail"] == true then
    return { count = 0 }
  end
  local retention = job["options"]["failure_retention"]
  if retention and retention ~= cjson.null then
    return retention
  end
  return nil
end

local function retention_count(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local count = retention["count"]
  if count == nil or count == cjson.null then
    return nil
  end
  return tonumber(count)
end

local function retention_age_millis(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  return duration_millis(retention["age"])
end

local function retention_limit(retention)
  if not retention or retention == cjson.null then
    return 1000
  end
  local limit = tonumber(retention["limit"] or '1000') or 1000
  if limit <= 0 then
    return 1000
  end
  return limit
end

local function remove_finished_job(job_id)
  local removed_raw = redis.call('HGET', KEYS[1], job_id)
  if removed_raw then
    local removed = cjson.decode(removed_raw)
    release_deduplication_key(removed, job_id)
    release_repeat_key(removed, job_id)
  end
  remove_state_indexes(job_id)
  redis.call('HDEL', KEYS[1], job_id)
  redis.call(
    'DEL',
    ARGV[6] .. job_id,
    ARGV[6] .. job_id .. ':processed',
    ARGV[6] .. job_id .. ':failed',
    ARGV[6] .. job_id .. ':unsuccessful'
  )
  redis.call('DEL', ARGV[9] .. job_id)
end

local function remove_finished_jobs_by_max_age(timestamp, retention)
  local max_age = retention_age_millis(retention)
  if not max_age then
    return
  end
  local max_limit = retention_limit(retention)
  local start = tonumber(timestamp) - max_age
  local job_ids = redis.call('ZREVRANGEBYSCORE', KEYS[7], start, '-inf', 'LIMIT', 0, max_limit)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(job_id)
  end
  if #job_ids > 0 then
    if #job_ids < max_limit then
      redis.call('ZREMRANGEBYSCORE', KEYS[7], '-inf', start)
    else
      for _, job_id in ipairs(job_ids) do
        redis.call('ZREM', KEYS[7], job_id)
      end
    end
  end
end

local function remove_finished_jobs_by_max_count(max_count)
  if not max_count or max_count <= 0 then
    return
  end
  local job_ids = redis.call('ZREVRANGE', KEYS[7], max_count, -1)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(job_id)
  end
  redis.call('ZREMRANGEBYRANK', KEYS[7], 0, -(max_count + 1))
end

local function apply_failed_retention(timestamp, retention)
  if not retention or retention == cjson.null then
    return
  end
  remove_finished_jobs_by_max_age(timestamp, retention)
  remove_finished_jobs_by_max_count(retention_count(retention))
end

local function event_state_name(state)
  if state == 'waiting_children' then
    return 'waiting-children'
  end
  return state or ''
end

local function emit_removed_event(job_id, job)
  redis.call('XADD', KEYS[10], 'MAXLEN', '~', ARGV[11], '*', 'event', 'removed', 'jobId', job_id, 'prev', event_state_name(job["state"]))
end

local function remove_job_record(job_id, job)
  release_deduplication_key(job, job_id)
  release_repeat_key(job, job_id)
  remove_state_indexes(job_id)
  redis.call('HDEL', KEYS[1], job_id)
  redis.call('DEL', ARGV[5] .. job_id)
  redis.call(
    'DEL',
    ARGV[6] .. job_id,
    ARGV[6] .. job_id .. ':processed',
    ARGV[6] .. job_id .. ':failed',
    ARGV[6] .. job_id .. ':unsuccessful'
  )
  redis.call('DEL', ARGV[9] .. job_id)
end

local function dependency_members(dependency_key)
  local members = {}
  if redis.call('EXISTS', dependency_key) == 1 then
    for _, child_id in ipairs(redis.call('SMEMBERS', dependency_key)) do
      members[child_id] = true
    end
    return members
  end
  return nil
end

local removed = {'ok'}

local remove_unprocessed_children
remove_unprocessed_children = function(parent, dependency_key)
  local pending = dependency_members(dependency_key)
  for _, child_id in ipairs(parent["child_ids"] or {}) do
    if not pending or pending[child_id] then
      local child_raw = redis.call('HGET', KEYS[1], child_id)
      if not child_raw then
        redis.call('SREM', dependency_key, child_id)
      else
        local child = cjson.decode(child_raw)
        local state = child["state"]
        local locked = redis.call('GET', ARGV[5] .. child_id)
        if not locked and state ~= "active" and state ~= "completed" and state ~= "failed" then
          remove_unprocessed_children(child, ARGV[6] .. child_id)
          redis.call('SREM', dependency_key, child_id)
          remove_job_record(child_id, child)
          emit_removed_event(child_id, child)
          removed[#removed + 1] = child_raw
        end
      end
    end
  end
end

local function release_parent_if_ready(parent_id, parent)
  if parent["state"] ~= "waiting_children" then
    return
  end

  local all_done = true
  local failed_child_id = nil
  local failed_reason = nil

  if redis.call('SCARD', KEYS[9]) > 0 then
    all_done = false
  else
    for _, child_id in ipairs(parent["child_ids"] or {}) do
      local child_raw = redis.call('HGET', KEYS[1], child_id)
      if child_raw then
        local child = cjson.decode(child_raw)
        if child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true)) then
          failed_child_id = child_id
          failed_reason = child["failed_reason"]
          if not failed_reason or failed_reason == cjson.null then
            failed_reason = "unknown error"
          end
          break
        elseif child["state"] ~= "completed" and child["state"] ~= "failed" then
          all_done = false
          break
        end
      end
    end
  end

  if failed_child_id then
    redis.call('DEL', KEYS[9])
    redis.call('ZREM', KEYS[5], parent_id)
    parent["state"] = "failed"
    parent["finished_at"] = ARGV[2]
    parent["worker_id"] = cjson.null
    parent["lock_token"] = cjson.null
    parent["lease_expires_at"] = cjson.null
    parent["deferred_failure"] = cjson.null
    parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
    release_deduplication_key(parent, parent_id)
    release_repeat_key(parent, parent_id)
    local parent_failure_retention = retention_options(parent)
    local parent_failure_max_count = retention_count(parent_failure_retention)
    if parent_failure_max_count == 0 then
      remove_finished_job(parent_id)
    else
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[7], ARGV[3], parent_id)
      apply_failed_retention(ARGV[3], parent_failure_retention)
    end
  elseif all_done then
    redis.call('DEL', KEYS[9])
    redis.call('ZREM', KEYS[5], parent_id)
    parent["processed_at"] = cjson.null
    parent["finished_at"] = cjson.null
    parent["worker_id"] = cjson.null
    parent["lock_token"] = cjson.null
    parent["lease_expires_at"] = cjson.null
    parent["deferred_failure"] = cjson.null
    parent["failed_reason"] = cjson.null

    local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
    if parent_scheduled_millis <= tonumber(ARGV[3]) then
      parent["state"] = "waiting"
      local priority = tonumber(parent["priority"] or '1000') or 1000
      enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[8], parent, parent_id, priority, ARGV[4], KEYS[#KEYS])
    else
      parent["state"] = "delayed"
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[3], parent_scheduled_millis, parent_id)
      refresh_delay_marker(KEYS[#KEYS], KEYS[3])
    end
  end
end

local parent_raw = redis.call('HGET', KEYS[1], ARGV[1])
if not parent_raw then
  redis.call('DEL', KEYS[9])
  return {'missing'}
end

local parent = cjson.decode(parent_raw)
remove_unprocessed_children(parent, KEYS[9])
local updated_parent_raw = redis.call('HGET', KEYS[1], ARGV[1])
if updated_parent_raw then
  release_parent_if_ready(ARGV[1], cjson.decode(updated_parent_raw))
end

return removed
"#;

const REMOVE_CHILD_DEPENDENCY_SCRIPT: &str = r#"
local function waiting_score_for(priority, sequence, job, bucket)
  local bucket_value = tonumber(bucket)
  local half_bucket = math.floor(bucket_value / 2)
  local sequence_index = sequence % half_bucket
  if job["options"] and job["options"] ~= cjson.null and job["options"]["lifo"] == true then
    return (priority * bucket_value) + (half_bucket - 1 - sequence_index)
  end
  return (priority * bucket_value) + half_bucket + sequence_index
end

local function enqueue_waiting_job(jobs_key, waiting_key, sequence_key, job, job_id, priority, bucket, marker_key)
  local sequence = redis.call('INCR', sequence_key)
  job["enqueued_seq"] = sequence
  local waiting_score = waiting_score_for(priority, sequence, job, bucket)
  local updated = cjson.encode(job)
  redis.call('HSET', jobs_key, job_id, updated)
  redis.call('ZADD', waiting_key, waiting_score, job_id)
  if marker_key and marker_key ~= '' then
    redis.call('ZADD', marker_key, 0, '0')
  end
  return updated
end

local function refresh_delay_marker(marker_key, delayed_key)
  if not marker_key or marker_key == '' then
    return
  end
  local next_delayed = redis.call('ZRANGE', delayed_key, 0, 0, 'WITHSCORES')
  if next_delayed[2] then
    redis.call('ZADD', marker_key, next_delayed[2], '1')
  else
    redis.call('ZREM', marker_key, '1')
  end
end

local function days_from_civil(year, month, day)
  year = year - (month <= 2 and 1 or 0)
  local era = math.floor(year / 400)
  local yoe = year - era * 400
  local shifted_month = month
  if month > 2 then
    shifted_month = month - 3
  else
    shifted_month = month + 9
  end
  local doy = math.floor((153 * shifted_month + 2) / 5) + day - 1
  local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
  return era * 146097 + doe - 719468
end

local function iso_to_millis(value)
  local year = tonumber(string.sub(value, 1, 4))
  local month = tonumber(string.sub(value, 6, 7))
  local day = tonumber(string.sub(value, 9, 10))
  local hour = tonumber(string.sub(value, 12, 13))
  local minute = tonumber(string.sub(value, 15, 16))
  local second = tonumber(string.sub(value, 18, 19))
  if not year or not month or not day or not hour or not minute or not second then
    return 0
  end

  local millis = 0
  if string.sub(value, 20, 20) == "." then
    local digits = string.match(string.sub(value, 21), "^(%d+)")
    if digits then
      digits = string.sub(digits .. "000", 1, 3)
      millis = tonumber(digits) or 0
    end
  end

  local days = days_from_civil(year, month, day)
  return (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis
end

local function deduplication_id(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  local deduplication = job["options"]["deduplication"]
  if not deduplication or deduplication == cjson.null then
    return nil
  end
  local id = deduplication["id"]
  if not id or id == cjson.null or id == '' then
    return nil
  end
  return id
end

local function release_deduplication_key(job, job_id)
  local id = deduplication_id(job)
  if id then
    local key = ARGV[6] .. id
    if redis.call('GET', key) == job_id then
      redis.call('DEL', key)
      if ARGV[9] and ARGV[9] ~= '' then
        redis.call('DEL', ARGV[9] .. id)
      end
    end
  end
end

local function repeat_key(job)
  local key = job["repeat_key"]
  if not key or key == cjson.null or key == '' then
    return nil
  end
  return key
end

local function repeat_scheduler_key(repeat_prefix)
  return string.sub(repeat_prefix, 1, -2)
end

local function repeat_scheduler_meta_key(repeat_prefix, repeat_key_value)
  return string.sub(repeat_prefix, 1, -8) .. 'repeat_meta:' .. repeat_key_value
end

local function release_repeat_key(job, job_id)
  local key = repeat_key(job)
  if key then
    local repeat_prefix = ARGV[7]
    local owner_key = repeat_prefix .. key
    local owner_matches = redis.call('GET', owner_key) == job_id
    if owner_matches then
      redis.call('DEL', owner_key)
    end

    local scheduler_meta_key = repeat_scheduler_meta_key(repeat_prefix, key)
    local scheduler_owner_id = redis.call('HGET', scheduler_meta_key, 'jid')
    if scheduler_owner_id == job_id or (owner_matches and not scheduler_owner_id) then
      redis.call('ZREM', repeat_scheduler_key(repeat_prefix), key)
      redis.call('DEL', scheduler_meta_key)
    end
  end
end

local function remove_state_indexes(job_id)
  for index = 2, 7 do
    redis.call('ZREM', KEYS[index], job_id)
  end
end

local function duration_millis(duration)
  if not duration or duration == cjson.null then
    return nil
  end
  if type(duration) == 'number' then
    return math.floor(duration)
  end
  local secs = tonumber(duration["secs"] or duration["seconds"] or 0) or 0
  local nanos = tonumber(duration["nanos"] or duration["subsec_nanos"] or 0) or 0
  local millis = (secs * 1000) + math.floor(nanos / 1000000)
  if millis <= 0 then
    return nil
  end
  return millis
end

local function retention_options(job)
  if not job["options"] or job["options"] == cjson.null then
    return nil
  end
  if job["options"]["remove_on_fail"] == true then
    return { count = 0 }
  end
  local retention = job["options"]["failure_retention"]
  if retention and retention ~= cjson.null then
    return retention
  end
  return nil
end

local function retention_count(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  local count = retention["count"]
  if count == nil or count == cjson.null then
    return nil
  end
  return tonumber(count)
end

local function retention_age_millis(retention)
  if not retention or retention == cjson.null then
    return nil
  end
  return duration_millis(retention["age"])
end

local function retention_limit(retention)
  if not retention or retention == cjson.null then
    return 1000
  end
  local limit = tonumber(retention["limit"] or '1000') or 1000
  if limit <= 0 then
    return 1000
  end
  return limit
end

local function remove_finished_job(job_id)
  local removed_raw = redis.call('HGET', KEYS[1], job_id)
  if removed_raw then
    local removed = cjson.decode(removed_raw)
    release_deduplication_key(removed, job_id)
    release_repeat_key(removed, job_id)
  end
  remove_state_indexes(job_id)
  redis.call('HDEL', KEYS[1], job_id)
  redis.call(
    'DEL',
    ARGV[5] .. job_id,
    ARGV[5] .. job_id .. ':processed',
    ARGV[5] .. job_id .. ':failed',
    ARGV[5] .. job_id .. ':unsuccessful'
  )
  redis.call('DEL', ARGV[8] .. job_id)
end

local function remove_finished_jobs_by_max_age(timestamp, retention)
  local max_age = retention_age_millis(retention)
  if not max_age then
    return
  end
  local max_limit = retention_limit(retention)
  local start = tonumber(timestamp) - max_age
  local job_ids = redis.call('ZREVRANGEBYSCORE', KEYS[7], start, '-inf', 'LIMIT', 0, max_limit)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(job_id)
  end
  if #job_ids > 0 then
    if #job_ids < max_limit then
      redis.call('ZREMRANGEBYSCORE', KEYS[7], '-inf', start)
    else
      for _, job_id in ipairs(job_ids) do
        redis.call('ZREM', KEYS[7], job_id)
      end
    end
  end
end

local function remove_finished_jobs_by_max_count(max_count)
  if not max_count or max_count <= 0 then
    return
  end
  local job_ids = redis.call('ZREVRANGE', KEYS[7], max_count, -1)
  for _, job_id in ipairs(job_ids) do
    remove_finished_job(job_id)
  end
  redis.call('ZREMRANGEBYRANK', KEYS[7], 0, -(max_count + 1))
end

local function apply_failed_retention(timestamp, retention)
  if not retention or retention == cjson.null then
    return
  end
  remove_finished_jobs_by_max_age(timestamp, retention)
  remove_finished_jobs_by_max_count(retention_count(retention))
end

local function release_parent_if_ready(parent_id, parent, dependency_key)
  if parent["state"] ~= "waiting_children" then
    redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
    return
  end

  local all_done = true
  local failed_child_id = nil
  local failed_reason = nil

  if redis.call('SCARD', dependency_key) > 0 then
    all_done = false
  else
    for _, child_id in ipairs(parent["child_ids"] or {}) do
      local child_raw = redis.call('HGET', KEYS[1], child_id)
      if child_raw then
        local child = cjson.decode(child_raw)
        if child["state"] == "failed" and not (child["options"] and child["options"] ~= cjson.null and (child["options"]["ignore_dependency_on_failure"] == true or child["options"]["remove_dependency_on_failure"] == true or child["options"]["continue_parent_on_failure"] == true)) then
          failed_child_id = child_id
          failed_reason = child["failed_reason"]
          if not failed_reason or failed_reason == cjson.null then
            failed_reason = "unknown error"
          end
          break
        elseif child["state"] ~= "completed" and child["state"] ~= "failed" then
          all_done = false
          break
        end
      end
    end
  end

  if failed_child_id then
    redis.call('DEL', dependency_key)
    redis.call('ZREM', KEYS[5], parent_id)
    parent["state"] = "failed"
    parent["finished_at"] = ARGV[2]
    parent["worker_id"] = cjson.null
    parent["lock_token"] = cjson.null
    parent["lease_expires_at"] = cjson.null
    parent["deferred_failure"] = cjson.null
    parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
    release_deduplication_key(parent, parent_id)
    release_repeat_key(parent, parent_id)
    local parent_failure_retention = retention_options(parent)
    local parent_failure_max_count = retention_count(parent_failure_retention)
    if parent_failure_max_count == 0 then
      remove_finished_job(parent_id)
    else
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[7], ARGV[3], parent_id)
      apply_failed_retention(ARGV[3], parent_failure_retention)
    end
  elseif all_done then
    redis.call('DEL', dependency_key)
    redis.call('ZREM', KEYS[5], parent_id)
    parent["processed_at"] = cjson.null
    parent["finished_at"] = cjson.null
    parent["worker_id"] = cjson.null
    parent["lock_token"] = cjson.null
    parent["lease_expires_at"] = cjson.null
    parent["deferred_failure"] = cjson.null
    parent["failed_reason"] = cjson.null

    local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
    if parent_scheduled_millis <= tonumber(ARGV[3]) then
      parent["state"] = "waiting"
      local priority = tonumber(parent["priority"] or '1000') or 1000
      enqueue_waiting_job(KEYS[1], KEYS[2], KEYS[8], parent, parent_id, priority, ARGV[4], KEYS[#KEYS])
    else
      parent["state"] = "delayed"
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[3], parent_scheduled_millis, parent_id)
      refresh_delay_marker(KEYS[#KEYS], KEYS[3])
    end
  else
    redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
  end
end

local child_raw = redis.call('HGET', KEYS[1], ARGV[1])
if not child_raw then
  return {'missing_child'}
end

local child = cjson.decode(child_raw)
local parent_id = child["parent_id"]
if not parent_id or parent_id == cjson.null or parent_id == '' then
  return {'no_relationship'}
end

local parent_raw = redis.call('HGET', KEYS[1], parent_id)
if not parent_raw then
  return {'missing_parent', parent_id}
end

local parent = cjson.decode(parent_raw)
local dependency_key = ARGV[5] .. parent_id
local dependency_removed = redis.call('SREM', dependency_key, ARGV[1]) == 1
local child_ids = {}
local child_id_removed = false
for _, child_id in ipairs(parent["child_ids"] or {}) do
  if child_id == ARGV[1] then
    child_id_removed = true
  else
    child_ids[#child_ids + 1] = child_id
  end
end

local processed_removed = redis.call('HDEL', dependency_key .. ':processed', ARGV[1]) == 1
local failed_removed = redis.call('HDEL', dependency_key .. ':failed', ARGV[1]) == 1
local unsuccessful_removed = redis.call('ZREM', dependency_key .. ':unsuccessful', ARGV[1]) == 1

if not dependency_removed
  and not child_id_removed
  and not processed_removed
  and not failed_removed
  and not unsuccessful_removed then
  return {'no_relationship'}
end

parent["child_ids"] = child_ids
child["parent_id"] = cjson.null
redis.call('HSET', KEYS[1], ARGV[1], cjson.encode(child))
release_parent_if_ready(parent_id, parent, dependency_key)

return {'ok'}
"#;

const FLOW_DEPENDENCIES_SCRIPT: &str = r#"
local parent_raw = redis.call('HGET', KEYS[1], ARGV[1])
if not parent_raw then
  return {'missing'}
end

local parent = cjson.decode(parent_raw)
local result = {'ok', parent_raw}
for _, child_id in ipairs(parent['child_ids'] or {}) do
  local child_raw = redis.call('HGET', KEYS[1], child_id)
  table.insert(result, child_id)
  if child_raw then
    table.insert(result, child_raw)
  else
    table.insert(result, '')
  end
end

return result
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
    claim_rate_limit: Option<JobRateLimit>,
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
            claim_rate_limit: None,
        })
    }

    /// Configure a worker-local claim rate limit with a Redis-backed counter.
    ///
    /// Every worker that uses the same namespace and queue shares the counter
    /// key, but each worker must be configured with the same limit. Use
    /// [`set_claim_rate_limit`](Self::set_claim_rate_limit) for Redis-shared
    /// limit configuration.
    pub fn with_claim_rate_limit(mut self, rate_limit: JobRateLimit) -> Result<Self> {
        rate_limit.validate()?;
        self.claim_rate_limit = Some(rate_limit);
        Ok(self)
    }

    /// Set a Redis-backed queue-level claim rate limit.
    ///
    /// Redis stores this value in the queue meta hash as `max` and `duration`,
    /// matching BullMQ's global rate-limit mechanism.
    pub async fn set_claim_rate_limit(&self, rate_limit: JobRateLimit) -> Result<()> {
        rate_limit.validate()?;
        let mut conn = self.connection().await?;
        let duration = duration_millis(rate_limit.window).max(1);
        redis::pipe()
            .hset(self.meta_key(), "max", rate_limit.max_claims)
            .hset(self.meta_key(), "duration", duration)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)
    }

    /// Read the Redis-backed queue-level claim rate limit.
    pub async fn get_claim_rate_limit(&self) -> Result<Option<JobRateLimit>> {
        let mut conn = self.connection().await?;
        let values: (Option<u64>, Option<u64>) = conn
            .hmget(self.meta_key(), &["max", "duration"])
            .await
            .map_err(redis_error)?;
        let (Some(max_claims), Some(duration)) = values else {
            return Ok(None);
        };
        let rate_limit = JobRateLimit::new(max_claims, Duration::from_millis(duration));
        rate_limit.validate()?;
        Ok(Some(rate_limit))
    }

    /// Return the current claim rate-limit TTL in milliseconds.
    ///
    /// When `max_claims` is provided, the TTL is returned only if the limiter
    /// counter has reached that threshold. When it is omitted, Redis-shared
    /// `meta.max` is used if present; otherwise this returns the raw `PTTL` for
    /// the limiter key.
    pub async fn get_claim_rate_limit_ttl(&self, max_claims: Option<u64>) -> Result<i64> {
        let mut conn = self.connection().await?;
        redis::cmd("EVAL")
            .arg(CLAIM_RATE_LIMIT_TTL_SCRIPT)
            .arg(2)
            .arg(self.claim_rate_limit_key())
            .arg(self.meta_key())
            .arg(max_claims.unwrap_or_default())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)
    }

    /// Override the claim rate limiter key for a fixed duration.
    ///
    /// This mirrors BullMQ's `rateLimit()` mechanism by setting the limiter key
    /// to a very large counter value with a millisecond TTL. The limiter blocks
    /// claims when a worker-local or Redis-shared max is configured.
    pub async fn rate_limit_claims_for(&self, duration: Duration) -> Result<()> {
        if duration.is_zero() {
            return Err(LaneError::ConfigError(
                "claim rate-limit duration must be greater than zero".to_string(),
            ));
        }
        let mut conn = self.connection().await?;
        redis::cmd("SET")
            .arg(self.claim_rate_limit_key())
            .arg(u64::MAX)
            .arg("PX")
            .arg(duration_millis(duration).max(1))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)
    }

    /// Remove the claim rate limiter key.
    pub async fn clear_claim_rate_limit_key(&self) -> Result<()> {
        let mut conn = self.connection().await?;
        conn.del(self.claim_rate_limit_key())
            .await
            .map_err(redis_error)
    }

    /// Clear the Redis-backed queue-level claim rate limit.
    pub async fn clear_claim_rate_limit(&self) -> Result<()> {
        let mut conn = self.connection().await?;
        conn.hdel(self.meta_key(), &["max", "duration"])
            .await
            .map_err(redis_error)
    }

    /// Set a Redis-backed queue-level active job limit.
    ///
    /// The limit is shared by every worker using the same namespace and queue.
    /// When the active set reaches this value, `claim_next` returns `None`
    /// until jobs complete, fail, or stalled recovery requeues them.
    ///
    /// Redis stores this value in the queue meta hash as `concurrency`, matching
    /// BullMQ's queue-maxed check while Lane keeps a Rust-native method name.
    pub async fn set_max_active_jobs(&self, max_active_jobs: usize) -> Result<()> {
        if max_active_jobs == 0 {
            return Err(LaneError::ConfigError(
                "max active jobs must be greater than zero".to_string(),
            ));
        }
        let mut conn = self.connection().await?;
        conn.hset(self.meta_key(), "concurrency", max_active_jobs)
            .await
            .map_err(redis_error)
    }

    /// Read the Redis-backed active job limit.
    ///
    /// Redis stores this value in the queue meta hash as `concurrency`, matching
    /// BullMQ's global concurrency getter.
    pub async fn get_max_active_jobs(&self) -> Result<Option<usize>> {
        let mut conn = self.connection().await?;
        conn.hget(self.meta_key(), "concurrency")
            .await
            .map_err(redis_error)
    }

    /// Return whether the Redis-shared active job limit is currently reached.
    ///
    /// This mirrors BullMQ's `isMaxed()` / queue-maxed check: Redis reads
    /// `meta.concurrency` and the active sorted-set cardinality inside one Lua
    /// turn. Missing or non-positive concurrency means the queue is not maxed.
    pub async fn is_maxed(&self) -> Result<bool> {
        let mut conn = self.connection().await?;
        let maxed: i64 = redis::cmd("EVAL")
            .arg(IS_MAXED_SCRIPT)
            .arg(2)
            .arg(self.meta_key())
            .arg(self.state_key(JobState::Active))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        Ok(maxed != 0)
    }

    /// Clear the Redis-backed active job limit.
    pub async fn clear_max_active_jobs(&self) -> Result<()> {
        let mut conn = self.connection().await?;
        conn.hdel(self.meta_key(), "concurrency")
            .await
            .map_err(redis_error)
    }

    /// Claim the next job, waiting on the Redis worker marker zset when idle.
    ///
    /// This mirrors BullMQ's worker wait path: `BZPOPMIN` is used only as a
    /// wake-up signal, and the actual ownership transition still goes through
    /// Lane's Lua claim script so pause, rate limit, concurrency, delay
    /// promotion, and lock creation remain atomic.
    pub async fn claim_next_blocking(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        block_for: Duration,
    ) -> Result<Option<Job>> {
        let deadline = Instant::now() + block_for;
        let mut claim_conn = self.connection().await?;
        let mut blocking_conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(redis_error)?;
        let mut block_until_millis = None;
        let mut first_attempt = true;

        loop {
            if !first_attempt
                && (block_for.is_zero()
                    || match deadline.checked_duration_since(Instant::now()) {
                        Some(remaining) => remaining.is_zero(),
                        None => true,
                    })
            {
                return Ok(None);
            }
            first_attempt = false;

            let now = Utc::now();
            if let Some(job) = self
                .claim_next_with_conn(&mut claim_conn, &worker_id, lease_for, now)
                .await?
            {
                return Ok(Some(job));
            }

            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(None);
            };
            if block_for.is_zero() || remaining.is_zero() {
                return Ok(None);
            }

            let rate_limit_ttl = self
                .active_claim_rate_limit_ttl_with_conn(&mut claim_conn)
                .await?;
            if rate_limit_ttl > 0 {
                let wait_for = Duration::from_millis(rate_limit_ttl as u64)
                    .min(remaining)
                    .min(MAX_MARKER_BLOCK_TIMEOUT);
                if wait_for.is_zero() {
                    continue;
                }
                tokio::time::sleep(wait_for).await;
                continue;
            }

            let wait_for = marker_block_timeout(block_until_millis, remaining, Utc::now());
            if wait_for.is_zero() {
                block_until_millis = None;
                continue;
            }

            let marker: Option<(String, String, f64)> = blocking_conn
                .bzpopmin(self.marker_key(), wait_for.as_secs_f64())
                .await
                .map_err(redis_error)?;
            if let Some((_key, member, score)) = marker {
                block_until_millis = if member == "1" {
                    Some(score.ceil() as i64)
                } else {
                    None
                };
            }
        }
    }

    async fn active_claim_rate_limit_ttl_with_conn(
        &self,
        conn: &mut ConnectionManager,
    ) -> Result<i64> {
        let rate_limit_max = self
            .claim_rate_limit
            .as_ref()
            .map(|limit| limit.max_claims)
            .unwrap_or_default();
        redis::cmd("EVAL")
            .arg(ACTIVE_CLAIM_RATE_LIMIT_TTL_SCRIPT)
            .arg(2)
            .arg(self.claim_rate_limit_key())
            .arg(self.meta_key())
            .arg(rate_limit_max)
            .query_async(conn)
            .await
            .map_err(redis_error)
    }

    /// Queue name.
    pub fn queue_name(&self) -> &str {
        &self.queue
    }

    /// Redis key namespace.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Add a parent-child flow using the current wall-clock time.
    pub async fn add_flow(&self, parent: JobSpec, children: Vec<JobSpec>) -> Result<JobFlow> {
        self.add_flow_at(parent, children, Utc::now()).await
    }

    /// Add multiple jobs using the current wall-clock time.
    pub async fn add_many(&self, jobs: Vec<JobSpec>) -> Result<Vec<Job>> {
        self.add_many_at(jobs, Utc::now()).await
    }

    /// List current non-terminal repeat series owners.
    pub async fn list_repeats(&self) -> Result<Vec<JobRepeatEntry>> {
        let mut conn = self.connection().await?;
        let mut repeats = Vec::new();
        let scheduler_keys: Vec<String> = redis::cmd("ZRANGE")
            .arg(self.repeat_schedulers_key())
            .arg(0)
            .arg(-1)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;

        let mut seen_repeat_keys = HashSet::new();
        for repeat_key in scheduler_keys {
            if seen_repeat_keys.insert(repeat_key.clone()) {
                if let Some(entry) = self.load_repeat_entry(&mut conn, &repeat_key).await? {
                    repeats.push(entry);
                }
            }
        }

        let prefix = self.repeat_key_prefix();
        let pattern = format!("{prefix}*");
        let mut cursor = 0_u64;

        loop {
            let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(100)
                .query_async(&mut conn)
                .await
                .map_err(redis_error)?;

            for owner_key in keys {
                let Some(repeat_key) = owner_key.strip_prefix(&prefix) else {
                    continue;
                };
                if repeat_key.is_empty() || !seen_repeat_keys.insert(repeat_key.to_string()) {
                    continue;
                }
                if let Some(entry) = self.load_repeat_entry(&mut conn, repeat_key).await? {
                    repeats.push(entry);
                }
            }

            if next_cursor == 0 {
                break;
            }
            cursor = next_cursor;
        }

        repeats.sort_by(|a, b| a.key.cmp(&b.key).then_with(|| a.job_id.cmp(&b.job_id)));
        Ok(repeats)
    }

    /// Return one repeat series / job scheduler by key.
    pub async fn get_repeat(&self, repeat_key: &str) -> Result<Option<JobRepeatEntry>> {
        let mut conn = self.connection().await?;
        self.load_repeat_entry(&mut conn, repeat_key).await
    }

    /// Return the number of current repeat series / job schedulers.
    pub async fn count_repeats(&self) -> Result<usize> {
        Ok(self.list_repeats().await?.len())
    }

    /// Return repeat series / job schedulers ordered by next scheduled time.
    pub async fn list_repeats_page(&self, options: JobRepeatListOptions) -> Result<JobRepeatPage> {
        Ok(page_repeat_entries(self.list_repeats().await?, options))
    }

    /// Create or replace the current non-active occurrence for a repeat series.
    pub async fn upsert_repeat(&self, spec: JobSpec, now: DateTime<Utc>) -> Result<Job> {
        validate_job_options_at(&spec.options, now)?;
        if spec.options.repeat.is_none() {
            return Err(LaneError::ConfigError(
                "repeat options are required to upsert a repeat series".to_string(),
            ));
        }

        let job = Job::new(
            self.queue.clone(),
            spec.name,
            spec.payload,
            spec.options,
            now,
        );
        let repeat_key = job_repeat_key(&job)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| LaneError::ConfigError("repeat key must not be empty".to_string()))?
            .to_string();
        let encoded = encode_job(&job)?;

        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(UPSERT_REPEAT_SCRIPT)
            .arg(10)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.sequence_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.events_key())
            .arg(self.marker_key())
            .arg(&job.id)
            .arg(encoded)
            .arg(job_state_name(job.state))
            .arg(millis(job.scheduled_at))
            .arg(job.priority)
            .arg(WAITING_SCORE_BUCKET)
            .arg(job_deduplication_id(&job).unwrap_or(""))
            .arg(self.deduplication_key_prefix())
            .arg(&repeat_key)
            .arg(self.repeat_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .arg(self.dependencies_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(self.lock_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_upsert_repeat_result(&result, &repeat_key, &job.id)
    }

    /// Add multiple jobs at an explicit timestamp.
    pub async fn add_many_at(&self, jobs: Vec<JobSpec>, now: DateTime<Utc>) -> Result<Vec<Job>> {
        for spec in &jobs {
            validate_job_options_at(&spec.options, now)?;
        }
        let created = jobs
            .into_iter()
            .map(|spec| {
                Job::new(
                    self.queue.clone(),
                    spec.name,
                    spec.payload,
                    spec.options,
                    now,
                )
            })
            .collect::<Vec<_>>();

        if created.is_empty() {
            return Ok(Vec::new());
        }

        let mut conn = self.connection().await?;
        let mut command = redis::cmd("EVAL");
        command
            .arg(ADD_JOBS_SCRIPT)
            .arg(8)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.sequence_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.events_key())
            .arg(self.marker_key())
            .arg(created.len());
        for job in &created {
            command
                .arg(&job.id)
                .arg(encode_job(job)?)
                .arg(job_state_name(job.state))
                .arg(millis(job.scheduled_at))
                .arg(job.priority)
                .arg(job_deduplication_id(job).unwrap_or(""))
                .arg(job_repeat_key(job).unwrap_or(""));
        }
        command
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION);

        let result: Vec<String> = command.query_async(&mut conn).await.map_err(redis_error)?;
        let added = decode_add_jobs_result(&result, created.len())?;

        Ok(added)
    }

    /// Add a parent-child flow at an explicit timestamp.
    pub async fn add_flow_at(
        &self,
        parent: JobSpec,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<JobFlow> {
        validate_job_options_at(&parent.options, now)?;
        for child in &children {
            validate_job_options_at(&child.options, now)?;
        }
        let mut parent_job = Job::new(
            self.queue.clone(),
            parent.name,
            parent.payload,
            parent.options,
            now,
        );
        parent_job.state = if children.is_empty() {
            state_after_dependencies(parent_job.scheduled_at, now)
        } else {
            JobState::WaitingChildren
        };

        let mut child_jobs = Vec::with_capacity(children.len());
        for child in children {
            let mut child_job = Job::new(
                self.queue.clone(),
                child.name,
                child.payload,
                child.options,
                now,
            );
            child_job.parent_id = Some(parent_job.id.clone());
            parent_job.child_ids.push(child_job.id.clone());
            child_jobs.push(child_job);
        }
        validate_flow_job_ids(&parent_job, &child_jobs)?;

        let mut conn = self.connection().await?;
        let add_result = self
            .add_flow_jobs(&mut conn, &parent_job, &child_jobs)
            .await?;
        if let Some(existing_parent_id) = add_result.existing_parent_id {
            return self
                .load_flow_for_parent(&mut conn, &existing_parent_id)
                .await;
        }
        let parent = self
            .load_job(&mut conn, &parent_job.id)
            .await?
            .unwrap_or(parent_job);
        let mut children = Vec::with_capacity(child_jobs.len());
        for child in child_jobs {
            if add_result.skipped_child_ids.contains(&child.id) {
                continue;
            }
            children.push(self.load_job(&mut conn, &child.id).await?.unwrap_or(child));
        }

        Ok(JobFlow { parent, children })
    }

    /// Add children to an active flow parent using the current wall-clock time.
    pub async fn add_flow_children(
        &self,
        parent_id: &str,
        lock_token: &str,
        children: Vec<JobSpec>,
    ) -> Result<Vec<Job>> {
        self.add_flow_children_at(parent_id, lock_token, children, Utc::now())
            .await
    }

    /// Add children to an active flow parent and move the parent to waiting-children.
    pub async fn add_flow_children_at(
        &self,
        parent_id: &str,
        lock_token: &str,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>> {
        if children.is_empty() {
            return Err(LaneError::ConfigError(
                "flow children cannot be empty".to_string(),
            ));
        }
        for child in &children {
            validate_job_options_at(&child.options, now)?;
        }

        let mut child_jobs = Vec::with_capacity(children.len());
        for child in children {
            let mut child_job = Job::new(
                self.queue.clone(),
                child.name,
                child.payload,
                child.options,
                now,
            );
            child_job.parent_id = Some(parent_id.to_string());
            child_jobs.push(child_job);
        }
        validate_flow_child_jobs(parent_id, &child_jobs)?;

        let mut conn = self.connection().await?;
        let skipped_child_ids = self
            .add_flow_children_jobs(&mut conn, parent_id, lock_token, &child_jobs, now)
            .await?;

        let mut children = Vec::with_capacity(child_jobs.len());
        for child in child_jobs {
            if skipped_child_ids.contains(&child.id) {
                continue;
            }
            children.push(self.load_job(&mut conn, &child.id).await?.unwrap_or(child));
        }
        Ok(children)
    }

    /// Return a parent flow's current child dependency snapshot.
    pub async fn get_flow_dependencies(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencies>> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(FLOW_DEPENDENCIES_SCRIPT)
            .arg(1)
            .arg(self.jobs_key())
            .arg(parent_id)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_flow_dependencies_result(&result, parent_id)
    }

    /// Return a parent flow's dependency counts.
    pub async fn get_flow_dependency_counts(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyCounts>> {
        let mut conn = self.connection().await?;
        let dependency_key = self.dependencies_key(parent_id);
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(FLOW_DEPENDENCY_COUNTS_SCRIPT)
            .arg(5)
            .arg(self.jobs_key())
            .arg(&dependency_key)
            .arg(format!("{dependency_key}:processed"))
            .arg(format!("{dependency_key}:failed"))
            .arg(format!("{dependency_key}:unsuccessful"))
            .arg(parent_id)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_flow_dependency_counts_result(&result, parent_id)
    }

    /// Return selected BullMQ-style dependency counts for a flow parent.
    pub async fn get_flow_dependency_selected_counts(
        &self,
        parent_id: &str,
        options: JobFlowDependencyCountOptions,
    ) -> Result<Option<JobFlowDependencySelectedCounts>> {
        let mut conn = self.connection().await?;
        let dependency_key = self.dependencies_key(parent_id);
        let mut command = redis::cmd("EVAL");
        command
            .arg(FLOW_DEPENDENCY_SELECTED_COUNTS_SCRIPT)
            .arg(5)
            .arg(self.jobs_key())
            .arg(&dependency_key)
            .arg(format!("{dependency_key}:processed"))
            .arg(format!("{dependency_key}:failed"))
            .arg(format!("{dependency_key}:unsuccessful"))
            .arg(parent_id);
        for kind in options.selected() {
            command.arg(kind.as_str());
        }
        let result: Vec<String> = command.query_async(&mut conn).await.map_err(redis_error)?;
        decode_flow_dependency_selected_counts_result(&result, parent_id)
    }

    /// Return BullMQ-style full dependency buckets for a flow parent.
    pub async fn get_flow_dependency_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyValues>> {
        let mut conn = self.connection().await?;
        let dependency_key = self.dependencies_key(parent_id);
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(FLOW_DEPENDENCY_VALUES_SCRIPT)
            .arg(5)
            .arg(self.jobs_key())
            .arg(&dependency_key)
            .arg(format!("{dependency_key}:processed"))
            .arg(format!("{dependency_key}:failed"))
            .arg(format!("{dependency_key}:unsuccessful"))
            .arg(parent_id)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_flow_dependency_values_result(&result, parent_id)
    }

    /// Return one cursor page from a parent flow dependency bucket.
    pub async fn get_flow_dependency_page(
        &self,
        parent_id: &str,
        options: JobFlowDependencyPageOptions,
    ) -> Result<Option<JobFlowDependencyPage>> {
        let kind = options.kind;
        let count = options.count.max(1);
        let mut conn = self.connection().await?;
        let dependency_key = self.dependencies_key(parent_id);
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(FLOW_DEPENDENCY_PAGE_SCRIPT)
            .arg(5)
            .arg(self.jobs_key())
            .arg(&dependency_key)
            .arg(format!("{dependency_key}:processed"))
            .arg(format!("{dependency_key}:failed"))
            .arg(format!("{dependency_key}:unsuccessful"))
            .arg(parent_id)
            .arg(kind.as_str())
            .arg(options.cursor)
            .arg(count)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_flow_dependency_page_result(&result, parent_id, kind, count)
    }

    /// Return cursor pages from several parent flow dependency buckets.
    pub async fn get_flow_dependency_pages(
        &self,
        parent_id: &str,
        options: JobFlowDependencyPagesOptions,
    ) -> Result<Option<JobFlowDependencyPages>> {
        let selected = options.selected();
        let mut conn = self.connection().await?;
        let dependency_key = self.dependencies_key(parent_id);
        let mut command = redis::cmd("EVAL");
        command
            .arg(FLOW_DEPENDENCY_PAGES_SCRIPT)
            .arg(5)
            .arg(self.jobs_key())
            .arg(&dependency_key)
            .arg(format!("{dependency_key}:processed"))
            .arg(format!("{dependency_key}:failed"))
            .arg(format!("{dependency_key}:unsuccessful"))
            .arg(parent_id);
        for page_options in selected {
            command
                .arg(page_options.kind.as_str())
                .arg(page_options.cursor)
                .arg(page_options.count.max(1));
        }
        let result: Vec<String> = command.query_async(&mut conn).await.map_err(redis_error)?;
        decode_flow_dependency_pages_result(&result, parent_id)
    }

    /// Return completed child result values for a flow parent.
    pub async fn get_flow_children_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowChildValues>> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(FLOW_CHILDREN_VALUES_SCRIPT)
            .arg(2)
            .arg(self.jobs_key())
            .arg(format!("{}:processed", self.dependencies_key(parent_id)))
            .arg(parent_id)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_flow_children_values_result(&result, parent_id)
    }

    /// Return ignored child failure reasons for a flow parent.
    pub async fn get_flow_ignored_children_failures(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowIgnoredFailures>> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(FLOW_IGNORED_CHILDREN_FAILURES_SCRIPT)
            .arg(2)
            .arg(self.jobs_key())
            .arg(format!("{}:failed", self.dependencies_key(parent_id)))
            .arg(parent_id)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_flow_ignored_children_failures_result(&result, parent_id)
    }

    /// Remove children that are still unprocessed and not active.
    pub async fn remove_unprocessed_children(
        &self,
        parent_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Vec<Job>>> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(REMOVE_UNPROCESSED_CHILDREN_SCRIPT)
            .arg(11)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.sequence_key())
            .arg(self.dependencies_key(parent_id))
            .arg(self.events_key())
            .arg(self.marker_key())
            .arg(parent_id)
            .arg(now.to_rfc3339())
            .arg(millis(now))
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.lock_key_prefix())
            .arg(self.dependencies_key_prefix())
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_remove_unprocessed_children_result(&result, parent_id)
    }

    /// Remove a single child from its parent dependency list without deleting the child job.
    pub async fn remove_child_dependency(
        &self,
        child_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(REMOVE_CHILD_DEPENDENCY_SCRIPT)
            .arg(9)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.sequence_key())
            .arg(self.marker_key())
            .arg(child_id)
            .arg(now.to_rfc3339())
            .arg(millis(now))
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.dependencies_key_prefix())
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_remove_child_dependency_result(&result, child_id)
    }

    /// Return BullMQ-style per-minute terminal job metrics.
    ///
    /// Only `JobState::Completed` and `JobState::Failed` have Redis metrics.
    /// Data points are returned newest first, matching BullMQ's `getMetrics`.
    pub async fn get_metrics(
        &self,
        state: JobState,
        start: isize,
        end: isize,
    ) -> Result<JobMetrics> {
        let mut conn = self.connection().await?;
        let result: redis::Value = redis::cmd("EVAL")
            .arg(GET_METRICS_SCRIPT)
            .arg(2)
            .arg(self.metrics_key(state)?)
            .arg(self.metrics_data_key(state)?)
            .arg(start)
            .arg(end)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_metrics_result(&result, state)
    }

    /// Remove retained job records that are no longer referenced by Redis indexes.
    ///
    /// This is a Redis maintenance helper for BullMQ-style orphan cleanup. It
    /// scans the Lane job hash in batches, checks the lifecycle indexes and the
    /// stalled candidate set atomically for each candidate, and deletes only job
    /// hash fields that have no Redis-side reference. A `count` of zero uses a
    /// scan count of 1000. A `limit` of zero removes every orphan found by the
    /// scan; otherwise no more than `limit` jobs are removed.
    pub async fn remove_orphaned_jobs(&self, count: usize, limit: usize) -> Result<usize> {
        let mut conn = self.connection().await?;
        let scan_count = if count == 0 { 1000 } else { count };
        let mut cursor = 0_u64;
        let mut total_removed = 0_usize;

        loop {
            let remaining = if limit == 0 {
                0
            } else {
                limit.saturating_sub(total_removed)
            };
            if limit > 0 && remaining == 0 {
                break;
            }

            let (next_cursor, removed): (u64, usize) = redis::cmd("EVAL")
                .arg(REMOVE_ORPHANED_JOBS_SCRIPT)
                .arg(8)
                .arg(self.jobs_key())
                .arg(self.state_key(JobState::Waiting))
                .arg(self.state_key(JobState::Delayed))
                .arg(self.state_key(JobState::Active))
                .arg(self.state_key(JobState::WaitingChildren))
                .arg(self.state_key(JobState::Completed))
                .arg(self.state_key(JobState::Failed))
                .arg(self.stalled_key())
                .arg(cursor)
                .arg(scan_count)
                .arg(remaining)
                .arg(self.dependencies_key_prefix())
                .arg(self.logs_key_prefix())
                .arg(self.lock_key_prefix())
                .query_async(&mut conn)
                .await
                .map_err(redis_error)?;

            total_removed = total_removed.saturating_add(removed);
            if next_cursor == 0 || (limit > 0 && total_removed >= limit) {
                break;
            }
            cursor = next_cursor;
        }

        Ok(total_removed)
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

    fn queue_key_prefix(&self) -> String {
        format!("{}:{}:", self.namespace, self.queue)
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

    fn events_key(&self) -> String {
        self.key("events")
    }

    fn marker_key(&self) -> String {
        self.key("marker")
    }

    fn metrics_key(&self, state: JobState) -> Result<String> {
        let suffix = terminal_metrics_suffix(state)?;
        Ok(self.key(&format!("metrics:{suffix}")))
    }

    fn metrics_data_key(&self, state: JobState) -> Result<String> {
        let suffix = terminal_metrics_suffix(state)?;
        Ok(self.key(&format!("metrics:{suffix}:data")))
    }

    fn claim_rate_limit_key(&self) -> String {
        self.key("claim_rate_limit")
    }

    fn stalled_key(&self) -> String {
        self.key("stalled")
    }

    fn lock_key(&self, job_id: &str) -> String {
        format!("{}:{}:locks:{}", self.namespace, self.queue, job_id)
    }

    fn lock_key_prefix(&self) -> String {
        format!("{}:{}:locks:", self.namespace, self.queue)
    }

    fn dependencies_key_prefix(&self) -> String {
        format!("{}:{}:dependencies:", self.namespace, self.queue)
    }

    fn dependencies_key(&self, parent_id: &str) -> String {
        format!("{}{}", self.dependencies_key_prefix(), parent_id)
    }

    fn logs_key(&self, job_id: &str) -> String {
        format!("{}:{}:logs:{}", self.namespace, self.queue, job_id)
    }

    fn logs_key_prefix(&self) -> String {
        format!("{}:{}:logs:", self.namespace, self.queue)
    }

    fn deduplication_key_prefix(&self) -> String {
        format!("{}:{}:deduplication:", self.namespace, self.queue)
    }

    fn deduplication_key(&self, deduplication_id: &str) -> String {
        format!("{}{}", self.deduplication_key_prefix(), deduplication_id)
    }

    fn deduplication_next_key_prefix(&self) -> String {
        format!("{}:{}:deduplication_next:", self.namespace, self.queue)
    }

    fn deduplication_next_key(&self, deduplication_id: &str) -> String {
        format!(
            "{}{}",
            self.deduplication_next_key_prefix(),
            deduplication_id
        )
    }

    fn repeat_key_prefix(&self) -> String {
        format!("{}:{}:repeat:", self.namespace, self.queue)
    }

    fn repeat_schedulers_key(&self) -> String {
        self.key("repeat")
    }

    fn repeat_owner_key(&self, repeat_key: &str) -> String {
        format!("{}{}", self.repeat_key_prefix(), repeat_key)
    }

    fn repeat_scheduler_meta_key(&self, repeat_key: &str) -> String {
        self.key(&format!("repeat_meta:{repeat_key}"))
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

    async fn add_new_job(&self, conn: &mut ConnectionManager, job: &Job) -> Result<Job> {
        let encoded = encode_job(job)?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(ADD_JOB_SCRIPT)
            .arg(8)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.sequence_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.events_key())
            .arg(self.marker_key())
            .arg(&job.id)
            .arg(encoded)
            .arg(job_state_name(job.state))
            .arg(millis(job.scheduled_at))
            .arg(job.priority)
            .arg(WAITING_SCORE_BUCKET)
            .arg(job_deduplication_id(job).unwrap_or(""))
            .arg(self.deduplication_key_prefix())
            .arg(job_repeat_key(job).unwrap_or(""))
            .arg(self.repeat_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .query_async(conn)
            .await
            .map_err(redis_error)?;
        decode_add_job_result(&result, &job.id)
    }

    async fn add_flow_jobs(
        &self,
        conn: &mut ConnectionManager,
        parent: &Job,
        children: &[Job],
    ) -> Result<RedisAddFlowResult> {
        let job_count = 1 + children.len();
        let mut command = redis::cmd("EVAL");
        command
            .arg(ADD_FLOW_SCRIPT)
            .arg(8)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.sequence_key())
            .arg(self.events_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.marker_key())
            .arg(job_count);

        for job in std::iter::once(parent).chain(children.iter()) {
            command
                .arg(&job.id)
                .arg(encode_job(job)?)
                .arg(job_state_name(job.state))
                .arg(millis(job.scheduled_at))
                .arg(job.priority)
                .arg(job_deduplication_id(job).unwrap_or(""))
                .arg(job_repeat_key(job).unwrap_or(""));
        }
        command
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.dependencies_key_prefix())
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .arg(millis(parent.created_at));

        let result: Vec<String> = command.query_async(conn).await.map_err(redis_error)?;
        decode_add_flow_result(&result)
    }

    async fn add_flow_children_jobs(
        &self,
        conn: &mut ConnectionManager,
        parent_id: &str,
        lock_token: &str,
        children: &[Job],
        now: DateTime<Utc>,
    ) -> Result<HashSet<JobId>> {
        let mut command = redis::cmd("EVAL");
        command
            .arg(ADD_FLOW_CHILDREN_SCRIPT)
            .arg(10)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Active))
            .arg(self.sequence_key())
            .arg(self.events_key())
            .arg(self.stalled_key())
            .arg(self.marker_key())
            .arg(self.lock_key(parent_id))
            .arg(parent_id)
            .arg(lock_token)
            .arg(children.len());

        for job in children {
            command
                .arg(&job.id)
                .arg(encode_job(job)?)
                .arg(job_state_name(job.state))
                .arg(millis(job.scheduled_at))
                .arg(job.priority)
                .arg(job_deduplication_id(job).unwrap_or(""))
                .arg(job_repeat_key(job).unwrap_or(""));
        }
        command
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.dependencies_key_prefix())
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .arg(millis(now));

        let result: Vec<String> = command.query_async(conn).await.map_err(redis_error)?;
        decode_add_flow_children_result(&result, parent_id)
    }

    async fn load_job(&self, conn: &mut ConnectionManager, job_id: &str) -> Result<Option<Job>> {
        let raw: Option<String> = conn
            .hget(self.jobs_key(), job_id)
            .await
            .map_err(redis_error)?;
        let Some(raw) = raw else {
            return Ok(None);
        };

        let mut job = decode_job(&raw)?;
        if job.state == JobState::Active {
            job.lock_token = conn.get(self.lock_key(job_id)).await.map_err(redis_error)?;
        }
        Ok(Some(job))
    }

    async fn load_flow_for_parent(
        &self,
        conn: &mut ConnectionManager,
        parent_id: &str,
    ) -> Result<JobFlow> {
        let parent = self
            .load_job(conn, parent_id)
            .await?
            .ok_or_else(|| LaneError::JobNotFound(parent_id.to_string()))?;
        let mut children = Vec::with_capacity(parent.child_ids.len());
        for child_id in &parent.child_ids {
            if let Some(child) = self.load_job(conn, child_id).await? {
                children.push(child);
            }
        }
        Ok(JobFlow { parent, children })
    }

    async fn load_repeat_entry(
        &self,
        conn: &mut ConnectionManager,
        repeat_key: &str,
    ) -> Result<Option<JobRepeatEntry>> {
        if repeat_key.is_empty() {
            return Ok(None);
        }

        let owner_key = self.repeat_owner_key(repeat_key);
        let scheduler_meta_key = self.repeat_scheduler_meta_key(repeat_key);
        let owner_id: Option<String> = conn.get(&owner_key).await.map_err(redis_error)?;
        let scheduler_owner_id: Option<String> = conn
            .hget(&scheduler_meta_key, "jid")
            .await
            .map_err(redis_error)?;

        if let Some(owner_id) = owner_id {
            if let Some(entry) = self
                .load_valid_repeat_entry(conn, repeat_key, &owner_id)
                .await?
            {
                return Ok(Some(entry));
            }

            if scheduler_owner_id.as_deref() == Some(owner_id.as_str())
                || scheduler_owner_id.is_none()
            {
                self.clear_repeat_owner_if_stale(conn, repeat_key, &owner_id)
                    .await?;
                return Ok(None);
            }

            self.clear_repeat_owner_key_if_stale(conn, repeat_key, &owner_id)
                .await?;
        }

        let Some(scheduler_owner_id) = scheduler_owner_id else {
            self.clear_repeat_scheduler_metadata(conn, repeat_key)
                .await?;
            return Ok(None);
        };

        let Some(entry) = self
            .load_valid_repeat_entry(conn, repeat_key, &scheduler_owner_id)
            .await?
        else {
            self.clear_repeat_scheduler_metadata(conn, repeat_key)
                .await?;
            return Ok(None);
        };

        self.restore_repeat_owner_if_missing(conn, repeat_key, &scheduler_owner_id)
            .await?;
        Ok(Some(entry))
    }

    async fn load_valid_repeat_entry(
        &self,
        conn: &mut ConnectionManager,
        repeat_key: &str,
        owner_id: &str,
    ) -> Result<Option<JobRepeatEntry>> {
        let Some(job) = self.load_job(conn, owner_id).await? else {
            return Ok(None);
        };
        if job.state.is_terminal() || job.repeat_key.as_deref() != Some(repeat_key) {
            return Ok(None);
        }
        Ok(repeat_entry(&job))
    }

    async fn remove_job_with_conn(
        &self,
        conn: &mut ConnectionManager,
        job_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>> {
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(REMOVE_JOB_SCRIPT)
            .arg(12)
            .arg(self.jobs_key())
            .arg(self.lock_key(job_id))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.sequence_key())
            .arg(self.marker_key())
            .arg(self.events_key())
            .arg(self.stalled_key())
            .arg(job_id)
            .arg(now.to_rfc3339())
            .arg(millis(now))
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.dependencies_key_prefix())
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .query_async(conn)
            .await
            .map_err(redis_error)?;
        decode_remove_job_result(&result, job_id)
    }

    async fn clear_repeat_owner_if_stale(
        &self,
        conn: &mut ConnectionManager,
        repeat_key: &str,
        owner_id: &str,
    ) -> Result<()> {
        let _: usize = redis::cmd("EVAL")
            .arg(
                "if redis.call('GET', KEYS[1]) == ARGV[1] then \
                 redis.call('DEL', KEYS[1]); \
                 redis.call('ZREM', KEYS[2], ARGV[2]); \
                 redis.call('DEL', KEYS[3]); \
                 return 1 end return 0",
            )
            .arg(3)
            .arg(self.repeat_owner_key(repeat_key))
            .arg(self.repeat_schedulers_key())
            .arg(self.repeat_scheduler_meta_key(repeat_key))
            .arg(owner_id)
            .arg(repeat_key)
            .query_async(conn)
            .await
            .map_err(redis_error)?;
        Ok(())
    }

    async fn clear_repeat_owner_key_if_stale(
        &self,
        conn: &mut ConnectionManager,
        repeat_key: &str,
        owner_id: &str,
    ) -> Result<()> {
        let _: usize = redis::cmd("EVAL")
            .arg(
                "if redis.call('GET', KEYS[1]) == ARGV[1] then \
                 return redis.call('DEL', KEYS[1]) end return 0",
            )
            .arg(1)
            .arg(self.repeat_owner_key(repeat_key))
            .arg(owner_id)
            .query_async(conn)
            .await
            .map_err(redis_error)?;
        Ok(())
    }

    async fn restore_repeat_owner_if_missing(
        &self,
        conn: &mut ConnectionManager,
        repeat_key: &str,
        owner_id: &str,
    ) -> Result<()> {
        let _: Option<String> = redis::cmd("SET")
            .arg(self.repeat_owner_key(repeat_key))
            .arg(owner_id)
            .arg("NX")
            .query_async(conn)
            .await
            .map_err(redis_error)?;
        Ok(())
    }

    async fn clear_repeat_scheduler_metadata(
        &self,
        conn: &mut ConnectionManager,
        repeat_key: &str,
    ) -> Result<()> {
        let _: usize = redis::cmd("EVAL")
            .arg("redis.call('ZREM', KEYS[1], ARGV[1]); return redis.call('DEL', KEYS[2])")
            .arg(2)
            .arg(self.repeat_schedulers_key())
            .arg(self.repeat_scheduler_meta_key(repeat_key))
            .arg(repeat_key)
            .query_async(conn)
            .await
            .map_err(redis_error)?;
        Ok(())
    }

    async fn claim_next_with_conn(
        &self,
        conn: &mut ConnectionManager,
        worker_id: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>> {
        let lease_expires_at = add_duration(now, lease_for);
        let lock_token = Uuid::new_v4().to_string();
        let (rate_limit_max, rate_limit_window_ms) = self
            .claim_rate_limit
            .as_ref()
            .map(|limit| (limit.max_claims, duration_millis(limit.window).max(1)))
            .unwrap_or((0, 0));
        let raw: Option<String> = redis::cmd("EVAL")
            .arg(CLAIM_SCRIPT)
            .arg(9)
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Active))
            .arg(self.jobs_key())
            .arg(self.claim_rate_limit_key())
            .arg(self.meta_key())
            .arg(self.state_key(JobState::Delayed))
            .arg(self.sequence_key())
            .arg(self.events_key())
            .arg(self.marker_key())
            .arg(millis(lease_expires_at))
            .arg(now.to_rfc3339())
            .arg(worker_id)
            .arg(lease_expires_at.to_rfc3339())
            .arg(&lock_token)
            .arg(lock_duration_millis(lease_for))
            .arg(self.lock_key_prefix())
            .arg(rate_limit_max)
            .arg(rate_limit_window_ms)
            .arg(millis(now))
            .arg(WAITING_SCORE_BUCKET)
            .arg(1_000_u16)
            .arg(1_000_u16)
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .arg(self.repeat_key_prefix())
            .query_async(conn)
            .await
            .map_err(redis_error)?;

        let Some(raw) = raw else {
            return Ok(None);
        };
        let mut job = decode_job(&raw)?;
        job.lock_token = Some(lock_token);
        Ok(Some(job))
    }

    async fn update_priority_order(
        &self,
        job_id: &str,
        priority: JobPriority,
        lifo: Option<bool>,
    ) -> Result<Job> {
        validate_job_priority(priority)?;

        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(UPDATE_PRIORITY_SCRIPT)
            .arg(4)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.sequence_key())
            .arg(self.marker_key())
            .arg(job_id)
            .arg(priority)
            .arg(WAITING_SCORE_BUCKET)
            .arg(match lifo {
                Some(true) => "1",
                Some(false) => "0",
                None => "",
            })
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_priority_update_result(&result, job_id)
    }

    async fn fail_job_with_retry_control(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
        allow_retry: bool,
    ) -> Result<Job> {
        let mut conn = self.connection().await?;
        let job = self.require_job(&mut conn, job_id).await?;
        let retry_at = if allow_retry && should_retry(&job) {
            let delay = job
                .options
                .retry_policy
                .delay_for_attempt(job.attempts_made);
            Some(add_duration(now, delay))
        } else {
            None
        };
        let scheduled_at = retry_at.unwrap_or(now);
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(FAIL_SCRIPT)
            .arg(13)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.lock_key(job_id))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.sequence_key())
            .arg(self.events_key())
            .arg(self.stalled_key())
            .arg(self.metrics_key(JobState::Failed)?)
            .arg(self.metrics_data_key(JobState::Failed)?)
            .arg(self.marker_key())
            .arg(job_id)
            .arg(lock_token)
            .arg(now.to_rfc3339())
            .arg(error)
            .arg(if retry_at.is_some() { "1" } else { "0" })
            .arg(scheduled_at.to_rfc3339())
            .arg(millis(scheduled_at))
            .arg(now.timestamp_millis())
            .arg(if job.options.remove_on_fail { "1" } else { "0" })
            .arg(self.dependencies_key_prefix())
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.deduplication_next_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .arg(DEFAULT_JOB_METRICS_RETENTION)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "fail")
    }
}

#[async_trait]
impl JobQueueBackend for RedisJobQueue {
    async fn add_job(&self, name: String, payload: Value, options: JobOptions) -> Result<Job> {
        let now = Utc::now();
        validate_job_options_at(&options, now)?;
        let job = Job::new(self.queue.clone(), name, payload, options, now);
        let mut conn = self.connection().await?;
        self.add_new_job(&mut conn, &job).await
    }

    async fn add_jobs(&self, jobs: Vec<JobSpec>, now: DateTime<Utc>) -> Result<Vec<Job>> {
        self.add_many_at(jobs, now).await
    }

    async fn add_flow(
        &self,
        parent: JobSpec,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<JobFlow> {
        self.add_flow_at(parent, children, now).await
    }

    async fn add_flow_children(
        &self,
        parent_id: &str,
        lock_token: &str,
        children: Vec<JobSpec>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>> {
        self.add_flow_children_at(parent_id, lock_token, children, now)
            .await
    }

    async fn get_flow_dependencies(&self, parent_id: &str) -> Result<Option<JobFlowDependencies>> {
        RedisJobQueue::get_flow_dependencies(self, parent_id).await
    }

    async fn get_flow_dependency_counts(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyCounts>> {
        RedisJobQueue::get_flow_dependency_counts(self, parent_id).await
    }

    async fn get_flow_dependency_selected_counts(
        &self,
        parent_id: &str,
        options: JobFlowDependencyCountOptions,
    ) -> Result<Option<JobFlowDependencySelectedCounts>> {
        RedisJobQueue::get_flow_dependency_selected_counts(self, parent_id, options).await
    }

    async fn get_flow_dependency_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowDependencyValues>> {
        RedisJobQueue::get_flow_dependency_values(self, parent_id).await
    }

    async fn get_flow_dependency_page(
        &self,
        parent_id: &str,
        options: JobFlowDependencyPageOptions,
    ) -> Result<Option<JobFlowDependencyPage>> {
        RedisJobQueue::get_flow_dependency_page(self, parent_id, options).await
    }

    async fn get_flow_dependency_pages(
        &self,
        parent_id: &str,
        options: JobFlowDependencyPagesOptions,
    ) -> Result<Option<JobFlowDependencyPages>> {
        RedisJobQueue::get_flow_dependency_pages(self, parent_id, options).await
    }

    async fn get_flow_children_values(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowChildValues>> {
        RedisJobQueue::get_flow_children_values(self, parent_id).await
    }

    async fn get_flow_ignored_children_failures(
        &self,
        parent_id: &str,
    ) -> Result<Option<JobFlowIgnoredFailures>> {
        RedisJobQueue::get_flow_ignored_children_failures(self, parent_id).await
    }

    async fn remove_unprocessed_children(
        &self,
        parent_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Vec<Job>>> {
        RedisJobQueue::remove_unprocessed_children(self, parent_id, now).await
    }

    async fn remove_child_dependency(&self, child_id: &str, now: DateTime<Utc>) -> Result<bool> {
        RedisJobQueue::remove_child_dependency(self, child_id, now).await
    }

    async fn claim_next(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>> {
        let mut conn = self.connection().await?;
        self.claim_next_with_conn(&mut conn, &worker_id, lease_for, now)
            .await
    }

    async fn claim_next_blocking(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        block_for: Duration,
    ) -> Result<Option<Job>> {
        RedisJobQueue::claim_next_blocking(self, worker_id, lease_for, block_for).await
    }

    async fn complete_job(
        &self,
        job_id: &str,
        lock_token: &str,
        value: Value,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut conn = self.connection().await?;
        let active_job = self.require_job(&mut conn, job_id).await?;
        let remove_on_complete = active_job.options.remove_on_complete;
        let next_repeat = next_repeat_job(&active_job, now)?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(COMPLETE_SCRIPT)
            .arg(16)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::Completed))
            .arg(self.lock_key(job_id))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.sequence_key())
            .arg(self.state_key(JobState::Failed))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.events_key())
            .arg(self.stalled_key())
            .arg(self.metrics_key(JobState::Completed)?)
            .arg(self.metrics_data_key(JobState::Completed)?)
            .arg(self.metrics_key(JobState::Failed)?)
            .arg(self.metrics_data_key(JobState::Failed)?)
            .arg(self.marker_key())
            .arg(job_id)
            .arg(lock_token)
            .arg(now.to_rfc3339())
            .arg(serde_json::to_string(&value).map_err(|error| {
                LaneError::Other(format!(
                    "failed to encode Redis job completion value: {error}"
                ))
            })?)
            .arg(now.timestamp_millis())
            .arg(if remove_on_complete { "1" } else { "0" })
            .arg(WAITING_SCORE_BUCKET)
            .arg(
                next_repeat
                    .as_ref()
                    .map(|job| job.id.as_str())
                    .unwrap_or(""),
            )
            .arg(
                next_repeat
                    .as_ref()
                    .map(encode_job)
                    .transpose()?
                    .unwrap_or_default(),
            )
            .arg(
                next_repeat
                    .as_ref()
                    .map(|job| job_state_name(job.state))
                    .unwrap_or(""),
            )
            .arg(
                next_repeat
                    .as_ref()
                    .map(|job| millis(job.scheduled_at))
                    .unwrap_or_default(),
            )
            .arg(
                next_repeat
                    .as_ref()
                    .map(|job| job.priority)
                    .unwrap_or_default(),
            )
            .arg(self.dependencies_key_prefix())
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .arg(DEFAULT_JOB_METRICS_RETENTION)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "complete")
    }

    async fn fail_job(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        self.fail_job_with_retry_control(job_id, lock_token, error, now, true)
            .await
    }

    async fn fail_job_discarding_retry(
        &self,
        job_id: &str,
        lock_token: &str,
        error: String,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        self.fail_job_with_retry_control(job_id, lock_token, error, now, false)
            .await
    }

    async fn renew_lease(
        &self,
        job_id: &str,
        lock_token: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut conn = self.connection().await?;
        let lease_expires_at = add_duration(now, lease_for);
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(RENEW_LEASE_SCRIPT)
            .arg(4)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.lock_key(job_id))
            .arg(self.stalled_key())
            .arg(self.marker_key())
            .arg(job_id)
            .arg(lock_token)
            .arg(lease_expires_at.to_rfc3339())
            .arg(millis(lease_expires_at))
            .arg(lock_duration_millis(lease_for))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        let mut job = decode_transition_result(&result, job_id, "renew lease")?;
        job.lock_token = Some(lock_token.to_string());
        Ok(job)
    }

    async fn renew_leases(
        &self,
        renewals: &[JobLeaseRenewal],
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Vec<JobId>> {
        if renewals.is_empty() {
            return Ok(Vec::new());
        }

        let mut conn = self.connection().await?;
        let lease_expires_at = add_duration(now, lease_for);
        let mut command = redis::cmd("EVAL");
        command
            .arg(RENEW_LEASES_SCRIPT)
            .arg(3)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.stalled_key())
            .arg(self.lock_key_prefix())
            .arg(lease_expires_at.to_rfc3339())
            .arg(millis(lease_expires_at))
            .arg(lock_duration_millis(lease_for))
            .arg(renewals.len());
        for renewal in renewals {
            command.arg(&renewal.job_id).arg(&renewal.lock_token);
        }

        command.query_async(&mut conn).await.map_err(redis_error)
    }

    async fn delay_active_job(
        &self,
        job_id: &str,
        lock_token: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut conn = self.connection().await?;
        let scheduled_at = add_duration(now, delay);
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(DELAY_ACTIVE_JOB_SCRIPT)
            .arg(8)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.lock_key(job_id))
            .arg(self.events_key())
            .arg(self.stalled_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.marker_key())
            .arg(job_id)
            .arg(lock_token)
            .arg(scheduled_at.to_rfc3339())
            .arg(scheduled_at.timestamp_millis())
            .arg(serde_json::to_string(&delay).map_err(|error| {
                LaneError::Other(format!("failed to encode Redis job delay: {error}"))
            })?)
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .arg(self.repeat_key_prefix())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "delay active")
    }

    async fn release_active_job(
        &self,
        job_id: &str,
        lock_token: &str,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(RELEASE_ACTIVE_JOB_SCRIPT)
            .arg(8)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.sequence_key())
            .arg(self.lock_key(job_id))
            .arg(self.events_key())
            .arg(self.stalled_key())
            .arg(self.marker_key())
            .arg(job_id)
            .arg(lock_token)
            .arg(now.to_rfc3339())
            .arg(millis(now))
            .arg(WAITING_SCORE_BUCKET)
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .arg(self.repeat_key_prefix())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "release active")
    }

    async fn promote_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(PROMOTE_JOB_SCRIPT)
            .arg(6)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.sequence_key())
            .arg(self.events_key())
            .arg(self.marker_key())
            .arg(job_id)
            .arg(now.to_rfc3339())
            .arg(WAITING_SCORE_BUCKET)
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .arg(self.repeat_key_prefix())
            .arg(millis(now))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "promote")
    }

    async fn reschedule_job(
        &self,
        job_id: &str,
        delay: Duration,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        if delay.is_zero() {
            return Err(LaneError::ConfigError(
                "job delay must be greater than zero".to_string(),
            ));
        }

        let scheduled_at = add_duration(now, delay);
        let delay = serde_json::to_string(&delay)
            .map_err(|error| LaneError::Other(format!("failed to encode job delay: {error}")))?;
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(RESCHEDULE_JOB_SCRIPT)
            .arg(4)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Delayed))
            .arg(self.marker_key())
            .arg(self.events_key())
            .arg(job_id)
            .arg(scheduled_at.to_rfc3339())
            .arg(scheduled_at.timestamp_millis())
            .arg(delay)
            .arg(self.repeat_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "reschedule")
    }

    async fn retry_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let mut conn = self.connection().await?;
        let retry_job = self.load_job(&mut conn, job_id).await?;
        let deduplication_expires_at = retry_job
            .as_ref()
            .and_then(|job| deduplication_expiration(&job.options, now))
            .map(|expires_at| expires_at.to_rfc3339())
            .unwrap_or_default();
        let deduplication_ttl_millis = retry_job
            .as_ref()
            .and_then(job_deduplication_ttl_millis)
            .map(|ttl| ttl.to_string())
            .unwrap_or_default();
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(RETRY_JOB_SCRIPT)
            .arg(9)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.sequence_key())
            .arg(self.events_key())
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.marker_key())
            .arg(job_id)
            .arg(now.to_rfc3339())
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(deduplication_expires_at)
            .arg(deduplication_ttl_millis)
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .arg(millis(now))
            .arg(self.dependencies_key_prefix())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "retry")
    }

    async fn update_priority(&self, job_id: &str, priority: JobPriority) -> Result<Job> {
        self.update_priority_order(job_id, priority, None).await
    }

    async fn update_priority_with_lifo(
        &self,
        job_id: &str,
        priority: JobPriority,
        lifo: bool,
    ) -> Result<Job> {
        self.update_priority_order(job_id, priority, Some(lifo))
            .await
    }

    async fn remove_job(&self, job_id: &str) -> Result<Option<Job>> {
        let mut conn = self.connection().await?;
        self.remove_job_with_conn(&mut conn, job_id, Utc::now())
            .await
    }

    async fn remove_repeat(&self, repeat_key: &str) -> Result<Option<Job>> {
        if repeat_key.is_empty() {
            return Ok(None);
        }

        let mut conn = self.connection().await?;
        let owner_id: Option<String> = conn
            .get(self.repeat_owner_key(repeat_key))
            .await
            .map_err(redis_error)?;
        let Some(owner_id) = owner_id else {
            let scheduler_owner_id: Option<String> = conn
                .hget(self.repeat_scheduler_meta_key(repeat_key), "jid")
                .await
                .map_err(redis_error)?;
            let Some(scheduler_owner_id) = scheduler_owner_id else {
                self.clear_repeat_scheduler_metadata(&mut conn, repeat_key)
                    .await?;
                return Ok(None);
            };

            let Some(scheduler_owner) = self.load_job(&mut conn, &scheduler_owner_id).await? else {
                self.clear_repeat_scheduler_metadata(&mut conn, repeat_key)
                    .await?;
                return Ok(None);
            };

            if scheduler_owner.state.is_terminal()
                || scheduler_owner.repeat_key.as_deref() != Some(repeat_key)
            {
                self.clear_repeat_scheduler_metadata(&mut conn, repeat_key)
                    .await?;
                return Ok(None);
            }

            let removed = self
                .remove_job_with_conn(&mut conn, &scheduler_owner_id, Utc::now())
                .await?;
            self.clear_repeat_scheduler_metadata(&mut conn, repeat_key)
                .await?;
            return Ok(removed);
        };

        let removed = self
            .remove_job_with_conn(&mut conn, &owner_id, Utc::now())
            .await?;
        if removed.is_none() {
            self.clear_repeat_owner_if_stale(&mut conn, repeat_key, &owner_id)
                .await?;
        }
        Ok(removed)
    }

    async fn remove_deduplication_key(&self, deduplication_id: &str) -> Result<bool> {
        if deduplication_id.is_empty() {
            return Ok(false);
        }

        let mut conn = self.connection().await?;
        let removed: usize = redis::cmd("EVAL")
            .arg(REMOVE_DEDUPLICATION_KEY_SCRIPT)
            .arg(2)
            .arg(self.deduplication_key(deduplication_id))
            .arg(self.deduplication_next_key(deduplication_id))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        Ok(removed > 0)
    }

    async fn get_deduplication_job_id(&self, deduplication_id: &str) -> Result<Option<JobId>> {
        if deduplication_id.is_empty() {
            return Ok(None);
        }

        let mut conn = self.connection().await?;
        redis::cmd("EVAL")
            .arg(GET_DEDUPLICATION_JOB_ID_SCRIPT)
            .arg(3)
            .arg(self.jobs_key())
            .arg(self.deduplication_key(deduplication_id))
            .arg(self.deduplication_next_key(deduplication_id))
            .arg(deduplication_id)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)
    }

    async fn list_repeats(&self) -> Result<Vec<JobRepeatEntry>> {
        RedisJobQueue::list_repeats(self).await
    }

    async fn upsert_repeat(&self, spec: JobSpec, now: DateTime<Utc>) -> Result<Job> {
        RedisJobQueue::upsert_repeat(self, spec, now).await
    }

    async fn clean_jobs(
        &self,
        state: JobState,
        grace: Duration,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<Job>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let cutoff = subtract_duration(now, grace);
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(CLEAN_JOBS_SCRIPT)
            .arg(11)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.sequence_key())
            .arg(self.marker_key())
            .arg(self.events_key())
            .arg(self.stalled_key())
            .arg(job_state_name(state))
            .arg(cutoff.to_rfc3339())
            .arg(limit)
            .arg(self.lock_key_prefix())
            .arg(now.to_rfc3339())
            .arg(millis(now))
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.dependencies_key_prefix())
            .arg(millis(cutoff))
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_clean_jobs_result(&result)
    }

    async fn drain_jobs(&self, include_delayed: bool) -> Result<Vec<Job>> {
        let mut conn = self.connection().await?;
        let now = Utc::now();
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(DRAIN_JOBS_SCRIPT)
            .arg(9)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.sequence_key())
            .arg(self.marker_key())
            .arg(if include_delayed { "1" } else { "0" })
            .arg(now.to_rfc3339())
            .arg(millis(now))
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.lock_key_prefix())
            .arg(self.dependencies_key_prefix())
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_clean_jobs_result(&result)
    }

    async fn obliterate(&self, force: bool) -> Result<usize> {
        let mut conn = self.connection().await?;
        let removed: i64 = redis::cmd("EVAL")
            .arg(OBLITERATE_SCRIPT)
            .arg(3)
            .arg(self.meta_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.jobs_key())
            .arg(self.queue_key_prefix())
            .arg(if force { "1" } else { "0" })
            .arg(1000_u16)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        match removed {
            -2 => Err(LaneError::JobStateConflict(
                "cannot obliterate queue with active jobs".to_string(),
            )),
            value if value < 0 => Err(LaneError::QueueError(format!(
                "Redis obliterate script returned unexpected code {value}"
            ))),
            value => usize::try_from(value).map_err(|error| {
                LaneError::Other(format!("failed to decode Redis obliterate count: {error}"))
            }),
        }
    }

    async fn list_jobs(&self, options: JobListOptions) -> Result<JobListPage> {
        let mut conn = self.connection().await?;
        let states = options.selected_states();
        let mut command = redis::cmd("EVAL");
        command
            .arg(LIST_JOBS_SCRIPT)
            .arg(7)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(options.offset)
            .arg(options.limit)
            .arg(if options.ascending { "1" } else { "0" });
        for state in states {
            command.arg(job_state_name(state));
        }

        let result: Vec<redis::Value> =
            command.query_async(&mut conn).await.map_err(redis_error)?;
        decode_list_jobs_result(&result, options.offset, options.limit)
    }

    async fn get_job_counts(&self, states: &[JobState]) -> Result<Vec<JobStateCount>> {
        let states = unique_states(states);
        let mut conn = self.connection().await?;
        let mut command = redis::cmd("EVAL");
        command.arg(JOB_COUNTS_SCRIPT).arg(states.len());
        for state in &states {
            command.arg(self.state_key(*state));
        }

        let result: Vec<i64> = command.query_async(&mut conn).await.map_err(redis_error)?;
        decode_state_counts(&states, &result)
    }

    async fn get_counts_per_priority(
        &self,
        priorities: &[JobPriority],
    ) -> Result<Vec<JobPriorityCount>> {
        let priorities = unique_priorities(priorities);
        let mut conn = self.connection().await?;
        let mut command = redis::cmd("EVAL");
        command
            .arg(COUNTS_PER_PRIORITY_SCRIPT)
            .arg(1)
            .arg(self.state_key(JobState::Waiting))
            .arg(WAITING_SCORE_BUCKET);
        for priority in &priorities {
            command.arg(*priority);
        }

        let result: Vec<i64> = command.query_async(&mut conn).await.map_err(redis_error)?;
        decode_priority_counts(&priorities, &result)
    }

    async fn update_data(&self, job_id: &str, payload: Value) -> Result<Job> {
        let mut conn = self.connection().await?;
        let payload = serde_json::to_string(&payload)
            .map_err(|error| LaneError::Other(format!("failed to encode job payload: {error}")))?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(UPDATE_DATA_SCRIPT)
            .arg(1)
            .arg(self.jobs_key())
            .arg(job_id)
            .arg(payload)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "update data")
    }

    async fn update_progress(&self, job_id: &str, progress: Value) -> Result<Job> {
        let mut conn = self.connection().await?;
        let progress = serde_json::to_string(&progress)
            .map_err(|error| LaneError::Other(format!("failed to encode job progress: {error}")))?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(UPDATE_PROGRESS_SCRIPT)
            .arg(2)
            .arg(self.jobs_key())
            .arg(self.events_key())
            .arg(job_id)
            .arg(progress)
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "update progress")
    }

    async fn save_stacktrace(
        &self,
        job_id: &str,
        stacktrace: Vec<String>,
        failed_reason: String,
    ) -> Result<Job> {
        let mut conn = self.connection().await?;
        let stacktrace = serde_json::to_string(&stacktrace).map_err(|error| {
            LaneError::Other(format!("failed to encode Redis job stacktrace: {error}"))
        })?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(SAVE_STACKTRACE_SCRIPT)
            .arg(1)
            .arg(self.jobs_key())
            .arg(job_id)
            .arg(stacktrace)
            .arg(failed_reason)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "save stacktrace")
    }

    async fn add_log(
        &self,
        job_id: &str,
        line: String,
        keep: usize,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut conn = self.connection().await?;
        let entry = JobLogEntry {
            timestamp: now,
            line: line.clone(),
        };
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(ADD_LOG_SCRIPT)
            .arg(2)
            .arg(self.jobs_key())
            .arg(self.logs_key(job_id))
            .arg(job_id)
            .arg(now.to_rfc3339())
            .arg(line)
            .arg(keep)
            .arg(serde_json::to_string(&entry).map_err(|error| {
                LaneError::Other(format!("failed to encode Redis job log entry: {error}"))
            })?)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "add log")
    }

    async fn get_job_logs(
        &self,
        job_id: &str,
        start: isize,
        end: isize,
        ascending: bool,
    ) -> Result<JobLogPage> {
        let mut conn = self.connection().await?;
        let logs_key = self.logs_key(job_id);
        let (range_start, range_end) = if ascending {
            (start, end)
        } else {
            (
                end.saturating_add(1).saturating_neg(),
                start.saturating_add(1).saturating_neg(),
            )
        };
        let mut raw_logs: Vec<String> = conn
            .lrange(&logs_key, range_start, range_end)
            .await
            .map_err(redis_error)?;
        if !ascending {
            raw_logs.reverse();
        }
        let count: usize = conn.llen(&logs_key).await.map_err(redis_error)?;
        Ok(JobLogPage {
            logs: decode_log_entries(raw_logs)?,
            count,
        })
    }

    async fn clear_job_logs(&self, job_id: &str, keep: usize) -> Result<JobLogPage> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(CLEAR_LOGS_SCRIPT)
            .arg(2)
            .arg(self.jobs_key())
            .arg(self.logs_key(job_id))
            .arg(job_id)
            .arg(keep)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_clear_logs_result(&result, job_id)
    }

    async fn read_events(&self, start: &str, end: &str, limit: usize) -> Result<Vec<JobEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut conn = self.connection().await?;
        let reply: StreamRangeReply = redis::cmd("XRANGE")
            .arg(self.events_key())
            .arg(start)
            .arg(end)
            .arg("COUNT")
            .arg(limit)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_event_stream(reply)
    }

    async fn trim_events(&self, max_len: usize) -> Result<usize> {
        let mut conn = self.connection().await?;
        redis::cmd("XTRIM")
            .arg(self.events_key())
            .arg("MAXLEN")
            .arg("~")
            .arg(max_len)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)
    }

    async fn promote_due_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut conn = self.connection().await?;
        let mut total = 0;
        loop {
            let promoted: usize = redis::cmd("EVAL")
                .arg(PROMOTE_DUE_JOBS_SCRIPT)
                .arg(6)
                .arg(self.jobs_key())
                .arg(self.state_key(JobState::Delayed))
                .arg(self.state_key(JobState::Waiting))
                .arg(self.sequence_key())
                .arg(self.events_key())
                .arg(self.marker_key())
                .arg(millis(now))
                .arg(WAITING_SCORE_BUCKET)
                .arg(1_000_u16)
                .arg(DEFAULT_JOB_EVENT_RETENTION)
                .arg(self.repeat_key_prefix())
                .query_async(&mut conn)
                .await
                .map_err(redis_error)?;
            total += promoted;
            if promoted < 1_000 {
                break;
            }
        }
        Ok(total)
    }

    async fn recover_stalled_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut conn = self.connection().await?;
        redis::cmd("EVAL")
            .arg(RECOVER_STALLED_SCRIPT)
            .arg(12)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Failed))
            .arg(self.sequence_key())
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.stalled_key())
            .arg(self.metrics_key(JobState::Failed)?)
            .arg(self.metrics_data_key(JobState::Failed)?)
            .arg(self.marker_key())
            .arg(self.events_key())
            .arg(now.timestamp_millis())
            .arg(now.to_rfc3339())
            .arg(self.lock_key_prefix())
            .arg(WAITING_SCORE_BUCKET)
            .arg(1_000_u16)
            .arg(self.dependencies_key_prefix())
            .arg(self.deduplication_key_prefix())
            .arg(self.repeat_key_prefix())
            .arg(self.deduplication_next_key_prefix())
            .arg(self.logs_key_prefix())
            .arg(DEFAULT_JOB_METRICS_RETENTION)
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)
    }

    async fn pause(&self) -> Result<()> {
        let mut conn = self.connection().await?;
        let _: i64 = redis::cmd("EVAL")
            .arg(
                "redis.call('HSET', KEYS[1], 'paused', 1); \
                 redis.call('DEL', KEYS[3]); \
                 redis.call('XADD', KEYS[2], 'MAXLEN', '~', ARGV[2], '*', 'event', ARGV[1]); \
                 return 1",
            )
            .arg(3)
            .arg(self.meta_key())
            .arg(self.events_key())
            .arg(self.marker_key())
            .arg("paused")
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        Ok(())
    }

    async fn resume(&self) -> Result<()> {
        let mut conn = self.connection().await?;
        let _: i64 = redis::cmd("EVAL")
            .arg(
                "redis.call('HDEL', KEYS[1], 'paused'); \
                 if redis.call('ZCARD', KEYS[3]) > 0 then \
                   redis.call('ZADD', KEYS[5], 0, '0'); \
                 end; \
                 local next_delayed = redis.call('ZRANGE', KEYS[4], 0, 0, 'WITHSCORES'); \
                 if next_delayed[2] then \
                   redis.call('ZADD', KEYS[5], next_delayed[2], '1'); \
                 else \
                   redis.call('ZREM', KEYS[5], '1'); \
                 end; \
                 redis.call('XADD', KEYS[2], 'MAXLEN', '~', ARGV[2], '*', 'event', ARGV[1]); \
                 return 1",
            )
            .arg(5)
            .arg(self.meta_key())
            .arg(self.events_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.marker_key())
            .arg("resumed")
            .arg(DEFAULT_JOB_EVENT_RETENTION)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        Ok(())
    }

    async fn is_paused(&self) -> Result<bool> {
        let mut conn = self.connection().await?;
        let paused: Option<String> = conn
            .hget(self.meta_key(), "paused")
            .await
            .map_err(redis_error)?;
        if paused.as_deref() == Some("0") {
            let _: usize = conn
                .hdel(self.meta_key(), "paused")
                .await
                .map_err(redis_error)?;
            return Ok(false);
        }
        Ok(paused.is_some())
    }

    async fn get_job(&self, job_id: &str) -> Result<Option<Job>> {
        let mut conn = self.connection().await?;
        self.load_job(&mut conn, job_id).await
    }

    async fn get_job_state(&self, job_id: &str) -> Result<Option<JobState>> {
        let mut conn = self.connection().await?;
        let state: String = redis::cmd("EVAL")
            .arg(GET_JOB_STATE_SCRIPT)
            .arg(6)
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(job_id)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_job_state(&state)
    }

    async fn get_job_finished_result(&self, job_id: &str) -> Result<Option<JobFinishedResult>> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(GET_JOB_FINISHED_RESULT_SCRIPT)
            .arg(3)
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.jobs_key())
            .arg(job_id)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_job_finished_result(&result, job_id)
    }

    async fn stats(&self) -> Result<JobQueueStats> {
        let mut conn = self.connection().await?;
        let result: Vec<i64> = redis::cmd("EVAL")
            .arg(STATS_SCRIPT)
            .arg(7)
            .arg(self.meta_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_stats_result(&result)
    }
}

impl RedisJobQueue {
    async fn require_job(&self, conn: &mut ConnectionManager, job_id: &str) -> Result<Job> {
        self.load_job(conn, job_id)
            .await?
            .ok_or_else(|| LaneError::JobNotFound(job_id.to_string()))
    }
}

fn encode_job(job: &Job) -> Result<String> {
    serde_json::to_string(job)
        .map_err(|error| LaneError::Other(format!("failed to encode Redis job: {error}")))
}

fn job_deduplication_id(job: &Job) -> Option<&str> {
    job.options
        .deduplication
        .as_ref()
        .map(|deduplication| deduplication.id.as_str())
}

fn job_deduplication_ttl_millis(job: &Job) -> Option<u64> {
    job.options
        .deduplication
        .as_ref()
        .filter(|deduplication| !deduplication.keep_last_if_active)
        .and_then(|deduplication| deduplication.ttl)
        .map(lock_duration_millis)
}

fn job_repeat_key(job: &Job) -> Option<&str> {
    job.repeat_key.as_deref()
}

fn repeat_entry(job: &Job) -> Option<JobRepeatEntry> {
    let key = job_repeat_key(job)?.to_string();
    let options = job.options.repeat.clone()?;
    Some(JobRepeatEntry {
        key,
        job_id: job.id.clone(),
        name: job.name.clone(),
        state: job.state,
        scheduled_at: job.scheduled_at,
        repeat_count: job.repeat_count,
        options,
    })
}

fn decode_job(raw: &str) -> Result<Job> {
    let mut value: Value = serde_json::from_str(raw)
        .map_err(|error| LaneError::Other(format!("failed to decode Redis job: {error}")))?;
    normalize_lua_empty_array(&mut value, "logs");
    normalize_lua_empty_array(&mut value, "child_ids");
    normalize_lua_empty_array(&mut value, "stacktrace");
    serde_json::from_value(value)
        .map_err(|error| LaneError::Other(format!("failed to decode Redis job: {error}")))
}

fn decode_add_job_result(result: &[String], job_id: &str) -> Result<Job> {
    match result.first().map(String::as_str) {
        Some("inserted" | "existing" | "deduplicated" | "repeat") => {
            let raw = result.get(1).ok_or_else(|| {
                LaneError::Other(format!(
                    "Redis add job script returned no payload for {job_id}"
                ))
            })?;
            decode_job(raw)
        }
        Some("missing") => Err(LaneError::JobNotFound(job_id.to_string())),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis add job script status `{other}` for {job_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis add job script returned no status for {job_id}"
        ))),
    }
}

fn decode_upsert_repeat_result(result: &[String], repeat_key: &str, job_id: &str) -> Result<Job> {
    match result.first().map(String::as_str) {
        Some("inserted") => {
            let raw = result.get(1).ok_or_else(|| {
                LaneError::Other(format!(
                    "Redis repeat upsert script returned no payload for {job_id}"
                ))
            })?;
            decode_job(raw)
        }
        Some("exists") => Err(LaneError::ConfigError(format!(
            "job id `{job_id}` already exists"
        ))),
        Some("active") => Err(LaneError::JobLeaseConflict(format!(
            "cannot upsert repeat key `{repeat_key}`; current owner {} is active",
            result.get(1).map(String::as_str).unwrap_or("unknown")
        ))),
        Some("flow") => Err(LaneError::JobStateConflict(format!(
            "cannot upsert repeat key `{repeat_key}`; current owner {} has flow dependencies",
            result.get(1).map(String::as_str).unwrap_or("unknown")
        ))),
        Some("deduplicated") => Err(LaneError::JobStateConflict(format!(
            "cannot upsert repeat key `{repeat_key}`; deduplication id is active on job {}",
            result.get(1).map(String::as_str).unwrap_or("unknown")
        ))),
        Some("config") => Err(LaneError::ConfigError(
            result
                .get(1)
                .cloned()
                .unwrap_or_else(|| "invalid repeat upsert configuration".to_string()),
        )),
        Some("missing") => Err(LaneError::JobNotFound(job_id.to_string())),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis repeat upsert script status `{other}` for {job_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis repeat upsert script returned no status for {job_id}"
        ))),
    }
}

fn decode_add_jobs_result(result: &[String], expected_len: usize) -> Result<Vec<Job>> {
    if result.first().map(String::as_str) == Some("missing") {
        let id = result.get(1).map(String::as_str).unwrap_or("unknown");
        return Err(LaneError::JobNotFound(id.to_string()));
    }

    if result.len() != expected_len {
        return Err(LaneError::Other(format!(
            "Redis add jobs script returned {} jobs, expected {expected_len}",
            result.len()
        )));
    }

    result.iter().map(|raw| decode_job(raw)).collect()
}

struct RedisAddFlowResult {
    existing_parent_id: Option<JobId>,
    skipped_child_ids: HashSet<JobId>,
}

fn decode_add_flow_result(result: &[String]) -> Result<RedisAddFlowResult> {
    match result.first().map(String::as_str) {
        Some("ok") => Ok(RedisAddFlowResult {
            existing_parent_id: None,
            skipped_child_ids: result.iter().skip(1).cloned().collect(),
        }),
        Some("deduplicated_next") => {
            let existing_parent_id = result.get(1).cloned().ok_or_else(|| {
                LaneError::Other(
                    "Redis add flow script returned deduplicated_next without parent id"
                        .to_string(),
                )
            })?;
            Ok(RedisAddFlowResult {
                existing_parent_id: Some(existing_parent_id),
                skipped_child_ids: HashSet::new(),
            })
        }
        Some("deduplicated_parent") => {
            let existing_parent_id = result.get(1).cloned().ok_or_else(|| {
                LaneError::Other(
                    "Redis add flow script returned deduplicated_parent without parent id"
                        .to_string(),
                )
            })?;
            Ok(RedisAddFlowResult {
                existing_parent_id: Some(existing_parent_id),
                skipped_child_ids: HashSet::new(),
            })
        }
        Some("exists") => {
            let id = result.get(1).map(String::as_str).unwrap_or("unknown");
            Err(LaneError::ConfigError(format!(
                "flow job id `{id}` already exists"
            )))
        }
        Some("parent_conflict") => {
            let id = result.get(1).map(String::as_str).unwrap_or("unknown");
            let parent_id = result.get(2).map(String::as_str).unwrap_or("unknown");
            Err(LaneError::JobStateConflict(format!(
                "flow child id `{id}` already belongs to parent `{parent_id}`"
            )))
        }
        Some("deduplicated") => {
            let id = result.get(1).map(String::as_str).unwrap_or("unknown");
            Err(LaneError::ConfigError(format!(
                "flow deduplication id is already active for job `{id}`"
            )))
        }
        Some("repeat") => {
            let id = result.get(1).map(String::as_str).unwrap_or("unknown");
            Err(LaneError::ConfigError(format!(
                "flow repeat key is already active for job `{id}`"
            )))
        }
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis add flow script status `{other}`"
        ))),
        None => Err(LaneError::Other(
            "Redis add flow script returned no status".to_string(),
        )),
    }
}

fn decode_add_flow_children_result(result: &[String], parent_id: &str) -> Result<HashSet<JobId>> {
    match result.first().map(String::as_str) {
        Some("ok") => Ok(result.iter().skip(1).cloned().collect()),
        Some("empty") => Err(LaneError::ConfigError(
            "flow children cannot be empty".to_string(),
        )),
        Some("missing") => Err(LaneError::JobNotFound(parent_id.to_string())),
        Some("state") => {
            let state = result.get(1).map(String::as_str).unwrap_or("unknown");
            Err(LaneError::JobStateConflict(format!(
                "cannot add flow children to job {parent_id}; expected active state, found {state}"
            )))
        }
        Some("lock_missing") => Err(LaneError::JobLeaseConflict(format!(
            "missing lock for job {parent_id}"
        ))),
        Some("lock_mismatch") => Err(LaneError::JobLeaseConflict(format!(
            "lock token mismatch for job {parent_id}"
        ))),
        Some("failed_dependencies") => Err(LaneError::JobStateConflict(format!(
            "cannot add flow children to job {parent_id}; it has failed flow dependencies"
        ))),
        Some("exists") => {
            let id = result.get(1).map(String::as_str).unwrap_or("unknown");
            Err(LaneError::ConfigError(format!(
                "flow child id `{id}` already exists"
            )))
        }
        Some("parent_conflict") => {
            let id = result.get(1).map(String::as_str).unwrap_or("unknown");
            let parent_id = result.get(2).map(String::as_str).unwrap_or("unknown");
            Err(LaneError::JobStateConflict(format!(
                "flow child id `{id}` already belongs to parent `{parent_id}`"
            )))
        }
        Some("deduplicated") => {
            let id = result.get(1).map(String::as_str).unwrap_or("unknown");
            Err(LaneError::ConfigError(format!(
                "flow deduplication id is already active for job `{id}`"
            )))
        }
        Some("repeat") => {
            let id = result.get(1).map(String::as_str).unwrap_or("unknown");
            Err(LaneError::ConfigError(format!(
                "flow repeat key is already active for job `{id}`"
            )))
        }
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis add flow children script status `{other}`"
        ))),
        None => Err(LaneError::Other(
            "Redis add flow children script returned no status".to_string(),
        )),
    }
}

fn decode_priority_update_result(result: &[String], job_id: &str) -> Result<Job> {
    match result.first().map(String::as_str) {
        Some("ok") => {
            let raw = result.get(1).ok_or_else(|| {
                LaneError::Other(format!(
                    "Redis priority update script returned no payload for {job_id}"
                ))
            })?;
            decode_job(raw)
        }
        Some("missing") => Err(LaneError::JobNotFound(job_id.to_string())),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis priority update script status `{other}` for {job_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis priority update script returned no status for {job_id}"
        ))),
    }
}

fn decode_flow_dependency_counts_result(
    result: &[String],
    parent_id: &str,
) -> Result<Option<JobFlowDependencyCounts>> {
    match result.first().map(String::as_str) {
        Some("missing") => Ok(None),
        Some("ok") => {
            if result.len() != 6 {
                return Err(LaneError::Other(format!(
                    "Redis flow dependency count script returned {} fields for {parent_id}",
                    result.len()
                )));
            }
            Ok(Some(JobFlowDependencyCounts {
                processed: decode_usize_field(&result[1], "processed", parent_id)?,
                unprocessed: decode_usize_field(&result[2], "unprocessed", parent_id)?,
                failed: decode_usize_field(&result[3], "failed", parent_id)?,
                ignored: decode_usize_field(&result[4], "ignored", parent_id)?,
                missing: decode_usize_field(&result[5], "missing", parent_id)?,
            }))
        }
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis flow dependency count script status `{other}` for {parent_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis flow dependency count script returned no status for {parent_id}"
        ))),
    }
}

fn decode_flow_dependency_selected_counts_result(
    result: &[String],
    parent_id: &str,
) -> Result<Option<JobFlowDependencySelectedCounts>> {
    match result.first().map(String::as_str) {
        Some("missing") => Ok(None),
        Some("ok") => {
            let payload = &result[1..];
            if !payload.len().is_multiple_of(2) {
                return Err(LaneError::Other(format!(
                    "Redis flow dependency selected count script returned an odd pair count for {parent_id}"
                )));
            }
            let mut counts = JobFlowDependencySelectedCounts::default();
            for pair in payload.chunks_exact(2) {
                let kind = decode_flow_dependency_kind(&pair[0], parent_id)?;
                let count = decode_usize_field(&pair[1], kind.as_str(), parent_id)?;
                counts.insert(kind, count);
            }
            Ok(Some(counts))
        }
        Some("invalid_kind") => Err(LaneError::Other(format!(
            "Redis flow dependency selected count script rejected kind `{}` for {parent_id}",
            result
                .get(1)
                .map(String::as_str)
                .unwrap_or("<missing kind>")
        ))),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis flow dependency selected count script status `{other}` for {parent_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis flow dependency selected count script returned no status for {parent_id}"
        ))),
    }
}

fn decode_flow_dependency_values_result(
    result: &[String],
    parent_id: &str,
) -> Result<Option<JobFlowDependencyValues>> {
    match result.first().map(String::as_str) {
        Some("missing") => Ok(None),
        Some("ok") => {
            let mut index = 1;
            let processed_raw =
                read_dependency_value_segment(result, &mut index, "processed", parent_id)?;
            if !processed_raw.len().is_multiple_of(2) {
                return Err(LaneError::Other(format!(
                    "Redis flow dependency values script returned an odd processed pair count for {parent_id}"
                )));
            }
            let mut processed = BTreeMap::new();
            for pair in processed_raw.chunks_exact(2) {
                let value = serde_json::from_str(&pair[1]).map_err(|error| {
                    LaneError::Other(format!(
                        "failed to decode Redis processed dependency value for {} on {parent_id}: {error}",
                        pair[0]
                    ))
                })?;
                processed.insert(pair[0].clone(), value);
            }

            let unprocessed =
                read_dependency_value_segment(result, &mut index, "unprocessed", parent_id)?
                    .to_vec();

            let ignored_raw =
                read_dependency_value_segment(result, &mut index, "ignored", parent_id)?;
            if !ignored_raw.len().is_multiple_of(2) {
                return Err(LaneError::Other(format!(
                    "Redis flow dependency values script returned an odd ignored pair count for {parent_id}"
                )));
            }
            let mut ignored = BTreeMap::new();
            for pair in ignored_raw.chunks_exact(2) {
                ignored.insert(pair[0].clone(), pair[1].clone());
            }

            let failed =
                read_dependency_value_segment(result, &mut index, "failed", parent_id)?.to_vec();

            if index != result.len() {
                return Err(LaneError::Other(format!(
                    "Redis flow dependency values script returned {} trailing fields for {parent_id}",
                    result.len() - index
                )));
            }

            Ok(Some(JobFlowDependencyValues {
                processed,
                unprocessed,
                failed,
                ignored,
            }))
        }
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis flow dependency values script status `{other}` for {parent_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis flow dependency values script returned no status for {parent_id}"
        ))),
    }
}

fn read_dependency_value_segment<'a>(
    result: &'a [String],
    index: &mut usize,
    field: &str,
    parent_id: &str,
) -> Result<&'a [String]> {
    let raw_len = result.get(*index).ok_or_else(|| {
        LaneError::Other(format!(
            "Redis flow dependency values script returned no {field} length for {parent_id}"
        ))
    })?;
    let len = decode_usize_field(raw_len, field, parent_id)?;
    *index += 1;
    if result.len().saturating_sub(*index) < len {
        return Err(LaneError::Other(format!(
            "Redis flow dependency values script returned a truncated {field} payload for {parent_id}"
        )));
    }
    let start = *index;
    *index += len;
    Ok(&result[start..start + len])
}

fn decode_flow_dependency_page_result(
    result: &[String],
    parent_id: &str,
    kind: JobFlowDependencyKind,
    count: usize,
) -> Result<Option<JobFlowDependencyPage>> {
    match result.first().map(String::as_str) {
        Some("missing") => Ok(None),
        Some("ok") => {
            if result.len() < 3 {
                return Err(LaneError::Other(format!(
                    "Redis flow dependency page script returned {} fields for {parent_id}",
                    result.len()
                )));
            }
            let returned_kind = &result[1];
            if returned_kind != kind.as_str() {
                return Err(LaneError::Other(format!(
                    "Redis flow dependency page script returned `{returned_kind}` for requested `{}` on {parent_id}",
                    kind.as_str()
                )));
            }
            let next_cursor = decode_u64_field(&result[2], "dependency page cursor", parent_id)?;
            let payload = &result[3..];
            let items = decode_flow_dependency_page_items(kind, payload, parent_id)?;

            Ok(Some(JobFlowDependencyPage {
                kind,
                items,
                next_cursor,
                count,
            }))
        }
        Some("invalid_kind") => Err(LaneError::Other(format!(
            "Redis flow dependency page script rejected kind `{}` for {parent_id}",
            result
                .get(1)
                .map(String::as_str)
                .unwrap_or("<missing kind>")
        ))),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis flow dependency page script status `{other}` for {parent_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis flow dependency page script returned no status for {parent_id}"
        ))),
    }
}

fn decode_flow_dependency_pages_result(
    result: &[String],
    parent_id: &str,
) -> Result<Option<JobFlowDependencyPages>> {
    match result.first().map(String::as_str) {
        Some("missing") => Ok(None),
        Some("ok") => {
            let request_count = result
                .get(1)
                .ok_or_else(|| {
                    LaneError::Other(format!(
                        "Redis flow dependency pages script returned no request count for {parent_id}"
                    ))
                })
                .and_then(|raw| decode_usize_field(raw, "dependency page request", parent_id))?;
            let mut pages = JobFlowDependencyPages::default();
            let mut index = 2;
            for _ in 0..request_count {
                if result.len().saturating_sub(index) < 4 {
                    return Err(LaneError::Other(format!(
                        "Redis flow dependency pages script returned a truncated page header for {parent_id}"
                    )));
                }
                let kind = decode_flow_dependency_kind(&result[index], parent_id)?;
                let next_cursor =
                    decode_u64_field(&result[index + 1], "dependency page cursor", parent_id)?;
                let count =
                    decode_usize_field(&result[index + 2], "dependency page count", parent_id)?;
                let payload_len =
                    decode_usize_field(&result[index + 3], "dependency page payload", parent_id)?;
                index += 4;
                if result.len().saturating_sub(index) < payload_len {
                    return Err(LaneError::Other(format!(
                        "Redis flow dependency pages script returned a truncated {kind:?} payload for {parent_id}"
                    )));
                }
                let payload = &result[index..index + payload_len];
                let items = decode_flow_dependency_page_items(kind, payload, parent_id)?;
                pages.insert(JobFlowDependencyPage {
                    kind,
                    items,
                    next_cursor,
                    count,
                });
                index += payload_len;
            }
            if index != result.len() {
                return Err(LaneError::Other(format!(
                    "Redis flow dependency pages script returned {} trailing fields for {parent_id}",
                    result.len() - index
                )));
            }
            Ok(Some(pages))
        }
        Some("invalid_kind") => Err(LaneError::Other(format!(
            "Redis flow dependency pages script rejected kind `{}` for {parent_id}",
            result
                .get(1)
                .map(String::as_str)
                .unwrap_or("<missing kind>")
        ))),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis flow dependency pages script status `{other}` for {parent_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis flow dependency pages script returned no status for {parent_id}"
        ))),
    }
}

fn decode_flow_dependency_kind(raw: &str, parent_id: &str) -> Result<JobFlowDependencyKind> {
    match raw {
        "processed" => Ok(JobFlowDependencyKind::Processed),
        "unprocessed" => Ok(JobFlowDependencyKind::Unprocessed),
        "ignored" => Ok(JobFlowDependencyKind::Ignored),
        "failed" => Ok(JobFlowDependencyKind::Failed),
        other => Err(LaneError::Other(format!(
            "unexpected Redis flow dependency kind `{other}` for {parent_id}"
        ))),
    }
}

fn decode_flow_dependency_page_items(
    kind: JobFlowDependencyKind,
    payload: &[String],
    parent_id: &str,
) -> Result<Vec<JobFlowDependencyPageItem>> {
    match kind {
        JobFlowDependencyKind::Processed => {
            if !payload.len().is_multiple_of(2) {
                return Err(LaneError::Other(format!(
                    "Redis processed dependency page returned an odd pair count for {parent_id}"
                )));
            }
            payload
                .chunks_exact(2)
                .map(|pair| {
                    let value = serde_json::from_str(&pair[1]).map_err(|error| {
                        LaneError::Other(format!(
                            "failed to decode Redis processed dependency value for {} on {parent_id}: {error}",
                            pair[0]
                        ))
                    })?;
                    Ok(JobFlowDependencyPageItem::Processed {
                        child_id: pair[0].clone(),
                        value,
                    })
                })
                .collect()
        }
        JobFlowDependencyKind::Unprocessed => Ok(payload
            .iter()
            .map(|child_id| JobFlowDependencyPageItem::Unprocessed {
                child_id: child_id.clone(),
            })
            .collect()),
        JobFlowDependencyKind::Ignored => {
            if !payload.len().is_multiple_of(2) {
                return Err(LaneError::Other(format!(
                    "Redis ignored dependency page returned an odd pair count for {parent_id}"
                )));
            }
            Ok(payload
                .chunks_exact(2)
                .map(|pair| JobFlowDependencyPageItem::Ignored {
                    child_id: pair[0].clone(),
                    failed_reason: pair[1].clone(),
                })
                .collect())
        }
        JobFlowDependencyKind::Failed => Ok(payload
            .iter()
            .map(|child_id| JobFlowDependencyPageItem::Failed {
                child_id: child_id.clone(),
            })
            .collect()),
    }
}

fn decode_flow_children_values_result(
    result: &[String],
    parent_id: &str,
) -> Result<Option<JobFlowChildValues>> {
    match result.first().map(String::as_str) {
        Some("missing") => Ok(None),
        Some("ok") => {
            if result.len().is_multiple_of(2) {
                return Err(LaneError::Other(format!(
                    "Redis flow child values script returned an odd pair count for {parent_id}"
                )));
            }
            let mut values = BTreeMap::new();
            for pair in result[1..].chunks_exact(2) {
                let value = serde_json::from_str(&pair[1]).map_err(|error| {
                    LaneError::Other(format!(
                        "failed to decode Redis flow child value for {} on {parent_id}: {error}",
                        pair[0]
                    ))
                })?;
                values.insert(pair[0].clone(), value);
            }
            Ok(Some(values))
        }
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis flow child values script status `{other}` for {parent_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis flow child values script returned no status for {parent_id}"
        ))),
    }
}

fn decode_flow_ignored_children_failures_result(
    result: &[String],
    parent_id: &str,
) -> Result<Option<JobFlowIgnoredFailures>> {
    match result.first().map(String::as_str) {
        Some("missing") => Ok(None),
        Some("ok") => {
            if result.len().is_multiple_of(2) {
                return Err(LaneError::Other(format!(
                    "Redis flow ignored child failures script returned an odd pair count for {parent_id}"
                )));
            }
            let mut failures = BTreeMap::new();
            for pair in result[1..].chunks_exact(2) {
                failures.insert(pair[0].clone(), pair[1].clone());
            }
            Ok(Some(failures))
        }
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis flow ignored child failures script status `{other}` for {parent_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis flow ignored child failures script returned no status for {parent_id}"
        ))),
    }
}

fn decode_usize_field(raw: &str, field: &str, owner_id: &str) -> Result<usize> {
    raw.parse::<usize>().map_err(|error| {
        LaneError::Other(format!(
            "failed to decode Redis {field} count for {owner_id}: {error}"
        ))
    })
}

fn decode_u64_field(raw: &str, field: &str, owner_id: &str) -> Result<u64> {
    raw.parse::<u64>().map_err(|error| {
        LaneError::Other(format!(
            "failed to decode Redis {field} for {owner_id}: {error}"
        ))
    })
}

fn decode_remove_unprocessed_children_result(
    result: &[String],
    parent_id: &str,
) -> Result<Option<Vec<Job>>> {
    match result.first().map(String::as_str) {
        Some("missing") => Ok(None),
        Some("ok") => result
            .iter()
            .skip(1)
            .map(|raw| decode_job(raw))
            .collect::<Result<Vec<_>>>()
            .map(Some),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis remove unprocessed children script status `{other}` for {parent_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis remove unprocessed children script returned no status for {parent_id}"
        ))),
    }
}

fn decode_remove_child_dependency_result(result: &[String], child_id: &str) -> Result<bool> {
    match result.first().map(String::as_str) {
        Some("ok") => Ok(true),
        Some("no_relationship") => Ok(false),
        Some("missing_child") => Err(LaneError::JobNotFound(child_id.to_string())),
        Some("missing_parent") => {
            let parent_id = result
                .get(1)
                .cloned()
                .unwrap_or_else(|| "unknown parent".to_string());
            Err(LaneError::JobNotFound(parent_id))
        }
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis remove child dependency script status `{other}` for {child_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis remove child dependency script returned no status for {child_id}"
        ))),
    }
}

fn decode_flow_dependencies_result(
    result: &[String],
    parent_id: &str,
) -> Result<Option<JobFlowDependencies>> {
    match result.first().map(String::as_str) {
        Some("missing") => Ok(None),
        Some("ok") => {
            let parent_raw = result.get(1).ok_or_else(|| {
                LaneError::Other(format!(
                    "Redis flow dependency script returned no parent payload for {parent_id}"
                ))
            })?;
            let parent = decode_job(parent_raw)?;
            let child_pairs = result.get(2..).unwrap_or_default();
            if child_pairs.len() % 2 != 0 {
                return Err(LaneError::Other(format!(
                    "Redis flow dependency script returned an odd child payload count for {parent_id}"
                )));
            }

            let mut children = Vec::new();
            let mut pending_child_ids = Vec::new();
            let mut missing_child_ids = Vec::new();
            for pair in child_pairs.chunks(2) {
                let child_id = &pair[0];
                let child_raw = &pair[1];
                if child_raw.is_empty() {
                    missing_child_ids.push(child_id.clone());
                    continue;
                }

                let child = decode_job(child_raw)?;
                if !child.state.is_terminal() {
                    pending_child_ids.push(child.id.clone());
                }
                children.push(child);
            }

            Ok(Some(JobFlowDependencies {
                parent,
                children,
                pending_child_ids,
                missing_child_ids,
            }))
        }
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis flow dependency script status `{other}` for {parent_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis flow dependency script returned no status for {parent_id}"
        ))),
    }
}

fn decode_job_state(state: &str) -> Result<Option<JobState>> {
    match state {
        "waiting" => Ok(Some(JobState::Waiting)),
        "delayed" => Ok(Some(JobState::Delayed)),
        "active" => Ok(Some(JobState::Active)),
        "waiting_children" => Ok(Some(JobState::WaitingChildren)),
        "completed" => Ok(Some(JobState::Completed)),
        "failed" => Ok(Some(JobState::Failed)),
        "unknown" => Ok(None),
        other => Err(LaneError::Other(format!(
            "unexpected Redis job state `{other}`"
        ))),
    }
}

fn decode_job_finished_result(
    result: &[String],
    job_id: &str,
) -> Result<Option<JobFinishedResult>> {
    match result.first().map(String::as_str) {
        Some("missing") => Ok(None),
        Some("not_finished") => Ok(Some(JobFinishedResult::NotFinished)),
        Some("completed") => {
            let return_value = result
                .get(1)
                .map(|raw| {
                    serde_json::from_str(raw).map_err(|error| {
                        LaneError::Other(format!(
                            "failed to decode Redis job completion value for {job_id}: {error}"
                        ))
                    })
                })
                .transpose()?;
            Ok(Some(JobFinishedResult::Completed { return_value }))
        }
        Some("failed") => Ok(Some(JobFinishedResult::Failed {
            failed_reason: result.get(1).cloned(),
        })),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis job finished script status `{other}` for {job_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis job finished script returned no status for {job_id}"
        ))),
    }
}

fn decode_log_entries(raw_logs: Vec<String>) -> Result<Vec<JobLogEntry>> {
    raw_logs
        .into_iter()
        .map(|raw| {
            serde_json::from_str(&raw).map_err(|error| {
                LaneError::Other(format!("failed to decode Redis job log entry: {error}"))
            })
        })
        .collect()
}

fn decode_event_stream(reply: StreamRangeReply) -> Result<Vec<JobEvent>> {
    reply
        .ids
        .into_iter()
        .map(|entry| {
            let event = stream_string_field(&entry.map, "event").ok_or_else(|| {
                LaneError::Other(format!(
                    "Redis event stream entry {} has no event",
                    entry.id
                ))
            })?;
            let timestamp = stream_id_timestamp(&entry.id);
            let job_id = stream_string_field(&entry.map, "jobId");
            let prev = stream_string_field(&entry.map, "prev")
                .as_deref()
                .map(decode_event_prev_state)
                .transpose()?;
            let mut fields = BTreeMap::new();
            for (key, value) in entry.map {
                if matches!(key.as_str(), "event" | "jobId" | "prev") {
                    continue;
                }
                fields.insert(key, stream_field_value(&value));
            }
            Ok(JobEvent {
                id: entry.id,
                event,
                timestamp,
                job_id,
                prev,
                fields,
            })
        })
        .collect()
}

fn stream_string_field(
    map: &std::collections::HashMap<String, redis::Value>,
    key: &str,
) -> Option<String> {
    map.get(key)
        .and_then(|value| redis::from_redis_value::<String>(value).ok())
}

fn stream_field_value(value: &redis::Value) -> Value {
    if let Ok(raw) = redis::from_redis_value::<String>(value) {
        serde_json::from_str(&raw).unwrap_or(Value::String(raw))
    } else if let Ok(number) = redis::from_redis_value::<i64>(value) {
        Value::from(number)
    } else {
        Value::Null
    }
}

fn stream_id_timestamp(id: &str) -> DateTime<Utc> {
    let millis = id
        .split_once('-')
        .and_then(|(millis, _)| millis.parse::<i64>().ok())
        .unwrap_or_default();
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap_or_else(Utc::now)
}

fn decode_event_prev_state(state: &str) -> Result<JobState> {
    match state {
        "waiting" => Ok(JobState::Waiting),
        "delayed" => Ok(JobState::Delayed),
        "active" => Ok(JobState::Active),
        "waiting_children" | "waiting-children" => Ok(JobState::WaitingChildren),
        "completed" => Ok(JobState::Completed),
        "failed" => Ok(JobState::Failed),
        other => Err(LaneError::Other(format!(
            "unexpected Redis event prev state `{other}`"
        ))),
    }
}

fn decode_clear_logs_result(result: &[String], job_id: &str) -> Result<JobLogPage> {
    let Some(raw_count) = result.first() else {
        return Err(LaneError::Other(format!(
            "Redis clear logs script returned no count for {job_id}"
        )));
    };
    let count = decode_usize_field(raw_count, "log", job_id)?;
    let logs = decode_log_entries(result.get(1..).unwrap_or_default().to_vec())?;
    if logs.len() > count {
        return Err(LaneError::Other(format!(
            "Redis clear logs script returned {} logs but count was {count} for {job_id}",
            logs.len()
        )));
    }

    Ok(JobLogPage { logs, count })
}

fn decode_transition_result(result: &[String], job_id: &str, action: &str) -> Result<Job> {
    match result.first().map(String::as_str) {
        Some("ok") => {
            let raw = result.get(1).ok_or_else(|| {
                LaneError::Other(format!("Redis job {action} script returned no job payload"))
            })?;
            decode_job(raw)
        }
        Some("missing") => Err(LaneError::JobNotFound(job_id.to_string())),
        Some("state") => Err(LaneError::JobStateConflict(format!(
            "cannot {action} job {job_id} from state {}",
            result.get(1).map(String::as_str).unwrap_or("unknown")
        ))),
        Some("pending_dependencies") => Err(LaneError::JobStateConflict(format!(
            "cannot {action} job {job_id}; it has pending flow dependencies"
        ))),
        Some("failed_dependencies") => Err(LaneError::JobStateConflict(format!(
            "cannot {action} job {job_id}; it has failed flow dependencies"
        ))),
        Some("lock_missing") => Err(LaneError::JobLeaseConflict(format!(
            "missing lock for job {job_id}"
        ))),
        Some("lock_mismatch") => Err(LaneError::JobLeaseConflict(format!(
            "lock token does not own job {job_id}"
        ))),
        Some("deduplicated") => Err(LaneError::JobStateConflict(format!(
            "cannot {action} job {job_id}; deduplication id is active on job {}",
            result.get(1).map(String::as_str).unwrap_or("unknown")
        ))),
        Some("repeat") => Err(LaneError::JobStateConflict(format!(
            "cannot {action} job {job_id}; repeat key is active on job {}",
            result.get(1).map(String::as_str).unwrap_or("unknown")
        ))),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis job {action} script status `{other}`"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis job {action} script returned no status"
        ))),
    }
}

fn decode_remove_job_result(result: &[String], job_id: &str) -> Result<Option<Job>> {
    match result.first().map(String::as_str) {
        Some("ok") => {
            let raw = result.get(1).ok_or_else(|| {
                LaneError::Other(format!(
                    "Redis remove job script returned no payload for {job_id}"
                ))
            })?;
            decode_job(raw).map(Some)
        }
        Some("missing") => Ok(None),
        Some("active") => Err(LaneError::JobLeaseConflict(format!(
            "cannot remove active leased job {job_id}"
        ))),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis remove job script status `{other}` for {job_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis remove job script returned no status for {job_id}"
        ))),
    }
}

fn decode_clean_jobs_result(result: &[String]) -> Result<Vec<Job>> {
    if result.first().map(String::as_str) == Some("bad_state") {
        return Err(LaneError::Other(
            "Redis clean jobs script received an unknown state".to_string(),
        ));
    }

    result.iter().map(|raw| decode_job(raw)).collect()
}

fn decode_list_jobs_result(
    result: &[redis::Value],
    offset: usize,
    limit: usize,
) -> Result<JobListPage> {
    if result.is_empty() {
        return Err(LaneError::Other(
            "Redis list jobs script returned no values".to_string(),
        ));
    }

    if matches!(decode_redis_string(&result[0]).as_deref(), Ok("bad_state")) {
        return Err(LaneError::Other(
            "Redis list jobs script received an unknown state".to_string(),
        ));
    }

    let total = redis::from_redis_value::<usize>(&result[0]).map_err(redis_error)?;
    let jobs = result[1..]
        .iter()
        .map(|value| {
            let raw = decode_redis_string(value)?;
            decode_job(&raw)
        })
        .collect::<Result<Vec<_>>>()?;

    if limit > 0 && jobs.len() > limit {
        return Err(LaneError::Other(format!(
            "Redis list jobs script returned {} jobs, expected at most {limit}",
            jobs.len()
        )));
    }

    Ok(JobListPage {
        jobs,
        total,
        offset,
        limit,
    })
}

fn decode_stats_result(result: &[i64]) -> Result<JobQueueStats> {
    if result.len() != 7 {
        return Err(LaneError::Other(format!(
            "Redis stats script returned {} values, expected 7",
            result.len()
        )));
    }

    let waiting = decode_stats_count(result[0], "waiting")?;
    let delayed = decode_stats_count(result[1], "delayed")?;
    let active = decode_stats_count(result[2], "active")?;
    let waiting_children = decode_stats_count(result[3], "waiting_children")?;
    let completed = decode_stats_count(result[4], "completed")?;
    let failed = decode_stats_count(result[5], "failed")?;
    let paused_flag = result[6];
    if paused_flag < 0 {
        return Err(LaneError::Other(format!(
            "Redis stats script returned negative paused flag: {paused_flag}"
        )));
    }

    let total = [
        waiting,
        delayed,
        active,
        waiting_children,
        completed,
        failed,
    ]
    .into_iter()
    .try_fold(0usize, |total, count| {
        total.checked_add(count).ok_or_else(|| {
            LaneError::Other("Redis stats script returned counts that overflow usize".to_string())
        })
    })?;

    Ok(JobQueueStats {
        total,
        waiting,
        delayed,
        active,
        waiting_children,
        completed,
        failed,
        paused: paused_flag != 0,
    })
}

fn decode_priority_counts(
    priorities: &[JobPriority],
    result: &[i64],
) -> Result<Vec<JobPriorityCount>> {
    if result.len() != priorities.len() {
        return Err(LaneError::Other(format!(
            "Redis priority count script returned {} values, expected {}",
            result.len(),
            priorities.len()
        )));
    }

    priorities
        .iter()
        .zip(result.iter())
        .map(|(&priority, &count)| {
            Ok(JobPriorityCount {
                priority,
                count: decode_stats_count(count, "priority")?,
            })
        })
        .collect()
}

fn decode_metrics_result(value: &redis::Value, state: JobState) -> Result<JobMetrics> {
    terminal_metrics_suffix(state)?;
    let redis::Value::Array(items) = value else {
        return Err(LaneError::Other(
            "Redis metrics script returned a non-array payload".to_string(),
        ));
    };
    if items.len() != 3 {
        return Err(LaneError::Other(format!(
            "Redis metrics script returned {} values, expected 3",
            items.len()
        )));
    }

    let redis::Value::Array(meta_values) = &items[0] else {
        return Err(LaneError::Other(
            "Redis metrics script returned non-array metadata".to_string(),
        ));
    };
    if meta_values.len() != 3 {
        return Err(LaneError::Other(format!(
            "Redis metrics metadata returned {} values, expected 3",
            meta_values.len()
        )));
    }

    let redis::Value::Array(data_values) = &items[1] else {
        return Err(LaneError::Other(
            "Redis metrics script returned non-array data points".to_string(),
        ));
    };

    let meta = JobMetricsMeta {
        count: decode_optional_usize_value(&meta_values[0], "metrics count")?,
        previous_timestamp_millis: decode_optional_i64_value(
            &meta_values[1],
            "metrics previous timestamp",
        )?,
        previous_count: decode_optional_usize_value(&meta_values[2], "metrics previous count")?,
    };
    let data = data_values
        .iter()
        .map(|value| decode_optional_usize_value(value, "metrics data point"))
        .collect::<Result<Vec<_>>>()?;
    let count = redis::from_redis_value::<usize>(&items[2]).map_err(redis_error)?;

    Ok(JobMetrics { meta, data, count })
}

fn decode_optional_usize_value(value: &redis::Value, field: &str) -> Result<usize> {
    match value {
        redis::Value::Nil => Ok(0),
        _ => redis::from_redis_value::<usize>(value)
            .map_err(|error| LaneError::Other(format!("failed to decode Redis {field}: {error}"))),
    }
}

fn decode_optional_i64_value(value: &redis::Value, field: &str) -> Result<i64> {
    match value {
        redis::Value::Nil => Ok(0),
        _ => redis::from_redis_value::<i64>(value)
            .map_err(|error| LaneError::Other(format!("failed to decode Redis {field}: {error}"))),
    }
}

fn decode_state_counts(states: &[JobState], result: &[i64]) -> Result<Vec<JobStateCount>> {
    if result.len() != states.len() {
        return Err(LaneError::Other(format!(
            "Redis job count script returned {} values, expected {}",
            result.len(),
            states.len()
        )));
    }

    states
        .iter()
        .zip(result.iter())
        .map(|(&state, &count)| {
            Ok(JobStateCount {
                state,
                count: decode_stats_count(count, job_state_name(state))?,
            })
        })
        .collect()
}

fn decode_stats_count(value: i64, field: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        LaneError::Other(format!(
            "Redis stats script returned negative {field} count: {value}"
        ))
    })
}

fn decode_redis_string(value: &redis::Value) -> Result<String> {
    redis::from_redis_value::<String>(value).map_err(redis_error)
}

fn normalize_lua_empty_array(value: &mut Value, field: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if matches!(object.get(field), Some(Value::Object(map)) if map.is_empty()) {
        object.insert(field.to_string(), Value::Array(Vec::new()));
    }
}

fn redis_error(error: redis::RedisError) -> LaneError {
    LaneError::Other(format!("Redis job backend error: {error}"))
}

fn marker_block_timeout(
    block_until_millis: Option<i64>,
    remaining: Duration,
    now: DateTime<Utc>,
) -> Duration {
    if remaining.is_zero() {
        return Duration::ZERO;
    }

    let mut timeout = remaining.min(MAX_MARKER_BLOCK_TIMEOUT);
    if let Some(block_until_millis) = block_until_millis {
        let delay_millis = block_until_millis.saturating_sub(now.timestamp_millis());
        if delay_millis <= 0 {
            return Duration::ZERO;
        }
        timeout = timeout.min(Duration::from_millis(delay_millis as u64));
    }

    if timeout < MIN_MARKER_BLOCK_TIMEOUT && remaining >= MIN_MARKER_BLOCK_TIMEOUT {
        MIN_MARKER_BLOCK_TIMEOUT
    } else {
        timeout
    }
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

fn unique_priorities(priorities: &[JobPriority]) -> Vec<JobPriority> {
    let mut unique = Vec::new();
    for &priority in priorities {
        if !unique.contains(&priority) {
            unique.push(priority);
        }
    }
    unique
}

fn unique_states(states: &[JobState]) -> Vec<JobState> {
    let states = if states.is_empty() {
        JobState::ALL.as_slice()
    } else {
        states
    };
    let mut unique = Vec::new();
    for &state in states {
        if !unique.contains(&state) {
            unique.push(state);
        }
    }
    unique
}

#[cfg(test)]
fn waiting_score(priority: JobPriority, sequence: u64, lifo: bool) -> f64 {
    let half_bucket = (WAITING_SCORE_BUCKET / 2.0) as u64;
    let sequence_index = sequence % half_bucket;
    let relative_score = if lifo {
        half_bucket - 1 - sequence_index
    } else {
        half_bucket + sequence_index
    };
    (priority as f64 * WAITING_SCORE_BUCKET) + relative_score as f64
}

fn job_state_name(state: JobState) -> &'static str {
    match state {
        JobState::Waiting => "waiting",
        JobState::Delayed => "delayed",
        JobState::Active => "active",
        JobState::WaitingChildren => "waiting_children",
        JobState::Completed => "completed",
        JobState::Failed => "failed",
    }
}

fn terminal_metrics_suffix(state: JobState) -> Result<&'static str> {
    match state {
        JobState::Completed => Ok("completed"),
        JobState::Failed => Ok("failed"),
        other => Err(LaneError::ConfigError(format!(
            "job metrics are only available for completed or failed jobs, got {}",
            job_state_name(other)
        ))),
    }
}

fn lock_duration_millis(duration: Duration) -> u64 {
    duration_millis(duration).max(1)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn should_retry(job: &Job) -> bool {
    job.options.retry_policy.max_retries > 0
        && job.attempts_made <= job.options.retry_policy.max_retries
}

fn state_after_dependencies(scheduled_at: DateTime<Utc>, now: DateTime<Utc>) -> JobState {
    if scheduled_at > now {
        JobState::Delayed
    } else {
        JobState::Waiting
    }
}

fn validate_job_options(options: &JobOptions) -> Result<()> {
    options.validate()
}

fn validate_job_options_at(options: &JobOptions, now: DateTime<Utc>) -> Result<()> {
    validate_job_options(options)?;
    if matches!(options.repeat.as_ref().and_then(|repeat| repeat.end_at), Some(end_at) if end_at < now)
    {
        return Err(LaneError::ConfigError(
            "repeat end_at must not be earlier than the add timestamp".to_string(),
        ));
    }
    Ok(())
}

fn validate_flow_job_ids(parent: &Job, children: &[Job]) -> Result<()> {
    let mut ids = HashSet::with_capacity(children.len() + 1);
    for id in std::iter::once(&parent.id).chain(children.iter().map(|job| &job.id)) {
        if !ids.insert(id) {
            return Err(LaneError::ConfigError(format!(
                "flow contains duplicate job id `{id}`"
            )));
        }
    }
    Ok(())
}

fn validate_flow_child_jobs(parent_id: &str, children: &[Job]) -> Result<()> {
    let mut ids = HashSet::with_capacity(children.len() + 1);
    ids.insert(parent_id);
    for child in children {
        if !ids.insert(child.id.as_str()) {
            return Err(LaneError::ConfigError(format!(
                "flow contains duplicate job id `{}`",
                child.id
            )));
        }
    }
    Ok(())
}

fn next_repeat_job(job: &Job, now: DateTime<Utc>) -> Result<Option<Job>> {
    let Some(repeat) = job.options.repeat.as_ref() else {
        return Ok(None);
    };
    let next_count = job.repeat_count.saturating_add(1);
    if matches!(repeat.limit, Some(limit) if next_count >= limit) {
        return Ok(None);
    }

    let Some(scheduled_at) = repeat.next_scheduled_at(now)? else {
        return Ok(None);
    };
    if matches!(repeat.end_at, Some(end_at) if scheduled_at > end_at) {
        return Ok(None);
    }

    let mut options = job.options.clone();
    options.job_id = None;
    let mut next = Job::new(
        job.queue.clone(),
        job.name.clone(),
        job.payload.clone(),
        options,
        now,
    );
    next.scheduled_at = scheduled_at;
    next.state = state_after_dependencies(scheduled_at, now);
    next.repeat_key = job.repeat_key.clone();
    next.repeat_count = next_count;
    Ok(Some(next))
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
        assert_eq!(queue.stalled_key(), "test:lane:email:stalled");
    }

    #[test]
    fn waiting_score_preserves_priority_then_lifo_fifo_position() {
        assert!(waiting_score(1, 99, false) < waiting_score(2, 1, true));
        assert!(waiting_score(5, 2, true) < waiting_score(5, 1, true));
        assert!(waiting_score(5, 1, true) < waiting_score(5, 1, false));
        assert!(waiting_score(5, 1, false) < waiting_score(5, 2, false));
    }

    #[test]
    fn subtract_duration_saturates_on_invalid_duration() {
        let now = Utc.timestamp_millis_opt(10_000).unwrap();
        assert_eq!(
            subtract_duration(now, Duration::from_millis(250)),
            Utc.timestamp_millis_opt(9_750).unwrap()
        );
    }

    #[test]
    fn claim_rate_limit_rejects_invalid_values() {
        let queue = RedisJobQueue::with_namespace("redis://127.0.0.1/", "test:lane", "email")
            .expect("valid Redis URL should build a queue client");

        let zero_max = queue
            .clone()
            .with_claim_rate_limit(JobRateLimit::new(0, Duration::from_secs(1)))
            .err()
            .expect("zero max should be rejected");
        assert!(matches!(zero_max, LaneError::ConfigError(_)));

        let zero_window = queue
            .with_claim_rate_limit(JobRateLimit::new(1, Duration::ZERO))
            .err()
            .expect("zero window should be rejected");
        assert!(matches!(zero_window, LaneError::ConfigError(_)));
    }

    #[test]
    fn flow_dependency_page_script_scans_bullmq_side_indexes() {
        assert!(FLOW_DEPENDENCY_SELECTED_COUNTS_SCRIPT.contains("redis.call('HLEN', KEYS[3])"));
        assert!(FLOW_DEPENDENCY_SELECTED_COUNTS_SCRIPT.contains("redis.call('SCARD', KEYS[2])"));
        assert!(FLOW_DEPENDENCY_SELECTED_COUNTS_SCRIPT.contains("redis.call('HLEN', KEYS[4])"));
        assert!(FLOW_DEPENDENCY_SELECTED_COUNTS_SCRIPT.contains("redis.call('ZCARD', KEYS[5])"));
        assert!(FLOW_DEPENDENCY_VALUES_SCRIPT.contains("redis.call('HGETALL', KEYS[3]"));
        assert!(FLOW_DEPENDENCY_VALUES_SCRIPT.contains("redis.call('SMEMBERS', KEYS[2]"));
        assert!(FLOW_DEPENDENCY_VALUES_SCRIPT.contains("redis.call('HGETALL', KEYS[4]"));
        assert!(FLOW_DEPENDENCY_VALUES_SCRIPT.contains("redis.call('ZRANGE', KEYS[5], 0, -1)"));
        assert!(FLOW_DEPENDENCY_VALUES_SCRIPT
            .contains("for _, child_id in ipairs(parent['child_ids'] or {}) do"));
        assert!(FLOW_DEPENDENCY_VALUES_SCRIPT.contains("redis.call('HGET', KEYS[1], child_id)"));
        assert!(FLOW_DEPENDENCY_PAGE_SCRIPT.contains("append_retained_dependency_fallback"));
        assert!(FLOW_DEPENDENCY_PAGE_SCRIPT.contains("if cursor ~= 0 then"));
        assert!(FLOW_DEPENDENCY_PAGES_SCRIPT.contains("append_retained_dependency_fallback"));
        assert!(FLOW_DEPENDENCY_PAGES_SCRIPT.contains("if cursor ~= 0 then"));
        assert!(FLOW_DEPENDENCY_PAGE_SCRIPT.contains("redis.call('HSCAN', KEYS[3]"));
        assert!(FLOW_DEPENDENCY_PAGE_SCRIPT.contains("redis.call('SSCAN', KEYS[2]"));
        assert!(FLOW_DEPENDENCY_PAGE_SCRIPT.contains("redis.call('HSCAN', KEYS[4]"));
        assert!(FLOW_DEPENDENCY_PAGE_SCRIPT.contains("redis.call('ZRANGE', KEYS[5]"));
        assert!(FLOW_DEPENDENCY_PAGES_SCRIPT.contains("redis.call('HSCAN', KEYS[3]"));
        assert!(FLOW_DEPENDENCY_PAGES_SCRIPT.contains("redis.call('SSCAN', KEYS[2]"));
        assert!(FLOW_DEPENDENCY_PAGES_SCRIPT.contains("redis.call('HSCAN', KEYS[4]"));
        assert!(FLOW_DEPENDENCY_PAGES_SCRIPT.contains("redis.call('ZRANGE', KEYS[5]"));
    }

    #[test]
    fn decode_flow_dependency_selected_counts_result_builds_requested_counts() {
        let counts = decode_flow_dependency_selected_counts_result(
            &[
                "ok".to_string(),
                "processed".to_string(),
                "3".to_string(),
                "failed".to_string(),
                "1".to_string(),
            ],
            "parent",
        )
        .expect("selected counts should decode")
        .expect("selected counts should exist");

        assert_eq!(counts.processed, Some(3));
        assert_eq!(counts.failed, Some(1));
        assert_eq!(counts.unprocessed, None);
        assert_eq!(counts.ignored, None);

        let missing = decode_flow_dependency_selected_counts_result(
            &["missing".to_string()],
            "missing-parent",
        )
        .expect("missing parent should decode");
        assert!(missing.is_none());
    }

    #[test]
    fn decode_flow_dependency_values_result_builds_bullmq_buckets() {
        let values = decode_flow_dependency_values_result(
            &[
                "ok".to_string(),
                "2".to_string(),
                "child-a".to_string(),
                serde_json::json!({ "ok": true }).to_string(),
                "1".to_string(),
                "child-b".to_string(),
                "2".to_string(),
                "child-c".to_string(),
                "optional child failed".to_string(),
                "1".to_string(),
                "child-d".to_string(),
            ],
            "parent",
        )
        .expect("dependency values should decode")
        .expect("dependency values should exist");

        assert_eq!(
            values.processed.get("child-a"),
            Some(&serde_json::json!({ "ok": true }))
        );
        assert_eq!(values.unprocessed, vec!["child-b".to_string()]);
        assert_eq!(
            values.ignored.get("child-c").map(String::as_str),
            Some("optional child failed")
        );
        assert_eq!(values.failed, vec!["child-d".to_string()]);

        let missing =
            decode_flow_dependency_values_result(&["missing".to_string()], "missing-parent")
                .expect("missing parent should decode");
        assert!(missing.is_none());
    }

    #[test]
    fn decode_flow_dependency_page_result_builds_bucket_items() {
        let processed = decode_flow_dependency_page_result(
            &[
                "ok".to_string(),
                "processed".to_string(),
                "8".to_string(),
                "child-a".to_string(),
                serde_json::json!({ "ok": true }).to_string(),
            ],
            "parent",
            JobFlowDependencyKind::Processed,
            20,
        )
        .expect("processed page should decode")
        .expect("processed page should exist");
        assert_eq!(processed.next_cursor, 8);
        assert_eq!(
            processed.items,
            vec![JobFlowDependencyPageItem::Processed {
                child_id: "child-a".to_string(),
                value: serde_json::json!({ "ok": true }),
            }]
        );

        let unprocessed = decode_flow_dependency_page_result(
            &[
                "ok".to_string(),
                "unprocessed".to_string(),
                "0".to_string(),
                "child-b".to_string(),
            ],
            "parent",
            JobFlowDependencyKind::Unprocessed,
            20,
        )
        .expect("unprocessed page should decode")
        .expect("unprocessed page should exist");
        assert_eq!(
            unprocessed.items,
            vec![JobFlowDependencyPageItem::Unprocessed {
                child_id: "child-b".to_string(),
            }]
        );

        let ignored = decode_flow_dependency_page_result(
            &[
                "ok".to_string(),
                "ignored".to_string(),
                "0".to_string(),
                "child-c".to_string(),
                "optional child failed".to_string(),
            ],
            "parent",
            JobFlowDependencyKind::Ignored,
            20,
        )
        .expect("ignored page should decode")
        .expect("ignored page should exist");
        assert_eq!(
            ignored.items,
            vec![JobFlowDependencyPageItem::Ignored {
                child_id: "child-c".to_string(),
                failed_reason: "optional child failed".to_string(),
            }]
        );

        let failed = decode_flow_dependency_page_result(
            &[
                "ok".to_string(),
                "failed".to_string(),
                "1".to_string(),
                "child-d".to_string(),
            ],
            "parent",
            JobFlowDependencyKind::Failed,
            1,
        )
        .expect("failed page should decode")
        .expect("failed page should exist");
        assert_eq!(failed.next_cursor, 1);
        assert_eq!(
            failed.items,
            vec![JobFlowDependencyPageItem::Failed {
                child_id: "child-d".to_string(),
            }]
        );

        let missing = decode_flow_dependency_page_result(
            &["missing".to_string()],
            "missing-parent",
            JobFlowDependencyKind::Processed,
            20,
        )
        .expect("missing parent should decode");
        assert!(missing.is_none());
    }

    #[test]
    fn decode_flow_dependency_pages_result_builds_requested_buckets() {
        let pages = decode_flow_dependency_pages_result(
            &[
                "ok".to_string(),
                "3".to_string(),
                "processed".to_string(),
                "0".to_string(),
                "20".to_string(),
                "2".to_string(),
                "child-a".to_string(),
                serde_json::json!({ "ok": true }).to_string(),
                "ignored".to_string(),
                "0".to_string(),
                "20".to_string(),
                "2".to_string(),
                "child-b".to_string(),
                "optional child failed".to_string(),
                "failed".to_string(),
                "1".to_string(),
                "1".to_string(),
                "1".to_string(),
                "child-c".to_string(),
            ],
            "parent",
        )
        .expect("multi dependency pages should decode")
        .expect("multi dependency pages should exist");

        assert_eq!(
            pages.get(JobFlowDependencyKind::Processed).unwrap().items,
            vec![JobFlowDependencyPageItem::Processed {
                child_id: "child-a".to_string(),
                value: serde_json::json!({ "ok": true }),
            }]
        );
        assert_eq!(pages.get(JobFlowDependencyKind::Failed).unwrap().count, 1);
        assert_eq!(
            pages
                .get(JobFlowDependencyKind::Failed)
                .unwrap()
                .next_cursor,
            1
        );
        assert_eq!(
            pages.get(JobFlowDependencyKind::Ignored).unwrap().items,
            vec![JobFlowDependencyPageItem::Ignored {
                child_id: "child-b".to_string(),
                failed_reason: "optional child failed".to_string(),
            }]
        );
        assert!(pages.get(JobFlowDependencyKind::Unprocessed).is_none());

        let missing =
            decode_flow_dependency_pages_result(&["missing".to_string()], "missing-parent")
                .expect("missing parent should decode");
        assert!(missing.is_none());
    }

    #[test]
    fn flow_parent_failure_scripts_use_retention_helpers() {
        for script in [
            REMOVE_JOB_SCRIPT,
            CLEAN_JOBS_SCRIPT,
            DRAIN_JOBS_SCRIPT,
            REMOVE_UNPROCESSED_CHILDREN_SCRIPT,
            REMOVE_CHILD_DEPENDENCY_SCRIPT,
        ] {
            assert!(script.contains("failure_retention"));
            assert!(script.contains("apply_failed_retention"));
            assert!(!script.contains(
                "parent[\"options\"] and parent[\"options\"][\"remove_on_fail\"] == true"
            ));
        }
        assert!(REMOVE_UNPROCESSED_CHILDREN_SCRIPT.contains(
            "redis.call('XADD', KEYS[10], 'MAXLEN', '~', ARGV[11], '*', 'event', 'removed'"
        ));
    }

    #[test]
    fn remove_child_dependency_script_clears_dependency_side_buckets() {
        assert!(REMOVE_CHILD_DEPENDENCY_SCRIPT
            .contains("local processed_removed = redis.call('HDEL', dependency_key .. ':processed', ARGV[1]) == 1"));
        assert!(REMOVE_CHILD_DEPENDENCY_SCRIPT.contains(
            "local failed_removed = redis.call('HDEL', dependency_key .. ':failed', ARGV[1]) == 1"
        ));
        assert!(REMOVE_CHILD_DEPENDENCY_SCRIPT
            .contains("local unsuccessful_removed = redis.call('ZREM', dependency_key .. ':unsuccessful', ARGV[1]) == 1"));
        assert!(REMOVE_CHILD_DEPENDENCY_SCRIPT.contains("and not processed_removed"));
        assert!(REMOVE_CHILD_DEPENDENCY_SCRIPT.contains("and not failed_removed"));
        assert!(REMOVE_CHILD_DEPENDENCY_SCRIPT.contains("and not unsuccessful_removed"));
    }

    #[test]
    fn clean_jobs_script_cleans_only_unlocked_active_jobs() {
        assert!(CLEAN_JOBS_SCRIPT.contains(
            "local active_locked = state == 'active' and redis.call('EXISTS', lock_prefix .. id) == 1"
        ));
        assert!(CLEAN_JOBS_SCRIPT.contains("if job[\"state\"] == state then"));
        assert!(CLEAN_JOBS_SCRIPT.contains("if not active_locked then"));
        assert!(!CLEAN_JOBS_SCRIPT.contains("job[\"state\"] == state and not active_locked"));
        assert!(!CLEAN_JOBS_SCRIPT.contains("if state == 'active' or limit <= 0 then"));
        assert!(CLEAN_JOBS_SCRIPT.contains("redis.call('SREM', KEYS[11], candidate.id)"));
    }

    #[test]
    fn remove_job_script_rejects_only_locked_active_jobs() {
        assert!(REMOVE_JOB_SCRIPT
            .contains("job[\"state\"] == \"active\" and redis.call('EXISTS', KEYS[2]) == 1"));
        assert!(REMOVE_JOB_SCRIPT.contains("redis.call('SREM', KEYS[12], ARGV[1])"));
        assert!(REMOVE_JOB_SCRIPT.contains("redis.call('SREM', KEYS[12], job_id)"));
    }

    #[test]
    fn stalled_recovery_scripts_use_bullmq_candidate_set_mechanism() {
        assert!(RECOVER_STALLED_SCRIPT.contains("redis.call('SPOP', KEYS[8]"));
        assert!(RECOVER_STALLED_SCRIPT.contains("mark_active_jobs_stalled(KEYS[2], KEYS[8])"));
        assert!(!RECOVER_STALLED_SCRIPT
            .contains("redis.call('ZRANGEBYSCORE', KEYS[2], '-inf', ARGV[1]"));

        assert!(RENEW_LEASE_SCRIPT.contains("redis.call('SREM', KEYS[4], ARGV[1])"));
        assert!(RENEW_LEASES_SCRIPT.contains("redis.call('SREM', stalled_key, job_id)"));
        assert!(RENEW_LEASES_SCRIPT.contains("failed[#failed + 1] = job_id"));
        assert!(COMPLETE_SCRIPT.contains("redis.call('SREM', KEYS[11], ARGV[1])"));
        assert!(FAIL_SCRIPT.contains("redis.call('SREM', KEYS[10], ARGV[1])"));
        assert!(DELAY_ACTIVE_JOB_SCRIPT.contains("redis.call('SREM', KEYS[6], ARGV[1])"));
        assert!(RELEASE_ACTIVE_JOB_SCRIPT.contains("redis.call('SREM', KEYS[7], ARGV[1])"));
        assert!(RECOVER_STALLED_SCRIPT
            .contains("emit_parent_waiting_children_transition_event(KEYS[12], ARGV[12]"));
    }

    #[test]
    fn orphaned_job_cleanup_scans_hash_and_checks_redis_references() {
        assert!(REMOVE_ORPHANED_JOBS_SCRIPT.contains("redis.call('HSCAN', jobs_key"));
        assert!(REMOVE_ORPHANED_JOBS_SCRIPT.contains("redis.call('ZSCORE', KEYS[key_index]"));
        assert!(REMOVE_ORPHANED_JOBS_SCRIPT.contains("redis.call('SISMEMBER', KEYS[key_index]"));
        assert!(REMOVE_ORPHANED_JOBS_SCRIPT.contains("redis.call('HDEL', jobs_key, job_id)"));
        assert!(REMOVE_ORPHANED_JOBS_SCRIPT.contains("dependencies_prefix .. job_id"));
        assert!(REMOVE_ORPHANED_JOBS_SCRIPT
            .contains("dependencies_prefix .. job_id .. ':unsuccessful'"));
    }

    #[test]
    fn move_to_finished_scripts_track_unsuccessful_flow_dependencies() {
        assert!(COMPLETE_SCRIPT.contains("ARGV[13] .. ARGV[1] .. ':unsuccessful'"));
        assert!(COMPLETE_SCRIPT
            .contains("redis.call('HSET', dependency_key .. ':processed', ARGV[1], ARGV[4])"));
        assert!(ADD_FLOW_SCRIPT.contains(
            "record_processed_child_dependency(dependency_key, child_id, existing_child)"
        ));
        assert!(ADD_FLOW_CHILDREN_SCRIPT
            .contains("record_processed_child_dependency(dependency_key, id, existing_child)"));
        assert!(ADD_FLOW_CHILDREN_SCRIPT
            .contains("redis.call('ZCARD', dependency_key .. ':unsuccessful')"));
        assert!(ADD_FLOW_CHILDREN_SCRIPT
            .contains("for _, child_id in ipairs(parent[\"child_ids\"] or {}) do"));
        assert!(FAIL_SCRIPT
            .contains("redis.call('HSET', dependency_key .. ':failed', ARGV[1], ARGV[4])"));
        assert!(FAIL_SCRIPT
            .contains("redis.call('ZADD', dependency_key .. ':unsuccessful', ARGV[8], ARGV[1])"));
        assert!(FAIL_SCRIPT.contains("releases_dependency_after_parent_release"));
        assert!(RECOVER_STALLED_SCRIPT.contains(
            "redis.call('HSET', dependency_key .. ':failed', id, job[\"failed_reason\"])"
        ));
        assert!(RECOVER_STALLED_SCRIPT
            .contains("redis.call('ZADD', dependency_key .. ':unsuccessful', ARGV[1], id)"));
        assert!(RECOVER_STALLED_SCRIPT.contains("releases_dependency_after_parent_release"));
        assert!(RETRY_JOB_SCRIPT
            .contains("redis.call('HDEL', ARGV[10] .. parent_id .. ':processed', ARGV[1])"));
        assert!(RETRY_JOB_SCRIPT
            .contains("redis.call('ZREM', ARGV[10] .. parent_id .. ':unsuccessful', ARGV[1])"));
        assert!(RETRY_JOB_SCRIPT
            .contains("redis.call('HDEL', ARGV[10] .. parent_id .. ':failed', ARGV[1])"));
        assert!(RETRY_JOB_SCRIPT.contains("local completed_indexed = redis.call('ZSCORE'"));
        assert!(RETRY_JOB_SCRIPT.contains("local failed_indexed = redis.call('ZSCORE'"));
        assert!(FLOW_CHILDREN_VALUES_SCRIPT.contains("redis.call('HGETALL', KEYS[2])"));
        assert!(FLOW_CHILDREN_VALUES_SCRIPT
            .contains("child[\"state\"] == \"completed\" and child[\"return_value\"] ~= nil"));
        assert!(FLOW_IGNORED_CHILDREN_FAILURES_SCRIPT.contains("redis.call('HGETALL', KEYS[2])"));
    }

    #[test]
    fn decode_flow_children_values_result_accepts_json_null() {
        let values = decode_flow_children_values_result(
            &["ok".to_string(), "child-a".to_string(), "null".to_string()],
            "parent",
        )
        .expect("child values should decode")
        .expect("child values should exist");

        assert_eq!(values.get("child-a"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn claim_script_suppresses_base_marker_when_paused_or_maxed() {
        assert!(CLAIM_SCRIPT.contains("local function queue_is_paused_or_maxed"));
        assert!(CLAIM_SCRIPT.contains("local promotion_marker_key = KEYS[#KEYS]"));
        assert!(CLAIM_SCRIPT.contains("if is_paused_or_maxed then\n  promotion_marker_key = ''"));
        assert!(CLAIM_SCRIPT.contains(
            "enqueue_waiting_job(KEYS[3], KEYS[1], KEYS[7], delayed_job, due_id, priority, ARGV[11], promotion_marker_key)"
        ));
    }

    #[test]
    fn metrics_scripts_follow_bullmq_minute_window_shape() {
        assert!(GET_METRICS_SCRIPT.contains("HMGET', KEYS[1], 'count', 'prevTS', 'prevCount'"));
        assert!(GET_METRICS_SCRIPT.contains("LRANGE', KEYS[2]"));
        assert!(GET_METRICS_SCRIPT.contains("LLEN', KEYS[2]"));
        for script in [COMPLETE_SCRIPT, FAIL_SCRIPT, RECOVER_STALLED_SCRIPT] {
            assert!(script.contains("local function collect_metrics"));
            assert!(script.contains("HINCRBY', meta_key, 'count', 1"));
            assert!(script.contains("math.floor(tonumber(timestamp) / 60000)"));
            assert!(script.contains("LPUSH', data_key"));
            assert!(script.contains("LTRIM', data_key"));
        }
    }

    #[test]
    fn decode_job_accepts_lua_empty_arrays_as_empty_sequences() {
        let job = Job::new(
            "jobs".to_string(),
            "high".to_string(),
            serde_json::json!({ "n": 1 }),
            JobOptions::new(),
            Utc.timestamp_millis_opt(10_000).unwrap(),
        );
        let raw = encode_job(&job)
            .unwrap()
            .replace("\"logs\":[]", "\"logs\":{}")
            .replace("\"child_ids\":[]", "\"child_ids\":{}");
        let mut value: serde_json::Value =
            serde_json::from_str(&raw).expect("encoded job should be valid JSON");
        value["stacktrace"] = serde_json::json!({});
        let raw = serde_json::to_string(&value).expect("job JSON should re-encode");

        let decoded = decode_job(&raw).expect("Lua-shaped JSON should decode");

        assert!(decoded.logs.is_empty());
        assert!(decoded.child_ids.is_empty());
        assert!(decoded.stacktrace.is_empty());
        assert_eq!(decoded.payload, serde_json::json!({ "n": 1 }));
    }

    #[test]
    fn decode_metrics_result_builds_job_metrics() {
        let raw = redis::Value::Array(vec![
            redis::Value::Array(vec![
                redis::Value::BulkString(b"3".to_vec()),
                redis::Value::BulkString(b"61000".to_vec()),
                redis::Value::BulkString(b"2".to_vec()),
            ]),
            redis::Value::Array(vec![redis::Value::BulkString(b"2".to_vec())]),
            redis::Value::Int(1),
        ]);

        let metrics =
            decode_metrics_result(&raw, JobState::Completed).expect("metrics result should decode");

        assert_eq!(metrics.meta.count, 3);
        assert_eq!(metrics.meta.previous_timestamp_millis, 61_000);
        assert_eq!(metrics.meta.previous_count, 2);
        assert_eq!(metrics.data, vec![2]);
        assert_eq!(metrics.count, 1);
    }

    #[test]
    fn decode_metrics_result_rejects_non_terminal_state() {
        let raw = redis::Value::Array(vec![
            redis::Value::Array(vec![
                redis::Value::Nil,
                redis::Value::Nil,
                redis::Value::Nil,
            ]),
            redis::Value::Array(Vec::new()),
            redis::Value::Int(0),
        ]);

        let error = decode_metrics_result(&raw, JobState::Waiting)
            .expect_err("waiting metrics should be rejected");
        assert!(matches!(error, LaneError::ConfigError(_)));
    }

    #[test]
    fn decode_list_jobs_result_builds_page() {
        let job = Job::new(
            "jobs".to_string(),
            "list".to_string(),
            serde_json::json!({ "n": 1 }),
            JobOptions::new(),
            Utc.timestamp_millis_opt(10_000).unwrap(),
        );
        let raw = encode_job(&job).expect("job should encode");

        let page = decode_list_jobs_result(
            &[
                redis::Value::Int(3),
                redis::Value::BulkString(raw.into_bytes()),
            ],
            2,
            10,
        )
        .expect("valid Redis list jobs payload should decode");

        assert_eq!(page.total, 3);
        assert_eq!(page.offset, 2);
        assert_eq!(page.limit, 10);
        assert_eq!(page.jobs, vec![job]);
    }

    #[test]
    fn decode_list_jobs_result_rejects_bad_script_payloads() {
        let empty = decode_list_jobs_result(&[], 0, 10)
            .expect_err("empty Redis list jobs payload should be rejected");
        assert!(matches!(
            empty,
            LaneError::Other(message)
                if message == "Redis list jobs script returned no values"
        ));

        let bad_state =
            decode_list_jobs_result(&[redis::Value::BulkString(b"bad_state".to_vec())], 0, 10)
                .expect_err("bad state payload should be rejected");
        assert!(matches!(
            bad_state,
            LaneError::Other(message)
                if message == "Redis list jobs script received an unknown state"
        ));
    }

    #[test]
    fn decode_stats_result_builds_queue_stats() {
        let stats = decode_stats_result(&[2, 3, 5, 7, 11, 13, 1])
            .expect("valid Redis stats payload should decode");

        assert_eq!(
            stats,
            JobQueueStats {
                total: 41,
                waiting: 2,
                delayed: 3,
                active: 5,
                waiting_children: 7,
                completed: 11,
                failed: 13,
                paused: true,
            }
        );
    }

    #[test]
    fn decode_stats_result_rejects_unexpected_shape() {
        let error =
            decode_stats_result(&[1, 2]).expect_err("short Redis stats payload should be rejected");

        assert!(matches!(
            error,
            LaneError::Other(message)
                if message == "Redis stats script returned 2 values, expected 7"
        ));
    }

    #[test]
    fn decode_stats_result_rejects_negative_values() {
        let count_error = decode_stats_result(&[1, -1, 0, 0, 0, 0, 0])
            .expect_err("negative counts should be rejected");
        assert!(matches!(
            count_error,
            LaneError::Other(message)
                if message == "Redis stats script returned negative delayed count: -1"
        ));

        let flag_error = decode_stats_result(&[1, 0, 0, 0, 0, 0, -1])
            .expect_err("negative paused flag should be rejected");
        assert!(matches!(
            flag_error,
            LaneError::Other(message)
                if message == "Redis stats script returned negative paused flag: -1"
        ));
    }
}
