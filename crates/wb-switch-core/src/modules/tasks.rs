//! WorkBuddy 成长中心任务查询。
//!
//! 官方接口 `/activity/growth/tasks/`（GET）返回当前账号的成长任务列表。
//! 这里只统计「可完成」（status=available）的任务数量，供账号卡片展示，
//! 不把完整任务/token 交给前端。
//!
//! 复用签到/旅行同一套鉴权与刷新逻辑：`build_auth_headers` + `http_request`，
//! token 失效时通过 refresh_token 刷新重试一次。

use serde_json::{json, Value};

use crate::modules::account::{account_display_name, build_auth_headers};
use crate::modules::config::{http_request, WORKBUDDY_API_ENDPOINT};
use crate::modules::refresh::{ensure_fresh_token, refresh_account_token};

/// 成长任务接口路径前缀。
const TASKS_PREFIX: &str = "/activity/growth/tasks/";

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

/// 拉取当前账号成长任务列表（优先 GET，失败回退 POST）。
async fn fetch_tasks(account: &Value) -> Value {
    let url = format!("{WORKBUDDY_API_ENDPOINT}{TASKS_PREFIX}");
    let headers = build_auth_headers(account);
    let mut resp = http_request(&url, "GET", None, Some(&headers)).await;
    if is_unauthorized(&resp)
        && !account
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
    {
        let refreshed = refresh_account_token(account.clone()).await;
        let headers = build_auth_headers(&refreshed);
        resp = http_request(&url, "GET", None, Some(&headers)).await;
    }
    resp
}

/// 解析任务响应：统计可完成（available）任务数，返回前端友好的扁平结构。
fn summarize_tasks(resp: &Value) -> Value {
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        return json!({
            "ok": false,
            "error": resp.get("message")
                .or_else(|| resp.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap_or(&format!("code={code}"))
                .to_string(),
        });
    }
    let tasks = resp
        .get("data")
        .and_then(|v| v.get("tasks"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut available = 0_u64;
    let mut titles = Vec::new();
    for task in &tasks {
        let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let title = task.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if status == "available" {
            available = available.saturating_add(1);
            titles.push(title.to_string());
        }
    }
    json!({
        "ok": true,
        "available": available,
        "total": tasks.len(),
        "tasks": titles.iter().take(10).collect::<Vec<_>>(),
    })
}

/// 前端入口：按账号查询可完成任务数量（脱敏）。
pub async fn available_tasks_for(account: &Value) -> Value {
    let cfg = crate::modules::config::load_checkin_config();
    let acc = ensure_fresh_token(account.clone(), &cfg).await;
    let resp = fetch_tasks(&acc).await;
    let summary = summarize_tasks(&resp);
    json!({
        "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
        "email": account_display_name(&acc),
        "tasks": summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_counts_only_available() {
        let resp = json!({
            "code": 0,
            "data": { "tasks": [
                {"title": "完成一次对话", "status": "available"},
                {"title": "完成技能安装", "status": "available"},
                {"title": "链接微信", "status": "claimed"},
                {"title": "召唤一次专家", "status": "completed"},
            ]}
        });
        let s = summarize_tasks(&resp);
        assert_eq!(s["available"], 2);
        assert_eq!(s["total"], 4);
        assert_eq!(s["tasks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn summarize_error_passthrough() {
        let resp = json!({"code": 500, "message": "boom"});
        let s = summarize_tasks(&resp);
        assert_eq!(s["ok"], false);
        assert_eq!(s["error"], "boom");
    }

    #[test]
    fn summarize_empty_data() {
        let resp = json!({"code": 0, "data": {}});
        let s = summarize_tasks(&resp);
        assert_eq!(s["available"], 0);
        assert_eq!(s["total"], 0);
    }
}
