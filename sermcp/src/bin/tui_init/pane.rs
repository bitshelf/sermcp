//! Embedded host for the shared serial terminal core.
use super::*;
use ratatui::widgets::Clear;
use sermcp::serial_term::{
    ColorView, InputEncoder, LocalAction, LocalInput, SerialSettings, TerminalCore, ViewScroll,
};
use std::sync::mpsc;

#[derive(Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Docked,
    Full,
    Hidden,
}

pub struct Pane {
    pub view: View,
    pub focused: bool,
    /// Attached as a PASSIVE monitor (TEST auto-attach): RX streams in,
    /// keystrokes are dropped server-side; clicking the pane upgrades to
    /// the interactive takeover.
    pub monitor: bool,
    pub outer_focus: bool,
    pub core: TerminalCore,
    input: LocalInput,
    newline: sermcp::serial_term::RxNewlineNormalizer,
    pub capture: bool,
    rx: Option<mpsc::Receiver<Result<Vec<u8>, String>>>,
    tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    pub message: String,
    pub target: Option<(String, String)>,
}
impl Default for Pane {
    fn default() -> Self {
        let settings = SerialSettings::default();
        // Scroll behavior comes from ~/.config/dutabo/terminal.jsonc (a
        // malformed file keeps the defaults — the form notes it via the
        // pane message).
        let (prefs, prefs_error) = sermcp::serial_term::prefs::TerminalPrefs::load();
        let mut core = TerminalCore::new(80, 6);
        core.set_scroll_policy(sermcp::serial_term::ScrollPolicy {
            follow_output: prefs.scroll.follow_output,
            jump_on_input: prefs.scroll.jump_on_input,
        });
        Self {
            view: View::Docked,
            focused: false,
            monitor: false,
            outer_focus: true,
            core,
            message: prefs_error
                .unwrap_or_else(|| "Click terminal to attach to the configured MCP".into()),
            capture: false,
            newline: sermcp::serial_term::RxNewlineNormalizer::new(settings.rx_newline),
            input: LocalInput::new(InputEncoder::new(settings.backspace, settings.enter)),
            rx: None,
            tx: None,
            target: None,
        }
    }
}
impl Pane {
    pub fn connected(&self) -> bool {
        self.rx.is_some()
    }

    /// Test hook: pretend a session is live (no worker thread behind it).
    #[cfg(test)]
    pub(super) fn connect_for_test(
        &mut self,
        rx: mpsc::Receiver<Result<Vec<u8>, String>>,
        monitor: bool,
    ) {
        self.rx = Some(rx);
        self.tx = None;
        self.monitor = monitor;
    }

    /// Watch the DUT PASSIVELY while an action (TEST, a reset/power pulse,
    /// a download) drives it: an INTERACTIVE takeover would reject every
    /// serial tool those actions call, so it is released first and the
    /// terminal re-attaches as a monitor — the DUT's output then streams in
    /// live for as long as the pane stays attached.
    pub fn watch(&mut self, ports: Vec<u16>, host: String, port: String) {
        if self.connected() && !self.monitor {
            self.detach();
            self.message = "watching passively — the action owns the serial".into();
        }
        self.attach_with_mode(ports, host, port, true);
    }

