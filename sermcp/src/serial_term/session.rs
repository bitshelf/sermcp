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

use crossterm::event::{self, Event, MouseButton, MouseEvent, MouseEventKind};

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use super::SerialSettings;
use super::input::{InputEncoder, LocalInput};
use super::newline::RxNewlineNormalizer;
use super::renderer::{HostSurface, TerminalRenderer};
use super::scrollbar::{Scrollbar, WIDTH as SCROLLBAR_WIDTH};
use super::terminal::{TerminalCore, ViewScroll};
use super::wire::{WireDecoder, WireMode};

/// Events into the terminal thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Serial payload from the transport (wire framing included).
    Rx(Vec<u8>),
    /// Historical payload: render it without replying to old VT queries.
    History(Vec<u8>),
    /// Host window resize.
    Resize {
        cols: u16,
        rows: u16,
    },
    Paste(String),
    /// A forwarded key press; apply the local input scroll policy.
    Input,
    /// Host pointer events, resolved against the current terminal geometry.
    Mouse(MouseEvent),
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
    host_cols: u16,
    mouse: bool,
    drag: Option<usize>,
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
            SessionEvent::Rx(bytes) => self.handle_serial(tx, bytes, false),
            SessionEvent::History(bytes) => self.handle_serial(tx, bytes, true),
            SessionEvent::Input => {
                self.note_input();
                None
            }
            SessionEvent::Paste(text) => {
                self.note_input();
                let _ = tx.blocking_send(self.term.encode_paste(&text));
                None
            }
            SessionEvent::Resize { cols, rows } => {
                self.host_cols = cols;
                self.drag = None;
                let gutter = self.mouse && usize::from(cols) > SCROLLBAR_WIDTH;
                self.renderer.set_scrollbar(gutter);
                self.term.resize(
                    (cols as usize)
                        .saturating_sub(if gutter { SCROLLBAR_WIDTH } else { 0 })
                        .max(1),
                    rows as usize,
                );
                self.flags
                    .scrolled
                    .store(self.term.display_offset() > 0, Ordering::Relaxed);
                self.pending_render = true;
                None
            }
            SessionEvent::Mouse(mouse) => {
                self.handle_mouse(mouse);
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

    fn note_input(&mut self) {
        let before = self.term.display_offset();
        self.term.note_input();
        if self.term.display_offset() != before {
            self.drag = None;
            self.flags.scrolled.store(false, Ordering::Relaxed);
            self.pending_render = true;
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !self.mouse || usize::from(self.host_cols) <= SCROLLBAR_WIDTH {
            self.drag = None;
            return;
        }
        let snap = self.term.snapshot();
        let Some(bar) = Scrollbar::new(&snap) else {
            self.drag = None;
            return;
        };
        let row = usize::from(mouse.row).min(bar.height - 1);
        let offset = match mouse.kind {
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                return;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(grab) = self.drag else {
                    return;
                };
                Some(bar.offset(row, grab))
            }
            MouseEventKind::Down(MouseButton::Left)
                if (snap.cols..snap.cols + SCROLLBAR_WIDTH)
                    .contains(&usize::from(mouse.column)) =>
            {
                self.drag = None;
                if row == 0 {
                    Some(bar.history)
                } else if row + 1 == bar.height {
                    Some(0)
                } else if (bar.start..bar.start + bar.thumb).contains(&row) {
                    self.drag = Some(row - bar.start);
                    None
                } else {
                    // Track clicks place the thumb at the pointer and can
                    // continue directly into a drag, even outside the gutter.
                    self.drag = Some(bar.thumb / 2);
                    Some(bar.offset(row, bar.thumb / 2))
                }
            }
            MouseEventKind::Down(_) => {
                self.drag = None;
                None
            }
            _ => None,
        };
        if let Some(offset) = offset {
            let delta = offset as i32 - self.term.display_offset() as i32;
            self.term.scroll(ViewScroll::Delta(delta));
            self.flags
                .scrolled
                .store(self.term.display_offset() > 0, Ordering::Relaxed);
            self.pending_render = true;
        }
    }

    /// Ingest one serial chunk into the VT core. Historical chunks render but
    /// must not answer stale VT queries (cursor reports, DA1, …) — the answers
    /// would inject bytes into the live target.
    fn handle_serial(
        &mut self,
        tx: &mpsc::Sender<Vec<u8>>,
        bytes: Vec<u8>,
        historical: bool,
    ) -> Option<SessionOutcome> {
        let mut payload = Vec::with_capacity(bytes.len());
        self.wire.decode(&bytes, &mut payload);
        let mut display = Vec::with_capacity(payload.len());
        self.newline.normalize(&payload, &mut display);
        let was_alt_screen = self.term.alt_screen();
        self.term.process(&display);
        if was_alt_screen != self.term.alt_screen() {
            self.drag = None;
        }
        for reply in self.term.drain_replies() {
            if !historical {
                let _ = tx.blocking_send(reply);
            }
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
            let gutter = settings.mouse && usize::from(cols) > SCROLLBAR_WIDTH;
            let mut term = TerminalCore::new(
                (cols as usize)
                    .saturating_sub(if gutter { SCROLLBAR_WIDTH } else { 0 })
                    .max(1),
                rows as usize,
            );
            let mut renderer = TerminalRenderer::new(settings.highlight);
            renderer.set_scrollbar(gutter);
            term.set_scroll_policy(super::terminal::ScrollPolicy {
                follow_output: prefs.scroll.follow_output,
                jump_on_input: prefs.scroll.jump_on_input,
            });
            let mut state = SessionState {
                wire: WireDecoder::new(wire),
                newline: RxNewlineNormalizer::new(settings.rx_newline),
                term,
                renderer,
                surface,
                flags,
                pending_render: true,
                host_cols: cols,
                mouse: settings.mouse,
                drag: None,
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
                                // Resolve the offset on the terminal thread so
                                // an earlier queued scroll cannot race this key.
                                if jump_on_input {
                                    let _ = events.blocking_send(SessionEvent::Input);
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
                            MouseEventKind::Down(_)
                            | MouseEventKind::Drag(_)
                            | MouseEventKind::Up(_) => {
                                let _ = events.blocking_send(SessionEvent::Mouse(mouse));
                                continue;
                            }
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

#[cfg(test)]
mod history_tests {
    use super::*;

    struct Sink;
    impl HostSurface for Sink {
        fn write_all(&mut self, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn state() -> SessionState {
        let mut renderer = TerminalRenderer::new(SerialSettings::default().highlight);
        renderer.set_scrollbar(true);
        SessionState {
            wire: WireDecoder::new(WireMode::RELAY),
            newline: RxNewlineNormalizer::new(SerialSettings::default().rx_newline),
            term: TerminalCore::new(40, 10),
            renderer,
            surface: Box::new(Sink),
            flags: SessionFlags::default(),
            pending_render: false,
            host_cols: 41,
            mouse: true,
            drag: None,
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> SessionEvent {
        SessionEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
    }

    #[test]
    fn key_and_paste_return_to_live_without_waiting_for_remote_echo() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut state = state();
        let history: String = (0..100).map(|n| format!("line {n}\r\n")).collect();
        state.handle(&tx, SessionEvent::History(history.into_bytes()));
        // Process a queued scroll before input, regardless of the input
        // thread's last observed scrolled flag.
        state.handle(&tx, SessionEvent::Scroll(ViewScroll::Top));
        state.pending_render = false;
        state.handle(&tx, SessionEvent::Input);
        assert_eq!(state.term.display_offset(), 0);
        assert!(state.pending_render);
        assert!(!state.flags.scrolled.load(Ordering::Relaxed));
        assert!(
            rx.try_recv().is_err(),
            "the display event must not duplicate key bytes"
        );
        state.handle(&tx, SessionEvent::Scroll(ViewScroll::Top));
        state.handle(&tx, SessionEvent::Rx(b"new output\r\n\x1b[?2004h".to_vec()));
        assert!(
            state.term.display_offset() > 0,
            "output alone must keep the reading position"
        );
        state.handle(&tx, SessionEvent::Paste("hello".into()));
        assert_eq!(state.term.display_offset(), 0);
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[200~hello\x1b[201~");
        state.term.set_scroll_policy(super::super::ScrollPolicy {
            follow_output: true,
            jump_on_input: false,
        });
        state.handle(&tx, SessionEvent::Scroll(ViewScroll::Top));
        let before = state.term.display_offset();
        state.handle(&tx, SessionEvent::Input);
        state.handle(&tx, SessionEvent::Paste("kept".into()));
        assert_eq!(state.term.display_offset(), before);
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[200~kept\x1b[201~");
    }

    #[test]
    fn scrollbar_drag_and_bottom_click_work_while_output_continues_without_tx() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut state = state();
        let history: String = (0..100).map(|n| format!("line {n}\r\n")).collect();
        state.handle(&tx, SessionEvent::History(history.into_bytes()));
        let bar = Scrollbar::new(&state.term.snapshot()).unwrap();
        state.handle(
            &tx,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                40,
                bar.start as u16,
            ),
        );
        state.handle(&tx, mouse(MouseEventKind::Drag(MouseButton::Left), 30, 0));
        assert_eq!(
            state.term.display_offset(),
            state.term.snapshot().history_size
        );
        let before = state.term.snapshot().rows;
        state.handle(&tx, SessionEvent::Rx(b"new output\r\n".to_vec()));
        assert_eq!(state.term.snapshot().rows, before);
        state.handle(&tx, mouse(MouseEventKind::Drag(MouseButton::Left), 30, 9));
        assert_eq!(state.term.display_offset(), 0);
        state.handle(&tx, mouse(MouseEventKind::Up(MouseButton::Left), 30, 9));
        assert!(state.drag.is_none());
        state.handle(&tx, mouse(MouseEventKind::Down(MouseButton::Left), 40, 0));
        assert!(state.term.display_offset() > 0);
        state.handle(&tx, SessionEvent::Rx(b"latest output\r\n".to_vec()));
        state.handle(&tx, mouse(MouseEventKind::Down(MouseButton::Left), 40, 9));
        assert_eq!(state.term.display_offset(), 0);
        state.handle(&tx, SessionEvent::Rx(b"followed output\r\n".to_vec()));
        assert_eq!(state.term.display_offset(), 0);
        assert!(state.term.snapshot().rows.iter().any(|row| {
            row.iter()
                .map(|c| c.c)
                .collect::<String>()
                .starts_with("followed output")
        }));
        assert!(
            rx.try_recv().is_err(),
            "pointer actions must never reach the DUT"
        );
    }

    #[test]
    fn scrollbar_resizing_alt_screen_and_body_clicks_do_not_leave_a_drag_active() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut state = state();
        let history: String = (0..100).map(|n| format!("line {n}\r\n")).collect();
        state.handle(&tx, SessionEvent::History(history.into_bytes()));
        state.handle(&tx, mouse(MouseEventKind::Down(MouseButton::Left), 20, 0));
        assert_eq!(state.term.display_offset(), 0);
        state.handle(&tx, mouse(MouseEventKind::Down(MouseButton::Left), 40, 3));
        assert!(state.drag.is_some());
        state.handle(&tx, SessionEvent::Resize { cols: 60, rows: 20 });
        assert_eq!(state.term.cols(), 59);
        assert!(state.drag.is_none());
        state.handle(&tx, mouse(MouseEventKind::Down(MouseButton::Left), 59, 19));
        assert_eq!(state.term.display_offset(), 0);
        state.handle(&tx, SessionEvent::Rx(b"\x1b[?1049h".to_vec()));
        state.handle(&tx, mouse(MouseEventKind::Down(MouseButton::Left), 59, 0));
        assert!(state.drag.is_none());
        assert_eq!(state.term.display_offset(), 0);
        state.handle(&tx, SessionEvent::Rx(b"\x1b[?1049l".to_vec()));
        state.handle(&tx, SessionEvent::Resize { cols: 1, rows: 1 });
        state.handle(&tx, mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));
        state.render_if_pending();
        assert_eq!(state.term.cols(), 1);
        state.handle(&tx, SessionEvent::Resize { cols: 80, rows: 24 });
        state.mouse = false;
        state.handle(&tx, SessionEvent::Resize { cols: 80, rows: 24 });
        assert_eq!(state.term.cols(), 80);
        state.handle(&tx, mouse(MouseEventKind::Down(MouseButton::Left), 79, 0));
        assert!(state.drag.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn history_is_scrollable_and_queries_only_reply_when_live() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut state = SessionState {
            wire: WireDecoder::new(WireMode::RELAY),
            newline: RxNewlineNormalizer::new(SerialSettings::default().rx_newline),
            term: TerminalCore::new(40, 4),
            renderer: TerminalRenderer::new(SerialSettings::default().highlight),
            surface: Box::new(Sink),
            flags: SessionFlags::default(),
            pending_render: false,
            host_cols: 41,
            mouse: true,
            drag: None,
        };
        let mut history = Vec::new();
        for line in 0..20 {
            history.extend_from_slice(format!("history {line}\r\n").as_bytes());
        }
        history.extend_from_slice(b"\x1b[6n");
        state.handle(&tx, SessionEvent::History(history));
        assert!(
            rx.try_recv().is_err(),
            "old cursor queries must not inject replies"
        );
        state.handle(&tx, SessionEvent::Scroll(ViewScroll::Top));
        let before = state.term.snapshot().rows;
        let text: String = before[0].iter().map(|cell| cell.c).collect();
        assert!(text.starts_with("history 0"));
        state.handle(&tx, SessionEvent::Rx(b"live\r\n\x1b[6n".to_vec()));
        assert_eq!(
            before,
            state.term.snapshot().rows,
            "new output must keep the reading position"
        );
        assert!(
            rx.try_recv().unwrap().ends_with(b"R"),
            "live cursor queries still receive replies"
        );
        state.handle(&tx, SessionEvent::Scroll(ViewScroll::Bottom));
        assert_eq!(state.term.display_offset(), 0);
    }
}
