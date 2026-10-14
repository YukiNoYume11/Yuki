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

- 音源：网易云音乐公开接口（搜索/元数据 + 免费曲目外链播放），VIP 曲目登录后按账号权益播放；Radio Browser 华语电台兜底
- UI：Claude Code 风格（暗色 #1F1E1D / 赭橙 #D97757 / 等宽字体 / 命令输入栏）
- 吉祥物：Claude 星芒拟人形象，播放事件驱动交互（律动/走位/气泡提示）

## 免责声明

仅供个人学习与私人使用，非商业用途。音源接口来自平台公开端点，音频版权归平台及版权方所有。
