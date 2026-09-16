//! HTTP API 层：把 buddy2api-core 暴露为本地 REST 接口，供 webui（浏览器）调用。
//!
//! 路由设计对应 Python 版 server.py 与桌面端 commands.rs。仅绑定 127.0.0.1，
//! token 不出本机。

use std::sync::Mutex;
#[cfg(target_os = "windows")]
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::RawQuery;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde_json::{json, Value};

use buddy2api_core::modules::{
    account, auth_file, checkin, codebuddy_cli, codebuddy_cn_ide, codebuddy_ide, config,
    credit_usage, credits, export_import, oauth, openai_proxy, process, refresh, rotate, session,
    switch, token_stats, travel, update, variant::WbVariant,
};

/// WorkBuddy 运行状态缓存：Windows 上检测要跑 tasklist（慢），缓存几秒避免
/// 前端切 tab 频繁触发命令行导致卡顿/闪窗。按档位分别缓存。
#[cfg(target_os = "windows")]
static RUNNING_CACHE: Mutex<Option<(Instant, bool, WbVariant)>> = Mutex::new(None);

fn cached_workbuddy_running(variant: WbVariant) -> bool {
    #[cfg(target_os = "windows")]
    {
        let mut cache = RUNNING_CACHE.lock().unwrap();
        if let Some((t, v, cached_variant)) = cache.as_ref() {
            if *cached_variant == variant && t.elapsed() < Duration::from_secs(3) {
                return *v;
            }
        }
        let v = process::is_workbuddy_running(variant);
        *cache = Some((Instant::now(), v, variant));
        v
    }
    #[cfg(not(target_os = "windows"))]
    {
        process::is_workbuddy_running(variant)
    }
}

#[derive(RustEmbed)]
#[folder = "../../dist/"]
struct Assets;

/// 切换进度缓存：webui 通过 GET /api/switch/progress 轮询。
static SWITCH_PROGRESS: Mutex<Option<String>> = Mutex::new(None);
static SWITCH_RUNNING: Mutex<bool> = Mutex::new(false);

pub fn router() -> Router {
    Router::new()
        .route("/api/status", get(api_status))
        .route("/api/accounts", get(api_accounts))
        .route("/api/codebuddy-cli/status", get(api_codebuddy_cli_status))
        .route(
            "/api/codebuddy-cli/install-helper",
            post(api_codebuddy_cli_install_helper),
        )
        .route("/api/codebuddy-cli/switch", post(api_codebuddy_cli_switch))
        .route(
            "/api/codebuddy-cn-ide/status",
            get(api_codebuddy_cn_ide_status),
        )
        .route(
            "/api/codebuddy-cn-ide/switch",
            post(api_codebuddy_cn_ide_switch),
        )
        .route(
            "/api/codebuddy-cn-ide/detect",
            post(api_codebuddy_cn_ide_detect),
        )
        .route("/api/codebuddy-ide/status", get(api_codebuddy_ide_status))
        .route("/api/codebuddy-ide/switch", post(api_codebuddy_ide_switch))
        .route("/api/codebuddy-ide/detect", post(api_codebuddy_ide_detect))
        .route("/api/delete", post(api_delete))
        .route("/api/oauth/start", post(api_oauth_start))
        .route("/api/oauth/status", post(api_oauth_status))
        .route("/api/import-local", post(api_import_local))
        .route("/api/export-accounts", post(api_export_accounts))
        .route(
            "/api/export-accounts-to-path",
            post(api_export_accounts_to_path),
        )
        .route("/api/import/preview", post(api_preview_import))
        .route("/api/import", post(api_import))
        .route("/api/switch", post(api_switch))
        .route("/api/switch/progress", get(api_switch_progress))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/copy", post(api_copy_sessions))
        .route("/api/checkin/status", get(api_checkin_status))
        .route("/api/credits", post(api_credits))
        .route("/api/credits/stats", get(api_credit_statistics))
        .route("/api/token-stats", get(api_token_statistics))
        .route("/api/checkin", post(api_checkin))
        .route("/api/checkin/all", post(api_checkin_all))
        .route(
            "/api/checkin/config",
            get(api_checkin_config).post(api_save_checkin_config),
        )
        .route("/api/checkin/logs", get(api_checkin_logs))
        .route("/api/travel/status", get(api_travel_status))
        .route(
            "/api/travel/config",
            get(api_travel_config).post(api_save_travel_config),
        )
        .route(
            "/api/rotate/config",
            get(api_rotate_config).post(api_save_rotate_config),
        )
        .route("/api/rotate/status", get(api_rotate_status))
        .route("/api/rotate/run", post(api_rotate_run))
        .route("/api/rotate/logs", get(api_rotate_logs))
        .route("/api/refresh-token", post(api_refresh_token))
        .route("/api/update/check", get(api_update_check))
        .route(
            "/api/update/config",
            get(api_update_config).post(api_save_update_config),
        )
        // 2API：本地 OpenAI 兼容代理（仅本机、免鉴权）。
        .route("/v1/chat/completions", post(proxy_chat_completions))
        .route("/v1/models", get(proxy_models))
        .route(
            "/api/proxy/config",
            get(api_proxy_config).post(api_save_proxy_config),
        )
        .fallback(static_handler)
}

