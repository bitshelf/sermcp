//! Serial port simulator — mimics a Linux development board's serial console.
//!
//! Listens on a TCP port (like ser2net) and behaves like a login shell:
//! - Boot banner + login prompt
//! - Shell prompt with hostname
//! - Echoes typed characters
//! - Responds to built-in commands (echo, ls, cat, dmesg, etc.)
//! - Mimics shell quoting and compound commands, so the engine's labgrid-style
//!   wrapped command line (`echo '<m1>''<m2>'; { cmd; }; echo "EXIT:$?"; ...`)
//!   executes end-to-end against the simulator
//! - Can inject fake kernel boot logs
//!
//! ## Usage
//!
//! ```bash
//! # Basic: login shell on port 9999
//! serial-sim
//!
//! # With boot log replay + login shell on port 9999
//! serial-sim --boot-log
//!
//! # Custom port and hostname
//! serial-sim --port <configured-port> --hostname dut1
//!
//! # Raw mode: no login, just shell prompt (for testing dutabo serial)
//! serial-sim --no-login
//! ```
//!
//! ## Test with dutabo
//!
//! ```bash
//! # Terminal 1
//! serial-sim --port <configured-port>
//!
//! # Terminal 2 — config points to localhost:<configured-port>
//! dutabo serial
//! ```

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const DEFAULT_PORT: u16 = 9999;

// ── Fake filesystem ────────────────────────────────────────────────────────

struct FakeFS {
    files: HashMap<String, String>,
}

impl FakeFS {
    fn new(hostname: &str) -> Self {
        let mut files = HashMap::new();
        files.insert(
            "/proc/version".into(),
            "Linux version 5.15.147 (tao@tao-virtual-machine) \
             (aarch64-none-linux-gnu-gcc 10.3.1) #29 SMP PREEMPT \
             Thu Nov 13 17:22:05 CST 2025\n"
                .into(),
        );
        files.insert(
            "/proc/cpuinfo".into(),
            "processor\t: 0\nBogoMIPS\t: 48.00\nFeatures\t: fp asimd evtstrm aes \
             pmull sha1 sha2 crc32\nCPU implementer\t: 0x41\nCPU architecture: 8\n\
             CPU variant\t: 0x2\nCPU part\t: 0xd05\nCPU revision\t: 0\n"
                .into(),
        );
        files.insert(
            "/proc/meminfo".into(),
            "MemTotal:        1924996 kB\nMemFree:         1500000 kB\n\
             MemAvailable:    1700000 kB\nBuffers:           50000 kB\n\
             Cached:           100000 kB\n"
                .into(),
        );
        files.insert("/etc/hostname".into(), format!("{hostname}\n"));
        Self { files }
    }

    fn read(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(|s| s.as_str())
    }
}

// ── Boot log generator ─────────────────────────────────────────────────────

const BOOT_LOG: &str = r#"[    0.000000] Booting Linux on physical CPU 0x0000000000 [0x412fd050]
[    0.000000] Linux version 5.15.147 (tao@tao-virtual-machine) (aarch64-none-linux-gnu-gcc (GNU Toolchain for the A-profile Architecture 10.3-2021.07 (arm-10.29)) 10.3.1 20210621, GNU ld (GNU Toolchain for the A-profile Architecture 10.3-2021.07 (arm-10.29)) 2.36.1.20210621) #29 SMP PREEMPT Thu Nov 13 17:22:05 CST 2025
[    0.000000] Machine model: sun55iw3
[    0.000000] Zone ranges:
[    0.000000]   DMA      [mem 0x0000000040000000-0x00000000bfffffff]
[    0.000000] psci: PSCIv1.1 detected in firmware.
[    0.068248] Detected VIPT I-cache on CPU4
[    0.070044] smp: Brought up 1 node, 8 CPUs
[    0.206583] SMP: Total of 8 processors activated.
[    0.762519] sunxi:ccu_sunxi_ng:[INFO]: rtc_ccu: sunxi ccu init OK
[    2.142047] iommu: Default domain type: Translated
[    2.154284] SCSI subsystem initialized
[    2.891909] sunxi:sunxi_mmc_host-4022000.sdmmc:[INFO]: SD/MMC/SDIO Host Controller Driver(v5.48 2024-07-17 16:19)
[    3.330066] mmcblk0: mmc0:0001 8GUF4R 7.28 GiB
[    3.333140]  mmcblk0: p1 p2 p3 p4
[    3.333767] mmcblk0rpmb: mmc0:0001 8GUF4R 4.00 MiB, chardev (241:0)
[    4.329162] mmc0: new HS200 MMC card at address 0001
[    5.477194] xhci-hcd xhci-hcd.23.auto: xHCI Host Controller
[    6.280812] In-situ OAM (IOAM) with IPv6
[    8.281880] EXT4-fs (mmcblk0p4): mounted filesystem with ordered data mode.
[   10.045413] EXT4-fs (mmcblk0p4): re-mounted. Opts: (null). Quota mode: disabled.
[   11.000000] Welcome to Embedded Linux
"#;

