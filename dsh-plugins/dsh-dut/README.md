# dsh-dut

sermcp × dsh（DeepSeek Harness）完整集成 bundle：**server 模式**
（MCP 工具桥接，dsh agent 可直接控制 DUT 串口）+ **client 模式**（statusline
状态展示）。一个包、两行插件、一条命令安装。

> 取代旧的 `dsh-dut-statusline`（只做展示）。装了本包后请移除旧包：
> `dsh plugin --profile dsh-tui remove dsh-dut-statusline`（两个 bundle 的
> `dut-statusline` 行 id 相同，并存会触发 loader 的 duplicate-id 拒绝）。

## 效果

在配置好的项目（存在 `.target.jsonc` 的目录）里启动 dsh-tui：

1. **server 模式（= 当前 MCP 的 dsh server plugin 行 `dut-mcp`）**：
   自动拉起（或复用）项目的 sermcp 服务器，通过 Streamable
   HTTP 连接，把全部 `serial_*` 工具按 dsh 的 server-plugin 命名契约注册为
   `mcp__dut__serial_*` 原生工具 —— dsh agent 可以像 Claude Code / pi.dev
   一样驱动 DUT（多 dev host / 多 DUT 由服务器自身支持，工具带 `dut_name`
   参数）：
   ```
   /mcp  →  mcp__dut__serial_get_state, mcp__dut__serial_send_command, …（28 个）
   ```
   不用官方 `@deepseek-ai/dsh-mcp-client` 行的原因见「升级兼容性」。
2. **client 模式（`dut-statusline`）= 对 MCP 状态的反应**：显示每个 DUT
   的串口状态（`● rk3576-ubuntu:active`，与 Claude Code 状态栏同源同色）。
   statusline 从不驱动服务器，只**反应** MCP 状态：fs.watch 服务器原子
   重写的 `inventory.json`，状态迁移毫秒级重渲染、空闲零 CPU。

启动目录**不是**配置好的项目时两行插件都保持休眠：不连服务器、不注册
工具、状态行不显示 —— 切进项目目录（`/workspace`、`--resume`）立即激活。

## 渲染位置：只用官方 `tuiStatus` 接缝，不补丁 dsh-tui

> 历史约束（已废止）：早期版本要求 DUT 状态与 footer 状态行同一行渲染，
> 为此本仓库随包分发打过补丁的 dsh-tui 构建（tarball + `dsh-tui-fix.diff`，
> 在 `src/dsh-adapter/status.ts` / `src/screens/StatusLine.tsx` 上加了
> `setStyled` 彩色段、footer 右字段组渲染、`statusBar.workingHint`）。
> 每次上游发布都要重新 apply 补丁、重打 tarball、重 pin profile —— 维护
> 成本随上游迭代线性增长，且与官方生态规范冲突。

**现方案（不补丁 dsh-tui）**：只用官方「稳定候选」接缝 —— dsh-tui 的
`ctx.tuiStatus`（生态准入规范「接缝十一 · 状态行」）。按上游契约，宿主
把各插件贡献按首次设置顺序拼成一行（` · ` 连接）渲染在**提示框上方**；
插件只供文本，不碰布局。dsh-tui 的 stock npm 构建（0.9.3 / 0.10.0-beta.x）
即满足全部依赖，profile 无需任何补丁构建或版本 pin。

由此产生的三处行为变化（均为上游契约，插件侧不可达）：

1. **位置**：DUT 状态显示为 prompt 上方的一行（如
   `● rk3576-ubuntu:active`），不再挤进 footer 右字段组 —— footer 注入
   seam 上游未提供；
2. **颜色**：stock dsh-tui 无 `setStyled`，插件降级为纯文本（ANSI 剥离），
   状态颜色不渲染。插件保留能力探测：一旦上游落地彩色段能力，自动启用
   （无需改插件）；
3. **`esc to interrupt` 提示**：工作态提示由 TUI 自身渲染，无配置项、
   无插件接缝可关 —— 插件无法也不应去控制它。

## 安装

**本项目固定从 GitHub 链接安装**（pnpm 的 git 子目录语法，`path:` 参数）：

