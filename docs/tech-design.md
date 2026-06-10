# Serial Debug MCP 技术设计

##  目标

本设计将 `sermcp` 扩展为一套可被 Agent 和人工 CLI 同时使用的控制与调试系统：

- 将 reset、download 抽象为“按下/松开”的按钮控制。
- 目前支持使用 CH340 继电器进行。
- 支持同时调试多天设备。
- 提供手改操作的 `dutabo` 命令，进行串口的控制。
- Agent 只能读取 `.target.jsonc`，不得生成或修改该文件（创建/更新用 `dutabo init`）。

## 总体架构

```text
Agent / Claude Code                 Developer CLI
MCP tools                           dutabo
      |                                |
      +---------------+----------------+
                      |
              Serial Debug Engine
        +-------------+--------------+
        | config registry            |
        | serial engine              |
        | state manager              |
        | power-control abstraction  |
        +-------------+--------------+
                      |
             Dev Host / ser2net / ssh
        +-------------+--------------+
        | UART TCP port              |
        | relay TCP port             |
        | upgrade_tool/phoenixconsole|
        +-------------+--------------+
                      |
                     DUT
```

关键原则：

- 一个项目目录只能运行一个 MCP 进程；多个 Code Agent 共享该进程时，首个会话
  可操作，后续会话由 MCP 服务端强制为只读。
- 串口连接由 MCP engine 长期持有；CLI 查看实时日志时通过 MCP/HTTP 或日志文件订阅。
- 硬件控制能力是可选能力；缺失时明确降级。
- `.target.jsonc`（JSONC，支持 `//` 注释）是唯一人工配置入口；Agent 只读。
- 日志分割基于学习得到的参考文本，正则仅用于崩溃、登录提示、U-Boot 提示符等强语义事件。

## 配置设计

### 配置发现

1. `TARGET_CONF` 环境变量显式指定（设置但文件缺失 → 报错）。
2. 从code Agent 启动目录查找 `.target.jsonc`；未找到 → 纯"缺少配置"错误。
3. 严格 JSONC 解析（禁止单引号/十六进制数/未知键，拼写错误立即报错）；
   解析失败时报错退出，绝不静默回退。

`dutabo init` 自动创建开发环境信息
SessionStart hook 也会在生成 `.mcp.json` 后启动 MCP。

### 只读约束

Agent 禁止生成、修改 `.target.jsonc`。允许操作：

- 读取配置。
- 生成 `.mcp.json`。
- 生成 `.dut-serial/*` 下的运行时文件。
- 使用继电器控制目标设备，生成学习日志和 `reference_log` 指向的参考日志文件。

### 单 DUT 示例

```jsonc
// Embedded Debug Target Configuration
// Agent must not edit this file. Create/update via `dutabo init`.

{
  "dev_hosts": [
    {
      "ip": "192.168.1.105",
      "user": "linaro",
      "pass": "",
      "duts": [
        {
          "dut_name": "rk3576-a",
          "serial": {
            "ip": "",         // empty means parent dev host ip
            "port": 2000
          },
          "relay": {
            "ip": "",         // empty means parent dev host ip
            "port": 2001,
            "reset_ch": 1
            "download_ch": 2     // the "download" row in the form: the
                                 // download/FEL key (reset + this = the
                                 // hardware entry sandwich)
          },
          "target": {
            "login_user": "root",
            "login_pass": ""
          },
          "monitor": {
            "hang_timeout": 60,
            "max_archived_logs": 10,
            "reference_log": ".dut-serial/reference-boot.log"
          },
          "flash": {
            "tool": "upgrade_tool",
            "upload_dir": "/tmp",
            "full_image_cmd": "uf {image}",
            "kernel_image_cmd": "di -k {image}",
            "loader_bin": "/opt/rockchip/rk3576_loader.bin",
            "loader_cmd": "db {loader}"
          }
        }
      ]
    }
  ]
}
```

###  多 DUT 示例

```jsonc
{
  "dev_hosts": [
    {
      "ip": "192.168.1.105",
      "user": "linaro",
      "pass": "",
      "duts": [
        {
          "dut_name": "rk3576-a",
          "serial": { "port": 2000 },
          "relay": { "port": 2001, "reset_ch": 1, "download_ch": 2 },
          "monitor": { "reference_log": ".dut-serial/rk3576-a/reference-boot.log" }
        },
        {
          "dut_name": "rk3588-b",
          "serial": { "port": 2010 },
          "relay": { "port": 2011, "reset_ch": 1 },
          "monitor": { "reference_log": ".dut-serial/rk3588-b/reference-boot.log" }
        }
      ]
    }
  ]
}
```

## 连接学习流程

### 硬件 reset 学习

流程：

1. 按下 reset。
2. 创建学习日志文件：`.dut-serial/learn/learn-<timestamp>-<n>.log`。
3. 松开 reset。
4. 捕获启动日志并写入文件。
5. 重复三次。
6. 比较三个文件开头部分文本相似度。
7. 如果三者两两相似度均大于 93%，保留最后一次文本为 `reference_log`。
8. 输出日志分割参考路径，判定连接建立。

相似度：

