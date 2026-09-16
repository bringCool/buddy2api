//! 2API：本地 OpenAI 兼容代理。
//!
//! 对外暴露 `/v1/chat/completions` 与 `/v1/models`，用账号库中的账号（默认当前
//! 激活账号，路由开启时按策略选号）注入鉴权头后透传到 WorkBuddy 官方 OpenAI
//! 兼容接口。上游本身就是标准 OpenAI 协议（实测 `workbuddy.har`），因此
//! **不做任何格式转换**，只做「选账号 + 注入头 + 双向流透传」。
//!
//! 域已统一：国内 `https://www.workbuddy.cn`、国际 `https://www.workbuddy.ai`
//! （见 `WbVariant::api_endpoint`）。聊天基址为 `{api_endpoint}/v2`。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::modules::account;
use crate::modules::account::get_str;
use crate::modules::config::{atomic_write, home_dir, http_client_streaming, now_ms, store_dir};
use crate::modules::variant::WbVariant;

/// 聊天接口默认池前缀：`cn/` 锁国内站，`intl/` 锁国际站，无前缀走默认池。
pub const PREFIX_CN: &str = "cn/";
pub const PREFIX_INTL: &str = "intl/";

/// 熔断：连续失败阈值与冷却时长。
const BREAKER_FAILURE_THRESHOLD: u32 = 3;
const BREAKER_COOLDOWN_MS: i64 = 5 * 60 * 1000;
/// 会话粘性 TTL。
const SESSION_TTL_MS: i64 = 30 * 60 * 1000;

/// 解析后的模型请求：`site` 为 `None` 表示默认池（跨站调度）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequest {
    pub site: Option<WbVariant>,
    pub model: String,
}

impl ModelRequest {
    /// 带回完整模型 id（用于上游请求体）。
    pub fn upstream_model(&self) -> &str {
        &self.model
    }
}

/// 解析客户端传入的模型 id。
///
/// - `cn/<model>` → 锁国内站
/// - `intl/<model>` → 锁国际站
/// - `<model>` → 默认池（`site = None`）
///
/// 未知前缀（含 `/` 但不是 `cn/` / `intl/`）返回 `Err`，由调用方按非法模型处理，
/// 避免把 `foo/bar` 之类误当默认池或拼出错误上游。
pub fn parse_model(raw: &str) -> Result<ModelRequest, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("model 不能为空".to_string());
    }
    if let Some(rest) = raw.strip_prefix(PREFIX_CN) {
        let model = rest.trim();
        if model.is_empty() {
            return Err("cn/ 后缺少模型名".to_string());
        }
        return Ok(ModelRequest {
            site: Some(WbVariant::Cn),
            model: model.to_string(),
        });
    }
    if let Some(rest) = raw.strip_prefix(PREFIX_INTL) {
        let model = rest.trim();
        if model.is_empty() {
            return Err("intl/ 后缺少模型名".to_string());
        }
        return Ok(ModelRequest {
            site: Some(WbVariant::Ai),
            model: model.to_string(),
        });
    }
    // 带斜杠但不是已知前缀：判定为非法，避免歧义。
    if raw.contains('/') {
        return Err(format!("未知模型前缀: {raw}"));
    }
    Ok(ModelRequest {
        site: None,
        model: raw.to_string(),
    })
}