// ── Shell simulator ────────────────────────────────────────────────────────

struct Shell {
    hostname: String,
    fs: FakeFS,
    boot_time: Instant,
    line_buf: String,
    cursor: usize,
    echo: bool,
    logged_in: bool,
    login_user: String,
    /// Exit status of the most recently executed statement (for `$?`).
    last_status: u32,
}

impl Shell {
    fn new(hostname: &str) -> Self {
        Self {
            hostname: hostname.to_string(),
            fs: FakeFS::new(hostname),
            boot_time: Instant::now(),
            line_buf: String::new(),
            cursor: 0,
            echo: true,
            logged_in: false,
            login_user: String::new(),
            last_status: 0,
        }
    }

    fn prompt(&self) -> String {
        if !self.logged_in {
            format!("\r\n{} login: ", self.hostname)
        } else {
            format!("\r\n{}@{}:/# ", self.login_user, self.hostname)
        }
    }

    fn initial_prompt(&self) -> String {
        if !self.logged_in {
            format!("{} login: ", self.hostname)
        } else {
            format!("{}@{}:/# ", self.login_user, self.hostname)
        }
    }

    /// Process a line of input, return output.
    ///
    /// Statements are split on unquoted `;` and `{ ... }` compound-command
    /// braces are stripped, so the engine's wrapped command line executes:
    ///
    /// ```text
    /// echo 'ABCD''EFGHJK'; { uname -a; }; echo "EXIT:$?"; echo 'ABCD''EFGHJK'
    /// ```
    ///
    /// A single prompt is appended after the last statement.
    fn process_line(&mut self, line: &str) -> String {
        let line = line.trim();
        if line.is_empty() {
            return self.prompt();
        }

        let mut out = String::new();
        let mut group_depth: u32 = 0;
        for stmt in split_statements(line) {
            let mut stmt = stmt.trim();
            while let Some(rest) = stmt.strip_prefix('{') {
                stmt = rest.trim();
                group_depth += 1;
            }
            while group_depth > 0
                && let Some(rest) = stmt.strip_suffix('}')
            {
                stmt = rest.trim();
                group_depth -= 1;
            }
            if stmt.is_empty() {
                continue;
            }
            let (stmt_out, status) = self.exec_simple(stmt);
            out.push_str(&stmt_out);
            self.last_status = status;
        }
        out.push_str(&self.prompt());
        out
    }

