//! 重复会话去重清理。
//!
//! 背景：账号迁移 / 云端同步时，同一批会话可能被以相同
//! `(user_id, last_activity_at, title, cwd)` 复制成多行，导致会话列表出现
//! 大量"同秒同标题"的重复项。
//!
//! 去重判据（严格，避免误删）：
//!   - 仅当 `(user_id, last_activity_at, title, cwd)` 四字段完全一致时才判定为重复，
//!     其中 `last_activity_at` 必须精确到毫秒一致（不能按分钟/秒粗分）。
//!   - 已软删（`deleted_at` 非空）的行不参与、也不动。
//!
//! 策略：每组重复中保留 `id` 最小（最早创建）的一条，其余执行【软删】——
//! 置 `deleted_at`，不做物理 DELETE、不 VACUUM。这样即使 WorkBuddy 主程序并发
//! 写库也不会损坏数据库（吸取并发物理删导致 db malformed 的教训）。
//!
//! 对照 migrate.py 的 `dedup_sessions()`（Python 版），此处为 Rust 版，供
//! wb-switch 桌面客户端集成调用。

use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

use crate::modules::config::now_ms;
use crate::modules::session::workbuddy_db_path;

/// 一组重复会话。
struct DupGroup {
    last_activity_at: Option<i64>,
    title: Option<String>,
    cwd: Option<String>,
    /// 保留的会话 id（id 最小，最早创建）
    keep_id: String,
    /// 待软删的会话 id 列表
    dup_ids: Vec<String>,
}

impl DupGroup {
    fn to_json(&self) -> Value {
        json!({
            "lastActivityAt": self.last_activity_at.unwrap_or(0),
            "title": self.title.clone().unwrap_or_default(),
            "cwd": self.cwd.clone().unwrap_or_default(),
            "count": self.dup_ids.len() + 1,
            "keepId": self.keep_id,
            "dupIds": self.dup_ids,
        })
    }
}

/// 打开数据库并设置 busy_timeout（对照 session::open_db，但本地私有）。
fn open_db(path: &Path, read_only: bool) -> Option<Connection> {
    let conn = if read_only {
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?
    } else {
        Connection::open(path).ok()?
    };
    let _ = conn.busy_timeout(Duration::from_secs(5));
    Some(conn)
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        == 1
}

/// 返回去重统计（预览，只读不删）。
///
/// 返回形如：
/// ```json
/// {
///   "uid": "...",
///   "groups": [{ "lastActivityAt": 123, "title": "...", "cwd": "...", "count": 3, "keepId": "x", "dupIds": ["y","z"] }],
///   "totalGroups": 2,
///   "totalToDelete": 3,
///   "ok": true
/// }
/// ```
/// 若当前账号无任何会话，`uid` 为 null。
pub fn dedup_preview(uid: Option<String>) -> Value {
    let db = workbuddy_db_path();
    if !db.is_file() {
        return json!({ "ok": false, "error": "未找到 workbuddy.db" });
    }
    let Some(conn) = open_db(&db, true) else {
        return json!({ "ok": false, "error": "无法打开 workbuddy.db" });
    };
    if !table_exists(&conn, "sessions") {
        return json!({ "ok": false, "error": "sessions 表不存在" });
    }

    let uid = match uid {
        Some(u) if !u.trim().is_empty() => u,
        _ => match crate::modules::session::current_user_uid() {
            Some(u) => u,
            None => return json!({ "ok": false, "error": "无法确定当前账号" }),
        },
    };

    let groups = find_dup_groups(&conn, &uid);
    let total_to_delete: usize = groups.iter().map(|g| g.dup_ids.len()).sum();

    json!({
        "ok": true,
        "uid": uid,
        "groups": groups.iter().map(|g| g.to_json()).collect::<Vec<_>>(),
        "totalGroups": groups.len(),
        "totalToDelete": total_to_delete,
    })
}

