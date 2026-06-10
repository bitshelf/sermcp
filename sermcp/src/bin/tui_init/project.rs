//! Dynamic skill selectors, driven entirely by
//! `~/.config/dutabo/config.jsonc` — the selector COUNT is whatever the
//! catalog shapes it to be (arbitrary depth), the agent selector only
//! appears once a leaf (`src`) is reached, and changing a parent clears
//! the suffix.
use super::*;
use sermcp::agent_deploy::{self, DeployOptions};
use sermcp::project_setup::{self, SavedSelection};
use sermcp::skill_catalog::{Catalog, CatalogStatus};

/// The open skill picker modal: which level (path prefix or agent) and
/// the option list computed from the catalog.
#[derive(Debug, Clone)]
pub struct SkillPicker {
    /// The path prefix the picker edits (`prefix.len()` = the chip index).
    pub prefix: Vec<String>,
    /// None = path level; Some(path) = the agent picker for that leaf.
    pub agent_for: Option<Vec<String>>,
    pub options: Vec<String>,
    pub cursor: usize,
}

impl FormState {
    /// The options for the path picker at `prefix` — catalog children,
    /// empty when the catalog is missing/invalid (the hint line says why).
    pub fn skill_path_options(&self, prefix: &[String]) -> Vec<String> {
        self.skill_catalog.children_at(prefix)
    }

    /// The agent picker options for a leaf path: `All` plus every agent
    /// that owns the leaf.
    pub fn skill_agent_options(&self, path: &[String]) -> Vec<String> {
        std::iter::once("All".to_string())
            .chain(self.skill_catalog.agents_for_leaf(path))
            .collect()
    }

    /// The footer chips: one per selected path segment, then the agent
    /// chip (or the pending `[Select]` / `[Select Agent]` affordance).
    pub fn skill_chips(&self) -> Vec<(String, bool)> {
        let mut chips: Vec<(String, bool)> = self
            .skills_path
            .iter()
            .map(|segment| (segment.clone(), true))
            .collect();
        if self.skill_catalog.is_leaf_path(&self.skills_path) {
            if self.skills_path.is_empty() {
                // Degenerate empty-path leaf cannot happen (path keys
                // name the levels); still render the affordance.
                chips.push(("Select".into(), false));
            } else if self.skills_agents.is_empty() {
                chips.push(("Select Agent".into(), false));
            } else if self.skills_agents.len() == 1 {
                chips.push((self.skills_agents[0].clone(), true));
            } else {
                chips.push(("All".into(), true));
            }
        } else {
            chips.push(("Select".into(), false));
        }
        chips
    }

    pub fn open_skill_picker(&mut self, chip: usize) {
        let prefix: Vec<String> = self.skills_path.iter().take(chip).cloned().collect();
        let (agent_for, options) = if chip > 0
            && chip == self.skills_path.len()
            && self.skill_catalog.is_leaf_path(&self.skills_path)
            && !self.skills_agents.is_empty()
        {
            // The agent chip reopens the agent picker for the leaf.
            (
                Some(self.skills_path.clone()),
                self.skill_agent_options(&self.skills_path),
            )
        } else if prefix.len() == self.skills_path.len()
            && self.skill_catalog.is_leaf_path(&self.skills_path)
            && self.skills_path.len() == prefix.len()
            && !self.skills_path.is_empty()
        {
            (
                Some(self.skills_path.clone()),
                self.skill_agent_options(&self.skills_path),
            )
        } else {
            (None, self.skill_path_options(&prefix))
        };
        if options.is_empty() {
            return;
        }
        self.set_focus(FocusSlot::Button(ButtonId::Skill(
            chip.min(self.skills_path.len().saturating_sub(1)),
        )));
        self.skill_picker = Some(SkillPicker {
            prefix,
            agent_for,
            cursor: 0,
            options,
        });
    }

    /// Choose option `index` in the open picker. A parent change clears
    /// the suffix (path tail + agents) and the agents reset with it.
    pub fn choose_skill(&mut self, index: usize) {
        let Some(picker) = self.skill_picker.take() else {
            return;
        };
        let Some(value) = picker.options.get(index) else {
            return;
        };
        match picker.agent_for {
            Some(path) => {
                self.skills_agents = if value == "All" {
                    self.skill_catalog.agents_for_leaf(&path)
                } else {
                    vec![value.clone()]
                };
                self.skills_path = path;
            }
            None => {
                self.skills_path.truncate(picker.prefix.len());
                self.skills_path.push(value.clone());
                self.skills_agents.clear();
            }
        }
    }

