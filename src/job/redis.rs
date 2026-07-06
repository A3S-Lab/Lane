use super::backend::JobQueueBackend;
use super::types::{
    add_duration, Job, JobFlow, JobListOptions, JobListPage, JobOptions, JobPriority,
    JobQueueStats, JobRateLimit, JobSpec, JobState, JobWorkerId, QueueName,
};
use crate::error::{LaneError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use uuid::Uuid;

const WAITING_SCORE_BUCKET: f64 = 1_000_000_000_000.0;

const ADD_JOB_SCRIPT: &str = r#"
local existing = redis.call('HGET', KEYS[1], ARGV[1])
if existing then
  return {'existing', existing}
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
if state == 'waiting' then
  local sequence = redis.call('INCR', KEYS[5])
  local waiting_score = (tonumber(ARGV[5]) * tonumber(ARGV[6])) + sequence
  redis.call('ZADD', KEYS[2], waiting_score, ARGV[1])
elseif state == 'delayed' then
  redis.call('ZADD', KEYS[3], ARGV[4], ARGV[1])
elseif state == 'waiting_children' then
  redis.call('ZADD', KEYS[4], ARGV[4], ARGV[1])
end

return {'inserted', ARGV[2]}
"#;

const ADD_JOBS_SCRIPT: &str = r#"
local count = tonumber(ARGV[1])
local offset = 2
local added = {}

for index = 1, count do
  local id = ARGV[offset]
  local raw = ARGV[offset + 1]
  local state = ARGV[offset + 2]
  local scheduled_score = ARGV[offset + 3]
  local priority = tonumber(ARGV[offset + 4])
  local existing = redis.call('HGET', KEYS[1], id)

  if existing then
    added[index] = existing
  else
    local inserted = redis.call('HSETNX', KEYS[1], id, raw)
    if inserted == 0 then
      local current = redis.call('HGET', KEYS[1], id)
      if current then
        added[index] = current
      else
        return {'missing', id}
      end
    else
      if state == 'waiting' then
        local sequence = redis.call('INCR', KEYS[5])
        local waiting_score = (priority * tonumber(ARGV[2 + count * 5])) + sequence
        redis.call('ZADD', KEYS[2], waiting_score, id)
      elseif state == 'delayed' then
        redis.call('ZADD', KEYS[3], scheduled_score, id)
      elseif state == 'waiting_children' then
        redis.call('ZADD', KEYS[4], scheduled_score, id)
      end
      added[index] = raw
    end
  end

  offset = offset + 5
end

return added
"#;

const ADD_FLOW_SCRIPT: &str = r#"
local count = tonumber(ARGV[1])
local offset = 2
local waiting_score_bucket = tonumber(ARGV[2 + count * 5])
local dependency_prefix = ARGV[3 + count * 5]

for index = 1, count do
  local id = ARGV[offset]
  if redis.call('HGET', KEYS[1], id) then
    return {'exists', id}
  end
  offset = offset + 5
end

offset = 2
for index = 1, count do
  local id = ARGV[offset]
  local raw = ARGV[offset + 1]
  local state = ARGV[offset + 2]
  local scheduled_score = ARGV[offset + 3]
  local priority = tonumber(ARGV[offset + 4])

  redis.call('HSET', KEYS[1], id, raw)
  if state == 'waiting' then
    local sequence = redis.call('INCR', KEYS[5])
    local waiting_score = (priority * waiting_score_bucket) + sequence
    redis.call('ZADD', KEYS[2], waiting_score, id)
  elseif state == 'delayed' then
    redis.call('ZADD', KEYS[3], scheduled_score, id)
  elseif state == 'waiting_children' then
    redis.call('ZADD', KEYS[4], scheduled_score, id)
  end

  offset = offset + 5
end

if count > 1 and ARGV[4] == 'waiting_children' then
  local parent_id = ARGV[2]
  local dependency_key = dependency_prefix .. parent_id
  redis.call('DEL', dependency_key)

  local child_offset = 7
  for index = 2, count do
    redis.call('SADD', dependency_key, ARGV[child_offset])
    child_offset = child_offset + 5
  end
end

return {'ok'}
"#;

const CLAIM_SCRIPT: &str = r#"
local due_ids = redis.call('ZRANGEBYSCORE', KEYS[6], '-inf', ARGV[10], 'LIMIT', 0, ARGV[12])
for _, due_id in ipairs(due_ids) do
  local delayed_raw = redis.call('HGET', KEYS[3], due_id)
  redis.call('ZREM', KEYS[6], due_id)
  if delayed_raw then
    local delayed_job = cjson.decode(delayed_raw)
    if delayed_job["state"] == "delayed" then
      delayed_job["state"] = "waiting"
      local priority = tonumber(delayed_job["priority"] or '1000') or 1000
      local sequence = redis.call('INCR', KEYS[7])
      local waiting_score = (priority * tonumber(ARGV[11])) + sequence
      redis.call('ZADD', KEYS[1], waiting_score, due_id)
      redis.call('HSET', KEYS[3], due_id, cjson.encode(delayed_job))
    end
  end
end

local paused = redis.call('HGET', KEYS[5], 'paused')
if paused and paused ~= '0' then
  return nil
end

local rate_limit_max = tonumber(ARGV[8])
if rate_limit_max and rate_limit_max > 0 then
  local current_claims = tonumber(redis.call('GET', KEYS[4]) or '0')
  if current_claims >= rate_limit_max then
    return nil
  end
end

local max_concurrency = tonumber(redis.call('HGET', KEYS[5], 'concurrency') or '0')
if max_concurrency and max_concurrency > 0 then
  local active_count = redis.call('ZCARD', KEYS[2])
  if active_count >= max_concurrency then
    return nil
  end
end

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

local lock_key = ARGV[7] .. id
redis.call('SET', lock_key, ARGV[5], 'PX', ARGV[6])
if rate_limit_max and rate_limit_max > 0 then
  local counter = redis.call('INCR', KEYS[4])
  if counter == 1 then
    redis.call('PEXPIRE', KEYS[4], ARGV[9])
  end
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

const COMPLETE_SCRIPT: &str = r#"
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

redis.call('DEL', KEYS[4])
redis.call('ZREM', KEYS[2], ARGV[1])

job["state"] = "completed"
job["finished_at"] = ARGV[3]
job["worker_id"] = cjson.null
job["lease_expires_at"] = cjson.null
job["return_value"] = cjson.decode(ARGV[4])

local updated = cjson.encode(job)
if ARGV[6] == '1' then
  redis.call('HDEL', KEYS[1], ARGV[1])
else
  redis.call('HSET', KEYS[1], ARGV[1], updated)
  redis.call('ZADD', KEYS[3], ARGV[5], ARGV[1])
end

local parent_id = job["parent_id"]
if parent_id and parent_id ~= cjson.null then
  local parent_raw = redis.call('HGET', KEYS[1], parent_id)
  if parent_raw then
    local parent = cjson.decode(parent_raw)
    if parent["state"] == "waiting_children" then
      local all_done = true
      local failed_child_id = nil
      local failed_reason = nil
      local dependency_key = ARGV[13] .. parent_id
      local had_dependency_set = redis.call('EXISTS', dependency_key) == 1

      if had_dependency_set then
        redis.call('SREM', dependency_key, ARGV[1])
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
            if child["state"] == "failed" then
              failed_child_id = child_id
              failed_reason = child["failed_reason"] or "unknown error"
              break
            elseif child["state"] ~= "completed" then
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
        parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
        if parent["options"] and parent["options"]["remove_on_fail"] == true then
          redis.call('HDEL', KEYS[1], parent_id)
        else
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[8], ARGV[5], parent_id)
        end
      elseif all_done then
        redis.call('DEL', dependency_key)
        redis.call('ZREM', KEYS[6], parent_id)
        parent["processed_at"] = cjson.null
        parent["finished_at"] = cjson.null
        parent["worker_id"] = cjson.null
        parent["lock_token"] = cjson.null
        parent["lease_expires_at"] = cjson.null
        parent["failed_reason"] = cjson.null

        local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
        if parent_scheduled_millis <= tonumber(ARGV[5]) then
          parent["state"] = "waiting"
          local priority = tonumber(parent["priority"] or '1000') or 1000
          local sequence = redis.call('INCR', KEYS[7])
          local waiting_score = (priority * tonumber(ARGV[7])) + sequence
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[5], waiting_score, parent_id)
        else
          parent["state"] = "delayed"
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[9], parent_scheduled_millis, parent_id)
        end
      end
    end
  end
