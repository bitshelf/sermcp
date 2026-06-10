# sermcp 部署指南

## 概述

`sermcp` 是一个标准的 **MCP (Model Context Protocol) 服务器**，
使用 stdio transport (JSON-RPC 2.0 newline-delimited)。

**不依赖任何特定 Agent** — 只要是 MCP 兼容的客户端都能使用。

## 快速开始 (2 步)

### 第一步: 构建 & 安装

```bash
cd embedded-debug/sermcp
rustup target add x86_64-unknown-linux-musl

# 方式 A: cargo 安装（构建 release，把 sermcp + dutabo 一起装入 ~/.cargo/bin）
cargo install --path . --locked --target x86_64-unknown-linux-musl --force

# 方式 B: 从 GitHub 直接安装（无需克隆仓库）
cargo install --git https://github.com/bitshelf/sermcp --locked --target x86_64-unknown-linux-musl
```

### 第二步: 配置 `.target.jsonc`

在 SDK 项目根目录创建/更新配置:

```bash
dutabo init
# 或从带注释的示例开始:
cp references/.target.jsonc.example .target.jsonc
```

必填: `dev_hosts[0].ip`（ser2net 主机）与 `duts[0].serial.port`（ser2net 端口）;
可选: `target.login_user`（自动登录）、`relay`（继电器复位）。

### 第三步: 选择 Agent 配置

#### Claude Code

`.mcp.json` (项目根目录):
```json
{
  "mcpServers": {
    "sermcp": {
      "command": "sermcp",
      "env": { "RUST_LOG": "info" }
    }
  }
}
```

#### Continue (VS Code / JetBrains)

`~/.continue/config.json`:
```json
{
  "experimental": {
    "mcpServers": [
      {
        "name": "embedded-debug",
        "command": "sermcp",
        "cwd": "/path/to/your/sdk"
      }
    ]
  }
}
```

#### Cursor

`.cursor/mcp.json` (或全局 `~/.cursor/mcp.json`):
```json
{
  "mcpServers": {
    "sermcp": {
      "command": "sermcp",
      "env": { "RUST_LOG": "info" }
    }
  }
}
```

#### Codex (OpenAI)

`codex.yaml`:
```yaml
mcp_servers:
  - name: sermcp
    command: sermcp
    env:
      TARGET_CONF: /path/to/.target.jsonc
```

#### Zed

`.zed/settings.json`:
```json
{
  "mcp": {
    "sermcp": {
      "command": {
        "path": "sermcp",
        "env": {
          "TARGET_CONF": "/path/to/.target.jsonc"
        }
      }
    }
  }
}
```

#### Goose (Block)

`~/.config/goose/mcp.toml`:
```toml
[mcp_servers.embedded-debug]
command = "sermcp"
cwd = "/path/to/sdk"
```

#### 通用 (手动启动)

```bash
# 直接启动 (bash / zsh)
TARGET_CONF=/path/to/.target.jsonc RUST_LOG=info sermcp

# stdin/stdout 就是 JSON-RPC:
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | sermcp
```

### 协议验证

```bash
# 测试 initialize 握手
printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n' | \
  TARGET_CONF=/path/to/.target.jsonc sermcp 2>/dev/null

# 预期输出:
# {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05",...}}

# 列出所有 tools
printf '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}\n' | \
  TARGET_CONF=/path/to/.target.jsonc sermcp 2>/dev/null
```

## 环境变量

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `TARGET_CONF` | `.target.jsonc` 路径 | 从 CWD 向上搜索 |
| `RUST_LOG` | 日志级别 (error/warn/info/debug/trace) | info |

## 工作原理

```
┌──────────────────────────────────────────────────────┐
│ 任何 MCP 客户端 (Claude/Cursor/Continue/Zed/...)      │
│                                                      │
│  1. spawn sermcp 子进程                   │
│  2. stdin → JSON-RPC request                          │
│  3. stdout → JSON-RPC response                        │
└──────────┬───────────────────────────────────────────┘
           │ stdio (pipe)
┌──────────▼───────────────────────────────────────────┐
│ sermcp (Rust binary, 3MB)                 │
│                                                      │
│  mcp_handler.rs → serve(stdio())                     │
│    ├── initialize → protocol version 2024-11-05      │
│    ├── tools/list → serial_* tools                   │
│    └── tools/call → SerialEngine                     │
│         ├── console.rs → TCP → ser2net :2000         │
│         ├── dut-ctrl (CH340) → TCP → ser2net :2001   │
│         ├── boot_detector.rs → 模式匹配              │
│         ├── command_queue.rs → marker echo           │
│         └── log_manager.rs → .dut-serial/logs/       │
└──────────────────────────────────────────────────────┘
```

## MCP Tools

工具目录按 session 角色过滤:代码 Agent 默认精简目录(`serial_exec` 统一命令
入口);`dutabo`/`tui_init`(Human 角色)与 `SERMCP_TOOL_PROFILE=full` 看到
全量。详见 README 的 "Tool Profiles" 一节。

Agent profile(代码 Agent 默认):

| Tool | 说明 |
|------|------|
| `serial_get_state` | 获取目标状态 (active/booting/uboot/crashed/...) |
| `serial_list_duts` | 多 DUT 清单:破坏性操作前确认 `current_dut` |
| `serial_get_logs` | 检索串口日志 (支持行数限制 + 正则过滤) |
| `serial_poll_logs` | 增量获取新输出 (cursor + long-poll) |
| `serial_exec` | 统一命令入口: mode=auto/shell/uboot |
| `serial_reset` | 继电器硬件复位 + 日志切割(需 reset 控制) |
| `serial_power_cycle` | 断电再上电(需 power 通道) |
| `serial_enter_uboot` | 强制进入 U-Boot 交互提示符 |
| `serial_wait_pattern` | 阻塞等待指定正则模式出现 |
| `serial_button` | 控制 reset/recovery/maskrom 按钮(需按钮配置) |
| `serial_flash_plan` | 生成烧录计划(不执行烧录) |
| `serial_claim` | 恢复无人认领/陈旧的串口锁 |

Full profile 追加(init/bring-up 与内部 CLI):

| Tool | 说明 |
|------|------|
| `serial_send_command` | 执行 shell 命令 (marker echo 模式) — Agent 用 `serial_exec` |
| `serial_uboot_command` | 在 U-Boot 提示符下发送命令 — Agent 用 `serial_exec(mode="uboot")` |
| `serial_send_raw` | 发送原始串口字节(逃生通道) |
| `serial_list_logs` | 列出所有归档启动日志 |
| `serial_get_config` | 获取当前目标配置 |
| `serial_get_metrics` | 获取引擎运行指标 |
| `serial_load_reference` | 加载参考日志并返回已学习阶段摘要 |
| `serial_append_reference` | 追加锚点行 + 热重载 StageLearner |
| `serial_learn_connection` | 多次复位学习连接稳定性 |
| `serial_verify_relay` | 读回验证继电器 |
| `serial_test_baud` / `serial_set_baud` | 波特率验证 / RFC 2217 动态切换 |

## 故障排除

```bash
# 查看帮助
sermcp --help

# 调试模式 (日志输出到 stderr)
sermcp --log-to-stderr -v

# 手动测试连接
nc -zv 192.168.1.231 2000

# 模拟 MCP 客户端
TARGET_CONF=.target.jsonc sermcp --log-to-stderr -v
```
