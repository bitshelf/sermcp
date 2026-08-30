---
name: embedded-debug
description: >-
  Operate and diagnose an embedded device through the sermcp managed serial
  console: check device status, read serial output, run a command, restart,
  power-cycle, enter U-Boot, control a configured button, prepare a flash plan,
  and explain a failed boot, kernel panic or Oops, boot timeout, disappearing
  serial output, or wrong boot-stage detection. Use only when the current
  project or a parent directory contains .target.jsonc. Do not activate in
  projects without .target.jsonc.
---

# Embedded Device Console and Diagnosis

## Activation boundary

Search from the current directory upward for `.target.jsonc` before calling any
serial tool. If the file is absent, explain that this plugin is unavailable for
the project. Do not create configuration unless the user explicitly asks to set
it up.

Treat `.target.jsonc` as user-owned configuration: read it when needed, but do
not edit it during debugging. Use the MCP tools for agent actions. `dutabo` is
the human-facing setup and interactive-console program; run it only when the
user explicitly asks for a CLI workflow. Never open the configured serial
endpoint with `nc`, `tio`, `screen`, or a second serial client while sermcp
manages it. Never terminate sermcp processes globally.

## Start every operation safely

1. Call `serial_get_state({})`. If the state is `connecting`, retry until startup
   finishes before calling tools that write to the device.
2. Before reset, power cycle, button, U-Boot entry, or any flash action, call
   `serial_list_duts({})`. Confirm that `current_dut` and the selected entry's
   `dut_name` identify the device the user intends to control. If more than one
   device is configured and the request does not identify one, ask the user
   which device to control before continuing.
3. The advertised tool list already reflects the DUT's configured capabilities
   (relay, buttons, RFC 2217, flash). When an operation depends on further
   configuration details, read `.target.jsonc` directly instead of probing with
   tool calls.
4. If a call returns an asynchronous MCP task, wait for that task to finish or
   fail, then verify the result with `serial_get_state` and fresh logs.

When another code agent owns the serial endpoint, state, configuration,
inventory, and log tools remain useful. Do not call `serial_claim` merely
because a live owner exists. Call it only when ownership is unclaimed or stale
and this agent needs write access; it must never displace a live owner.

## Routine operations

| User intent | MCP call |
| --- | --- |
| Check whether the device is online | `serial_get_state({})` |
| Read recent serial output | `serial_get_logs({"archive":0,"lines":50})` |
| Run a console command (shell or U-Boot) | `serial_exec({"command":"...","mode":"auto","timeout":90})` |
| Restart Linux through its shell | `serial_exec({"command":"reboot","timeout":5})` |
| Restart through configured reset control | `serial_reset({"wait_boot":true})` |
| Power the device off and back on | `serial_power_cycle({"wait_boot":true})` |
| Enter U-Boot | `serial_enter_uboot({"restart":"auto"})` |
| Run a command at the U-Boot prompt | `serial_exec({"command":"...","mode":"uboot","timeout":15})` |
| Wait for serial text | `serial_wait_pattern({"pattern":"...","timeout":60})` |
| Press a configured button | `serial_button({"button":"...","action":"pulse"})` |
| Inspect a firmware operation | `serial_flash_plan({"image_path":"...","image_type":"full"})` |

`serial_flash_plan` only describes the planned operation; it does not flash the
device. Use `image_type: "kernel"` for a kernel or boot image.

`serial_exec` is the single command entry: `mode=auto` (default) picks the
protocol from the detected target state, so it works at a Linux shell prompt and
at a U-Boot prompt alike. In shell mode treat the returned `output`,
`exit_code`, and `timed_out` fields as the result; the response also carries the
`mode` that ran. A queued write by itself does not prove that the device
received or completed the command. If the detected state is stale, pass
`mode:"shell"` or `mode:"uboot"` explicitly.

The configured `uboot.interrupt_char` is the byte used to interrupt autoboot.
`ctrl_c` means byte `0x03`.

## Diagnosis

Reach for this section when the user wants an explanation — a failed boot, a
kernel crash, disappearing serial output, or incorrect boot-stage detection —
rather than a routine action.

Start with current evidence:

1. Call `serial_get_state({})` and record `dut_name`, state, and connection
   errors.
2. Read the current boot cycle with
   `serial_get_logs({"archive":0,"lines":200})`.
3. List boot archives via the MCP resource listing (`log://boot/{index}`) before
   comparing archived boots. In `serial_get_logs`, archive `0` is the current
   cycle and archive `1` is the newest completed archive.
4. Separate observed log evidence from hypotheses. Do not claim that a command,
   reset, or boot completed without its returned result or fresh serial output.

Do not reset, power-cycle, press buttons, or change U-Boot state unless the
user's requested diagnosis requires that action.

For a boot failure, filter the current log after reading its surrounding
context:

```text
serial_get_logs({
  "archive": 0,
  "lines": 200,
  "pattern": "panic|BUG|Oops|Call trace|watchdog|timeout|fail|error",
})
```

For live boot monitoring, carry the `since` cursor returned by
`serial_poll_logs` into the next call. Recheck `serial_get_state` after new
output. Stop when the requested condition is met or the state becomes `active`,
`crashed`, `DUT-off`, or `disconnected`; do not poll indefinitely.

For a clean reset-and-monitor run: confirm the device with
`serial_list_duts({})`, call `serial_reset({"wait_boot":true})`, wait for any
asynchronous task, then read the new cycle with `serial_get_logs` and confirm
the final state with `serial_get_state`.

For U-Boot diagnosis, call `serial_enter_uboot({"restart":"auto"})`, verify that
the returned state is U-Boot, then keep `mode:"uboot"` explicit on `serial_exec`
— the shell-mode marker protocol does not work at the U-Boot prompt.

Use boot-stage learning only when the task calls for adapting detection to a new
device or correcting classifications. The learning tools
(`serial_load_reference`, `serial_append_reference`) belong to the server's init
profile; when this session's tool list does not advertise them, ask the user to
run the bring-up flow (`dutabo init`) instead of improvising. When they are
advertised, `serial_load_reference` requires an absolute reference-log path, and
`serial_append_reference` should only receive stable, distinctive boot-stage
anchors; omit timestamps, addresses, and random values.