    /// Drop the current connection (the worker thread exits when its TX
    /// channel closes).
    pub fn detach(&mut self) {
        self.rx = None;
        self.tx = None;
        self.monitor = false;
    }
    pub fn poll(&mut self) {
        let mut ended = false;
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(Ok(bytes)) => {
                        let mut display = Vec::new();
                        self.newline.normalize(&bytes, &mut display);
                        self.core.process(&display);
                    }
                    Ok(Err(e)) => {
                        self.message = e;
                        ended = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        ended = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
        }
        for reply in self.core.drain_replies() {
            self.send(reply);
        }
        if ended {
            self.rx = None;
            self.tx = None;
        }
    }
    pub(super) fn send(&self, bytes: Vec<u8>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(bytes);
        }
    }
    pub fn key(&mut self, key: KeyEvent) {
        match self.input.handle(key, self.core.app_cursor()) {
            LocalAction::Bytes(bytes) => {
                self.send(bytes);
                self.core.note_input();
            }
            LocalAction::Quit => {
                self.focused = false;
                self.view = View::Docked;
            }
            LocalAction::Scroll(key) => {
                use sermcp::serial_term::input::ScrollKey;
                self.core.scroll(match key {
                    ScrollKey::PageUp => ViewScroll::PageUp,
                    ScrollKey::PageDown => ViewScroll::PageDown,
                    ScrollKey::Top => ViewScroll::Top,
                    ScrollKey::Bottom => ViewScroll::Bottom,
                });
            }
            LocalAction::None => {}
        }
    }
    pub fn attach(&mut self, ports: Vec<u16>, host: String, port: String) {
        self.attach_with_mode(ports, host, port, false);
    }

    /// Attach to the engine's serial feed. `monitor` uses the passive
    /// mode (`?mode=monitor`): the engine keeps serving TEST probes —
    /// an interactive attach would claim the manual session and block
    /// every serial tool for the whole run.
    pub fn attach_with_mode(&mut self, ports: Vec<u16>, host: String, port: String, monitor: bool) {
        if self.connected() {
            return;
        }
        self.monitor = monitor;
        self.target = Some((host.clone(), port.clone()));
        let (rx_tx, rx) = mpsc::channel();
        let (tx, mut tx_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        self.rx = Some(rx);
        self.tx = Some(tx);
        self.message = "Connecting…".into();
        std::thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())?;
                rt.block_on(async {
                    use futures_util::{StreamExt,SinkExt};
                    use tokio_tungstenite::tungstenite::Message;
                    let http=reqwest::Client::builder().timeout(std::time::Duration::from_secs(2)).build().map_err(|e|e.to_string())?;
                    // A cold-started MCP needs longer than a listening socket:
                    // its startup serial probe holds the engine lock, so
                    // /health answers `starting` (serial: null) until the
                    // probe finishes — a 2s window lost that race and told a
                    // user who had just started the server to "run Test
                    // first". Wait it out instead (10s, the same budget the
                    // TEST readiness wait uses).
                    let mut selected=None;
                    // A server that does not advertise the monitor capability
                    // IGNORES `?mode=monitor`: attaching would take the manual
                    // serial session over (blocking the very tools an action
                    // is about to call) — refuse it and say so instead.
                    let mut stale_monitor_server=false;
                    for _ in 0..100 {
                        for &candidate in &ports {
                            if let Ok(response)=http.get(format!("http://127.0.0.1:{candidate}/health")).send().await
                                && let Ok(health)=response.json::<serde_json::Value>().await
                                && health["serial"]["host"].as_str()==Some(host.as_str())
                                && health["serial"]["port"].as_str()==Some(port.as_str()) {
                                    if monitor && health["capabilities"]["monitor"].as_bool()!=Some(true) {
                                        stale_monitor_server=true;
                                        continue;
                                    }
                                    selected=Some(candidate);break;
                                }
                        }
                        if selected.is_some() || stale_monitor_server {break;}
                        if tx_rx.is_closed() {return Ok(());}
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    let port=selected.ok_or(if stale_monitor_server {
                        "this MCP server ignores the passive monitor (?mode=monitor): restart it with the current sermcp build, or the action would take the serial over"
                    } else {
                        "no MCP is serving this DUT's serial endpoint — start one from the form (TEST spawns it, the quick actions and this click do too) or check the sermcp binary and the DUT's serial port"
                    })?;
                    let url=if monitor {format!("ws://127.0.0.1:{port}/serial/ws?mode=monitor")} else {format!("ws://127.0.0.1:{port}/serial/ws")};
                    let (mut ws,_)=tokio::time::timeout(std::time::Duration::from_secs(3),tokio_tungstenite::connect_async(url)).await.map_err(|e|e.to_string())?.map_err(|e|e.to_string())?;
                    loop {tokio::select! {
                        bytes=tx_rx.recv()=>match bytes {Some(bytes)=>ws.send(Message::Binary(bytes.into())).await.map_err(|e|e.to_string())?, None=>{let _=ws.close(None).await;break;}},
                        frame=ws.next()=>match frame {
                            Some(Ok(Message::Binary(bytes)))=>{if rx_tx.send(Ok(bytes.to_vec())).is_err(){break;}},
                            Some(Ok(Message::Close(_)))|None=>break,
                            Some(Err(e))=>return Err(e.to_string()),
                            _=>{},
                        }
                    }}
                    Ok(())
                })
            })();
            let _ = rx_tx.send(Err(result
                .err()
                .unwrap_or_else(|| "Serial detached".into())));
        });
    }
}

