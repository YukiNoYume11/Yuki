# ClaudeMusic — 私人非商用音乐播放器

Tauri 桌面音乐软件 · Claude Code 终端风格 UI · Claude 吉祥物交互

## 目录结构（单元化，均可独立回退）

```
D:\ClaudeMusic\
├── webui\     # 单元1：纯 Web 界面（可浏览器预览，后期作为 Tauri 前端）
├── app\       # 单元2：Tauri 壳（Rust 后端 + 打包，待工具链就绪）
├── assets\    # 单元3：素材（吉祥物 SVG、图标）
└── docs\      # 单元4：设计与运维文档
```

## 回退机制

- 全项目 git 版本控制，**每个功能单元单独提交**（commit message 前缀标注单元名）
- 里程碑打 tag（如 `v0.1-webui`），随时可 `git checkout <tag>` 回到任意稳定版本
- 单个单元回退：`git log --oneline -- webui/` 查看该单元历史 → `git revert <commit>` 安全撤销
- 详见 docs/UNITS.md

## 技术要点

- 音源：网易云音乐公开接口（搜索/元数据 + 免费曲目外链播放）；QQ 音乐搜索已接入（仅元数据浏览，播放暂跳过）；Radio Browser 华语电台兜底
- **网易云登录（v0.4.11）**：扫码登录为主、粘贴 Cookie 兜底（`login` / `login cookie <值>`）；登录后解锁 VIP 曲目播放与「我的歌单」侧边栏；`playlist <url|id>` 可导入任意网易云歌单（公开歌单无需登录）
- UI：Claude Code 风格（暗色 #1F1E1D / 赭橙 #D97757 / 等宽字体 / 命令输入栏）
- 吉祥物：Claude 像素形象浮动层，播放事件驱动交互（走位/气泡/情绪律动）

## 免责声明

仅供个人学习与私人使用，非商业用途。音源接口来自平台公开端点，音频版权归平台及版权方所有。

## 维护提示

- 运行日志位于系统应用日志目录（`%APPDATA%\local.claudemusic\logs\`），带 UTF-8 BOM，超 2MB 自动轮转
- `app/src-tauri/target/` 约 2.7GB，`toolchain/` 约 1.1GB——空间紧张时可执行 `cargo clean`（下次构建需全量重编）
