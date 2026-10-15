#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! ClaudeMusic Tauri 后端
//! 核心能力: proxy 自定义协议 —— 把网易云 API/音频请求代理为同源请求,
//! 解决 CORS 限制, 让前端 fetch 与 Web Audio 情绪分析(AnalyserNode)正常工作。

use std::io::{Read, Write};
use tauri::http::{Response, StatusCode};
use tauri::Manager;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BODY: u64 = 48 * 1024 * 1024; // 48MB 上限(网易云 mp3 约 3-10MB)

/// 前端黑匣子日志: 写入 Tauri 标准日志目录(随安装位置自适应, 不硬编码盘符)
/// 文件带 UTF-8 BOM(中文 Windows 下记事本/PowerShell 不乱码), 超 2MB 自动轮转
#[tauri::command]
fn app_log(app: tauri::AppHandle, msg: String) {
    let dir = match app.path().app_log_dir() {
        Ok(d) => d,
        Err(_) => std::env::temp_dir(),
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("claudemusic.log");
    let is_new = !path.exists();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        if is_new {
            let _ = f.write_all(b"\xEF\xBB\xBF"); // UTF-8 BOM
        }
        // 轮转: 超 2MB 归档为 .old 后重写
        let need_rotate = std::fs::metadata(&path).map(|m| m.len() > 2 * 1024 * 1024).unwrap_or(false);
        if need_rotate {
            drop(f);
            let _ = std::fs::rename(&path, dir.join("claudemusic.log.old"));
            if let Ok(mut f2) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = f2.write_all(b"\xEF\xBB\xBF");
                let _ = writeln!(f2, "{}", msg);
            }
            return;
        }
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "[{}.{:03}] {}", ms / 1000, ms % 1000, msg);
    }
}

/// 域名白名单: 精确或点分后缀匹配(防 evil163.com 这类绕过)
fn host_allowed(host: &str) -> bool {
    ["163.com", "126.net", "qq.com"].iter().any(|suffix| {
        host == *suffix || host.ends_with(&format!(".{}", suffix))
    })
}

/// 复用单例 HTTP 客户端: 连接池 + TLS 会话复用, seek/切歌不再重复握手
static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
fn get_client() -> &'static reqwest::blocking::Client {
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) ClaudeMusic/0.3")
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("failed to build http client")
    })
}

fn handle_proxy(target: String, range: Option<String>) -> Response<Vec<u8>> {
    let allowed = url::Url::parse(&target)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .map(|h| host_allowed(&h))
        .unwrap_or(false);

    if !allowed {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(format!("host not allowed: {target}").into_bytes())
            .unwrap();
    }

    let client = get_client();

    let mut req = client.get(&target);
    // QQ 音乐接口需要 Referer 才返回数据
    if target.contains("qq.com") {
        req = req.header("Referer", "https://y.qq.com/");
    }
    // 转发 Range 头 → 上游返回 206 分段, 音频 seek 才能正常工作
    if let Some(r) = &range {
        req = req.header("Range", r);
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
            let content_range = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let mut buf = Vec::with_capacity(1 << 20);
            let _ = resp.take(MAX_BODY).read_to_end(&mut buf);
            // 注意: 不转发上游 Content-Length —— take(MAX_BODY) 截断后会与实际字节不符,
            // Tauri 会按 buf 实际长度自动设置
            let mut b = Response::builder()
                .status(status)
                .header("Access-Control-Allow-Origin", "*")
                .header("Content-Type", ct)
                .header("Accept-Ranges", "bytes");
            if let Some(cr) = content_range {
                b = b.header("Content-Range", cr);
            }
            b.body(buf).unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(format!("proxy error: {e}").into_bytes())
            .unwrap(),
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_log])
        // 异步协议处理器: 网络请求在独立线程执行, 不阻塞 UI/WebView 线程
        .register_asynchronous_uri_scheme_protocol("proxy", |_ctx, request, responder| {
            let uri = request.uri().clone();
            let range = request
                .headers()
                .get("range")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            // 首选: https://proxy.localhost/?u=<urlencoded-target>
            // 兼容: 路径传参变体 (部分 WebView 会改写路径)
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
            std::thread::spawn(move || {
                responder.respond(handle_proxy(target, range));
            });
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