    /// Execute one simple command (no `;`, grouping braces already stripped).
    /// Returns `(output_without_prompt, exit_status)`.
    fn exec_simple(&mut self, stmt: &str) -> (String, u32) {
        let parts: Vec<&str> = stmt.splitn(2, ' ').collect();
        let cmd = parts[0];
        let arg = parts.get(1).unwrap_or(&"").trim();

        match cmd {
            // Login flow
            "login" | "root" if !self.logged_in => {
                self.logged_in = true;
                self.login_user = if cmd == "login" && !arg.is_empty() {
                    arg.to_string()
                } else {
                    "root".to_string()
                };
                ("\r\nPassword: \r\n".to_string(), 0)
            }

            // Built-in commands
            "echo" => {
                // Mimic shell echo: quotes are removed and adjacent quoted
                // segments concatenate, so a quote-split marker such as
                // 'ABCD''EFGHJK' prints as one continuous string. `$?`
                // expands outside single quotes (engine exit-code line).
                let words = shell_expand_words(arg, self.last_status);
                (format!("\r\n{}", words.join(" ")), 0)
            }
            "cat" => {
                if arg.is_empty() {
                    ("\r\n".to_string(), 0)
                } else if let Some(content) = self.fs.read(arg) {
                    (format!("\r\n{content}"), 0)
                } else {
                    (format!("\r\ncat: {arg}: No such file or directory"), 1)
                }
            }
            "ls" => {
                let target = if arg.is_empty() { "/" } else { arg };
                let listing = match target {
                    "/" => "bin\r\ndev\r\netc\r\nproc\r\ntmp\r\nusr\r\nvar",
                    "/proc" => "cpuinfo\r\nmeminfo\r\nversion",
                    "/etc" => "hostname",
                    _ => "",
                };
                (format!("\r\n{listing}"), 0)
            }
            "dmesg" => {
                let lines: Vec<&str> = BOOT_LOG.lines().take(20).collect();
                (format!("\r\n{}", lines.join("\r\n")), 0)
            }
            "dmesg-full" => (format!("\r\n{}", BOOT_LOG.trim()), 0),
            "uptime" => {
                let secs = self.boot_time.elapsed().as_secs();
                let mins = secs / 60;
                let hours = mins / 60;
                (
                    format!(
                        "\r\n{:02}:{:02}:{:02} up {} min, load average: 0.00, 0.00, 0.00",
                        hours % 24,
                        mins % 60,
                        secs % 60,
                        mins,
                    ),
                    0,
                )
            }
            "whoami" => (format!("\r\n{}", self.login_user), 0),
            "hostname" => (format!("\r\n{}", self.hostname), 0),
            "pwd" => ("\r\n/".to_string(), 0),
            "uname" => {
                let out: String = match arg {
                    "-a" => format!(
                        "Linux {} 5.15.147 #29 SMP PREEMPT Thu Nov 13 17:22:05 CST 2025 aarch64 GNU/Linux",
                        self.hostname
                    ),
                    "-r" => "5.15.147".into(),
                    _ => "Linux".into(),
                };
                (format!("\r\n{out}"), 0)
            }
            "stty" => {
                if arg.contains("echo") {
                    self.echo = !arg.contains("-echo");
                }
                (String::new(), 0)
            }
            "false" => ("\r\n".to_string(), 1),
            "true" => (String::new(), 0),
            // Any other input → echo it if echo is on
            _ => {
                if !self.logged_in {
                    ("\r\nLogin incorrect\r\n".to_string(), 0)
                } else {
                    // Unknown command: simulate "command not found"
                    (format!("\r\n/bin/sh: {cmd}: not found"), 127)
                }
            }
        }
    }
}

// ── Shell word/statement parsing (labgrid wrapped-command support) ─────────

/// Split a line into statements on unquoted `;`.
///
/// The engine wraps every MCP command as
/// `echo '<m1>''<m2>'; { cmd; }; echo "EXIT:$?"; echo '<m1>''<m2>'`,
/// so the simulator must split on `;` like a real shell (quote-aware) and
/// strip `{`/`}` grouping braces to execute the inner command.
fn split_statements(line: &str) -> Vec<&str> {
    let mut statements = Vec::new();
    let mut start = 0;
    let mut in_single = false;
    let mut in_double = false;
    for (i, c) in line.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ';' if !in_single && !in_double => {
                statements.push(&line[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    statements.push(&line[start..]);
    statements
}

/// Shell-like word expansion of `echo` arguments.
///
/// - Quote characters are removed and adjacent quoted/unquoted segments
///   concatenate into one word, so `echo 'ABCD''EFGHJK'` prints the
///   continuous marker `ABCDEFGHJK` (naive arg passthrough keeps the quotes
///   and breaks the engine's begin-marker scan).
/// - Unquoted whitespace separates words; quoted whitespace is preserved.
/// - `$?` expands to `status` unless inside single quotes.
fn shell_expand_words(arg: &str, status: u32) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut has_word = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = arg.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '$' if !in_single && chars.peek() == Some(&'?') => {
                chars.next();
                cur.push_str(&status.to_string());
                has_word = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_word {
                    words.push(std::mem::take(&mut cur));
                    has_word = false;
                }
            }
            _ => {
                cur.push(c);
                has_word = true;
            }
        }
    }
    if has_word {
        words.push(cur);
    }
    words
}

// ── Connection handler ─────────────────────────────────────────────────────

enum InputState {
    Ground,
    Esc,
    Csi(Vec<u8>),
}

fn redraw_inserted_tail(stream: &mut TcpStream, tail: &str, cursor_back: usize) {
    let _ = stream.write_all(tail.as_bytes());
    for _ in 0..cursor_back {
        let _ = stream.write_all(b"\x1b[D");
    }
}

