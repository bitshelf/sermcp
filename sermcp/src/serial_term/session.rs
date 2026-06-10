//! Session orchestration for `dutabo serial`: the single-owner terminal
//! thread plus the host keyboard thread and RAII terminal guard.
//!
//! Concurrency shape (no `Arc<Mutex<Term>>` anywhere):
//!
//! ```text
//! tokio WS/TCP reader task ──SessionEvent──► ┌──────────────────────┐
//! crossterm input thread ────SessionEvent──► │ terminal thread      │
//!                      │                     │ (sole Term owner)    │
//!                      └──key bytes──tx────► │  wire → newline → VT │──PtyWrite replies──► tx ──► forwarder task ──► transport
//!                                            │  damage → render     │──frame bytes──► surface
//!                                            └──────────────────────┘
//! ```
//!
//! The TX fast path never waits on rendering: key bytes go straight from
//! the input thread to the transport channel; the terminal thread only
//! appends VT-generated replies. RX ingestion and rendering are decoupled
//! by a coalescing window — under a boot-log flood the emulator keeps
//! consuming every chunk while visible renders are merged, and input
//! processing is never blocked.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use super::SerialSettings;
use super::input::{InputEncoder, LocalInput};
use super::newline::RxNewlineNormalizer;
use super::renderer::{HostSurface, TerminalRenderer};
use super::terminal::{TerminalCore, ViewScroll};
use super::wire::{WireDecoder, WireMode};

/// Events into the terminal thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Serial payload from the transport (wire framing included).
    Rx(Vec<u8>),
    /// Host window resize.
    Resize {
        cols: u16,
        rows: u16,
    },
    Paste(String),
    /// Local scrollback navigation (display-only).
    Scroll(super::terminal::ViewScroll),
    /// Transport closed or errored.
    NetClosed(String),
    /// Local quit (`Ctrl-T q`, stdin EOF).
    Quit,
}

/// Why the session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    Quit,
    NetClosed(String),
}

/// Render coalescing window: the only thing allowed to batch under load.
const RENDER_COALESCE: Duration = Duration::from_millis(4);

/// Input-thread poll cadence: how fast a shutdown flag is noticed when no
/// key is being pressed.
const INPUT_POLL: Duration = Duration::from_millis(200);

/// Terminal-thread machinery. `run` is the loop body, kept as a method so
/// tests can drive it without real threads.
struct SessionState {
    wire: WireDecoder,
    newline: RxNewlineNormalizer,
    term: TerminalCore,
    renderer: TerminalRenderer,
    surface: Box<dyn HostSurface + Send>,
    flags: SessionFlags,
    pending_render: bool,
}

/// Emulated-terminal state the input thread reads without touching the
/// terminal thread's ownership.
#[derive(Debug, Clone, Default)]
pub struct SessionFlags {
    /// DECCKM application cursor keys active (arrow encoding form).
    pub app_cursor: Arc<AtomicBool>,
    /// Remote full-screen application active (alt screen).
    pub alt_screen: Arc<AtomicBool>,
    /// Viewport scrolled into history (typing jumps back to the live view).
    pub scrolled: Arc<AtomicBool>,
}

impl SessionState {
    fn handle(
        &mut self,
        tx: &mpsc::Sender<Vec<u8>>,
        event: SessionEvent,
    ) -> Option<SessionOutcome> {
        match event {
            SessionEvent::Rx(bytes) => {
                let mut payload = Vec::with_capacity(bytes.len());
                self.wire.decode(&bytes, &mut payload);
                let mut display = Vec::with_capacity(payload.len());
                self.newline.normalize(&payload, &mut display);
                self.term.process(&display);
                for reply in self.term.drain_replies() {
                    let _ = tx.blocking_send(reply);
                }
                self.flags
                    .app_cursor
                    .store(self.term.app_cursor(), Ordering::Relaxed);
                self.flags
                    .alt_screen
                    .store(self.term.alt_screen(), Ordering::Relaxed);
                self.flags
                    .scrolled
                    .store(self.term.display_offset() > 0, Ordering::Relaxed);
                self.pending_render = true;
                None
            }
            SessionEvent::Paste(text) => {
                let _ = tx.blocking_send(self.term.encode_paste(&text));
                None
            }
            SessionEvent::Resize { cols, rows } => {
                self.term.resize(cols as usize, rows as usize);
                self.pending_render = true;
                None
            }
            SessionEvent::Scroll(view) => {
                self.term.scroll(view);
                self.flags
                    .scrolled
                    .store(self.term.display_offset() > 0, Ordering::Relaxed);
                self.pending_render = true;
                None
            }
            SessionEvent::NetClosed(reason) => Some(SessionOutcome::NetClosed(reason)),
            SessionEvent::Quit => Some(SessionOutcome::Quit),
        }
    }