end

local repeat_next_id = ARGV[8]
if repeat_next_id and repeat_next_id ~= '' then
  local inserted = redis.call('HSETNX', KEYS[1], repeat_next_id, ARGV[9])
  if inserted == 1 then
    local repeat_next_state = ARGV[10]
    if repeat_next_state == 'waiting' then
      local repeat_priority = tonumber(ARGV[12])
      local sequence = redis.call('INCR', KEYS[7])
      local waiting_score = (repeat_priority * tonumber(ARGV[7])) + sequence
      redis.call('ZADD', KEYS[5], waiting_score, repeat_next_id)
    elseif repeat_next_state == 'delayed' then
      redis.call('ZADD', KEYS[9], ARGV[11], repeat_next_id)
    elseif repeat_next_state == 'waiting_children' then
      redis.call('ZADD', KEYS[6], ARGV[11], repeat_next_id)
    end
  end
end

return {'ok', updated}
"#;

const FAIL_SCRIPT: &str = r#"
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
redis.call('ZREM', KEYS[2], ARGV[1])

job["worker_id"] = cjson.null
job["lease_expires_at"] = cjson.null
job["failed_reason"] = ARGV[4]

if ARGV[5] == '1' then
  job["state"] = "delayed"
  job["scheduled_at"] = ARGV[6]
  job["finished_at"] = cjson.null
  local updated = cjson.encode(job)
  redis.call('HSET', KEYS[1], ARGV[1], updated)
  redis.call('ZADD', KEYS[3], ARGV[7], ARGV[1])
  return {'ok', updated}