pub fn rect(area: Rect, state: &FormState) -> Rect {
    if state.pane.view == View::Full {
        return area;
    }
    let map = form_mode_layout(area, state);
    // A pinned height (border drag) keeps the pane's BOTTOM at the
    // footer and its TOP at the DUT panel's last row + 1 — the layout
    // gave the panel every row the pane does not claim, so the split
    // line trades rows one for one. The pin is capped by the same
    // DUT_MIN_ABS reserve the layout uses (a pin from a LARGER terminal
    // must not swallow the panel after a shrink).
    let height = match (&state.pane.view, state.pane_h_pin) {
        (View::Hidden, _) => 1,
        (_, Some(pin)) => pin
            .min(
                map.footer
                    .y
                    .saturating_sub(map.dut_panel.y)
                    .saturating_sub(DUT_MIN_ABS),
            )
            .max(MIN_PANE_HEIGHT),
        (_, None) => map.footer.y.saturating_sub(map.dut_panel.bottom()),
    };
    Rect::new(
        area.x,
        map.footer.y.saturating_sub(height),
        area.width,
        height,
    )
}
fn color(c: ColorView) -> Color {
    match c {
        ColorView::Palette(n) | ColorView::Indexed(n) => Color::Indexed(n),
        ColorView::Rgb(r, g, b) => Color::Rgb(r, g, b),
        ColorView::Default => Color::Reset,
    }
}

/// Theme inheritance: a cell the DUT left UNSTYLED renders with the form
/// palette — Default FG = the theme text, Default BG = the panel
/// background. Explicit ANSI colors (vim/top/menuconfig) stay verbatim;
/// the embedded terminal never owns a separate fixed gray-on-black theme.
fn fg_color(c: ColorView) -> Color {
    match c {
        ColorView::Default => TEXT,
        other => color(other),
    }
}

