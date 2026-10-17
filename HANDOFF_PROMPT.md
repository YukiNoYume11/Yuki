# ClaudeMusic 项目交接提示词（复制此全文作为下一场对话的开场消息）

我正在开发 **ClaudeMusic**——一个 Claude Code 终端风格的桌面音乐播放器，项目位于 `D:\ClaudeMusic`。你之前已参与开发到 **v0.4.21**（网易云登录 + VIP 播放 + 侧边栏歌单均已完成并实测通过），以下是完整上下文，请以此为基础继续。

## 项目概况

- **技术栈**：Tauri 2（Rust 后端 `app/src-tauri/`）+ 纯 HTML/CSS/JS 单文件前端（`webui/index.html`，约 900 行，无框架无构建）
- **设计**：Claude Code 终端风格（暗色 #1F1E1D / 赭橙 #D97757 / 等宽字体 / `>` 命令提示符 / 像素小螃蟹吉祥物）
- **核心功能**（全部可用）：网易云搜索+播放（免费曲目外链）、**网易云登录（扫码为主+粘贴Cookie兜底）→ VIP曲目播放（eapi客户端取链）+侧边栏「我的歌单」+歌单URL/ID导入**、三级模糊搜索（主搜→suggest→QQ兜底+相关度排序）、Radio Browser 电台兜底、LRC 歌词字幕层（情绪词典染色）、频谱均衡器（Web Audio 真实分析+模拟降级）、Claude 像素形象浮动层（走位交互+右上角气泡）、情绪引擎（歌内相对能量归一化：高潮/欢快/平静/忧郁）、命令行（↑历史/Tab补全/光标竖线跟随/失焦隐藏）、快进快退±10s/倍速0.5-2.0x、方向键绑定（单击±5s/长按→2倍速/长按←持续快退）、设置 localStorage 持久化、`diag` 自检命令、黑匣子日志
- **版本管理**：git 仓库在 D:\ClaudeMusic，tag v0.1-webui → **v0.4.21**（最新里程碑；具体 HEAD 见 `git log --oneline -1`）。版本号统一 0.4.6（Cargo.toml/tauri.conf.json/package.json）。**git 不在系统 PATH**，用便携版：`C:\Users\Yuki\.workbuddy\binaries\PortableGit\versions\1.3.0\cmd\git.exe`
- **命令**：play <词> / radio <名> / next / prev / pause / vol 0-100 / mood auto|欢快|平静|忧郁|高潮 / lyric on|off / ff|rw [秒] / speed [倍速] / login [cookie 值] / logout / my / playlist <url|id> / demo / diag / help / clear
- **构建**：`cd D:\ClaudeMusic\app\src-tauri && export RUSTUP_HOME="D:\ClaudeMusic\toolchain\rustup" CARGO_HOME="D:\ClaudeMusic\toolchain\cargo" PATH="/d/ClaudeMusic/toolchain/cargo/bin:$PATH" && cargo build`（rsproxy 镜像已配好）。产物 target/debug/claudemusic.exe；测试用副本 D:\ClaudeMusic\claudemusic-test.exe（用户双击运行的那个）

## 架构关键点（改动前必读）