fn handle_graphic_input(stream: &mut TcpStream, shell: &mut Shell, b: u8) {
    let ch = b as char;
    if shell.cursor >= shell.line_buf.len() {
        shell.line_buf.push(ch);
        shell.cursor = shell.line_buf.len();
        if shell.echo {
            let _ = stream.write_all(&[b]);
            let _ = stream.flush();
        }
        return;
    }

    shell.line_buf.insert(shell.cursor, ch);
    shell.cursor += 1;
    if shell.echo {
        let tail = &shell.line_buf[shell.cursor..];
        let _ = stream.write_all(&[b]);
        redraw_inserted_tail(stream, tail, tail.len());
        let _ = stream.flush();
    }
}

fn handle_backspace(stream: &mut TcpStream, shell: &mut Shell) {
    if shell.cursor == 0 || shell.line_buf.is_empty() {
        return;
    }

    shell.cursor -= 1;
    shell.line_buf.remove(shell.cursor);
    if shell.echo {
        if shell.cursor == shell.line_buf.len() {
            let _ = stream.write_all(b"\x08 \x08");
        } else {
            let _ = stream.write_all(b"\x08\x1b[P");
        }
        let _ = stream.flush();
    }
}

fn handle_csi_input(stream: &mut TcpStream, shell: &mut Shell, seq: &[u8]) {
    match seq {
        b"\x1b[D" => {
            if shell.cursor > 0 {
                shell.cursor -= 1;
                if shell.echo {
                    let _ = stream.write_all(seq);
                    let _ = stream.flush();
                }
            }
        }
        b"\x1b[C" if shell.cursor < shell.line_buf.len() => {
            shell.cursor += 1;
            if shell.echo {
                let _ = stream.write_all(seq);
                let _ = stream.flush();
            }
        }
        _ => {}
    }
}

