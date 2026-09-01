//! 本地 WorkBuddy / CodeBuddy CLI JSONL Token 统计。
//!
//! 这个模块是统计数据的唯一归属：日志只在这里解码、去重和按时间聚合，
//! Tauri 与 HTTP 层只负责转发结果。响应只包含聚合数字和脱敏标识，不返回
//! 消息正文、arguments 或认证信息。

use chrono::{Datelike, Local, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Usage {
    input: u64,
    output: u64,
    read: u64,
    write: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Totals {
    usage: Usage,
    records: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionTotals {
    key: String,
    title: Option<String>,
    project: String,
    session_id: String,
    totals: Totals,
}

/// 单个 JSONL 文件独立聚合结果（供增量缓存按文件粒度复用）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FileStats {
    total: Totals,
    models: HashMap<String, Totals>,
    projects: HashMap<String, Totals>,
    daily: HashMap<String, Totals>,
    /// 模型 × 日期 的 Token 趋势（Token 趋势模型筛选）。
    /// `#[serde(default)]`：旧缓存（schema 1）无此字段时用默认空值。
    #[serde(default)]
    daily_by_model: HashMap<String, HashMap<String, Totals>>,
    hours: HashMap<String, Totals>,
    sessions: Vec<SessionTotals>,
    /// 该文件记录的时间覆盖范围（毫秒），合并时用于整体 coverage。
    first_ts: Option<i64>,
    last_ts: Option<i64>,
}

impl Totals {
    fn merge_into(&mut self, other: &Totals) {
        self.usage.input = self.usage.input.saturating_add(other.usage.input);
        self.usage.output = self.usage.output.saturating_add(other.usage.output);
        self.usage.read = self.usage.read.saturating_add(other.usage.read);
        self.usage.write = self.usage.write.saturating_add(other.usage.write);
        self.records = self.records.saturating_add(other.records);
    }
}

/// 增量缓存：文件路径 → 各时间桶的 (指纹, 聚合)。指纹用于判断文件是否变更。
///
/// 关键设计：`scan_file` 的结果受时间范围（cutoff）影响，因此缓存按时间桶
/// 分桶存储，避免 7 天/30 天/全量之间互相污染。每个桶独立增量更新。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FileCache {
    schema_version: u32,
    /// 文件相对根目录的路径（稳定 key，避免绝对路径漂移）
    /// → 时间桶 key（"all"/"7"/"30"/"90"）→ 指纹+聚合
    files: HashMap<String, HashMap<String, CachedFile>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedFile {
    /// 文件 mtime（毫秒）
    mtime_ms: i64,
    /// 文件大小（字节）——与 mtime 一起作为变更指纹
    size: u64,
    /// 该文件在该时间桶下的聚合结果
    stats: FileStats,
}

const CACHE_SCHEMA_VERSION: u32 = 2;

/// 时间桶 key：None=全量，Some(7/30/90)。
fn bucket_key(days: Option<i64>) -> String {
    match days {
        Some(7) => "7".to_string(),
        Some(30) => "30".to_string(),
        Some(90) => "90".to_string(),
        _ => "all".to_string(),
    }
}

impl Totals {
    fn add(&mut self, usage: Usage) {
        self.usage.input = self.usage.input.saturating_add(usage.input);
        self.usage.output = self.usage.output.saturating_add(usage.output);
        self.usage.read = self.usage.read.saturating_add(usage.read);
        self.usage.write = self.usage.write.saturating_add(usage.write);
        self.records = self.records.saturating_add(1);
    }

    fn value(&self) -> Value {
        let cache_hit_rate = (self.usage.input > 0)
            .then(|| self.usage.read as f64 / self.usage.input as f64);
        // `input` already includes cache reads; expose the same headline total
        // used by the dashboard without double-counting the cached portion.
        let total = self
            .usage
            .input
            .saturating_add(self.usage.output)
            .saturating_add(self.usage.write);
        json!({
            "total": total,
            "input": self.usage.input,
            "output": self.usage.output,
            "cacheRead": self.usage.read,
            "cacheWrite": self.usage.write,
            "uncachedInput": self.usage.input.saturating_sub(self.usage.read),
            "records": self.records,
            "cacheHitRate": cache_hit_rate,
        })
    }
}

/// Read a non-negative integer from a JSON number or string.
fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| value.as_f64().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u64))
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| object.get(*key).and_then(number))
}

fn positive_field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(number)
            .filter(|value| *value > 0)
    })
}

