# 单元管理与回退手册

## 单元划分

| 单元 | 路径 | 内容 | 依赖 |
|------|------|------|------|
| U1 | `webui/` | 纯 Web 界面（UI/播放器/吉祥物/命令栏） | 无，浏览器可独立运行 |
| U2 | `app/` | Tauri 壳（Rust 后端、窗口、打包） | U1、Rust 工具链 |
| U3 | `assets/` | 吉祥物与图标素材源文件 | 无 |
| U4 | `docs/` | 文档 | 无 |
| UT | `toolchain/`（可选） | 便携版 Rust 工具链（仅当用户同意且需隔离时） | 磁盘空间 ~1.5GB |

U1 是唯一硬依赖核心：U2 只是 U1 的原生外壳，U1 可独立开发、预览、回退。

## 常用回退操作

```bash
cd /d/ClaudeMusic

# 查看所有提交（每个功能一个提交）
git log --oneline

# 查看里程碑版本
git tag

# 整体回到某个里程碑（保留工作区确认后切换）
git checkout v0.1-webui

# 只回退某个单元的某次改动（生成反向提交，历史完整可再恢复）
git revert <commit-hash>

# 只回退某单元到指定提交（危险：会丢弃之后的改动，需先备份）
git checkout <commit-hash> -- webui/

# 查看某单元的修改历史
git log --oneline -- webui/
```

## 提交规范

- 格式：`<单元>: <动作> <摘要>`，如 `U1: feat 吉祥物交互状态机`
- 每个功能点独立提交，保证 revert 粒度足够细
- 里程碑 tag 命名：`v<主>.<次>-<单元或全量>`

## 环境变量（UT 启用时）

便携 Rust 安装在 `toolchain/` 内，不污染系统：

```bash
export RUSTUP_HOME=/d/ClaudeMusic/toolchain/rustup
export CARGO_HOME=/d/ClaudeMusic/toolchain/cargo
export PATH="/d/ClaudeMusic/toolchain/cargo/bin:$PATH"
```

卸载 = 删除 toolchain 目录，系统无残留。