/// 解析官方 `/v3/config` 的 `credits` 倍率字段。
///
/// 实测格式不统一：`"x0.29"`、`"x0.00"`、`"x3.31 credits"`、`""`、字段缺失。
/// 返回 `Some(f64)` 表示倍率；`None` 表示「无倍率」（缺失/空/解析失败），
/// 由路由按约定排最后处理。
pub fn parse_credits(raw: Option<&Value>) -> Option<f64> {
    let text = raw?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    // 去掉开头 `x`/`X` 与结尾 `credits` 等说明，只取数字。
    let trimmed = text
        .trim_start_matches(['x', 'X'])
        .trim_end_matches("credits")
        .trim();
    trimmed.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// 聊天上游 URL（含 `/v2`）。
pub fn chat_url(variant: WbVariant) -> String {
    format!("{}/v2/chat/completions", variant.api_endpoint())
}

/// 模型配置 URL（`/v3/config`）。
pub fn models_config_url(variant: WbVariant) -> String {
    format!("{}/v3/config", variant.api_endpoint())
}

/// 构造透传上游所需的鉴权头。
///
/// 注入/覆盖：`Authorization` / `X-Domain` / `X-User-Id` / `X-Product: SaaS`。
/// 以官方客户端请求指纹为准（见 `workbuddy.har`）。缺 uid/domain 时跳过对应头。
pub fn upstream_headers(account: &Value) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    if let Some(token) = get_str(account, "access_token") {
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    }
    headers.insert("X-Product".to_string(), "SaaS".to_string());
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    if let Some(uid) = get_str(account, "uid") {
        headers.insert("X-User-Id".to_string(), uid);
    }
    if let Some(domain) = get_str(account, "domain") {
        headers.insert("X-Domain".to_string(), domain);
    }
    if let Some(eid) = get_str(account, "enterpriseId").or_else(|| get_str(account, "enterprise_id"))
    {
        headers.insert("X-Enterprise-Id".to_string(), eid.clone());
        headers.insert("X-Tenant-Id".to_string(), eid);
    }
    headers
}

/// 账号是否有可用于代理的 token。
fn has_token(account: &Value) -> bool {
    get_str(account, "access_token").is_some()
}

// ---------------------------------------------------------------------------
// 账号选择：站点过滤 + 熔断 + 会话粘性 + 倍率最少 + 轮询
// ---------------------------------------------------------------------------

/// 熔断状态（按账号 id）。
#[derive(Default, Clone)]
struct BreakerState {
    failures: u32,
    cooldown_until: i64,
}

#[derive(Default)]
struct RouterState {
    /// round-robin 游标，按站点分组。
    cursor: HashMap<&'static str, usize>,
    /// 熔断状态，按账号 id。
    breakers: HashMap<String, BreakerState>,
    /// 会话粘性：session_key → (account_id, expire_at)。
    sticky: HashMap<String, (String, i64)>,
}

static ROUTER: OnceLock<Mutex<RouterState>> = OnceLock::new();

fn router() -> &'static Mutex<RouterState> {
    ROUTER.get_or_init(|| Mutex::new(RouterState::default()))
}

fn account_key(account: &Value) -> String {
    get_str(account, "id").unwrap_or_default()
}

/// 选择可用账号。
///
/// `site` 锁定站点（前缀请求），`None` 表示默认池（两站候选）。
/// `session_key` 命中粘性时固定同号；否则按站点轮询。冷却中的账号被跳过。
pub fn select_account(
    accounts: &[Value],
    site: Option<WbVariant>,
    session_key: Option<&str>,
    now: i64,
) -> Option<Value> {
    let candidates: Vec<&Value> = accounts
        .iter()
        .filter(|acc| has_token(acc))
        .filter(|acc| match site {
            Some(v) => WbVariant::from_account(acc) == v,
            None => true,
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }

    let mut state = router().lock().unwrap();

    // 会话粘性：命中且账号仍可用则复用。
    if let Some(key) = session_key.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some((account_id, expire_at)) = state.sticky.get(key).cloned() {
            if expire_at > now {
                if let Some(acc) = candidates
                    .iter()
                    .find(|acc| account_key(acc) == account_id)
                    .copied()
                {
                    return Some(acc.clone());
                }
            }
            state.sticky.remove(key);
        }
    }

    // 熔断过滤。
    let healthy: Vec<&Value> = candidates
        .iter()
        .copied()
        .filter(|acc| {
            state
                .breakers
                .get(&account_key(acc))
                .map(|b| b.cooldown_until <= now)
                .unwrap_or(true)
        })
        .collect();
    // 全部熔断时兜底用原候选，避免完全不可用（冷却窗口外重试）。
    let pool = if healthy.is_empty() { candidates } else { healthy };

    // 倍率最少优先：按账号对应档位已知的最低倍率排序（同倍率保持给定顺序）。
    // 倍率通过 `account_min_credit` 读取缓存；无倍率视为不优先。
    let mut indexed: Vec<(usize, &Value)> = pool.into_iter().enumerate().collect();
    indexed.sort_by(|(ia, a), (ib, b)| {
        let ca = account_min_credit(a);
        let cb = account_min_credit(b);
        match (ca, cb) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal).then(ia.cmp(ib)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => ia.cmp(ib),
        }
    });

    // 在最小倍率组内轮询：只取与最小倍率相同的一组做 round-robin。
    let best_credit = indexed.first().and_then(|(_, a)| account_min_credit(a));
    let group: Vec<&Value> = indexed
        .iter()
        .filter(|(_, a)| account_min_credit(a) == best_credit)
        .map(|(_, a)| *a)
        .collect();

    let cursor_key = match site {
        Some(v) => v.as_str(),
        None => "all",
    };
    let cursor = state.cursor.entry(cursor_key).or_insert(0);
    let picked = group[*cursor % group.len()].clone();
    *cursor = cursor.wrapping_add(1);

    // 写入粘性。
    if let Some(key) = session_key.map(str::trim).filter(|s| !s.is_empty()) {
        state
            .sticky
            .insert(key.to_string(), (account_key(&picked), now + SESSION_TTL_MS));
        // 顺带清理过期粘性，避免无限增长。
        state.sticky.retain(|_, (_, expire)| *expire > now);
    }

    Some(picked)
}

