# Codex Provider Hub

Codex Provider Hub 是一款基于 Tauri 2、Rust 和 React/TypeScript 的本地桌面工具，用于修复 Codex 切换 Provider 后历史会话仍在磁盘上、却不再出现在侧栏的问题。

工具只处理本机数据，不管理 Provider 密钥或模型，不连接远端主机，也不会上传会话内容。

## 解决什么问题

切换 Provider 后，Codex 当前使用的 <code>config.toml.model_provider</code> 可能与历史会话记录的 Provider 不一致。会话文件并没有丢失，但 Codex 会按 Provider 过滤本地线程，于是旧会话看起来像“消失了”。

Codex Provider Hub 会：

1. 自动发现 <code>CODEX_HOME</code> 和当前 Provider。
2. 从活动与归档 rollout 中列出全部有效本地会话。
3. 允许恢复全部会话，或只恢复用户勾选的会话。
4. 在写入前自动备份。
5. 将目标会话对齐到当前 Provider，并立即验证结果。
6. 保留会话原有的归档、内部线程和来源语义。

它不会创建 <code>codex_local_session</code> 之类的中间 Provider。无论当前 Provider 是 <code>openai</code>、<code>custom</code> 还是此前从未见过的合法 ID，工具都读取并精确使用根级 <code>model_provider</code>。切换到另一个 Provider 后，再运行一次修复即可。

## 快速使用

1. 双击 <code>codex-provider-hub.exe</code>，或从安装包启动 Codex Provider Hub。
2. 等待自动扫描完成。首页直接显示全部有效本地会话，不按原 Provider 隐藏。
3. 不勾选会话时，主按钮为“恢复全部会话”；勾选一条或多条后，主按钮变为“恢复选中的 N 个会话”。
4. 点击恢复并确认当前目标 Provider。
5. 工具自动完成预览、备份、短事务写入和验证。
6. 返回 Codex 查看会话。若侧栏仍显示旧缓存，再重启 Codex。

默认不需要关闭 Codex。只有真正发生 SQLite 写冲突并在重试后仍未释放时，界面才会提示关闭占用程序。普通的“检测到 Codex 正在运行”不是错误，也不是修复前置条件。

### 两种恢复范围

- 恢复全部会话：处理扫描到的全部有效本地 rollout。
- 恢复选中的会话：只处理列表中明确勾选的 thread ID。

本项目不再提供“最近 50 条”修复范围。修复范围只有“全部”或“选定”；Codex 侧栏最终展示多少条，仍由 Codex 自身的界面策略决定。

### 哪些会话会出现

默认包含：

- <code>sessions/</code> 下的活动会话；
- <code>archived_sessions/</code> 下的归档会话；
- 普通用户会话；
- 子代理、自动化等本机内部会话。

修复不会把归档会话变为活动会话，也不会把内部会话改成普通用户会话。工具只对齐可见性所需的状态。

默认排除：

- 元数据明确标记为 SSH、WSL、Dev Container、Codespaces 或其他远端主机的会话；
- 缺少有效 <code>session_meta</code> 的损坏 rollout；
- thread ID 重复或文件名与 ID 冲突、无法安全确定唯一来源的记录。

<code>source=vscode</code> 本身不代表远端。只有结构化的远端强信号才会触发排除。

## 数据来源

这些文件不是几套可以相加的“会话总数”，而是同一批 thread ID 的不同状态层：

| 数据源 | 用途 | 修复时是否写入 |
| --- | --- | --- |
| <code>sessions/**/*.jsonl</code> | 活动 rollout，会话全集来源之一 | 只更新首条 <code>session_meta</code> 中的 Provider 元数据 |
| <code>archived_sessions/**/*.jsonl</code> | 归档 rollout，会话全集来源之一 | 只更新首条 <code>session_meta</code> 中的 Provider 元数据 |
| <code>state_5.sqlite / threads</code> | Codex 官方线程状态，权威修复数据库 | 是，按 thread ID 精确更新或补建必要字段 |
| <code>sqlite/codex-dev.db / local_thread_catalog</code> | 本地侧栏缓存/投影 | 否，仅用于诊断覆盖情况 |
| <code>session_index.jsonl</code> | 可能滞后的辅助索引 | 否，仅用于诊断 |
| <code>config.toml</code> | 提供当前根级 <code>model_provider</code> | 否，只读 |
| <code>auth.json</code> | 凭据 | 否，不读取密钥、不修改 |

