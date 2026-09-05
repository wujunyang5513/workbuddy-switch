//! WorkBuddy 成长中心任务查询与操作。
//!
//! 官方接口（对齐成长中心 SPA 实际调用）：
//!   - 任务列表：GET  /v2/activity/growth/tasks
//!   - 接受任务：POST /activity/growth/tasks/accept        body {"task_codes": [...]}
//!   - 领取奖励：POST /activity/growth/tasks/<task_code>/claim
//!
//! 任务状态字段 `accept_status`（对齐页面枚举）：
//!   not_accepted → 去完成（未接受）
//!   accepted / in_progress → 进行中
//!   completed → 可领取（已完成条件、奖励待领）
//!   claimed → 已完成（奖励已领）
//!
//! 页面「未完成任务 N」= accept_status != claimed 的任务数。
//! 这里只返回脱敏统计与操作结果，不把完整任务/token 交给前端。

use serde_json::{json, Value};

use crate::modules::account::{account_display_name, build_auth_headers};
use crate::modules::config::{http_request, WORKBUDDY_API_ENDPOINT};
use crate::modules::refresh::{ensure_fresh_token, refresh_account_token};

/// 成长任务接口路径。
const TASKS_LIST_PATH: &str = "/v2/activity/growth/tasks";
const TASKS_ACCEPT_PATH: &str = "/activity/growth/tasks/accept";
const TASKS_CLAIM_PREFIX: &str = "/activity/growth/tasks/";