/// 记录一次请求结果，用于熔断。
///
/// `account_failure = true` 表示账号级失败（401/额度/风控），才计入熔断；
/// 网络/5xx/超时不算。成功则清零。
pub fn record_result(account: &Value, account_failure: bool, now: i64) {
    let key = account_key(account);
    if key.is_empty() {
        return;
    }
    let mut state = router().lock().unwrap();
    if account_failure {
        let entry = state.breakers.entry(key.clone()).or_default();
        entry.failures += 1;
        if entry.failures >= BREAKER_FAILURE_THRESHOLD {
            entry.cooldown_until = now + BREAKER_COOLDOWN_MS;
            entry.failures = 0;
        }
    } else {
        state.breakers.remove(&key);
    }
}

// ---------------------------------------------------------------------------
// 每个账号的倍率缓存（来自 /v3/config）
// ---------------------------------------------------------------------------

static ACCOUNT_CREDITS: OnceLock<Mutex<HashMap<String, (f64, i64)>>> = OnceLock::new();

fn credits_cache() -> &'static Mutex<HashMap<String, (f64, i64)>> {
    ACCOUNT_CREDITS.get_or_init(|| Mutex::new(HashMap::new()))
}

const CREDITS_TTL_MS: i64 = 5 * 60 * 1000;

/// 该账号档位已知的最低模型倍率（越小越优先）。无缓存或已过期返回 None。
fn account_min_credit(account: &Value) -> Option<f64> {
    let key = account_key(account);
    let now = now_ms();
    let mut cache = credits_cache().lock().unwrap();
    match cache.get(&key) {
        Some((credit, at)) if now - at < CREDITS_TTL_MS => Some(*credit),
        Some(_) => {
            cache.remove(&key);
            None
        }
        None => None,
    }
}

/// 根据 `/v3/config` 的模型列表更新某账号的最低倍率缓存。
pub fn update_account_credits(account: &Value, models: &[Value]) {
    let key = account_key(account);
    if key.is_empty() {
        return;
    }
    let min = models
        .iter()
        .filter_map(|m| parse_credits(m.get("credits")))
        .fold(None::<f64>, |acc, v| Some(acc.map_or(v, |a| a.min(v))));
    let mut cache = credits_cache().lock().unwrap();
    if let Some(min) = min {
        cache.insert(key, (min, now_ms()));
    } else {
        cache.remove(&key);
    }
}

// ---------------------------------------------------------------------------
// 配置（~/.wb-switch/openai_proxy.json）
// ---------------------------------------------------------------------------

fn proxy_config_path() -> std::path::PathBuf {
    store_dir().join("openai_proxy.json")
}

/// 默认配置：默认开启（本机免鉴权代理；`enabled=false` 时 `/v1/*` 返回 404）。
pub fn default_proxy_config() -> Value {
    json!({ "enabled": true })
}