### 总会话数的口径

会话总数以 <code>sessions/</code> 和 <code>archived_sessions/</code> 中可读取、ID 唯一、具有有效 <code>session_meta</code> 的本地 rollout 为准，再排除明确远端记录。

<code>session_index.jsonl</code> 可能不完整，不能当作总数。<code>local_thread_catalog</code> 可能缺行或滞后，也不能当作总数。缺少 catalog 数据不会阻止扫描、恢复或验证，catalog 数据库不存在也不是修复失败。

### 为什么能看到两套 SQLite

<code>state_5.sqlite</code> 是 Codex 的官方线程状态库，也是本工具的实际修复目标。

<code>sqlite/codex-dev.db</code> 中的 <code>local_thread_catalog</code> 是辅助缓存。工具会读取它解释“为何侧栏当前覆盖不完整”，但正常修复不再依赖它，也不会为了恢复会话去改写它。因此，本工具不是“双 SQLite 同步器”。

## 修复流程

~~~text
扫描本地 rollout
  -> 按 thread ID 建立全部本地会话清单
  -> 读取 config.toml 当前 Provider
  -> 生成全部或选定会话的预览计划
  -> 获取 Provider Hub 操作锁
  -> 重新扫描并校验 planToken
  -> 在线备份 state_5.sqlite 与 rollout Provider 前镜像
  -> 短事务写入
  -> 重新扫描并验证
  -> 返回结果与日志
~~~

目标 Provider 必须与执行时 <code>config.toml</code> 根级 <code>model_provider</code> 精确一致。工具不使用固定 Provider 白名单，也不要求目标 ID 已出现在历史会话中。Provider 在预览与写入之间发生变化时，旧计划会被拒绝，不会误写到另一个 Provider。

### 实际写入内容

对恢复范围内的会话，工具只写入可见性所需内容：

- <code>state_5.sqlite / threads</code> 中的 Provider、rollout 路径及必要线程元数据；
- rollout 首条 <code>session_meta</code> 中的 Provider 字段；
- 工具自身用于安全恢复的内部状态。

会话消息、标题正文、后续 JSONL 记录、Provider 配置、密钥和模型配置不会被批量重写。归档状态和内部线程类型保持原样。

## 安全、备份与回滚

### 只读预览和计划令牌

扫描、预览和验证都是只读操作。每次 apply 都必须携带刚刚预览生成的 <code>planToken</code>。如果当前 Provider、数据库、rollout 或选择范围在预览后发生变化，工具会拒绝旧令牌并要求重新预览。

### 自动备份

真正写入前会在下列目录创建备份：

~~~text
CODEX_HOME/backups/provider-hub/
~~~

当前备份包含：

- <code>state_5.sqlite</code> 的一致性快照；
- 本次将修改的 rollout Provider 前镜像；
- 存在时的工具内部投影状态；
- 带校验信息的 <code>manifest.json</code>。

备份不复制整份会话消息，因此不会因为大量 JSONL 内容造成无意义的备份膨胀。

### 失败处理

- 写入前或提交前失败时，工具不会留下半完成结果，并会按记录进行安全补偿。
- 提交后验证失败时，工具保留操作记录和备份，不会在 Codex 可能已经写入新数据后盲目整库覆盖。
- 用户可以从技术信息区域显式恢复最近备份或指定备份。

整库恢复属于应急操作。执行恢复前应先关闭实际占用数据库的程序，避免覆盖恢复期间的新写入。

### 幂等

