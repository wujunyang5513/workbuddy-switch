//! 猫猫旅行（GrowthSpace / Buddy Travel）。
//!
//! 官方接口前缀 `/activity/growth/buddy/travel`，本模块封装：
//!   - config  查询可去目的地列表（GET）
//!   - status  查询当前旅行状态（GET）
//!   - depart  派遣旅行（POST，body={"location_id": <int>}）
//!   - claim   领取旅行奖励（POST，body={}）
//!
//! 复用签到/积分同一套鉴权与刷新逻辑：`build_auth_headers` + `http_request`，
//! token 失效时通过 refresh_token 刷新重试一次。只把脱敏结果交给上层。

use serde_json::{json, Value};

use crate::modules::account::{account_display_name, build_auth_headers};
use crate::modules::config::{http_request, WORKBUDDY_API_ENDPOINT};
use crate::modules::refresh::{ensure_fresh_token, refresh_account_token};

/// 旅行接口路径前缀（host 用统一的 WORKBUDDY_API_ENDPOINT）。
const TRAVEL_PREFIX: &str = "/activity/growth/buddy/travel";

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

/// 发送旅行接口请求；遇到未授权且存在 refresh token 时刷新一次并重试。
async fn travel_request(path: &str, account: &Value, body: Option<Value>) -> Value {
    let url = format!("{WORKBUDDY_API_ENDPOINT}{TRAVEL_PREFIX}{path}");
    let headers = build_auth_headers(account);
    let mut resp = http_request(&url, "POST", body.clone().or(Some(json!({}))), Some(&headers)).await;
    if is_unauthorized(&resp)
        && !account
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
    {
        let refreshed = refresh_account_token(account.clone()).await;
        let headers = build_auth_headers(&refreshed);
        resp = http_request(&url, "POST", body.or(Some(json!({}))), Some(&headers)).await;
    }
    resp
}

/// 拉取可去目的地列表（优先用 GET，失败回退 POST）。
pub async fn get_travel_config(account: &Value) -> Value {
    let url = format!("{WORKBUDDY_API_ENDPOINT}{TRAVEL_PREFIX}/config");
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
    if resp.get("code").and_then(|v| v.as_i64()) == Some(0) {
        return resp;
    }
    // 部分网关只接受 POST，回退一次。
    travel_request("/config", account, Some(json!({}))).await
}

/// 查询当前旅行状态。
pub async fn get_travel_status(account: &Value) -> Value {
    let url = format!("{WORKBUDDY_API_ENDPOINT}{TRAVEL_PREFIX}/status");
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

/// 派遣旅行到指定目的地。
pub async fn depart_travel(account: &Value, location_id: i64) -> Value {
    travel_request("/depart", account, Some(json!({"location_id": location_id}))).await
}

/// 领取旅行奖励（幂等）。
pub async fn claim_travel(account: &Value) -> Value {
    travel_request("/claim", account, Some(json!({}))).await
}

/// 解析状态响应的关键字段，返回前端友好的扁平结构（不泄露 token 等敏感信息）。
fn summarize_status(resp: &Value) -> Value {
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
    let d = resp.get("data").cloned().unwrap_or_else(|| json!({}));
    let loc = d.get("location").cloned().unwrap_or_else(|| json!({}));
    let arrived = match (d.get("arrive_at").and_then(|v| v.as_i64()),
                         d.get("server_now").and_then(|v| v.as_i64())) {
        (Some(arrive), Some(now)) => now >= arrive,
        _ => false,
    };
    json!({
        "ok": true,
        "state": d.get("state").and_then(|v| v.as_str()).unwrap_or("unknown"),
        "buddyName": d.get("buddy_name").and_then(|v| v.as_str()).unwrap_or(""),
        "locationId": loc.get("id").cloned().unwrap_or(Value::Null),
        "locationCode": loc.get("code").and_then(|v| v.as_str()).unwrap_or(""),
        "locationName": loc.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        "arriveAt": d.get("arrive_at").cloned().unwrap_or(Value::Null),
        "serverNow": d.get("server_now").cloned().unwrap_or(Value::Null),
        "rewardCredit": d.get("reward_credit").cloned().unwrap_or(Value::Null),
        "dailyLimitReached": d
            .get("daily_limit_reached")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "hasLetter": d.get("letter").is_some(),
        "arrived": arrived,
    })
}

/// 前端入口：按账号查询旅行状态（脱敏）。
pub async fn travel_status_for(account: &Value) -> Value {
    let cfg = crate::modules::config::load_checkin_config();
    let acc = ensure_fresh_token(account.clone(), &cfg).await;
    let resp = get_travel_status(&acc).await;
    let summary = summarize_status(&resp);
    if summary.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return json!({
            "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
            "email": account_display_name(&acc),
            "travel": summary,
        });
    }
    json!({
        "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
        "email": account_display_name(&acc),
        "travel": summary,
    })
}