/// 读取 2API 配置（缺失/损坏时返回默认值）。
pub fn load_proxy_config() -> Value {
    let mut merged = default_proxy_config();
    if let Ok(text) = std::fs::read_to_string(proxy_config_path()) {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) {
            if let Some(enabled) = map.get("enabled").and_then(Value::as_bool) {
                merged["enabled"] = json!(enabled);
            }
        }
    }
    merged
}

/// 保存 2API 配置（只保留已知字段）。
pub fn save_proxy_config(cfg: &Value) -> Result<(), String> {
    let mut merged = default_proxy_config();
    if let Some(enabled) = cfg.get("enabled").and_then(Value::as_bool) {
        merged["enabled"] = json!(enabled);
    }
    std::fs::create_dir_all(store_dir()).map_err(|e| e.to_string())?;
    let content = serde_json::to_string_pretty(&merged).map_err(|e| e.to_string())?;
    atomic_write(&proxy_config_path(), &content).map_err(|e| e.to_string())
}

/// 代理是否开启。
pub fn proxy_enabled() -> bool {
    load_proxy_config()
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

// ---------------------------------------------------------------------------
// 账号解析
// ---------------------------------------------------------------------------

/// 当前激活账号（CodeBuddy CLI rotate state），找不到时回落账号库第一条。
///
/// 读取 `~/.codebuddy-rotate/state.json` 的 `activeAccountId`；与 CLI 当前账号一致。
pub fn active_account() -> Option<Value> {
    let accounts = account::load_accounts();
    active_account_from(&accounts)
}

fn active_account_from(accounts: &[Value]) -> Option<Value> {
    if accounts.is_empty() {
        return None;
    }
    let state_path = home_dir().join(".codebuddy-rotate").join("state.json");
    let state = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}));

    let active_id = state
        .get("activeAccountId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(id) = active_id {
        if let Some(found) = accounts
            .iter()
            .find(|account| account.get("id").and_then(Value::as_str) == Some(id))
            .or_else(|| {
                accounts
                    .iter()
                    .find(|account| account.get("uid").and_then(Value::as_str) == Some(id))
            })
        {
            return Some(found.clone());
        }
    }
    accounts.first().cloned()
}

/// 选定某档位下可用于代理的账号（当前激活账号若同档位优先，否则该档位第一条）。
pub fn account_for_site(accounts: &[Value], site: WbVariant) -> Option<Value> {
    let active = active_account_from(accounts);
    if let Some(acc) = active {
        if WbVariant::from_account(&acc) == site && has_token(&acc) {
            return Some(acc);
        }
    }
    accounts
        .iter()
        .find(|acc| WbVariant::from_account(acc) == site && has_token(acc))
        .cloned()
}

// ---------------------------------------------------------------------------
// 上游请求
// ---------------------------------------------------------------------------

/// access token 剩余不足该时长即视为「快过期」，代理发请求前主动刷新。
const PRE_REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

/// 账号 token 是否已过期或临近过期（缺 `expiresAt` 视为需要刷新）。
fn token_stale(account: &Value, now: i64) -> bool {
    account
        .get("expiresAt")
        .and_then(Value::as_i64)
        .map(|exp| exp - now < PRE_REFRESH_MARGIN_MS)
        .unwrap_or(true)
}

/// 按账号串行化刷新：刷新会轮换 refresh token，并发刷新会让其中一次失效。
static REFRESH_LOCKS: OnceLock<Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

fn refresh_locks() -> &'static Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>> {
    REFRESH_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn refresh_lock_for(account: &Value) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let key = account_key(account);
    let mut locks = refresh_locks().lock().unwrap();
    locks
        .entry(key)
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// 代理发请求前保证账号 token 新鲜：`expiresAt` 缺失/已过期/临近过期时刷新一次。
///
/// 刷新失败（refresh token 失效等）时原样返回旧账号，由后续上游 401 触发重试或
/// 最终报错，不在这里中断；刷新结果会落盘并返回新账号。
pub async fn ensure_account_fresh(account: &Value) -> Value {
    let expired_or_near = token_stale(account, now_ms());
    let has_rt = get_str(account, "refresh_token").is_some();
    if expired_or_near && has_rt {
        let _guard = refresh_lock_for(account).lock_owned().await;
        // 拿到锁后复核：可能已被并发请求刷新过，账号库里的 token 已是新的。
        if let Some(latest) = account::find_account(&account_key(account)) {
            let still_stale = latest
                .get("expiresAt")
                .and_then(Value::as_i64)
                .map(|exp| exp - now_ms() < PRE_REFRESH_MARGIN_MS)
                .unwrap_or(true);
            if !still_stale {
                return latest;
            }
            return crate::modules::refresh::refresh_account_token(latest).await;
        }
        return crate::modules::refresh::refresh_account_token(account.clone()).await;
    }
    account.clone()
}

