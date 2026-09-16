//! 2API：本地 OpenAI 兼容代理。
//!
//! 对外暴露 `/v1/chat/completions` 与 `/v1/models`，用账号库中**当前激活账号**
//! 的凭据注入鉴权头后透传到 WorkBuddy 官方 OpenAI 兼容接口。上游本身就是标准
//! OpenAI 协议（实测 `workbuddy.har`），因此**不做任何格式转换**，只做
//! 「选账号 + 注入头 + 双向流透传」。
//!
//! 域已统一：国内 `https://www.workbuddy.cn`、国际 `https://www.workbuddy.ai`
//! （见 `WbVariant::api_endpoint`）。聊天基址为 `{api_endpoint}/v2`。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::modules::account;
use crate::modules::account::get_str;
use crate::modules::config::{atomic_write, home_dir, http_client_streaming, store_dir};
use crate::modules::variant::WbVariant;

/// 聊天接口默认池前缀：`cn/` 锁国内站，`intl/` 锁国际站，无前缀走默认池。
pub const PREFIX_CN: &str = "cn/";
pub const PREFIX_INTL: &str = "intl/";

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
    headers.insert(
        "Content-Type".to_string(),
        "application/json".to_string(),
    );
    if let Some(uid) = get_str(account, "uid") {
        headers.insert("X-User-Id".to_string(), uid);
    }
    if let Some(domain) = get_str(account, "domain") {
        headers.insert("X-Domain".to_string(), domain);
    }
    if let Some(eid) = get_str(account, "enterpriseId")
        .or_else(|| get_str(account, "enterprise_id"))
    {
        headers.insert("X-Enterprise-Id".to_string(), eid.clone());
        headers.insert("X-Tenant-Id".to_string(), eid);
    }
    headers
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
        if WbVariant::from_account(&acc) == site
            && get_str(&acc, "access_token").is_some()
        {
            return Some(acc);
        }
    }
    accounts
        .iter()
        .find(|acc| {
            WbVariant::from_account(acc) == site && get_str(acc, "access_token").is_some()
        })
        .cloned()
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

        // 前后空白容错
        assert_eq!(parse_model("  cn/hy3  ").unwrap().model, "hy3");
    }

    #[test]
    fn parse_model_rejects_unknown_prefix_and_empty() {
        assert!(parse_model("").is_err());
        assert!(parse_model("cn/").is_err());
        assert!(parse_model("intl/").is_err());
        // 带斜杠但非已知前缀 → 非法，不回落默认池
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
        // 空 / 缺失 / 非数字 → None（无倍率）
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
        // 无 state 文件时（测试环境）回落第一条
        let picked = active_account_from(&accounts).unwrap();
        assert_eq!(picked["id"], "a");
        assert!(active_account_from(&[]).is_none());
    }

    /// 集成：用本地 mock 上游验证头注入与流式响应体原样透传。
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

            // 断言注入了鉴权与身份头。
            assert!(headers_lower.contains("authorization: bearer smoke-token"));
            assert!(headers_lower.contains("x-domain: www.workbuddy.ai"));
            assert!(headers_lower.contains("x-user-id: u-1"));
            assert!(headers_lower.contains("x-product: saas"));
            // 且请求体里模型为裸名（已去前缀）。
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
        let resp = send_chat_to(&url, &account, &json!({"model": "glm-5.3"})).await.unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        assert!(text.contains("chat.completion.chunk"));
        server.await.unwrap();
    }

    #[test]
    fn account_for_site_filters_by_variant_and_token() {
        let accounts = vec![
            json!({"id": "cn", "uid": "u-cn", "variant": "cn", "access_token": "t"}),
            json!({"id": "ai", "uid": "u-ai", "variant": "ai", "access_token": "t"}),
            json!({"id": "ai-no-token", "uid": "u-x", "variant": "ai"}),
        ];
        assert_eq!(
            account_for_site(&accounts, WbVariant::Ai).unwrap()["id"],
            "ai"
        );
        assert_eq!(
            account_for_site(&accounts, WbVariant::Cn).unwrap()["id"],
            "cn"
        );
    }
}
