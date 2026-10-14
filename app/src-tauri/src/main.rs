#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! ClaudeMusic Tauri 后端
//! 核心能力: proxy 自定义协议 —— 把网易云 API/音频请求代理为同源请求,
//! 解决 CORS 限制, 让前端 fetch 与 Web Audio 情绪分析(AnalyserNode)正常工作。

use std::io::Read;
use tauri::http::{Response, StatusCode};

const MAX_BODY: u64 = 48 * 1024 * 1024; // 48MB 上限(网易云 mp3 约 3-10MB)

fn handle_proxy(path_enc: &str) -> Response<Vec<u8>> {
    let target = urlencoding::decode(path_enc)
        .map(|c| c.into_owned())
        .unwrap_or_default();

    let allowed = url::Url::parse(&target)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .map(|h| h.ends_with("163.com") || h.ends_with("126.net"))
        .unwrap_or(false);

    if !allowed {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(b"host not allowed".to_vec())
            .unwrap();
    }

    let client = match reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) ClaudeMusic/0.3")
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(format!("client error: {e}").into_bytes())
                .unwrap()
        }
    };

    match client.get(&target).send() {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            let mut buf = Vec::with_capacity(1 << 20);
            let _ = resp.take(MAX_BODY).read_to_end(&mut buf);
            Response::builder()
                .status(status)
                .header("Access-Control-Allow-Origin", "*")
                .header("Content-Type", ct)
                .header("Cache-Control", "no-cache")
                .body(buf)
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(format!("proxy error: {e}").into_bytes())
            .unwrap(),
    }
}

fn main() {
    tauri::Builder::default()
        .register_uri_scheme_protocol("proxy", |_ctx, request| {
            let uri = request.uri().to_string();
            // Windows 下形如 http://proxy.localhost/<urlencoded-target>
            let enc = uri
                .split("proxy.localhost/")
                .nth(1)
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("");
            handle_proxy(enc)
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
