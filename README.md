# YukiMusic

<p align="center">
  <img src="app/src-tauri/icons/icon.png" width="128" alt="YukiMusic">
</p>

<p align="center">
  一个用 <b>Tauri + Rust + WebView2</b> 写的桌面音乐播放器 —— 终端风格的壳，双平台在线曲库，会跟着歌的情绪呼吸的字幕。
</p>

> 个人作品，非商业用途。代码公开仅供学习参考。

---

## 特性

**双平台曲库**
- 网易云 + QQ 音乐双源搜索，结果合并排序（跨字段相关度、拼写容错）
- QQ 音乐通过 Cookie 登录后可直接播放 VIP 曲目，网易云支持扫码登录
- 侧边栏聚合两家「我的歌单 / 收藏歌单」，一键整单导入

**音质**
- 五档可切：`standard` 128k / `exhigh` 320k / `lossless` 无损 / `hires` Hi-Res / `jymaster` 母带
- 双平台共用同一档位设置，拿不到时逐级降级
- 播放条实时显示实际命中档位（`128k` / `320k` / `SQ` / `Hi-Res` / `母带`）
- QQ 列表标注 `SQ` / `HQ`，一眼看出哪首有高解析

**歌词**
- 网易云 LRC、QQ 音乐歌词（服务端 base64 解码，UTF-8/GBK 双编码兼容）
- 本地音频自动读取同目录同名 `.lrc`
- 字幕层独立渲染，120ms 同步精度，支持 `[offset:]`、多时间戳与手动偏移校准

**情绪引擎**
- 实时频谱分析（Web Audio AnalyserNode）→ 判定 欢快/平静/忧郁/高潮 四种歌内情绪
- 情绪驱动字幕动效、吉祥物动画与界面配色（可关闭：`mood auto`）

**主题**
- `dark`：Claude 终端风（暖橙色 + 等宽字体 + 像素螃蟹吉祥物）
- `transfish`：跨性别旗粉蓝白 × 鱼板 🍥，完全独立的配色与文案
- 主题即一组 CSS 变量覆盖，扩展新主题只需加一个 `html[data-theme]` 块

**播放控制**
- 顺序循环 / 随机循环 / 单曲循环三态切换
- 倍速 0.5×–2×、音量、快进快退、队列跳转
- **本地音源导入**：`import <路径>`，支持 mp3 / flac / m4a / ogg / wav / aac

**命令行优先**
- 底部命令行 + `help` 全量手册；所有能力都有命令入口
- 内置黑匣子日志与 `diag` 环境自检，出问题能自己说话

---

## 技术架构

```
┌──────────────────────────────────────────────────────┐
│  WebView2 (UI = 单文件 webui/index.html, 原生 JS)      │
│  ├─ 命令行解析 / 渲染列表 / 字幕层 / 情绪引擎           │
│  └─ <audio> ── 同源代理 ──┐                           │
├──────────────────────────┼───────────────────────────┤
│  Tauri 壳 (Rust)          │                           │
│  ├─ proxy:// 自定义协议 ───┘  URL 白名单 + Range 转发   │
│  │    ├─ 网易云 eapi/weapi 加密                       │
│  │    ├─ QQ 音乐 musicu / vkey 取链                   │
│  │    └─ local:// 本地文件读盘                        │
│  ├─ 二维码登录 / Cookie 登录 / 歌单 / 歌词             │
│  └─ 黑匣子日志 + NSIS 打包                            │
└──────────────────────────────────────────────────────┘

---

## 目录结构

```
YukiMusic/
├── webui/                    # U1 纯前端（浏览器可直接打开预览）
│   └── index.html            # 全部 UI/逻辑/主题/命令
├── app/                      # U2 Tauri 壳
│   ├── src-tauri/
│   │   ├── src/main.rs       # 代理、登录、取链、歌词、日志、打包配置
│   │   ├── icons/            # 雪花图标（多尺寸 ICO）
│   │   └── tauri.conf.json
│   └── package.json
├── docs/UNITS.md             # 单元划分与回退手册
└── README.md

---

## 构建

