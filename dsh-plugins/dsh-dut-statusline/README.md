# dsh-dut-statusline

把 sermcp 的 DUT 串口状态接入 **dsh-tui** 状态行（statusline）。

> Show sermcp DUT serial states — `● alpha:active`,
> `✗ beta:crashed` — on the dsh-tui statusline via the `tuiStatus` seam.

## 效果

> **显示位置硬性要求（第一优先级）**：DUT 状态必须与 dsh-tui **底部状态行
> （footer statusline）同一行** —— 即包含 `deepseek-v4-flash · max · cache
> 99.7% · ≈¥0.15 peak · sermcp`（模型 · effort · cache · 成本 ·
> 工作区）的那一行，且位于该行**最右侧**：

```
… · ≈¥0.15 peak · sermcp · ● rk3576-ubuntu:active
```

**禁止**：渲染为 prompt 上方的独立行、transcript 消息区、弹窗、标题行或
任何其它位置。以"贡献到了 tuiStatus"作为交付**不算满足**本要求 —— 验收
只看最终渲染行。

**现状与约束**：dsh-tui 0.9.3 与 0.10.0-beta.1 把 `tuiStatus` seam 的插件
贡献渲染在 prompt 上方的独立 dim 行（Chat.js `statusEntries`），且没有注入
底部状态行的公开 seam；这两个版本也没有 `tuiStatus.setStyled`（能力探测恒
走纯文本回退）。因此**当前 dsh-tui 版本无法由插件实现同一行渲染**。落地
路径：dsh-tui 上游提供 statusbar 贡献 seam（如 `channel` 扩展字段 /
`statusBar` 自定义字段）后，插件把 DUT 状态接在 footer 最右侧；或等 dsh-tui
升级到贡献与 footer 同排的版本。在此之前任何插件改动都**不得**把 DUT 状态
挪到其它位置充当"满足要求"。

目标配色（上游 seam 落地后）：active 绿 `●`、crashed/disconnected 红 `✗`、
booting 黄 `◐`、uboot 青 `●`、dutabo 紫 `●` —— 符号/颜色逐字取自 Rust
预生成的 `state_text`（ANSI 字符串，如 `\x1b[32m● alpha:active\x1b[0m`），
与 Claude Code 的 statusline hook 渲染同一份数据，状态→颜色映射只在
`state_manager.rs`，插件只把收到的 ANSI 重新编码，永不漂移；
- **事件驱动渲染**：`fs.watch`（Linux inotify）监视
  `<project>/.dut-serial/`，MCP Server 每次原子重写 `inventory.json`
  （状态迁移）都会即时触发重渲染（毫秒级上屏），空闲项目零 CPU；
  另有 20 秒一次的低频看门狗兜底：重挂失效的 watcher、重试项目发现、
  并让死服务器留下的陈旧快照经新鲜度门禁老化下屏；
- **自动拉起 MCP**：启动目录是配置好的项目（存在 `.target.jsonc`）但服务器
  不在时，插件自己把 MCP Server 拉起来（与 Claude Code 的 session-start
  hook 同款行为：`sermcp --http 127.0.0.1:<port>`，detached，
  端口取 inventory 记录值 / 确定性哈希，二进制走 PATH 与常见安装目录），
  最多每 60 秒尝试一次；
- **从不空白、从不撒谎**：服务器未就绪/连接异常时显示诚实的红色
  `✗ <dut>:disconnected`（与真实断连同款符号），绝不伪造健康状态；
  存活判定只认 mcp_pid 身份 + 心跳新鲜度——TCP 端口被占仅作
  “(port held)” 注记，绝不让僵尸端口把陈旧快照洗成健康态。

## 数据契约

与 Python hooks（`hooks/claude/inventory.py`）共用同一份
`schema_version: 1` inventory，语义对齐：