end

job["state"] = "failed"
job["finished_at"] = ARGV[3]
local updated = cjson.encode(job)
if ARGV[9] == '1' then
  redis.call('HDEL', KEYS[1], ARGV[1])
else
  redis.call('HSET', KEYS[1], ARGV[1], updated)
  redis.call('ZADD', KEYS[4], ARGV[8], ARGV[1])
end

local parent_id = job["parent_id"]
if parent_id and parent_id ~= cjson.null then
  local parent_raw = redis.call('HGET', KEYS[1], parent_id)
  if parent_raw then
    local parent = cjson.decode(parent_raw)
    if parent["state"] == "waiting_children" then
      redis.call('DEL', ARGV[10] .. parent_id)
      redis.call('ZREM', KEYS[6], parent_id)
      parent["state"] = "failed"
      parent["finished_at"] = ARGV[3]
      parent["worker_id"] = cjson.null
      parent["lock_token"] = cjson.null
      parent["lease_expires_at"] = cjson.null
      parent["failed_reason"] = "child job " .. ARGV[1] .. " failed: " .. ARGV[4]
      if parent["options"] and parent["options"]["remove_on_fail"] == true then
        redis.call('HDEL', KEYS[1], parent_id)
      else
        redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
        redis.call('ZADD', KEYS[4], ARGV[8], parent_id)
      end
    end
  end
end

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
job["lease_expires_at"] = ARGV[3]
local updated = cjson.encode(job)
redis.call('HSET', KEYS[1], ARGV[1], updated)
redis.call('ZADD', KEYS[2], ARGV[4], ARGV[1])
return {'ok', updated}
"#;

const RECOVER_STALLED_SCRIPT: &str = r#"
local ids = redis.call('ZRANGEBYSCORE', KEYS[2], '-inf', ARGV[1], 'LIMIT', 0, ARGV[5])
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
        job["failed_reason"] = "job stalled after worker lease expired"
        redis.call('ZREM', KEYS[2], id)

        local max_stalled = 1
        if job["options"] and job["options"]["max_stalled_count"] ~= nil then
          max_stalled = tonumber(job["options"]["max_stalled_count"])
        end

        if job["stalled_count"] > max_stalled then
          job["state"] = "failed"
          job["finished_at"] = ARGV[2]
          redis.call('DEL', ARGV[6] .. id)

          if job["options"] and job["options"]["remove_on_fail"] == true then
            redis.call('HDEL', KEYS[1], id)
          else
            redis.call('HSET', KEYS[1], id, cjson.encode(job))
            redis.call('ZADD', KEYS[4], ARGV[1], id)
          end

          local parent_id = job["parent_id"]
          if parent_id and parent_id ~= cjson.null then
            local parent_raw = redis.call('HGET', KEYS[1], parent_id)
            if parent_raw then
              local parent = cjson.decode(parent_raw)
              if parent["state"] == "waiting_children" then
                redis.call('DEL', ARGV[6] .. parent_id)
                redis.call('ZREM', KEYS[6], parent_id)
                parent["state"] = "failed"
                parent["finished_at"] = ARGV[2]
                parent["worker_id"] = cjson.null
                parent["lock_token"] = cjson.null
                parent["lease_expires_at"] = cjson.null
                parent["failed_reason"] = "child job " .. id .. " failed: " .. job["failed_reason"]
                if parent["options"] and parent["options"]["remove_on_fail"] == true then
                  redis.call('HDEL', KEYS[1], parent_id)
                else
                  redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
                  redis.call('ZADD', KEYS[4], ARGV[1], parent_id)
                end
              end
            end
          end
        else
          job["state"] = "waiting"
          job["processed_at"] = cjson.null
          local priority = tonumber(job["priority"] or '1000') or 1000
          local sequence = redis.call('INCR', KEYS[5])
          local waiting_score = (priority * tonumber(ARGV[4])) + sequence
          redis.call('HSET', KEYS[1], id, cjson.encode(job))
          redis.call('ZADD', KEYS[3], waiting_score, id)
        end

        recovered = recovered + 1
      end
    end
  end