/// 无条件刷新账号 token（上游返回 401/403 后重试前调用）。
///
/// 无 refresh token 时原样返回（会带 `needs_relogin`），调用方据此放弃重试。
pub async fn force_refresh(account: &Value) -> Value {
    if get_str(account, "refresh_token").is_none() {
        let mut unchanged = account.clone();
        unchanged["needs_relogin"] = json!(true);
        unchanged["needs_relogin_reason"] = json!("缺少 refresh token，无法刷新，需重新登录");
        return unchanged;
    }
    let _guard = refresh_lock_for(account).lock_owned().await;
    crate::modules::refresh::refresh_account_token(account.clone()).await
}

/// 发送聊天请求，返回上游原始响应（供上层流式/非流式透传）。
pub async fn send_chat(account: &Value, body: &Value) -> Result<reqwest::Response, String> {
    let url = chat_url(WbVariant::from_account(account));
    send_chat_to(&url, account, body).await
}

/// 指定 URL 发送聊天请求（便于用本地 mock 上游做集成测试）。
pub async fn send_chat_to(
    url: &str,
    account: &Value,
    body: &Value,
) -> Result<reqwest::Response, String> {
    let headers = upstream_headers(account);
    let mut req = http_client_streaming().post(url).json(body);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req.send().await.map_err(|e| e.to_string())
}

