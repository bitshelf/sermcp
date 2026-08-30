---
description: Cold-reset the DUT and stream the boot log until login prompt. serial_reset(wait_boot=true) runs as a long-running MCP task, so the call stays responsive.
argument-hint: "[dut-alias]"
---

Cold-reset the target DUT and watch a full boot. The server materializes
`serial_reset(wait_boot=true)` into a long-running MCP task (SEP-2663), so
you stay responsive while the DUT boots:

1. Call `serial_reset({"wait_boot": true})` — the call returns immediately with the task state.
2. Poll every ~10s with `serial_get_state`; stop when it reports `active`, `crashed`, or the task completes.
3. On failure, call `serial_get_logs({"pattern": "panic|BUG|Oops|Call trace"})` to fetch the crash section and diagnose.
4. On success, report `serial_get_state` and any new boot anomalies vs the reference log.