end

return recovered
"#;

const PROMOTE_JOB_SCRIPT: &str = r#"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] ~= "delayed" then
  return {'ok', raw}
end

job["state"] = "waiting"
job["scheduled_at"] = ARGV[2]
local priority = tonumber(job["priority"] or '1000') or 1000
local sequence = redis.call('INCR', KEYS[4])
local waiting_score = (priority * tonumber(ARGV[3])) + sequence
local updated = cjson.encode(job)

redis.call('ZREM', KEYS[3], ARGV[1])
redis.call('ZADD', KEYS[2], waiting_score, ARGV[1])
redis.call('HSET', KEYS[1], ARGV[1], updated)

return {'ok', updated}
"#;

const RETRY_JOB_SCRIPT: &str = r#"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] ~= "failed" then
  return {'state', job["state"] or ''}
end

job["state"] = "waiting"
job["scheduled_at"] = ARGV[2]
job["processed_at"] = cjson.null
job["finished_at"] = cjson.null
job["worker_id"] = cjson.null
job["lock_token"] = cjson.null
job["lease_expires_at"] = cjson.null
job["failed_reason"] = cjson.null

local priority = tonumber(job["priority"] or '1000') or 1000
local sequence = redis.call('INCR', KEYS[4])
local waiting_score = (priority * tonumber(ARGV[3])) + sequence
local updated = cjson.encode(job)

redis.call('ZREM', KEYS[2], ARGV[1])
redis.call('ZADD', KEYS[3], waiting_score, ARGV[1])
redis.call('HSET', KEYS[1], ARGV[1], updated)

return {'ok', updated}
"#;

const UPDATE_PRIORITY_SCRIPT: &str = r#"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] == "completed" or job["state"] == "failed" then
  return {'terminal'}
end

local priority = tonumber(ARGV[2])
job["priority"] = priority
if not job["options"] or job["options"] == cjson.null then
  job["options"] = {}
end
job["options"]["priority"] = priority

if job["state"] == "waiting" then
  local sequence = redis.call('INCR', KEYS[3])
  local waiting_score = (priority * tonumber(ARGV[3])) + sequence
  redis.call('ZADD', KEYS[2], waiting_score, ARGV[1])
end

local updated = cjson.encode(job)
redis.call('HSET', KEYS[1], ARGV[1], updated)

return {'ok', updated}
"#;

const PROMOTE_DUE_JOBS_SCRIPT: &str = r#"
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
      local sequence = redis.call('INCR', KEYS[4])
      local waiting_score = (priority * tonumber(ARGV[2])) + sequence
      redis.call('ZADD', KEYS[3], waiting_score, id)
      redis.call('HSET', KEYS[1], id, cjson.encode(job))
      promoted = promoted + 1
    end
  end
end

return promoted
"#;

const REMOVE_JOB_SCRIPT: &str = r#"
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

local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] == "active" then
  return {'active'}
end

redis.call('DEL', KEYS[2])
for index = 3, 8 do
  redis.call('ZREM', KEYS[index], ARGV[1])
end
redis.call('HDEL', KEYS[1], ARGV[1])
redis.call('DEL', ARGV[5] .. ARGV[1])

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
            if child["state"] == "failed" then
              failed_child_id = child_id
              failed_reason = child["failed_reason"] or "unknown error"
              break
            elseif child["state"] ~= "completed" then
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
        parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
        if parent["options"] and parent["options"]["remove_on_fail"] == true then
          redis.call('HDEL', KEYS[1], parent_id)
        else
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[8], ARGV[3], parent_id)
        end
      elseif all_done then
        redis.call('DEL', dependency_key)
        redis.call('ZREM', KEYS[6], parent_id)
        parent["processed_at"] = cjson.null
        parent["finished_at"] = cjson.null
        parent["worker_id"] = cjson.null
        parent["lock_token"] = cjson.null
        parent["lease_expires_at"] = cjson.null
        parent["failed_reason"] = cjson.null

        local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
        if parent_scheduled_millis <= tonumber(ARGV[3]) then
          parent["state"] = "waiting"
          local priority = tonumber(parent["priority"] or '1000') or 1000
          local sequence = redis.call('INCR', KEYS[9])
          local waiting_score = (priority * tonumber(ARGV[4])) + sequence
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[3], waiting_score, parent_id)
        else
          parent["state"] = "delayed"
          redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
          redis.call('ZADD', KEYS[4], parent_scheduled_millis, parent_id)
        end
      end
    end
  end