```bash
# 1.（首次迁移）移除只做展示的旧插件 —— 功能已并入本 bundle
dsh plugin --profile dsh-tui remove dsh-dut-statusline

# 2. 从 GitHub 安装（#path 要加引号，防 shell 当注释吞掉）
dsh plugin --profile dsh-tui add "bitshelf/sermcp#path:/dsh-plugins/dsh-dut"
```

- dsh-tui 用 npm 的 stock 版本即可（`dsh plugin`/`dsh-tui /update` 正常
  升级），**不再**需要把 profile 的 dsh-tui 依赖 pin 到任何 tarball；
- 安装产物：profile 的 package.json 写入
  `"dsh-dut": "github:bitshelf/sermcp#path:/dsh-plugins/dsh-dut"`，
  CLI 的 reconcile 把它加进 `dsh.profile.bundles`；
- 仓库代码/文档更新并 push 后，重新执行第 2 步即可刷新安装；
- 依赖 `@modelcontextprotocol/sdk`：pnpm 随包自动安装，无需手工处理；
- 开发期本地联调可临时改用
  `dsh plugin --profile dsh-tui add "link:./dsh-plugins/dsh-dut"`（改代码
  即时生效），交付前换回 GitHub 链接；
- 卸载：`dsh plugin --profile dsh-tui remove dsh-dut`。

## 配置

在 `~/.dsh/profiles/dsh-tui/cordis.patch.yml` 覆盖行配置（默认即可用）：

```yaml
# server 模式行：工具命名空间与注册过滤
- id: dut-mcp
  config:
    serverName: dut            # 工具名 mcp__<serverName>__serial_*（默认 dut）
    toolPattern: '^serial_(get_|list_)'   # 只注册只读工具（省 token）；默认全部
    autoStart: true            # 项目内服务器不在时自动拉起，默认 true
    pollIntervalMs: 2000       # 连接维护轮询间隔，默认 2000
    reconnectInitialMs: 500    # 断线重连退避初值，默认 500（翻倍至 30000）

# client 模式行：状态行（事件驱动，无需轮询配置）
- id: dut-statusline
  config:
    autoStart: true
```

> `dut-statusline` 是 **fs.watch（inotify）事件驱动**：MCP 每次状态迁移
> 原子重写 inventory.json，rename 事件即时触发重渲染（毫秒级上屏），
> 空闲项目零 CPU 消耗，无需任何轮询配置。

> 注意：patch 会**整体替换**目标行的 `config`，覆盖时要把想保留的键一并写上。

## 工作原理

两行共用同一套项目发现/活性门控/自动拉起逻辑（与 Claude Code 的
session-start hook、Python hooks 语义对齐）：

1. **项目发现**：`TARGET_CONF` 显式覆盖 → 从工作区 CWD 逐级向上找
   `.dut-serial/inventory.json` / `.target.jsonc`（`TARGET_EXCLUDE` 排除）；
   工作区跟随 `DSH_TUI_WORKSPACE_TARGET` 与 `tui/session-switched` 事件。
2. **端口解析**：优先 inventory 记录的 `mcp_http_port`（真实绑定端口），
   否则走确定性哈希端口（md5 → 3001–3099，与 `sermcp/src/ports.rs` 逐字节
   一致）。
3. **活性门控**：`mcp_pid` 存活（`kill(pid, 0)`）→ 否则 127.0.0.1 TCP 探测。
   服务器不在时按 `autoStartIntervalMs`（默认 60s）限频自动拉起
   （`--http 127.0.0.1:<port>`，detached，与 session-start hook 同款行为）。
4. **诚实注销**：服务器死亡/端口变化 → 立即注销全部已注册工具（模型工具集
   通过 tools/change 刷新），绝不保留"每次调用必失败"的僵尸工具；恢复后
   自动重连重注册。连接失败按指数退避重试（500ms → 30s 封顶）。
5. **多代理隔离**：沿用 `owner.json` 的会话身份，选择 Rust 生成的
   `state_text` 或 `guest_display.text`。其他会话占用时显示 `busy`；
   `dutabo serial` 接管时所有会话显示 `dutabo`，不覆盖设备真实状态。

工具调用：`execute` 走 `tools/call`（raw 名 + 参数），`isError` 结果抛错让
ToolRuntime 渲染为失败；文本块按序拼接返回模型，`structuredContent` 透传。