    fn render_if_pending(&mut self) {
        if !self.pending_render {
            return;
        }
        let damage = self.term.take_damage();
        let snapshot = self.term.snapshot();
        if let Err(e) = self
            .renderer
            .render(&snapshot, &damage, self.surface.as_mut())
        {
            // Render failures must never take the serial session down.
            tracing::warn!("serial render failed: {e}");
        }
        self.pending_render = false;
    }
}

/// Spawn the terminal thread. Returns its handle plus the shared flags the
/// input thread reads (arrow-key encoding form, remote alt-screen state).
#[allow(clippy::too_many_arguments)]
pub fn spawn_terminal_thread(
    mut events: mpsc::Receiver<SessionEvent>,
    tx: mpsc::Sender<Vec<u8>>,
    settings: SerialSettings,
    prefs: super::prefs::TerminalPrefs,
    wire: WireMode,
    cols: u16,
    rows: u16,
    surface: Box<dyn HostSurface + Send>,
) -> (thread::JoinHandle<SessionOutcome>, SessionFlags) {
    let flags = SessionFlags::default();
    let out_flags = flags.clone();
    let handle = thread::Builder::new()
        .name("serial-terminal".into())
        .spawn(move || {
            let mut term = TerminalCore::new(cols as usize, rows as usize);
            term.set_scroll_policy(super::terminal::ScrollPolicy {
                follow_output: prefs.scroll.follow_output,
                jump_on_input: prefs.scroll.jump_on_input,
            });
            let mut state = SessionState {
                wire: WireDecoder::new(wire),
                newline: RxNewlineNormalizer::new(settings.rx_newline),
                term,
                renderer: TerminalRenderer::new(settings.highlight),
                surface,
                flags,
                pending_render: true,
            };
            loop {
                let event = match events.blocking_recv() {
                    Some(event) => event,
                    None => return SessionOutcome::NetClosed("transport closed".into()),
                };
                let mut outcome = state.handle(&tx, event);
                // Coalesce: drain everything already queued (bounded by the
                // coalescing window) and render once per batch.
                let deadline = Instant::now() + RENDER_COALESCE;
                while outcome.is_none() {
                    match events.try_recv() {
                        Ok(event) => outcome = state.handle(&tx, event),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            outcome = Some(SessionOutcome::NetClosed("transport closed".into()));
                            break;
                        }
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                }
                state.render_if_pending();
                if let Some(outcome) = outcome {
                    return outcome;
                }
            }
        })
        .expect("spawn serial-terminal thread");
    (handle, out_flags)
}

/// A typed key while the viewport is in history: the view resets to the
/// live bottom (display-only; the key bytes were already transmitted).
pub(super) fn typing_view_reset(scrolled: bool) -> Option<SessionEvent> {
    scrolled.then_some(SessionEvent::Scroll(ViewScroll::Bottom))
}

