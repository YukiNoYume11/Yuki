#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! YukiMusic Tauri 后端(原名 ClaudeMusic; identifier 保留 local.claudemusic 以兼容既有数据)
//! 核心能力: proxy 自定义协议 —— 把网易云 API/音频请求代理为同源请求,
//! 解决 CORS 限制, 让前端 fetch 与 Web Audio 情绪分析(AnalyserNode)正常工作。
//! 登录/歌单/VIP 取链: 直接走 weapi(AES-128-CBC + 原始 RSA)加密接口, Cookie 存进程内状态。

use std::io::{Read, Write};
use tauri::http::{Response, StatusCode};
use tauri::Manager;
use std::time::{SystemTime, UNIX_EPOCH};

// ---- weapi/eapi 加密相关依赖 ----
use aes::Aes128;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyInit, KeyIvInit};
use cbc::{Decryptor, Encryptor};
use ecb::Encryptor as EcbEncryptor;
use md5::{Digest as Md5Digest, Md5};
use num_bigint::BigUint;
use num_traits::Num;
use rand::Rng;
use qrcode::render::svg;

type Aes128CbcEnc = Encryptor<Aes128>;
type Aes128CbcDec = Decryptor<Aes128>;
type Aes128EcbEnc = EcbEncryptor<Aes128>;

const MAX_BODY: u64 = 48 * 1024 * 1024; // 48MB 上限(网易云 mp3 约 3-10MB)

// weapi 标准常量(公开): 首轮 AES key / IV / RSA 公钥
const WEAPI_NONCE: &[u8] = b"0CoJUm6Qyw8W8jud";
const WEAPI_IV: &[u8] = b"0102030405060708";
const RSA_MODULUS: &str = "00e0b509f6259df8642dbc35662901477df22677ec152b5ff68ace615bb7b725152b3ab17a876aea8a5aa76d2e417629ec4ee341f56135fccf695280104e0312ecbda92557c93870114af6c9d05c4f7f0c3685b7a46bee255932575cce10b424d813cfe4875d3e82047b97ddef52741d546b8e289dc6935b3ece0462db0a22b8e7";
const RSA_EXPONENT: u32 = 65537;
const BASE62: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

// eapi(官方客户端加密)常量: 用于取 VIP 可播直链(weapi/os_web 对客户端专属曲返回受限 jdymusic 直链, CDN 403)
const EAPI_KEY: &[u8] = b"e82ckenh8dichen8";
const API_DOMAIN: &str = "https://interface.music.163.com";
const UA_CLIENT: &str = "NeteaseMusic 9.0.90/5038 (iPhone; iOS 16.2; zh_CN)";

/// 前端黑匣子日志: 写入 Tauri 标准日志目录(随安装位置自适应, 不硬编码盘符)
/// 文件带 UTF-8 BOM(中文 Windows 下记事本/PowerShell 不乱码), 超 2MB 自动轮转
fn log_msg(app: &tauri::AppHandle, msg: &str) {
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

#[tauri::command]
fn app_log(app: tauri::AppHandle, msg: String) {
    log_msg(&app, &msg);
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
            .user_agent(UA)
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("failed to build http client")
    })
}

/// 登录/歌单专用客户端(浏览器 UA + Referer, 网易对非浏览器 UA 更敏感)
static AUTH_CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
fn auth_client() -> &'static reqwest::blocking::Client {
    AUTH_CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent(UA)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build auth client")
    })
}

/// 执行一次代理抓取, 返回 (状态码, Content-Type, Content-Range, 响应体)
/// send_referer/send_range 用于 403 时做请求头 A/B(定位网易 jdymusic 防盗链到底拒绝什么)。
/// 注意: 绝不向 126.net CDN 透传 .163.com 的 MUSIC_U Cookie——跨域 Cookie 会被 Tengine 边缘直接 403。
fn proxy_fetch(target: &str, range: &Option<String>, send_referer: bool) -> Result<(StatusCode, String, Option<String>, Vec<u8>), String> {
    let client = get_client();
    let mut req = client.get(target);
    // QQ 音乐接口需要 Referer 才返回数据
    if target.contains("qq.com") {
        req = req.header("Referer", "https://y.qq.com/");
    }
    if send_referer && (target.contains("126.net") || target.contains("163.com")) {
        req = req.header("Referer", "https://music.163.com/");
    }
    // 转发 Range 头 → 上游返回 206 分段, 音频 seek 才能正常工作
    if let Some(r) = range {
        req = req.header("Range", r);
    }
    let resp = req.send().map_err(|e| e.to_string())?;
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
    Ok((status, ct, content_range, buf))
}

/// 音频 URL 判定(按扩展名, 与域名无关)
fn is_audio_url(t: &str) -> bool {
    let l = t.to_lowercase();
    [".mp3", ".flac", ".m4a", ".ogg", ".aac", ".wav"].iter().any(|e| l.contains(e))
}

/// 伪流式: 音频请求的 Range 钳制为 ≤2MB 分片。
/// 代理是缓冲转发(受 Tauri API 限制无法真流式), 无损 FLAC 40MB+ 遇慢 CDN(QQ tc.qq.com)会整段
/// 下载完才响应 → 起播卡 20s+。钳制后浏览器收到 206+Content-Range 会自动续传后续分片,
/// 起播只需第一个 2MB, 与 CDN 速度解耦。
fn clamp_audio_range(range: &Option<String>, is_audio_file: bool) -> Option<String> {
    if !is_audio_file {
        return range.clone();
    }
    const CAP: u64 = 2 * 1024 * 1024;
    match range {
        None => Some(format!("bytes=0-{}", CAP - 1)),
        Some(r) => {
            let s = r.trim().trim_start_matches("bytes=");
            let mut it = s.split('-');
            let start: u64 = it.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            let end: Option<u64> = it.next().and_then(|v| v.trim().parse().ok());
            let capped = match end {
                Some(e) if e >= start && e - start + 1 <= CAP => e,
                _ => start + CAP - 1,
            };
            Some(format!("bytes={start}-{capped}"))
        }
    }
}