fn cached_input_field(object: &Map<String, Value>) -> u64 {
    // Providers have emitted both flat aliases and OpenAI-compatible nested
    // details. Prefer a positive flat alias so a stale `cache_read...: 0`
    // field cannot hide a populated `prompt_cache_hit_tokens` value.
    positive_field(
        object,
        &[
            "cache_read_input_tokens",
            "cacheReadInputTokens",
            "prompt_cache_hit_tokens",
            "cached_tokens",
        ],
    )
    .or_else(|| {
        object
            .get("prompt_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| positive_field(details, &["cached_tokens"]))
    })
    .or_else(|| {
        object
            .get("inputTokensDetails")
            .and_then(Value::as_array)
            .and_then(|details| {
                details.iter().find_map(|detail| {
                    detail
                        .as_object()
                        .and_then(|detail| positive_field(detail, &["cached_tokens"]))
                })
            })
    })
    .unwrap_or(0)
}

const CACHE_WRITE_KEYS: &[&str] = &[
    "cache_write_input_tokens",
    "cacheWriteInputTokens",
    "cache_creation_input_tokens",
    "prompt_cache_write_tokens",
];

fn usage_fields(object: &Map<String, Value>) -> Usage {
    Usage {
        input: field(object, &["input_tokens", "inputTokens", "prompt_tokens"]).unwrap_or(0),
        output: field(
            object,
            &["output_tokens", "outputTokens", "completion_tokens"],
        )
        .unwrap_or(0),
        read: cached_input_field(object),
        write: positive_field(object, CACHE_WRITE_KEYS).unwrap_or(0),
    }
}

fn usage_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value?.as_object().filter(|object| {
        // Input is the required anchor for a usage record. It may legitimately
        // be zero (for example a provider reports output-only retries), so do
        // not use `input > 0` as the validity check.
        field(object, &["input_tokens", "inputTokens", "prompt_tokens"]).is_some()
    })
}

/// Decode one record. Usage precedence is message.usage > providerData.usage >
/// top-level usage. Cache-write metadata may only exist on a non-selected
/// usage object or rawUsage, so those objects are consulted without counting
/// their input/output again.
fn usage(value: &Value) -> Option<Usage> {
    let provider = value.get("providerData");
    let candidates = [
        value.get("message").and_then(|message| message.get("usage")),
        provider.and_then(|data| data.get("usage")),
        value.get("usage"),
    ];
    let selected = candidates.iter().copied().find_map(usage_object)?;
    let mut result = usage_fields(selected);

    if result.write == 0 {
        result.write = candidates
            .iter()
            .copied()
            .filter_map(|candidate| candidate.and_then(Value::as_object))
            .chain(
                provider
                    .and_then(|data| data.get("rawUsage"))
                    .and_then(Value::as_object),
            )
            .find_map(|object| positive_field(object, CACHE_WRITE_KEYS))
            .unwrap_or(0);
    }

    // prompt_cache_miss_tokens is deliberately not a write alias: current
    // WorkBuddy/CodeBuddy logs use it for newly computed (uncached) input,
    // while their explicit cache-write fields may legitimately remain zero.

    Some(result)
}

fn timestamp(value: &Value) -> Option<i64> {
    value
        .get("timestamp")
        .or_else(|| value.get("ts"))
        .and_then(|timestamp| {
            timestamp
                .as_i64()
                .or_else(|| timestamp.as_u64().and_then(|n| i64::try_from(n).ok()))
                .or_else(|| timestamp.as_str()?.trim().parse::<i64>().ok())
        })
}

fn date(value: &Value) -> Option<String> {
    let timestamp = timestamp(value)?;
    chrono::DateTime::from_timestamp_millis(timestamp)
        .map(|date| date.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

fn hour(value: &Value) -> Option<String> {
    let timestamp = timestamp(value)?;
    chrono::DateTime::from_timestamp_millis(timestamp).map(|date| {
        let local = date.with_timezone(&Local);
        format!("{}-{}", local.weekday().num_days_from_monday(), local.hour())
    })
}

fn model(value: &Value) -> String {
    value
        .get("providerData")
        .and_then(|data| data.get("model"))
        .or_else(|| value.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("未知模型")
        .to_string()
}

fn files(root: &Path, output: &mut Vec<PathBuf>, cutoff: Option<i64>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Subagent logs duplicate parent-session context and are not part
            // of either product's primary usage accounting.
            if path.file_name().and_then(|name| name.to_str()) != Some("subagents") {
                files(&path, output, cutoff);
            }
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            // File-level prefilter: a jsonl file is append-only, so if its last
            // write (mtime) is before the cutoff, it cannot contain records
            // within the requested range and can be skipped entirely. This
            // avoids parsing gigabytes of stale history when a short window
            // (7/30/90 days) is requested.
            if let Some(minimum) = cutoff {
                let Ok(metadata) = path.metadata() else { continue };
                if let Ok(modified) = metadata.modified() {
                    if let Ok(modified_ms) = modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_millis() as i64)
                    {
                        if modified_ms < minimum {
                            continue;
                        }
                    }
                }
            }
            output.push(path);
        }
    }
}