fn bg_color(c: ColorView) -> Color {
    match c {
        ColorView::Default => PANEL_BG,
        other => color(other),
    }
}
pub fn render(f: &mut Frame<'_>, state: &mut FormState) {
    let area = rect(f.area(), state);
    if area.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    // The pane is part of the form — ONE theme: the border/titles paint
    // on the panel background (never transparent, which would let the
    // host terminal's own gray show through and visually split the UI),
    // and the accent uses the form palette instead of raw ANSI colors.
    let accent = if state.pane_drag || (state.pane.focused && state.pane.outer_focus) {
        ACCENT
    } else {
        OVERLAY1
    };
    let actions = match state.pane.view {
        View::Docked => "[ Full ] [ Hide ]",
        View::Full => "[ Dock ] [ Hide ]",
        View::Hidden => "[ Show ] [ Full ]",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::new().bg(PANEL_BG))
        .border_style(Style::new().fg(accent).bg(PANEL_BG))
        .title(Span::styled(
            " Serial Terminal ",
            Style::new().fg(accent).bg(PANEL_BG),
        ))
        .title_top(
            Line::from(Span::styled(
                actions,
                Style::new().fg(OVERLAY1).bg(PANEL_BG),
            ))
            .right_aligned(),
        );
    let body = block.inner(area);
    f.render_widget(block, area);
    if state.pane.view == View::Hidden || body.is_empty() {
        return;
    }
    state
        .pane
        .core
        .resize(body.width as usize, body.height as usize);
    let screen = state.pane.core.snapshot();
    for (y, row) in screen.rows.iter().enumerate() {
        for (x, cell) in row.iter().enumerate() {
            if cell.wide_spacer {
                continue;
            }
            let mut symbol = cell.c.to_string();
            symbol.extend(cell.zerowidth.iter());
            let mut style = Style::new().fg(fg_color(cell.fg)).bg(bg_color(cell.bg));
            if cell.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.inverse {
                style = style.add_modifier(Modifier::REVERSED);
            }
            f.buffer_mut()[(body.x + x as u16, body.y + y as u16)]
                .set_symbol(&symbol)
                .set_style(style);
        }
    }
    if !state.pane.connected() && screen.rows.iter().flatten().all(|c| c.c == ' ') {
        f.render_widget(
            Paragraph::new(state.pane.message.clone())
                .style(Style::new().fg(OVERLAY1).bg(PANEL_BG)),
            body,
        );
    }
    if state.pane.input.prefix_active() {
        let popup = Rect::new(body.x, body.y, body.width.min(65), body.height.min(4));
        f.render_widget(Clear, popup);
        f.render_widget(Paragraph::new("q: return to form   Ctrl-T: send literal Ctrl-T\nPgUp/PgDn: scroll history   Home/End: top/bottom").block(Block::default().borders(Borders::ALL).title("Ctrl-T")),popup);
    }
    if state.pane.focused && state.pane.outer_focus && screen.cursor_visible {
        f.set_cursor_position((
            body.x + screen.cursor.0 as u16,
            body.y + screen.cursor.1 as u16,
        ));
    }
}