    /// Every key while the picker is open is picker-owned.
    pub fn handle_skill_key(&mut self, key: KeyEvent) -> bool {
        let Some(picker) = self.skill_picker.as_ref() else {
            return false;
        };
        let len = picker.options.len();
        let cursor = picker.cursor;
        match key.code {
            KeyCode::Up => self.skill_picker.as_mut().unwrap().cursor = (cursor + len - 1) % len,
            KeyCode::Down => self.skill_picker.as_mut().unwrap().cursor = (cursor + 1) % len,
            KeyCode::Enter => self.choose_skill(cursor),
            KeyCode::Esc => self.skill_picker = None,
            _ => {}
        }
        true
    }
}

/// The skill chips' rects on the footer line (after the `skills:` label).
pub fn selector_buttons(footer: Rect, chips: usize) -> Vec<(ButtonId, Rect)> {
    let narrow = footer.width < 90;
    // Wide: the chips must end BEFORE [Test] — the button strip
    // ([Test][Save][Save & Exit][Exit]) is 34 cells from the right edge plus
    // the RIGHT_MARGIN gap; reserve its span + the 8-cell "skills:" label +
    // one gap cell so a click on [Test] can never land on a chip.
    let usable = if narrow {
        footer.width.saturating_sub(8)
    } else {
        footer.width.saturating_sub(43 + RIGHT_MARGIN)
    };
    let width = if chips > 0 {
        usable / chips as u16
    } else {
        usable
    };
    (0..chips as u16)
        .map(|i| {
            (
                ButtonId::Skill(i as usize),
                Rect::new(
                    footer.x + 8 + i * width,
                    footer.y + u16::from(narrow),
                    width.saturating_sub(1),
                    1,
                ),
            )
        })
        .collect()
}

pub(super) fn popup(state: &FormState, area: Rect) -> Option<Rect> {
    let picker = state.skill_picker.as_ref()?;
    let chips = state.skill_chips().len();
    let anchor = selector_buttons(form_layout(area, state).footer, chips)
        .into_iter()
        .min_by_key(|(_, rect)| rect.x)
        .map(|(_, rect)| rect)?;
    let longest = picker
        .options
        .iter()
        .map(|o| o.len())
        .max()
        .unwrap_or(6)
        .max(8) as u16;
    let width = (longest + 4).min(area.width);
    let height = (picker.options.len() as u16 + 2).min(area.height.saturating_sub(2));
    Some(Rect::new(
        anchor.x.min(area.right().saturating_sub(width)),
        anchor.y.saturating_sub(height).max(area.y),
        width,
        height,
    ))
}

pub fn render_selector(f: &mut Frame<'_>, state: &FormState, map: &LayoutMap) {
    let area = f.area();
    let chips = state.skill_chips();
    f.render_widget(
        Paragraph::new("skills:"),
        Rect::new(
            map.footer.x,
            map.footer.y + u16::from(map.footer.width < 90),
            7.min(map.footer.width),
            1,
        ),
    );
    for (button, rect) in selector_buttons(map.footer, chips.len()) {
        let ButtonId::Skill(index) = button else {
            continue;
        };
        let Some((label, settled)) = chips.get(index) else {
            continue;
        };
        let style = if state.focus == FocusSlot::Button(button) {
            Style::new().fg(ACCENT).bg(FOCUSED_FIELD_BG)
        } else if *settled {
            Style::new().fg(TEXT).bg(FIELD_BG)
        } else {
            Style::new().fg(OVERLAY1).bg(FIELD_BG)
        };
        f.render_widget(Paragraph::new(format!("[{label}]")).style(style), rect);
    }
    let Some(rect) = popup(state, area) else {
        return;
    };
    let picker = state.skill_picker.as_ref().unwrap();
    let title = if picker.agent_for.is_some() {
        "Agent"
    } else if picker.prefix.is_empty() {
        "Skill"
    } else {
        picker.prefix.last().map(String::as_str).unwrap_or("Skill")
    };
    let items: Vec<_> = picker
        .options
        .iter()
        .map(|option| ListItem::new(option.as_str()))
        .collect();
    f.render_widget(ratatui::widgets::Clear, rect);
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(Style::new().fg(ACCENT).bg(FOCUSED_FIELD_BG));
    let visible = rect.height.saturating_sub(2) as usize;
    let offset = picker.cursor.saturating_sub(visible.saturating_sub(1));
    let mut selected = ListState::default()
        .with_offset(offset)
        .with_selected(Some(picker.cursor));
    f.render_stateful_widget(list, rect, &mut selected);
}

