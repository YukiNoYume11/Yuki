#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! ClaudeMusic Tauri 后端
//! 核心能力: proxy 自定义协议 —— 把网易云 API/音频请求代理为同源请求,
//! 解决 CORS 限制, 让前端 fetch 与 Web Audio 情绪分析(AnalyserNode)正常工作。
//! 登录/歌单/VIP 取链: 直接走 weapi(AES-128-CBC + 原始 RSA)加密接口, Cookie 存进程内状态。

use std::io::{Read, Write};
use tauri::http::{Response, StatusCode};
use tauri::Manager;
use std::time::{SystemTime, UNIX_EPOCH};

// ---- weapi 加密相关依赖 ----
use aes::Aes128;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use cbc::{Decryptor, Encryptor};
use num_bigint::BigUint;
use num_traits::Num;
use rand::Rng;
use qrcode::render::svg;

type Aes128CbcEnc = Encryptor<Aes128>;
type Aes128CbcDec = Decryptor<Aes128>;

const MAX_BODY: u64 = 48 * 1024 * 1024; // 48MB 上限(网易云 mp3 约 3-10MB)

// weapi 标准常量(公开): 首轮 AES key / IV / RSA 公钥
const WEAPI_NONCE: &[u8] = b"0CoJUm6Qyw8W8jud";
const WEAPI_IV: &[u8] = b"0102030405060708";
const RSA_MODULUS: &str = "00e0b509f6259df8642dbc35662901477df22677ec152b5ff68ace615bb7b725152b3ab17a876aea8a5aa76d2e417629ec4ee341f56135fccf695280104e0312ecbda92557c93870114af6c9d05c4f7f0c3685b7a46bee255932575cce10b424d813cfe4875d3e82047b97ddef52741d546b8e289dc6935b3ece0462db0a22b8e7";
const RSA_EXPONENT: u32 = 65537;
const BASE62: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

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

    let client = get_client();

    let mut req = client.get(&target);
    // QQ 音乐接口需要 Referer 才返回数据
    if target.contains("qq.com") {
        req = req.header("Referer", "https://y.qq.com/");
    }
    // 网易 CDN(m70x/m80x.music.126.net)对 VIP 音频有防盗链: 需浏览器 UA(get_client 已设)+ Referer, 否则 403
    if target.contains("126.net") || target.contains("163.com") {
        req = req.header("Referer", "https://music.163.com/");
        // 关键: 网易 VIP 直链与会话绑定——不带登录 Cookie 取流会返回 text/plain 防盗链页(非音频)。
        // 这里把进程内登录态的 Cookie 透传给网易自家 CDN(同信任域, 用户本人账号, 不泄漏第三方)。
        if let Some(st) = app.try_state::<LoginState>() {
            if let Ok(g) = st.inner().0.lock() {
                if let Some(info) = g.as_ref() {
                    req = req.header("Cookie", info.cookie.as_str());
                }
            }
        }
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
            // 音频 CDN 探针: 记录状态/类型/字节数, 排障 VIP 播放(403/格式错误)必读
            if target.contains("126.net") || target.contains("163.com") {
                let mut line = format!(
                    "PROXY music status={status} ct={ct} bytes={} url={}",
                    buf.len(),
                    &target[..target.len().min(90)]
                );
                // 正常音频为数 MB 的 audio/mpeg; 若响应很小则是防盗链/错误文本, dump 前 300 字定位
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

#[tauri::command]
fn netease_song_url(app: tauri::AppHandle, state: tauri::State<'_, LoginState>, id: i64) -> Result<String, String> {
    let cookie = login_info(&state).map(|i| i.cookie);
    let ids = format!("[{id}]");
    let j = weapi_post(
        "/weapi/song/enhance/player/url",
        serde_json::json!({"ids": ids, "br": 320000}),
        cookie.as_deref(),
    )?;
    let code = j["code"].as_i64().unwrap_or(-1);
    let url = j["data"][0]["url"].as_str().unwrap_or("").to_string();
    let host = url::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();
    log_msg(
        &app,
        &format!("SONGURL id={id} code={code} urlLen={} host={} hasCookie={}", url.len(), host, cookie.is_some()),
    );
    if url.is_empty() {
        Err("no url".into())
    } else {
        Ok(url)
    }
}

fn main() {
    for var in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"] {
        std::env::remove_var(var);
    }
    tauri::Builder::default()
        .manage(LoginState(std::sync::Mutex::new(None)))
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
            netease_song_url
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