1. **代理协议**：Rust 自定义协议 `proxy`，前端经 `https://proxy.localhost/?u=<urlencode目标>` 转发网易云/QQ/CDN 请求。支持 HTTP Range/206（seek 必需）、域名白名单精确/点分匹配（163.com/126.net/qq.com）、QQ 注入 Referer、OnceLock 复用 reqwest Client、48MB 上限、不转发 Content-Length（Tauri 自动设置）。**必须 useHttpsScheme:true**（WebView2 Windows 下 https）
2. **启动时清除代理环境变量**（main.rs）：WorkBuddy 终端注入 http_proxy/https_proxy=127.0.0.1:61255（其内部代理），WebView2 遵循后所有 fetch 永久挂起——这是 v0.4.8 修的头号玄学问题，勿删
3. **黑匣子日志**：Rust app_log 命令 → `%LOCALAPPDATA%\local.claudemusic\logs\claudemusic.log`（UTF-8+BOM、2MB 轮转）；另有 proxy.log 记录代理请求。前端钩子：启动自检（搜索/CDN/QQ 三通道）、AUDIO 事件、LRC、LOAD、ANALYSER tainted、LYRIC-CALL/FETCHLRC 探针。**排障第一步永远是读这个日志**
4. **布局**：`.viz-strip`（固定 118px，频谱+字幕层）位于输出区与播放条之间——不可移回可滚动区（曾因 absolute bottom:0 滚出视野），字幕层渐变已移到条带背景（曾遮挡频谱）
5. **歌词**：仅切歌时 loadLyrics 一次（playing 事件不得触发）；syncLrc 二分查找（支持回退 seek）；染色用 ~80 词情绪词典（k1/k2/k3 循环，只有命中词上色）
6. **情绪**：歌内相对能量（rms min/max/avg 统计，单帧超均值 4 倍截断去极值）。阈值已两次下调（v0.4.12、v0.4.21）使变色更灵敏：rel>0.55 且 onsets>1 → 高潮 / rel>0.32 或 rms>1.1×均值 → 欢快 / rel<0.42 且 centroid<0.34 → 忧郁；前 2s 热身。节流：情绪 1.2s 稳定窗、气泡间隔 2.5s、闲聊 55s 窗 25% 概率
7. **健壮性**：loadTrack 自动跳过无音源曲目（QQ 未登录）；failStreak 连播失败计数防死循环（连续 min(5,队列) 首失败停止）；tainted 需连续 3s 全零才判污染且每 60 帧嗅探可恢复；moodLoop 仅播放时跑 rAF
8. **CSP**：tauri.conf 的 csp 当前为 **null**（调试期间临时禁用；基础 CSP 策略在 v0.4.6 有过可用版本，稳定后应恢复）
9. **mediaSession 已移除**（怀疑与 WebView2 155 崩溃相关，媒体键功能暂缓）
10. **登录/VIP取链（v0.4.11–v0.4.21，关键）**：
    - **weapi vs eapi 是本项目最重要的网络知识**。weapi(AES-CBC+RSA, os_web 网页身份)用于登录/歌单/搜索；但 **VIP「需客户端播放」曲目的取链必须用 eapi**(AES-128-ECB, key=`e82ckenh8dichen8`, 官方客户端身份 os=pc)。weapi 取链对这类曲只返回受限 `jdymusic/obj` 直链, CDN 一律 403(4种请求头变体全试遍仍 403); eapi 取链返回**已授权**直链(ymusic / jd-musicrep-ts, 甚至 jdymusic 也可播), 实测 206 audio/mpeg + ID3 头。
    - eapi 加密: `params = AES-128-ECB("{uri}-36cd479b6b5-{json}-36cd479b6b5-{md5(nobody{uri}use{json}md5forencrypt)}")` → **大写hex**; POST `interface.music.163.com/eapi/{uri去掉/api/}`; 客户端 header 对象(os/appver/deviceId/channel/MUSIC_U…)同时注入加密体与 Cookie; UA=iPhone NeteaseMusic; `e_r:false` 取明文JSON响应。serde_json 字段序为字母序(BTreeMap), 与JS插入序不同但无妨(服务端按解密后的text重算digest自洽)。
    - **登录态**: `/api/nav` 已废弃(返回 code=404「接口未找到」, 明文GET与linux-forward都一样); 现役校验端点是 `/weapi/user/info`(301=未登录/200=已登录), uid 在 `userPoint.userId`。扫码 8821=易盾风控「需行为验证码」, 对策=unikey响应的匿名Set-Cookie存入QR_SESSION供轮询复用同一会话 + 前端遇8821自动换新码(连续3次引导粘贴Cookie)。Cookie存进程内Mutex+localStorage(`cm_music_u`)。
    - **代理取音频流**: 绝不向 126.net CDN 透传 .163.com 的 MUSIC_U(跨域Cookie被Tengine边缘403); 用浏览器UA(get_client)+Referer即可; 403时自动A/B变体(https/去Referer/去Range)。

## 踩过的坑（重要教训，勿重蹈）