impl<B: Backend, E: EventSource> FormTui<B, E> {
    pub fn skill_mouse(&mut self, m: MouseEvent) -> Result<(), String> {
        let size = self.terminal.size().map_err(|e| e.to_string())?;
        let area = Rect::new(0, 0, size.width, size.height);
        if let Some(rect) = popup(&self.state, area) {
            let inside = inset(rect, 1);
            if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                if inside.contains((m.column, m.row).into()) {
                    let cursor = self.state.skill_picker.as_ref().unwrap().cursor;
                    let offset = cursor.saturating_sub((inside.height as usize).saturating_sub(1));
                    self.state
                        .choose_skill(offset + (m.row - inside.y) as usize);
                } else {
                    self.state.skill_picker = None;
                }
            }
        }
        Ok(())
    }

    pub fn finish_project(
        &mut self,
        opts: &InitOptions,
        prepared: &PreparedInit,
        answers: &WizardAnswers,
    ) -> Result<WizardOutcome, String> {
        let root = prepared
            .target_path
            .parent()
            .ok_or("missing SDK directory")?
            .to_path_buf();
        let selection = SavedSelection {
            path: self.state.skills_path.clone(),
            agents: self.state.skills_agents.clone(),
        };
        if !selection.agents.is_empty() {
            if let Some(error) = &self.state.setup_error {
                return Err(error.clone());
            }
            self.state.note = Some((
                NoteKind::Info,
                "Fetching skills and installing project integrations...".into(),
            ));
            self.draw()?;
        }
        // Skill deploy FIRST (the leaf's git source — e.g.
        // https://gitcode.com/JZ_Loh/skills — is cloned into the shared
        // cache and its post hook lays the skills out for the agent).
        let mut deployed = Vec::new();
        for agent in &selection.agents {
            let Some(leaf) = self
                .state
                .skill_catalog
                .resolve_leaf(&selection.path, agent)
            else {
                return Err(format!(
                    "skill {}/{} is not a leaf for agent {agent:?}",
                    selection.path.join("/"),
                    agent
                ));
            };
            let deploy_options = DeployOptions {
                source: leaf.src.clone(),
                r#ref: leaf.git_ref.clone(),
                post_hook: leaf.post_hook.clone(),
                dst: leaf.deploy.dst.clone(),
                mode: leaf.deploy.mode.clone(),
                agent: agent.clone(),
                path: leaf.path.clone(),
                project_dir: root.clone(),
                src_root: agent_deploy::default_src_root(),
                offline: false,
                force: false,
                timeout: agent_deploy::DEPLOY_TIMEOUT,
            };
            let report = agent_deploy::deploy(&deploy_options)
                .map_err(|e| format!("config saved; skill deploy for {agent} failed: {e}"))?;
            deployed.push(format!(
                "{agent}: {} ({})",
                selection.path.join("/"),
                report
                    .stdout_tail
                    .last()
                    .cloned()
                    .unwrap_or_else(|| report.deployed_dir.display().to_string())
            ));
        }
        let mut outcome = finish_init(self, opts, prepared, answers)?;
        if !selection.agents.is_empty() {
            self.state.note = Some((
                NoteKind::Info,
                "Installing sermcp hooks and integrations...".into(),
            ));
            self.draw()?;
            if let Some(plan) = project_setup::plan_integrations(&root, &selection)
                .map_err(|e| format!("config saved; project integration incomplete: {e}"))?
            {
                plan.apply()
                    .map_err(|e| format!("config saved; project installation incomplete: {e}"))?;
                project_setup::install_dependencies_for(&root, &selection.agents)
                    .map_err(|e| format!("config and assets saved; {e}"))?;
            }
            outcome.changes.push(format!(
                "skills {} deployed for {} in {}",
                selection.path.join("/"),
                selection.agents.join(", "),
                root.display()
            ));
            for line in deployed {
                outcome.changes.push(format!("skill deploy: {line}"));
            }
        }
        Ok(outcome)
    }
}

/// Load the catalog + the project's saved selection into form-ready
/// state. Stale saved selections (renamed/removed catalog keys) shrink
/// to their longest valid prefix.
pub fn load_skill_state(root: &Path) -> (Catalog, Option<String>, Vec<String>, Vec<String>) {
    let (catalog, error) = match sermcp::skill_catalog::load() {
        CatalogStatus::Loaded(_, catalog) => (catalog, None),
        CatalogStatus::Missing(path) => (
            Catalog::empty(),
            Some(format!(
                "no skill catalog at {} — `skills:` selectors are disabled until it exists",
                path.display()
            )),
        ),
        CatalogStatus::Invalid(path, reason) => (
            Catalog::empty(),
            Some(format!("skill catalog {}: {reason}", path.display())),
        ),
    };
    let mut path = Vec::new();
    let mut agents = Vec::new();
    if let Some(saved) = project_setup::saved_selection(root) {
        for segment in &saved.path {
            let mut candidate = path.clone();
            candidate.push(segment.clone());
            if catalog.children_at(&path).contains(segment) || catalog.is_leaf_path(&candidate) {
                path = candidate;
            } else {
                break;
            }
        }
        if catalog.is_leaf_path(&path) {
            let valid = catalog.agents_for_leaf(&path);
            agents = saved
                .agents
                .into_iter()
                .filter(|agent| valid.contains(agent))
                .collect();
        }
    }
    (catalog, error, path, agents)
}
