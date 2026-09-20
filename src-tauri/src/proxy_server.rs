//! Desktop-only loopback listener; configuration is shared with the CLI.
use buddy2api_core::modules::{account, openai_proxy, proxy_http};
use serde_json::{json, Value};
use tokio::{sync::Mutex, task::JoinHandle};

const ADDRESS: &str = "127.0.0.1:57891";
static SERVER: Mutex<Option<JoinHandle<()>>> = Mutex::const_new(None);

async fn reconcile(enabled: bool) -> (bool, Option<String>) {
    let mut server = SERVER.lock().await;
    let mut error = None;
    if server.as_ref().is_some_and(|task| task.is_finished()) {
        *server = None;
    }
    if enabled && server.is_none() {
        match tokio::net::TcpListener::bind(ADDRESS).await {
            Ok(listener) => {
                *server = Some(tokio::spawn(async move {
                    if let Err(error) = axum::serve(listener, proxy_http::router()).await {
                        eprintln!("[2API] 服务停止: {error}");
                    }
                }));
            }
            Err(e) => error = Some(format!("无法监听 {ADDRESS}：{e}。请检查端口是否被占用。")),
        }
    } else if !enabled {
        if let Some(task) = server.take() {
            task.abort();
            let _ = task.await;
        }
    }
    (server.is_some(), error)
}

pub async fn status() -> Value {
    let enabled = openai_proxy::proxy_enabled();
    let (running, error) = reconcile(enabled).await;
    let account = openai_proxy::active_account();
    json!({
        "enabled": enabled,
        "running": running,
        "error": error,
        "baseUrl": format!("http://{ADDRESS}/v1"),
        "config": openai_proxy::load_proxy_config(),
        "activeAccount": account.as_ref().map(account::account_meta),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn listener_reports_conflicts_serves_http_and_releases_port() {
        let occupied = tokio::net::TcpListener::bind(ADDRESS).await.unwrap();
        let (running, error) = reconcile(true).await;
        assert!(!running);
        assert!(error.unwrap().contains(ADDRESS));
        drop(occupied);

        assert_eq!(reconcile(true).await, (true, None));
        assert_eq!(reconcile(true).await, (true, None));
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut connection = tokio::net::TcpStream::connect(ADDRESS).await.unwrap();
        connection
            .write_all(b"GET /unknown HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        connection.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 404"));

        assert_eq!(reconcile(false).await, (false, None));
        let rebound = tokio::net::TcpListener::bind(ADDRESS).await.unwrap();
        drop(rebound);
    }
}
