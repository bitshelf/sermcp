//! dutabo — serial control CLI for embedded devices.
//!
//! Architecture:
//!   dutabo serial → pause Agent → WebSocket/TCP relay → resume Agent
//!   dutabo status/reboot/etc → MCP HTTP → Agent

mod tui_init;

use std::io::IsTerminal;
use std::path::PathBuf;

// Interactive serial lives in the library (`sermcp::serial_term`): an
// alacritty_terminal VT core rendered onto a crossterm host surface. The
// legacy `PromptHighlighter` (ANSI injection + debounce) is banned from
// the interactive display path; the stateless `highlight_serial_prompt`
// stays for static/MCP log paths and the lib's tests.
use sermcp::serial_term::InputEncoder;
use sermcp::serial_term::SerialSettings;
use sermcp::serial_term::SessionEvent;
use sermcp::serial_term::SessionOutcome;
use sermcp::serial_term::StdoutSurface;
use sermcp::serial_term::TerminalGuard;
use sermcp::serial_term::spawn_input_thread;
use sermcp::serial_term::spawn_terminal_thread;

const MCP_DEFAULT_PORT: u16 = 3000;

// Engine startup may spend up to 35s connecting/probing and another 5s
// handing ownership over from an Agent MCP. Keep the manual takeover wait
// above that bound instead of attempting WebSocket while the port is closed.

/// Per-port MCP session cache: `(port, mcp-session-id)`. The Streamable HTTP
/// transport requires the `Mcp-Session-Id` header returned by `initialize` on
/// subsequent requests. Keying by port lets the candidate-probe loop talk to
/// different servers (per-DUT vs project) without a stale session id.
static MCP_SESSION: std::sync::Mutex<Option<(u16, String)>> = std::sync::Mutex::new(None);

struct TerminalSanitizer {
    state: AnsiState,
    allow_color: bool,
    /// Telnet IAC sub-state (negotiation/subnegotiation stripping).
    telnet: TelnetState,
    /// Cross-chunk partial line for the ser2net-banner suppressor.
    line_buf: Vec<u8>,
    /// Banner suppression released (first non-banner byte seen) — plain
    /// passthrough from then on.
    banner_passthrough: bool,
    /// Real content has been emitted — leading blank lines (the banner's
    /// surrounding CRLFs) are dropped until then (field-observed:
    /// no blank-line pile-up before the first prompt).
    saw_content: bool,
}

enum AnsiState {
    Ground,
    Esc(Vec<u8>),
    Csi(Vec<u8>),
    Osc,
}

/// Telnet IAC (0xFF) protocol traffic must never reach the terminal —
/// ser2net's rfc2217 mode opens with WILL/DO negotiations and
/// subnegotiations that render as "ÿ…" garbage (field-observed).
#[derive(Default)]
enum TelnetState {
    #[default]
    Ground,
    /// Saw IAC; expecting the command byte.
    Cmd,
    /// Command takes one option byte (WILL/WONT/DO/DONT).
    Opt,
    /// Inside a subnegotiation (SB) until IAC SE.
    Sub,
    /// Saw IAC inside a subnegotiation (expect SE or escaped IAC).
    SubIac,
}

impl TerminalSanitizer {
    fn new(allow_color: bool) -> Self {
        Self {
            state: AnsiState::Ground,
            allow_color,
            telnet: TelnetState::Ground,
            line_buf: Vec::new(),
            banner_passthrough: false,
            saw_content: false,
        }
    }

    /// One byte of telnet IAC handling: true = protocol byte (drop).
    fn telnet_byte(&mut self, b: u8) -> bool {
        use self::TelnetState::*;
        match std::mem::take(&mut self.telnet) {
            Ground => {
                if b == 0xff {
                    self.telnet = Cmd;
                    true
                } else {
                    self.telnet = Ground;
                    false
                }
            }
            Cmd => match b {
                0xfb..=0xfe => {
                    self.telnet = Opt; // WILL/WONT/DO/DONT + option
                    true
                }
                0xfa => {
                    self.telnet = Sub; // SB ... IAC SE
                    true
                }
                0xff => {
                    self.telnet = Ground; // escaped literal 0xFF data byte
                    false
                }
                _ => {
                    self.telnet = Ground; // two-byte command (NOP/AYT/…)
                    true
                }
            },
            Opt => {
                self.telnet = Ground;
                true
            }
            Sub => {
                self.telnet = if b == 0xff { SubIac } else { Sub };
                true
            }
            SubIac => {
                self.telnet = if b == 0xf0 { Ground } else { Sub };
                true // SE consumed; anything else stays protocol
            }
        }
    }

    /// Ser2net banner suppressor (field-observed: the banner
    /// line must not display). A line containing "ser2net port" is
    /// dropped — WITHOUT holding ordinary partial lines (that would
    /// re-break typed echo). Strategy: bytes are HELD only while the
    /// line-so-far is still a PREFIX of "ser2net port" (after optional
    /// leading \r/\n); the first mismatching byte flushes everything
    /// and switches to permanent passthrough.
    fn banner_line(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.banner_passthrough {
            out.push(b);
            return;
        }
        self.line_buf.push(b);
        let content: &[u8] = {
            let mut i = 0;
            while i < self.line_buf.len()
                && (self.line_buf[i] == b'\r' || self.line_buf[i] == b'\n')
            {
                i += 1;
            }
            &self.line_buf[i..]
        };
        if b == b'\n' {
            // Complete line: banner lines are dropped, everything else
            // passes. Leading BLANK lines are dropped too until the first
            // real content arrives (the banner carries surrounding CRLFs —
            // without this they pile up as blank lines before the prompt).
            let is_banner = content.windows(12).any(|w| *w == *b"ser2net port");
            let is_blank = content
                .iter()
                .all(|&c| c == b'\r' || c == b'\n' || c == b' ');
            if !is_banner && !(is_blank && !self.saw_content) {
                if !is_blank {
                    self.saw_content = true;
                }
                out.extend_from_slice(&self.line_buf);
            }
            self.line_buf.clear();
            return;
        }
        let still_prefix =
            content.len() <= b"ser2net port".len() && b"ser2net port"[..content.len()] == *content;
        // Once the full marker matched, this IS the banner line — hold
        // the rest until its newline (however long the line grows).
        if still_prefix || content.starts_with(b"ser2net port") {
            return;
        }
        if !still_prefix {
            // Not a banner — release the held bytes and stop holding.
            self.saw_content = true;
            out.extend_from_slice(&self.line_buf);
            self.line_buf.clear();
            self.banner_passthrough = true;
        }
    }

    fn filter(&mut self, raw: &[u8], out: &mut Vec<u8>) {
        for &b in raw {
            if self.telnet_byte(b) {
                continue;
            }
            match &mut self.state {
                AnsiState::Ground => {
                    if b == 0x1b {
                        self.state = AnsiState::Esc(vec![b]);
                    } else {
                        // Plain byte: banner-suppressed IN ORDER (escape
                        // sequences bypass the line assembler above).
                        self.banner_line(b, out);
                    }
                }
                AnsiState::Esc(seq) => {
                    seq.push(b);
                    match b {
                        b'[' => {
                            let seq = std::mem::take(seq);
                            self.state = AnsiState::Csi(seq);
                        }
                        b']' => {
                            self.state = AnsiState::Osc;
                        }
                        b'(' | b')' | b'>' | b'=' => {}
                        _ => {
                            self.state = AnsiState::Ground;
                        }
                    }
                }
                AnsiState::Csi(seq) => {
                    seq.push(b);
                    if (b'@'..=b'~').contains(&b) {
                        if b == b'm' {
                            // SGR color code — strip (or preserve if allow_color + safe)
                            if self.allow_color && is_safe_sgr(seq) {
                                out.extend_from_slice(seq);
                            }
                        } else {
                            // Cursor movement, erase, insert/delete, save/restore —
                            // readline needs all non-SGR CSI sequences for correct
                            // visual feedback.  Stripping any of them causes cursor
                            // desync and backspace-deletes-wrong-character bugs.
                            out.extend_from_slice(seq);
                        }
                        self.state = AnsiState::Ground;
                    }
                }
                AnsiState::Osc => {
                    if b == b'\x07' || b == b'\\' {
                        self.state = AnsiState::Ground;
                    }
                }
            }
        }
    }
}

fn is_safe_sgr(seq: &[u8]) -> bool {
    seq.starts_with(b"\x1b[")
        && seq[2..seq.len().saturating_sub(1)]
            .iter()
            .all(|b| b.is_ascii_digit() || *b == b';')
}

/// Which prompt transport `dutabo init` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Tui,
    Plain,
}

/// Pure dispatch decision for the init prompt transport (headless-testable).
/// Auto (both flags off): TUI when stdin AND stdout are terminals, TERM is
/// not "dumb", and the terminal is >= 40x10; plain otherwise (piped scripts
/// stay byte-identical). `--plain` forces plain; `--tui` forces the TUI and
/// ERRORS when stdin/stdout are not terminals (a piped stdin can never feed
/// the crossterm event loop — loud at dispatch instead of a mid-wizard
/// hang/error).
fn transport_for(
    opts: &sermcp::init::InitOptions,
    stdin_tty: bool,
    stdout_tty: bool,
    term: &str,
    size: (u16, u16),
) -> Result<Transport, String> {
    if opts.plain {
        return Ok(Transport::Plain);
    }
    if opts.tui {
        if !(stdin_tty && stdout_tty) {
            return Err("--tui requires a terminal on both stdin and stdout".into());
        }
        return Ok(Transport::Tui);
    }
    Ok(
        if tui_init::tui_allowed(stdin_tty, stdout_tty, term, size) {
            Transport::Tui
        } else {
            Transport::Plain
        },
    )
}

