//! Shared OpenAI HTTP routes for desktop and CLI.
use super::{account, config, openai_proxy, variant::WbVariant};
use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

pub fn router() -> Router {
    Router::new()
        .route("/v1/chat/completions", post(proxy_chat_completions))
        .route("/v1/models", get(proxy_models))
}

/// POST /v1/chat/completions —— 注入当前激活账号凭据后透传到官方 OpenAI 兼容接口。
///
/// 上游本身是标准 OpenAI 协议，不做格式转换；`stream` 语义由请求体决定，
/// 这里一律用流式 body 透传（非流式响应是单块 JSON，也能正常返回）。
pub async fn proxy_chat_completions(
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !openai_proxy::proxy_enabled() {
        return openai_error("2API 代理未开启", StatusCode::NOT_FOUND, "proxy_disabled");
    }
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    let request = match openai_proxy::parse_model(model) {
        Ok(request) => request,
        Err(error) => {
            return openai_error(&error, StatusCode::BAD_REQUEST, "invalid_request_error")
        }
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
        if refreshed.get("needs_relogin").and_then(Value::as_bool) != Some(true) {
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
pub async fn proxy_models() -> Response {
    if !openai_proxy::proxy_enabled() {
        return openai_error("2API 代理未开启", StatusCode::NOT_FOUND, "proxy_disabled");
    }
    Json(list_models().await).into_response()
}

pub async fn list_models() -> Value {
    let accounts = account::load_accounts();
    let mut errors = Vec::new();
    let mut entries: Vec<Value> = Vec::new();
    let mut plain_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for site in [WbVariant::Cn, WbVariant::Ai] {
        let Some(account) = openai_proxy::account_for_site(&accounts, site) else {
            continue;
        };
        let models = match openai_proxy::fetch_site_models(&account).await {
            Ok(models) => models,
            Err(error) => {
                errors.push(format!("{site:?} 模型加载失败: {error}"));
                continue;
            }
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

    json!({ "object": "list", "data": entries, "errors": errors })
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
            openai_error(
                "响应构造失败",
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
            )
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