end

return {'ok', raw}
"#;

const CLEAN_JOBS_SCRIPT: &str = r#"
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
        if child["state"] == "failed" then
          failed_child_id = child_id
          failed_reason = child["failed_reason"]
          if not failed_reason or failed_reason == cjson.null then
            failed_reason = "unknown error"
          end
          break
        elseif child["state"] ~= "completed" then
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
    parent["failed_reason"] = "child job " .. failed_child_id .. " failed: " .. failed_reason
    if parent["options"] and parent["options"]["remove_on_fail"] == true then
      redis.call('HDEL', KEYS[1], parent_id)
    else
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[7], ARGV[6], parent_id)
    end
  elseif all_done then
    redis.call('DEL', dependency_key)
    redis.call('ZREM', KEYS[5], parent_id)
    parent["processed_at"] = cjson.null
    parent["finished_at"] = cjson.null
    parent["worker_id"] = cjson.null
    parent["lock_token"] = cjson.null
    parent["lease_expires_at"] = cjson.null
    parent["failed_reason"] = cjson.null

    local parent_scheduled_millis = iso_to_millis(parent["scheduled_at"])
    if parent_scheduled_millis <= tonumber(ARGV[6]) then
      parent["state"] = "waiting"
      local priority = tonumber(parent["priority"] or '1000') or 1000
      local sequence = redis.call('INCR', KEYS[8])
      local waiting_score = (priority * tonumber(ARGV[7])) + sequence
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[2], waiting_score, parent_id)
    else
      parent["state"] = "delayed"
      redis.call('HSET', KEYS[1], parent_id, cjson.encode(parent))
      redis.call('ZADD', KEYS[3], parent_scheduled_millis, parent_id)
    end
  end
end

local state = ARGV[1]
local cutoff = ARGV[2]
local limit = tonumber(ARGV[3])
local lock_prefix = ARGV[4]
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

if state == 'active' or limit <= 0 then
  return {}
end

local ids = redis.call('ZRANGE', state_key, 0, -1)
local candidates = {}

for _, id in ipairs(ids) do
  local raw = redis.call('HGET', KEYS[1], id)
  if raw then
    local job = cjson.decode(raw)
    if job["state"] == state then
      local reference = job["finished_at"]
      if not reference or reference == cjson.null then
        reference = job["processed_at"]
      end
      if not reference or reference == cjson.null then
        reference = job["scheduled_at"]
      end

      if reference and reference ~= cjson.null and reference <= cutoff then
        table.insert(candidates, { id = id, reference = reference, raw = raw, job = job })
      end
    else
      redis.call('ZREM', state_key, id)
    end
  else
    redis.call('ZREM', state_key, id)
  end
end

table.sort(candidates, function(left, right)
  if left.reference == right.reference then
    return left.id < right.id
  end
  return left.reference < right.reference
end)