fn project_name(root: &Path, file: &Path) -> String {
    let name = file
        .strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .filter(|name| !name.is_empty() && !name.ends_with(".jsonl"));
    match name {
        // Product directories commonly encode the complete absolute path.
        // Returning that would leak a user name and parent directories.
        Some(name) if !name.starts_with("Users-") && !name.starts_with("home-") => {
            name.to_string()
        }
        _ => "未知项目".to_string(),
    }
}

fn record_project(value: &Value, fallback: &str) -> String {
    value
        .get("cwd")
        .and_then(Value::as_str)
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 120)
        .unwrap_or(fallback)
        .to_string()
}

fn non_empty_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn groups(groups: HashMap<String, Totals>) -> Vec<Value> {
    let mut values: Vec<_> = groups
        .into_iter()
        .map(|(key, totals)| {
            let mut value = totals.value();
            value["key"] = json!(key);
            value
        })
        .collect();
    values.sort_by(|left, right| {
        total_value(right).cmp(&total_value(left))
    });
    values
}

fn session_groups(sessions: Vec<SessionTotals>) -> Vec<Value> {
    let mut values: Vec<_> = sessions
        .into_iter()
        .map(|session| {
            let mut value = session.totals.value();
            value["key"] = json!(session.key);
            value["title"] = json!(session.title);
            value["project"] = json!(session.project);
            value["sessionId"] = json!(session.session_id);
            value
        })
        .collect();
    values.sort_by(|left, right| {
        total_value(right).cmp(&total_value(left))
    });
    values
}

fn total_value(value: &Value) -> u64 {
    value
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            value
                .get("input")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(value.get("output").and_then(Value::as_u64).unwrap_or(0))
                .saturating_add(value.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0))
        })
}

/// 解析单个 JSONL 文件，返回该文件独立的聚合结果。
fn scan_file(path: &Path, fallback_project: &str, cutoff: Option<i64>) -> (FileStats, u64) {
    let mut stats = FileStats::default();
    let mut parse_errors = 0_u64;
    let session_id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("未知会话")
        .to_string();

    let Ok(file) = std::fs::File::open(path) else {
        return (stats, 1);
    };

    let mut session_totals = Totals::default();
    let mut session_project: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut summary: Option<String> = None;

    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            parse_errors = parse_errors.saturating_add(1);
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            parse_errors = parse_errors.saturating_add(1);
            continue;
        };
        // Title metadata belongs to the whole JSONL session file. Read it
        // before applying the usage cutoff so an older title can still
        // label usage that falls inside the selected range. aiTitle has
        // precedence over summary regardless of event order.
        if let Some(title) = non_empty_text(value.get("aiTitle")) {
            ai_title = Some(title);
        }
        if let Some(value) = non_empty_text(value.get("summary")) {
            summary = Some(value);
        }
        // Records with a missing timestamp are excluded from a bounded
        // range rather than guessed from file mtime or browser time.
        if cutoff.is_some_and(|minimum| timestamp(&value).is_none_or(|ts| ts < minimum)) {
            continue;
        }
        let Some(usage) = usage(&value) else {
            continue;
        };
        let project = record_project(&value, fallback_project);
        if session_project.is_none() {
            session_project = Some(project.clone());
        }
        session_totals.add(usage);
        stats.total.add(usage);
        stats
            .models
            .entry(model(&value))
            .or_insert_with(Totals::default)
            .add(usage);
        stats
            .projects
            .entry(project.clone())
            .or_insert_with(Totals::default)
            .add(usage);
        if let Some(day) = date(&value) {
            stats
                .daily
                .entry(day.clone())
                .or_insert_with(Totals::default)
                .add(usage);
            // 模型 × 日期 趋势（Token 趋势模型筛选）
            stats
                .daily_by_model
                .entry(model(&value))
                .or_default()
                .entry(day)
                .or_insert_with(Totals::default)
                .add(usage);
        }
        if let Some(hour) = hour(&value) {
            stats
                .hours
                .entry(hour)
                .or_insert_with(Totals::default)
                .add(usage);
        }
        if let Some(ts) = timestamp(&value) {
            stats.first_ts = Some(stats.first_ts.map_or(ts, |current| current.min(ts)));
            stats.last_ts = Some(stats.last_ts.map_or(ts, |current| current.max(ts)));
        }
    }

    if session_totals.records > 0 {
        let project = session_project.unwrap_or_else(|| fallback_project.to_string());
        stats.sessions.push(SessionTotals {
            key: String::new(), // 占位，合并时统一生成
            title: ai_title.or(summary),
            project,
            session_id,
            totals: session_totals,
        });
    }

    (stats, parse_errors)
}