fn handle_client(
    mut stream: TcpStream,
    hostname: &str,
    with_boot_log: bool,
    no_login: bool,
    running: Arc<AtomicBool>,
) {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();
    stream.set_nonblocking(false).ok();

    let mut shell = Shell::new(hostname);
    if no_login {
        shell.logged_in = true;
        shell.login_user = "root".to_string();
    }

    // Send boot log if requested
    if with_boot_log {
        // Simulate staggered boot log output
        let boot_lines: Vec<&str> = BOOT_LOG.lines().collect();
        for line in &boot_lines {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            let _ = stream.write_all(format!("{}\r\n", line).as_bytes());
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // Send initial prompt
    let _ = stream.write_all(shell.initial_prompt().as_bytes());
    let _ = stream.flush();

    // Disable Nagle for responsive character echo
    let _ = stream.set_nodelay(true);

    let mut read_buf = [0u8; 256];
    let mut input_state = InputState::Ground;

    while running.load(Ordering::Relaxed) {
        match stream.read(&mut read_buf) {
            Ok(0) => break, // connection closed
            Ok(n) => {
                let data = &read_buf[..n];

                // Handle backspace
                for &b in data {
                    match &mut input_state {
                        InputState::Esc => {
                            if b == b'[' {
                                input_state = InputState::Csi(vec![0x1b, b'[']);
                            } else {
                                input_state = InputState::Ground;
                            }
                            continue;
                        }
                        InputState::Csi(seq) => {
                            seq.push(b);
                            if (b'@'..=b'~').contains(&b) {
                                let seq = std::mem::take(seq);
                                handle_csi_input(&mut stream, &mut shell, &seq);
                                input_state = InputState::Ground;
                            }
                            continue;
                        }
                        InputState::Ground => {}
                    }

                    match b {
                        b'\x1b' => {
                            input_state = InputState::Esc;
                        }
                        b'\x7f' | b'\x08' => {
                            handle_backspace(&mut stream, &mut shell);
                        }
                        b'\r' | b'\n' => {
                            // Enter — process the line
                            if shell.echo {
                                let _ = stream.write_all(b"\r\n");
                            }
                            let line = std::mem::take(&mut shell.line_buf);
                            shell.cursor = 0;
                            let response = shell.process_line(&line);
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                        }
                        b'\x04' => {
                            // Ctrl-D → exit
                            let _ = stream.write_all(b"\r\nlogout\r\n");
                            let _ = stream.flush();
                            shell.logged_in = false;
                            shell.login_user.clear();
                            let _ = stream.write_all(shell.initial_prompt().as_bytes());
                            let _ = stream.flush();
                        }
                        b'\x03' => {
                            // Ctrl-C → new prompt
                            shell.line_buf.clear();
                            shell.cursor = 0;
                            let _ = stream.write_all(b"^C");
                            let response = shell.prompt();
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                        }
                        _ if b.is_ascii_graphic() || b == b' ' || b == b'\t' => {
                            handle_graphic_input(&mut stream, &mut shell, b);
                        }
                        _ => {
                            // Pass through other control chars (e.g. ANSI sequences)
                            if shell.echo {
                                let _ = stream.write_all(&[b]);
                            }
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => break,
        }
    }
}

// ── Main ───────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Derive hostname from system, with --hostname flag to override.
    let sys_hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "embedded".to_string());

    let mut port = DEFAULT_PORT;
    let mut hostname = sys_hostname;
    let mut with_boot_log = false;
    let mut no_login = false;
    let mut print_port = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => {
                i += 1;
                if i < args.len() {
                    port = args[i].parse().unwrap_or(DEFAULT_PORT);
                }
            }
            "--hostname" | "-h" => {
                i += 1;
                if i < args.len() {
                    hostname = args[i].clone();
                }
            }
            "--boot-log" | "-b" => with_boot_log = true,
            "--no-login" | "-n" => no_login = true,
            "--print-port" | "-P" => print_port = true,
            "--help" => {
                println!(
                    "serial-sim — Fake embedded Linux serial console\n\
                     \nUsage: serial-sim [OPTIONS]\n\
                     \nOptions:\n  \
                     -p, --port PORT     TCP port to listen on (default: {DEFAULT_PORT})\n  \
                     -h, --hostname NAME Hostname for the simulated board (default: system hostname)\n  \
                     -b, --boot-log      Send fake kernel boot log on connect\n  \
                     -n, --no-login      Skip login prompt, go straight to shell\n  \
                     -P, --print-port    Print listening port on stdout\n  \
                     --help              Show this help\n\
                     \nCommands supported: echo, cat, ls, dmesg, uptime, whoami, \
                     hostname, pwd, uname, stty"
                );
                return;
            }
            _ => {
                eprintln!("Unknown flag: {}", args[i]);
                return;
            }
        }
        i += 1;
    }

    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            // Try with a random port
            eprintln!("Cannot bind to {addr}: {e}");
            std::process::exit(1);
        }
    };

    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    eprintln!(
        "[serial-sim] Listening on 127.0.0.1:{actual_port} (hostname={hostname}, login={})\n",
        if no_login { "no" } else { "yes" }
    );

    if print_port {
        println!("{actual_port}");
    }

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Graceful shutdown: wait for Enter on stdin (only if stdin is a terminal).
    // In test environments stdin is /dev/null or a pipe, so we skip this thread
    // and rely on SIGTERM/SIGINT for shutdown.
    let stdin_is_tty = unsafe { libc::isatty(0) != 0 };
    if stdin_is_tty {
        let _ = std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = std::io::stdin().read_line(&mut buf);
            r.store(false, Ordering::Relaxed);
        });
        eprintln!("[serial-sim] Press Enter to stop.");
    } else {
        eprintln!("[serial-sim] Running (non-interactive — send SIGTERM to stop).");
    }

    // Accept loop — single connection at a time (like real serial)
    for incoming in listener.incoming() {
        if !running.load(Ordering::Relaxed) {
            break;
        }
        match incoming {
            Ok(stream) => {
                let peer = stream.peer_addr().unwrap();
                eprintln!("[serial-sim] Client connected: {peer}");
                handle_client(stream, &hostname, with_boot_log, no_login, running.clone());
                eprintln!("[serial-sim] Client disconnected: {peer}");
            }
            Err(e) => {
                eprintln!("[serial-sim] Accept error: {e}");
                break;
            }
        }
    }

    eprintln!("[serial-sim] Shutting down.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logged_in_shell() -> Shell {
        let mut sh = Shell::new("simdut");
        sh.logged_in = true;
        sh.login_user = "root".to_string();
        sh
    }

    #[test]
    fn test_split_statements_respects_quotes() {
        let parts = split_statements("echo 'A;B'; { uname -a; }; echo \"EXIT:$?\"");
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0].trim(), "echo 'A;B'");
        assert_eq!(parts[1].trim(), "{ uname -a");
        assert_eq!(parts[2].trim(), "}");
        assert_eq!(parts[3].trim(), "echo \"EXIT:$?\"");
    }

    #[test]
    fn test_shell_expand_words_quote_concat() {
        // Adjacent quoted segments concatenate into one continuous word —
        // this is what makes the engine's quote-split marker appear intact.
        let words = shell_expand_words("'ABCD''EFGHJK'", 0);
        assert_eq!(words, ["ABCDEFGHJK".to_string()]);

        // Unquoted space between quotes produces separate words.
        let words = shell_expand_words("'AB' 'CD'", 0);
        assert_eq!(words, ["AB".to_string(), "CD".to_string()]);

        // Double quotes and mixed quoted/unquoted text concatenate too.
        let words = shell_expand_words("\"AB\"cd", 0);
        assert_eq!(words, ["ABcd".to_string()]);

        // Single quotes are literal; embedded double quotes stay.
        let words = shell_expand_words("'say \"hi\"'", 0);
        assert_eq!(words, ["say \"hi\"".to_string()]);

        // Empty quoted segments vanish (shell semantics).
        assert!(shell_expand_words("''''", 0).is_empty());
    }

    #[test]
    fn test_shell_expand_words_exit_status() {
        // $? expands outside single quotes (unquoted and double-quoted).
        assert_eq!(shell_expand_words("\"EXIT:$?\"", 7), ["EXIT:7".to_string()]);
        assert_eq!(shell_expand_words("$?", 3), ["3".to_string()]);
        // Inside single quotes $? is literal.
        assert_eq!(shell_expand_words("'$?'", 3), ["$?".to_string()]);
    }

    #[test]
    fn test_engine_wrapped_command_end_to_end() {
        let mut sh = logged_in_shell();
        // Exactly the line CommandQueue::send_command emits for marker
        // "ABCDEFGHJK" and command "uname -a".
        let line = "echo 'ABCD''EFGHJK'; { uname -a; }; echo \"EXIT:$?\"; echo 'ABCD''EFGHJK'";
        let out = sh.process_line(line);
        // Begin and end markers appear as one continuous 10-char string each.
        assert_eq!(out.matches("ABCDEFGHJK").count(), 2, "out: {out:?}");
        let begin = out.find("ABCDEFGHJK").unwrap() + "ABCDEFGHJK".len();
        let end = out.rfind("ABCDEFGHJK").unwrap();
        let middle = &out[begin..end];
        assert!(middle.contains("Linux"), "command output missing: {out:?}");
        assert!(middle.contains("EXIT:0"), "exit code missing: {out:?}");
        // Exactly one prompt, appended after the end marker.
        assert!(out.ends_with("root@simdut:/# "));
        assert_eq!(out.matches("root@simdut:/# ").count(), 1);
    }

    #[test]
    fn test_compound_command_exit_status() {
        let mut sh = logged_in_shell();
        let out = sh.process_line("false; echo \"EXIT:$?\"");
        assert!(out.contains("EXIT:1"), "out: {out:?}");

        let out = sh.process_line("{ true; }; echo \"EXIT:$?\"");
        assert!(out.contains("EXIT:0"), "out: {out:?}");

        // Unknown commands report 127 like a real shell.
        let out = sh.process_line("nosuchcmd; echo \"EXIT:$?\"");
        assert!(out.contains("EXIT:127"), "out: {out:?}");
    }

    #[test]
    fn test_single_statement_output_unchanged() {
        let mut sh = logged_in_shell();
        // Byte-identical to the pre-refactor format: CRLF-prefixed output
        // followed by exactly one prompt.
        assert_eq!(sh.process_line("echo hi"), "\r\nhi\r\nroot@simdut:/# ");
        assert_eq!(sh.process_line(""), "\r\nroot@simdut:/# ");
    }

    #[test]
    fn test_brace_group_without_inner_semicolon() {
        let mut sh = logged_in_shell();
        let out = sh.process_line("{ uname -a }");
        assert!(out.contains("Linux"), "out: {out:?}");
    }

    #[test]
    fn test_multiple_statements_single_prompt() {
        let mut sh = logged_in_shell();
        let out = sh.process_line("echo a; echo b");
        assert!(out.contains("\r\na\r\nb\r\n"), "out: {out:?}");
        assert_eq!(out.matches("root@simdut:/# ").count(), 1, "out: {out:?}");
    }
}