fn json_ok(v: Value) -> Response {
    Json(v).into_response()
}

fn json_err(e: String, code: StatusCode) -> Response {
    (code, Json(json!({ "ok": false, "error": e }))).into_response()
}

/// 从 query string 解析档位（缺省国内版）。与 Tauri 命令的可选 `variant` 参数同义。
fn query_variant(query: Option<&str>) -> WbVariant {
    let raw = query.unwrap_or("").split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == "variant").then_some(value)
    });
    WbVariant::parse(raw)
}

/// 从请求体解析档位（缺省国内版）。与 Tauri 命令的可选 `variant` 参数同义。
fn body_variant(body: &Value) -> WbVariant {
    WbVariant::parse(body.get("variant").and_then(Value::as_str))
}

// ---------------------------------------------------------------------------
// 状态 / 账号
// ---------------------------------------------------------------------------

async fn api_status(RawQuery(query): RawQuery) -> Response {
    let variant = query_variant(query.as_deref());
    let auth = auth_file::read_auth_file(variant);
    let current = auth.as_ref().and_then(|a| {
        let acct = a.get("account").cloned().unwrap_or_else(|| json!({}));
        Some(json!({
            "uid": acct.get("uid"),
            "nickname": acct.get("nickname"),
            "email": acct.get("email"),
            // 同一手机号在不同组织下 uid 相同，前端要靠组织标识才能认出是哪一条账号
            "enterpriseId": account::identity_enterprise_id(&acct),
            "enterpriseName": acct.get("enterpriseName"),
            "orgKey": account::identity_org_key(&acct),
        }))
    });
    json_ok(json!({
        "running": cached_workbuddy_running(variant),
        "authFile": auth_file::auth_file_path(variant).to_string_lossy(),
        "current": current,
        "appPath": auth_file::workbuddy_app_path(variant).to_string_lossy(),
        "version": update::APP_VERSION,
        "variant": variant.as_str(),
    }))
}

/// GET /api/accounts —— 返回全部档位的账号，`current` 取请求档位的登录态。
async fn api_accounts(RawQuery(query): RawQuery) -> Response {
    let variant = query_variant(query.as_deref());
    json_ok(json!({
        "accounts": account::load_accounts()
            .iter()
            .map(account::account_meta)
            .collect::<Vec<_>>(),
        "current": auth_file::read_auth_file(variant)
            .and_then(|a| a.get("account").and_then(|x| x.get("uid")).and_then(|x| x.as_str()).map(String::from)),
        "variant": variant.as_str(),
    }))
}

async fn api_codebuddy_cli_status() -> Response {
    json_ok(codebuddy_cli::status())
}