1. 项目根发现：`TARGET_CONF` 显式覆盖 → 从 CWD 逐级向上找
   `.dut-serial/inventory.json` / `.target.jsonc`（`TARGET_EXCLUDE` 排除）；
   工作区跟随 `DSH_TUI_WORKSPACE_TARGET` 与 `tui/session-switched` 事件
   （--resume、/workspace 切换）；
2. 活性门控：`mcp_pid` 存活（`kill(pid, 0)`）→ 否则回退对 `mcp_http_port`
   的 127.0.0.1 TCP 探测 → 都不通过时：`autoStart` 开启则拉起 MCP，并显示
   `✗ <dut>:disconnected` 兜底（服务器活了之后由真实状态接管）；
3. `state_text` 为空的 DUT（unknown/stopped）不渲染 —— 绝不伪造健康状态；
4. 多代理隔离：`.dut-serial/owner.json` 存在且存活的所有者不是当前进程时，
   选择服务端生成的 `guest_display.text`，显示优先级为 `dutabo → busy → 实际状态`。


## 安装

插件本体**随本仓库分发**（`dsh-plugins/dsh-dut-statusline/`），profile 里
只留安装产物：

```bash
# 挂载插件（软链到仓库；CLI 的 reconcile 会把它加进 dsh.profile.bundles）
dsh plugin --profile dsh-tui add link:./dsh-plugins/dsh-dut-statusline
```

- 开发期用 `link:`（软链，改代码即时生效）；分发用 `file:` 或发布 npm。
- 卸载：`dsh plugin --profile dsh-tui remove dsh-dut-statusline`。
- **着色与同排渲染都依赖上游 seam，当前版本不可用**：截至 dsh-tui
  0.10.0-beta.1，上游**没有** `tuiStatus.setStyled`，插件能力探测恒走纯文本
  回退（无颜色，渲染在 prompt 上方的独立 dim 行）；也没有注入底部状态行
  （footer）的公开 seam。两条路径都等上游（statusbar 贡献 seam / 贡献与
  footer 同排的版本），落地前不要声称"已满足同一行要求"。

## 配置

在 `~/.dsh/profiles/dsh-tui/cordis.patch.yml` 覆盖行配置（默认即可用）：

```yaml
- id: dut-statusline
  config:
    tcpTimeoutMs: 300          # TCP 活性探测超时，默认 300
    autoStart: true            # 项目内服务器不在时自动拉起 MCP，默认 true
    autoStartIntervalMs: 60000 # 自动拉起的最小间隔，默认 60000
    autoStartRetryMs: 3000     # 拉起后多久重查一次，默认 3000
    serverBinary: /path/to/sermcp  # 显式指定二进制，默认自动查找
```

## 验证

```bash
node dsh-plugins/dsh-dut-statusline/selftest.mjs   # 全部 OK，exit 0

# 组合树里应出现该行：
dsh --profile dsh-tui --dump-config | grep -A3 dut-statusline
```

## 实现说明

- 纯 ESM JS、零构建、零运行时依赖（仅 `peerDependencies` 声明
  `@deepseek-ai/cordis`，运行时只使用注入的 `ctx`）；
- Cordis 插件三件套：`export const name / inject / apply(ctx, config)`；
  `inject: ['tuiStatus']`（bundle patch 行同样声明）保证组合顺序；
- 每轮 tick 只做一次小 JSON 读取 + 最多一次本地 TCP 连接，不写任何文件；
  自动拉起最多每 `autoStartIntervalMs` 一次（detached、stdio ignore）；
- 所有 I/O 与 `tuiStatus.set/setStyled()` 均被兜底，状态行插件绝不把 TUI
  打崩；
- `ctx.effect` 卸载时清理定时器与贡献，热重载不留残留行；
- 与 dsh-tui 的契约只有 `tuiStatus` seam（`set` + `setStyled` 能力探测）+
  `tui/session-switched` 事件，均是 TUI 的稳定插件面，升级不需要任何适配。
