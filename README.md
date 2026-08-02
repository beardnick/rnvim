# rnvim

远程开发工具：**编辑在本地，文件和智能在远程**——VSCode Remote 的架构，Neovim 的内核。

```bash
rnvim dev-box:~/project
```

一条命令：自动下载锁定版本的 Neovim（与用户已有的 Neovim 完全隔离）、通过 SSH 自动部署远程
agent、在本地打开远程工作区。buffer 在本地，打字零延迟；文件、LSP、工具链在远程。

## 设计原则

- **本地前端 + 远程 workspace 后端**。不走 `--remote-ui`（打字延迟 = 网络 RTT），也不走
  sshfs（一致性泥潭）。权威文件永远在远程，本地只持有打开文件的 buffer 副本。
- **锁定运行时**。每个 rnvim 版本钉死一个 Neovim 版本（当前 v0.12.4），client、agent、
  内置 Lua runtime 作为一个原子单元发版，协议版本精确匹配，没有兼容矩阵。
- **不迁就存量生态**。workspace 能力（finder、git、LSP 集成）做第一方远程原生实现，
  而不是给现有插件写兼容 shim。纯 buffer 类插件（surround、textobjects、主题）天然工作，
  通过用户叠加层加载（`~/.config/rnvim/user/init.lua`）。

## 当前状态（MVP：M0 + M1）

- [x] 托管 Neovim：首次运行自动下载锁定版本，`NVIM_APPNAME=rnvim` 隔离启动
- [x] 内嵌 Lua runtime，随二进制发布，启动时展开
- [x] SSH 自动部署 agent：同平台推送自身二进制，跨平台回退到内嵌的纯 stdlib Python agent
- [x] 协议握手 + 版本校验（JSON-lines over stdio）
- [x] 远程文件打开 / 编辑 / 保存（BufReadCmd/BufWriteCmd → agent fs 服务）
- [x] 远程目录浏览（`<CR>` 进入，`-` 返回上级）
- [x] 新建远程文件（含自动创建父目录）
- [x] `local:` 回环模式（无需 sshd 的开发/测试路径）

### 试用

```bash
cargo build --release

# 远程会话（需要 ssh 免密登录）
./target/release/rnvim dev-box:~/project

# 本地回环（无需远程机器）
./target/release/rnvim local:/tmp/somedir
```

## 架构

```
┌─ 本地 ────────────────────────────┐      ┌─ 远程 ──────────────┐
│  Neovim v0.12.4 (托管、隔离)       │      │                     │
│    └─ 内嵌 Lua runtime             │      │  rnvim agent        │
│         │ unix socket (JSON lines) │ ssh  │   fs.read/write/    │
│  rnvim client (broker)  ───────────┼──────┼─  list/stat/resolve │
│    版本管理 / agent 部署 / 泵线程   │      │                     │
└───────────────────────────────────┘      └─────────────────────┘
```

- `crates/rnvim-proto` — 协议类型，client/agent 唯一共享真相源
- `crates/rnvim-agent` — 远程 agent（`rnvim agent --stdio`）
- `crates/rnvim` — 客户端：CLI、Neovim 版本管理、传输、部署、会话 broker
- `runtime/` — 内嵌 Neovim Lua runtime（rpc 客户端 + 虚拟文件系统）

路径模型：远程绝对路径挂载在本地前缀 `~/.rnvim/ws/<host>/` 之下（前缀映射而非 URL
scheme），为后续 LSP 代理的 URI 重写做的先手设计——翻译退化为纯前缀替换。

## 路线图

- **M2**：LSP 代理（协议层 URI 前缀重写、root_dir 接管、watcher 短路）、远程 `:terminal`
- **M3**：finder/grep（nucleo + ignore，远程算匹配回传 top-N）、quickfix 集成
- **M4**：QUIC 传输（0-RTT 重连、漫游）+ SSH stdio 降级、端口转发、git 只读三件套
- [x] 发布工程：CI（fmt/clippy/test）+ tag 触发四平台构建（含 musl 静态 agent）发布到
  GitHub Release；客户端按需拉取远程平台的预编译 agent（本地经 `gh` 认证下载、缓存于
  `~/.rnvim/dist/`、SSH 推送——远程机器无需访问 GitHub）
- 待做：协议快照测试、docker sshd 集成测试、下载校验和验证