/// 把多个文件的 FileStats 合并为最终聚合结果，并统一生成 session 显示 key。
fn merge_file_stats(sources: &[FileStats], name: &str, files_scanned: usize, parse_errors: u64) -> Value {
    let mut total = Totals::default();
    let mut models: HashMap<String, Totals> = HashMap::new();
    let mut projects: HashMap<String, Totals> = HashMap::new();
    let mut daily: HashMap<String, Totals> = HashMap::new();
    let mut daily_by_model: HashMap<String, HashMap<String, Totals>> = HashMap::new();
    let mut hours: HashMap<String, Totals> = HashMap::new();
    let mut sessions: Vec<SessionTotals> = Vec::new();
    let mut coverage_start_at: Option<i64> = None;
    let mut coverage_end_at: Option<i64> = None;

    // 先收集所有 session 的（project, session_id, totals）用于全局计数
    let mut session_key_counts = HashMap::<String, usize>::new();

    for stats in sources {
        total.merge_into(&stats.total);
        for (key, totals) in &stats.models {
            models.entry(key.clone()).or_default().merge_into(totals);
        }
        for (key, totals) in &stats.projects {
            projects.entry(key.clone()).or_default().merge_into(totals);
        }
        for (key, totals) in &stats.daily {
            daily.entry(key.clone()).or_default().merge_into(totals);
        }
        for (model, points) in &stats.daily_by_model {
            let bucket = daily_by_model.entry(model.clone()).or_default();
            for (day, totals) in points {
                bucket.entry(day.clone()).or_default().merge_into(totals);
            }
        }
        for (key, totals) in &stats.hours {
            hours.entry(key.clone()).or_default().merge_into(totals);
        }
        if let Some(ts) = stats.first_ts {
            coverage_start_at = Some(coverage_start_at.map_or(ts, |current| current.min(ts)));
        }
        if let Some(ts) = stats.last_ts {
            coverage_end_at = Some(coverage_end_at.map_or(ts, |current| current.max(ts)));
        }
        for session in &stats.sessions {
            let base_key = format!("{} · {}", session.project, session.session_id);
            let count = session_key_counts.entry(base_key.clone()).or_default();
            *count += 1;
            let key = if *count == 1 {
                base_key
            } else {
                format!("{base_key} · {}", *count)
            };
            sessions.push(SessionTotals {
                key,
                title: session.title.clone(),
                project: session.project.clone(),
                session_id: session.session_id.clone(),
                totals: session.totals.clone(),
            });
        }
    }

    let daily_by_model = daily_by_model
        .into_iter()
        .map(|(model, points)| (model, Value::Array(groups(points))))
        .collect::<Map<String, Value>>();

    json!({
        "source": name,
        "summary": total.value(),
        "models": groups(models),
        "projects": groups(projects),
        "sessions": session_groups(sessions),
        "daily": groups(daily),
        "dailyByModel": daily_by_model,
        "hours": groups(hours),
        "filesScanned": files_scanned,
        "parseErrors": parse_errors,
        "coverageStartAt": coverage_start_at,
        "coverageEndAt": coverage_end_at,
    })
}

#[cfg(test)]
fn source(root: PathBuf, name: &str, cutoff: Option<i64>) -> Value {
    let mut paths = Vec::new();
    files(&root, &mut paths, cutoff);
    paths.sort();
    let mut merged = Vec::new();
    let mut parse_errors = 0_u64;
    for path in &paths {
        let fallback_project = project_name(&root, path);
        let (stats, errors) = scan_file(path, &fallback_project, cutoff);
        parse_errors = parse_errors.saturating_add(errors);
        merged.push(stats);
    }
    merge_file_stats(&merged, name, paths.len(), parse_errors)
}

/// 增量缓存路径：~/.wb-switch/token-stats-cache.json
fn cache_path() -> PathBuf {
    crate::modules::config::store_dir().join("token-stats-cache.json")
}

/// 读取增量缓存；文件缺失 / schema 不符 / 解析失败时返回空缓存（自动失效重建）。
fn load_cache(path: &Path) -> FileCache {
    let Ok(text) = std::fs::read_to_string(path) else {
        return FileCache::default();
    };
    match serde_json::from_str::<FileCache>(&text) {
        Ok(cache) if cache.schema_version == CACHE_SCHEMA_VERSION => cache,
        _ => FileCache::default(),
    }
}