impl std::fmt::Debug for Pane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pane")
            .field("focused", &self.focused)
            .finish_non_exhaustive()
    }
}
impl<B: Backend, E: EventSource> FormTui<B, E> {
    pub(super) fn pane_mouse(&mut self, m: MouseEvent) -> Result<bool, String> {
        let size = self.terminal.size().map_err(|e| e.to_string())?;
        let area = rect(Rect::new(0, 0, size.width, size.height), &self.state);
        // While the pane-height drag is active the outer handle_mouse
        // arms own the movement — pass everything through.
        if self.state.pane_drag {
            return Ok(false);
        }
        // Drag handle: the Docked pane's TOP border row starts a height
        // drag — same gesture as the host/DUT divider. That row IS the DUT
        // panel's bottom edge (the shared line), so the row ABOVE it is
        // CONTENT and must stay clickable for its own field/action. The
        // title's right-side buttons keep their own click behavior.
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left))
            && self.state.pane.view == View::Docked
            && m.row == area.y
            && m.column < area.right().saturating_sub(17)
        {
            self.state.pane_drag = true;
            return Ok(true);
        }
        if self.state.pane.capture
            && matches!(m.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
        {
            if matches!(m.kind, MouseEventKind::Up(_)) {
                self.state.pane.capture = false;
            }
            return Ok(true);
        }
        if area.contains((m.column, m.row).into()) && matches!(m.kind, MouseEventKind::Down(_)) {
            self.state.pane.capture = true;
        }
        if !area.contains((m.column, m.row).into()) {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.state.pane.focused = false;
            }
            return Ok(false);
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if m.row == area.y => {
                let right = area.right();
                if m.column >= right.saturating_sub(8) {
                    self.state.pane.view = if self.state.pane.view == View::Hidden {
                        View::Full
                    } else {
                        View::Hidden
                    };
                } else if m.column >= right.saturating_sub(17) {
                    self.state.pane.view = match self.state.pane.view {
                        View::Full | View::Hidden => View::Docked,
                        View::Docked => View::Full,
                    };
                }
                if self.state.pane.view == View::Hidden {
                    self.state.pane.focused = false;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // A passive monitor (TEST auto-attach) upgrades to the
                // interactive takeover on click so typing reaches the DUT —
                // EXCEPT while a TEST run is in flight: the takeover would
                // reject every serial probe (reset cycle first) and the run
                // would fail for as long as the terminal stays attached.
                // The click is a no-op then (not even focus: keystrokes
                // would be silently dropped server-side).
                if self.state.pane.connected() && self.state.pane.monitor && self.state.busy() {
                    return Ok(true);
                }
                self.state.pane.focused = true;
                if self.state.pane.connected() && self.state.pane.monitor {
                    self.state.pane.detach();
                }
                let hi = self.state.selected_host;
                if let Some(di) = self.state.selected_dut {
                    let host = self
                        .state
                        .values
                        .get(&FieldKey::HostIp(hi))
                        .cloned()
                        .unwrap_or_default();
                    let port = self
                        .state
                        .values
                        .get(&FieldKey::DutPort(hi, di))
                        .cloned()
                        .unwrap_or_default();
                    let root = Path::new(&self.state.target_label)
                        .parent()
                        .unwrap_or(Path::new("."));
                    let root = root.to_path_buf();
                    let ports = mcp_ports_for_dut(&root, "");
                    if !self.state.pane.connected()
                        && let Some((prepared, _)) = &self.write_ctx
                    {
                        let answers = resolve_answers(
                            &self.state.schema,
                            &self.state.submit_values(),
                            self.state.gate,
                            NamePolicy::ErrorOnCollision,
                        )
                        .map_err(|_| {
                            "Complete the required form fields before attaching serial".to_string()
                        });
                        match answers
                            .and_then(|answers| init::render_test_config(prepared, &answers))
                        {
                            Ok(text) => {
                                let config = self.mcp.write_test_config(&root, &text)?;
                                let name = self
                                    .state
                                    .values
                                    .get(&FieldKey::DutName(hi, di))
                                    .cloned()
                                    .unwrap_or_default();
                                self.mcp.ensure(&root, &name, ports[0], &config);
                            }
                            Err(error) => {
                                self.state.pane.message = error;
                                return Ok(true);
                            }
                        }
                    }
                    self.state.pane.attach(ports, host, port);
                }
            }
            MouseEventKind::ScrollUp => self.state.pane.core.scroll(ViewScroll::Delta(3)),
            MouseEventKind::ScrollDown => self.state.pane.core.scroll(ViewScroll::Delta(-3)),
            _ => {}
        }
        Ok(true)
    }
}

pub fn relay_buttons(state: &FormState, map: &LayoutMap) -> Vec<(FieldKey, Rect, &'static str)> {
    map.fields
        .iter()
        .filter_map(|(key, rect)| {
            let name = match key {
                FieldKey::DutRelayReset(..) => "reset",
                FieldKey::DutRelayDownload(..) => "download",
                FieldKey::DutRelayPower(..) => "power",
                _ => return None,
            };
            let rect = row_shift(state, map, *rect, false)?;
            Some((
                key.clone(),
                Rect::new(rect.right().saturating_sub(3), rect.y, 3, 1),
                name,
            ))
        })
        .collect()
}
/// Effective download-entry capability for the `[>]` action (design §31):
/// the button is usable when EITHER entry exists — the hardware sandwich
/// (reset + download key on an enabled dev_ctrl) OR the provider's
/// software loader_cmd. Never merely `download_channel > 0`.
pub(super) fn download_action_ready(state: &FormState, hi: usize, di: usize) -> bool {
    let provider = download_provider_for(state);
    let Some(provider) = provider else {
        return false;
    };
    if provider
        .default_loader_cmd()
        .is_some_and(|cmd| !cmd.trim().is_empty())
    {
        return true;
    }
    let reset = |key: FieldKey| -> Option<u8> {
        state
            .values
            .get(&key)?
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|ch| *ch > 0)
    };
    !dut_ctrl_disabled(state, hi, di)
        && reset(FieldKey::DutRelayReset(hi, di)).is_some()
        && download_channel(state, hi, di).is_some()
}