- 读取每个文件前开头部分(例如50)行。
- 使用 `strsim::jaro_winkler` 与 n-gram Jaccard 加权：
  `score = jaro_winkler * 0.6 + jaccard * 0.4`。
- 三个文件的最低两两分数作为本轮分数。

### 继电器不可用判定

学习阶段额外判定：

- 控制 reset/power 按下时，串口应该没有日志输出,松开才有日志输出, 否则就判定继电器不可用。
- 不对继电器强依赖。

如果没有 reset 继电器但已有 `reference_log`：

1. 执行 `reboot`。
2. 可以通过 MCP 获取到新的日志，判定连接建立。

## 日志分割设计

运行时日志分为三类：

- `full.serial.log`：连续全量日志，不截断。
- `current.serial.log`：当前 boot cycle。
- `boot-<n>_<timestamp>.log`：归档日志。

分割触发：

- 学习参考匹配到启动开头。
- DDR/SPL/U-Boot 早期启动锚点出现。
- reset/reboot 控制动作开始。

## 状态机与 Agent 通知

状态集合：

- `active`：目标可交互。
- `booting`：正在启动。
- `uboot`：处于 U-Boot 交互提示符。
- `crashed`：检测到 Kernel panic/BUG/Oops。
- `DUT-off`：目标无输出或心跳失败。
- `disconnected`：串口 TCP/ser2net 不可达。

事件来源：

- 串口输出。
- watchdog。
- reset/reboot/flash 控制动作。
- crash 正则：`Kernel panic`、`BUG:`、`Oops`、`Call trace`。

通知方式：

- 写 `.dut-serial/target-state`。
- 写 `.dut-serial/statusline-cache`。
- 写 `.dut-serial/inventory.json`（原子 tmp+rename；Python hooks 的唯一数据源，
  含 per-DUT 预格式化 `state_text`/`state_plain`、`critical` 标记和端口信息）。
- hook 在用户提交 prompt 前读取状态。
- 当状态为 `crashed`、`DUT-off`、`disconnected` 时，主动提示 Agent 下一步处理。

## 手工 CLI：`dutabo`

### 设计目标

CLI 运行于目根路径，读取 `.target.jsonc`：

```bash
dutabo list [--json]
dutabo status [--dut <board_name>] [--watch]
dutabo serial [--dut <dut_name>]
dutabo button <name> <press|release|pulse> [delay_ms]
```

### 不挤掉 Agent 连接

实现方式：

- 优先连接本项目 MCP HTTP 端口。
- 如果 MCP 未运行，CLI 可启动 HTTP MCP。
- `serial` 实时日志通过 `serial_poll_logs` 或读取 `current.serial.log` 实现。
- 不直接打开 UART TCP 端口，避免抢占 Agent 的串口连接。


### 执行边界

MCP/CLI 不内置 SoC 细节。具体烧录命令由 `.target.jsonc` 定义，从而支持不同 SoC 和工具。

## notify 事件驱动

仅监听 `target-state`，让 hook 和 CLI 即时感知 DUT 状态变化，避免轮询。
实现使用跨平台 `notify` crate；Linux 上由其 inotify 后端提供事件。

`.target.jsonc` 对 Agent 只读，缺失时 MCP 启动失败；watcher 监听配置变化。
串口日志由现有流式通道处理，也不纳入文件 watcher。

限制：

- `.target.jsonc` 不由 Agent 修改，由用户通过 `dutabo init` 修改并确认。
- 会改变连接拓扑的字段不热更新，要求重启 MCP。

## MCP Tasks（SEP-2663）

`serial_reset(wait_boot=true)` 是当前的长任务入口。声明
`io.modelcontextprotocol/tasks` capability 的客户端会立即收到
`resultType: "task"` 句柄，随后用 `tasks/get` 轮询；未声明 Tasks 的客户端
走同步兼容路径。`wait_boot=false` 属于短操作，仍直接返回普通工具结果。

任务状态机直接复用 rmcp：`tasks/update` 忽略未知或已经消费的 input key；
`tasks/cancel` 只确认取消意图，worker 真正观察到取消后才进入 `cancelled`。
工具运行失败以 `completed + result.isError=true` 返回，只有 JSON-RPC 执行错误
使用 `failed`。每个 MCP 进程只允许一个 DUT 长任务并发运行。

### 目标板控制

- [ ] reset/download 都通过 press/release 布尔抽象。
- [ ] `.target.jsonc` 未定义通道时对应能力不存在。
- [ ] 控制后端通过 plugin 注册表接入（`register_backend`）。

### Agent

- [ ] Agent 不生成、不修改 `.target.jsonc`。
- [ ] `.target.jsonc` 存在但 `.mcp.json` 不存在时自动生成 MCP 配置。
- [ ] 任意 Agent 命令通过 MCP 串口工具执行。
- [ ] Kernel panic/BUG/Oops 进入 `crashed` 状态。
- [ ] `crashed`、`DUT-off`、`disconnected` 主动通知 Agent。

### 事件驱动与通知完善

- inotify 监听日志、状态、配置。
- hook 主动提示 Agent 状态变化。
- 完善 HTTP MCP 与 CLI 共用连接。