/// 前端入口：按账号派遣旅行。
/// `location_id` 传 0 表示自动按日期轮转挑选目的地。
pub async fn depart_for(account: &Value, location_id: i64) -> Value {
    let cfg = crate::modules::config::load_checkin_config();
    let acc = ensure_fresh_token(account.clone(), &cfg).await;

    // 先读状态：非空闲时不派遣。
    let st = get_travel_status(&acc).await;
    let d = st.get("data").cloned().unwrap_or_else(|| json!({}));
    let state = d.get("state").and_then(|v| v.as_str()).unwrap_or("unknown").to_lowercase();
    if state != "idle" {
        return json!({
            "ok": false,
            "skipped": true,
            "reason": format!("猫猫当前处于 '{state}' 状态，仅空闲时可派遣"),
            "email": account_display_name(&acc),
        });
    }
    if d
        .get("daily_limit_reached")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return json!({
            "ok": false,
            "skipped": true,
            "reason": "今日派遣次数已达上限（daily_limit_reached），明日再试",
            "email": account_display_name(&acc),
        });
    }

    // 确定目的地：显式指定 > 按日期轮转。
    let mut loc_id = location_id;
    let mut loc_name = loc_id.to_string();
    if loc_id <= 0 {
        let cfg_resp = get_travel_config(&acc).await;
        let locations = cfg_resp
            .get("data")
            .and_then(|v| v.get("locations"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if locations.is_empty() {
            loc_id = 1; // 兜底默认目的地
        } else {
            let ids: Vec<i64> = locations
                .iter()
                .filter_map(|l| l.get("id").and_then(|v| v.as_i64()))
                .collect();
            if ids.is_empty() {
                loc_id = 1;
            } else {
                // 按日期轮转：用「年份*366 + 年内第几天」作为稳定递增序号。
                use chrono::Datelike;
                let now = chrono::Local::now();
                let day = (now.year() * 366 + now.ordinal() as i32) as usize;
                loc_id = ids[day % ids.len()];
            }
            loc_name = locations
                .iter()
                .find(|l| l.get("id").and_then(|v| v.as_i64()) == Some(loc_id))
                .and_then(|l| l.get("name").and_then(|v| v.as_str()))
                .unwrap_or(&loc_id.to_string())
                .to_string();
        }
    }

    let resp = depart_travel(&acc, loc_id).await;
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 0 {
        json!({
            "ok": true,
            "locationId": loc_id,
            "locationName": loc_name,
            "email": account_display_name(&acc),
            "raw": resp.get("data").cloned().unwrap_or_else(|| json!({})),
        })
    } else {
        json!({
            "ok": false,
            "error": resp.get("message")
                .or_else(|| resp.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap_or(&format!("code={code}"))
                .to_string(),
            "email": account_display_name(&acc),
        })
    }
}

/// 前端入口：按账号领取旅行奖励（幂等，未到达时返回提示）。
pub async fn claim_for(account: &Value) -> Value {
    let cfg = crate::modules::config::load_checkin_config();
    let acc = ensure_fresh_token(account.clone(), &cfg).await;

    // 未到达不领取。
    let st = get_travel_status(&acc).await;
    let d = st.get("data").cloned().unwrap_or_else(|| json!({}));
    let arrived = match (d.get("arrive_at").and_then(|v| v.as_i64()),
                         d.get("server_now").and_then(|v| v.as_i64())) {
        (Some(arrive), Some(now)) => now >= arrive,
        _ => false,
    };
    if !arrived {
        let state = d.get("state").and_then(|v| v.as_str()).unwrap_or("unknown");
        return json!({
            "ok": false,
            "skipped": true,
            "reason": format!("猫猫旅行尚未到达（state={state}），暂无可领取奖励"),
            "email": account_display_name(&acc),
        });
    }

    let resp = claim_travel(&acc).await;
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 0 {
        json!({
            "ok": true,
            "rewardCredit": d.get("reward_credit").cloned().unwrap_or(Value::Null),
            "hasLetter": d.get("letter").is_some(),
            "email": account_display_name(&acc),
            "raw": resp.get("data").cloned().unwrap_or_else(|| json!({})),
        })
    } else {
        json!({
            "ok": false,
            "error": resp.get("message")
                .or_else(|| resp.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap_or(&format!("code={code}"))
                .to_string(),
            "email": account_display_name(&acc),
        })
    }
}

// ---------------------------------------------------------------------------
// 批量操作：一键派遣全部 / 一键领取全部
// ---------------------------------------------------------------------------

/// 记录一次批量旅行操作日志。
fn record_travel_log(kind: &str, trigger: &str, summary: &Value) {
    use crate::modules::config::add_travel_log;
    add_travel_log(&json!({
        "ts": crate::modules::config::now_ms(),
        "kind": kind,          // "depart" | "claim"
        "trigger": trigger,    // "manual" | "auto"
        "summary": summary,
    }));
}

/// 汇总批量结果，返回 `{ total, ok, skipped, failed, accounts: [...] }`。
fn build_travel_summary(kind: &str, results: Vec<Value>) -> Value {
    let total = results.len();
    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for r in &results {
        if r.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            ok += 1;
        } else if r.get("skipped").and_then(|v| v.as_bool()) == Some(true) {
            skipped += 1;
        } else {
            failed += 1;
        }
    }
    json!({
        "kind": kind,
        "total": total,
        "ok": ok,
        "skipped": skipped,
        "failed": failed,
        "accounts": results,
    })
}

/// 一键派遣全部：把所有处于空闲、且今日未达上限的账号全部派出。
/// `location_id` 传 0 表示每个账号都自动按日期轮转挑选目的地。
pub async fn depart_all_for(location_id: i64, trigger: &str) -> Value {
    let accounts = crate::modules::account::load_accounts();
    let mut results: Vec<Value> = Vec::new();
    for acc in accounts {
        let r = depart_for(&acc, location_id).await;
        results.push(r);
    }
    let summary = build_travel_summary("depart", results);
    record_travel_log("depart", trigger, &summary);
    summary
}

/// 一键领取全部：把所有已到达的账号奖励一次性领取。
pub async fn claim_all_for(trigger: &str) -> Value {
    let accounts = crate::modules::account::load_accounts();
    let mut results: Vec<Value> = Vec::new();
    for acc in accounts {
        let r = claim_for(&acc).await;
        results.push(r);
    }
    let summary = build_travel_summary("claim", results);
    record_travel_log("claim", trigger, &summary);
    summary
}