/// The DOWNLOAD (FEL/download) channel of one DUT and the engine button name
/// that presses it: the `download` row is the ONE download-family key (the
/// sandwich is reset + this channel).
pub(super) fn download_channel(
    state: &FormState,
    hi: usize,
    di: usize,
) -> Option<(u8, &'static str)> {
    let channel = state
        .values
        .get(&FieldKey::DutRelayDownload(hi, di))?
        .trim()
        .parse::<u8>()
        .ok()
        .filter(|ch| *ch > 0)?;
    Some((channel, "download"))
}

/// The provider resolved from the selected skill path (the ONLY platform
/// context; quick actions use the provider defaults — the full overlay
/// applies in TEST).
pub(super) fn download_provider_for(
    state: &FormState,
) -> Option<&'static dyn crate::tui_init::download::DownloadProvider> {
    if state.skills_path.is_empty() {
        return None;
    }
    download::resolve_provider(None, &state.skills_path)
}

pub fn render_actions(f: &mut Frame<'_>, state: &FormState, map: &LayoutMap) {
    for (key, rect, _) in relay_buttons(state, map) {
        let enabled = match &key {
            // reset / power: PHYSICAL relay actions — enabled iff the
            // channel is configured (design §31).
            FieldKey::DutRelayReset(..) | FieldKey::DutRelayPower(..) => {
                state
                    .values
                    .get(&key)
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(0)
                    > 0
            }
            // download: effective ENTRY capability — hardware sandwich
            // OR the provider's software loader_cmd, never just the
            // channel number.
            FieldKey::DutRelayDownload(hi, di) => download_action_ready(state, *hi, *di),
            _ => false,
        };
        let running = state.probes.get(&key) == Some(&ProbeMark::Pending);
        f.render_widget(
            Paragraph::new(if running { "[*]" } else { "[>]" }).style(Style::new().fg(
                if enabled {
                    Color::Cyan
                } else {
                    Color::DarkGray
                },
            )),
            rect,
        );
    }
}
impl<B: Backend, E: EventSource> FormTui<B, E> {
    pub(super) fn relay_mouse(&mut self, m: MouseEvent) -> Result<bool, String> {
        if self.state.pane.view == View::Full
            || m.kind != MouseEventKind::Down(MouseButton::Left)
            || self.state.busy()
        {
            return Ok(false);
        }
        let size = self.terminal.size().map_err(|e| e.to_string())?;
        let map = form_layout(Rect::new(0, 0, size.width, size.height), &self.state);
        for (key, rect, name) in relay_buttons(&self.state, &map) {
            if !rect.contains((m.column, m.row).into()) {
                continue;
            }
            // Gate per capability: reset/power need their channel; the
            // download action needs an EFFECTIVE entry (hardware sandwich
            // or the provider's software loader_cmd) — a bare channel
            // number is not enough.
            let download_row = matches!(key, FieldKey::DutRelayDownload(..));
            let ready = if download_row {
                let FieldKey::DutRelayDownload(hi, di) = key else {
                    unreachable!("checked above")
                };
                download_action_ready(&self.state, hi, di)
            } else {
                self.state
                    .values
                    .get(&key)
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(0)
                    > 0
            };
            if !ready {
                return Ok(true);
            }
            let root = Path::new(&self.state.target_label)
                .parent()
                .unwrap_or(Path::new("."));
            let ports = mcp_ports_for_dut(root, "");
            let hi = self.state.selected_host;
            let Some(di) = self.state.selected_dut else {
                return Ok(true);
            };
            // The action needs an MCP owning THIS DUT's endpoint. None
            // may exist yet (TEST never ran, or the port was just
            // edited) — spawn one from the CURRENT form values so quick
            // actions work independently of prior runs and edits.
            let prepared = self.write_ctx.as_ref().map(|(p, _)| p.clone());
            if let Some(prepared) = prepared
                && let Ok(answers) = resolve_answers(
                    &self.state.schema,
                    &self.state.submit_values(),
                    self.state.gate,
                    NamePolicy::ErrorOnCollision,
                )
                && let Ok(text) = init::render_test_config(&prepared, &answers)
                && let Ok(config) = self.mcp.write_test_config(root, &text)
            {
                let name = self
                    .state
                    .values
                    .get(&FieldKey::DutName(hi, di))
                    .cloned()
                    .unwrap_or_default();
                self.mcp.ensure(root, &name, ports[0], &config);
            }
            let host = self
                .state
                .values
                .get(&FieldKey::HostIp(hi))
                .cloned()
                .unwrap_or_default();
            let serial_port = self
                .state
                .values
                .get(&FieldKey::DutPort(hi, di))
                .cloned()
                .unwrap_or_default();
            // Pulse the relay with the terminal WATCHING: without the
            // passive feed the pane shows no boot output at all after a
            // reset (the action alone drives the DUT).
            self.state
                .pane
                .watch(ports.clone(), host.clone(), serial_port.clone());
            let (tx, rx) = mpsc::channel();
            self.state.probes.insert(key.clone(), ProbeMark::Pending);
            self.state.relay_action_active = true;
            // The download proof covers baseline + entry + polling —
            // give it the TEST budget; a physical pulse is quick.
            let budget = if download_row { 60 } else { 30 };
            self.state.test_run = Some(TestRun {
                rx,
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(budget),
                summary: false,
            });
            let skills_path = self.state.skills_path.clone();
            let user = self
                .state
                .values
                .get(&FieldKey::HostUser(hi))
                .cloned()
                .unwrap_or_default();
            let pass = self
                .state
                .values
                .get(&FieldKey::HostPass(hi))
                .cloned()
                .unwrap_or_default();
            let dev_ctrl = !dut_ctrl_disabled(&self.state, hi, di);
            let channel = |key: FieldKey| -> Option<u8> {
                self.state
                    .values
                    .get(&key)?
                    .trim()
                    .parse::<u8>()
                    .ok()
                    .filter(|ch| *ch > 0)
            };
            let reset_ch = channel(FieldKey::DutRelayReset(hi, di));
            // ONE download-family channel: the `download` row, else the
            // (the download row is the ONE download-family key).
            let download_ch = download_channel(&self.state, hi, di);
            std::thread::spawn(move || {
                // The MCP owning this DUT's endpoint (serial entry and
                // relay presses go through it).
                let matched = probe_runtime().block_on(async {
                    let http = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(2))
                        .build()
                        .ok()?;
                    // A just-spawned MCP needs a moment to listen — retry
                    // the health match for a few seconds before failing.
                    for _ in 0..50 {
                        for &port in &ports {
                            if let Ok(response) = http
                                .get(format!("http://127.0.0.1:{port}/health"))
                                .send()
                                .await
                                && let Ok(health) = response.json::<serde_json::Value>().await
                                && health["serial"]["host"].as_str() == Some(host.as_str())
                                && health["serial"]["port"].as_str() == Some(serial_port.as_str())
                            {
                                return Some(port);
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    None
                });
                if download_row {
                    // The full download proof through the resolved
                    // provider: enumerate BEFORE → enter (hardware
                    // sandwich or software plan) → poll + identity diff.
                    // Quick actions use the provider DEFAULTS; the
                    // `.target.jsonc` download overlay applies in TEST.
                    let Some(provider) =
                        crate::tui_init::download::resolve_provider(None, &skills_path)
                    else {
                        let _ = tx.send((
                            key,
                            ProbeMark::Skip,
                            Some("no download provider for the selected skill path".into()),
                        ));
                        return;
                    };
                    let Some(port) = matched else {
                        let _ = tx.send((
                            key,
                            ProbeMark::Err,
                            Some("no MCP server owns this DUT's endpoint".into()),
                        ));
                        return;
                    };
                    let download_ch = download_ch.map(|(ch, button)| (ch, button.to_string()));
                    let target = crate::tui_init::ProbeTarget::ListDevices {
                        host,
                        user,
                        pass,
                        provider_id: provider.id().to_string(),
                        list_cmd: provider.default_list_devices_cmd().to_string(),
                        loader_cmd: provider.default_loader_cmd().map(str::to_string),
                        reset_ch,
                        download_ch,
                        dev_ctrl,
                        mcp_ports: vec![port],
                    };
                    let (mark, detail) =
                        crate::tui_init::flash::run_list_devices_probe_quick(target);
                    let _ = tx.send((key, mark, detail));
                    return;
                }
                let (tool, args) = match name {
                    "reset" => ("serial_reset", serde_json::json!({"wait_boot":false})),
                    "power" => ("serial_power_cycle", serde_json::json!({})),
                    _ => unreachable!("the download row returned above"),
                };
                // A JUST-spawned engine may still be connecting when
                // health already answers — retry the physical action a
                // few times before reporting failure.
                let mark = matched
                    .map(|port| {
                        let mut mark = mcp_probe_mark(&[port], tool, args.clone());
                        for _ in 0..3 {
                            if mark != ProbeMark::Err {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(700));
                            mark = mcp_probe_mark(&[port], tool, args.clone());
                        }
                        mark
                    })
                    .unwrap_or(ProbeMark::Err);
                let _ = tx.send((key, mark, None));
            });
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server that does NOT advertise the monitor capability ignores
    /// `?mode=monitor` — attaching would silently take the manual session
    /// over (blocking every tool the action is about to call). The pane
    /// must refuse it and say why.
    #[test]
    fn monitor_attach_refuses_a_server_without_the_capability() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let stop = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < stop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0u8; 4096];
                        let _ = stream.read(&mut request);
                        // No `capabilities` block: an OLD server build.
                        let body = r#"{"serial":{"host":"test-host","port":"2002"}}"#;
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        );
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        let mut pane = Pane::default();
        pane.watch(vec![port], "test-host".into(), "2002".into());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pane.connected() && std::time::Instant::now() < deadline {
            pane.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !pane.connected(),
            "an old server must never be watched (it would take the session over)"
        );
        assert!(
            pane.message.contains("mode=monitor"),
            "the pane explains WHY: {}",
            pane.message
        );
    }

    #[test]
    fn embedded_websocket_keeps_receiving_when_hidden_and_sends_escape() {
        let (port_tx, port_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                use tokio::io::{AsyncReadExt,AsyncWriteExt};
                use futures_util::{StreamExt,SinkExt};
                use tokio_tungstenite::tungstenite::Message;
                let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                port_tx.send(listener.local_addr().unwrap().port()).unwrap();
                let (mut http,_)=listener.accept().await.unwrap();
                let mut request=[0;4096];let read=http.read(&mut request).await.unwrap();
                assert!(read>0,"empty HTTP request");
                let body=r#"{"serial":{"host":"test-host","port":"2002"}}"#;
                http.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                drop(http);
                let (socket,_)=listener.accept().await.unwrap();
                let mut ws=tokio_tungstenite::accept_async(socket).await.unwrap();
                ws.send(Message::Binary(b"ready".to_vec().into())).await.unwrap();
                let key=tokio::time::timeout(std::time::Duration::from_secs(3),ws.next()).await.unwrap().unwrap().unwrap();
                assert_eq!(key,Message::Binary(vec![0x1b].into()));
                ws.send(Message::Binary(b"\r\nhidden-rx".to_vec().into())).await.unwrap();
            });
        });
        let mut pane = Pane::default();
        pane.attach(
            vec![port_rx.recv().unwrap()],
            "test-host".into(),
            "2002".into(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while pane.core.snapshot().rows[0][0].c != 'r' {
            pane.poll();
            assert!(std::time::Instant::now() < deadline, "{}", pane.message);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        pane.view = View::Hidden;
        pane.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        while pane.core.snapshot().rows[1][0].c != 'h' {
            pane.poll();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        pane.view = View::Full;
        assert_eq!(pane.core.snapshot().rows[0][0].c, 'r');
        worker.join().unwrap();
    }
}
