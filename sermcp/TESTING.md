# MCP 冒烟测试 & 回归测试

## 测试流程

### 1. 部署
```bash
cd sermcp && cargo build --release
cp target/release/sermcp ~/.local/bin/
```

### 2. 启动
```bash
cd ~/project-with-target-conf
nohup ~/.local/bin/sermcp --http > /dev/null 2>&1 &
```

### 3. 冒烟检查 (每次修改代码后必做)
```bash
# 3a. 进程存活
pgrep sermcp

# 3b. 健康检查
curl -s http://localhost:3000/health  # → OK

# 3c. 状态查询
curl -s -X POST http://localhost:3000/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"serial_get_state","arguments":{}}}'
# → state=active|booting|uboot|disconnected|DUT-off

# 3d. 状态文件
cat .dut-serial/target-state
# → 与 API 返回值一致

# 3e. 日志文件
ls .dut-serial/logs/boot-*.log
# → 每次上电一个文件

# 3f. reboot 性能 (必须 < 3s)
time curl ... serial_send_command '{"command":"reboot","timeout":5}'
# → 必须 < 3s 返回 {"output":"reboot sent"}
# ❌ 若 > 3s: wait_readable 持锁过久或 serial_send_command 未走 fast-path
```

### 4. 继电器回归测试 (自动化)
```bash
# 通过继电器复位板子，验证状态流转
serial_reset(wait_boot=true)
# 预期状态流转: active → booting → uboot → booting → active
# 预期日志: 一个 boot-NNN.log 新文件
```

### 5. 心跳检测测试
```bash
# 5a. 板子上电运行中 → 状态 active
# 5b. 断开板子电源
# 5c. 60s 后状态 → DUT-off
watch -n 1 cat .dut-serial/target-state
```

### 6. 常见问题排查

| 现象 | 检查 |
|------|------|
| state=disconnected | `nc -zw1 $RK_DEV_HOST_IP $RK_SERIAL_PORT` |
| state=DUT-off 板子在线 | 检查 ser2net 是否正常 |
| statusline 不同步 | `touch .dut-serial/target-state` 触发 notify（Linux: inotify） |
| 无日志文件 | 检查 `RK_MAX_ARCHIVED_LOGS` 和 `.boot_count` |
| MCP 不启动 | `fuser -k 3000/tcp` 释放端口 |
| reboot 耗时 > 30s | `wait_readable` 持锁 300s 阻塞 HTTP → 改为 1s; `reboot` 加 fast-path 直接发送 |
| HTTP 请求卡死 | 读循环持引擎锁阻塞 → `wait_readable` 1s 超时 + 不持锁等待 |

## 测试结果 (2026-06-12)

### 冒烟检查 ✅
```bash
curl -s http://localhost:3000/health           # → OK
cat .dut-serial/target-state                   # → 与 API serial_get_state 一致
```

### reboot 状态流转 ✅
```bash
# 1. 发送 reboot
serial_send_command("reboot")
# 2. 监控 target-state
watch -n 0.5 cat .dut-serial/target-state
# 预期: active → booting → uboot → booting → active
# 结果: active → booting → uboot → booting → active ✅
# boot_count 递增, 新 boot-NNN.log 生成
```

### 实时日志 ✅
```bash
# current.serial.log 实时写入所有串口数据
tail -f .dut-serial/logs/current.serial.log
# 预期: DDR init → SPL → U-Boot → kernel → shell, 完整启动日志
# 结果: 332KB 完整日志, 实时更新 ✅
```

### enter_uboot ⚠️
```bash
# 继电器复位 + Ctrl-C flood → 进入 U-Boot ✅
serial_enter_uboot()
# 问题: prompt 检测超时 (watcher 在 flood 之后设置)
# 修复: watcher 提前于 relay_reset 设置
# U-Boot 交互: serial_send_command 不支持 U-Boot (需 marker-echo shell)
```

### 心跳探针 ✅
```bash
# 1. 板子 active
# 2. 断开电源
# 3. 60s 后 target-state → DUT-off
watch -n 1 cat .dut-serial/target-state
```

### 测试命令速查
```bash
# 发送命令
curl -s -X POST http://localhost:3000/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
curl -s -X POST http://localhost:3000/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"serial_send_command","arguments":{"command":"uname -a","timeout":10}}}'

# 查询状态
curl ... "tools/call" ... "name":"serial_get_state"
# 进入 U-Boot
curl ... "tools/call" ... "name":"serial_enter_uboot"
```

## 已知修复记录

### 2026-06-12
- P0: `read_once` Err 分支 Disconnected → DUT-off 误判
- P0: ser2net banner 未在 read_once 中过滤
- P0: `flush_boot_log` 每行 shell 都创建日志 (spam)
- P0: `.dut-serial/` 与 `.target.jsonc` 目录分离 bug
- P1: `console.rs` try_write 丢数据 → write_all
- P1: `lock_manager.rs` md5_hash 命名 → fnv1a_hash
- P1: double-rotation: serial_reset + BootStart 各一次
- 事件驱动读循环: 100ms 轮询 → tokio readable() epoll
- 心跳探针: active 无数据 60s → \n 探测 → 3 次 miss → DUT-off
- 文本相似度: strsim::sorensen_dice 比对参考日志
- 内存缓冲: ring buffer → BootStart flush → 一个文件/周期
- Hook 自动启动: `.target.jsonc` 检测 → `dutabo list --json` bootstrap → MCP HTTP + statusline