同一 Provider、同一范围重复执行修复时，第二次应返回 <code>0 changes</code>。这既是正常结果，也是判断修复是否稳定的重要验证。

### 操作锁

Provider Hub 使用 Windows 原生文件锁防止两个修复任务同时写入。磁盘上存在 <code>.lck</code> 文件不代表锁仍被占用；只要没有进程持有 OS 锁，状态就是可用。不要手动删除锁文件。

## 验证代表什么

修复后的验证会重新读取本地数据，并检查：

- 目标 thread ID 仍有唯一、有效的本地 rollout；
- <code>state_5.sqlite</code> 中的 Provider 已与当前 <code>model_provider</code> 对齐；
- rollout 首条 Provider 元数据已对齐；
- rollout 路径、归档状态和内部线程语义符合修复计划；
- 选定修复没有误改未选中的会话。

验证通过表示本地会话状态已完成对齐，不等于 Codex UI 必须在同一分组中一次渲染全部历史。遇到侧栏缓存时，重启 Codex 后再检查。

## CLI

桌面 GUI 与 CLI 是两个入口。Windows 下双击 GUI 不会打开控制台窗口；自动化脚本使用 <code>codex-provider-hub-cli.exe</code>。

~~~powershell
codex-provider-hub-cli.exe scan
codex-provider-hub-cli.exe backup
$preview = codex-provider-hub-cli.exe repair --dry-run | ConvertFrom-Json
codex-provider-hub-cli.exe repair --apply --plan-token $preview.planToken
codex-provider-hub-cli.exe verify
codex-provider-hub-cli.exe restore
~~~

完整语法：

~~~text
codex-provider-hub-cli.exe scan
  [--codex-home PATH]

codex-provider-hub-cli.exe backup
  [--codex-home PATH]

codex-provider-hub-cli.exe repair
  [--codex-home PATH]
  [--target-provider ID]
  [--plan-token TOKEN]
  [--dry-run|--apply]

codex-provider-hub-cli.exe verify
  [--codex-home PATH]
  [--target-provider ID]

codex-provider-hub-cli.exe restore [BACKUP_PATH]
  [--codex-home PATH]
~~~

CLI 的 <code>repair</code> 当前处理全部会话；选定会话请使用桌面端。CLI 不支持 <code>--scope</code>，也没有“最近 50 条”模式。

规则：

- <code>repair</code> 默认为 dry-run。
- 只有 <code>--apply</code> 或 <code>--write</code> 才会写入。
- apply 必须带上前一次 dry-run 返回的 <code>planToken</code>。
- 省略 <code>--target-provider</code> 时读取当前 <code>config.toml.model_provider</code>；显式传入时也必须与当前值一致。
- 省略 <code>--codex-home</code> 时依次使用 <code>CODEX_HOME</code>、Windows 的 <code>%USERPROFILE%\.codex</code> 或 Unix 的 <code>$HOME/.codex</code>。
- <code>restore</code> 省略路径时恢复最近的完整备份。
- 成功结果以 JSON 写入 stdout，错误写入 stderr；成功退出码为 0，失败为 1。

## 能力边界

当前支持：

- 自动发现本机 <code>CODEX_HOME</code>；
- 扫描全部有效本地活动、归档和内部 rollout；
- 动态识别任意合法 Provider ID；
- 恢复全部会话或桌面端选定会话；
- Provider 精确匹配、预览令牌、自动备份、验证和回滚；
- Codex 保持运行时的在线修复；
- 标题与项目名模糊搜索；
- 打开项目目录、定位 rollout 文件和复制会话 ID；
- 本地操作日志和技术诊断。

当前不做：

- Provider 密钥、模型或 <code>config.toml</code> 管理；
- 自动切换 Provider；
- SSH、WSL、Dev Container 或其他远端会话修复；
- 云同步或后台常驻 watcher；
- 会话消息内容迁移；
- 强制绕过 Codex 自身的侧栏显示上限或缓存策略。

## 排障

### 扫描或恢复提示 SQLite busy