## 升级兼容性（dsh / dsh-tui 升级不得致不可用）

> 用户约定：dsh、dsh-tui 任何版本升级都**不得**让本插件不可用。

保证手段（已在代码与构建检查中固化）：

1. **零内部依赖**：运行时只 import node 内置模块 +
   `@modelcontextprotocol/sdk`（官方协议库，语义冻结）；peer 只声明
   `@deepseek-ai/cordis >= 4`。selftest 有"import surface"检查 —— 任何人
   再 import `@deepseek-ai/*` 运行时包，测试直接失败。
2. **不用官方 dsh-mcp-client 行**：`@deepseek-ai/dsh-mcp-client` 是 rc 线
   版本，运行时依赖钉死在 rc 版 dsh 核心（`@deepseek-ai/dsh-tools` 等）；
   在 alpha 线 profile 上 pnpm 会装进第二份不兼容的 dsh 核心，dsh/dsh-tui
   一升级就坏。本插件的 `dut-mcp` 行自带桥接，使用相同的
   `mcp__<server>__<tool>` 命名契约（/mcp 面板、模型工具集同源）。
3. **只依赖官方稳定插件面**：`tuiStatus` seam（生态准入规范「接缝十一」，
   稳定候选级）+ `tui/session-switched` 事件 + `ctx.tools` 服务，升级无需
   适配。
4. **能力探测而非版本判断**：`setStyled` 存在与否运行时探测（上游落地
   statusbar 贡献 seam 后自动启用彩色段，无需改插件）；`tuiStatus.set`
   失败只写日志，绝不打崩 TUI。
5. **行配置独立**：两行各自兜底，任一行的服务缺失时另一行照常工作；
   profile 层可用 cordis.patch.yml 按 id 覆盖任意键（patch 整体替换
   config，覆盖时保留想留的键）。
6. **零补丁维护**：不再随包分发 dsh-tui 补丁构建（历史 tarball 与
   `dsh-tui-fix.diff` 已从仓库移除，git 历史可查）；profile 的 dsh-tui
   依赖走 npm stock 版本，上游发布新版本时插件无需任何跟进动作。

## 验证

```bash
node dsh-plugins/dsh-dut/selftest.mjs    # 全部 OK，exit 0
# 覆盖：纯函数、端口公式 known-answer、自动拉起 spawn 形状、mock-ctx 集成、
#       ——以及真实 MCP 桥接（内置假 MCP server 走 Streamable HTTP 全链路：
#       连接 → 工具注册 → tools/call 往返 → 服务器死亡诚实注销）

# 组合树里应出现两行：
dsh --profile dsh-tui --dump-config | grep -A4 "dut-statusline\|dut-mcp"

# 真机验证（dsh-tui 里）：
#   - DUT 状态渲染在 prompt 上方一行（如 `● rk3576-ubuntu:active`，纯文本
#     无颜色 —— stock dsh-tui 的官方 tuiStatus 契约）；占用状态由服务端生成
#   - /mcp 面板显示 dut 服务器与 28 个 mcp__dut__serial_* 工具
#   - 让 agent 执行 serial_get_state 或 serial_list_duts 应直接可用
```

## 实现说明

- 纯 ESM、零构建；唯一运行时依赖 `@modelcontextprotocol/sdk`（官方协议
  客户端）；`peerDependencies` 声明 `@deepseek-ai/cordis`；
- 一个包、一个插件定义（`inject: ['tuiStatus', 'tools']`），`cordis.patch.yml`
  插入两行，按 `config.role`（`statusline` / `mcp`）分派 —— 与
  dsh-mcp-client 多实例同款模式；
- 全部 I/O（读文件、TCP 探测、spawn、连接、注册）均兜底，任何失败只写
  debug 日志，绝不把 TUI 打崩；`ctx.effect` 卸载清理定时器/连接/注册；
- 与 dsh-tui 的契约只有 `tuiStatus` seam + `tui/session-switched` 事件 +
  `ctx.tools` 服务，均为官方稳定插件面，升级无需适配；
- 调试：`DSH_DUT_DEBUG=/tmp/dsh-dut.log dsh --profile dsh-tui` 输出每个
  tick/连接/注册决策的 JSON 行（statusline 行仍读 `DUT_STATUSLINE_DEBUG`）。