async fn api_codebuddy_cli_install_helper() -> Response {
    match codebuddy_cli::install_helper() {
        Ok(result) => json_ok(result),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_cli_switch(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let close_running_cli = body
        .get("closeRunningCli")
        .or_else(|| body.get("close_running_cli"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match codebuddy_cli::switch_active_account(id, close_running_cli) {
        Ok(result) => json_ok(result),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_cn_ide_status() -> Response {
    json_ok(codebuddy_cn_ide::status())
}

async fn api_codebuddy_cn_ide_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .or_else(|| body.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let restart = body
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    match codebuddy_cn_ide::switch_account(account_id, restart) {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_cn_ide_detect() -> Response {
    match codebuddy_cn_ide::detect_current_account() {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_ide_status() -> Response {
    json_ok(codebuddy_ide::status())
}

async fn api_codebuddy_ide_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .or_else(|| body.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let restart = body
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    match codebuddy_ide::switch_account(account_id, restart) {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_ide_detect() -> Response {
    match codebuddy_ide::detect_current_account() {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_delete(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    match account::delete_account(id) {
        Ok(()) => json_ok(json!({ "ok": true })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/import-local —— 导入本机当前账号（body 可选 `variant`，缺省国内版）。
///
/// body 允许缺失，保持改造前的调用方式可用。
async fn api_import_local(body: Option<Json<Value>>) -> Response {
    let variant = body
        .as_ref()
        .map(|Json(value)| body_variant(value))
        .unwrap_or_else(|| WbVariant::parse(None));
    match account::import_local(variant) {
        Ok(acc) => json_ok(json!({ "ok": true, "account": acc })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 导出 / 导入账号
// ---------------------------------------------------------------------------

async fn api_export_accounts(Json(body): Json<Value>) -> Response {
    let ids: Vec<String> = body
        .get("accountIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    match export_import::export_accounts(&ids) {
        Ok(records) => json_ok(json!({ "ok": true, "accounts": records })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_export_accounts_to_path(Json(body): Json<Value>) -> Response {
    let ids: Vec<String> = body
        .get("accountIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match export_import::export_accounts_to_path(&ids, &path) {
        Ok(path) => json_ok(json!({ "ok": true, "path": path })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_preview_import(Json(body): Json<Value>) -> Response {
    let text = body
        .get("fileText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match export_import::preview_accounts(&text) {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_import(Json(body): Json<Value>) -> Response {
    let text = body
        .get("fileText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let indexes: Vec<usize> = body
        .get("indexes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|n| n as usize))
                .collect()
        })
        .unwrap_or_default();
    match export_import::import_accounts(&text, &indexes) {
        Ok(result) => json_ok(json!({
            "ok": true,
            "imported": result.imported,
            "skipped": result.skipped,
            "overwritten": result.overwritten,
        })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// OAuth 登录
// ---------------------------------------------------------------------------

/// POST /api/oauth/start —— 发起扫码登录（body 可选 `variant`，缺省国内版）。
async fn api_oauth_start(body: Option<Json<Value>>) -> Response {
    let variant = body
        .as_ref()
        .map(|Json(value)| body_variant(value))
        .unwrap_or_else(|| WbVariant::parse(None));
    match oauth::oauth_start(variant).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/oauth/status —— 轮询采集结果（档位取发起时记录，无需传参）。
async fn api_oauth_status(Json(body): Json<Value>) -> Response {
    let login_id = body
        .get("loginId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    json_ok(oauth::oauth_poll(&login_id).await)
}

// ---------------------------------------------------------------------------
// 切换
// ---------------------------------------------------------------------------

async fn api_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if account_id.trim().is_empty() {
        return json_err("缺少 accountId".to_string(), StatusCode::BAD_REQUEST);
    }
    let restart = body
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let share_sessions = body
        .get("shareSessions")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let copy_ids: Vec<String> = body
        .get("copySessionIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    {
        let mut running = SWITCH_RUNNING.lock().unwrap();
        if *running {
            return json_err("已有切换任务进行中".to_string(), StatusCode::CONFLICT);
        }
        *running = true;
        *SWITCH_PROGRESS.lock().unwrap() = Some("开始切换账号…".to_string());
    }

    let progress: switch::ProgressFn = Box::new(|msg| {
        *SWITCH_PROGRESS.lock().unwrap() = Some(msg.to_string());
    });

    let result = tokio::task::spawn_blocking(move || {
        switch::switch_account(
            Some(&progress),
            &account_id,
            restart,
            share_sessions,
            &copy_ids,
        )
    })
    .await;

    *SWITCH_RUNNING.lock().unwrap() = false;

    match result {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_switch_progress() -> Response {
    let p = SWITCH_PROGRESS.lock().unwrap().clone();
    let running = *SWITCH_RUNNING.lock().unwrap();
    json_ok(json!({ "running": running, "progress": p }))
}

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

/// GET /api/sessions —— 当前账号的会话列表（query 可选 `variant`，缺省国内版）。
async fn api_sessions(RawQuery(query): RawQuery) -> Response {
    let variant = query_variant(query.as_deref());
    match session::current_user_uid(variant) {
        Some(uid) => json_ok(json!({
            "sessions": session::list_sessions_for_user(variant, &uid),
            "current": uid,
            "variant": variant.as_str(),
        })),
        None => json_ok(json!({ "sessions": [], "current": null, "variant": variant.as_str() })),
    }
}

async fn api_copy_sessions(Json(body): Json<Value>) -> Response {
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let session_ids: Vec<String> = body
        .get("sessionIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let Some(target) = account::find_account(&target_account_id) else {
        return json_err("目标账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    // 档位取目标账号自身（源 uid 也从该档位的登录态读）。
    let variant = account::variant_of(&target);
    let source_uid = session::current_user_uid(variant);
    // 能力不满足时返回明确错误而不是空对象。
    let copied = match session::copy_sessions_for_switch(&target, &session_ids) {
        Ok(report) => report,
        Err(error) => {
            return json_err(error, StatusCode::BAD_REQUEST);
        }
    };
    json_ok(json!({
        "sourceUid": source_uid,
        "targetUid": target.get("uid"),
        "copied": copied,
        "variant": variant.as_str(),
    }))
}

// ---------------------------------------------------------------------------
// 签到 / 保活
// ---------------------------------------------------------------------------

/// GET /api/checkin/status —— 全部账号的签到状态（每行带 `variant`，档位取账号自身）。
async fn api_checkin_status() -> Response {
    let list = account::load_accounts();
    let mut items = Vec::new();
    for acc in &list {
        let status = checkin::get_checkin_status(acc).await;
        items.push(checkin_status_item(acc, status));
    }
    json_ok(json!({ "accounts": items }))
}

fn checkin_status_item(account: &Value, mut status: Value) -> Value {
    status["accountId"] = account.get("id").cloned().unwrap_or(Value::Null);
    status["email"] = json!(account::account_display_name(account));
    status["variant"] = json!(account::variant_of(account).as_str());
    status
}

async fn api_credits(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(credits::get_credit_expiry(&acc).await)
}

fn query_flag_enabled(query: Option<&str>, name: &str) -> bool {
    query.unwrap_or("").split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, "true"));
        key == name && matches!(value, "" | "1" | "true" | "yes")
    })
}

async fn api_credit_statistics(RawQuery(query): RawQuery) -> Response {
    json_ok(credit_usage::get_statistics(query_flag_enabled(query.as_deref(), "refresh")).await)
}

async fn api_token_statistics(RawQuery(query): RawQuery) -> Response {
    let days = query.as_deref().and_then(|value| {
        value
            .split('&')
            .find_map(|part| part.strip_prefix("days=")?.parse::<i64>().ok())
    });
    match tokio::task::spawn_blocking(move || token_stats::get_statistics(days)).await {
        Ok(statistics) => json_ok(statistics),
        Err(error) => json_err(
            format!("扫描 Token 统计失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

async fn api_checkin(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(checkin::checkin_account(&acc).await)
}

async fn api_checkin_all(body: Option<Json<Value>>) -> Response {
    // 缺省（无 body / 无 variant）= 全部档位，保持与桌面端 set 前的行为一致。
    let variant = body
        .as_ref()
        .and_then(|Json(value)| value.get("variant"))
        .and_then(|value| value.as_str())
        .map(|raw| WbVariant::parse(Some(raw)));
    json_ok(checkin::run_checkin_all(variant).await)
}

async fn api_checkin_config() -> Response {
    json_ok(config::load_checkin_config())
}

async fn api_save_checkin_config(Json(body): Json<Value>) -> Response {
    let submitted = body.get("config").unwrap_or(&body);
    match config::save_checkin_config(submitted) {
        Ok(()) => json_ok(config::load_checkin_config()),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

/// GET /api/checkin/logs —— 签到日志（每行带 `variant`，便于前端按档位过滤）。
async fn api_checkin_logs() -> Response {
    json_ok(json!({ "logs": checkin::load_checkin_logs_with_variant() }))
}

async fn api_travel_status() -> Response {
    travel::reconcile_due_travel(None).await;
    let items = account::load_accounts()
        .iter()
        .map(|acc| {
            let id = acc.get("id").and_then(Value::as_str).unwrap_or("");
            let mut value = travel::travel_display(id);
            value["accountId"] = acc.get("id").cloned().unwrap_or(Value::Null);
            value["email"] = json!(account::account_display_name(acc));
            value
        })
        .collect::<Vec<_>>();
    json_ok(json!({ "accounts": items }))
}

async fn api_travel_config() -> Response {
    json_ok(config::load_travel_config())
}

async fn api_save_travel_config(Json(body): Json<Value>) -> Response {
    let submitted = body.get("config").unwrap_or(&body);
    match config::save_travel_config(submitted) {
        Ok(()) => {
            let saved = config::load_travel_config();
            if saved.get("enabled").and_then(Value::as_bool) == Some(true) {
                tokio::spawn(async {
                    let _ = travel::run_travel_cycle().await;
                    let _ = travel::run_travel_claim_cycle().await;
                });
            }
            json_ok(saved)
        }
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

async fn api_refresh_token(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(refresh::refresh_account_token(acc).await)
}

// ---------------------------------------------------------------------------
// 自动轮换（CodeBuddy CLI）
// ---------------------------------------------------------------------------

async fn api_rotate_config() -> Response {
    json_ok(config::load_auto_rotate_config())
}

async fn api_save_rotate_config(Json(body): Json<Value>) -> Response {
    match config::save_auto_rotate_config(&body) {
        Ok(()) => json_ok(json!({ "ok": true, "config": config::load_auto_rotate_config() })),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

async fn api_rotate_status() -> Response {
    json_ok(rotate::rotate_status())
}

async fn api_rotate_run() -> Response {
    json_ok(rotate::run_rotate_cycle().await)
}

async fn api_rotate_logs() -> Response {
    json_ok(json!({ "logs": rotate::rotate_logs() }))
}

// ---------------------------------------------------------------------------
// 更新
// ---------------------------------------------------------------------------

async fn api_update_check() -> Response {
    json_ok(update::update_check(None, false).await)
}

async fn api_update_config() -> Response {
    json_ok(update::load_github_config())
}

async fn api_save_update_config(Json(body): Json<Value>) -> Response {
    match update::save_github_config(&body) {
        Ok(()) => json_ok(json!({ "ok": true, "config": update::load_github_config() })),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 2API：本地 OpenAI 兼容代理（仅本机、免鉴权）
// ---------------------------------------------------------------------------

/// POST /v1/chat/completions —— 注入当前激活账号凭据后透传到官方 OpenAI 兼容接口。
///
/// 上游本身是标准 OpenAI 协议，不做格式转换；`stream` 语义由请求体决定，
/// 这里一律用流式 body 透传（非流式响应是单块 JSON，也能正常返回）。
async fn proxy_chat_completions(
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !openai_proxy::proxy_enabled() {
        return openai_error("2API 代理未开启", StatusCode::NOT_FOUND, "proxy_disabled");
    }
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    let request = match openai_proxy::parse_model(model) {
        Ok(request) => request,
        Err(error) => return openai_error(&error, StatusCode::BAD_REQUEST, "invalid_request_error"),
    };

    // 会话粘性键：官方客户端发 `X-Conversation-ID`；命中即固定同号以命中上游 cache。
    let session_key = headers
        .get("X-Conversation-ID")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // 路由选号：前缀锁站、默认池跨站；倍率最少优先 + 轮询 + 熔断 + 会话粘性。
    let accounts = account::load_accounts();
    let account = openai_proxy::select_account(
        &accounts,
        request.site,
        session_key.as_deref(),
        config::now_ms(),
    );
    let Some(mut account) = account else {
        return openai_error(
            "没有可用账号：请先在账号页添加账号",
            StatusCode::BAD_GATEWAY,
            "no_account",
        );
    };

    // 发请求前惰性刷新：expiresAt 缺失/已过期/临近过期时先刷新，避免首个请求 401。
    account = openai_proxy::ensure_account_fresh(&account).await;

    // 用裸模型名（去掉前缀）替换请求体里的 model 再转发。
    let mut upstream_body = body;
    upstream_body["model"] = json!(request.upstream_model());

    let mut resp = match openai_proxy::send_chat(&account, &upstream_body).await {
        Ok(resp) => resp,
        Err(error) => {
            // 网络/传输错误不计入熔断（可能只是本地网络问题）。
            return openai_error(
                &format!("上游请求失败: {error}"),
                StatusCode::BAD_GATEWAY,
                "upstream_error",
            );
        }
    };

    // 上游仍返回鉴权失败：刷新一次并重试一次（应对服务端提前失效或并发刷新窗口）。
    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        let refreshed = openai_proxy::force_refresh(&account).await;
        if refreshed
            .get("needs_relogin")
            .and_then(Value::as_bool)
            != Some(true)
        {
            account = refreshed;
            match openai_proxy::send_chat(&account, &upstream_body).await {
                Ok(retry) => resp = retry,
                Err(error) => {
                    return openai_error(
                        &format!("上游请求失败: {error}"),
                        StatusCode::BAD_GATEWAY,
                        "upstream_error",
                    )
                }
            }
        }
    }

    // 账号级失败（401/403）计入熔断；成功清零。5xx 与其它不计。
    let final_status = resp.status();
    if final_status.as_u16() == 401 || final_status.as_u16() == 403 {
        openai_proxy::record_result(&account, true, config::now_ms());
    } else if final_status.is_success() {
        openai_proxy::record_result(&account, false, config::now_ms());
    }

    forward_response(resp).await
}

/// GET /v1/models —— 聚合国内站 + 国际站的官方模型列表，带 `cn/`、`intl/` 前缀。
async fn proxy_models() -> Response {
    if !openai_proxy::proxy_enabled() {
        return openai_error("2API 代理未开启", StatusCode::NOT_FOUND, "proxy_disabled");
    }
    let accounts = account::load_accounts();
    let mut entries: Vec<Value> = Vec::new();
    let mut plain_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for site in [WbVariant::Cn, WbVariant::Ai] {
        let Some(account) = openai_proxy::account_for_site(&accounts, site) else {
            continue;
        };
        let Ok(models) = openai_proxy::fetch_site_models(&account).await else {
            // 单站失败只丢弃该站结果，不整体失败。
            continue;
        };
        for entry in openai_proxy::site_model_entries(site, &models) {
            if let Some(id) = entry.get("workbuddy_id").and_then(Value::as_str) {
                // 同时导出无前缀官方 id（指向默认池），两站去重。
                if plain_ids.insert(id.to_string()) {
                    let mut plain = entry.clone();
                    plain["id"] = json!(id);
                    entries.push(plain);
                }
            }
            entries.push(entry);
        }
    }

    json_ok(json!({ "object": "list", "data": entries }))
}

/// 把上游响应（流式或非流式）透传给客户端。
async fn forward_response(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let stream = resp.bytes_stream();
    let body = Body::from_stream(stream);
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(body)
        .unwrap_or_else(|_| {
            openai_error("响应构造失败", StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        })
}

/// 标准 OpenAI 错误体。
fn openai_error(message: &str, status: StatusCode, kind: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": kind,
            }
        })),
    )
        .into_response()
}

/// GET /api/proxy/config —— 2API 状态与配置。
async fn api_proxy_config() -> Response {
    let config = openai_proxy::load_proxy_config();
    let account = openai_proxy::active_account();
    json_ok(json!({
        "enabled": openai_proxy::proxy_enabled(),
        "baseUrl": "/v1",
        "config": config,
        "activeAccount": account.as_ref().map(account::account_meta),
    }))
}

async fn api_save_proxy_config(Json(body): Json<Value>) -> Response {
    match openai_proxy::save_proxy_config(&body) {
        Ok(()) => json_ok(json!({ "ok": true, "config": openai_proxy::load_proxy_config() })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 静态前端
// ---------------------------------------------------------------------------

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".js") || path.ends_with(".mjs") {
        "text/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}

async fn static_handler(uri: Uri) -> Response {
    let mut path = uri.path().trim_start_matches('/').to_string();
    if path.is_empty() || path == "index.html" {
        path = "index.html".to_string();
    }
    // 前端路由回退到 index.html
    let data = Assets::get(&path).or_else(|| Assets::get("index.html"));
    match data {
        Some(f) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type(&path))
            .body(Body::from(f.data.into_owned()))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::{body_variant, checkin_status_item, query_variant};
    use serde_json::json;
    use buddy2api_core::modules::variant::WbVariant;

    /// 缺省档位必须与改造前一致（不传 variant 即国内版）。
    #[test]
    fn variant_query_defaults_to_cn() {
        assert_eq!(query_variant(None), WbVariant::Cn);
        assert_eq!(query_variant(Some("")), WbVariant::Cn);
        assert_eq!(query_variant(Some("refresh=true")), WbVariant::Cn);
        assert_eq!(
            query_variant(Some("refresh=true&variant=cn")),
            WbVariant::Cn
        );
        assert_eq!(query_variant(Some("variant=ai")), WbVariant::Ai);
        assert_eq!(
            query_variant(Some("variant=ai&refresh=true")),
            WbVariant::Ai
        );
        assert_eq!(query_variant(Some("variant=unknown")), WbVariant::Cn);
    }

    #[test]
    fn variant_body_defaults_to_cn() {
        assert_eq!(body_variant(&json!({})), WbVariant::Cn);
        assert_eq!(body_variant(&json!({"variant": null})), WbVariant::Cn);
        assert_eq!(body_variant(&json!({"variant": "ai"})), WbVariant::Ai);
        assert_eq!(body_variant(&json!({"accountId": "x"})), WbVariant::Cn);
    }

    #[test]
    fn web_checkin_status_keeps_account_identity() {
        let item = checkin_status_item(
            &json!({"id": "account-1", "email": "user@example.com"}),
            json!({"ok": true, "todayCheckedIn": true}),
        );

        assert_eq!(item["accountId"], "account-1");
        assert_eq!(item["email"], "user@example.com");
        assert_eq!(item["todayCheckedIn"], true);
        assert_eq!(item["variant"], "cn");
    }

    #[test]
    fn web_checkin_status_preserves_failure_state() {
        let item = checkin_status_item(
            &json!({"id": "account-2"}),
            json!({"ok": false, "todayCheckedIn": false, "error": "status failed"}),
        );

        assert_eq!(item["accountId"], "account-2");
        assert_eq!(item["ok"], false);
        assert_eq!(item["error"], "status failed");
    }

    #[test]
    fn web_checkin_status_row_carries_variant() {
        let item = checkin_status_item(
            &json!({"id": "ai-1", "variant": "ai"}),
            json!({"ok": false, "statusUnsupported": true}),
        );

        assert_eq!(item["variant"], "ai");
        assert_eq!(item["statusUnsupported"], true);
    }
}
