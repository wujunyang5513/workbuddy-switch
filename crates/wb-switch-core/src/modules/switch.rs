//! 账号切换：备份 → 关进程 → 复制会话（可选）→ 写认证 → 启动。
//!
//! 对照 server.py `switch_account`。切换过程中通过进度回调向前端推送实时进度，
//! 避免界面长时间无反馈被误认为卡死。core 不依赖 Tauri，进度回调由宿主适配
//! （桌面端转发为 `switch-progress` 事件，HTTP 端写入轮询/SSE）。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::modules::account;
use crate::modules::auth_file;
use crate::modules::process::{close_workbuddy, launch_workbuddy};
use crate::modules::session;

/// 切换进度回调（宿主注入，如 Tauri `app.emit` 或 HTTP 进度缓存）。
pub type ProgressFn = Box<dyn Fn(&str) + Send + Sync>;

/// 迁移脚本路径解析：
/// 1. 环境变量 WB_SWITCH_MIGRATE_SCRIPT 显式指定
/// 2. 默认用户目录 workbuddy-account-migrate 下的 migrate.py
fn migrate_script_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("WB_SWITCH_MIGRATE_SCRIPT") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let home = crate::modules::config::home_dir();
    let candidates = [
        home.join("workbuddy-account-migrate/workbuddy-account-migrate/scripts/migrate.py"),
        home.join("workbuddy-account-migrate/scripts/migrate.py"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

/// Python 可执行文件解析：WB_SWITCH_PYTHON 环境变量，否则 "python"。
fn python_exe() -> String {
    std::env::var("WB_SWITCH_PYTHON").unwrap_or_else(|_| "python".to_string())
}

/// 通过 migrate.py 脚本执行完整迁移（会话 + memory + connectors）。
///
/// 复用脚本的成熟逻辑：备份 → WAL checkpoint → UPDATE 改归属 → 验证 →
/// 输出回滚标签。脚本以「源账号整体迁移」为语义（migrate --source S --target T），
/// 与客户端「勾选部分会话」不同——按用户要求完全复用脚本，切换时迁移源账号全部会话。
fn migrate_via_script(source_uid: &str, target_uid: &str) -> Result<Value, String> {
    let script = migrate_script_path()
        .ok_or_else(|| "未找到 migrate.py，请设置环境变量 WB_SWITCH_MIGRATE_SCRIPT".to_string())?;

    let mut cmd = Command::new(python_exe());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.arg(&script)
        .arg("--source")
        .arg(source_uid)
        .arg("--target")
        .arg(target_uid)
        .arg("--yes")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("启动 migrate.py 失败: {e}"))?;
    let stdout = child.stdout.take().ok_or("无法读取脚本输出")?;
    let stderr = child.stderr.take().ok_or("无法读取脚本错误")?;

    // 读取完整输出（脚本输出量不大，直接读到 EOF）
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut status = None;
    while status.is_none() {
        match child.try_wait() {
            Ok(Some(s)) => status = Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err("migrate.py 执行超时（120s）".to_string());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(format!("等待 migrate.py 失败: {e}")),
        }
    }
    let status = status.expect("status checked");
    let mut out = String::new();
    let mut err = String::new();
    std::io::Read::read_to_string(&mut std::io::BufReader::new(stdout), &mut out)
        .map_err(|e| format!("读取脚本输出失败: {e}"))?;
    std::io::Read::read_to_string(&mut std::io::BufReader::new(stderr), &mut err)
        .map_err(|e| format!("读取脚本错误失败: {e}"))?;

    if !status.success() {
        return Err(format!(
            "migrate.py 执行失败 (exit={}):\n{}",
            status.code().unwrap_or(-1),
            if err.trim().is_empty() { &out } else { &err }
        ));
    }

    // 从输出中提取回滚标签
    let backup_tag = out
        .lines()
        .find(|l| l.contains("备份标签"))
        .and_then(|l| l.rsplit(':').next())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    Ok(json!({
        "ok": true,
        "via": "migrate.py",
        "script": script.to_string_lossy().to_string(),
        "backupTag": backup_tag,
        "output": out.lines().filter(|l| !l.trim().is_empty()).take(30).collect::<Vec<_>>(),
    }))
}

/// 切换账号。
///   - `restart=true` 时关进程后做会话处理（数据库在运行中不宜写入）
///   - `migrate_session_ids` 与 `copy_session_ids` 二选一：
///       · `migrate_session_ids` 非空 → 走路径 A（UPDATE 改归属，不会产生重复）
///       · `copy_session_ids`    非空 → 走路径 B（INSERT 新 id，云端归属目标）
///       · 两者都非空时，优先 migrate（向「不产生重复」靠拢）
///   - `share_sessions=true` → 旧的「全体转移」兼容路径（默认关闭）
pub fn switch_account(
    progress_fn: Option<&ProgressFn>,
    account_id: &str,
    restart: bool,
    share_sessions: bool,
    copy_session_ids: &[String],
    migrate_session_ids: &[String],
) -> Result<Value, String> {
    let progress = |message: &str| {
        eprintln!("[switch] progress: {message}");
        if let Some(p) = progress_fn {
            p(message);
        }
    };

    progress("开始切换账号…");
    let acc =
        account::find_account(account_id).ok_or_else(|| format!("账号不存在: {account_id}"))?;
    let backup = auth_file::backup_auth_file();

    let mut migrate_report: Option<Value> = None;
    let mut copy_report: Option<Value> = None;
    let mut session_report: Option<Value> = None;
    if restart {
        progress("正在关闭 WorkBuddy…");
        close_workbuddy(20)?;
        // 只有重启场景才做会话操作（数据库在运行中不宜写入）
        if !migrate_session_ids.is_empty() {
            progress("正在调用 migrate.py 迁移会话到目标账号…");
            // 复用 migrate.py 脚本：源=当前认证账号（current_user_uid），目标=目标账号
            let src = session::current_user_uid().unwrap_or_default();
            let tgt = acc
                .get("uid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if src.is_empty() || tgt.is_empty() {
                migrate_report = Some(json!({"ok": false, "error": "无法确定源/目标 uid"}));
            } else if src == tgt {
                migrate_report = Some(json!({"ok": false, "error": "源目标账号相同"}));
            } else {
                migrate_report = migrate_via_script(&src, &tgt).ok();
            }
        } else if !copy_session_ids.is_empty() {
            progress("正在复制会话到目标账号（INSERT 新 id）…");
            copy_report = session::copy_sessions_for_switch(&acc, copy_session_ids);
        }
        if share_sessions {
            // 旧的「全体转移」兼容路径（默认关闭），Rust 版暂未实现
            session_report = Some(json!({"error": "share_sessions 兼容路径暂未在 Rust 版实现"}));
        }
    }
    progress("正在写入认证文件…");
    auth_file::write_account_to_auth_file(&acc)?;
    if restart {
        progress("正在启动 WorkBuddy…");
        launch_workbuddy()?;
    }
    progress("切换完成");

    let mut result = json!({
        "ok": true,
        "account": account::account_display_name(&acc),
        "backup": backup.map(|p| p.to_string_lossy().to_string()),
    });
    if let Some(m) = migrate_report {
        result["sessionMigrate"] = m;
    }
    if let Some(c) = copy_report {
        result["sessionCopy"] = c;
    }
    if let Some(s) = session_report {
        result["sessionShare"] = s;
    }
    Ok(result)
}