- WebView2 自定义协议：路径传参会改写编码→用 ?u= query；Windows 下走 https 不走 http
- 代理不支持 Range → seek 重置为 0（Range/206 是音频播放器关键基建）
- 字符串替换类自动编辑后**必须立即 node --check 校验**（v0.4.9 曾因多一个引号导致主脚本全死：指令/光标/字幕全失效，特征=日志网络绿但 UI 死）。注意 PowerShell 5.1 `Get-Content -Raw` 会把 UTF-8 当 GBK 读致中文串损坏→node --check 误报；改用 Node 脚本 `fs.readFileSync(path,'utf8')` + `new vm.Script(code)` 逐 `<script>` 块校验（脚本在 `%TEMP%\claude\check_cm_js.js`）
- **VIP曲目「Format error」排障史（v0.4.11–v0.4.21，勿重蹈）**：现象=登录成功但VIP曲 AUDIO_ERROR Format error。逐步定位：①先疑UA→代理 get_client 曾用非浏览器UA"ClaudeMusic/0.3"被CDN 403，改浏览器UA；②再疑缺Cookie→向126.net透传MUSIC_U反而触发Tengine 403(跨域Cookie)，撤销；③A/B四变体(https/去Referer/去Range/组合)全403→证明非请求头问题；④根因=weapi(os_web)对客户端专属曲只给受限jdymusic直链。**最终解=eapi客户端身份取链**(见架构关键点10)。教训：网易VIP取链认准 eapi，别在 weapi 直链的请求头上浪费时间
- **排障务必让日志说话**：每个网络命令都要有探针(LOGIN/SONGURL/PROXY/PLAYLIST 前缀)记录 code/host/bytes/body；曾靠 `PROXY music ct=text/plain bytes=2818` 与 `ct=audio/mpeg bytes=6503488` 的对比一眼定位。body<100KB 时 dump 前300字(网易的拒绝原因常写在里面)
- **Node探针先行**：改 Rust 加密/接口前，先用 Node(`%TEMP%\*.js`)打真接口验证算法与响应结构(weapi/eapi/nav/song_url 都这么验的)，避免在 Rust 里盲改+编译+部署的空转。下载第三方源码(如 NeteaseCloudMusicApi/util/crypto.js)到隔离目录只读不执行
- headless 测试时窗口被遮挡 → WebView2 冻结定时器/rAF → "定时器不触发"是测试伪影，同步探针才可靠；我在 WorkBuddy 沙箱内调试时 WebView2 会被注入 tsbx.dll 引发渲染器崩溃（用户双击 exe 运行不受影响）
- 每完成一轮修复：git commit + 打 tag（vX.Y.Z）+ 更新 `D:\ClaudeMusic\docs\UNITS.md` 与 README（保持一致）。**git 用便携版全路径**(见版本管理行)；多行中文 commit message 用 `git commit -F <utf8文件>`，勿用 PowerShell here-string(会被空格/引号截断成 pathspec 报错)

## 待用户确认事项

截至 v0.4.21：网易云登录(扫码+Cookie)、VIP曲目播放(eapi)、侧边栏歌单导入均已由用户实测**确认正常播放**；情绪变色阈值已按用户要求两次下调。开场可询问：字幕/频谱/情绪变色是否观感满意，是否需要更高音质(现 eapi 取 exhigh=320k，SVIP 可改 lossless/jymaster)。

## 未来需求（用户已提过/规划中）

1. ~~网易云扫码登录 + VIP曲目播放~~ **已完成(v0.4.11–v0.4.21)**：扫码/Cookie登录、eapi取链播放VIP、侧边栏我的歌单+URL/ID导入。遗留可选项：①SVIP更高音质(lossless/jymaster，现固定exhigh)②歌单分页(现单页limit1000)③扫码风控8821若频发可加eapi取unikey
2. **yrc 逐字卡拉OK歌词**
3. **release 构建 + NSIS 安装包**（npx tauri build；日志已改 app_log_dir 可移植）
4. 代理流式转发（受 Tauri 响应体 API 限制，现为缓冲转发）
5. 恢复 CSP（调试完稳定后）
6. mediaSession 媒体键（验证 WebView2 155 兼容后）
7. 定期 `cargo clean`（target/ 2.7GB + toolchain/ 1.1GB 磁盘占用）

## 环境注意

- 用户 Windows，bash shell（Git Bash）；git 在 bash 里可用但系统 PATH 无 git
- Python: `C:/Users/Yuki/.workbuddy/binaries/python/versions/3.13.12/python.exe`；Node: `C:/Users/Yuki/.workbuddy/binaries/node/versions/22.22.2-6/node.exe`
- 本机 Clash Verge 存在（TUN/服务模式），系统代理绕过列表不含 proxy.localhost——应用已用"清除 env 代理 + 直连"方案规避
- 沟通风格：用户是中文用户，反馈直接、常带代码级审查意见（喜欢逐条编号提 bug），喜欢我"结论先行 + 证据链 + 每条反馈逐一落实 + tag 可回退"的工作方式。修完必须让用户双击 claudemusic-test.exe 实测，不要依赖我在沙箱内的 headless 测试结果

请先确认你已理解以上全部上下文，然后询问我对当前版本(v0.4.21)的验证结果，再继续后续开发。
