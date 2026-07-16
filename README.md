# Codex Provider Hub

切换 Provider 后，一键恢复本地 Codex 历史会话。

Codex Provider Hub 是一款面向 Windows 的本地桌面工具。它扫描保存在 `CODEX_HOME` 中的历史会话，并将全部会话或用户选中的会话恢复到当前 Provider。

[下载页面](https://github.com/XVibeCoding/codex-provider-hub/releases)

技术栈：Tauri 2、Rust、React、TypeScript。

## 解决什么问题

Codex 切换 `model_provider` 后，旧会话可能从侧栏中消失。多数情况下，会话文件并没有丢失，而是 rollout、SQLite 线程状态和本地目录中的 Provider 信息不一致，导致 Codex 按当前 Provider 过滤掉了历史会话。

Codex Provider Hub 会：

- 自动发现本机 `CODEX_HOME` 和当前 Provider；
- 列出全部可识别的本地活动、归档和内部会话；
- 恢复全部会话，或只恢复用户勾选的会话；
- 在写入前自动备份，并在完成后验证结果；
- 自动保留最近 5 个健康回滚点，避免备份目录持续占用系统盘；
- 动态识别 `openai`、`custom` 及其他合法 Provider ID。

工具不会创建中间 Provider，也不负责切换 Provider、管理密钥或修改模型配置。先切换到准备使用的 Provider，再执行恢复即可。

## 快速使用

1. 在 Codex 或现有 Provider 管理工具中切换到目标 Provider。
2. 从 [Releases](https://github.com/XVibeCoding/codex-provider-hub/releases) 下载 Windows EXE 或安装包。
3. 打开 Codex Provider Hub，等待自动扫描完成。
4. 点击“恢复全部会话”，或勾选会话后恢复选中项。
5. 返回 Codex 检查侧栏；如果仍显示旧缓存，重启 Codex 后再看。

> 所有扫描和修复都在本机完成，不上传会话内容。写入前会自动备份，正常情况下也不要求先关闭 Codex。

自动备份默认最多保留 5 份、容量上限 250 MiB，并始终保留至少 2 个健康回滚点。手动锁定、正在使用和未完成操作关联的备份不会被自动删除；旧版不兼容备份只会在用户确认后清理。

## 本地开发

当前主要验证平台为 Windows 10/11。开始前请安装：

- Node.js 和 npm；
- Rust stable 和 Cargo；
- Microsoft C++ Build Tools；
- Microsoft Edge WebView2 Runtime。

安装依赖并启动完整桌面开发环境：

```powershell
npm ci
npm run tauri -- dev
```

`npm run dev` 只启动 Vite 前端页面，不包含 Rust core 和 Tauri IPC，不能用于验证真实会话恢复。

常用检查：

```powershell
npm run build
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

构建 Windows EXE 和安装包：

```powershell
npm run tauri -- build
```

常见输出位于：

```text
src-tauri/target/release/codex-provider-hub.exe
src-tauri/target/release/codex-provider-hub-cli.exe
src-tauri/target/release/bundle/
```

## 项目结构

```text
src/                         React/TypeScript 桌面界面
src/components/              会话列表、恢复确认和技术详情组件
src-tauri/src/core.rs        扫描、计划、备份、修复和验证
src-tauri/src/projection.rs  多 Provider 会话投影模型
src-tauri/src/rollout.rs     rollout 解析与元数据更新
src-tauri/src/platform.rs    Windows 原生进程、锁和系统操作
src-tauri/src/lib.rs         Tauri 命令入口
src-tauri/src/bin/           CLI 入口
src-tauri/tauri.conf.json    桌面窗口与安装包配置
```

## 交流群

<p align="center">QQ群：<strong>1063954502</strong></p>

<p align="center">扫码加入 Codex Provider Hub 交流群：</p>

<p align="center">
  <img src="asserts/qq交流群.jpg" alt="Codex Provider Hub QQ 交流群二维码" width="320" />
</p>

## License

本项目采用 MIT 许可证，详见 [LICENSE](LICENSE)。
