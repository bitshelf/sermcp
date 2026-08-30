---
description: Monitor an embedded Linux DUT boot cycle with managed reset and incremental serial polling
---
Start a monitored boot cycle on the target DUT and report the outcome.

## Steps

1. Verify the sermcp server is available:

   `serial_get_state`

   If the state is `connecting`, retry until startup finishes. In multi-DUT
   projects, call `serial_list_duts` first and confirm `current_dut` names
   the device the user intends to reset.

2. Start a monitored boot cycle:

   - With reset control configured: `serial_reset(wait_boot=true)` —
     task-capable hosts receive a pollable MCP Task automatically; hosts
     without tasks support (the pi bridge) get one bounded synchronous
     call that returns when boot completes or times out. There is no
     separate task-start tool.
   - Without reset control: `serial_exec({"command":"reboot","timeout":5})`
     and monitor manually.

3. Monitor progress with incremental polling:

   `serial_poll_logs({"since":<cursor>})`

   Carry the returned `since` cursor into the next call. Recheck
   `serial_get_state` after new output. Stop when the state becomes
   `active`, `crashed`, `DUT-off`, or `disconnected`; do not poll
   indefinitely.

4. On completion, report:

   - Boot time
   - Stage transitions observed
   - Any warnings or errors in the boot log
   - Final DUT state (active/crashed/DUT-off)

5. If boot fails:
   - Capture crash logs: `serial_get_logs({"archive":0,"lines":200,"pattern":"panic|BUG|Oops|Call trace"})`
   - Report the failure stage and crash details to the user

## Important

- The first `serial_exec` after MCP start must be a warmup: `serial_exec({"command":"echo warmup","timeout":3})`
- BusyBox targets may need `printf` instead of `echo` in piped commands
- Do NOT use raw `nc`/`tio`/`screen` to access the serial port