/// 执行软删，返回实际删除结果。入参结构与 `dedup_preview` 相同。
pub fn dedup_execute(uid: Option<String>) -> Value {
    let db = workbuddy_db_path();
    if !db.is_file() {
        return json!({ "ok": false, "error": "未找到 workbuddy.db" });
    }
    let Some(conn) = open_db(&db, false) else {
        return json!({ "ok": false, "error": "无法打开 workbuddy.db" });
    };
    if !table_exists(&conn, "sessions") {
        return json!({ "ok": false, "error": "sessions 表不存在" });
    }

    // checkpoint WAL，确保读到最新数据（并发写库时也能拿到全量）
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");

    let uid = match uid {
        Some(u) if !u.trim().is_empty() => u,
        _ => match crate::modules::session::current_user_uid() {
            Some(u) => u,
            None => return json!({ "ok": false, "error": "无法确定当前账号" }),
        },
    };

    let groups = find_dup_groups(&conn, &uid);
    let total_to_delete: usize = groups.iter().map(|g| g.dup_ids.len()).sum();

    if total_to_delete == 0 {
        return json!({
            "ok": true,
            "uid": uid,
            "deleted": 0,
            "groups": [],
            "totalGroups": 0,
            "message": "未发现重复会话，无需清理",
        });
    }

    // 逐组软删（置 deleted_at），保留每组第一条（id 最小）
    let now = now_ms();
    let mut deleted = 0usize;
    let mut tx_ok = true;
    if let Err(_) = conn.execute("BEGIN IMMEDIATE", []) {
        tx_ok = false;
    }
    if tx_ok {
        for g in &groups {
            if g.dup_ids.is_empty() {
                continue;
            }
            let placeholders = std::iter::repeat("?").take(g.dup_ids.len()).collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE sessions SET deleted_at = ?1 WHERE id IN ({})",
                placeholders
            );
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&now];
            for id in &g.dup_ids {
                params.push(id);
            }
            match conn.execute(&sql, rusqlite::params_from_iter(params)) {
                Ok(n) => deleted += n,
                Err(_) => {
                    tx_ok = false;
                    let _ = conn.execute("ROLLBACK", []);
                    break;
                }
            }
        }
        if tx_ok {
            let _ = conn.execute("COMMIT", []);
        }
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }

    if !tx_ok {
        return json!({ "ok": false, "error": "软删事务失败，未做任何修改" });
    }

    json!({
        "ok": true,
        "uid": uid,
        "deleted": deleted,
        "totalGroups": groups.len(),
        "totalToDelete": total_to_delete,
    })
}

/// 找出重复组。每组含保留的 keep_id（id 最小）和待删的 dup_ids。
fn find_dup_groups(conn: &Connection, uid: &str) -> Vec<DupGroup> {
    // 按四字段分组，取 count>1 的组
    let mut stmt = match conn.prepare(
        "SELECT last_activity_at, title, cwd, COUNT(*) AS c \
         FROM sessions \
         WHERE user_id = ?1 AND deleted_at IS NULL AND last_activity_at IS NOT NULL \
         GROUP BY last_activity_at, title, cwd \
         HAVING c > 1 \
         ORDER BY c DESC, last_activity_at ASC",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let rows = stmt
        .query_map([uid], |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .ok();

    let mut groups: Vec<DupGroup> = Vec::new();
    if let Some(iter) = rows {
        for r in iter.flatten() {
            let (last_activity_at, title, cwd, _count) = r;
            // 取该组全部 id（含保留与待删）
            let mut stmt2 = match conn.prepare(
                "SELECT id FROM sessions \
                 WHERE user_id = ?1 AND deleted_at IS NULL AND last_activity_at = ?2 \
                   AND title IS ?3 AND cwd = ?4 \
                 ORDER BY id ASC",
            ) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let ids: Vec<String> = stmt2
                .query_map(rusqlite::params![uid, last_activity_at, title, cwd], |row| {
                    row.get::<_, String>(0)
                })
                .ok()
                .map(|iter| iter.flatten().collect())
                .unwrap_or_default();

            if ids.len() <= 1 {
                continue;
            }
            let keep_id = ids[0].clone();
            let dup_ids = ids[1..].to_vec();
            groups.push(DupGroup {
                last_activity_at,
                title,
                cwd,
                keep_id,
                dup_ids,
            });
        }
    }
    groups
}