先直接重试。Codex 的写事务通常很短，正常情况下不必关闭它。持续失败时，再保存当前工作并使用界面提供的关闭占用程序功能。

只有关闭动作明确返回 <code>Access denied</code>，并确认占用进程以更高权限运行时，才需要以管理员身份启动 Provider Hub。

### Provider 不匹配

先确认 <code>config.toml</code> 根级 <code>model_provider</code> 是你当前实际使用的 Provider，再重新扫描。Provider ID 区分大小写并按原值处理；工具不会回退到 OpenAI，也不会自动选择历史来源 Provider。

### 计划已过期

重新扫描并预览，然后使用新返回的 <code>planToken</code>。这表示预览后 Provider、会话选择或本地数据发生了变化，属于安全保护。

### catalog 或 session index 数量较少

这是允许的。它们只是诊断来源，不是会话全集，也不是修复前置条件。请以有效本地 rollout 列表为准。

### 修复成功但 Codex 侧栏尚未刷新

先重启 Codex 以清除侧栏缓存。若某个会话仍未出现，查看工具日志中的远端排除、重复 ID、损坏 rollout 或并发修改原因。

### 点击会话后归到另一个项目

这通常是历史 rollout 中的 cwd 与当前项目目录不一致。可见性修复不会猜测并批量改写项目路径；先确认会话实际所属目录，再处理项目迁移问题。

## 本地开发

需要：

- Node.js 与 npm；
- Rust stable 与 Cargo；
- Tauri 2 对应平台的原生构建依赖；
- Windows 上的 WebView2 Runtime。

安装依赖并启动桌面开发环境：

~~~powershell
npm ci
npm run tauri -- dev
~~~

常用检查：

~~~powershell
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
~~~

<code>npm run dev</code> 只启动 Vite 前端开发服务器，不是另一个浏览器版本，也不包含 Rust core 与 Tauri IPC。完整功能必须通过 <code>npm run tauri -- dev</code> 或桌面 EXE 运行。

## 构建与发布产物

### 当前平台构建

~~~powershell
npm ci
npm run tauri -- build
~~~

Windows 常见输出：

~~~text
src-tauri/target/release/codex-provider-hub.exe
src-tauri/target/release/codex-provider-hub-cli.exe
src-tauri/target/release/bundle/msi/*.msi
src-tauri/target/release/bundle/nsis/*-setup.exe
~~~

<code>codex-provider-hub.exe</code> 是可直接双击运行的 GUI 程序；安装包更适合普通用户分发。Windows 10/11 通常已包含 WebView2，缺失时需要先安装 Microsoft Edge WebView2 Runtime。

macOS 与 Linux 应在对应系统或 CI runner 上分别构建。Tauri 会按平台生成 <code>.app/.dmg</code>、<code>.deb</code>、<code>.rpm</code> 或 <code>.AppImage</code> 等产物，具体类型取决于该平台安装的打包工具。

### 首次发布建议

1. 同步 <code>package.json</code>、<code>src-tauri/Cargo.toml</code> 和 <code>src-tauri/tauri.conf.json</code> 的版本号。
2. 在 Windows、macOS、Linux 上分别完成构建和基本流程验证。
3. 创建形如 <code>v0.1.0</code> 的 Git tag。
4. 在对应 GitHub Release 中上传各平台安装包、便携 GUI、可选 CLI 与 SHA-256 校验文件。
5. 在 Release Notes 中说明支持范围、已知限制和升级注意事项。

本地构建不会自动创建或发布 Git tag。发版应在测试通过后单独执行。

## 项目结构

~~~text
src/                         React/TypeScript 桌面界面
src-tauri/src/core.rs        扫描、计划、备份、修复与验证
src-tauri/src/rollout.rs     rollout 解析与精确元数据更新
src-tauri/src/platform.rs    Windows 原生进程、锁与系统操作
src-tauri/src/lib.rs         Tauri 命令入口
src-tauri/src/bin/           CLI 入口
src-tauri/tauri.conf.json    桌面窗口与 bundle 配置
~~~
