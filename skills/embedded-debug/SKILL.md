---
name: embedded-debug
description: >-
  Diagnose an embedded device that hangs, crashes, or fails to boot by
  using its managed serial logs and boot-stage state. Use only when the current
  project or a parent directory contains .target.jsonc. Covers kernel panic and
  Linux kernel panic and Oops analysis, boot-timeout diagnosis,
  reset-and-monitor workflows, and
  reference-log learning. Do not use for routine serial commands or projects
  without .target.jsonc.
---

# Boot and Crash Diagnosis

Use this skill when the user wants an explanation for a failed boot, kernel
crash, disappearing serial output, or incorrect boot-stage detection. Use
`serial-debug` for a routine status check, command, restart, U-Boot operation,
button action, or flash plan.

## Activation and evidence

Search from the current directory upward for `.target.jsonc`. If it is absent,
do not call sermcp tools or create configuration implicitly.

Start with current evidence:

1. Call `serial_get_state({})` and record `dut_name`, state, and connection
   errors.
2. Read the current boot cycle with
   `serial_get_logs({"archive":0,"lines":200,"highlighted":false})`.
3. Use `serial_list_logs({})` before comparing archived boots. In
   `serial_get_logs`, archive `0` is the current cycle and archive `1` is the
   newest completed archive.
4. Separate observed log evidence from hypotheses. Do not claim that a command,
   reset, or boot completed without its returned result or fresh serial output.

Do not reset, power-cycle, press buttons, or change U-Boot state unless the
user's requested diagnosis requires that action. Before any such action, call
`serial_list_duts({})` and confirm `current_dut` identifies the intended device.

## Diagnostic workflows

For a boot failure, filter the current log after reading its surrounding
context:

```text
serial_get_logs({
  "archive": 0,
  "lines": 200,
  "pattern": "panic|BUG|Oops|Call trace|watchdog|timeout|fail|error",
  "highlighted": false
})
```

For live boot monitoring, carry the `since` cursor returned by
`serial_poll_logs` into the next call. Recheck `serial_get_state` after new
output. Stop when the requested condition is met or the state becomes
`active`, `crashed`, `DUT-off`, or `disconnected`; do not poll indefinitely.

For a clean reset-and-monitor run:

1. Confirm the device with `serial_list_duts({})`.
2. Call `serial_reset({"wait_boot":true})`.
3. If the client returns an asynchronous task, wait for its completion.
4. Read the new cycle with `serial_get_logs` and confirm the final state with
   `serial_get_state`.

For U-Boot diagnosis, call `serial_enter_uboot({"restart":"auto"})`, verify
that the returned state is U-Boot, then use `serial_uboot_command`. Never use
`serial_send_command` at the U-Boot prompt.

Use boot-stage learning only when the task calls for adapting detection to a
new device or correcting classifications. `serial_load_reference` requires an
absolute reference-log path. Add lines with `serial_append_reference` only when
they are stable, distinctive boot-stage anchors; omit timestamps, addresses,
and random values.

When another live code agent owns the endpoint, continue with state and log
inspection. Do not steal ownership or terminate the owner. Use `serial_claim`
only for an unclaimed or stale lock when the diagnosis needs write access.