/// 是否因 token 失效被拒（触发刷新重试）。
fn is_unauthorized(resp: &Value) -> bool {
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 401 || code == 403 {
        return true;
    }
    let msg = resp
        .get("message")
        .or_else(|| resp.get("msg"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    ["unauthorized", "401", "登录", "失效", "过期", "token"]
        .iter()
        .any(|k| msg.contains(k))
}

/// 带刷新重试的 GET 请求。
async fn get_with_retry(account: &Value, url: &str) -> Value {
    let headers = build_auth_headers(account);
    let mut resp = http_request(url, "GET", None, Some(&headers)).await;
    if is_unauthorized(&resp)
        && !account
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
    {
        let refreshed = refresh_account_token(account.clone()).await;
        let headers = build_auth_headers(&refreshed);
        resp = http_request(url, "GET", None, Some(&headers)).await;
    }
    resp
}

/// 带刷新重试的 POST 请求。
async fn post_with_retry(account: &Value, url: &str, body: Value) -> Value {
    let headers = build_auth_headers(account);
    let mut resp = http_request(url, "POST", Some(body.clone()), Some(&headers)).await;
    if is_unauthorized(&resp)
        && !account
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
    {
        let refreshed = refresh_account_token(account.clone()).await;
        let headers = build_auth_headers(&refreshed);
        resp = http_request(url, "POST", Some(body), Some(&headers)).await;
    }
    resp
}

/// 拉取任务列表并返回原始 data.tasks 数组。
async fn fetch_tasks(account: &Value) -> Vec<Value> {
    let url = format!("{WORKBUDDY_API_ENDPOINT}{TASKS_LIST_PATH}");
    let resp = get_with_retry(account, &url).await;
    resp.get("data")
        .and_then(|d| d.get("tasks"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 任务状态：claimed=true 表示已完成（奖励已领），其余视为「未完成」。
fn is_claimed(task: &Value) -> bool {
    task.get("accept_status").and_then(|v| v.as_str()) == Some("claimed")
}

/// 解析任务响应，返回未完成统计（对齐页面「未完成任务 N」）。
fn summarize_tasks(tasks: &[Value]) -> Value {
    let total = tasks.len();
    let mut not_claimed = Vec::new();
    let mut claimable = Vec::new();
    for task in tasks {
        let status = task.get("accept_status").and_then(|v| v.as_str()).unwrap_or("");
        let title = task.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let has_reward = task.get("has_reward").and_then(|v| v.as_bool()).unwrap_or(false);
        let locked = task.get("locked").and_then(|v| v.as_bool()).unwrap_or(false);
        if !locked && status != "claimed" {
            not_claimed.push(title.clone());
        }
        if !locked && status == "completed" && has_reward {
            claimable.push(title);
        }
    }
    json!({
        "ok": true,
        "todo": not_claimed.len(),          // 未完成任务数（accept_status != claimed）
        "total": total,
        "claimable": claimable.len(),       // 可领取奖励数（completed && has_reward）
        "titles": not_claimed.iter().take(10).collect::<Vec<_>>(),
    })
}

/// 前端入口：查询账号的未完成/可领取任务统计（脱敏）。
pub async fn available_tasks_for(account: &Value) -> Value {
    let cfg = crate::modules::config::load_checkin_config();
    let acc = ensure_fresh_token(account.clone(), &cfg).await;
    let tasks = fetch_tasks(&acc).await;
    let summary = summarize_tasks(&tasks);
    json!({
        "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
        "email": account_display_name(&acc),
        "tasks": summary,
    })
}

/// 一键接受所有未接受（not_accepted）的任务。
/// 接受后任务进入进行中/可完成状态，用户才能在客户端完成对应动作。
pub async fn accept_all_tasks(account: &Value) -> Value {
    let cfg = crate::modules::config::load_checkin_config();
    let acc = ensure_fresh_token(account.clone(), &cfg).await;
    let tasks = fetch_tasks(&acc).await;
    let not_accepted: Vec<String> = tasks
        .iter()
        .filter(|t| {
            t.get("accept_status").and_then(|v| v.as_str()) == Some("not_accepted")
                && !t.get("locked").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .filter_map(|t| t.get("task_code").and_then(|v| v.as_str()).map(String::from))
        .collect();

    let mut ok = 0_u64;
    let mut failed: Vec<Value> = Vec::new();
    let mut accepted: Vec<String> = Vec::new();
    if !not_accepted.is_empty() {
        let url = format!("{WORKBUDDY_API_ENDPOINT}{TASKS_ACCEPT_PATH}");
        let body = json!({ "task_codes": not_accepted.clone() });
        let resp = post_with_retry(&acc, &url, body).await;
        if resp.get("code").and_then(|v| v.as_i64()) == Some(0) {
            ok = 1;
            accepted = not_accepted.clone();
        } else {
            failed.push(resp.clone());
        }
    }

    json!({
        "ok": true,
        "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
        "email": account_display_name(&acc),
        "result": {
            "attempted": not_accepted.len(),
            "accepted": accepted,
            "ok": ok,
            "failed": failed,
        },
    })
}

/// 一键领取所有可领取（completed && has_reward）任务的奖励。
pub async fn claim_all_tasks(account: &Value) -> Value {
    let cfg = crate::modules::config::load_checkin_config();
    let acc = ensure_fresh_token(account.clone(), &cfg).await;
    let tasks = fetch_tasks(&acc).await;
    let claimable: Vec<Value> = tasks
        .iter()
        .filter(|t| {
            t.get("accept_status").and_then(|v| v.as_str()) == Some("completed")
                && t.get("has_reward").and_then(|v| v.as_bool()).unwrap_or(false)
                && !t.get("locked").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .cloned()
        .collect();

    let mut claimed: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    for task in &claimable {
        let code = task.get("task_code").and_then(|v| v.as_str()).unwrap_or("");
        let title = task.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if code.is_empty() {
            continue;
        }
        let url = format!("{WORKBUDDY_API_ENDPOINT}{TASKS_CLAIM_PREFIX}{code}/claim");
        let resp = post_with_retry(&acc, &url, json!({})).await;
        if resp.get("code").and_then(|v| v.as_i64()) == Some(0) {
            claimed.push(code.to_string());
        } else {
            let is_already = {
                let msg = resp
                    .get("message")
                    .or_else(|| resp.get("msg"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                ["already", "claimed", "已领取", "已完成"].iter().any(|k| msg.contains(k))
            };
            if is_already {
                skipped.push(code.to_string());
            } else {
                errors.push(json!({"task_code": code, "title": title, "resp": resp}));
            }
        }
    }

    json!({
        "ok": true,
        "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
        "email": account_display_name(&acc),
        "result": {
            "found": claimable.len(),
            "claimed": claimed,
            "skipped": skipped,
            "errors": errors,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(code: &str, status: &str, has_reward: bool, locked: bool) -> Value {
        json!({
            "task_code": code,
            "title": format!("任务-{code}"),
            "accept_status": status,
            "has_reward": has_reward,
            "locked": locked,
        })
    }

    #[test]
    fn summarize_counts_unclaimed_as_todo() {
        let tasks = vec![
            task("a", "not_accepted", true, false),
            task("b", "accepted", true, false),
            task("c", "in_progress", true, false),
            task("d", "completed", true, false), // 可领取（未领取前也算未完成）
            task("e", "claimed", true, false),
            task("f", "claimed", true, true),     // locked 不算
        ];
        let s = summarize_tasks(&tasks);
        // 页面口径：未完成 = accept_status != claimed（completed 领取前也算未完成）
        assert_eq!(s["todo"], 4, "not_accepted+accepted+in_progress+completed 均未 claimed");
        assert_eq!(s["claimable"], 1, "completed 且 has_reward 可领");
        assert_eq!(s["total"], 6);
    }

    #[test]
    fn summarize_excludes_locked() {
        let tasks = vec![
            task("a", "not_accepted", true, false),
            task("b", "not_accepted", true, true), // locked 不算未完成
            task("c", "completed", true, true),    // locked 不可领
        ];
        let s = summarize_tasks(&tasks);
        assert_eq!(s["todo"], 1);
        assert_eq!(s["claimable"], 0);
    }
}