local removed = {}
local count = math.min(limit, #candidates)
for index = 1, count do
  local candidate = candidates[index]
  redis.call('DEL', lock_prefix .. candidate.id)
  for key_index = 2, 7 do
    redis.call('ZREM', KEYS[key_index], candidate.id)
  end
  redis.call('DEL', ARGV[8] .. candidate.id)
  redis.call('HDEL', KEYS[1], candidate.id)
  release_parent_after_removed_child(candidate.job, candidate.id)
  removed[index] = candidate.raw
end

return removed
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
local state = ARGV[3]
local result = {}

if state ~= '' then
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

  local ids = redis.call('ZRANGE', state_key, 0, -1)
  local raws = {}
  for _, id in ipairs(ids) do
    local raw = redis.call('HGET', KEYS[1], id)
    if raw then
      local job = cjson.decode(raw)
      if job["state"] == state then
        table.insert(raws, raw)
      else
        redis.call('ZREM', state_key, id)
      end
    else
      redis.call('ZREM', state_key, id)
    end
  end

  result[1] = #raws
  if limit > 0 then
    local first = offset + 1
    local last = math.min(offset + limit, #raws)
    local out = 2
    for index = first, last do
      result[out] = raws[index]
      out = out + 1
    end
  end
  return result
end

local raw_jobs = redis.call('HVALS', KEYS[1])
local jobs = {}
for _, raw in ipairs(raw_jobs) do
  local job = cjson.decode(raw)
  table.insert(jobs, {
    raw = raw,
    state_rank = state_rank(job["state"]),
    priority = tonumber(job["priority"] or '1000') or 1000,
    scheduled_at = iso_sort_key(job["scheduled_at"]),
    created_at = iso_sort_key(job["created_at"]),
    id = job["id"] or ''
  })
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
  local first = offset + 1
  local last = math.min(offset + limit, #jobs)
  local out = 2
  for index = first, last do
    result[out] = jobs[index].raw
    out = out + 1
  end
end

return result
"#;

const UPDATE_PROGRESS_SCRIPT: &str = r#"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if not raw then
  return {'missing'}
end

local job = cjson.decode(raw)
if job["state"] == "completed" or job["state"] == "failed" then
  return {'state', job["state"] or ''}
end

job["progress"] = cjson.decode(ARGV[2])
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

return {'ok', updated}
"#;

const STATS_SCRIPT: &str = r#"
local paused = redis.call('HGET', KEYS[1], 'paused')
local paused_value = 0
if paused and paused ~= '0' then
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

    /// Configure a Redis-backed queue-level claim rate limit.
    ///
    /// The limit is shared by every worker using the same namespace and queue.
    pub fn with_claim_rate_limit(mut self, rate_limit: JobRateLimit) -> Result<Self> {
        rate_limit.validate()?;
        self.claim_rate_limit = Some(rate_limit);
        Ok(self)
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

    /// Clear the Redis-backed active job limit.
    pub async fn clear_max_active_jobs(&self) -> Result<()> {
        let mut conn = self.connection().await?;
        conn.hdel(self.meta_key(), "concurrency")
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

    /// Add multiple jobs at an explicit timestamp.
    pub async fn add_many_at(&self, jobs: Vec<JobSpec>, now: DateTime<Utc>) -> Result<Vec<Job>> {
        for spec in &jobs {
            validate_job_options(&spec.options)?;
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
            .arg(5)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.sequence_key())
            .arg(created.len());
        for job in &created {
            command
                .arg(&job.id)
                .arg(encode_job(job)?)
                .arg(job_state_name(job.state))
                .arg(millis(job.scheduled_at))
                .arg(job.priority);
        }
        command.arg(WAITING_SCORE_BUCKET);

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
        validate_job_options(&parent.options)?;
        for child in &children {
            validate_job_options(&child.options)?;
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
        self.add_flow_jobs(&mut conn, &parent_job, &child_jobs)
            .await?;

        Ok(JobFlow {
            parent: parent_job,
            children: child_jobs,
        })
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

    fn claim_rate_limit_key(&self) -> String {
        self.key("claim_rate_limit")
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
            .arg(5)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.sequence_key())
            .arg(&job.id)
            .arg(encoded)
            .arg(job_state_name(job.state))
            .arg(millis(job.scheduled_at))
            .arg(job.priority)
            .arg(WAITING_SCORE_BUCKET)
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
    ) -> Result<()> {
        let job_count = 1 + children.len();
        let mut command = redis::cmd("EVAL");
        command
            .arg(ADD_FLOW_SCRIPT)
            .arg(5)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.sequence_key())
            .arg(job_count);

        for job in std::iter::once(parent).chain(children.iter()) {
            command
                .arg(&job.id)
                .arg(encode_job(job)?)
                .arg(job_state_name(job.state))
                .arg(millis(job.scheduled_at))
                .arg(job.priority);
        }
        command
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.dependencies_key_prefix());

        let result: Vec<String> = command.query_async(conn).await.map_err(redis_error)?;
        decode_add_flow_result(&result)
    }

    async fn load_job(&self, conn: &mut ConnectionManager, job_id: &str) -> Result<Option<Job>> {
        let raw: Option<String> = conn
            .hget(self.jobs_key(), job_id)
            .await
            .map_err(redis_error)?;
        raw.map(|raw| decode_job(&raw)).transpose()
    }
}

#[async_trait]
impl JobQueueBackend for RedisJobQueue {
    async fn add_job(&self, name: String, payload: Value, options: JobOptions) -> Result<Job> {
        validate_job_options(&options)?;
        let now = Utc::now();
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

    async fn claim_next(
        &self,
        worker_id: JobWorkerId,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<Job>> {
        let mut conn = self.connection().await?;

        let lease_expires_at = add_duration(now, lease_for);
        let lock_token = Uuid::new_v4().to_string();
        let (rate_limit_max, rate_limit_window_ms) = self
            .claim_rate_limit
            .as_ref()
            .map(|limit| (limit.max_claims, duration_millis(limit.window).max(1)))
            .unwrap_or((0, 0));
        let raw: Option<String> = redis::cmd("EVAL")
            .arg(CLAIM_SCRIPT)
            .arg(7)
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Active))
            .arg(self.jobs_key())
            .arg(self.claim_rate_limit_key())
            .arg(self.meta_key())
            .arg(self.state_key(JobState::Delayed))
            .arg(self.sequence_key())
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
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;

        let Some(raw) = raw else {
            return Ok(None);
        };
        let mut job = decode_job(&raw)?;
        job.lock_token = Some(lock_token);
        Ok(Some(job))
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
            .arg(9)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::Completed))
            .arg(self.lock_key(job_id))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.sequence_key())
            .arg(self.state_key(JobState::Failed))
            .arg(self.state_key(JobState::Delayed))
            .arg(job_id)
            .arg(lock_token)
            .arg(now.to_rfc3339())
            .arg(serde_json::to_string(&value).map_err(|error| {
                LaneError::Other(format!(
                    "failed to encode Redis job completion value: {error}"
                ))
            })?)
            .arg(millis(now))
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
        let mut conn = self.connection().await?;
        let job = self.require_job(&mut conn, job_id).await?;
        let retry_at = if should_retry(&job) {
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
            .arg(6)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.lock_key(job_id))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(job_id)
            .arg(lock_token)
            .arg(now.to_rfc3339())
            .arg(error)
            .arg(if retry_at.is_some() { "1" } else { "0" })
            .arg(scheduled_at.to_rfc3339())
            .arg(millis(scheduled_at))
            .arg(millis(now))
            .arg(if job.options.remove_on_fail { "1" } else { "0" })
            .arg(self.dependencies_key_prefix())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "fail")
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
            .arg(3)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.lock_key(job_id))
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

    async fn promote_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(PROMOTE_JOB_SCRIPT)
            .arg(4)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.sequence_key())
            .arg(job_id)
            .arg(now.to_rfc3339())
            .arg(WAITING_SCORE_BUCKET)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "promote")
    }

    async fn retry_job(&self, job_id: &str, now: DateTime<Utc>) -> Result<Job> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(RETRY_JOB_SCRIPT)
            .arg(4)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Failed))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.sequence_key())
            .arg(job_id)
            .arg(now.to_rfc3339())
            .arg(WAITING_SCORE_BUCKET)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "retry")
    }

    async fn update_priority(&self, job_id: &str, priority: JobPriority) -> Result<Job> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(UPDATE_PRIORITY_SCRIPT)
            .arg(3)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.sequence_key())
            .arg(job_id)
            .arg(priority)
            .arg(WAITING_SCORE_BUCKET)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_priority_update_result(&result, job_id)
    }

    async fn remove_job(&self, job_id: &str) -> Result<Option<Job>> {
        let mut conn = self.connection().await?;
        let now = Utc::now();
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(REMOVE_JOB_SCRIPT)
            .arg(9)
            .arg(self.jobs_key())
            .arg(self.lock_key(job_id))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.sequence_key())
            .arg(job_id)
            .arg(now.to_rfc3339())
            .arg(millis(now))
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.dependencies_key_prefix())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_remove_job_result(&result, job_id)
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
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(CLEAN_JOBS_SCRIPT)
            .arg(8)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Delayed))
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(self.state_key(JobState::Completed))
            .arg(self.state_key(JobState::Failed))
            .arg(self.sequence_key())
            .arg(job_state_name(state))
            .arg(cutoff.to_rfc3339())
            .arg(limit)
            .arg(self.lock_key_prefix())
            .arg(now.to_rfc3339())
            .arg(millis(now))
            .arg(WAITING_SCORE_BUCKET)
            .arg(self.dependencies_key_prefix())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_clean_jobs_result(&result)
    }

    async fn list_jobs(&self, options: JobListOptions) -> Result<JobListPage> {
        let mut conn = self.connection().await?;
        let result: Vec<redis::Value> = redis::cmd("EVAL")
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
            .arg(options.state.map(job_state_name).unwrap_or(""))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_list_jobs_result(&result, options.offset, options.limit)
    }

    async fn update_progress(&self, job_id: &str, progress: Value) -> Result<Job> {
        let mut conn = self.connection().await?;
        let progress = serde_json::to_string(&progress)
            .map_err(|error| LaneError::Other(format!("failed to encode job progress: {error}")))?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(UPDATE_PROGRESS_SCRIPT)
            .arg(1)
            .arg(self.jobs_key())
            .arg(job_id)
            .arg(progress)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "update progress")
    }

    async fn add_log(
        &self,
        job_id: &str,
        line: String,
        keep: usize,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut conn = self.connection().await?;
        let result: Vec<String> = redis::cmd("EVAL")
            .arg(ADD_LOG_SCRIPT)
            .arg(1)
            .arg(self.jobs_key())
            .arg(job_id)
            .arg(now.to_rfc3339())
            .arg(line)
            .arg(keep)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        decode_transition_result(&result, job_id, "add log")
    }

    async fn promote_due_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut conn = self.connection().await?;
        let mut total = 0;
        loop {
            let promoted: usize = redis::cmd("EVAL")
                .arg(PROMOTE_DUE_JOBS_SCRIPT)
                .arg(4)
                .arg(self.jobs_key())
                .arg(self.state_key(JobState::Delayed))
                .arg(self.state_key(JobState::Waiting))
                .arg(self.sequence_key())
                .arg(millis(now))
                .arg(WAITING_SCORE_BUCKET)
                .arg(1_000_u16)
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
            .arg(6)
            .arg(self.jobs_key())
            .arg(self.state_key(JobState::Active))
            .arg(self.state_key(JobState::Waiting))
            .arg(self.state_key(JobState::Failed))
            .arg(self.sequence_key())
            .arg(self.state_key(JobState::WaitingChildren))
            .arg(millis(now))
            .arg(now.to_rfc3339())
            .arg(self.lock_key_prefix())
            .arg(WAITING_SCORE_BUCKET)
            .arg(1_000_u16)
            .arg(self.dependencies_key_prefix())
            .query_async(&mut conn)
            .await
            .map_err(redis_error)
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

fn decode_job(raw: &str) -> Result<Job> {
    let mut value: Value = serde_json::from_str(raw)
        .map_err(|error| LaneError::Other(format!("failed to decode Redis job: {error}")))?;
    normalize_lua_empty_array(&mut value, "logs");
    normalize_lua_empty_array(&mut value, "child_ids");
    serde_json::from_value(value)
        .map_err(|error| LaneError::Other(format!("failed to decode Redis job: {error}")))
}

fn decode_add_job_result(result: &[String], job_id: &str) -> Result<Job> {
    match result.first().map(String::as_str) {
        Some("inserted" | "existing") => {
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

fn decode_add_flow_result(result: &[String]) -> Result<()> {
    match result.first().map(String::as_str) {
        Some("ok") => Ok(()),
        Some("exists") => {
            let id = result.get(1).map(String::as_str).unwrap_or("unknown");
            Err(LaneError::ConfigError(format!(
                "flow job id `{id}` already exists"
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
        Some("terminal") => Err(LaneError::JobStateConflict(format!(
            "cannot update priority for terminal job {job_id}"
        ))),
        Some(other) => Err(LaneError::Other(format!(
            "unexpected Redis priority update script status `{other}` for {job_id}"
        ))),
        None => Err(LaneError::Other(format!(
            "Redis priority update script returned no status for {job_id}"
        ))),
    }
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
        Some("lock_missing") => Err(LaneError::JobLeaseConflict(format!(
            "missing lock for job {job_id}"
        ))),
        Some("lock_mismatch") => Err(LaneError::JobLeaseConflict(format!(
            "lock token does not own job {job_id}"
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

fn millis(at: DateTime<Utc>) -> f64 {
    at.timestamp_millis() as f64
}

fn subtract_duration(at: DateTime<Utc>, duration: Duration) -> DateTime<Utc> {
    match chrono::Duration::from_std(duration) {
        Ok(delta) => at.checked_sub_signed(delta).unwrap_or(at),
        Err(_) => at,
    }
}

#[cfg(test)]
fn waiting_score(priority: JobPriority, sequence: u64) -> f64 {
    (priority as f64 * WAITING_SCORE_BUCKET) + sequence as f64
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

        let decoded = decode_job(&raw).expect("Lua-shaped JSON should decode");

        assert!(decoded.logs.is_empty());
        assert!(decoded.child_ids.is_empty());
        assert_eq!(decoded.payload, serde_json::json!({ "n": 1 }));
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
