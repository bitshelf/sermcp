---
name: serial-debug
description: >-
  Operate an embedded device through the sermcp managed serial console.
  Use when the current project or a parent directory contains .target.jsonc
  and the user asks to check device status, read serial output, run a command,
  restart, enter U-Boot, control a configured button, or prepare a flash plan.
  Do not activate in projects without .target.jsonc.
---

# Device Serial Console

Use this skill for direct serial-console operations. Use `embedded-debug`
instead when the main task is diagnosing a boot hang, kernel crash, or changing
boot-stage detection.

## Activation boundary

Search from the current directory upward for `.target.jsonc` before calling a
serial tool. If the file is absent, explain that this plugin is unavailable for
the project. Do not create configuration unless the user explicitly asks to
set it up.

Treat `.target.jsonc` as user-owned configuration. Read it when needed, but do
not edit it during debugging. Use the MCP tools for agent actions. `dutabo` is
the human-facing setup and interactive-console program; run it only when the
user explicitly asks for a CLI workflow.

Never open the configured serial endpoint with `nc`, `tio`, `screen`, or a
second serial client while sermcp manages it. Never terminate sermcp processes
globally.

## Start every operation safely

1. Call `serial_get_state({})`.
2. If the state is `connecting`, retry `serial_get_state` until startup finishes
   before calling tools that write to the device.
3. Before reset, power cycle, button, U-Boot entry, or any future flash action,
   call `serial_list_duts({})`. Confirm that `current_dut` and the selected
   entry's `dut_name` identify the device the user intends to control. If more
   than one device is configured and the request does not identify one, ask the
   user which device to control before continuing.
4. Call `serial_get_config({})` only when the operation depends on relay,
   U-Boot, baud-rate, or flash configuration.

State, configuration, inventory, and log tools remain useful when another code
agent owns the serial endpoint. Do not call `serial_claim` merely because a
live owner exists. Call it only when ownership is unclaimed or stale and this
agent needs write access; it must never displace a live owner.

## Choose the matching tool

| User intent | MCP call |
| --- | --- |
| Check whether the device is online | `serial_get_state({})` |
| Read recent serial output | `serial_get_logs({"archive":0,"lines":50})` |
| Run a Linux shell command | `serial_send_command({"command":"...","timeout":90})` |
| Restart Linux through its shell | `serial_send_command({"command":"reboot","timeout":5})` |
| Restart through configured reset control | `serial_reset({"wait_boot":true})` |
| Power the device off and back on | `serial_power_cycle({"wait_boot":true})` |
| Enter U-Boot | `serial_enter_uboot({"restart":"auto"})` |
| Run a command at the U-Boot prompt | `serial_uboot_command({"command":"...","timeout":15})` |
| Wait for serial text | `serial_wait_pattern({"pattern":"...","timeout":60})` |
| Press a configured button | `serial_button({"button":"...","action":"pulse"})` |
| Inspect a firmware operation | `serial_flash_plan({"image_path":"...","image_type":"full"})` |

`serial_flash_plan` only describes the planned operation; it does not flash the
device. Use `image_type: "kernel"` for a kernel or boot image.

For `serial_send_command`, treat the returned `output`, `exit_code`, and
`timed_out` fields as the result. A queued write by itself does not prove that
the device received or completed the command. Use `serial_send_raw` only when an
exact byte sequence is required and a structured command tool cannot do the
job.

At a U-Boot prompt, use `serial_uboot_command`; the Linux-shell marker protocol
used by `serial_send_command` does not work there. The configured
`uboot.interrupt_char` is the byte used to interrupt autoboot. `ctrl_c` means
byte `0x03`.

If a reset call returns an asynchronous MCP task, wait for that task to finish
or fail. Otherwise, verify the result with `serial_get_state` and fresh logs.