/// 原子写缓存：先写临时文件再 rename，避免写一半崩溃留下损坏缓存。
fn save_cache(path: &Path, cache: &FileCache) {
    let Ok(json) = serde_json::to_string(cache) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    let ok = std::fs::write(&tmp, json).is_ok();
    if ok {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// 增量扫描单个数据源：
/// 1. 读磁盘缓存；2. 遍历文件，指纹（mtime+size）未变则复用缓存聚合，
///    变更/新增则重新解析并更新缓存；3. 缓存中存在但磁盘已删除的条目丢弃；
/// 4. 合并全部文件聚合；5. 写回缓存。
fn source_cached(
    root: PathBuf,
    name: &str,
    days: Option<i64>,
    cutoff: Option<i64>,
    cache: &mut FileCache,
) -> Value {
    cache.schema_version = CACHE_SCHEMA_VERSION;
    let mut paths = Vec::new();
    files(&root, &mut paths, cutoff);
    paths.sort();

    let bucket = bucket_key(days);
    let mut merged: Vec<FileStats> = Vec::new();
    let mut parse_errors = 0_u64;
    let mut seen_keys = std::collections::HashSet::new();

    for path in &paths {
        // 稳定相对路径作为缓存 key，加 source 前缀避免两个数据源冲突
        let rel = format!(
            "{}/{}",
            name,
            path.strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
        );
        let fallback_project = project_name(&root, path);
        let metadata = match path.metadata() {
            Ok(m) => m,
            Err(_) => {
                parse_errors = parse_errors.saturating_add(1);
                continue;
            }
        };
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|modified| {
                modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|duration| duration.as_millis() as i64)
            })
            .unwrap_or(0);
        let size = metadata.len();
        seen_keys.insert(rel.clone());

        // 命中缓存且指纹未变 → 直接复用
        let hit = cache
            .files
            .get(&rel)
            .and_then(|buckets| buckets.get(&bucket))
            .filter(|cached| cached.mtime_ms == mtime_ms && cached.size == size);
        let stats = if let Some(cached) = hit {
            cached.stats.clone()
        } else {
            // 变更/新增：重新解析
            let (stats, errors) = scan_file(path, &fallback_project, cutoff);
            parse_errors = parse_errors.saturating_add(errors);
            // 更新缓存（保留该文件其他时间桶）
            let entry = cache.files.entry(rel).or_default();
            entry.insert(
                bucket.clone(),
                CachedFile {
                    mtime_ms,
                    size,
                    stats: stats.clone(),
                },
            );
            stats
        };
        merged.push(stats);
    }

    // 只删除「当前 source 前缀」下磁盘已不存在的文件条目；
    // 其他 source 的缓存条目必须保留，否则第二个 source 会清空第一个
    // source 刚写入的缓存（codebuddy 为空时会把 workbuddy 缓存全删掉）。
    let prefix = format!("{name}/");
    cache.files.retain(|key, _| {
        if key.starts_with(&prefix) {
            seen_keys.contains(key)
        } else {
            true
        }
    });

    let value = merge_file_stats(&merged, name, paths.len(), parse_errors);
    value
}