**前置**
- Windows 10/11 x64
- [Rust](https://rustup.rs/)（stable，MSVC toolchain）
- Node.js 18+（仅打包用）
- WebView2 Runtime（Win11 自带；缺失时安装包会自动下载）

**开发运行**

```bash
cd app/src-tauri
cargo build            # debug 版（带控制台窗口，便于看 stderr）
```

**正式构建**

```bash
cd app
npm install
npx tauri build        # 产出 release exe + NSIS 安装包
```

产物：
- `app/src-tauri/target/release/yukimusic.exe`（绿色版）
- `app/src-tauri/target/release/bundle/nsis/YukiMusic_<版本>_x64-setup.exe`（安装包）

> release 构建走 `windows` 子系统，不会弹控制台窗口；lto + strip 后约 10.9MB。

---

## 命令手册

输入 `help` 可在应用内查看完整手册。

### 播放
| 命令 | 说明 |
|---|---|
| `play <关键词>` | 双平台搜索并播放 |
| `import <路径>` | 导入本地音频（不带参数看流程） |
| `radio <名称>` | 播放电台 |
| `next` / `prev` | 下一首 / 上一首 |
| `ff [秒]` / `rw [秒]` | 快进 / 快退（默认 10 秒） |
| `speed [0.5-2]` | 倍速 |
| `vol 0-100` | 音量 |

### 队列与歌单
| 命令 | 说明 |
|---|---|
| `mode [seq\|shuffle\|one]` | 顺序循环 / 随机循环 / 单曲循环 |
| `playlist <url\|id>` | 导入网易云歌单 |
| `my` | 我的网易云歌单 |
| `qmy` | 我的 QQ 歌单 |
| `demo` | 内置演示歌单 |

### 音质与显示
| 命令 | 说明 |
|---|---|
| `quality [standard\|exhigh\|lossless\|hires\|jymaster]` | 音质档位（双平台通用） |
| `lyric on\|off\|offset ±秒` | 歌词开关 / 手动校准 |
| `theme [dark\|transfish]` | 主题切换 |
| `mood auto\|欢快\|平静\|忧郁\|高潮` | 情绪引擎 |

### 账号
| 命令 | 说明 |
|---|---|
| `login` | 网易云扫码登录（`login cookie <值>` 可直接粘贴） |
| `logout` | 退出网易云 |
| `qlogin` | QQ 一键导入（读应用数据目录下的导入文件）；`qlogin <cookie>` 直接粘贴 |
| `qlogout` | 退出 QQ |

### 系统
| 命令 | 说明 |
|---|---|
| `diag` | 环境自检（网络 / 搜索 / CDN / 登录态） |
| `logs` | 打开日志目录 |
| `clear` | 清屏 |
| `help` | 命令手册 |

---

## QQ 音乐登录（Cookie 导入）

1. 浏览器打开 `y.qq.com`，扫码登录
2. `F12` → **Network** → 刷新页面 → 点任一 `y.qq.com` 请求
3. 复制 **Request Headers** 里 `Cookie:` 整行的值
4. 应用内输入 `qlogin <粘贴的 cookie>`

> 不要从 Console 复制 `document.cookie` —— 拿不到 HttpOnly 的 `uin`/`qqmusic_key`。
> Cookie 仅存本机 localStorage，只在请求时使用。

---

## 常见问题

**QQ 歌有进度条但没声音？**
v0.5.6 起音频已全部走同源代理（跨域会被 Web Audio 静音），若仍复现请把 `logs` 目录里的日志发出来。

**点歌单提示「为空或无法读取」？**
v0.5.5 的字段名 bug，已修复。若复现请确认用的是最新版本。

**日志在哪？**
`%LOCALAPPDATA%\local.claudemusic\logs\`，或应用内输入 `logs` 直接打开。

**其他电脑安装需要什么？**
Windows 10/11 x64，WebView2（缺了安装包会自动联网下载），免管理员权限（装到用户目录）。

**APE 格式能播吗？**
不能，WebView2 不支持 APE。请转成 FLAC。

---

## 免责声明

- 本项目为个人学习与自用作品，**不作任何商业用途**
- 音源均来自公开接口，版权归各平台与版权方所有，请支持正版
- 请勿用于任何违反平台服务条款的用途，使用风险自负

---

<p align="center"><sub>Made by Yuki · エムティーエフの空·夢ちゃんが寝たくない</sub></p>
