//! Side-effect-free replay tests for the interactive serial terminal.
//!
//! The central invariant (doc §52): removing the semantic highlighter must
//! not change RX/TX bytes, screen text, cursor, geometry or terminal
//! behavior — only colors may differ. Everything here replays fixtures
//! through the same pipeline the session uses (newline → VT core →
//! damage → renderer) against an in-memory host surface.

use super::*;

struct ScriptedSurface {
    out: Vec<u8>,
}

impl ScriptedSurface {
    fn new() -> Self {
        Self { out: Vec::new() }
    }
}

impl HostSurface for ScriptedSurface {
    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.out.extend_from_slice(data);
        Ok(())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One full replay: returns what the user would observe.
struct Replay {
    /// Raw bytes consumed (identical by construction; asserted anyway).
    raw_rx: Vec<u8>,
    /// Visible screen text, wide spacers collapsed, trailing blanks cut.
    screen_text: Vec<String>,
    cursor: (usize, usize),
    cursor_visible: bool,
    /// Last frame's highlight overlay per row.
    overlays: Vec<Vec<super::highlight::HighlightSpan>>,
    /// Everything painted to the host surface (style evidence).
    painted: Vec<u8>,
}

fn replay(chunks: &[&[u8]], settings: &SerialSettings) -> Replay {
    replay_sized(chunks, settings, 80, 24)
}

fn replay_sized(chunks: &[&[u8]], settings: &SerialSettings, cols: usize, rows: usize) -> Replay {
    let mut term = TerminalCore::new(cols, rows);
    let mut newline = RxNewlineNormalizer::new(settings.rx_newline);
    let mut renderer = TerminalRenderer::new(settings.highlight);
    let mut surface = ScriptedSurface::new();
    let mut raw_rx = Vec::new();
    for chunk in chunks {
        raw_rx.extend_from_slice(chunk);
        let mut display = Vec::new();
        newline.normalize(chunk, &mut display);
        term.process(&display);
        term.drain_replies();
        let damage = term.take_damage();
        let snapshot = term.snapshot();
        renderer.render(&snapshot, &damage, &mut surface).unwrap();
    }
    let snapshot = term.snapshot();
    let overlays = (0..snapshot.rows.len())
        .map(|row| renderer.presented_overlay(row))
        .collect();
    Replay {
        raw_rx,
        screen_text: screen_text(&snapshot),
        cursor: snapshot.cursor,
        cursor_visible: snapshot.cursor_visible,
        overlays,
        painted: surface.out,
    }
}

fn screen_text(snapshot: &terminal::ScreenSnapshot) -> Vec<String> {
    snapshot
        .rows
        .iter()
        .map(|row| {
            let mut s = String::new();
            for cell in row {
                if !cell.wide_spacer {
                    s.push(cell.c);
                }
            }
            s.trim_end().to_string()
        })
        .collect()
}

fn off() -> SerialSettings {
    SerialSettings {
        highlight: highlight::HighlightStyle::Off,
        ..SerialSettings::default()
    }
}

fn on() -> SerialSettings {
    SerialSettings::default()
}

const PROMPT_SESSION: &[&[u8]] = &[b"[    0.000000] Booting Linux\r\n", b"root@rk3576:/# "];

// ── §37: highlight on/off invariant ───────────────────────────────────────

#[test]
fn highlight_toggle_changes_styles_only() {
    let a = replay(PROMPT_SESSION, &off());
    let b = replay(PROMPT_SESSION, &on());

    assert_eq!(a.raw_rx, b.raw_rx);
    assert_eq!(a.screen_text, b.screen_text);
    assert_eq!(a.cursor, b.cursor);
    assert_eq!(a.cursor_visible, b.cursor_visible);
    assert_eq!(a.overlays.len(), b.overlays.len());
    // Only rendered styles may differ: the overlay colored the prompt.
    assert!(!b.overlays[1].is_empty(), "prompt row has an overlay");
    assert!(a.overlays[1].is_empty(), "highlight off has no overlay");
    assert_ne!(a.painted, b.painted, "rendered styles differ");
    // The green user@host SGR (32) appears only in the highlighted frame.
    assert!(b.painted.windows(3).any(|w| w == b";32"));
}

#[test]
fn tx_bytes_independent_of_highlight() {
    let keys = [
        crossterm::event::KeyCode::Char('l'),
        crossterm::event::KeyCode::Char('s'),
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyCode::Backspace,
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyCode::Tab,
    ];
    for mode in [off(), on()] {
        let enc = InputEncoder::new(mode.backspace, mode.enter);
        let mut all = Vec::new();
        for k in keys {
            enc.encode(k, crossterm::event::KeyModifiers::NONE, false, &mut all);
        }
        assert_eq!(all, b"ls\r\x7f\x1b[A\x09");
    }
}

// ── §38: chunk independence ───────────────────────────────────────────────

#[test]
fn same_stream_any_chunking_same_terminal_state() {
    let stream: &[&[u8]] = &[
        b"[    0.000000] Booting Linux on physical CPU\r\n",
        b"\x1b[?2004hroot@rk3576:~# uptime\r\n",
        b" 21:42:05 up 1:02,  0 users,  load average: 0.00, 0.00, 0.00\r\n",
        b"root@rk3576:~# ",
    ];
    let whole = replay(stream, &on());
    let bytewise = replay(&[&stream.concat()[..]], &on());
    let split = replay(
        &[
            b"[    0.000000] Bootin",
            b"g Linux on physical CPU\r\n\x1b[?2004",
            b"hroot@rk3576:~# uptime",
            b"\r\n 21:42:05 up 1:02,",
            b"  0 users,  load average:",
            b" 0.00, 0.00, 0.00\r\nro",
            b"ot@rk3576:~# ",
        ],
        &on(),
    );
    assert_eq!(whole.screen_text, bytewise.screen_text);
    assert_eq!(whole.cursor, bytewise.cursor);
    assert_eq!(whole.overlays, bytewise.overlays);
    assert_eq!(whole.screen_text, split.screen_text);
    assert_eq!(whole.cursor, split.cursor);
    assert_eq!(whole.overlays, split.overlays);
}

#[test]
fn random_chunk_splits_converge() {
    use proptest::prelude::*;
    proptest!(ProptestConfig::with_cases(24), |(splits in prop::collection::vec(1usize..24, 0..16))| {
        let data = PROMPT_SESSION.concat();
        let mut chunks: Vec<&[u8]> = Vec::new();
        let mut start = 0usize;
        for s in splits {
            let end = (start + s).min(data.len());
            chunks.push(&data[start..end]);
            start = end;
            if start >= data.len() { break; }
        }
        if start < data.len() {
            chunks.push(&data[start..]);
        }
        let whole = replay(&[&data[..]], &on());
        let split = replay(&chunks, &on());
        prop_assert_eq!(whole.screen_text, split.screen_text);
        prop_assert_eq!(whole.cursor, split.cursor);
        prop_assert_eq!(whole.overlays, split.overlays);
    });
}

// ── §39: backspace ────────────────────────────────────────────────────────

#[test]
fn backspace_echo_erases_via_tty_semantics() {
    // Classic tty erase: BS, overwrite with space, BS again.
    let r = replay(&[b"abc\x08 \x08"], &on());
    assert_eq!(r.screen_text[0], "ab");
    assert_eq!(r.cursor, (2, 0));
}

#[test]
fn bare_backspace_moves_cursor_without_erasing() {
    let r = replay(&[b"abc\x08"], &on());
    assert_eq!(r.screen_text[0], "abc");
    assert_eq!(r.cursor, (2, 0));
}

#[test]
fn tx_backspace_byte_per_mode() {
    let mut out = Vec::new();
    InputEncoder::new(BackspaceMode::Delete, EnterMode::Cr).encode(
        crossterm::event::KeyCode::Backspace,
        crossterm::event::KeyModifiers::NONE,
        false,
        &mut out,
    );
    assert_eq!(out, vec![0x7f]);
    let mut out = Vec::new();
    InputEncoder::new(BackspaceMode::CtrlH, EnterMode::Cr).encode(
        crossterm::event::KeyCode::Backspace,
        crossterm::event::KeyModifiers::NONE,
        false,
        &mut out,
    );
    assert_eq!(out, vec![0x08]);
}

// ── §40: CR/LF matrix ─────────────────────────────────────────────────────

#[test]
fn crlf_and_cr_only_are_verbatim_in_both_modes() {
    for settings in [off(), on()] {
        let r = replay(&[b"a\r\nb"], &settings);
        assert_eq!(r.screen_text[0], "a");
        assert_eq!(r.screen_text[1], "b");
        let r = replay(&[b"a\rb"], &settings);
        // True VT CR: back to column zero — 'b' overwrites 'a'.
        assert_eq!(r.screen_text[0], "b");
    }
}

#[test]
fn lone_lf_staircases_in_raw_and_resets_in_lf_implies_cr() {
    let r = replay(
        &[b"a\nb"],
        &SerialSettings {
            rx_newline: RxNewlineMode::Raw,
            ..on()
        },
    );
    assert_eq!(r.screen_text[0], "a");
    assert_eq!(r.screen_text[1], " b", "raw LF keeps the column (true VT)");

    let r = replay(&[b"a\nb"], &on());
    assert_eq!(r.screen_text[1], "b");
    assert_eq!(r.cursor, (1, 1));
}

#[test]
fn lf_implies_cr_survives_chunk_boundaries() {
    let r = replay(&[b"a\r", b"\nb"], &on());
    assert_eq!(r.screen_text[1], "b");
    let r = replay(&[b"prompt# ", b"out1\n", b"out2\n"], &on());
    assert_eq!(r.screen_text[0], "prompt# out1");
    assert_eq!(r.screen_text[1], "out2");
}

// ── §41: prompt retroactive repaint ───────────────────────────────────────

#[test]
fn prompt_completing_retroactively_recolors_whole_prompt() {
    // Frame 1: an unrecognized prefix arrives — plain, no overlay.
    let mut term = TerminalCore::new(80, 24);
    let mut renderer = TerminalRenderer::new(highlight::HighlightStyle::Prompt);
    let mut surface = ScriptedSurface::new();
    let mut newline = RxNewlineNormalizer::new(RxNewlineMode::LfImpliesCr);
    let feed = |term: &mut TerminalCore,
                newline: &mut RxNewlineNormalizer,
                renderer: &mut TerminalRenderer,
                surface: &mut ScriptedSurface,
                data: &[u8]| {
        let mut display = Vec::new();
        newline.normalize(data, &mut display);
        term.process(&display);
        let damage = term.take_damage();
        let snapshot = term.snapshot();
        renderer.render(&snapshot, &damage, surface).unwrap();
    };
    feed(
        &mut term,
        &mut newline,
        &mut renderer,
        &mut surface,
        b"root@bo",
    );
    assert!(
        renderer.presented_overlay(0).is_empty(),
        "incomplete prefix has no overlay yet"
    );
    let painted_before = surface.out.len();
    // Frame 2: only the tail is NEW terminal damage — the semantic change
    // must repaint the whole row, including the already-shown "root@bo".
    feed(
        &mut term,
        &mut newline,
        &mut renderer,
        &mut surface,
        b"ard:/# ",
    );
    let overlay = renderer.presented_overlay(0);
    assert_eq!(overlay.len(), 3, "user/dir/sigil spans");
    let tail = &surface.out[painted_before..];
    let tail_str = String::from_utf8_lossy(tail).to_string();
    assert!(
        tail_str.contains("root@board"),
        "repaint covers the previously plain columns: {tail_str:?}"
    );
    assert!(
        tail_str.contains("\x1b[1;1H"),
        "repaint starts at column 1 of the row"
    );
}

// ── §42: ANSI / full-screen ───────────────────────────────────────────────

#[test]
fn ansi_fixture_sgr_clear_and_cursor_survive_highlight_toggle() {
    let fixture: &[&[u8]] = &[
        b"line one\x1b[K\r\n",
        b"\x1b[32mgreen\x1b[0m plain\r\n",
        b"\x1b[2;5Hjumped\r\n",
        b"\x1b[2J\x1b[Hcleared",
    ];
    let a = replay(fixture, &off());
    let b = replay(fixture, &on());
    assert_eq!(a.screen_text, b.screen_text);
    assert_eq!(a.cursor, b.cursor);
    assert_eq!(a.screen_text[0], "cleared");
}

#[test]
fn full_screen_application_not_broken_by_highlight() {
    // A vim-style alternate-screen burst: enter alt screen, draw a colored
    // ruler, leave. Screen text must be identical with highlight on/off.
    let fixture: &[&[u8]] = &[
        b"\x1b[?1049h\x1b[H\x1b[2J",
        b"~\r\n~\r\n\x1b[1;32m-- INSERT --\x1b[0m\r\n",
        b"\x1b[4;1Htype here",
        b"\x1b[?1049l",
        b"\r\nroot@rk3576:/# ",
    ];
    let a = replay(fixture, &off());
    let b = replay(fixture, &on());
    assert_eq!(a.screen_text, b.screen_text);
    assert_eq!(a.cursor, b.cursor);
    // Back on the main screen (cursor pushed to row 1 by the CRLF), the
    // prompt is highlighted again.
    assert!(!b.overlays[1].is_empty());
}

#[test]
fn alt_screen_disables_semantic_highlight() {
    let r = replay(&[b"\x1b[?1049hroot@rk3576:/# "], &on());
    assert!(
        r.overlays[0].is_empty(),
        "no prompt overlay inside alternate screen"
    );
    assert_eq!(r.screen_text[0], "root@rk3576:/#");
}

#[test]
fn remote_sgr_wins_over_overlay() {
    // A PS1 that colors its own user@host: the overlay matches the row but
    // must not restyle cells the remote already colored — the renderer only
    // recolors unstyled cells, so magenta survives and only the plain dir
    // part gets overlay colors.
    let r = replay(&[b"\x1b[35mroot@rk3576\x1b[0m:/# "], &on());
    assert_eq!(r.overlays[0].len(), 3, "overlay exists (text-level)");
    assert!(
        r.painted.windows(3).any(|w| w == b";35"),
        "remote magenta fg survives in the painted frame"
    );
    let mut term = TerminalCore::new(80, 24);
    term.process(b"\x1b[35mroot@rk3576\x1b[0m:/# ");
    let snapshot = term.snapshot();
    assert_eq!(snapshot.rows[0][0].fg, ColorView::Palette(5));
}

// ── §10: terminal-generated replies ───────────────────────────────────────

#[test]
fn dsr_query_produces_pty_write_reply() {
    let mut term = TerminalCore::new(80, 24);
    term.process(b"ab\x1b[6n");
    let replies = term.drain_replies();
    assert_eq!(replies.len(), 1, "exactly one DSR reply");
    let reply = String::from_utf8(replies[0].clone()).unwrap();
    assert_eq!(reply, "\x1b[1;3R", "cursor at row 1 col 3 (1-based)");
}

// ── resize / wide chars / wire ─────────────────────────────────────────────

#[test]
fn resize_marks_full_damage_and_keeps_cursor() {
    let mut term = TerminalCore::new(80, 24);
    term.process(b"hello\r\nworld");
    term.take_damage();
    term.resize(40, 10);
    assert_eq!(term.take_damage(), DamageReport::Full);
    let snapshot = term.snapshot();
    assert_eq!((snapshot.cols, snapshot.lines), (40, 10));
    assert_eq!(snapshot.cursor, (5, 1));
}

#[test]
fn wide_characters_occupy_two_columns() {
    let r = replay(&["中文x".as_bytes()], &on());
    assert_eq!(r.screen_text[0], "中文x");
    assert_eq!(r.cursor, (5, 0), "two wide chars = 4 columns + 1");
}

#[test]
fn wire_banner_and_telnet_are_display_path_only() {
    // The wire decoder strips ser2net framing; the same stream WITH
    // framing produces the same screen as the clean stream without it.
    let clean = replay(&[b"root@rk3576:/# "], &on());
    let mut wire = WireDecoder::new(WireMode::DIRECT);
    let mut payload = Vec::new();
    wire.decode(
        b"\xff\xfb\x2cser2net port 9999\r\n\r\nroot@rk3576:/# ",
        &mut payload,
    );
    let framed = replay(&[&payload], &on());
    assert_eq!(clean.screen_text, framed.screen_text);
    assert_eq!(clean.cursor, framed.cursor);
}

// ── settings resolution ────────────────────────────────────────────────────

#[test]
fn settings_parse_from_flat_keys() {
    let settings = SerialSettings::from_lookup(|key| match key {
        "SERIAL_BACKSPACE" => Some("ctrl-h".into()),
        "SERIAL_ENTER" => Some("crlf".into()),
        "SERIAL_RX_NEWLINE" => Some("raw".into()),
        "SERIAL_HIGHLIGHT" => Some("off".into()),
        _ => None,
    });
    assert_eq!(settings.backspace, BackspaceMode::CtrlH);
    assert_eq!(settings.enter, EnterMode::CrLf);
    assert_eq!(settings.rx_newline, RxNewlineMode::Raw);
    assert_eq!(settings.highlight, highlight::HighlightStyle::Off);
    // Unknown values keep defaults instead of panicking mid-session.
    let settings = SerialSettings::from_lookup(|key| match key {
        "SERIAL_HIGHLIGHT" => Some("rainbow".into()),
        _ => None,
    });
    assert_eq!(settings.highlight, highlight::HighlightStyle::Prompt);
}

// ── scrollback (local viewport scroll; display state only) ────────────────

fn scroll_feed(term: &mut TerminalCore, count: usize) {
    for i in 0..count {
        term.process(format!("line {i}\r\n").as_bytes());
    }
}

#[test]
fn scrollback_reveals_history_and_returns_to_bottom() {
    let mut term = TerminalCore::new(80, 6);
    scroll_feed(&mut term, 12);

    let snap = term.snapshot();
    assert_eq!(snap.display_offset, 0);
    assert_eq!(screen_text(&snap)[0], "line 7", "bottom view shows newest");

    term.scroll(super::terminal::ViewScroll::Top);
    let snap = term.snapshot();
    assert!(snap.display_offset > 0, "scrolled into history");
    assert_eq!(screen_text(&snap)[0], "line 0", "top of history visible");

    term.scroll(super::terminal::ViewScroll::Bottom);
    let snap = term.snapshot();
    assert_eq!(snap.display_offset, 0);
    assert_eq!(screen_text(&snap)[0], "line 7", "back to the live view");
}

#[test]
fn scrolled_view_stays_anchored_under_new_output() {
    let mut term = TerminalCore::new(80, 4);
    scroll_feed(&mut term, 8);
    term.scroll(super::terminal::ViewScroll::Top);
    let before = screen_text(&term.snapshot());
    term.process(b"more A\r\nmore B\r\n");
    let after = screen_text(&term.snapshot());
    assert_eq!(before, after, "new output does not move the scrolled view");
}

#[test]
fn scroll_does_not_touch_serial_state() {
    // Scrolling is pure display state: the same bytes with and without
    // interleaved scrolling produce identical terminal content at offset 0.
    let mut a = TerminalCore::new(80, 6);
    let mut b = TerminalCore::new(80, 6);
    for i in 0..12 {
        let data = format!("line {i}\r\n");
        a.process(data.as_bytes());
        b.process(data.as_bytes());
        b.scroll(super::terminal::ViewScroll::PageUp);
        b.scroll(super::terminal::ViewScroll::PageDown);
    }
    b.scroll(super::terminal::ViewScroll::Top);
    b.scroll(super::terminal::ViewScroll::Bottom);
    assert_eq!(screen_text(&a.snapshot()), screen_text(&b.snapshot()));
    assert_eq!(a.snapshot().cursor, b.snapshot().cursor);
    assert_eq!(a.drain_replies(), b.drain_replies());
}

#[test]
fn cursor_hidden_while_scrolled_and_restored_at_bottom() {
    use super::highlight;
    use super::renderer::HostSurface;

    struct Sink(Vec<u8>);
    impl HostSurface for Sink {
        fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
            self.0.extend_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let render = |term: &mut TerminalCore, sink: &mut Sink| {
        let damage = term.take_damage();
        let snapshot = term.snapshot();
        let mut renderer = TerminalRenderer::new(highlight::HighlightStyle::Off);
        renderer.render(&snapshot, &damage, sink).unwrap();
    };
    let mut term = TerminalCore::new(80, 4);
    scroll_feed(&mut term, 8);
    let mut sink = Sink(Vec::new());
    render(&mut term, &mut sink);
    assert!(
        sink.0.windows(6).any(|w| w == b"\x1b[?25h"),
        "cursor shown at bottom"
    );
    sink.0.clear();
    term.scroll(super::terminal::ViewScroll::Top);
    render(&mut term, &mut sink);
    assert!(
        sink.0.windows(6).any(|w| w == b"\x1b[?25l"),
        "cursor hidden while scrolled"
    );
    assert!(!sink.0.windows(6).any(|w| w == b"\x1b[?25h"));
    sink.0.clear();
    term.scroll(super::terminal::ViewScroll::Bottom);
    render(&mut term, &mut sink);
    assert!(
        sink.0.windows(6).any(|w| w == b"\x1b[?25h"),
        "cursor back at bottom"
    );
}

#[test]
fn view_scroll_delta_moves_by_lines() {
    // Positive delta scrolls TOWARD the history top (wheel-up direction).
    let mut term = TerminalCore::new(80, 6);
    scroll_feed(&mut term, 12);
    term.scroll(super::terminal::ViewScroll::Bottom);
    let bottom_first = screen_text(&term.snapshot())[0].clone();
    assert_eq!(bottom_first, "line 7", "live view shows the newest lines");
    term.scroll(super::terminal::ViewScroll::Delta(4));
    let snap = term.snapshot();
    assert_eq!(snap.display_offset, 4);
    assert_eq!(
        screen_text(&snap)[0],
        "line 3",
        "four lines up from the live view"
    );
    // Over-scroll clamps at the top without panicking.
    term.scroll(super::terminal::ViewScroll::Delta(10_000));
    assert_eq!(screen_text(&term.snapshot())[0], "line 0");
}

#[test]
fn settings_parse_mouse_toggle() {
    let settings = SerialSettings::from_lookup(|key| match key {
        "SERIAL_MOUSE" => Some("off".into()),
        _ => None,
    });
    assert!(!settings.mouse);
    assert!(SerialSettings::default().mouse);
    let settings = SerialSettings::from_lookup(|key| match key {
        "SERIAL_MOUSE" => Some("bogus".into()),
        _ => None,
    });
    assert!(settings.mouse, "unknown values keep the default");
}

#[test]
fn typing_while_scrolled_resets_the_view() {
    use super::session::typing_view_reset;
    // Not scrolled: no view event — the TX fast path stays untouched.
    assert_eq!(typing_view_reset(false), None);
    // Scrolled: the view jumps back to the live bottom.
    assert_eq!(
        typing_view_reset(true),
        Some(SessionEvent::Scroll(super::terminal::ViewScroll::Bottom))
    );
}

#[test]
fn display_offset_tracks_the_scrollback_distance() {
    let mut term = TerminalCore::new(80, 6);
    scroll_feed(&mut term, 12);
    assert_eq!(term.display_offset(), 0);
    term.scroll(super::terminal::ViewScroll::Top);
    assert!(term.display_offset() > 0);
    term.scroll(super::terminal::ViewScroll::Bottom);
    assert_eq!(term.display_offset(), 0);
}

#[test]
fn literal_prefix_and_bracketed_paste_use_shared_core() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut input = super::LocalInput::new(super::InputEncoder::new(
        super::BackspaceMode::Delete,
        super::EnterMode::Cr,
    ));
    let prefix = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(input.handle(prefix, false), super::LocalAction::None);
    assert_eq!(
        input.handle(prefix, false),
        super::LocalAction::Bytes(vec![0x14])
    );
    assert_eq!(
        input.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), false),
        super::LocalAction::Bytes(vec![0x1b])
    );
    let mut core = super::TerminalCore::new(80, 24);
    assert_eq!(core.encode_paste("中文"), "中文".as_bytes());
    core.process(b"\x1b[?2004h");
    assert_eq!(
        core.encode_paste("中文"),
        "\x1b[200~中文\x1b[201~".as_bytes()
    );
}

