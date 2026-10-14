#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! ClaudeMusic Tauri 后端
//! 核心能力: proxy 自定义协议 —— 把网易云 API/音频请求代理为同源请求,
//! 解决 CORS 限制, 让前端 fetch 与 Web Audio 情绪分析(AnalyserNode)正常工作。

use std::io::{Read, Write};
use tauri::http::{Response, StatusCode};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BODY: u64 = 48 * 1024 * 1024; // 48MB 上限(网易云 mp3 约 3-10MB)

/// 前端黑匣子日志: 写入 D:/ClaudeMusic/logs/cm-debug.log, 便于离线诊断
#[tauri::command]
fn app_log(msg: String) {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("D:/ClaudeMusic/logs/cm-debug.log")
    {
        let _ = writeln!(f, "[{}.{:03}] {}", ms / 1000, ms % 1000, msg);
    }
}

fn handle_proxy(target: String) -> Response<Vec<u8>> {
    let allowed = url::Url::parse(&target)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .map(|h| {
            h.ends_with("163.com")
                || h.ends_with("126.net")
                || h.ends_with("qq.com")
        })
        .unwrap_or(false);

    if !allowed {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(format!("host not allowed: {target}").into_bytes())
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

    let mut req = client.get(&target);
    // QQ 音乐接口需要 Referer 才返回数据
    if target.contains("qq.com") {
        req = req.header("Referer", "https://y.qq.com/");
    }

    match req.send() {
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
    let _ = std::fs::create_dir_all("D:/ClaudeMusic/logs");
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_log])
        .register_uri_scheme_protocol("proxy", |_ctx, request| {
            let uri = request.uri().clone();
            // 首选: http://proxy.localhost/?u=<urlencoded-target>
            // 兼容: http://proxy.localhost/<urlencoded-target> (部分 WebView 会改写路径)
            let mut target = String::new();
            if let Some(q) = uri.query() {
                if let Some(rest) = q.strip_prefix("u=") {
                    target = urlencoding::decode(rest)
                        .map(|c| c.into_owned())
                        .unwrap_or_default();
                }
            }
            if target.is_empty() {
                let raw = uri.path().trim_start_matches('/');
                let dec = urlencoding::decode(raw)
                    .map(|c| c.into_owned())
                    .unwrap_or_default();
                if dec.starts_with("http") {
                    target = dec;
                } else if raw.starts_with("http") {
                    target = raw.to_string();
                }
            }
            handle_proxy(target)
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