/// Print the wizard outcome (diff summary lines) or its error — shared by
/// the TUI and plain init dispatches.
fn print_init_result(result: Result<sermcp::init::WizardOutcome, String>) {
    match result {
        Ok(outcome) => {
            for line in &outcome.changes {
                println!("{line}");
            }
        }
        Err(e) => {
            // A user-initiated abort (Esc/q, or declining the occupancy
            // gate) is NOT an error: no `Error:` prefix, exit 0, so scripts
            // can distinguish a deliberate decline from a real failure.
            if e == "aborted by user" {
                return;
            }
            if let Some(reason) = e.strip_prefix("aborted: ") {
                eprintln!("[init] aborted: {reason}");
                sermcp::trace_chrome::exit_with_trace(0);
            }
            eprintln!("Error: {e}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    }
}

#[tokio::main]
async fn main() {
    // Native Perfetto trace (env-gated): SERMCP_TRACE=<path>.pftrace
    // records probe spans without a C++ SDK. No fmt layer is installed because
    // the init TUI owns the terminal. Child MCPs receive sibling trace paths.
    let _perfetto_trace = {
        use tracing_subscriber::util::SubscriberInitExt;
        let (registry, guard) =
            sermcp::trace_perfetto::attach_perfetto(tracing_subscriber::registry());
        registry.init();
        guard
    };
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        // Usage error: scripts must be able to tell "no command" from success.
        print_usage();
        sermcp::trace_chrome::exit_with_trace(1);
    }

    let mut dut_name: Option<String> = None;
    let mut mcp_port: u16 = std::env::var("MCP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(MCP_DEFAULT_PORT);
    let mut mcp_port_explicit = false;
    let mut cmd: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        if (args[i] == "--dut" || args[i] == "-d") && i + 1 < args.len() {
            dut_name = Some(args[i + 1].clone());
            i += 2;
        } else if (args[i] == "--mcp-port" || args[i] == "-p") && i + 1 < args.len() {
            match args[i + 1].parse::<u16>() {
                Ok(port) => {
                    mcp_port = port;
                    mcp_port_explicit = true;
                }
                Err(_) => {
                    eprintln!("Error: invalid --mcp-port value: {}", args[i + 1]);
                    print_usage();
                    sermcp::trace_chrome::exit_with_trace(1);
                }
            }
            i += 2;
        } else if args[i] == "--version" || args[i] == "-V" {
            print_version();
            return;
        } else if !args[i].starts_with('-') && cmd.is_none() {
            cmd = Some(args[i].clone());
            i += 1;
        } else {
            positional.push(args[i].clone());
            i += 1;
        }
    }

    let cmd = match cmd {
        Some(c) => c,
        None => {
            // A bare `-h`/`--help` is a help request (success); a bare
            // `dutabo` without any argument is a usage error.
            if positional.iter().any(|arg| arg == "-h" || arg == "--help") {
                print_help();
                return;
            }
            print_usage();
            sermcp::trace_chrome::exit_with_trace(1);
        }
    };

    if !matches!(
        cmd.as_str(),
        "list"
            | "status"
            | "serial"
            | "reboot"
            | "uboot"
            | "button"
            | "uf"
            | "flash-kernel"
            | "init"
            | "help"
            | "names"
    ) {
        eprintln!("Unknown command: {cmd}");
        print_usage();
        sermcp::trace_chrome::exit_with_trace(1);
    }
    if positional.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return;
    }
    let arg_error = match cmd.as_str() {
        "list" if positional.iter().any(|arg| arg != "--json") => Some(format!(
            "`dutabo list` does not accept positional arguments: {}",
            positional.join(" ")
        )),
        "serial" | "reboot" | "uboot" if !positional.is_empty() => Some(format!(
            "`dutabo {cmd}` does not accept positional arguments: {}",
            positional.join(" ")
        )),
        "status" if positional.iter().any(|arg| arg != "--watch") => Some(format!(
            "unknown argument for `dutabo status`: {}",
            positional.join(" ")
        )),
        "uf" | "flash-kernel" if positional.len() != 1 => {
            Some(format!("`dutabo {cmd}` requires exactly one image path"))
        }
        "button" if !(2..=3).contains(&positional.len()) => {
            Some("`dutabo button` requires <button-name> <press|release|pulse> [delay_ms]".into())
        }
        "help" if !positional.is_empty() => {
            Some("`dutabo help` does not accept positional arguments".into())
        }
        _ => None,
    };
    if let Some(error) = arg_error {
        eprintln!("Error: {error}");
        print_usage();
        sermcp::trace_chrome::exit_with_trace(1);
    }

    if cmd == "help" {
        print_help();
        return;
    }

    // `init` dispatches BEFORE config discovery: the JSONC wizard must run in
    // an empty directory with no .target.jsonc at all (init_spec). The
    // headless core lives in sermcp::init (TUI-free); this binary
    // supplies the prompt layer (the ratatui form on a terminal, plain line
    // reading on piped stdin) and prints the wizard's diff summary.
    if cmd == "init" {
        let opts = match sermcp::init::InitOptions::from_args(
            &positional,
            std::env::var("DUTABO_NON_INTERACTIVE").is_ok_and(|v| v == "1"),
        ) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("Error: {e}");
                sermcp::trace_chrome::exit_with_trace(1);
            }
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        // --non-interactive (or DUTABO_NON_INTERACTIVE=1) NEVER launches the
        // form — route to the plain headless pipeline regardless of TTY
        // (zero prompts; defaults only).
        if opts.non_interactive {
            let result = sermcp::init::run_init(&mut tui_init::PlainPromptIO, &opts, &cwd);
            return print_init_result(result);
        }
        // Dispatch gate (before config discovery, so `dutabo init` works in
        // an empty dir): auto = the form TUI on a real, big-enough terminal,
        // plain line prompts otherwise (piped scripts stay byte-identical);
        // --tui / --plain force either transport (see transport_for).
        let stdin_tty = std::io::stdin().is_terminal();
        let stdout_tty = std::io::stdout().is_terminal();
        let term = std::env::var("TERM").unwrap_or_default();
        let size = tui_init::probe_size();
        let transport = match transport_for(&opts, stdin_tty, stdout_tty, &term, size) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Error: {e}");
                sermcp::trace_chrome::exit_with_trace(1);
            }
        };
        let result = match transport {
            Transport::Tui => {
                // The TerminalGuard owns the terminal lifecycle: its Drop
                // restores raw mode / alt screen / mouse capture on EVERY
                // path (Esc, q, error, panic), so the shell is never left
                // in a TUI state.
                if let Some(_guard) = tui_init::TerminalGuard::new() {
                    tui_init::run_form(&opts, &cwd)
                } else if opts.tui {
                    eprintln!(
                        "Error: --tui requested but the TUI could not start \
                         (terminal unavailable or too small)"
                    );
                    sermcp::trace_chrome::exit_with_trace(1);
                } else {
                    eprintln!("note: terminal unavailable for the TUI — using plain prompts");
                    sermcp::init::run_init(&mut tui_init::PlainPromptIO, &opts, &cwd)
                }
            }
            Transport::Plain => {
                if stdin_tty && stdout_tty && !opts.plain {
                    if term.eq_ignore_ascii_case("dumb") {
                        eprintln!("note: TERM=dumb — using plain prompts");
                    } else {
                        eprintln!("note: terminal too small for the TUI — using plain prompts");
                    }
                }
                sermcp::init::run_init(&mut tui_init::PlainPromptIO, &opts, &cwd)
            }
        };
        return print_init_result(result);
    }

    // ── Config discovery: JSONC only (see config::find_config_path). ──
    let config_path = match sermcp::config::find_config_path() {
        Ok(Some(p)) => p,
        Ok(None) => {
            eprintln!("No .target.jsonc found.");
            sermcp::trace_chrome::exit_with_trace(1);
        }
        Err(e) => {
            eprintln!("{e}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    };
    let (dev_hosts, duts) = match sermcp::config::parse_config_file(&config_path) {
        Ok(cfg) => (cfg.dev_hosts, cfg.duts),
        Err(e) => {
            eprintln!("{e}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    };

    if cmd == "list" {
        let json = positional.iter().any(|arg| arg == "--json");
        cmd_list(&duts, &dev_hosts, &config_path, json);
        return;
    }
    // Hidden machine subcommand: one DUT name per line (completions/dutabo.bash).
    if cmd == "names" {
        for dut in &duts {
            println!("{}", dut.dut_name);
        }
        return;
    }

    let dut = match select_dut(&duts, dut_name.as_deref()) {
        Some(d) => d,
        None => sermcp::trace_chrome::exit_with_trace(1),
    };

    // Compute the port only after DUT selection succeeds: an unknown name
    // must never probe the project's MCP. Every DUT uses the same service.
    // An explicit --mcp-port / MCP_PORT bypasses auto-detection entirely.
    let mcp_port_candidates: Vec<u16> = if mcp_port_explicit {
        vec![mcp_port]
    } else {
        let project_dir = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let project_port = detect_mcp_port_from_config()
            .unwrap_or_else(|| sermcp::ports::project_mcp_port(project_dir));
        vec![project_port]
    };

    match cmd.as_str() {
        "status" => {
            let watch = positional.iter().any(|arg| arg == "--watch");
            let project_dir = config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            cmd_status(&dut, &mcp_port_candidates, project_dir, watch).await;
        }
        "serial" => cmd_serial(&dut, &mcp_port_candidates).await,
        "reboot" => cmd_reboot(&dut, &mcp_port_candidates).await,
        "uboot" => cmd_uboot(&dut, &mcp_port_candidates).await,
        "button" => cmd_button(&dut, &positional, &mcp_port_candidates).await,
        "uf" => {
            if !positional.is_empty() {
                cmd_flash(&dut, &positional[0], "full", &mcp_port_candidates).await
            } else {
                // Unreachable: arg validation above enforces exactly one path.
                eprintln!("Usage: dutabo uf <image>");
                sermcp::trace_chrome::exit_with_trace(1);
            }
        }
        "flash-kernel" => {
            if !positional.is_empty() {
                cmd_flash(&dut, &positional[0], "kernel", &mcp_port_candidates).await
            } else {
                // Unreachable: arg validation above enforces exactly one path.
                eprintln!("Usage: dutabo flash-kernel <image>");
                sermcp::trace_chrome::exit_with_trace(1);
            }
        }
        _ => {
            // Unreachable: unknown commands exit above.
            eprintln!("Unknown command: {cmd}");
            print_usage();
            sermcp::trace_chrome::exit_with_trace(1);
        }
    }
}

const HELP: &str = "\
dutabo — Serial access and control for embedded devices

Usage:
  dutabo <command> [options]

Commands:
  Setup:
    init [options]                  Create or update .target.jsonc
      --non-interactive, --quick, --advanced, --force, --tui, --plain
    list [--json]                   List configured devices

  Device:
    status [--watch]                Show device and code-agent status
    serial                          Open an interactive serial console
    reboot                          Restart the selected device
    uboot                           Enter the U-Boot console
    button <name> <action> [delay]  Control a configured device button
    uf <image>                      Flash a full firmware image
    flash-kernel <image>            Flash a kernel or boot image

    Common options:
      -d, --dut <name>    Select a device
      -p, --mcp-port <N>  Use a specific MCP HTTP port


  Help:
    help                            Show help

";

fn print_usage() {
    eprint!("{HELP}");
}

fn print_help() {
    print!("{HELP}");
}

fn print_version() {
    println!("dutabo v{}", env!("CARGO_PKG_VERSION"));
}

fn fetch_health(health_url: &str) -> Option<serde_json::Value> {
    let output = std::process::Command::new("curl")
        .args(["-sf", "--max-time", "1", health_url])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| serde_json::from_slice(&output.stdout).ok())
        .flatten()
}

/// Walk up from CWD to find `.mcp.json` and extract port from `sermcp.url`.
fn detect_mcp_port_from_config() -> Option<u16> {
    let mut d = std::env::current_dir().ok()?;
    loop {
        let mcp_json = d.join(".mcp.json");
        if mcp_json.exists() {
            if let Ok(content) = std::fs::read_to_string(&mcp_json)
                && let Ok(cfg) = serde_json::from_str::<serde_json::Value>(&content)
                && let Some(url) = cfg["mcpServers"]["sermcp"]["url"].as_str()
                && let Some(port_str) = url.rsplit(':').next()
                && let Some(port) = port_str.split('/').next()
                && let Ok(p) = port.parse::<u16>()
            {
                return Some(p);
            }
            return None;
        }
        if !d.pop() {
            break;
        }
    }
    None
}

fn select_dut(
    duts: &[sermcp::config::DutConfig],
    name: Option<&str>,
) -> Option<sermcp::config::DutConfig> {
    if duts.is_empty() {
        eprintln!("No boards are configured.");
        return None;
    }
    if let Some(a) = name {
        for d in duts {
            if d.dut_name == a {
                return Some(d.clone());
            }
        }
        eprintln!("Device '{a}' not found. Available:");
        for d in duts {
            eprintln!("  - {}", d.dut_name);
        }
        return None;
    }
    if duts.len() == 1 {
        return Some(duts[0].clone());
    }
    eprintln!("Multiple boards are configured. Choose one with -d/--dut:");
    for d in duts {
        eprintln!("  - {}", d.dut_name);
    }
    None
}

// ── `dutabo list` — state table (human) + inventory JSON (machine) ──────

use sermcp::config::{DevHostConfig, DutConfig};

/// Render the `list` command: the state table (human) or the canonical
/// inventory JSON (machine). The JSON is built by the same pure builder as
/// the server's `.dut-serial/inventory.json` with per-DUT state from the ONE
/// shared helper `dut_state::inventory_state_for`, so the two outputs can
/// never diverge.
fn cmd_list(
    duts: &[DutConfig],
    dev_hosts: &[DevHostConfig],
    config_path: &std::path::Path,
    json: bool,
) {
    let project_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    if json {
        print_list_json(duts, dev_hosts, config_path, project_dir);
    } else {
        print_dut_table(duts, dev_hosts, project_dir);
    }
}

/// `dutabo list --json`: emit the canonical inventory JSON (schema_version 1)
/// — identical shape to `.dut-serial/inventory.json` so Python hooks and this
/// command read the same data; `mcp_http_port` is the project control port
/// (the server bound in stdio mode) and the CLI never writes the file.
fn print_list_json(
    duts: &[DutConfig],
    dev_hosts: &[DevHostConfig],
    config_path: &std::path::Path,
    project_dir: &std::path::Path,
) {
    // Construct the Config the way load_config would (defaults + file map)
    // without a second discovery walk — discovery already resolved this path.
    let mut values = sermcp::config::defaults();
    match sermcp::config::parse_jsonc_config(config_path) {
        Ok(file_map) => values.extend(file_map),
        Err(e) => {
            eprintln!("{e}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    }
    let cfg = sermcp::config::Config {
        values,
        config_path: Some(config_path.to_path_buf()),
        project_dir: Some(project_dir.to_path_buf()),
        format: sermcp::config::ConfigFormat::Jsonc,
    };
    let current_dut_name = std::env::var("TARGET_DUT_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let written_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let inventory = sermcp::config::build_inventory_json(
        &cfg,
        dev_hosts,
        duts,
        current_dut_name.as_deref(),
        sermcp::config::InventoryMeta {
            written_by: "dutabo",
            written_at_unix,
            mcp_pid: None,
            mcp_http_port: Some(sermcp::ports::project_mcp_port(project_dir)),
            heartbeat_secs: None,
            owner: None,
        },
        |dut| sermcp::dut_state::inventory_state_for(project_dir, dut),
    );
    match serde_json::to_string_pretty(&inventory) {
        Ok(text) => println!("{text}"),
        Err(e) => {
            eprintln!("Cannot serialize inventory JSON: {e}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    }
}

/// `dutabo list` state table: NAME | HOST | USER@IP | PORT | STATE.
///
/// Rendering rules (spec dutabo_list_table, TDD-14):
/// - STATE vocabulary comes from the one shared reader
///   (`dut_state::read_dut_state`); critical states carry a "✗ " prefix so a
///   kernel crash is always prominent, missing files render "unknown";
/// - age is never hidden: files >= 600s old get an " (42m ago)" suffix;
/// - ANSI color only when stdout is a terminal — piped output stays greppable;
/// - terminal width from COLUMNS (fallback 120); overflow truncates
///   HOST → USER@IP → PORT, then NAME (min 12); STATE never drops the
///   "✗" glyph.
fn print_dut_table(duts: &[DutConfig], dev_hosts: &[DevHostConfig], project_dir: &std::path::Path) {
    use std::io::IsTerminal;
    let color = std::io::stdout().is_terminal();
    let rows = build_rows(duts, dev_hosts, project_dir, color);
    let widths = column_widths(&rows, terminal_width());
    for row in &rows {
        println!("{}", format_row(row, &widths));
    }
}

/// Table header and column indices. Truncation order is index-based:
/// HOST(1) → USER@IP(2) → PORT(3) → NAME(0); STATE(4) last.
const TABLE_HEADER: [&str; 5] = ["NAME", "HOST", "USER@IP", "PORT", "STATE"];
/// Minimum visible column widths (NAME 12, the rest 4; STATE additionally
/// never drops below the 2-char "✗ " prefix).
const MIN_WIDTHS: [usize; 5] = [12, 4, 4, 4, 4];
/// Gap between columns (spaces).
const COLUMN_GAP: usize = 2;

fn build_rows(
    duts: &[DutConfig],
    dev_hosts: &[DevHostConfig],
    project_dir: &std::path::Path,
    color: bool,
) -> Vec<[String; 5]> {
    let mut rows = vec![TABLE_HEADER.map(str::to_string)];
    for dut in duts {
        // The ONE shared path derivation (name dir over configured dut_dir)
        // — same file the Python statusline reads.
        let state_file = sermcp::dut_state::dut_state_file(project_dir, dut);
        let view = sermcp::dut_state::read_dut_state(&state_file);
        rows.push([
            dut.dut_name.clone(),
            host_name_cell(dut, dev_hosts),
            user_at_ip(dut),
            if dut.serial_port.is_empty() {
                "-".into()
            } else {
                dut.serial_port.clone()
            },
            state_cell(&view, color),
        ]);
    }
    rows
}

/// HOST cell: the host's explicit display name — "-" when the host has no
/// name of its own. `host_config()` resolves the ip fallback, so a resolved
/// name equal to the ip means "no explicit name" and USER@IP already shows
/// the ip — printing it twice would only widen the table.
fn host_name_cell(dut: &DutConfig, dev_hosts: &[DevHostConfig]) -> String {
    dev_hosts
        .iter()
        .find(|h| h.ip == dut.dev_host_ip)
        .filter(|h| !h.host_name.is_empty() && h.host_name != h.ip)
        .map(|h| h.host_name.clone())
        .unwrap_or_else(|| "-".into())
}

/// USER@IP cell: "{user}@{ip}", bare ip when user is empty, "-" when the
/// host ip is empty too.
fn user_at_ip(dut: &DutConfig) -> String {
    if dut.dev_host_ip.is_empty() {
        return "-".into();
    }
    if dut.dev_host_user.is_empty() {
        dut.dev_host_ip.clone()
    } else {
        format!("{}@{}", dut.dev_host_user, dut.dev_host_ip)
    }
}

/// STATE cell: label (+ age suffix) wrapped in the statusline color scheme.
/// "stopped"/"unknown" render dim — never as healthy states.
fn state_cell(view: &sermcp::dut_state::DutStateView, color: bool) -> String {
    let text = sermcp::dut_state::format_state_cell(view);
    if !color {
        return text;
    }
    let code = match view.state.as_str() {
        "active" => "32",
        "booting" => "33",
        "uboot" => "36",
        "crashed" | "DUT-off" | "disconnected" => "31",
        "dutabo" => "35",
        "stopped" | "unknown" => "2",
        _ => return text,
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

/// Terminal width from COLUMNS (fallback 120; invalid/zero → 120).
fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&w| w > 0)
        .unwrap_or(120)
}

/// Visible width of a cell — ANSI escapes occupy no columns.
fn visible_width(text: &str) -> usize {
    String::from_utf8_lossy(&sermcp::ansi_strip::strip(text.as_bytes()))
        .chars()
        .count()
}

/// Per-column maximum VISIBLE widths over all rows (ANSI stripped).
fn column_maxes(rows: &[[String; 5]]) -> [usize; 5] {
    let mut maxes = [0usize; 5];
    for row in rows {
        for (col, cell) in row.iter().enumerate() {
            maxes[col] = maxes[col].max(visible_width(cell));
        }
    }
    maxes
}

/// Truncation budget: when the row is wider than the terminal, shrink
/// HOST → USER@IP → PORT, then NAME (min 12); STATE is the last resort and
/// never drops below the "✗" glyph (2 visible chars).
fn column_widths(rows: &[[String; 5]], term_width: usize) -> [usize; 5] {
    let mut widths = column_maxes(rows);
    let total: usize = widths.iter().sum::<usize>() + COLUMN_GAP * (widths.len() - 1);
    let mut overflow = total.saturating_sub(term_width);
    for col in [1usize, 2, 3, 0] {
        if overflow == 0 {
            break;
        }
        let reduce = overflow.min(widths[col].saturating_sub(MIN_WIDTHS[col]));
        widths[col] -= reduce;
        overflow -= reduce;
    }
    if overflow > 0 {
        let reduce = overflow.min(widths[4].saturating_sub(2));
        widths[4] -= reduce;
    }
    widths
}

/// Cut a cell to `width` visible chars, appending "...". The first char
/// (the "✗" glyph of a critical state) always survives.
fn truncate_cell(cell: &str, width: usize) -> String {
    let stripped = sermcp::ansi_strip::strip(cell.as_bytes());
    let plain = String::from_utf8_lossy(&stripped);
    let chars: Vec<char> = plain.chars().collect();
    if chars.len() <= width {
        return cell.to_string();
    }
    if width <= 3 {
        return chars[..width].iter().collect();
    }
    let cut = width - 3;
    format!("{}...", chars[..cut].iter().collect::<String>())
}

/// Pad/truncate a cell to exactly `width` visible columns. Colored cells keep
/// their ANSI when they fit; truncation strips color (the cell is cut).
fn fit_cell(cell: &str, width: usize) -> String {
    let visible = visible_width(cell);
    if visible >= width {
        return truncate_cell(cell, width);
    }
    format!("{cell}{}", " ".repeat(width - visible))
}

fn format_row(row: &[String; 5], widths: &[usize; 5]) -> String {
    let cells: Vec<String> = row
        .iter()
        .zip(widths)
        .map(|(cell, width)| fit_cell(cell, *width))
        .collect();
    cells.join(&" ".repeat(COLUMN_GAP))
}

/// The rmcp Streamable HTTP server requires this exact Accept header —
/// a bare reqwest request gets HTTP 406 Not Acceptable.
const MCP_ACCEPT: &str = "application/json, text/event-stream";

/// Total timeout for one `tools/call` HTTP request. Long-running tools are
/// legitimate: `serial_power_cycle` waits up to 120s for login,
/// `serial_enter_uboot` floods for 15s per retry, and a synchronous
/// `serial_reset` on a server without task support can retry for ~10
/// minutes. A short timeout aborts the request while the hardware keeps
/// working and misattributes the failure to a missing server. Connection
/// failures are bounded separately by the client's `connect_timeout`, so a
/// dead server still fails fast.
const TOOL_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// Per-request timeout for `tasks/get` polling — the server answers from
/// in-memory task state, so anything sane responds well within this.
const TASK_POLL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Default `tasks/get` interval (SEP-2663). The server may suggest another
/// via `pollIntervalMs`, honored within the clamp bounds below.
const TASK_POLL_INTERVAL_MS: u64 = 1500;
const TASK_POLL_MIN_INTERVAL_MS: u64 = 500;
const TASK_POLL_MAX_INTERVAL_MS: u64 = 5000;

/// Consecutive transport failures tolerated while polling before giving up —
/// a dead server must not hang dutabo forever.
const TASK_POLL_MAX_FAILURES: u32 = 5;

/// Overall polling budget. The server-side reset is bounded on its own; this
/// only guards against a stuck task hanging dutabo indefinitely.
const TASK_POLL_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

/// Result of an MCP call across the candidate ports.
///
/// Gap list §1.3: a JSON-RPC `error` frame means the server *answered* — it
/// must never be reported as "no MCP server reachable". Only transport-level
/// failures count as unreachable; protocol errors keep their own category so
/// the subcommands can surface `error.message` verbatim.
#[derive(Debug)]
enum McpOutcome {
    /// A server returned a JSON-RPC `result` payload.
    Result(serde_json::Value),
    /// A server answered with a JSON-RPC `error` object (e.g. -32602 invalid
    /// params). The server is reachable; `message` is its own description.
    RpcError { code: i64, message: String },
    /// No candidate port answered at the transport level.
    Unreachable,
}

/// Outcome of a call attempt against one server/port.
#[derive(Debug)]
enum PortAttempt {
    /// JSON-RPC `result` payload. Task-derived payloads keep the shapes the
    /// subcommands already parse: a failed task's `error` object and the
    /// poll-timeout marker both come back through this variant.
    Result(serde_json::Value),
    /// JSON-RPC `error` frame — the server answered, the call was rejected.
    RpcError { code: i64, message: String },
    /// Transport-level failure (unreachable, HTTP error, malformed frame).
    Failed,
}

/// Try the JSON-RPC `method` against each candidate port in order; the first
/// server that answers wins. Normal callers supply the one project port;
/// explicit compatibility overrides may add another candidate.
///
/// Gap list §1.3: a JSON-RPC error stops the probe immediately — the
/// answering server is the authoritative one and its error is the real
/// problem; trying fallback ports would mask it.
async fn mcp_call(method: &str, args: serde_json::Value, ports: &[u16]) -> McpOutcome {
    for &port in ports {
        match mcp_call_on_port(method, args.clone(), port).await {
            PortAttempt::Result(result) => return McpOutcome::Result(result),
            PortAttempt::RpcError { code, message } => {
                return McpOutcome::RpcError { code, message };
            }
            PortAttempt::Failed => {}
        }
    }
    McpOutcome::Unreachable
}

/// Call `method` against one Streamable HTTP server on `port`.
///
/// A task-capable server (the `io.modelcontextprotocol/tasks` extension
/// declared by [`ensure_initialized`]) answers long `tools/call` requests
/// with a SEP-2663 task handle (`resultType: "task"`) instead of blocking
/// synchronously for minutes; the result is then fetched by polling
/// `tasks/get` ([`poll_task_result`]).
///
/// Gap list §1.5: if the cached `Mcp-Session-Id` is rejected with HTTP 404
/// (MCP spec: terminated/unknown sessions MUST get 404 — the server
/// restarted and recycled the port), the cache is dropped and the call is
/// retried once with a fresh `initialize`.
async fn mcp_call_on_port(method: &str, args: serde_json::Value, port: u16) -> PortAttempt {
    let url = format!("http://127.0.0.1:{port}/mcp");
    // Only the connect phase is tightly bounded: a dead server fails fast
    // (connection refused), while a live server answering a long tool call
    // must not be abandoned mid-flight (see TOOL_CALL_TIMEOUT).
    let Ok(client) = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .build()
    else {
        return PortAttempt::Failed;
    };
    for attempt in 0..2 {
        let session_id = ensure_initialized(&client, &url, port).await;
        // Clone: the stale-session retry (attempt 1) re-sends the same body.
        let body =
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":args.clone()});
        let frame = match mcp_post(
            &client,
            &url,
            session_id.as_deref(),
            body,
            TOOL_CALL_TIMEOUT,
        )
        .await
        {
            PostOutcome::Frame(frame) => frame,
            PostOutcome::StaleSession if attempt == 0 => {
                clear_cached_session(port);
                continue;
            }
            _ => return PortAttempt::Failed,
        };
        if let Some(result) = frame.get("result").cloned() {
            if result.get("resultType").and_then(serde_json::Value::as_str) == Some("task") {
                let Some(task_id) = result.get("taskId").and_then(serde_json::Value::as_str) else {
                    return PortAttempt::Failed;
                };
                return poll_task_result(&client, &url, port, task_id).await;
            }
            return PortAttempt::Result(result);
        }
        if let Some(error) = frame.get("error") {
            // Gap list §1.3: the server answered — this is a protocol
            // error, classified, not a transport failure.
            return PortAttempt::RpcError {
                code: error["code"].as_i64().unwrap_or(-1),
                message: error["message"]
                    .as_str()
                    .unwrap_or("unknown JSON-RPC error")
                    .to_string(),
            };
        }
        // A frame with neither `result` nor `error` is malformed; treat it
        // as a transport failure.
        return PortAttempt::Failed;
    }
    PortAttempt::Failed
}

/// Outcome of one POST to the Streamable HTTP endpoint.
///
/// Gap list §1.3: the wire protocol distinguishes "server answered with a
/// JSON-RPC error" from "server unreachable" — previously both collapsed
/// into `None` and were misreported as "no MCP server".
#[derive(Debug)]
enum PostOutcome {
    /// Parsed JSON-RPC frame — classified by the caller via its
    /// `result`/`error` keys.
    Frame(serde_json::Value),
    /// HTTP 404 while carrying a session header: the cached session id no
    /// longer resolves server-side (server restart — MCP spec mandates 404
    /// for terminated/unknown sessions). The caller drops the cache and
    /// retries once with a fresh `initialize`.
    StaleSession,
    /// Everything else: connection refused, timeout, unexpected HTTP
    /// status, or an unparseable body.
    Transport,
}

/// POST a JSON-RPC body to the Streamable HTTP endpoint and return the
/// outcome. Quiet on failure: the per-port attempt is one candidate among
/// possible compatibility overrides, and the caller reports one actionable
/// message when all fail.
async fn mcp_post(
    client: &reqwest::Client,
    url: &str,
    session_id: Option<&str>,
    body: serde_json::Value,
    timeout: std::time::Duration,
) -> PostOutcome {
    let mut request = client
        .post(url)
        .header("Accept", MCP_ACCEPT)
        .json(&body)
        .timeout(timeout);
    if let Some(session_id) = session_id {
        request = request.header("Mcp-Session-Id", session_id);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => return PostOutcome::Transport,
    };
    // Gap list §1.5: 404 while carrying a session header means the session
    // died server-side (rmcp answers terminated/unknown sessions with 404).
    if response.status() == reqwest::StatusCode::NOT_FOUND && session_id.is_some() {
        return PostOutcome::StaleSession;
    }
    if !response.status().is_success() {
        return PostOutcome::Transport;
    }
    // Streamable HTTP responses are SSE frames (`data: {json}` lines), not a
    // bare JSON body — parse the payload frames instead of resp.json().
    let Ok(text) = response.text().await else {
        return PostOutcome::Transport;
    };
    parse_sse_payload(&text).map_or(PostOutcome::Transport, PostOutcome::Frame)
}

/// Poll `tasks/get` (SEP-2663) until the task reaches a terminal state, then
/// return the task payload. Completed tasks carry the original
/// `CallToolResult` under `result` — the same shape a synchronous
/// `tools/call` would have returned — so callers keep parsing
/// `content[0].text` unchanged. Failed tasks return the JSON-RPC error
/// object; cancelled tasks return a small marker. Transport failures are
/// retried up to [`TASK_POLL_MAX_FAILURES`] consecutive attempts so a
/// momentary hiccup does not lose a task still running server-side.
///
/// Gap list §1.3: a JSON-RPC `error` frame from `tasks/get` (e.g. "task
/// not found" after the server restarted) is a definitive protocol answer —
/// surfaced verbatim instead of retried into a "no MCP server" message.
/// Gap list §1.5: a 404 on the cached session drops the cache and retries
/// once with a fresh `initialize` (the task itself does not survive a
/// server restart, so further polls answer with a proper error instead of
/// transport noise).
async fn poll_task_result(
    client: &reqwest::Client,
    url: &str,
    port: u16,
    task_id: &str,
) -> PortAttempt {
    let deadline = tokio::time::Instant::now() + TASK_POLL_TOTAL_TIMEOUT;
    let mut failures = 0u32;
    let mut interval_ms = TASK_POLL_INTERVAL_MS;
    let mut session_retried = false;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return PortAttempt::Result(serde_json::json!({
                "success": false,
                "error": "timed out waiting for task to reach a terminal state"
            }));
        }
        let Some(session_id) = ensure_initialized(client, url, port).await else {
            failures += 1;
            if failures >= TASK_POLL_MAX_FAILURES {
                return PortAttempt::Failed;
            }
            tokio::time::sleep(std::time::Duration::from_millis(TASK_POLL_INTERVAL_MS)).await;
            continue;
        };
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/get",
            "params": {"taskId": task_id}
        });
        let frame = match mcp_post(
            client,
            url,
            Some(&session_id),
            body,
            TASK_POLL_REQUEST_TIMEOUT,
        )
        .await
        {
            PostOutcome::Frame(frame) => frame,
            PostOutcome::StaleSession if !session_retried => {
                clear_cached_session(port);
                session_retried = true;
                failures = 0;
                continue;
            }
            _ => {
                failures += 1;
                if failures >= TASK_POLL_MAX_FAILURES {
                    return PortAttempt::Failed;
                }
                tokio::time::sleep(std::time::Duration::from_millis(TASK_POLL_INTERVAL_MS)).await;
                continue;
            }
        };
        failures = 0;
        if let Some(result) = frame.get("result").cloned() {
            // Honor the server's suggested poll interval when present.
            if let Some(ms) = result
                .get("pollIntervalMs")
                .and_then(serde_json::Value::as_u64)
            {
                interval_ms = ms.clamp(TASK_POLL_MIN_INTERVAL_MS, TASK_POLL_MAX_INTERVAL_MS);
            }
            match result.get("status").and_then(serde_json::Value::as_str) {
                Some("completed") => {
                    return PortAttempt::Result(
                        result
                            .get("result")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    );
                }
                Some("failed") => {
                    return PortAttempt::Result(
                        result
                            .get("error")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    );
                }
                Some("cancelled") => {
                    return PortAttempt::Result(serde_json::json!({"cancelled": true}));
                }
                _ => {}
            }
        } else if let Some(error) = frame.get("error") {
            // Gap list §1.3: the server rejected the poll (unknown/expired
            // task, ...) — a protocol answer, not a transport failure.
            return PortAttempt::RpcError {
                code: error["code"].as_i64().unwrap_or(-1),
                message: error["message"]
                    .as_str()
                    .unwrap_or("unknown JSON-RPC error")
                    .to_string(),
            };
        } else {
            // Frame with neither `result` nor `error` — malformed; count it
            // against the failure budget so a misbehaving server cannot
            // hang dutabo forever.
            failures += 1;
            if failures >= TASK_POLL_MAX_FAILURES {
                return PortAttempt::Failed;
            }
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
            continue;
        }
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
    }
}

/// Protocol version dutabo requests at `initialize`.
///
/// Gap list §1.2: pinned to 2024-11-05, the revision this client's wire
/// handling was written against. rmcp's default negotiate echoes the
/// client's version when supported and falls back to the server default
/// otherwise (it never errors), so the pin is safe against our own server;
/// proper `ClientLifecycleMode::Auto` negotiation would come with the
/// rmcp-client migration (gap §1.4 — not undertaken, see report).
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Ensure this port has an MCP session: POST `initialize`, remember the
/// `mcp-session-id` response header, then send `notifications/initialized`.
///
/// `initialize` declares the `io.modelcontextprotocol/tasks` extension
/// (SEP-2663) so the server answers long-running `tools/call` requests
/// (e.g. `serial_reset` with `wait_boot=true`) with a task handle instead
/// of blocking synchronously for minutes — a synchronous response would
/// outlive the request timeout and be misreported as "server not found"
/// while the hardware keeps running.
///
/// Gap list §1.2: the handshake only counts as successful when the server
/// answers 2xx with a JSON-RPC frame whose `result.protocolVersion` echoes
/// a negotiated version — an `error` frame (e.g. -32022) or a transport
/// failure leaves the cache empty instead of "initialized", so follow-up
/// requests can no longer ride a session that was never accepted.
/// Gap list §1.1: `notifications/initialized` carries the same
/// `Mcp-Session-Id` header — the Streamable HTTP contract requires the id
/// on every request after initialize, notifications included, or the
/// notification lands in a fresh, unrelated session. No session id → no
/// notification (nothing to attach it to).
async fn ensure_initialized(client: &reqwest::Client, url: &str, port: u16) -> Option<String> {
    {
        let cache = MCP_SESSION.lock().unwrap();
        if let Some((cached_port, session_id)) = cache.as_ref()
            && *cached_port == port
        {
            return Some(session_id.clone());
        }
    }
    let Some(response) = client
        .post(url)
        .header("Accept", MCP_ACCEPT)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "extensions": {"io.modelcontextprotocol/tasks": {}}
                },
                "clientInfo": {"name": "dutabo", "version": env!("CARGO_PKG_VERSION")}
            }
        }))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()
        .filter(|response| response.status().is_success())
    else {
        // Unreachable, timed out, or rejected at the HTTP level — nothing
        // to cache.
        return None;
    };
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response.text().await.ok()?;
    let frame = parse_sse_payload(&body)?;
    // Validate the negotiation result instead of trusting the transport
    // (gap §1.2): an `error` frame means the server rejected this client.
    if frame.get("error").is_some() {
        return None;
    }
    // A successful initialize echoes the negotiated protocolVersion; its
    // absence means the body is not a real initialize result. (The server's
    // task extension support needs no gate here: without it, `tools/call`
    // simply never returns a task handle and the synchronous path applies.)
    match frame["result"]["protocolVersion"].as_str() {
        Some(version) if !version.is_empty() => {}
        _ => return None,
    }
    let session_id = session_id?;
    let _ = client
        .post(url)
        .header("Accept", MCP_ACCEPT)
        .header("Mcp-Session-Id", &session_id)
        .json(&serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
    *MCP_SESSION.lock().unwrap() = Some((port, session_id.clone()));
    Some(session_id)
}

/// Drop the cached session for `port`.
///
/// Gap list §1.5: the server restarted and the old `Mcp-Session-Id` now
/// answers 404 — the next `ensure_initialized` re-runs the handshake.
fn clear_cached_session(port: u16) {
    let mut cache = MCP_SESSION.lock().unwrap();
    if cache
        .as_ref()
        .is_some_and(|(cached_port, _)| *cached_port == port)
    {
        *cache = None;
    }
}

/// Parse a Streamable HTTP response body into JSON-RPC frames.
///
/// Streamable HTTP wraps payloads as SSE (`data: <json>` lines). Per the
/// SSE spec, consecutive `data:` lines of one event are joined with a
/// newline before parsing, `:` lines are comments, and `event:`/`id:`/
/// `retry:` fields carry no JSON payload. A server may also answer with a
/// bare JSON body (non-SSE). Returns the last parseable frame.
fn parse_sse_payload(text: &str) -> Option<serde_json::Value> {
    // Bare JSON body — never valid whole-document JSON for SSE text.
    if let Ok(value) = serde_json::from_str(text.trim()) {
        return Some(value);
    }
    let mut last: Option<serde_json::Value> = None;
    let mut data: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            // Event boundary: flush the accumulated `data:` lines.
            if !data.is_empty() {
                if let Ok(value) = serde_json::from_str(&data.join("\n")) {
                    last = Some(value);
                }
                data.clear();
            }
            continue;
        }
        if line.starts_with(':') {
            continue; // SSE comment.
        }
        if let Some(payload) = line.strip_prefix("data:") {
            data.push(payload.trim_start());
            continue;
        }
        if line.starts_with("event:") || line.starts_with("id:") || line.starts_with("retry:") {
            continue; // SSE control fields — no JSON payload.
        }
        // Bare JSON line outside SSE framing (defensive fallback).
        if let Ok(value) = serde_json::from_str(line) {
            last = Some(value);
        }
    }
    if !data.is_empty()
        && let Ok(value) = serde_json::from_str(&data.join("\n"))
    {
        last = Some(value);
    }
    last
}