#[cfg(test)]
mod scroll_behavior_probe {
    use crate::serial_term::{ScrollPolicy, TerminalCore, ViewScroll};

    #[test]
    fn probe_scroll_anchor_on_new_output() {
        let mut core = TerminalCore::new(40, 5);
        for i in 0..30 {
            core.process(format!("line{i}\r\n").as_bytes());
        }
        core.scroll(ViewScroll::Delta(5));
        let before = core.display_offset();
        for i in 30..40 {
            core.process(format!("line{i}\r\n").as_bytes());
        }
        let after = core.display_offset();
        println!("offset before={before} after={after}");
        assert!(after >= before, "offset never decreases on output");
        // anchored: offset grows by the number of lines pushed into history
        assert_eq!(
            after,
            before + 10,
            "view stays anchored to the same content"
        );
    }

    #[test]
    fn probe_at_bottom_stays_bottom() {
        let mut core = TerminalCore::new(40, 5);
        for i in 0..30 {
            core.process(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(core.display_offset(), 0);
        core.process(b"more\r\n");
        assert_eq!(core.display_offset(), 0, "at bottom: glued to live output");
    }

    /// Drift mode (follow_output=false): a scrolled view RIDES along
    /// with the output instead of anchoring.
    #[test]
    fn drift_mode_rides_with_output() {
        let mut core = TerminalCore::new(40, 5);
        core.set_scroll_policy(ScrollPolicy {
            follow_output: false,
            jump_on_input: false,
        });
        for i in 0..30 {
            core.process(format!("line{i}\r\n").as_bytes());
        }
        core.scroll(ViewScroll::Delta(5));
        for i in 30..40 {
            core.process(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(
            core.display_offset(),
            5,
            "drift: the offset stays — the view rides with the output"
        );
    }

    /// Typing never moves the view by default; jump_on_input opts in to
    /// the pull-back-to-live behavior.
    #[test]
    fn input_keeps_view_unless_opted_in() {
        let mut core = TerminalCore::new(40, 5);
        for i in 0..30 {
            core.process(format!("line{i}\r\n").as_bytes());
        }
        core.scroll(ViewScroll::Delta(7));
        core.note_input();
        assert_eq!(core.display_offset(), 7, "default: typing keeps the view");
        core.set_scroll_policy(ScrollPolicy {
            follow_output: true,
            jump_on_input: true,
        });
        core.note_input();
        assert_eq!(core.display_offset(), 0, "opt-in: typing jumps to live");
    }
}