/// Return independent WorkBuddy and CodeBuddy CLI aggregates. `days` is
/// interpreted in Rust using the same millisecond clock for both sources.
///
/// 持久化增量缓存：首次调用全量计算所有数据并写入本地缓存文件；之后每次
/// 只重新解析新增/变更的文件（按 mtime+size 指纹判断），未变更文件直接复用
/// 缓存聚合，最终合并覆盖全部数据，保证统计结果完整性与一致性。
pub fn get_statistics(days: Option<i64>) -> Value {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let generated_at = crate::modules::config::now_ms();
    let range_days = match days {
        Some(7) => Some(7),
        Some(30) => Some(30),
        Some(90) => Some(90),
        _ => None,
    };
    let cutoff = range_days.map(|value| generated_at - value * 86_400_000);

    let cache_path = cache_path();
    let mut cache = load_cache(&cache_path);

    let sources = json!([
        source_cached(
            home.join(".workbuddy/projects"),
            "workbuddy",
            days,
            cutoff,
            &mut cache,
        ),
        source_cached(
            home.join(".codebuddy/projects"),
            "codebuddy-cli",
            days,
            cutoff,
            &mut cache,
        ),
    ]);

    save_cache(&cache_path, &cache);

    json!({
        "generatedAt": generated_at,
        "rangeDays": range_days,
        "sources": sources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn usage_priority_aliases_and_raw_cache_write() {
        let value = json!({
            "providerData": {
                "usage": { "inputTokens": 99, "outputTokens": 22 },
                "rawUsage": { "prompt_cache_write_tokens": 2 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        assert_eq!(usage(&value), Some(Usage { input: 10, output: 3, read: 4, write: 2 }));
    }

    #[test]
    fn cache_write_uses_explicit_aliases_but_never_cache_miss() {
        let provider_usage_write = json!({
            "providerData": {
                "usage": {
                    "inputTokens": 99,
                    "outputTokens": 22,
                    "cache_write_input_tokens": 0,
                    "cache_creation_input_tokens": 7
                },
                "rawUsage": {
                    "prompt_cache_miss_tokens": 91,
                    "prompt_cache_write_tokens": 0
                }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        assert_eq!(
            usage(&provider_usage_write),
            Some(Usage {
                input: 10,
                output: 3,
                read: 4,
                write: 7,
            })
        );

        let cache_miss_only = json!({
            "providerData": {
                "rawUsage": { "prompt_cache_miss_tokens": 91 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3
            }}
        });
        assert_eq!(
            usage(&cache_miss_only),
            Some(Usage {
                input: 10,
                output: 3,
                read: 0,
                write: 0,
            })
        );
    }

    #[test]
    fn cache_read_accepts_nested_provider_details() {
        let value = json!({
            "providerData": {
                "usage": {
                    "inputTokens": 99,
                    "outputTokens": 3,
                    "inputTokensDetails": [{ "cached_tokens": 7 }]
                }
            }
        });
        assert_eq!(
            usage(&value),
            Some(Usage {
                input: 99,
                output: 3,
                read: 7,
                write: 0,
            })
        );

        let raw = json!({
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 2,
                "cache_read_input_tokens": 0,
                "prompt_cache_hit_tokens": 12
            }
        });
        assert_eq!(
            usage(&raw),
            Some(Usage {
                input: 20,
                output: 2,
                read: 12,
                write: 0,
            })
        );

    }

    #[test]
    fn source_excludes_subagents_and_counts_each_record_once() {
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-{}-{}",
            std::process::id(),
            crate::modules::config::now_ms()
        ));
        let project = root.join("fixture-project");
        let ignored = project.join("subagents");
        fs::create_dir_all(&ignored).expect("create fixture dirs");
        let record = json!({
            "timestamp": crate::modules::config::now_ms(),
            "providerData": {
                "model": "fixture-model",
                "usage": { "inputTokens": 20, "outputTokens": 5 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        fs::write(project.join("session.jsonl"), format!("{}\nnot-json\n", record))
            .expect("write fixture");
        fs::write(ignored.join("agent.jsonl"), format!("{}\n", record)).expect("write ignored fixture");

        let result = source(root.clone(), "fixture", None);
        assert_eq!(result["filesScanned"], 1);
        assert_eq!(result["parseErrors"], 1);
        assert_eq!(result["summary"]["input"], 10);
        assert_eq!(result["summary"]["output"], 3);
        assert_eq!(result["summary"]["cacheRead"], 4);
        assert_eq!(result["summary"]["total"], 13);
        assert_eq!(result["summary"]["records"], 1);
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(result["dailyByModel"]["fixture-model"][0]["key"], today);
        assert_eq!(result["projects"][0]["key"], "fixture-project");
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn bounded_source_excludes_records_before_cutoff() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-range-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = |timestamp| {
            json!({
                "timestamp": timestamp,
                "cwd": "/fixture/example-project",
                "message": { "usage": { "input_tokens": 10, "output_tokens": 2 } }
            })
        };
        fs::write(
            project.join("session.jsonl"),
            format!("{}\n{}\n", record(now - 10_000), record(now - 100_000)),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", Some(now - 50_000));
        assert_eq!(result["summary"]["records"], 1);
        assert_eq!(result["summary"]["input"], 10);
        assert_eq!(result["projects"][0]["key"], "example-project");
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn session_titles_are_file_scoped_and_independent_from_usage_cutoff() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-titles-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let usage_record = |input| {
            json!({
                "timestamp": now,
                "cwd": "/private/example-project",
                "message": { "usage": { "input_tokens": input, "output_tokens": 2 } }
            })
        };

        fs::write(
            project.join("session-a.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n",
                json!({ "type": "summary", "summary": "摘要不应覆盖 AI 标题" }),
                json!({ "type": "ai-title", "aiTitle": "旧标题" }),
                usage_record(10),
                json!({ "type": "ai-title", "aiTitle": "最新 AI 标题" }),
            ),
        )
        .expect("write ai title fixture");
        fs::write(
            project.join("session-b.jsonl"),
            format!(
                "{}\n{}\n",
                json!({
                    "type": "ai-title",
                    "timestamp": now - 100_000,
                    "aiTitle": "范围外保留标题"
                }),
                usage_record(20),
            ),
        )
        .expect("write cutoff title fixture");
        fs::write(
            project.join("session-c.jsonl"),
            format!(
                "{}\n{}\n",
                usage_record(30),
                json!({ "type": "summary", "summary": "摘要回退标题" }),
            ),
        )
        .expect("write summary fixture");
        fs::write(
            project.join("session-d.jsonl"),
            format!("{}\n", usage_record(40)),
        )
        .expect("write untitled fixture");
        fs::write(
            project.join("session-e.jsonl"),
            format!(
                "{}\n{}\n",
                json!({ "type": "ai-title", "aiTitle": "最新 AI 标题" }),
                usage_record(50),
            ),
        )
        .expect("write duplicate title fixture");

        let result = source(root.clone(), "fixture", Some(now - 50_000));
        let sessions = result["sessions"].as_array().expect("session groups");
        let by_id = |session_id: &str| {
            sessions
                .iter()
                .find(|session| session["sessionId"] == session_id)
                .expect("session group by id")
        };

        assert_eq!(result["summary"]["input"], 150);
        assert_eq!(result["summary"]["records"], 5);
        assert_eq!(sessions.len(), 5);
        assert_eq!(by_id("session-a")["title"], "最新 AI 标题");
        assert_eq!(by_id("session-b")["title"], "范围外保留标题");
        assert_eq!(by_id("session-c")["title"], "摘要回退标题");
        assert!(by_id("session-d")["title"].is_null());
        assert_eq!(by_id("session-a")["project"], "example-project");
        assert_ne!(by_id("session-a")["key"], by_id("session-e")["key"]);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    // ---------- 增量缓存一致性测试 ----------

    fn usage_record(now: i64, input: u64) -> Value {
        json!({
            "timestamp": now,
            "cwd": "/fixture/example-project",
            "message": { "usage": { "input_tokens": input, "output_tokens": 2 } }
        })
    }

    /// 构造临时目录：root/{project}/session.jsonl，返回 root。
    fn fixture_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-{tag}-{}-{}",
            std::process::id(),
            crate::modules::config::now_ms()
        ));
        fs::create_dir_all(root.join("fixture-project")).expect("create fixture dirs");
        root
    }

    #[test]
    fn incremental_cache_full_scan_then_merge_matches_direct_source() {
        let now = crate::modules::config::now_ms();
        let root = fixture_root("incremental-1");
        fs::write(
            root.join("fixture-project/session-a.jsonl"),
            format!("{}\n{}\n", usage_record(now, 10), usage_record(now, 20)),
        )
        .expect("write a");
        fs::write(
            root.join("fixture-project/session-b.jsonl"),
            format!("{}\n", usage_record(now, 30)),
        )
        .expect("write b");

        // 直接 source（无缓存）作为基准
        let baseline = source(root.clone(), "fixture", None);
        assert_eq!(baseline["summary"]["input"], 60);

        // 增量缓存：首次全量
        let mut cache = FileCache::default();
        let first = source_cached(root.clone(), "fixture", None, None, &mut cache);
        assert_eq!(first["summary"]["input"], 60);
        assert_eq!(first["filesScanned"], 2);
        assert_eq!(cache.files.len(), 2, "首次全量应缓存 2 个文件");

        // 未变更再扫：指纹命中，结果一致，缓存条目数不变
        let second = source_cached(root.clone(), "fixture", None, None, &mut cache);
        assert_eq!(second["summary"]["input"], 60);
        assert_eq!(second["filesScanned"], 2);
        assert_eq!(cache.files.len(), 2);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn incremental_cache_only_reparses_changed_file_and_drops_deleted() {
        let now = crate::modules::config::now_ms();
        let root = fixture_root("incremental-2");
        fs::write(
            root.join("fixture-project/session-a.jsonl"),
            format!("{}\n", usage_record(now, 10)),
        )
        .expect("write a");
        fs::write(
            root.join("fixture-project/session-b.jsonl"),
            format!("{}\n", usage_record(now, 30)),
        )
        .expect("write b");

        let mut cache = FileCache::default();
        let first = source_cached(root.clone(), "fixture", None, None, &mut cache);
        assert_eq!(first["summary"]["input"], 40);

        // 变更 a（追加记录 input 20），删除 b
        fs::write(
            root.join("fixture-project/session-a.jsonl"),
            format!("{}\n{}\n", usage_record(now, 10), usage_record(now, 20)),
        )
        .expect("rewrite a");
        fs::remove_file(root.join("fixture-project/session-b.jsonl")).expect("remove b");

        // 增量重扫：a 重解析(10+20=30)，b 从缓存中移除
        let second = source_cached(root.clone(), "fixture", None, None, &mut cache);
        assert_eq!(second["summary"]["input"], 30, "增量后应只含 a 的 30");
        assert_eq!(second["filesScanned"], 1, "只剩 a 一个文件");
        assert_eq!(cache.files.len(), 1, "缓存应只剩 a");
        assert!(
            cache.files.keys().all(|k| k.contains("session-a")),
            "缓存 key 应只含 session-a"
        );

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn incremental_cache_buckets_are_independent_by_days() {
        let now = crate::modules::config::now_ms();
        let root = fixture_root("incremental-3");
        // 一个 10 天前的旧记录 + 一个今天的新记录
        fs::write(
            root.join("fixture-project/session-a.jsonl"),
            format!(
                "{}\n{}\n",
                usage_record(now - 10 * 86_400_000, 100),
                usage_record(now, 5)
            ),
        )
        .expect("write a");

        let mut cache = FileCache::default();
        // 全量桶
        let all = source_cached(root.clone(), "fixture", None, None, &mut cache);
        assert_eq!(all["summary"]["input"], 105, "全量应含两条");
        // 7 天桶：cutoff 过滤掉旧记录
        let cutoff = now - 7 * 86_400_000;
        let week = source_cached(root.clone(), "fixture", Some(7), Some(cutoff), &mut cache);
        assert_eq!(week["summary"]["input"], 5, "7天桶只应含今天的 5");
        // 桶独立：全量桶不受 7 天桶影响
        let all_again = source_cached(root.clone(), "fixture", None, None, &mut cache);
        assert_eq!(all_again["summary"]["input"], 105, "全量桶应保持独立");

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn cache_roundtrip_via_serde_preserves_stats() {
        let now = crate::modules::config::now_ms();
        let root = fixture_root("incremental-4");
        fs::write(
            root.join("fixture-project/session-a.jsonl"),
            format!("{}\n", usage_record(now, 42)),
        )
        .expect("write a");

        // 走完整的 load → scan → save → load → scan 周期
        let cache_file = root.join("cache.json");
        let mut cache = FileCache::default();
        let first = source_cached(root.clone(), "fixture", None, None, &mut cache);
        assert_eq!(first["summary"]["input"], 42);
        save_cache(&cache_file, &cache);

        let loaded = load_cache(&cache_file);
        assert_eq!(loaded.schema_version, CACHE_SCHEMA_VERSION);
        let second = source_cached(root.clone(), "fixture", None, None, &mut cache);
        assert_eq!(second["summary"]["input"], 42, "roundtrip 后结果一致");

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn multiple_sources_do_not_clear_each_others_cache() {
        let now = crate::modules::config::now_ms();
        let root = fixture_root("multi-source");

        // 两个数据源：workbuddy 有 1 个文件，codebuddy 为空（模拟用户环境）
        let wb = root.join("wb");
        let cb = root.join("cb");
        fs::create_dir_all(wb.join("project")).expect("create wb dir");
        fs::create_dir_all(&cb).expect("create cb dir");
        fs::write(
            wb.join("project/session-a.jsonl"),
            format!("{}\n", usage_record(now, 42)),
        )
        .expect("write wb session");

        let mut cache = FileCache::default();
        // 先扫 workbuddy（有数据），再扫 codebuddy（空目录）
        let wb_result = source_cached(wb.clone(), "workbuddy", None, None, &mut cache);
        assert_eq!(wb_result["summary"]["input"], 42);
        assert_eq!(cache.files.len(), 1, "workbuddy 扫描后应有 1 个缓存条目");

        let cb_result = source_cached(cb.clone(), "codebuddy-cli", None, None, &mut cache);
        assert_eq!(cb_result["summary"]["input"], 0);
        // 关键断言：codebuddy 扫描后，workbuddy 的缓存必须还在
        assert_eq!(
            cache.files.len(),
            1,
            "codebuddy 空扫描不应清空 workbuddy 的缓存条目（回归 bug）"
        );

        // 再扫一次 workbuddy，应命中缓存（指纹未变）
        let wb_again = source_cached(wb.clone(), "workbuddy", None, None, &mut cache);
        assert_eq!(wb_again["summary"]["input"], 42, "workbuddy 缓存应被保留并可复用");

        fs::remove_dir_all(root).expect("remove fixture");
    }
}