// ── Subcommands ──────────────────────────────────────────────────────────

/// Report a failed MCP call the way every subcommand expects, then exit.
///
/// Gap list §1.3: JSON-RPC errors carry the server's own message (the
/// server IS reachable — "no MCP server" would be a lie); only transport
/// failures get the "no MCP server" guidance.
fn report_mcp_failure(dut: &sermcp::config::DutConfig, ports: &[u16], outcome: McpOutcome) -> ! {
    match outcome {
        McpOutcome::RpcError { code, message } => {
            eprintln!("MCP error {code}: {message}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
        McpOutcome::Unreachable => {
            eprintln!("{}", missing_server_message(dut, ports, None));
            sermcp::trace_chrome::exit_with_trace(1);
        }
        McpOutcome::Result(_) => unreachable!("success is not a failure"),
    }
}

/// Parse the text payload of a `tools/call` result into the tool's own JSON
/// object. `Err(message)` when the tool reported `success: false` (with its
/// `error` message when present) or when the text is not JSON at all — a
/// server that answered with garbage is a failure, not a success. A missing
/// `success` field counts as success (`serial_get_state` and
/// `serial_send_command` carry none).
fn tool_payload(result: &serde_json::Value) -> Result<serde_json::Value, String> {
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    let v = serde_json::from_str::<serde_json::Value>(text)
        .map_err(|_| format!("unparseable tool result: {text}"))?;
    if v["success"].as_bool().unwrap_or(true) {
        Ok(v)
    } else {
        Err(v["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string()))
    }
}

/// Like [`tool_payload`], but reports the failure as `what failed: <error>`
/// and exits 1 — a tool-level failure must never look like success to
/// scripts and CI.
fn require_tool_success(result: &serde_json::Value, what: &str) -> serde_json::Value {
    match tool_payload(result) {
        Ok(v) => v,
        Err(error) => {
            eprintln!("{what} failed: {error}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CodeAgentSession {
    session_id: String,
    role: String,
    started: String,
}

/// Read the hook-maintained registry of code-agent sessions using this
/// project's shared MCP service. The owner record is authoritative for the
/// current owner/guest role; a marker's role is only a fallback while no
/// owner record exists.
fn code_agent_sessions(project_dir: &std::path::Path) -> Vec<CodeAgentSession> {
    let owner_session_id = std::fs::read_to_string(project_dir.join(".dut-serial/owner.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|owner| owner["owner_session_id"].as_str().map(str::to_string));
    let Ok(entries) = std::fs::read_dir(project_dir.join(".dut-serial/sessions")) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let marker = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let mut lines = marker.lines();
        let timestamp = lines.next().unwrap_or("");
        let marker_role = lines.next().unwrap_or("");
        let role = if owner_session_id.as_deref() == Some(name.as_str()) {
            "owner"
        } else if owner_session_id.is_some() {
            "guest"
        } else if matches!(marker_role, "owner" | "guest") {
            marker_role
        } else {
            "registered"
        };
        let started = chrono::DateTime::parse_from_rfc3339(timestamp)
            .map(|stamp| {
                stamp
                    .with_timezone(&chrono::Utc)
                    .format("%Y-%m-%d %H:%M:%SZ")
                    .to_string()
            })
            .unwrap_or_else(|_| "-".to_string());
        sessions.push(CodeAgentSession {
            session_id: name,
            role: role.to_string(),
            started,
        });
    }
    sessions.sort_by(|a, b| {
        let a_owner = a.role == "owner";
        let b_owner = b.role == "owner";
        b_owner
            .cmp(&a_owner)
            .then_with(|| a.started.cmp(&b.started))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    sessions
}

fn print_code_agent_table(project_dir: &std::path::Path) {
    let sessions = code_agent_sessions(project_dir);
    println!("Code agents:");
    if sessions.is_empty() {
        println!("  No registered code-agent sessions.");
        return;
    }
    const SESSION_WIDTH: usize = 36;
    const ROLE_WIDTH: usize = 10;
    println!(
        "  {}  {}  STARTED (UTC)",
        fit_cell("SESSION", SESSION_WIDTH),
        fit_cell("ROLE", ROLE_WIDTH)
    );
    for session in sessions {
        println!(
            "  {}  {}  {}",
            fit_cell(&session.session_id, SESSION_WIDTH),
            fit_cell(&session.role, ROLE_WIDTH),
            session.started
        );
    }
}

async fn cmd_status(
    dut: &sermcp::config::DutConfig,
    ports: &[u16],
    project_dir: &std::path::Path,
    watch: bool,
) {
    let args = serde_json::json!({"name":"serial_get_state","arguments":{}});
    let r = match mcp_call("tools/call", args, ports).await {
        McpOutcome::Result(r) => r,
        outcome => {
            print_code_agent_table(project_dir);
            report_mcp_failure(dut, ports, outcome)
        }
    };
    let s = require_tool_success(&r, "status");
    println!("Status:    {}", s["state"].as_str().unwrap_or("unknown"));
    println!("Boot #:    {}", s["boot_number"]);
    println!(
        "Last data: {:.1}s ago",
        s["last_data_seconds"].as_f64().unwrap_or(0.0)
    );
    println!("Log path:  {}", s["log_path"].as_str().unwrap_or(""));
    println!(
        "Relay:     {}",
        if s["relay_configured"].as_bool().unwrap_or(false) {
            "configured"
        } else {
            "not configured"
        }
    );
    println!(
        "Login:     {}",
        if s["login_configured"].as_bool().unwrap_or(false) {
            "configured"
        } else {
            "not configured"
        }
    );
    println!();
    print_code_agent_table(project_dir);

    if !watch {
        return;
    }

    let mut watcher = match sermcp::inotify_watcher::InotifyWatcher::new(project_dir, &dut.dut_dir)
    {
        Ok(watcher) => watcher,
        Err(error) => {
            eprintln!("Cannot watch DUT state: {error}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    };

    eprintln!("Watching {} (Ctrl-C to stop)...", dut.dut_name);
    while let Some(event) = watcher.recv().await {
        let state = std::fs::read_to_string(&event.path)
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|_| "unavailable".to_string());
        println!("{}: {}", dut.dut_name, state);
    }
}

async fn cmd_reboot(dut: &sermcp::config::DutConfig, ports: &[u16]) {
    // Priority: power cycle > relay reset > software reboot
    if dut.power_ch > 0 {
        println!(
            "Power cycling (channel {}, {}ms off)...",
            dut.power_ch, dut.power_off_time_ms
        );
        let args = serde_json::json!({"name":"serial_power_cycle","arguments":{"wait_boot":true}});
        match mcp_call("tools/call", args, ports).await {
            McpOutcome::Result(r) => {
                require_tool_success(&r, "reboot");
                println!("{r}");
            }
            outcome => report_mcp_failure(dut, ports, outcome),
        }
    } else if dut.reset_ch > 0 {
        println!("Relay reset (channel {})...", dut.reset_ch);
        let args = serde_json::json!({"name":"serial_reset","arguments":{"wait_boot":true}});
        match mcp_call("tools/call", args, ports).await {
            McpOutcome::Result(r) => {
                require_tool_success(&r, "reboot");
                println!("{r}");
            }
            outcome => report_mcp_failure(dut, ports, outcome),
        }
    } else {
        println!("Software reboot via serial...");
        let args = serde_json::json!({"name":"serial_send_command","arguments":{"command":"reboot","timeout":5}});
        match mcp_call("tools/call", args, ports).await {
            McpOutcome::Result(r) => {
                require_tool_success(&r, "reboot");
                println!("{r}");
            }
            outcome => report_mcp_failure(dut, ports, outcome),
        }
    }
}

async fn cmd_uboot(dut: &sermcp::config::DutConfig, ports: &[u16]) {
    // Delegate to the platform-agnostic `cmd_bootloader` — kept as a
    // thin alias so existing `dutabo uboot` muscle memory keeps working.
    cmd_bootloader(dut, ports).await;
}

/// `dutabo bootloader` — enter the target's bootloader interactive prompt
/// (U-Boot, UEFI Shell, barebox, GRUB, ...) by interrupting its autoboot
/// countdown. `serial_enter_uboot` selects a hardware reset when available
/// and otherwise falls back to a software reboot.
///
/// The underlying MCP tools keep their `*_uboot` names for historical wire
/// stability — agents have prompts hardcoded around them — but this CLI
/// subcommand is named `bootloader` because the same interrupt-flood
/// mechanism works for any bootloader with an autoboot countdown. The
/// interrupt byte itself is configured via `UBOOT_INTERRUPT_CHAR`.
async fn cmd_bootloader(dut: &sermcp::config::DutConfig, ports: &[u16]) {
    let args = serde_json::json!({
        "name": "serial_enter_uboot",
        "arguments": {"restart": "auto"}
    });
    let r = match mcp_call("tools/call", args, ports).await {
        McpOutcome::Result(r) => r,
        outcome => report_mcp_failure(dut, ports, outcome),
    };
    match tool_payload(&r) {
        Ok(v) => {
            println!("{v}");
        }
        Err(error) => {
            eprintln!("enter bootloader failed: {error}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    }
}

/// `dutabo button <name> <press|release|pulse> [delay_ms]` — direct CLI
/// wrapper for the generic `serial_button` MCP tool. SoC-agnostic: the
/// caller decides which relay button (`reset` / `download` / whatever the
/// board wires) to drive, and composes a boot-mode sequence from those
/// presses — nothing here knows a vendor's sequence.
async fn cmd_button(dut: &sermcp::config::DutConfig, positional: &[String], ports: &[u16]) {
    let button = positional[0].as_str();
    let action = positional[1].as_str();
    let mut args = serde_json::json!({
        "name": "serial_button",
        "arguments": { "button": button, "action": action }
    });
    if positional.len() >= 3 {
        match positional[2].parse::<i64>() {
            Ok(delay) => {
                args["arguments"]["delay_ms"] = serde_json::json!(delay);
            }
            Err(_) => {
                eprintln!(
                    "Error: invalid delay_ms for `dutabo button`: {}",
                    positional[2]
                );
                print_usage();
                sermcp::trace_chrome::exit_with_trace(1);
            }
        }
    }
    let r = match mcp_call("tools/call", args, ports).await {
        McpOutcome::Result(r) => r,
        outcome => report_mcp_failure(dut, ports, outcome),
    };
    let v = require_tool_success(&r, &format!("button {button} {action}"));
    println!("{v}");
}

// ── Serial (interactive) ─────────────────────────────────────────────────

async fn cmd_serial(dut: &sermcp::config::DutConfig, ports: &[u16]) {
    // dutabo never starts or replaces a server; it only attaches to a
    // server's WebSocket relay. Candidates are probed in order (per-DUT
    // first, project port as fallback); the first server that is healthy
    // AND owns this DUT's serial endpoint wins.
    //
    // Interactive serial knobs ([dut.serial] backspace / enter / rx_newline
    // / highlight): per-DUT section values over the built-in defaults.
    let settings = SerialSettings::from_lookup(|key| dut.section_values.get(key).cloned());
    let (winner, owner_mismatch) = probe_serial_server_detailed(dut, ports).await;
    if let Some(port) = winner {
        serial_ws_relay(port, settings).await;
        // Session ended — clean exit, no message needed.
        return;
    }
    // No healthy MCP owns this DUT — rule: as long as .target.jsonc exists
    // and the dev host + DUT are healthy, `dutabo serial` must always be
    // able to interact. Connect directly to ser2net (host:port form), first
    // asking the endpoint-owning MCP to yield via SIGUSR1 and, if it never
    // does, taking the endpoint forcibly (explicit human preemption is
    // absolute; the agent-side hook still blocks dutabo serial).
    let Ok(serial_port_num) = dut.serial_port.parse::<u16>() else {
        eprintln!("{}", missing_server_message(dut, ports, owner_mismatch));
        sermcp::trace_chrome::exit_with_trace(1);
    };
    let lock_dir = sermcp::lock_manager::default_lock_dir();
    let mut yielded_owner: Option<u32> = None;
    let stream = match connect_serial_direct(
        &dut.dev_host_ip,
        serial_port_num,
        &lock_dir,
        &mut yielded_owner,
    )
    .await
    {
        DirectTakeover::Connected(stream) => stream,
        DirectTakeover::ForeignOwner(owner) => {
            eprintln!(
                "Serial endpoint {}:{} is held by pid {owner}, which dutabo serial \
                 does not take over (a non-MCP interactive session, or an MCP that \
                 survived forced takeover). Close that session first.",
                dut.dev_host_ip, serial_port_num
            );
            sermcp::trace_chrome::exit_with_trace(1);
        }
        DirectTakeover::Unreachable => {
            eprintln!(
                "No MCP service for device {} owns {}:{} and direct connect failed \
                 (dev host or serial endpoint unreachable).",
                dut.dut_name, dut.dev_host_ip, serial_port_num
            );
            sermcp::trace_chrome::exit_with_trace(1);
        }
    };
    serial_tcp_relay(
        stream,
        &format!(
            "Connected direct to {}:{}",
            dut.dev_host_ip, serial_port_num
        ),
        settings,
    )
    .await;
    // The human session owns the endpoint lock even when ser2net permits
    // several simultaneous TCP clients. Release it before asking the MCP we
    // preempted to re-acquire and reconnect.
    sermcp::lock_manager::release_lock(&dut.dev_host_ip, &serial_port_num.to_string(), &lock_dir);
    // Session ended: let the owner we preempted resume (SIGUSR2 → re-acquire
    // the lock + reconnect).
    if let Some(owner) = yielded_owner {
        eprintln!("Restoring serial ownership to MCP server (pid {owner})…");
        unsafe { libc::kill(owner as i32, libc::SIGUSR2) };
    }
}

/// Outcome of a direct serial takeover attempt.
enum DirectTakeover {
    Connected(tokio::net::TcpStream),
    /// The endpoint is held by a pid `dutabo serial` does not take over: any
    /// non-MCP owner (another interactive session, an unrelated program —
    /// first come wins among humans), or an MCP that survived every
    /// polite window and forced-takeover round. Holds the last observed pid.
    ForeignOwner(u32),
    /// We held the lock but ser2net refused the TCP connection.
    Unreachable,
}

/// Polite yield window before forced takeover. Covers the engine's 30 s
/// reconnect-backoff cap plus a moderate command hold; an owner still holding
/// the endpoint after this is wedged (or runs a binary without the SIGUSR1
/// handler) and the human takes over regardless.
const POLITE_YIELD_SECS: u64 = 45;
/// Re-send SIGUSR1 at this cadence while waiting: the first signal can race a
/// server that has not spawned its handler task yet.
const YIELD_NUDGE_SECS: u64 = 3;
/// Polite-window-plus-forced-takeover rounds. The first round settles every
/// realistic case (yield or steal); more is pathological lock churn we must
/// not fight forever.
const TAKEOVER_ROUNDS: usize = 3;

/// Direct-connect to the serial endpoint after atomically taking its lock.
///
/// The lock is checked BEFORE TCP connect: ser2net may allow multiple clients,
/// so "connect succeeded" does not prove exclusive access. A live MCP owner is
/// asked to yield with SIGUSR1; this dutabo process then claims the same lock,
/// preventing every Agent from reconnecting for the whole human session.
///
/// Preemption ladder for an MCP owner, per [`TAKEOVER_ROUNDS`]: a
/// [`POLITE_YIELD_SECS`] polite window (SIGUSR1 re-nudged every
/// [`YIELD_NUDGE_SECS`]), then forced takeover via
/// [`steal_lock`](sermcp::lock_manager::steal_lock) — human
/// preemption is absolute. `steal_lock` re-verifies the recorded pid right
/// before unlinking, so an owner that yielded or a brand-new owner is never
/// evicted. Non-MCP owners are never preempted.
async fn connect_serial_direct(
    host: &str,
    port: u16,
    lock_dir: &str,
    yielded_owner: &mut Option<u32>,
) -> DirectTakeover {
    let target = port.to_string();
    let lock_path = sermcp::lock_manager::endpoint_lock_path(host, &target, lock_dir);
    let my_pid = std::process::id();
    let mut requested_owner: Option<u32> = None;
    let mut last_holder: Option<u32> = None;
    let mut acquired = false;

    'rounds: for _ in 0..TAKEOVER_ROUNDS {
        let mut nudged_at = tokio::time::Instant::now();
        let polite_deadline = nudged_at + std::time::Duration::from_secs(POLITE_YIELD_SECS);
        // Claim the physical endpoint first. This is the authority boundary:
        // an MCP that starts or resumes during the human session sees dutabo's
        // PID and remains a non-owner, regardless of ser2net's multi-client
        // policy.
        loop {
            match sermcp::lock_manager::acquire_lock(host, &target, lock_dir) {
                None => {
                    acquired = true;
                    break 'rounds;
                }
                // A non-MCP holder is never preempted. (acquire_lock's
                // retry-exhausted fallback reports our own pid — a holder we
                // cannot identify as a server is exactly that: not ours to
                // displace.)
                Some(owner) if !is_mcp_process(owner) => {
                    last_holder = Some(owner);
                    break 'rounds;
                }
                Some(owner) => {
                    let now = tokio::time::Instant::now();
                    if requested_owner != Some(owner) {
                        eprintln!(
                            "Serial endpoint busy — asking MCP server (pid {owner}) to yield…"
                        );
                        unsafe { libc::kill(owner as i32, libc::SIGUSR1) };
                        requested_owner = Some(owner);
                        nudged_at = now;
                    } else if now - nudged_at >= std::time::Duration::from_secs(YIELD_NUDGE_SECS) {
                        eprintln!("  still waiting for MCP server (pid {owner}) to yield…");
                        unsafe { libc::kill(owner as i32, libc::SIGUSR1) };
                        nudged_at = now;
                    }
                    last_holder = Some(owner);
                }
            }
            if tokio::time::Instant::now() >= polite_deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        // Polite window over with the lock still held: displace the owner.
        // Losing the steal race (a new owner slipped in) just means the next
        // round politely asks THAT owner.
        if let Some(owner) = requested_owner
            && sermcp::lock_manager::steal_lock(host, &target, owner, lock_dir)
        {
            eprintln!(
                "MCP server (pid {owner}) did not yield within \
                 {POLITE_YIELD_SECS}s — taking over (human preemption is absolute)."
            );
        }
    }
    if !acquired {
        return match last_holder.or(requested_owner) {
            Some(owner) => DirectTakeover::ForeignOwner(owner),
            None => DirectTakeover::Unreachable,
        };
    }
    if sermcp::lock_manager::endpoint_lock_owner(&lock_path) != Some(my_pid) {
        // Lost the post-steal O_EXCL race to a fresh owner — do not fight it;
        // the human reruns against whoever holds the endpoint now.
        return match last_holder.or(requested_owner) {
            Some(owner) => DirectTakeover::ForeignOwner(owner),
            None => DirectTakeover::Unreachable,
        };
    }

    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    .ok()
    .and_then(Result::ok);
    if stream.is_none() {
        sermcp::lock_manager::release_lock(host, &target, lock_dir);
        if let Some(owner) = requested_owner {
            unsafe { libc::kill(owner as i32, libc::SIGUSR2) };
        }
        return DirectTakeover::Unreachable;
    }
    *yielded_owner = requested_owner;
    DirectTakeover::Connected(stream.unwrap())
}

/// True when the PID identifies a current or pre-rename server process.
fn is_mcp_process(pid: u32) -> bool {
    sermcp::lock_manager::is_live_server_holder(pid)
}

/// Write one sanitized passthrough chunk to stdout (non-interactive mode).
fn write_stdout(bytes: &[u8]) {
    use std::io::Write;
    if bytes.is_empty() {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// Shared interactive VT-core session.
///
/// `spawn_transport` wires the concrete transport (MCP WebSocket or direct
/// ser2net TCP) to the session channels: it forwards serial payload as
/// [`SessionEvent::Rx`] and consumes TX bytes from the bounded channel.
/// The returned task handles follow the fixed convention `[reader, writer]`
/// — the reader is aborted at teardown, the writer gets a short grace
/// window to flush its Close frame / EOF shutdown.
///
/// This function owns the RAII host terminal: raw mode + alternate screen
/// are restored on every exit path (Ctrl-T q, transport error, panic).
async fn run_interactive_session<F>(
    settings: SerialSettings,
    wire: sermcp::serial_term::WireMode,
    banner_text: &str,
    spawn_transport: F,
) -> SessionOutcome
where
    F: FnOnce(
        tokio::sync::mpsc::Sender<SessionEvent>,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) -> Vec<tokio::task::JoinHandle<()>>,
{
    let guard = match TerminalGuard::enter(settings.mouse) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Cannot enter raw mode: {e}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    };
    let (events_tx, events_rx) = tokio::sync::mpsc::channel::<SessionEvent>(64);
    let (tx, tx_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (prefs, prefs_error) = sermcp::serial_term::prefs::TerminalPrefs::load();
    if let Some(error) = prefs_error {
        eprintln!("warning: {error} — using the default terminal preferences");
    }
    let (terminal, flags) = spawn_terminal_thread(
        events_rx,
        tx.clone(),
        settings.clone(),
        prefs,
        wire,
        cols,
        rows,
        Box::new(StdoutSurface::new()),
    );
    let input = spawn_input_thread(
        stop.clone(),
        flags,
        events_tx.clone(),
        tx.clone(),
        InputEncoder::new(settings.backspace, settings.enter),
        prefs.scroll.jump_on_input,
    );
    // The relay itself holds no TX sender — the writer task ends once the
    // terminal and input threads drop theirs.
    drop(tx);

    eprintln!("{banner_text}");

    let transport = spawn_transport(events_tx.clone(), tx_rx);
    let outcome = terminal
        .join()
        .unwrap_or(SessionOutcome::NetClosed("terminal thread crashed".into()));

    // Teardown: the input thread only reads stdin (stop flag unblocks its
    // poll loop); the reader is aborted (the session is over either way);
    // the writer gets a grace window for a clean close.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = input.join();
    if let Some(reader) = transport.first() {
        reader.abort();
    }
    if let Some(writer) = transport.into_iter().nth(1) {
        // Grace window for the Close frame / EOF shutdown; a wedged socket
        // cannot hang the process — the runtime drops when main returns.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    }

    // Restore the host terminal BEFORE reporting, so messages land on the
    // normal screen.
    drop(guard);
    if let SessionOutcome::NetClosed(reason) = &outcome {
        eprintln!("Serial session ended: {reason}");
    }
    outcome
}

/// Direct ser2net TCP serial console for `dutabo serial` when no MCP server
/// owns the endpoint. Telnet IAC framing and the ser2net connect banner are
/// stripped client-side; the VT-core session is shared with the WS relay.
/// Non-TTY invocations keep the legacy sanitized passthrough (30s) so
/// `dutabo serial | tee` output stays byte-stable.
async fn serial_tcp_relay(stream: tokio::net::TcpStream, banner: &str, settings: SerialSettings) {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    // Keystrokes must not wait for TCP packet coalescing.
    let _ = stream.set_nodelay(true);
    let (mut read_half, write_half) = stream.into_split();

    if !std::io::stdin().is_terminal() {
        eprintln!("{banner} (non-interactive, 30s timeout)");
        let mut sanitizer = TerminalSanitizer::new(true);
        let mut out = Vec::with_capacity(4096);
        let mut buf = vec![0u8; 4096];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            if remain.is_zero() {
                break;
            }
            match tokio::time::timeout(remain, read_half.read(&mut buf)).await {
                Err(_) => break,
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => {
                    out.clear();
                    sanitizer.filter(&buf[..n], &mut out);
                    write_stdout(&out);
                }
            }
        }
        return;
    }

    run_interactive_session(
        settings,
        // Telnet IAC framing and the ser2net connect banner live on the
        // direct wire only.
        sermcp::serial_term::WireMode::DIRECT,
        &format!("{banner} (Ctrl-T q quit, Ctrl-T PgUp/PgDn or wheel scrollback)"),
        |events, mut tx_rx| {
            let reader = tokio::spawn(async move {
                let mut read_half = read_half;
                let mut buf = vec![0u8; 4096];
                loop {
                    match read_half.read(&mut buf).await {
                        Ok(0) => {
                            let _ = events
                                .send(SessionEvent::NetClosed("serial endpoint closed".into()))
                                .await;
                            break;
                        }
                        Ok(n) => {
                            if events
                                .send(SessionEvent::Rx(buf[..n].to_vec()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = events.send(SessionEvent::NetClosed(e.to_string())).await;
                            break;
                        }
                    }
                }
            });
            let writer = tokio::spawn(async move {
                let mut write_half = write_half;
                while let Some(bytes) = tx_rx.recv().await {
                    if write_half.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                let _ = write_half.shutdown().await;
            });
            vec![reader, writer]
        },
    )
    .await;
}

/// Probe `/health` on each candidate port; return the first port whose server
/// is healthy AND owns `dut`'s serial endpoint. Owner mismatches are recorded
/// so the caller can report the actual owner instead of a bare "does not own".
/// (Non-detailed convenience wrapper; the CLI uses the `_detailed` variant.)
#[cfg_attr(not(test), allow(dead_code))]
async fn probe_serial_server(dut: &sermcp::config::DutConfig, ports: &[u16]) -> Option<u16> {
    probe_serial_server_detailed(dut, ports).await.0
}

/// Like [`probe_serial_server`], but also returns the first owner mismatch as
/// `Some((probe_port, actual_host, actual_port))` for error reporting.
async fn probe_serial_server_detailed(
    dut: &sermcp::config::DutConfig,
    ports: &[u16],
) -> (Option<u16>, Option<(u16, String, String)>) {
    let mut owner_mismatch: Option<(u16, String, String)> = None;
    for &port in ports {
        let health_url = format!("http://127.0.0.1:{port}/health");
        let Some(health) = fetch_health(&health_url) else {
            continue;
        };
        if health_owns_dut(&health, dut) {
            return (Some(port), owner_mismatch);
        }
        // A server is up but owns a different DUT (stale or wrong
        // TARGET_DUT_NAME) — record it and keep probing the fallback ports.
        let other_host = health["serial"]["host"].as_str().unwrap_or("?").to_string();
        let other_port = health["serial"]["port"].as_str().unwrap_or("?").to_string();
        owner_mismatch = Some((port, other_host, other_port));
    }
    (None, owner_mismatch)
}

/// Does this `/health` response describe a healthy server owning `dut`?
fn health_owns_dut(health: &serde_json::Value, dut: &sermcp::config::DutConfig) -> bool {
    let serial_host = dut.dev_host_ip.as_str();
    health["status"] == "ok"
        && health["serial"]["owned"] == true
        && health["serial"]["host"].as_str() == Some(serial_host)
        && health["serial"]["port"].as_str() == Some(dut.serial_port.as_str())
}

/// Build the actionable "no MCP service" message. When `owner_mismatch` is
/// `Some((port, host, port))`, a server was up but owning a different DUT.
fn missing_server_message(
    dut: &sermcp::config::DutConfig,
    ports: &[u16],
    owner_mismatch: Option<(u16, String, String)>,
) -> String {
    let serial_host = dut.dev_host_ip.as_str();
    let project_port = ports.first().copied().unwrap_or(0);
    match owner_mismatch {
        Some((port, other_host, other_port)) => format!(
            "No shared MCP service for device {} ({serial_host}:{}) is reachable. \
             The project service on port {port} owns {other_host}:{other_port} instead. \
             Agent sessions share the project service; the first session normally manages its lifecycle. \
             From the project directory, restart the shared service and retry:\n  \
             sermcp --http 127.0.0.1:{project_port}",
            dut.dut_name, dut.serial_port
        ),
        None => format!(
            "No shared MCP service for device {} is available on project port {project_port}. \
             Agent sessions share the project service; the first session normally manages its lifecycle. \
             From the project directory, start the shared service and retry:\n  \
             sermcp --http 127.0.0.1:{project_port}",
            dut.dut_name
        ),
    }
}

// ── WebSocket serial relay (via MCP /serial/ws) ─────────────────────────

/// MCP WebSocket serial relay. The server's console driver already removed
/// telnet/RFC2217 framing before broadcasting, so payload arrives clean —
/// the VT-core session runs without client-side framing decode. Non-TTY
/// invocations keep the legacy sanitized passthrough (30s).
async fn serial_ws_relay(mcp_port: u16, settings: SerialSettings) {
    use futures_util::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let url = format!("ws://127.0.0.1:{mcp_port}/serial/ws");
    let (ws, _) = match connect_async(&url).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("WS connect failed: {e}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    };
    let (ws_sink, mut ws_stream) = ws.split();

    if !std::io::stdin().is_terminal() {
        eprintln!("Connected via MCP (non-interactive, 30s timeout)");
        let mut sanitizer = TerminalSanitizer::new(true);
        let mut out = Vec::with_capacity(4096);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            if remain.is_zero() {
                break;
            }
            match tokio::time::timeout(remain, ws_stream.next()).await {
                Err(_) => break,
                Ok(Some(Ok(Message::Binary(data)))) => {
                    out.clear();
                    sanitizer.filter(&data, &mut out);
                    write_stdout(&out);
                }
                Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => break,
                Ok(Some(Ok(_))) => {}
            }
        }
        return;
    }

    run_interactive_session(
        settings,
        // No telnet/banner decode: the server publishes clean payload.
        sermcp::serial_term::WireMode::RELAY,
        "Connected via MCP (Ctrl-T q quit, Ctrl-T PgUp/PgDn or wheel scrollback)",
        |events, mut tx_rx| {
            let reader = tokio::spawn(async move {
                let mut ws_stream = ws_stream;
                loop {
                    match ws_stream.next().await {
                        Some(Ok(Message::Binary(data))) => {
                            if events.send(SessionEvent::Rx(data.to_vec())).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            let _ = events
                                .send(SessionEvent::NetClosed("server closed the relay".into()))
                                .await;
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            let _ = events.send(SessionEvent::NetClosed(e.to_string())).await;
                            break;
                        }
                    }
                }
            });
            let writer = tokio::spawn(async move {
                use futures_util::SinkExt;
                let mut sink = ws_sink;
                // Serial bytes are binary: control keys, ESC and any
                // non-UTF-8 payload must traverse transparently.
                while let Some(bytes) = tx_rx.recv().await {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                let _ = sink.send(Message::Close(None)).await;
            });
            vec![reader, writer]
        },
    )
    .await;
}

async fn cmd_flash(
    dut: &sermcp::config::DutConfig,
    image_path: &str,
    image_type: &str,
    ports: &[u16],
) {
    let real = std::fs::canonicalize(image_path).unwrap_or_else(|_| PathBuf::from(image_path));
    if !real.exists() {
        eprintln!("Image not found: {}", real.display());
        sermcp::trace_chrome::exit_with_trace(1);
    }
    println!(
        "Image: {} ({} bytes)",
        real.display(),
        std::fs::metadata(&real).map(|m| m.len()).unwrap_or(0)
    );

    let plan = match mcp_call("tools/call", serde_json::json!({"name":"serial_flash_plan","arguments":{"image_path":real.to_string_lossy(),"image_type":image_type}}), ports).await {
        McpOutcome::Result(r) => serde_json::from_str::<serde_json::Value>(r["content"][0]["text"].as_str().unwrap_or("{}")).unwrap_or_default(),
        outcome => report_mcp_failure(dut, ports, outcome),
    };
    if !plan["success"].as_bool().unwrap_or(false) {
        eprintln!("{}", plan["error"].as_str().unwrap_or("unknown"));
        sermcp::trace_chrome::exit_with_trace(1);
    }

    let dh = plan["dev_host"].as_str().unwrap_or("");
    let du = plan["dev_user"].as_str().unwrap_or("");
    let rp = plan["remote_path"].as_str().unwrap_or("");
    let tool = plan["tool"].as_str().unwrap_or("");
    let fcmd = plan["selected_flash_cmd"].as_str().unwrap_or_else(|| {
        if image_type == "kernel" {
            plan["kernel_image_cmd"].as_str().unwrap_or("")
        } else {
            plan["full_image_cmd"].as_str().unwrap_or("")
        }
    });
    let lb = plan["loader_bin"].as_str().unwrap_or("");
    let list_devices_cmd = plan["list_devices_cmd"].as_str().unwrap_or("LD");
    let loader_cmd = plan["loader_cmd"].as_str().unwrap_or("");

    if dh.is_empty() {
        eprintln!("dev_host not configured");
        sermcp::trace_chrome::exit_with_trace(1);
    }

    // Upload
    println!("\n[1/3] Uploading to {du}@{dh}:{rp} ...");
    let scp_dest = format!("{du}@{dh}:{rp}");
    let ssh_dest = format!("{du}@{dh}");
    let real_str = real.to_string_lossy().to_string();
    if !std::process::Command::new("scp")
        .arg(&real_str)
        .arg(&scp_dest)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        eprintln!("SCP failed");
        sermcp::trace_chrome::exit_with_trace(1);
    }

    // Verify
    println!("\n[2/3] Verifying sha256 ...");
    let lsha = String::from_utf8_lossy(
        &std::process::Command::new("sha256sum")
            .arg(&real)
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default(),
    )
    .split_whitespace()
    .next()
    .unwrap_or("")
    .to_string();
    let rsha = String::from_utf8_lossy(
        &std::process::Command::new("ssh")
            .args([ssh_dest.as_str(), "sha256sum", rp])
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default(),
    )
    .split_whitespace()
    .next()
    .unwrap_or("")
    .to_string();
    if lsha != rsha || lsha.is_empty() {
        eprintln!("SHA256 MISMATCH: local={lsha} remote={rsha}");
        sermcp::trace_chrome::exit_with_trace(1);
    }
    println!("  OK");

    // Flash — auto-enter Loader mode if needed
    println!("\n[3/3] Flashing ...");
    let ld_cmd = format!("{tool} {list_devices_cmd}");

    // Check if device is connected, enter Loader mode if not
    let devs_out = std::process::Command::new("ssh")
        .args([ssh_dest.as_str(), ld_cmd.as_str()])
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    let devs = String::from_utf8_lossy(&devs_out).to_string();

    if devs.contains("connected(0)") || devs.contains("No found") {
        println!("  No device in Loader mode — sending 'reboot loader'...");
        // Use MCP to reboot target into Loader mode
        let _ = mcp_call(
            "tools/call",
            serde_json::json!({
                "name": "serial_send_raw", "arguments": {"data": "reboot loader\n"}
            }),
            ports,
        )
        .await;
        println!("  Waiting for device...");
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let check = std::process::Command::new("ssh")
                .args([ssh_dest.as_str(), ld_cmd.as_str()])
                .output()
                .map(|o| o.stdout)
                .unwrap_or_default();
            let check_str = String::from_utf8_lossy(&check);
            if !check_str.contains("connected(0)") && check_str.len() > 10 {
                break;
            }
        }
    }
    // Re-check device status
    let devs_out = std::process::Command::new("ssh")
        .args([ssh_dest.as_str(), ld_cmd.as_str()])
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    let devs = String::from_utf8_lossy(&devs_out).to_string();
    let download = devs.contains("Download") || devs.contains("MASKROM");

    if download && !lb.is_empty() {
        println!("  MASKROM detected → flashing loader: {lb}");
        let loader_full_cmd = format!("{tool} {loader_cmd}");
        std::process::Command::new("ssh")
            .args([ssh_dest.as_str(), loader_full_cmd.as_str()])
            .status()
            .ok();
        std::thread::sleep(std::time::Duration::from_secs(5));
    } else if devs.contains("Loader") {
        println!("  Device in Loader mode — flashing...");
    } else {
        eprintln!("  Device not detected. Ensure USB is connected and try again.");
        sermcp::trace_chrome::exit_with_trace(1);
    }

    let flash_cmd = format!("{tool} {fcmd}");
    let st = std::process::Command::new("ssh")
        .args([ssh_dest.as_str(), flash_cmd.as_str()])
        .status();
    match st {
        Ok(s) if s.success() => println!("  Done!"),
        Ok(s) => {
            eprintln!("  Failed (exit {s})");
            sermcp::trace_chrome::exit_with_trace(1);
        }
        Err(e) => {
            eprintln!("  Error: {e}");
            sermcp::trace_chrome::exit_with_trace(1);
        }
    }
}

#[cfg(test)]
#[path = "dutabo/tests.rs"]
mod tests;

/// Serialize env access between the bin tests that mutate env vars (the
/// Tests share this same mutex — one instance per process).
#[cfg(test)]
pub(crate) static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