/// Spawn the host keyboard thread. Encoded key bytes go straight to the
/// transport channel; resize, scroll and quit go through the session
/// events. Wheel notches scroll the local scrollback (or drive arrow keys
/// for a remote full-screen application).
pub fn spawn_input_thread(
    stop: Arc<AtomicBool>,
    flags: SessionFlags,
    events: mpsc::Sender<SessionEvent>,
    tx: mpsc::Sender<Vec<u8>>,
    encoder: InputEncoder,
    jump_on_input: bool,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("serial-input".into())
        .spawn(move || {
            let mut local = LocalInput::new(encoder);
            while !stop.load(Ordering::Relaxed) {
                let Ok(ready) = event::poll(INPUT_POLL) else {
                    break;
                };
                if !ready {
                    continue;
                }
                let Ok(event) = event::read() else {
                    break;
                };
                match event {
                    Event::Key(key) => {
                        let app_cursor = flags.app_cursor.load(Ordering::Relaxed);
                        match local.handle(key, app_cursor) {
                            super::input::LocalAction::Bytes(bytes) => {
                                let _ = tx.blocking_send(bytes);
                                // Typing jumps back to the live view ONLY
                                // when the user opted in
                                // (terminal.toml scroll.jump_on_input) —
                                // the Xshell-like default keeps the view
                                // wherever it was scrolled.
                                if jump_on_input
                                    && let Some(jump) =
                                        typing_view_reset(flags.scrolled.load(Ordering::Relaxed))
                                {
                                    let _ = events.blocking_send(jump);
                                }
                            }
                            super::input::LocalAction::Quit => {
                                let _ = events.blocking_send(SessionEvent::Quit);
                                break;
                            }
                            super::input::LocalAction::Scroll(scroll) => {
                                let view = match scroll {
                                    super::input::ScrollKey::PageUp => ViewScroll::PageUp,
                                    super::input::ScrollKey::PageDown => ViewScroll::PageDown,
                                    super::input::ScrollKey::Top => ViewScroll::Top,
                                    super::input::ScrollKey::Bottom => ViewScroll::Bottom,
                                };
                                let _ = events.blocking_send(SessionEvent::Scroll(view));
                            }
                            super::input::LocalAction::None => {}
                        }
                    }
                    Event::Paste(text) => {
                        let _ = events.blocking_send(SessionEvent::Paste(text));
                    }
                    Event::Mouse(mouse) => {
                        let up = match mouse.kind {
                            crossterm::event::MouseEventKind::ScrollUp => true,
                            crossterm::event::MouseEventKind::ScrollDown => false,
                            // Clicks/drag/release: not relayed — text
                            // selection stays a host concern
                            // (Shift+drag where the host supports it).
                            _ => continue,
                        };
                        let alt_screen = flags.alt_screen.load(Ordering::Relaxed);
                        match super::input::wheel_action(up, alt_screen) {
                            super::input::WheelAction::Scroll(view) => {
                                let _ = events.blocking_send(SessionEvent::Scroll(view));
                            }
                            super::input::WheelAction::Send(bytes) => {
                                let _ = tx.blocking_send(bytes);
                            }
                        }
                    }
                    Event::Resize(cols, rows) => {
                        let _ = events.blocking_send(SessionEvent::Resize { cols, rows });
                    }
                    _ => {}
                }
            }
            let _ = events.blocking_send(SessionEvent::Quit);
        })
        .expect("spawn serial-input thread")
}

/// RAII host terminal state. `enter` puts the host into raw mode on the
/// alternate screen; `Drop` restores everything — covering Ctrl-T q,
/// transport errors, early returns and panics.
///
/// Host compatibility is CAPABILITY-PROBED, never name-sniffed:
/// * the kitty keyboard protocol is queried (WezTerm, Windows Terminal,
///   kitty, foot, … answer; xterm/Xshell/VTE answer the DA fallback and
///   report no support) and pushed only when supported — modified keys
///   (Shift+PageUp, Alt combos) then arrive unambiguously everywhere that
///   can report them;
/// * everything else (raw mode, alternate screen, SGR mouse reporting) is
///   universal VT standard, applied identically on every host.
pub struct TerminalGuard {
    entered: bool,
    mouse: bool,
    keyboard_enhanced: bool,
}

impl TerminalGuard {
    /// `mouse`: capture mouse reporting so wheel notches reach the session
    /// (text selection stays available via Shift+drag on hosts that follow
    /// the xterm convention). Pass `false` to keep the host's default
    /// mouse behavior entirely.
    pub fn enter(mouse: bool) -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut guard = Self {
            entered: true,
            mouse,
            keyboard_enhanced: false,
        };
        // Probe BEFORE the alternate screen and before the input thread
        // starts reading events (the probe consumes its own query replies).
        // Unsupported terminals answer the primary DA within milliseconds;
        // a failure to probe at all simply keeps the legacy input encoding.
        guard.keyboard_enhanced = crossterm::terminal::supports_keyboard_enhancement()
            .ok()
            .unwrap_or(false);
        if guard.keyboard_enhanced {
            use crossterm::event::KeyboardEnhancementFlags;
            let _ = crossterm::execute!(
                io::stdout(),
                crossterm::event::PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                )
            );
        }
        if guard.mouse {
            let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture);
        }
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0),
            crossterm::cursor::Show
        );
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.entered {
            if self.mouse {
                let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
            }
            let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
            if self.keyboard_enhanced {
                let _ = crossterm::execute!(
                    io::stdout(),
                    crossterm::event::PopKeyboardEnhancementFlags
                );
            }
            let _ = crossterm::execute!(
                io::stdout(),
                crossterm::cursor::Show,
                crossterm::terminal::LeaveAlternateScreen
            );
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}