fn handle_proxy(app: tauri::AppHandle, target: String, range: Option<String>) -> Response<Vec<u8>> {
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

    let is_audio = target.contains("music.126.net");
    let is_audio_file = is_audio_url(&target);
    let range = clamp_audio_range(&range, is_audio_file);
    let mut used = target.clone();
    // 首选: http + Referer + Range(与浏览器 <audio> 取流一致)
    let mut result = proxy_fetch(&used, &range, true);

    // 网易 VIP 高级直链(jdymusic, m704/m804)常被 CDN 边缘 403。对同一新鲜直链就地做请求变体 A/B,
    // 命中非 403 即用——避免直链过期(20分钟)且无需多次重试。逐个记录状态以定位真凶。
    if is_audio {
        if let Ok((st, _, _, _)) = &result {
            if st.as_u16() == 403 {
                log_msg(&app, &format!("PROXY 403, 开始变体A/B fullUrl={used}"));
                // 变体1: 升级 https
                if used.starts_with("http://") {
                    let https_target = format!("https://{}", &used["http://".len()..]);
                    if let Ok(r) = proxy_fetch(&https_target, &range, true) {
                        log_msg(&app, &format!("PROXY var1 https -> {}", r.0.as_u16()));
                        if r.0.as_u16() != 403 {
                            used = https_target;
                            result = Ok(r);
                        }
                    }
                }
                // 变体2: 去掉 Referer(部分对象存储防盗链白名单不含 163.com Referer)
                if matches!(&result, Ok((s, _, _, _)) if s.as_u16() == 403) {
                    if let Ok(r) = proxy_fetch(&used, &range, false) {
                        log_msg(&app, &format!("PROXY var2 no-referer -> {}", r.0.as_u16()));
                        if r.0.as_u16() != 403 {
                            result = Ok(r);
                        }
                    }
                }
                // 变体3: 去掉 Range(部分对象存储对 Range 请求返回 403)
                if matches!(&result, Ok((s, _, _, _)) if s.as_u16() == 403) {
                    if let Ok(r) = proxy_fetch(&used, &None, true) {
                        log_msg(&app, &format!("PROXY var3 no-range -> {}", r.0.as_u16()));
                        if r.0.as_u16() != 403 {
                            result = Ok(r);
                        }
                    }
                }
                // 变体4: 去 Referer + 去 Range
                if matches!(&result, Ok((s, _, _, _)) if s.as_u16() == 403) {
                    if let Ok(r) = proxy_fetch(&used, &None, false) {
                        log_msg(&app, &format!("PROXY var4 no-referer-no-range -> {}", r.0.as_u16()));
                        if r.0.as_u16() != 403 {
                            result = Ok(r);
                        }
                    }
                }
            }
        }
    }

    match result {
        Ok((status, ct, content_range, buf)) => {
            // 音频 CDN 探针(全部音频流, 含 QQ): 正常音频为数 MB audio/*; 小响应则 dump 正文定位
            if is_audio || is_audio_file {
                let mut line = format!(
                    "PROXY music status={status} ct={ct} bytes={} url={}",
                    buf.len(),
                    &used[..used.len().min(120)]
                );
                if buf.len() < 100_000 {
                    let preview: String = String::from_utf8_lossy(&buf[..buf.len().min(300)])
                        .chars()
                        .filter(|c| !c.is_control())
                        .collect();
                    line.push_str(&format!(" body={preview}"));
                }
                log_msg(&app, &line);
            }
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
        Err(e) => {
            log_msg(&app, &format!("PROXY send_err={e} url={}", &target[..target.len().min(90)]));
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(format!("proxy error: {e}").into_bytes())
                .unwrap()
        }
    }
}

// ==================== weapi 加解密 ====================

fn aes_encrypt_b64(plain: &[u8], key: &[u8]) -> String {
    let ct = Aes128CbcEnc::new_from_slices(key, WEAPI_IV)
        .expect("aes key len")
        .encrypt_padded_vec_mut::<Pkcs7>(plain);
    B64.encode(ct)
}

fn aes_decrypt_b64(b64: &str, key: &[u8]) -> String {
    let bytes = B64.decode(b64.trim()).unwrap_or_default();
    let pt = Aes128CbcDec::new_from_slices(key, WEAPI_IV)
        .expect("aes key len")
        .decrypt_padded_vec_mut::<Pkcs7>(&bytes)
        .unwrap_or_default();
    String::from_utf8_lossy(&pt).into_owned()
}

fn random_secret_key() -> String {
    let mut rng = rand::thread_rng();
    (0..16).map(|_| BASE62[rng.gen_range(0..BASE62.len())] as char).collect()
}

/// 原始 RSA(无 padding): c = m^e mod n, 输入为字节串, 输出 hex
fn rsa_encrypt(data: &str) -> String {
    let m = BigUint::from_bytes_be(data.as_bytes());
    let e = BigUint::from(RSA_EXPONENT);
    let n = BigUint::from_str_radix(RSA_MODULUS, 16).expect("modulus");
    let c = m.modpow(&e, &n);
    let mut hex = c.to_str_radix(16);
    if hex.len() % 2 == 1 {
        hex.insert(0, '0');
    }
    hex
}

/// weapi 请求: 返回 (params, encSecKey) 表单字段
fn weapi(obj: &serde_json::Value) -> (String, String) {
    let text = obj.to_string();
    let secret = random_secret_key();
    let inner = aes_encrypt_b64(text.as_bytes(), WEAPI_NONCE);
    let params = aes_encrypt_b64(inner.as_bytes(), secret.as_bytes());
    let reversed: String = secret.chars().rev().collect();
    let enc_sec_key = rsa_encrypt(&reversed);
    (params, enc_sec_key)
}

/// POST weapi 接口, 返回解密后的 JSON; cookie 可选
fn weapi_post(path: &str, obj: serde_json::Value, cookie: Option<&str>) -> Result<serde_json::Value, String> {
    weapi_post_sc(path, obj, cookie).map(|(j, _)| j)
}

/// 同 weapi_post, 额外返回 Set-Cookie 头("name=value"列表)——扫码登录需要会话延续
fn weapi_post_sc(path: &str, obj: serde_json::Value, cookie: Option<&str>) -> Result<(serde_json::Value, Vec<String>), String> {
    let (params, enc_sec_key) = weapi(&obj);
    let mut req = auth_client()
        .post(format!("https://music.163.com{path}"))
        .header("Referer", "https://music.163.com/")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[("params", params.as_str()), ("encSecKey", enc_sec_key.as_str())]);
    if let Some(c) = cookie {
        req = req.header("Cookie", c);
    }
    let resp = req.send().map_err(|e| format!("send: {e}"))?;
    let status = resp.status();
    let set_cookies: Vec<String> = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|s| s.split(';').next())
        .map(|s| s.trim().to_string())
        .filter(|s| s.contains('='))
        .collect();
    let body = resp.text().map_err(|e| format!("body: {e}"))?;
    let dec = if body.trim_start().starts_with('{') {
        body.clone()
    } else {
        aes_decrypt_b64(&body, WEAPI_NONCE)
    };
    let j = serde_json::from_str(&dec)
        .or_else(|_| serde_json::from_str(&body))
        .map_err(|e| format!("parse {status}: {e} :: {}", &dec[..dec.len().min(200)]))?;
    Ok((j, set_cookies))
}