/// 拉取某站 `/v3/config` 的 `data.models[]`。失败返回 `Err`（上层可跳过该站）。
pub async fn fetch_site_models(account: &Value) -> Result<Vec<Value>, String> {
    let variant = WbVariant::from_account(account);
    let url = models_config_url(variant);
    let headers = upstream_headers(account);
    let mut req = http_client_streaming().get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("上游 {} 返回 {}", url, resp.status()));
    }
    let value: Value = resp.json().await.map_err(|e| e.to_string())?;
    let models = value
        .get("data")
        .and_then(|d| d.get("models"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 顺带更新该账号的倍率缓存（用于路由排序）。
    update_account_credits(account, &models);
    Ok(models)
}

/// 把某站模型列表映射成 OpenAI `/v1/models` 条目，并加上档位前缀。
///
/// 每站导出两份 id：带前缀（`intl/<id>` / `cn/<id>`）与无前缀（默认池）。
/// 无前缀可能来自两站，由调用方去重。
pub fn site_model_entries(variant: WbVariant, models: &[Value]) -> Vec<Value> {
    let prefix = match variant {
        WbVariant::Cn => PREFIX_CN,
        WbVariant::Ai => PREFIX_INTL,
    };
    let mut entries = Vec::new();
    for model in models {
        let Some(id) = model.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let owned_by = model
            .get("vendor")
            .and_then(Value::as_str)
            .unwrap_or("workbuddy");
        entries.push(model_entry(&format!("{prefix}{id}"), id, owned_by, model));
    }
    entries
}

fn model_entry(exported_id: &str, id: &str, owned_by: &str, model: &Value) -> Value {
    let mut entry = json!({
        "id": exported_id,
        "object": "model",
        "owned_by": owned_by,
        // 非标准附加字段：保留原始 id 与官方元数据，不破坏 OpenAI 客户端解析。
        "workbuddy_id": id,
    });
    if let Some(name) = model.get("name") {
        entry["name"] = name.clone();
    }
    if let Some(v) = model.get("maxInputTokens") {
        entry["max_input_tokens"] = v.clone();
    }
    if let Some(v) = model.get("maxOutputTokens") {
        entry["max_output_tokens"] = v.clone();
    }
    if let Some(v) = model.get("credits") {
        entry["credits"] = v.clone();
    }
    entry
}

    /// 路由状态是进程级全局，测试并行会互相干扰：用锁串行化并重置。
    #[cfg(test)]
    static ROUTER_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 获取路由测试串行锁并清空状态。返回的 guard 需持有到测试结束。
    #[cfg(test)]
    fn reset_router_for_test() -> std::sync::MutexGuard<'static, ()> {
        let guard = ROUTER_TEST_LOCK.lock().unwrap();
        let mut state = router().lock().unwrap();
        *state = RouterState::default();
        credits_cache().lock().unwrap().clear();
        guard
    }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_reads_known_prefixes() {
        let cn = parse_model("cn/glm-5.3").unwrap();
        assert_eq!(cn.site, Some(WbVariant::Cn));
        assert_eq!(cn.model, "glm-5.3");

        let intl = parse_model("intl/deepseek-v4.1-flash").unwrap();
        assert_eq!(intl.site, Some(WbVariant::Ai));
        assert_eq!(intl.model, "deepseek-v4.1-flash");

        let plain = parse_model("deepseek-v4.1-flash").unwrap();
        assert_eq!(plain.site, None);
        assert_eq!(plain.model, "deepseek-v4.1-flash");

        assert_eq!(parse_model("  cn/hy3  ").unwrap().model, "hy3");
    }

    #[test]
    fn parse_model_rejects_unknown_prefix_and_empty() {
        assert!(parse_model("").is_err());
        assert!(parse_model("cn/").is_err());
        assert!(parse_model("intl/").is_err());
        assert!(parse_model("foo/bar").is_err());
        assert!(parse_model("openai/gpt-5.6").is_err());
    }

    #[test]
    fn parse_credits_handles_all_observed_shapes() {
        assert_eq!(parse_credits(Some(&json!("x0.29"))), Some(0.29));
        assert_eq!(parse_credits(Some(&json!("x0.00"))), Some(0.0));
        assert_eq!(parse_credits(Some(&json!("x3.31 credits"))), Some(3.31));
        assert_eq!(parse_credits(Some(&json!("x0.34 credits"))), Some(0.34));
        assert_eq!(parse_credits(Some(&json!("x5.00"))), Some(5.0));
        assert_eq!(parse_credits(Some(&json!(""))), None);
        assert_eq!(parse_credits(None), None);
        assert_eq!(parse_credits(Some(&json!("n/a"))), None);
        assert_eq!(parse_credits(Some(&json!(1.5))), None);
    }

    #[test]
    fn urls_use_unified_product_domains() {
        assert_eq!(
            chat_url(WbVariant::Cn),
            "https://www.workbuddy.cn/v2/chat/completions"
        );
        assert_eq!(
            chat_url(WbVariant::Ai),
            "https://www.workbuddy.ai/v2/chat/completions"
        );
        assert_eq!(
            models_config_url(WbVariant::Cn),
            "https://www.workbuddy.cn/v3/config"
        );
        assert_eq!(
            models_config_url(WbVariant::Ai),
            "https://www.workbuddy.ai/v3/config"
        );
    }

    #[test]
    fn upstream_headers_inject_identity_and_product() {
        let account = json!({
            "access_token": "AT",
            "uid": "u-1",
            "domain": "www.workbuddy.ai",
            "enterpriseId": "ent-1",
        });
        let headers = upstream_headers(&account);
        assert_eq!(headers.get("Authorization").map(String::as_str), Some("Bearer AT"));
        assert_eq!(headers.get("X-User-Id").map(String::as_str), Some("u-1"));
        assert_eq!(headers.get("X-Domain").map(String::as_str), Some("www.workbuddy.ai"));
        assert_eq!(headers.get("X-Product").map(String::as_str), Some("SaaS"));
        assert_eq!(headers.get("X-Enterprise-Id").map(String::as_str), Some("ent-1"));
    }

    #[test]
    fn upstream_headers_skip_missing_identity() {
        let account = json!({ "access_token": "AT" });
        let headers = upstream_headers(&account);
        assert_eq!(headers.get("Authorization").map(String::as_str), Some("Bearer AT"));
        assert_eq!(headers.get("X-Product").map(String::as_str), Some("SaaS"));
        assert!(!headers.contains_key("X-User-Id"));
        assert!(!headers.contains_key("X-Domain"));
    }

    #[test]
    fn site_model_entries_adds_prefix_and_keeps_metadata() {
        let models = vec![json!({
            "id": "glm-5.3",
            "name": "GLM-5.3",
            "vendor": "e",
            "credits": "x0.79",
            "maxInputTokens": 1000000,
        })];
        let entries = site_model_entries(WbVariant::Cn, &models);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "cn/glm-5.3");
        assert_eq!(entries[0]["object"], "model");
        assert_eq!(entries[0]["owned_by"], "e");
        assert_eq!(entries[0]["workbuddy_id"], "glm-5.3");
        assert_eq!(entries[0]["credits"], "x0.79");

        let entries = site_model_entries(WbVariant::Ai, &models);
        assert_eq!(entries[0]["id"], "intl/glm-5.3");
    }

    #[test]
    fn active_account_prefers_state_then_falls_back() {
        let accounts = vec![
            json!({"id": "a", "uid": "u-a", "variant": "cn"}),
            json!({"id": "b", "uid": "u-b", "variant": "ai"}),
        ];
        let picked = active_account_from(&accounts).unwrap();
        assert_eq!(picked["id"], "a");
        assert!(active_account_from(&[]).is_none());
    }

    #[test]
    fn account_for_site_filters_by_variant_and_token() {
        let accounts = vec![
            json!({"id": "cn", "uid": "u-cn", "variant": "cn", "access_token": "t"}),
            json!({"id": "ai", "uid": "u-ai", "variant": "ai", "access_token": "t"}),
            json!({"id": "ai-no-token", "uid": "u-x", "variant": "ai"}),
        ];
        assert_eq!(account_for_site(&accounts, WbVariant::Ai).unwrap()["id"], "ai");
        assert_eq!(account_for_site(&accounts, WbVariant::Cn).unwrap()["id"], "cn");
    }

    #[test]
    fn select_account_filters_by_site() {
        let _guard = reset_router_for_test();
        let accounts = vec![
            json!({"id": "cn", "variant": "cn", "access_token": "t"}),
            json!({"id": "ai", "variant": "ai", "access_token": "t"}),
        ];
        let picked = select_account(&accounts, Some(WbVariant::Ai), None, 0).unwrap();
        assert_eq!(picked["id"], "ai");
        let picked = select_account(&accounts, Some(WbVariant::Cn), None, 0).unwrap();
        assert_eq!(picked["id"], "cn");
    }

    #[test]
    fn select_account_round_robins_within_site() {
        let _guard = reset_router_for_test();
        let accounts = vec![
            json!({"id": "cn-1", "variant": "cn", "access_token": "t"}),
            json!({"id": "cn-2", "variant": "cn", "access_token": "t"}),
        ];
        let a = select_account(&accounts, Some(WbVariant::Cn), None, 1).unwrap();
        let b = select_account(&accounts, Some(WbVariant::Cn), None, 2).unwrap();
        let c = select_account(&accounts, Some(WbVariant::Cn), None, 3).unwrap();
        assert_eq!(a["id"], "cn-1");
        assert_eq!(b["id"], "cn-2");
        assert_eq!(c["id"], "cn-1");
    }

    #[test]
    fn select_account_honors_session_stickiness() {
        let _guard = reset_router_for_test();
        let accounts = vec![
            json!({"id": "cn-1", "variant": "cn", "access_token": "t"}),
            json!({"id": "cn-2", "variant": "cn", "access_token": "t"}),
        ];
        // 首次选定后，同 session_key 必须固定返回同一账号。
        let first = select_account(&accounts, Some(WbVariant::Cn), Some("s-1"), 1).unwrap();
        let second = select_account(&accounts, Some(WbVariant::Cn), Some("s-1"), 2).unwrap();
        assert_eq!(first["id"], second["id"]);

        // 不同会话不受影响（可拿到另一个账号）。
        let other = select_account(&accounts, Some(WbVariant::Cn), Some("s-2"), 3).unwrap();
        assert_ne!(other["id"], first["id"]);
    }

    #[test]
    fn select_account_skips_tripped_breaker_then_recovers() {
        let _guard = reset_router_for_test();
        let accounts = vec![
            json!({"id": "cn-1", "variant": "cn", "access_token": "t"}),
            json!({"id": "cn-2", "variant": "cn", "access_token": "t"}),
        ];
        // 触发 cn-1 熔断。
        for _ in 0..BREAKER_FAILURE_THRESHOLD {
            record_result(&accounts[0], true, 0);
        }
        // 冷却期内所有请求都只能选 cn-2（cn-1 被跳过）。
        for _ in 0..4 {
            let picked = select_account(&accounts, Some(WbVariant::Cn), None, 0).unwrap();
            assert_eq!(picked["id"], "cn-2");
        }

        // 冷却结束后 cn-1 恢复可选：多次选择中必然出现 cn-1。
        let mut seen_cn1 = false;
        for tick in 0..4 {
            let picked = select_account(
                &accounts,
                Some(WbVariant::Cn),
                None,
                BREAKER_COOLDOWN_MS + 1 + tick,
            )
            .unwrap();
            seen_cn1 |= picked["id"] == "cn-1";
        }
        assert!(seen_cn1, "冷却结束后 cn-1 应重新参与轮询");
    }

    #[test]
    fn select_account_prefers_lower_credit() {
        let _guard = reset_router_for_test();
        let accounts = vec![
            json!({"id": "cn-1", "variant": "cn", "access_token": "t"}),
            json!({"id": "cn-2", "variant": "cn", "access_token": "t"}),
        ];
        update_account_credits(&accounts[0], &[json!({"credits": "x0.79"})]);
        update_account_credits(&accounts[1], &[json!({"credits": "x0.05"})]);
        // cn-2 倍率更低，应被优先选中（即使轮询起点是 cn-1）。
        let picked = select_account(&accounts, Some(WbVariant::Cn), None, 0).unwrap();
        assert_eq!(picked["id"], "cn-2");
    }

    #[test]
    fn record_result_success_clears_breaker() {
        let _guard = reset_router_for_test();
        let account = json!({"id": "cn-1", "variant": "cn", "access_token": "t"});
        record_result(&account, true, 0);
        record_result(&account, true, 0);
        // 一次成功清零，不至于累计到阈值。
        record_result(&account, false, 0);
        record_result(&account, true, 0);
        let accounts = vec![account];
        // 未熔断，仍可选。
        assert!(select_account(&accounts, Some(WbVariant::Cn), None, 0).is_some());
    }

    #[test]
    fn token_stale_covers_expired_near_and_missing() {
        let now = 1_000_000;
        // 已过期
        assert!(token_stale(&json!({"expiresAt": now - 1}), now));
        // 临近过期（余量内）
        assert!(token_stale(
            &json!({"expiresAt": now + PRE_REFRESH_MARGIN_MS - 1}),
            now
        ));
        // 仍新鲜
        assert!(!token_stale(
            &json!({"expiresAt": now + PRE_REFRESH_MARGIN_MS + 1}),
            now
        ));
        // 缺失 expiresAt：无法判断，按需刷新
        assert!(token_stale(&json!({}), now));
    }

    #[tokio::test]
    async fn send_chat_to_injects_headers_and_streams_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let headers_lower = request.to_ascii_lowercase();

            assert!(headers_lower.contains("authorization: bearer smoke-token"));
            assert!(headers_lower.contains("x-domain: www.workbuddy.ai"));
            assert!(headers_lower.contains("x-user-id: u-1"));
            assert!(headers_lower.contains("x-product: saas"));
            assert!(request.contains("\"model\":\"glm-5.3\""));

            let body = "data: {\"object\":\"chat.completion.chunk\"}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        });

        let account = json!({
            "variant": "ai",
            "access_token": "smoke-token",
            "uid": "u-1",
            "domain": "www.workbuddy.ai",
        });
        let url = format!("http://{addr}/v2/chat/completions");
        let resp = send_chat_to(&url, &account, &json!({"model": "glm-5.3"}))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        assert!(text.contains("chat.completion.chunk"));
        server.await.unwrap();
    }
}