fn extract_csrf(cookie: &str) -> String {
    cookie
        .split(';')
        .map(|kv| kv.trim())
        .find_map(|kv| {
            let mut it = kv.splitn(2, '=');
            if it.next()? == "__csrf" {
                it.next().map(|s| s.to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

// ==================== eapi (官方客户端加密) ====================
// weapi(os_web 网页身份)对"需客户端播放"的 VIP 曲目只返回受限 jdymusic/obj 直链, CDN 一律 403。
// eapi 以官方客户端身份(os=pc + 客户端 header 对象 + 客户端 UA)请求, 换取可拉取的直链
// (ymusic / jd-musicrep-ts), 实测返回真实 MP3(ID3 头, 206 audio/mpeg)。

static EAPI_CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
fn eapi_client() -> &'static reqwest::blocking::Client {
    EAPI_CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent(UA_CLIENT)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build eapi client")
    })
}

/// 进程级稳定 deviceId(真实设备 deviceId 固定, 避免每次请求都变触发风控)
static DEVICE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
fn device_id() -> &'static str {
    DEVICE_ID.get_or_init(|| {
        let mut rng = rand::thread_rng();
        (0..32).map(|_| BASE62[rng.gen_range(0..BASE62.len())] as char).collect()
    })
}

/// 从 Cookie 串按名取值
fn extract_cookie_value(cookie: &str, name: &str) -> String {
    cookie
        .split(';')
        .map(|kv| kv.trim())
        .find_map(|kv| {
            let mut it = kv.splitn(2, '=');
            if it.next()? == name {
                it.next().map(|s| s.to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn md5_hex(data: &[u8]) -> String {
    let mut h = Md5::new();
    Md5Digest::update(&mut h, data);
    let out = h.finalize();
    let mut s = String::with_capacity(32);
    for b in out.iter() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn hex_upper(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02X}", b));
    }
    s
}

/// eapi 加密: params = AES-128-ECB("{uri}-36cd479b6b5-{json}-36cd479b6b5-{md5}") → 大写 hex
fn eapi_encrypt(uri: &str, data: &serde_json::Value) -> String {
    let text = data.to_string();
    let digest = md5_hex(format!("nobody{uri}use{text}md5forencrypt").as_bytes());
    let payload = format!("{uri}-36cd479b6b5-{text}-36cd479b6b5-{digest}");
    let ct = Aes128EcbEnc::new_from_slice(EAPI_KEY)
        .expect("eapi key len")
        .encrypt_padded_vec_mut::<Pkcs7>(payload.as_bytes());
    hex_upper(&ct)
}

/// 客户端 header 对象序列化为 Cookie 串(eapi 请求头字段同时以 Cookie 发送)
fn header_cookie(header: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(obj) = header.as_object() {
        for (k, v) in obj {
            let vs = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            parts.push(format!("{}={}", urlencoding::encode(k), urlencoding::encode(&vs)));
        }
    }
    parts.join("; ")
}

/// 构造 eapi 客户端 header 对象(os=pc 官方客户端身份; 登录则带 MUSIC_U)
fn build_client_header(cookie: Option<&str>) -> serde_json::Value {
    let now_ms = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut rng = rand::thread_rng();
    let mut h = serde_json::json!({
        "osver": "Microsoft-Windows-10-Professional-build-19045-64bit",
        "deviceId": device_id(),
        "os": "pc",
        "appver": "3.1.17.204416",
        "versioncode": "140",
        "mobilename": "",
        "buildver": (now_ms / 1000).to_string(),
        "resolution": "1920x1080",
        "__csrf": cookie.map(extract_csrf).unwrap_or_default(),
        "channel": "netease",
        "requestId": format!("{}_{}", now_ms, rng.gen_range(0..1000)),
    });
    if let Some(c) = cookie {
        let mu = extract_cookie_value(c, "MUSIC_U");
        if !mu.is_empty() {
            h["MUSIC_U"] = serde_json::Value::String(mu);
        }
    }
    h
}

/// POST eapi 接口(interface.music.163.com/eapi/...); e_r=false 时响应为明文 JSON
fn eapi_post(uri: &str, cookie: Option<&str>, mut data: serde_json::Value) -> Result<serde_json::Value, String> {
    let header = build_client_header(cookie);
    data["header"] = header.clone();
    data["e_r"] = serde_json::json!(false);
    let params = eapi_encrypt(uri, &data);
    let url = format!("{API_DOMAIN}/eapi/{}", &uri[5..]); // 去掉 "/api/" 前缀
    let resp = eapi_client()
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Referer", "https://music.163.com/")
        .header("Cookie", header_cookie(&header))
        .form(&[("params", params.as_str())])
        .send()
        .map_err(|e| format!("send: {e}"))?;
    let body = resp.text().map_err(|e| format!("body: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("parse: {e} :: {}", &body[..body.len().min(200)]))
}

// ==================== 登录态 ====================

#[derive(Clone)]
struct LoginInfo {
    cookie: String,
    uid: i64,
    nickname: String,
    avatar: String,
}

struct LoginState(std::sync::Mutex<Option<LoginInfo>>);

/// 扫码会话的匿名 Cookie(unikey 响应的 Set-Cookie)——轮询必须带上, 否则易触发易盾风控 8821
static QR_SESSION: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn login_info(state: &tauri::State<'_, LoginState>) -> Option<LoginInfo> {
    state.0.lock().ok()?.clone()
}

/// 规整用户粘贴的 Cookie: 若只给了 MUSIC_U 的值(无 '='), 自动补成 MUSIC_U=<值>
fn normalize_cookie(raw: &str) -> String {
    let c = raw.trim().trim_matches('"').trim();
    if c.is_empty() {
        return String::new();
    }
    if c.contains('=') {
        c.to_string()
    } else {
        format!("MUSIC_U={c}")
    }
}

/// 从 JSON 按多个候选路径取第一个非零 i64
fn json_pick_i64(j: &serde_json::Value, paths: &[&[&str]]) -> i64 {
    for path in paths {
        let mut cur = j;
        for seg in *path {
            cur = &cur[*seg];
        }
        if let Some(v) = cur.as_i64() {
            if v != 0 {
                return v;
            }
        }
    }
    0
}
/// 从 JSON 按多个候选路径取第一个非空字符串
fn json_pick_str(j: &serde_json::Value, paths: &[&[&str]]) -> String {
    for path in paths {
        let mut cur = j;
        for seg in *path {
            cur = &cur[*seg];
        }
        if let Some(s) = cur.as_str() {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// 用 Cookie 拉取用户信息。/api/nav 已废弃(实测返回 code=404「接口未找到」),
/// 改用 weapi /weapi/user/info(301=未登录, 200=已登录); 昵称/头像缺失时用
/// /weapi/v1/user/detail/<uid> 补全。全链路带日志探针(排障第一步读此日志)。
fn fetch_profile(app: &tauri::AppHandle, cookie: &str) -> Option<LoginInfo> {
    let j = match weapi_post("/weapi/user/info", serde_json::json!({}), Some(cookie)) {
        Ok(j) => j,
        Err(e) => {
            log_msg(app, &format!("LOGIN userinfo err={e}"));
            return None;
        }
    };
    let code = j["code"].as_i64().unwrap_or(-1);
    let head: String = j.to_string().chars().take(500).collect();
    log_msg(app, &format!("LOGIN userinfo code={code} cookieLen={} body={head}", cookie.len()));
    if code != 200 {
        return None; // 301/其它 = cookie 无效或过期
    }
    let uid = json_pick_i64(
        &j,
        &[
            &["userPoint", "userId"][..],
            &["profile", "userId"][..],
            &["userId"][..],
            &["data", "profile", "userId"][..],
            &["account", "id"][..],
            &["data", "userId"][..],
            &["data", "account", "id"][..],
            &["profile", "uid"][..],
        ],
    );
    let mut nickname = json_pick_str(
        &j,
        &[
            &["profile", "nickname"][..],
            &["nickname"][..],
            &["data", "profile", "nickname"][..],
            &["data", "nickname"][..],
        ],
    );
    let mut avatar = json_pick_str(
        &j,
        &[
            &["profile", "avatarUrl"][..],
            &["avatarUrl"][..],
            &["data", "profile", "avatarUrl"][..],
            &["data", "avatarUrl"][..],
        ],
    );
    // uid 存在但昵称/头像缺失时, 用 user/detail 补全
    if uid > 0 && (nickname.is_empty() || avatar.is_empty()) {
        if let Ok(d) = weapi_post(&format!("/weapi/v1/user/detail/{uid}"), serde_json::json!({}), Some(cookie)) {
            if nickname.is_empty() {
                nickname = json_pick_str(&d, &[&["profile", "nickname"][..], &["nickname"][..]]);
            }
            if avatar.is_empty() {
                avatar = json_pick_str(&d, &[&["profile", "avatarUrl"][..], &["avatarUrl"][..]]);
            }
        }
    }
    if uid == 0 {
        log_msg(app, "LOGIN userinfo code=200 但未解析出 uid(歌单不可用, VIP 播放不受影响)");
    }
    Some(LoginInfo {
        cookie: cookie.to_string(),
        uid,
        nickname,
        avatar,
    })
}

fn profile_json(info: &LoginInfo) -> serde_json::Value {
    serde_json::json!({"nickname": info.nickname, "avatar": info.avatar, "uid": info.uid})
}

// ==================== Tauri commands: 登录/歌单 ====================

#[tauri::command]
fn netease_qr_key(app: tauri::AppHandle) -> Result<String, String> {
    // 浏览器同款流程: unikey 响应的匿名会话 Cookie 存下, 后续轮询必须复用同一会话
    let (j, sc) = weapi_post_sc(
        "/weapi/login/qrcode/unikey",
        serde_json::json!({"type": 1, "csrf_token": ""}),
        None,
    )?;
    if !sc.is_empty() {
        if let Ok(mut g) = QR_SESSION.lock() {
            *g = Some(sc.join("; "));
        }
    }
    log_msg(&app, &format!("LOGIN qr_key resp={j} setcookie={}", sc.len()));
    j["unikey"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("no unikey: {j}"))
}

#[tauri::command]
fn netease_qr_svg(unikey: String) -> Result<String, String> {
    let content = format!("https://music.163.com/login?codekey={unikey}");
    let code = qrcode::QrCode::new(content.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build())
}

#[tauri::command]
fn netease_qr_check(app: tauri::AppHandle, state: tauri::State<'_, LoginState>, key: String) -> Result<serde_json::Value, String> {
    let sess = QR_SESSION.lock().ok().and_then(|g| g.clone());
    let (j, sc) = weapi_post_sc(
        "/weapi/login/qrcode/client/login",
        serde_json::json!({"key": key, "type": 1}),
        sess.as_deref(),
    )?;
    let code = j["code"].as_i64().unwrap_or(-1);
    log_msg(&app, &format!("LOGIN qr_check code={code}"));
    let mut out = serde_json::json!({"code": code});
    if code == 803 {
        // 803 授权成功: cookie 可能在响应体的 cookie 字段, 也可能只在 Set-Cookie 头
        let mut cookie = j["cookie"].as_str().unwrap_or("").to_string();
        // 仅补充 body 里缺失的键(避免 MUSIC_U 重复歧义)
        for kv in &sc {
            let name = kv.split('=').next().unwrap_or("");
            if !name.is_empty() && !cookie.contains(&format!("{name}=")) {
                if !cookie.is_empty() && !cookie.ends_with(';') {
                    cookie.push_str("; ");
                }
                cookie.push_str(kv);
            }
        }
        if !cookie.is_empty() {
            if let Some(info) = fetch_profile(&app, &cookie) {
                log_msg(&app, &format!("LOGIN ok uid={} nick={}", info.uid, info.nickname));
                out["profile"] = profile_json(&info);
                out["cookie"] = serde_json::Value::String(cookie.clone());
                *state.0.lock().unwrap() = Some(info);
            } else {
                log_msg(&app, "LOGIN 803 but fetch_profile failed");
            }
        } else {
            log_msg(&app, "LOGIN 803 but no cookie in body/header");
        }
    }
    Ok(out)
}

#[tauri::command]
fn netease_set_cookie(app: tauri::AppHandle, state: tauri::State<'_, LoginState>, cookie: String) -> Result<serde_json::Value, String> {
    let cookie = normalize_cookie(&cookie);
    if cookie.is_empty() {
        return Err("cookie 为空".to_string());
    }
    log_msg(&app, &format!("LOGIN set_cookie len={} preview={}", cookie.len(), &cookie[..cookie.len().min(24)]));
    let info = fetch_profile(&app, &cookie).ok_or_else(|| "cookie 无效或已过期(/api/nav 未返回用户信息)".to_string())?;
    let out = profile_json(&info);
    *state.0.lock().unwrap() = Some(info);
    Ok(out)
}

#[tauri::command]
fn netease_logout(state: tauri::State<'_, LoginState>) -> Result<(), String> {
    *state.0.lock().unwrap() = None;
    Ok(())
}

#[tauri::command]
fn netease_status(state: tauri::State<'_, LoginState>) -> Result<Option<serde_json::Value>, String> {
    Ok(login_info(&state).as_ref().map(profile_json))
}

#[tauri::command]
fn netease_user_playlists(app: tauri::AppHandle, state: tauri::State<'_, LoginState>) -> Result<serde_json::Value, String> {
    let info = login_info(&state).ok_or("未登录")?;
    let csrf = extract_csrf(&info.cookie);
    let j = weapi_post(
        "/weapi/user/playlist",
        serde_json::json!({"uid": info.uid, "limit": 1000, "offset": 0, "csrf_token": csrf}),
        Some(&info.cookie),
    )?;
    let arr = j["playlist"].as_array().cloned().unwrap_or_default();
    log_msg(&app, &format!("PLAYLIST user count={}", arr.len()));
    let list: Vec<serde_json::Value> = arr
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p["id"],
                "name": p["name"],
                "trackCount": p["trackCount"],
                "subscribed": p["subscribed"],
            })
        })
        .collect();
    Ok(serde_json::json!(list))
}

#[tauri::command]
fn netease_playlist_detail(app: tauri::AppHandle, state: tauri::State<'_, LoginState>, id: i64) -> Result<serde_json::Value, String> {
    let cookie = login_info(&state).map(|i| i.cookie);
    let url = format!("https://music.163.com/api/playlist/detail?id={id}&limit=1000&offset=0");
    let mut req = auth_client().get(&url).header("Referer", "https://music.163.com/");
    if let Some(c) = &cookie {
        req = req.header("Cookie", c.as_str());
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    let j: serde_json::Value = serde_json::from_str(&resp.text().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let tracks = j["result"]["tracks"].as_array().cloned().unwrap_or_default();
    log_msg(&app, &format!("PLAYLIST detail id={id} tracks={}", tracks.len()));
    let list: Vec<serde_json::Value> = tracks
        .into_iter()
        .map(|t| {
            let artist = t["ar"]
                .as_array()
                .map(|ar| {
                    ar.iter()
                        .filter_map(|a| a["name"].as_str())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default();
            serde_json::json!({
                "id": t["id"],
                "name": t["name"],
                "artist": artist,
                "album": t["al"]["name"],
            })
        })
        .collect();
    Ok(serde_json::json!(list))
}

/// 原生级搜索: weapi cloudsearch(官方客户端/网页同款排序), 返回含热度(pop)/VIP标记(fee)/时长(dt)等丰富字段
#[tauri::command]
fn netease_search(app: tauri::AppHandle, state: tauri::State<'_, LoginState>, kw: String, limit: Option<u32>) -> Result<serde_json::Value, String> {
    let cookie = login_info(&state).map(|i| i.cookie);
    let csrf = cookie.as_deref().map(extract_csrf).unwrap_or_default();
    let lim = limit.unwrap_or(30);
    let j = weapi_post(
        "/weapi/cloudsearch/get/web",
        serde_json::json!({"s": kw, "type": 1, "limit": lim, "offset": 0, "total": true, "csrf_token": csrf}),
        cookie.as_deref(),
    )?;
    let code = j["code"].as_i64().unwrap_or(-1);
    let songs = j["result"]["songs"].as_array().cloned().unwrap_or_default();
    log_msg(&app, &format!("SEARCH cloudsearch kw={} code={} count={}", kw, code, songs.len()));
    if code != 200 {
        return Err(format!("cloudsearch code={code}"));
    }
    let list: Vec<serde_json::Value> = songs
        .into_iter()
        .map(|t| {
            let artist = t["ar"]
                .as_array()
                .map(|ar| {
                    ar.iter()
                        .filter_map(|a| a["name"].as_str())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default();
            serde_json::json!({
                "id": t["id"],
                "name": t["name"],
                "artist": artist,
                "album": t["al"]["name"],
                "dt": t["dt"],
                "pop": t["pop"],
                "fee": t["fee"],
                "alia": t["alia"],
            })
        })
        .collect();
    Ok(serde_json::json!(list))
}

/// 取可播直链, 支持音质偏好: standard(128k)/exhigh(320k)/lossless(无损)/hires(Hi-Res)/jymaster(母带)
/// 返回 {"url","level","br","size"} —— level 为实际命中的档位(可能因版权/会员等级降级)
#[tauri::command]
fn netease_song_url(app: tauri::AppHandle, state: tauri::State<'_, LoginState>, id: i64, level: Option<String>) -> Result<serde_json::Value, String> {
    let cookie = login_info(&state).map(|i| i.cookie);
    let ck = cookie.as_deref();
    let want = level.unwrap_or_else(|| "exhigh".to_string());
    // 偏好降级链: 高档位拿不到时逐级回退, 保证有歌可播
    let prefs: Vec<&str> = match want.as_str() {
        "standard" => vec!["standard"],
        "lossless" => vec!["lossless", "exhigh", "standard"],
        "hires" => vec!["hires", "lossless", "exhigh", "standard"],
        "jymaster" => vec!["jymaster", "hires", "lossless", "exhigh", "standard"],
        _ => vec!["exhigh", "standard"],
    };
    // 主路径: eapi(官方客户端身份)。weapi(os_web)对"需客户端播放"的 VIP 曲只给受限 jdymusic 直链(CDN 403),
    // eapi 换取可拉取的 ymusic / jd-musicrep-ts 直链(Node 实测返回真实 MP3)。
    let mut fallback = String::new();
    for level in &prefs {
        let data = serde_json::json!({"ids": format!("[{id}]"), "level": level, "encodeType": "flac"});
        match eapi_post("/api/song/enhance/player/url/v1", ck, data) {
            Ok(j) => {
                let url = j["data"][0]["url"].as_str().unwrap_or("").to_string();
                if url.is_empty() {
                    log_msg(&app, &format!("SONGURL eapi id={id} level={level} code={} emptyUrl", j["code"].as_i64().unwrap_or(-1)));
                    continue;
                }
                let host = url::Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.to_string()))
                    .unwrap_or_default();
                let real_level = j["data"][0]["level"].as_str().unwrap_or(level).to_string();
                let br = j["data"][0]["br"].as_i64().unwrap_or(0);
                log_msg(
                    &app,
                    &format!("SONGURL eapi id={id} want={want} level={real_level} code={} host={} br={br} urlLen={}", j["code"].as_i64().unwrap_or(-1), host, url.len()),
                );
                // eapi(客户端身份)取得的直链均已授权可播——即便是 jdymusic 路径(实测 206 audio/mpeg)。
                return Ok(serde_json::json!({"url": url, "level": real_level, "br": br}));
            }
            Err(e) => log_msg(&app, &format!("SONGURL eapi id={id} level={level} err={e}")),
        }
    }
    // eapi 全失败时回退 weapi v1(免费曲/部分曲目仍可用)
    if fallback.is_empty() {
        log_msg(&app, &format!("SONGURL id={id} eapi 未得可播直链, 回退 weapi"));
        for level in ["exhigh", "standard"] {
            if let Ok(j) = weapi_post(
                "/weapi/song/enhance/player/url/v1",
                serde_json::json!({"ids": format!("[{id}]"), "level": level, "encodeType": "flac", "csrf_token": ""}),
                ck,
            ) {
                let url = j["data"][0]["url"].as_str().unwrap_or("").to_string();
                if !url.is_empty() && !url.contains("jdymusic") {
                    log_msg(&app, &format!("SONGURL weapi id={id} level={level} urlLen={}", url.len()));
                    return Ok(serde_json::json!({"url": url, "level": level}));
                }
                if fallback.is_empty() && !url.is_empty() {
                    fallback = url;
                }
            }
        }
    }
    if fallback.is_empty() {
        Err("no url".into())
    } else {
        log_msg(&app, &format!("SONGURL id={id} 仅有 jdymusic 高级直链(CDN 403, 无法播放)"));
        Ok(serde_json::json!({"url": fallback, "level": "unknown"}))
    }
}

// ==================== QQ 音乐(Cookie 导入登录) ====================
// 账号导入: 用户在 https://y.qq.com 登录后复制整段 Cookie(F12→Application→Cookies)粘贴导入。
// 关键 cookie: uin(微信登录为 wxuin + login_type=2) / qqmusic_key 或 qm_keyst(播放凭证) / p_skey。
// 歌单: c.y.qq.com 的 fcg 接口; 取链: u.y.qq.com musicu.fcg vkey.GetVkeyServer(VIP 曲需 VIP cookie)。

struct QqInfo {
    cookie: String,
    uin: String,
    key: String,
    nickname: String,
}
struct QqState(std::sync::Mutex<Option<QqInfo>>);

fn qq_info(state: &tauri::State<'_, QqState>) -> Option<QqInfo> {
    state.0.lock().unwrap().as_ref().map(|i| QqInfo {
        cookie: i.cookie.clone(),
        uin: i.uin.clone(),
        key: i.key.clone(),
        nickname: i.nickname.clone(),
    })
}

/// 解析 QQ cookie 串: 返回 (uin, qqmusic_key)
/// 容错: 允许带 "Cookie:" 前缀(Network 面板整行复制)/键名空白; uin 按 login_type 与多键名兜底
fn qq_parse_cookie(cookie: &str) -> (String, String) {
    let mut c = cookie.trim();
    // 剥离 "Cookie:" 前缀(从 Network 面板连 header 名一起复制的情况)
    if c.len() > 7 && c[..7].eq_ignore_ascii_case("cookie:") {
        c = c[7..].trim();
    }
    let mut map = std::collections::HashMap::new();
    for kv in c.split(';') {
        let kv = kv.trim();
        if let Some(eq) = kv.find('=') {
            map.insert(kv[..eq].trim().to_lowercase(), kv[eq + 1..].trim().to_string());
        }
    }
    let digits = |s: &str| -> String { s.chars().filter(|c| c.is_ascii_digit()).collect() };
    let login_type = map.get("login_type").map(|s| s.as_str()).unwrap_or("");
    let raw_uin = if login_type == "2" {
        map.get("wxuin").cloned().unwrap_or_default()
    } else {
        // 依次兜底: uin → p_uin → wxuin
        ["uin", "p_uin", "wxuin"]
            .iter()
            .find_map(|k| map.get(*k).cloned())
            .unwrap_or_default()
    };
    let uin = digits(&raw_uin);
    let key = map
        .get("qqmusic_key")
        .or_else(|| map.get("qm_keyst"))
        .cloned()
        .unwrap_or_default();
    (uin, key)
}

/// GET c.y.qq.com 系 fcg 接口(JSON)
fn qq_fcg_get(path: &str, query: &[(&str, String)], cookie: &str) -> Result<serde_json::Value, String> {
    let qs: Vec<(String, String)> = query.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    let resp = get_client()
        .get(format!("https://c.y.qq.com/{path}"))
        .query(&qs)
        .header("Referer", "https://y.qq.com/portal/profile.html")
        .header("Cookie", cookie)
        .send()
        .map_err(|e| e.to_string())?;
    let text = resp.text().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("fcg json 解析失败: {e} body={}", &text[..text.len().min(200)]))
}

/// 调 u.y.qq.com musicu.fcg(统一网关)。
/// 注意: POST 表单带 data 会被网关拒(500001), 必须用 GET + 完整 query 参数(实测验证)
fn qq_musicu(cookie: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
    let (uin, _) = qq_parse_cookie(cookie);
    let url = format!(
        "https://u.y.qq.com/cgi-bin/musicu.fcg?-=getplaysongvkey&g_tk=5381&loginUin={uin}&hostUin=0&format=json&inCharset=utf8&outCharset=utf-8&notice=0&platform=yqq.json&needNewCode=0&data={}",
        urlencoding::encode(&body.to_string())
    );
    let resp = get_client()
        .get(url)
        .header("Referer", "https://y.qq.com/")
        .header("Cookie", cookie)
        .send()
        .map_err(|e| e.to_string())?;
    let text = resp.text().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("musicu json 解析失败: {e} body={}", &text[..text.len().min(200)]))
}

/// 拉取用户创建的歌单(同时用作登录态校验; data.hostname 即昵称)
fn qq_fetch_created(cookie: &str, uin: &str) -> Result<serde_json::Value, String> {
    qq_fcg_get(
        "rsc/fcgi-bin/fcg_user_created_diss",
        &[
            ("hostUin", "0".into()),
            ("hostuin", uin.to_string()),
            ("sin", "0".into()),
            ("size", "200".into()),
            ("g_tk", "5381".into()),
            ("loginUin", "0".into()),
            ("format", "json".into()),
            ("inCharset", "utf8".into()),
            ("outCharset", "utf-8".into()),
            ("notice", "0".into()),
            ("platform", "yqq.json".into()),
            ("needNewCode", "0".into()),
        ],
        cookie,
    )
}

#[tauri::command]
fn qq_set_cookie(app: tauri::AppHandle, state: tauri::State<'_, QqState>, cookie: String) -> Result<serde_json::Value, String> {
    let cookie = cookie.trim().to_string();
    if cookie.is_empty() {
        return Err("cookie 为空".into());
    }
    let (uin, key) = qq_parse_cookie(&cookie);
    if uin.is_empty() {
        // 列出识别到的键名(不记值), 便于用户自查是否复制完整
        let keys: Vec<String> = cookie
            .split(';')
            .filter_map(|kv| kv.split('=').next())
            .map(|k| k.trim().trim_start_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_').to_string())
            .filter(|k| !k.is_empty())
            .take(20)
            .collect();
        log_msg(&app, &format!("QLOGIN uin 未识别, keys={:?}", keys));
        return Err(format!(
            "未找到 uin/wxuin（识别到 {} 个键: {}）。请用 F12→Network→任一 y.qq.com 请求→Headers→Cookie 整行复制（document.cookie 拿不到 HttpOnly 的 uin）",
            keys.len(),
            keys.join(", ")
        ));
    }
    log_msg(&app, &format!("QLOGIN set_cookie uin={uin} keyLen={}", key.len()));
    let j = qq_fetch_created(&cookie, &uin).map_err(|e| format!("校验失败: {e}"))?;
    let code = j["code"].as_i64().unwrap_or(-1);
    if code != 0 {
        return Err(format!("cookie 无效或已过期(code={code})"));
    }
    let nickname = j["data"]["hostname"].as_str().unwrap_or("").to_string();
    log_msg(&app, &format!("QLOGIN ok uin={uin} nick={nickname}"));
    *state.0.lock().unwrap() = Some(QqInfo { cookie, uin: uin.clone(), key, nickname: nickname.clone() });
    Ok(serde_json::json!({"uin": uin, "nickname": nickname}))
}

/// 一键导入: 读取 %LOCALAPPDATA%\local.claudemusic\qq-cookie.txt(由浏览器辅助提取写入), 成功后删除文件
#[tauri::command]
fn qq_import_cookie_file(app: tauri::AppHandle, state: tauri::State<'_, QqState>) -> Result<serde_json::Value, String> {
    let path = std::env::var("LOCALAPPDATA")
        .map(|d| format!(r"{d}\local.claudemusic\qq-cookie.txt"))
        .map_err(|_| "无法定位应用数据目录".to_string())?;
    let cookie = std::fs::read_to_string(&path)
        .map_err(|_| "未找到 Cookie 导入文件（可改用 qlogin <cookie> 手动粘贴）".to_string())?;
    let keep = cookie.trim().to_string();
    let mut r = qq_set_cookie(app, state, cookie)?;
    let _ = std::fs::remove_file(&path); // 导入后删除, 不落盘存凭证
    // 回传 cookie 供前端 localStorage 持久化(与手动粘贴路径一致, 仅存本机)
    r.as_object_mut().map(|m| m.insert("cookie".into(), serde_json::Value::String(keep)));
    Ok(r)
}

#[tauri::command]
fn qq_status(state: tauri::State<'_, QqState>) -> Result<Option<serde_json::Value>, String> {
    Ok(qq_info(&state).map(|i| serde_json::json!({"uin": i.uin, "nickname": i.nickname})))
}

#[tauri::command]
fn qq_logout(state: tauri::State<'_, QqState>) -> Result<(), String> {
    *state.0.lock().unwrap() = None;
    Ok(())
}

#[tauri::command]
fn qq_user_playlists(app: tauri::AppHandle, state: tauri::State<'_, QqState>) -> Result<serde_json::Value, String> {
    let info = qq_info(&state).ok_or("QQ 未登录")?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    // 创建的歌单(dirid=201 即「我喜欢」)
    if let Ok(j) = qq_fetch_created(&info.cookie, &info.uin) {
        for p in j["data"]["disslist"].as_array().cloned().unwrap_or_default() {
            let tid = p["tid"].as_i64().unwrap_or(0);
            if tid == 0 {
                continue;
            }
            out.push(serde_json::json!({
                "id": tid,
                "name": p["diss_name"].as_str().unwrap_or("未命名"),
                "trackCount": p["song_cnt"].as_i64().unwrap_or(0),
                "subscribed": false,
                "src": "qq",
            }));
        }
    }
    // 收藏的歌单
    if let Ok(j) = qq_fcg_get(
        "fav/fcgi-bin/fcg_get_profile_order_asset.fcg",
        &[
            ("ct", "20".into()),
            ("cid", "205360956".into()),
            ("userid", info.uin.clone()),
            ("reqtype", "3".into()),
            ("sin", "0".into()),
            ("ein", "200".into()),
            ("format", "json".into()),
            ("outCharset", "utf-8".into()),
        ],
        &info.cookie,
    ) {
        for p in j["data"]["cdlist"].as_array().cloned().unwrap_or_default() {
            let tid = p["dissid"].as_i64().unwrap_or(0);
            if tid == 0 {
                continue;
            }
            out.push(serde_json::json!({
                "id": tid,
                "name": p["diss_name"].as_str().or_else(|| p["dissname"].as_str()).unwrap_or("未命名"),
                "trackCount": p["song_cnt"].as_i64().or_else(|| p["songnum"].as_i64()).unwrap_or(0),
                "subscribed": true,
                "src": "qq",
            }));
        }
    }
    log_msg(&app, &format!("QPLAYLIST user count={}", out.len()));
    Ok(serde_json::json!(out))
}

#[tauri::command]
fn qq_playlist_detail(app: tauri::AppHandle, state: tauri::State<'_, QqState>, id: i64) -> Result<serde_json::Value, String> {
    let info = qq_info(&state).ok_or("QQ 未登录")?;
    let j = qq_fcg_get(
        "qzone/fcg-bin/fcg_ucc_getcdinfo_byids_cp.fcg",
        &[
            ("type", "1".into()),
            ("utf8", "1".into()),
            ("disstid", id.to_string()),
            ("loginUin", "0".into()),
            ("format", "json".into()),
        ],
        &info.cookie,
    )?;
    let songs = j["cdlist"][0]["songlist"].as_array().cloned().unwrap_or_default();
    log_msg(&app, &format!("QPLAYLIST detail id={id} tracks={}", songs.len()));
    // 字段名以 Node 探针实测为准: songmid/songname/albumname/strMediaMid/songid
    let list: Vec<serde_json::Value> = songs
        .into_iter()
        .filter_map(|t| {
            let mid = t["songmid"].as_str().or_else(|| t["mid"].as_str())?.to_string();
            let artist = t["singer"]
                .as_array()
                .map(|ar| {
                    ar.iter()
                        .filter_map(|a| a["name"].as_str())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default();
            Some(serde_json::json!({
                "mid": mid,
                "mediaMid": t["strMediaMid"].as_str().unwrap_or(""),
                "id": t["songid"].as_i64().or_else(|| t["id"].as_i64()).unwrap_or(0),
                "name": t["songname"].as_str().or_else(|| t["name"].as_str()).unwrap_or("未知曲目"),
                "artist": artist,
                "album": t["albumname"].as_str().or_else(|| t["album"]["name"].as_str()).unwrap_or(""),
                "dt": t["interval"].as_i64().unwrap_or(0) * 1000,
                "src": "qq",
                // 可用音质(字节数>0 即存在该格式): 供列表标注 SQ/HQ 与取链参考
                "hasHQ": t["size320"].as_i64().unwrap_or(0) > 0,
                "hasSQ": t["sizeflac"].as_i64().unwrap_or(0) > 0 || t["sizeape"].as_i64().unwrap_or(0) > 0,
            }))
        })
        .collect();
    Ok(serde_json::json!(list))
}

/// QQ 取链: vkey.GetVkeyServer, 按音质偏好给候选文件名, 命中首个非空 purl
/// filename = 前缀+songmid+media_mid+扩展名(strMediaMid 由歌单详情提供; 缺失时退化为 songmid)
#[tauri::command]
fn qq_song_url(app: tauri::AppHandle, state: tauri::State<'_, QqState>, mid: String, level: Option<String>, media_mid: Option<String>) -> Result<serde_json::Value, String> {
    let info = qq_info(&state).ok_or("QQ 未登录")?;
    let want = level.unwrap_or_else(|| "exhigh".into());
    let mm = media_mid.filter(|s| !s.is_empty()).unwrap_or_else(|| mid.clone());
    // (文件名前缀, 扩展名, 对应音质档位)
    let cands: Vec<(&str, &str, &str)> = match want.as_str() {
        "standard" => vec![("M500", ".mp3", "standard"), ("C400", ".m4a", "standard")],
        "lossless" | "hires" | "jymaster" => vec![
            ("F000", ".flac", "lossless"),
            ("M800", ".mp3", "exhigh"),
            ("M500", ".mp3", "standard"),
        ],
        _ => vec![("M800", ".mp3", "exhigh"), ("M500", ".mp3", "standard")],
    };
    let mut filenames: Vec<String> = Vec::new();
    let mut tags: Vec<&str> = Vec::new();
    for (pre, ext, lv) in &cands {
        filenames.push(format!("{pre}{mid}{mm}{ext}")); // 标准写法 songmid+strMediaMid
        filenames.push(format!("{pre}{mid}{mid}{ext}")); // 兼容 media_mid=songmid
        tags.push(lv);
        tags.push(lv);
    }
    let body = serde_json::json!({
        "req_0": {
            "module": "vkey.GetVkeyServer",
            "method": "CgiGetVkey",
            "param": {
                "filename": filenames,
                "guid": "2796982635",
                "songmid": vec![mid.clone(); filenames.len()],
                "songtype": vec![0; filenames.len()],
                "uin": info.uin,
                "loginflag": 1,
                "platform": "20"
            }
        },
        "comm": {
            "uin": info.uin,
            "format": "json",
            "ct": 19,
            "cv": 0,
            "authst": info.key
        }
    });
    let j = qq_musicu(&info.cookie, body)?;
    let code = j["req_0"]["code"].as_i64().unwrap_or(-1);
    let infos = j["req_0"]["data"]["midurlinfo"].as_array().cloned().unwrap_or_default();
    let sips = j["req_0"]["data"]["sip"].as_array().cloned().unwrap_or_default();
    let domain = sips
        .iter()
        .filter_map(|s| s.as_str())
        .find(|s| !s.starts_with("http://ws"))
        .or_else(|| sips.first().and_then(|s| s.as_str()))
        .unwrap_or("https://isure.stream.qqmusic.qq.com/")
        .to_string();
    for (i, it) in infos.iter().enumerate() {
        let purl = it["purl"].as_str().unwrap_or("");
        if !purl.is_empty() {
            let lv = tags.get(i).copied().unwrap_or("exhigh");
            log_msg(&app, &format!("QSONGURL mid={mid} want={want} hit={lv} idx={i}"));
            return Ok(serde_json::json!({"url": format!("{domain}{purl}"), "level": lv}));
        }
    }
    log_msg(&app, &format!("QSONGURL mid={mid} want={want} 无可用 purl code={code}(非VIP曲或需更高权限)"));
    Err("QQ 未返回可播链接(可能需 VIP 或曲目无版权)".into())
}

/// QQ 歌词: fcg_query_lyric_new 返回 base64 编码的 LRC(及可选翻译 trans); 无需登录
#[tauri::command]
fn qq_lyric(app: tauri::AppHandle, state: tauri::State<'_, QqState>, mid: String) -> Result<serde_json::Value, String> {
    let cookie = qq_info(&state).map(|i| i.cookie).unwrap_or_default();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let url = format!(
        "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg?songmid={mid}&format=json&nobase64=0&g_tk=5381&loginUin=0&hostUin=0&inCharset=utf8&outCharset=utf-8&notice=0&platform=yqq.json&needNewCode=0&pcachetime={ts}"
    );
    let resp = get_client()
        .get(url)
        .header("Referer", "https://y.qq.com/portal/player.html")
        .header("Cookie", cookie)
        .send()
        .map_err(|e| e.to_string())?;
    let text = resp.text().map_err(|e| e.to_string())?;
    let j: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("lyric json 解析失败: {e} body={}", &text[..text.len().min(200)]))?;
    let decode = |k: &str| -> String {
        j[k]
            .as_str()
            .and_then(|s| B64.decode(s).ok())
            .map(|b| {
                // 歌词内容可能是 UTF-8 或 GBK(老歌): 先 UTF-8, 失败按 GBK 解码
                match String::from_utf8(b.clone()) {
                    Ok(s) => s,
                    Err(_) => {
                        let (s, _, _) = encoding_rs::GBK.decode(&b);
                        s.into_owned()
                    }
                }
            })
            .unwrap_or_default()
    };
    let mut lyric = decode("lyric");
    let trans = decode("trans");
    log_msg(&app, &format!("QLYRIC mid={mid} code={} lrcLen={} transLen={}", j["code"].as_i64().unwrap_or(-1), lyric.len(), trans.len()));
    if lyric.is_empty() {
        lyric = trans.clone(); // 纯外文曲: 用翻译兜底
    }
    if lyric.is_empty() {
        return Err("QQ 无歌词".into());
    }
    Ok(serde_json::json!({"lyric": lyric, "trans": trans}))
}

fn main() {
    for var in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"] {
        std::env::remove_var(var);
    }
    tauri::Builder::default()
        .manage(LoginState(std::sync::Mutex::new(None)))
        .manage(QqState(std::sync::Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            app_log,
            netease_qr_key,
            netease_qr_svg,
            netease_qr_check,
            netease_set_cookie,
            netease_logout,
            netease_status,
            netease_user_playlists,
            netease_playlist_detail,
            netease_song_url,
            netease_search,
            qq_set_cookie,
            qq_import_cookie_file,
            qq_status,
            qq_logout,
            qq_user_playlists,
            qq_playlist_detail,
            qq_song_url,
            qq_lyric
        ])
        // 异步协议处理器: 网络请求在独立线程执行, 不阻塞 UI/WebView 线程
        .register_asynchronous_uri_scheme_protocol("proxy", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
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
                responder.respond(handle_proxy(app, target, range));
            });
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
