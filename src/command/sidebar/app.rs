//! Application state for the sidebar TUI.

use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent_display::{extract_project_name, extract_worktree_name, resolve_labels};
use crate::cmd::Cmd;
use crate::config::{
    AgentIcons, Config, SidebarPosition, SidebarTreeGroupBy, SidebarWidth, StatusIcons,
};
use crate::git::GitStatus;
use crate::github::PrSummary;
use ratatui::style::Color;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::str::FromStr;
use tracing::warn;

use crate::multiplexer::{AgentPane, Multiplexer};

use crate::ui::theme::ThemePalette;

use super::snapshot::SidebarSnapshot;
use super::template::parser::{ParseError, Token, parse_line};

/// Sidebar layout mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarLayoutMode {
    Compact,
    #[default]
    Tiles,
}

impl SidebarLayoutMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Tiles => "tiles",
        }
    }
}

/// Whether the sidebar auto-follows its host window or the user is navigating manually.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionMode {
    FollowHost,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DragState {
    source_idx: usize,
    current_idx: usize,
}

/// Runtime form of `sidebar.agent_icons`: icon strings and parsed colors.
///
/// Built once when config loads or reloads. Color strings are parsed eagerly
/// so the render path does no string parsing per row per frame, and invalid
/// colors warn once at load time instead of being silently ignored every
/// render.
///
/// The `colors` map distinguishes:
///   - `Some(Some(c))`: user override color.
///   - `Some(None)`: explicit opt-out (`color: ''`); skip the
///     `AgentKind::default_color` fallback.
///   - kind missing from map: no override, fall through to default.
#[derive(Debug, Default, Clone)]
pub struct ResolvedAgentIcons {
    pub icons: BTreeMap<String, String>,
    pub colors: BTreeMap<String, Option<Color>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitBox {
    pub idx: usize,
    pub x_start: u16,
    pub x_end: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarDisplayRow {
    Group {
        key: String,
        label: String,
        agent_count: usize,
        sleeping_count: usize,
        expanded: bool,
    },
    Agent {
        agent_idx: usize,
        depth: usize,
    },
}

impl ResolvedAgentIcons {
    pub fn from_config(map: Option<&AgentIcons>) -> Self {
        let mut icons = BTreeMap::new();
        let mut colors = BTreeMap::new();
        let Some(map) = map else {
            return Self { icons, colors };
        };
        for (kind, spec) in map {
            if let Some(i) = spec.icon() {
                icons.insert(kind.clone(), i.to_string());
            }
            if let Some(raw) = spec.color() {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    colors.insert(kind.clone(), None);
                } else {
                    match Color::from_str(trimmed) {
                        Ok(c) => {
                            colors.insert(kind.clone(), Some(c));
                        }
                        Err(_) => warn!(
                            "sidebar.agent_icons.{kind}.color = {raw:?}: invalid color, ignoring"
                        ),
                    }
                }
            }
        }
        Self { icons, colors }
    }
}

const DEFAULT_COMPACT_TEMPLATE: &str = "{status_icon} {primary} {pane_suffix} {fill} {elapsed}";
const DEFAULT_TILE_TEMPLATES: &[&str] = &[
    "{primary} {pane_suffix} {fill} {elapsed}",
    "{secondary} {fill} {git_stats}",
    "{pane_title}",
];
const DEFAULT_HORIZONTAL_TEMPLATES: &[&str] = &[
    "{status_icon} {primary} {pane_suffix} {fill} {elapsed}",
    "{secondary} {fill} {git_stats}",
    "{pane_title}",
];

/// Parsed templates for one sidebar instance.
#[derive(Debug, Clone)]
pub struct ParsedTemplates {
    pub compact: Vec<Token>,
    pub tiles: Vec<Vec<Token>>,
    pub horizontal: Vec<Vec<Token>>,
}

/// Latest sidebar template parsing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    pub location: String,
    pub message: String,
}

impl TemplateError {
    fn new(location: impl Into<String>, error: &ParseError) -> Self {
        Self {
            location: location.into(),
            message: error.to_string(),
        }
    }

    pub fn display_message(&self) -> String {
        format!("template error: {} in {}", self.message, self.location)
    }
}

/// Lightweight sidebar app state. No preview, git, PR, diff, or input mode.
pub struct SidebarApp {
    pub mux: Arc<dyn Multiplexer>,
    pub agents: Vec<AgentPane>,
    pub has_loaded_snapshot: bool,
    pub list_state: ListState,
    pub should_quit: bool,
    pub quit_reason: Option<String>,
    pub palette: ThemePalette,
    pub status_icons: StatusIcons,
    pub spinner_frame: u8,
    pub stale_threshold_secs: u64,
    pub position: SidebarPosition,
    pub layout_mode: SidebarLayoutMode,
    /// Area where the list was last rendered (for mouse hit testing)
    pub list_area: Rect,
    /// Window prefix from config
    window_prefix: String,
    /// The sidebar's own host session (immutable, detected once at startup via TMUX_PANE)
    host_session: Option<String>,
    /// Stable tmux window ID (e.g., @42) for active-window detection
    host_window_id: Option<String>,
    /// Index of the agent in the sidebar's host window (updated each snapshot)
    pub host_agent_idx: Option<usize>,
    /// Whether this sidebar's host window is the active window in the session
    host_window_active: bool,
    selection_mode: SelectionMode,
    drag_state: Option<DragState>,
    /// Git status per worktree path (received from daemon snapshots).
    pub git_statuses: HashMap<PathBuf, GitStatus>,
    /// PR summary per worktree path (received from daemon snapshots).
    pub pr_statuses: HashMap<PathBuf, PrSummary>,
    /// Pane IDs of agents detected as interrupted by the daemon.
    pub interrupted_pane_ids: std::collections::HashSet<String>,
    /// Pane IDs of agents manually marked as sleeping by the user.
    pub sleeping_pane_ids: std::collections::HashSet<String>,
    /// Parsed sidebar templates.
    pub templates: ParsedTemplates,
    /// Most recent template parse failure, shown in the sidebar until fixed.
    pub template_error: Option<TemplateError>,
    /// Per-agent icon and color overrides, parsed once at config load.
    pub agent_icons: ResolvedAgentIcons,
    /// Cached tile heights for hit testing (updated each render).
    pub tile_heights: Vec<usize>,
    /// Cached horizontal chip hitboxes for top bar mouse hit testing.
    pub horizontal_hitboxes: Vec<HitBox>,
    /// First agent index rendered in the horizontal top bar.
    pub first_visible_agent_idx: usize,
    /// Maximum width of each horizontal item in columns.
    pub horizontal_item_width: usize,
    /// Whether left sidebar rows should be grouped under tree headers.
    pub tree_enabled: bool,
    /// Field used to group tree rows.
    pub tree_group_by: SidebarTreeGroupBy,
    /// Group keys currently collapsed by the user.
    pub collapsed_tree_groups: HashSet<String>,
    /// Render-time display rows for the left sidebar.
    pub display_rows: Vec<SidebarDisplayRow>,
    /// Last `config_version` from the daemon snapshot. Increments trigger a
    /// client-side config reload.
    pub last_config_version: u64,
    /// String form of the compact template currently parsed into `templates`.
    /// Tracked so we don't re-parse on every snapshot, and so we don't retry
    /// an unchanged broken value after logging once.
    pub current_compact_str: String,
    /// String forms of tile templates currently parsed into `templates`.
    pub current_tile_strs: Vec<String>,
    /// String forms of horizontal bar templates currently parsed into `templates`.
    pub current_horizontal_strs: Vec<String>,
    /// Live sidebar width as last loaded from config. Stored for parity with
    /// other live keys; tmux pane resize is not driven from here.
    pub current_width: Option<SidebarWidth>,
    /// Last known window width (for detecting manual pane resizes).
    last_window_width: Option<u16>,
    /// Last known window height (for detecting manual top bar resizes).
    last_window_height: Option<u16>,
    /// Pending resize columns to process after debounce.
    pending_resize_cols: Option<u16>,
    /// Pending resize rows to process after debounce.
    pending_resize_rows: Option<u16>,
    /// Deadline after which pending resize should be processed.
    pub(super) resize_deadline: Option<Instant>,
}

impl SidebarApp {
    #[cfg(test)]
    pub(crate) fn test_with_template_error(template_error: TemplateError) -> Self {
        Self {
            mux: Arc::new(crate::multiplexer::TmuxBackend::new()),
            agents: Vec::new(),
            has_loaded_snapshot: true,
            list_state: ListState::default(),
            should_quit: false,
            quit_reason: None,
            palette: ThemePalette::from_config(
                &Config::default().theme,
                crate::config::ThemeMode::Dark,
            ),
            status_icons: StatusIcons::default(),
            spinner_frame: 0,
            stale_threshold_secs: 3600,
            position: SidebarPosition::Left,
            layout_mode: SidebarLayoutMode::Compact,
            list_area: Rect::default(),
            window_prefix: "wm-".to_string(),
            host_session: None,
            host_window_id: None,
            host_agent_idx: None,
            host_window_active: true,
            selection_mode: SelectionMode::FollowHost,
            drag_state: None,
            git_statuses: HashMap::new(),
            pr_statuses: HashMap::new(),
            interrupted_pane_ids: std::collections::HashSet::new(),
            sleeping_pane_ids: std::collections::HashSet::new(),
            templates: ParsedTemplates {
                compact: parse_line("{primary}").unwrap(),
                tiles: vec![parse_line("{primary}").unwrap()],
                horizontal: vec![parse_line("{primary}").unwrap()],
            },
            template_error: Some(template_error),
            agent_icons: ResolvedAgentIcons::default(),
            tile_heights: Vec::new(),
            horizontal_hitboxes: Vec::new(),
            first_visible_agent_idx: 0,
            horizontal_item_width: 24,
            tree_enabled: false,
            tree_group_by: SidebarTreeGroupBy::Project,
            collapsed_tree_groups: HashSet::new(),
            display_rows: Vec::new(),
            last_config_version: 0,
            current_compact_str: "{primary}".to_string(),
            current_tile_strs: vec!["{primary}".to_string()],
            current_horizontal_strs: vec!["{primary}".to_string()],
            current_width: None,
            last_window_width: None,
            last_window_height: None,
            pending_resize_cols: None,
            pending_resize_rows: None,
            resize_deadline: None,
        }
    }

    /// Create a new sidebar client. Does config + host detection only, no tmux polling.
    pub fn new_client(mux: Arc<dyn Multiplexer>) -> Result<Self> {
        let config = Config::load(None)?;

        let theme_mode = config
            .theme
            .mode
            .unwrap_or_else(|| match terminal_light::luma() {
                Ok(luma) if luma > 0.6 => crate::config::ThemeMode::Light,
                _ => crate::config::ThemeMode::Dark,
            });
        let palette = ThemePalette::from_config(&config.theme, theme_mode);
        let window_prefix = config.window_prefix().to_string();
        let status_icons = config.status_icons.clone();

        let (host_session, host_window_id) = detect_host_window();

        let (templates, template_error) = parse_templates(&config);
        let (current_compact_str, current_tile_strs, current_horizontal_strs) =
            resolved_template_strings(&config);
        let agent_icons = ResolvedAgentIcons::from_config(config.sidebar.agent_icons.as_ref());
        let current_width = config.sidebar.width.clone();
        let horizontal_item_width = config.sidebar.horizontal.item_width();
        let tree_enabled = config.sidebar.tree.enabled.unwrap_or(false);
        let tree_group_by = config.sidebar.tree.group_by.unwrap_or_default();
        let position = super::read_sidebar_position(&config);

        // Seed last_window_width so the first resize event after startup grace
        // can be compared against a baseline (fixes first-resize-dropped bug).
        let initial_window_width = query_window_width_for_pane();
        let initial_window_height = query_window_height_for_pane();

        Ok(Self {
            mux,
            agents: Vec::new(),
            has_loaded_snapshot: false,
            list_state: ListState::default(),
            should_quit: false,
            quit_reason: None,
            palette,
            status_icons,
            spinner_frame: 0,
            stale_threshold_secs: 60 * 60, // 60 minutes
            position,
            layout_mode: SidebarLayoutMode::default(),
            list_area: Rect::default(),
            window_prefix,
            host_session,
            host_window_id,
            host_agent_idx: None,
            host_window_active: true,
            selection_mode: SelectionMode::FollowHost,
            drag_state: None,
            git_statuses: HashMap::new(),
            pr_statuses: HashMap::new(),
            interrupted_pane_ids: std::collections::HashSet::new(),
            sleeping_pane_ids: std::collections::HashSet::new(),
            templates,
            template_error,
            agent_icons,
            tile_heights: Vec::new(),
            horizontal_hitboxes: Vec::new(),
            first_visible_agent_idx: 0,
            horizontal_item_width,
            tree_enabled,
            tree_group_by,
            collapsed_tree_groups: HashSet::new(),
            display_rows: Vec::new(),
            last_config_version: 0,
            current_compact_str,
            current_tile_strs,
            current_horizontal_strs,
            current_width,
            last_window_width: initial_window_width,
            last_window_height: initial_window_height,
            pending_resize_cols: None,
            pending_resize_rows: None,
            resize_deadline: None,
        })
    }

    /// Apply a snapshot received from the daemon.
    pub fn apply_snapshot(&mut self, snapshot: SidebarSnapshot) {
        self.has_loaded_snapshot = true;

        // Compute host agent index from the new snapshot first so that a
        // config_version bump anchors the reload to the *current* host path,
        // not whatever was selected from the previous snapshot.
        self.host_agent_idx = self.host_window_id.as_ref().and_then(|wid| {
            let mut first_match = None;
            for (i, agent) in snapshot.agents.iter().enumerate() {
                if agent.window_id != *wid {
                    continue;
                }
                if snapshot.active_pane_ids.contains(&agent.pane_id) {
                    return Some(i);
                }
                first_match.get_or_insert(i);
            }
            first_match
        });

        if snapshot.config_version != self.last_config_version {
            self.last_config_version = snapshot.config_version;
            self.reload_config_from_disk(&snapshot);
        }

        self.position = snapshot.position;
        self.layout_mode = snapshot.layout_mode;
        self.git_statuses = snapshot.git_statuses;
        self.pr_statuses = snapshot.pr_statuses;
        self.interrupted_pane_ids = snapshot.interrupted_pane_ids;
        self.sleeping_pane_ids = snapshot.sleeping_pane_ids;

        // Check if host window is active
        let was_active = self.host_window_active;
        self.host_window_active =
            if let (Some(session), Some(window_id)) = (&self.host_session, &self.host_window_id) {
                snapshot
                    .active_windows
                    .contains(&(session.clone(), window_id.clone()))
            } else {
                true
            };

        // Re-arm FollowHost when window becomes active
        if !was_active && self.host_window_active {
            self.selection_mode = SelectionMode::FollowHost;
        }

        // Preserve selection by pane_id
        let selected_pane = self
            .selected_agent_idx()
            .and_then(|i| self.agents.get(i))
            .map(|a| a.pane_id.clone());

        self.agents = snapshot.agents;
        self.rebuild_display_rows();

        // Restore selection
        if let Some(ref pane_id) = selected_pane {
            if let Some(idx) = self.agents.iter().position(|a| &a.pane_id == pane_id) {
                self.list_state
                    .select(Some(self.display_row_for_agent_idx(idx).unwrap_or(idx)));
            } else if !self.agents.is_empty() {
                let clamped = self
                    .list_state
                    .selected()
                    .unwrap_or(0)
                    .min(self.visible_row_count().saturating_sub(1));
                self.list_state.select(Some(clamped));
            } else {
                self.list_state.select(None);
            }
        } else if !self.agents.is_empty() && self.list_state.selected().is_none() {
            self.list_state.select(Some(0));
        }

        self.sync_selection();
    }

    /// Select the agent belonging to this sidebar's host window (only in FollowHost mode).
    pub fn sync_selection(&mut self) {
        if self.selection_mode != SelectionMode::FollowHost {
            return;
        }
        if let Some(idx) = self.host_agent_idx {
            self.list_state
                .select(Some(self.display_row_for_agent_idx(idx).unwrap_or(idx)));
        }
    }

    /// Re-read the merged config from disk and apply live-reloadable fields:
    /// templates, agent icons, and width. Templates are anchored at the host
    /// agent's worktree path so per-project `.workmux.yaml` overrides are
    /// honored. On any parse error, keep the previously valid templates.
    fn reload_config_from_disk(&mut self, snapshot: &SidebarSnapshot) {
        let host_path = self
            .host_agent_idx
            .and_then(|i| snapshot.agents.get(i))
            .map(|a| a.path.clone());

        let cfg_result = match host_path.as_ref() {
            Some(p) => Config::load_with_location_from(p, None).map(|(c, _)| c),
            None => Config::load(None),
        };
        let cfg = match cfg_result {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("client config reload failed: {}", e);
                return;
            }
        };

        let (new_compact, new_tiles, new_horizontal) = resolved_template_strings(&cfg);
        if new_compact != self.current_compact_str
            || new_tiles != self.current_tile_strs
            || new_horizontal != self.current_horizontal_strs
        {
            self.template_error = try_reparse_templates(
                &mut self.templates,
                &mut self.current_compact_str,
                &mut self.current_tile_strs,
                &mut self.current_horizontal_strs,
                &new_compact,
                &new_tiles,
                &new_horizontal,
            );
        }

        self.agent_icons = ResolvedAgentIcons::from_config(cfg.sidebar.agent_icons.as_ref());
        self.horizontal_item_width = cfg.sidebar.horizontal.item_width();
        self.tree_enabled = cfg.sidebar.tree.enabled.unwrap_or(false);
        self.tree_group_by = cfg.sidebar.tree.group_by.unwrap_or_default();
        self.current_width = cfg.sidebar.width.clone();
        self.rebuild_display_rows();
    }

    pub fn host_window_id(&self) -> Option<&str> {
        self.host_window_id.as_deref()
    }

    pub fn host_window_active(&self) -> bool {
        self.host_window_active
    }

    pub fn tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1) % 10;
    }

    pub fn next(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        if self.agents.is_empty() {
            return;
        }
        if self.tree_enabled && !self.display_rows.is_empty() {
            let i = self.list_state.selected().unwrap_or(0);
            let next = if i >= self.display_rows.len() - 1 {
                0
            } else {
                i + 1
            };
            self.list_state.select(Some(next));
        } else {
            let i = self.list_state.selected().unwrap_or(0);
            let next = if i >= self.agents.len() - 1 { 0 } else { i + 1 };
            self.list_state.select(Some(next));
        }
    }

    pub fn previous(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        if self.agents.is_empty() {
            return;
        }
        if self.tree_enabled && !self.display_rows.is_empty() {
            let i = self.list_state.selected().unwrap_or(0);
            let prev = if i == 0 {
                self.display_rows.len() - 1
            } else {
                i - 1
            };
            self.list_state.select(Some(prev));
        } else {
            let i = self.list_state.selected().unwrap_or(0);
            let prev = if i == 0 { self.agents.len() - 1 } else { i - 1 };
            self.list_state.select(Some(prev));
        }
    }

    pub fn select_first(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        if self.tree_enabled && !self.display_rows.is_empty() {
            self.list_state.select(Some(0));
        } else if !self.agents.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    pub fn select_last(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        if self.tree_enabled && !self.display_rows.is_empty() {
            self.list_state.select(Some(self.display_rows.len() - 1));
        } else if !self.agents.is_empty() {
            self.list_state.select(Some(self.agents.len() - 1));
        }
    }

    pub fn select_index(&mut self, idx: usize) {
        self.selection_mode = SelectionMode::Manual;
        if self.tree_enabled && !self.display_rows.is_empty() {
            self.list_state
                .select(Some(idx.min(self.display_rows.len() - 1)));
        } else if !self.agents.is_empty() {
            self.list_state.select(Some(idx.min(self.agents.len() - 1)));
        }
    }

    pub fn scroll_up(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        if let Some(i) = self.list_state.selected() {
            self.list_state.select(Some(i.saturating_sub(1)));
        }
    }

    pub fn scroll_down(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        if let Some(i) = self.list_state.selected() {
            let last = self.agents.len().saturating_sub(1);
            let last = if self.tree_enabled && !self.display_rows.is_empty() {
                self.display_rows.len().saturating_sub(1)
            } else {
                last
            };
            self.list_state.select(Some((i + 1).min(last)));
        }
    }

    pub fn hit_test(&self, column: u16, row: u16) -> Option<usize> {
        if self.agents.is_empty() {
            return None;
        }
        let area = self.list_area;
        if row < area.y || row >= area.y + area.height {
            return None;
        }

        if self.position == SidebarPosition::Top {
            return self
                .horizontal_hitboxes
                .iter()
                .find(|hit| column >= hit.x_start && column < hit.x_end)
                .map(|hit| hit.idx);
        }

        let relative_row = (row - area.y) as usize;
        let offset = self.list_state.offset();

        if self.tree_enabled {
            if self.layout_mode == SidebarLayoutMode::Compact {
                let idx = offset + relative_row;
                return (idx < self.display_rows.len()).then_some(idx);
            }

            let mut y = 0;
            for idx in offset..self.display_rows.len() {
                let h = self.tile_heights.get(idx).copied().unwrap_or(1);
                if relative_row < y + h {
                    return Some(idx);
                }
                y += h;
            }
            return None;
        }

        match self.layout_mode {
            SidebarLayoutMode::Compact => {
                let idx = offset + relative_row;
                (idx < self.agents.len()).then_some(idx)
            }
            SidebarLayoutMode::Tiles => {
                let mut y = 0;
                for idx in offset..self.agents.len() {
                    let h = self.tile_item_height(idx);
                    if relative_row < y + h {
                        return Some(idx);
                    }
                    y += h;
                }
                None
            }
        }
    }

    pub fn start_drag(&mut self, idx: usize) {
        if self.tree_enabled {
            self.selection_mode = SelectionMode::Manual;
            let Some(SidebarDisplayRow::Agent { .. }) = self.display_rows.get(idx) else {
                return;
            };
            self.drag_state = Some(DragState {
                source_idx: idx,
                current_idx: idx,
            });
            self.list_state.select(Some(idx));
            return;
        }
        if idx >= self.agents.len() {
            return;
        }
        self.selection_mode = SelectionMode::Manual;
        self.drag_state = Some(DragState {
            source_idx: idx,
            current_idx: idx,
        });
        self.list_state.select(Some(idx));
    }

    pub fn update_drag(&mut self, idx: usize) {
        if self.tree_enabled {
            self.update_tree_drag(idx);
            return;
        }
        let Some(mut drag_state) = self.drag_state else {
            return;
        };
        if idx >= self.agents.len() || idx == drag_state.current_idx {
            return;
        }
        let agent = self.agents.remove(drag_state.current_idx);
        self.agents.insert(idx, agent);
        drag_state.current_idx = idx;
        self.drag_state = Some(drag_state);
        self.list_state.select(Some(idx));
    }

    fn update_tree_drag(&mut self, idx: usize) {
        let Some(mut drag_state) = self.drag_state else {
            return;
        };
        if idx == drag_state.current_idx || idx >= self.display_rows.len() {
            return;
        }
        let Some((source_agent_idx, source_group_key)) =
            self.tree_agent_and_group(drag_state.current_idx)
        else {
            return;
        };
        let Some((target_agent_idx, target_group_key)) = self.tree_agent_and_group(idx) else {
            return;
        };
        if source_group_key != target_group_key {
            return;
        }

        let dragged_pane_id = self.agents[source_agent_idx].pane_id.clone();
        let Some(new_order) = self.tree_reordered_agent_indices(
            &source_group_key,
            source_agent_idx,
            target_agent_idx,
        ) else {
            return;
        };
        let old_agents = self.agents.clone();
        self.agents = new_order
            .into_iter()
            .filter_map(|agent_idx| old_agents.get(agent_idx).cloned())
            .collect();
        self.rebuild_display_rows();
        let new_agent_idx = self
            .agents
            .iter()
            .position(|agent| agent.pane_id == dragged_pane_id)
            .unwrap_or(source_agent_idx);
        drag_state.current_idx = self.display_row_for_agent_idx(new_agent_idx).unwrap_or(idx);
        self.drag_state = Some(drag_state);
        self.list_state.select(Some(drag_state.current_idx));
    }

    fn tree_reordered_agent_indices(
        &self,
        group_key: &str,
        source_agent_idx: usize,
        target_agent_idx: usize,
    ) -> Option<Vec<usize>> {
        let mut group_slots = Vec::new();
        for (agent_idx, agent) in self.agents.iter().enumerate() {
            let (key, _) = tree_group_for_agent(agent, self.tree_group_by, &self.window_prefix);
            if key == group_key {
                group_slots.push(agent_idx);
            }
        }

        let source_pos = group_slots
            .iter()
            .position(|idx| *idx == source_agent_idx)?;
        let target_pos = group_slots
            .iter()
            .position(|idx| *idx == target_agent_idx)?;
        let moved = group_slots.remove(source_pos);
        group_slots.insert(target_pos, moved);

        let mut next_order: Vec<usize> = (0..self.agents.len()).collect();
        for (slot, agent_idx) in self
            .agents
            .iter()
            .enumerate()
            .filter_map(|(agent_idx, agent)| {
                let (key, _) = tree_group_for_agent(agent, self.tree_group_by, &self.window_prefix);
                (key == group_key).then_some(agent_idx)
            })
            .zip(group_slots)
        {
            next_order[slot] = agent_idx;
        }

        Some(next_order)
    }

    fn tree_agent_and_group(&self, row_idx: usize) -> Option<(usize, String)> {
        let SidebarDisplayRow::Agent { agent_idx, .. } = self.display_rows.get(row_idx)? else {
            return None;
        };
        let group_key = self.display_rows[..row_idx].iter().rev().find_map(|row| {
            if let SidebarDisplayRow::Group { key, .. } = row {
                Some(key.clone())
            } else {
                None
            }
        })?;
        Some((*agent_idx, group_key))
    }

    pub fn cancel_drag(&mut self) {
        self.drag_state = None;
    }

    pub fn finish_drag(&mut self) -> bool {
        let Some(drag_state) = self.drag_state.take() else {
            return false;
        };
        if drag_state.source_idx == drag_state.current_idx {
            return false;
        }
        self.persist_sidebar_order();
        super::daemon_ctrl::signal_daemon();
        true
    }

    fn persist_sidebar_order(&self) {
        if let Ok(store) = crate::state::StateStore::new()
            && let Ok(mut settings) = store.load_settings()
        {
            settings.sidebar_order = self
                .agents
                .iter()
                .map(|agent| agent.pane_id.clone())
                .collect();
            let _ = store.save_settings(&settings);
        }
    }

    pub fn ensure_selected_visible(&mut self, visible_count: usize) {
        let Some(selected) = self.list_state.selected() else {
            return;
        };
        if selected < self.first_visible_agent_idx {
            self.first_visible_agent_idx = selected;
        } else if visible_count > 0 && selected >= self.first_visible_agent_idx + visible_count {
            self.first_visible_agent_idx = selected + 1 - visible_count;
        }
    }

    pub fn selected_agent_idx(&self) -> Option<usize> {
        let selected = self.list_state.selected()?;
        if self.tree_enabled {
            match self.display_rows.get(selected)? {
                SidebarDisplayRow::Agent { agent_idx, .. } => Some(*agent_idx),
                SidebarDisplayRow::Group { .. } => None,
            }
        } else {
            Some(selected)
        }
    }

    pub fn display_row_for_agent_idx(&self, agent_idx: usize) -> Option<usize> {
        if !self.tree_enabled {
            return Some(agent_idx);
        }
        self.display_rows.iter().position(|row| {
            matches!(row, SidebarDisplayRow::Agent { agent_idx: idx, .. } if *idx == agent_idx)
        })
    }

    pub fn visible_row_count(&self) -> usize {
        if self.tree_enabled {
            self.display_rows.len()
        } else {
            self.agents.len()
        }
    }

    fn toggle_group_if_selected(&mut self, row_idx: usize) -> bool {
        let Some(SidebarDisplayRow::Group { key, .. }) = self.display_rows.get(row_idx) else {
            return false;
        };
        let key = key.clone();
        if !self.collapsed_tree_groups.insert(key.clone()) {
            self.collapsed_tree_groups.remove(&key);
        }
        self.rebuild_display_rows();
        self.list_state
            .select(Some(row_idx.min(self.display_rows.len().saturating_sub(1))));
        true
    }

    pub fn toggle_selected_group(&mut self) {
        if let Some(row_idx) = self.list_state.selected() {
            self.toggle_group_if_selected(row_idx);
        }
    }

    fn rebuild_display_rows(&mut self) {
        self.display_rows = build_display_rows(
            &self.agents,
            self.tree_enabled,
            self.tree_group_by,
            &self.collapsed_tree_groups,
            &self.sleeping_pane_ids,
            &self.window_prefix,
        );
    }

    /// Height in rows of a tile-mode item at the given index.
    /// Uses cached heights from the last render pass.
    fn tile_item_height(&self, idx: usize) -> usize {
        let base = self.tile_heights.get(idx).copied().unwrap_or(3);
        let mut h = base;
        if idx > 0 {
            h += 1; // top separator
        }
        if idx == self.agents.len() - 1 {
            h += 1; // bottom separator
        }
        h
    }

    pub fn jump_to_selected(&mut self) {
        if let Some(idx) = self.selected_agent_idx()
            && let Some(agent) = self.agents.get(idx)
        {
            let pane_id = agent.pane_id.clone();
            let _ = self.mux.switch_to_pane(&pane_id, None);
            // Signal daemon directly to bypass tmux hook round-trip latency
            super::daemon_ctrl::signal_daemon();
        }
    }

    pub fn activate_selected(&mut self) {
        if let Some(idx) = self.list_state.selected()
            && self.toggle_group_if_selected(idx)
        {
            return;
        }
        self.jump_to_selected();
    }

    pub fn toggle_layout_mode(&mut self) {
        if self.position == SidebarPosition::Top {
            return;
        }
        self.layout_mode = match self.layout_mode {
            SidebarLayoutMode::Compact => SidebarLayoutMode::Tiles,
            SidebarLayoutMode::Tiles => SidebarLayoutMode::Compact,
        };
        // Persist to tmux so all sidebar instances pick it up immediately
        let _ = Cmd::new("tmux")
            .args(&[
                "set-option",
                "-g",
                "@workmux_sidebar_layout",
                self.layout_mode.as_str(),
            ])
            .run();
        // Persist to settings.json so it survives tmux restarts
        if let Ok(store) = crate::state::StateStore::new()
            && let Ok(mut settings) = store.load_settings()
        {
            settings.sidebar_layout = Some(self.layout_mode.as_str().to_string());
            let _ = store.save_settings(&settings);
        }
    }

    /// Toggle the sleeping state of the selected agent.
    /// Does a read-modify-write on the tmux global option so concurrent
    /// toggles from different sidebar clients don't clobber each other.
    pub fn toggle_sleeping(&mut self) {
        let Some(pane_id) = self
            .selected_agent_idx()
            .and_then(|i| self.agents.get(i))
            .map(|a| a.pane_id.clone())
        else {
            return;
        };

        // Read current set from tmux (source of truth) to avoid losing
        // toggles made by other sidebar clients since our last snapshot.
        let mut current: std::collections::HashSet<String> = Cmd::new("tmux")
            .args(&["show-option", "-gqv", "@workmux_sleeping_panes"])
            .run_and_capture_stdout()
            .ok()
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();

        if !current.insert(pane_id.clone()) {
            current.remove(&pane_id);
        }

        // Update local state for immediate rendering
        self.sleeping_pane_ids = current.clone();

        // Write back to tmux
        let panes: String = current.into_iter().collect::<Vec<_>>().join(" ");
        if panes.is_empty() {
            let _ = Cmd::new("tmux")
                .args(&["set-option", "-gu", "@workmux_sleeping_panes"])
                .run();
        } else {
            let _ = Cmd::new("tmux")
                .args(&["set-option", "-g", "@workmux_sleeping_panes", &panes])
                .run();
        }

        // Signal daemon for immediate refresh (re-sort + broadcast)
        super::daemon_ctrl::signal_daemon();
    }

    pub fn window_prefix(&self) -> &str {
        &self.window_prefix
    }

    /// Record a resize event for debounced manual pane resize processing.
    pub fn on_resize_event(&mut self, cols: u16, rows: u16) {
        match self.position {
            SidebarPosition::Left => {
                let window_w = self.query_host_window_width();
                if self.last_window_width.is_some_and(|prev| prev != window_w) {
                    self.last_window_width = Some(window_w);
                    self.pending_resize_cols = None;
                    self.pending_resize_rows = None;
                    self.resize_deadline = None;
                    let _ = super::reflow_all_to_window_extent(Some(window_w));
                    return;
                }
                self.pending_resize_cols = Some(cols);
            }
            SidebarPosition::Top => {
                let window_h = self.query_host_window_height();
                if self.last_window_height.is_some_and(|prev| prev != window_h) {
                    self.last_window_height = Some(window_h);
                    self.pending_resize_cols = None;
                    self.pending_resize_rows = None;
                    self.resize_deadline = None;
                    let _ = super::reflow_all_to_window_extent(Some(window_h));
                    return;
                }
                self.pending_resize_rows = Some(rows);
            }
        }

        self.resize_deadline = Some(Instant::now() + Duration::from_millis(500));
    }

    /// Process any pending resize after the debounce period has elapsed.
    /// Skips detection during startup grace period.
    pub fn process_pending_resize(&mut self, startup: &Instant, startup_grace: Duration) {
        if startup.elapsed() < startup_grace {
            // Suppress detection during startup to avoid false positives from
            // initial pane creation layout divergence.
            self.pending_resize_cols = None;
            self.pending_resize_rows = None;
            self.resize_deadline = None;
            return;
        }

        let Some(deadline) = self.resize_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }

        let config = Config::load(None).unwrap_or_default();
        match self.position {
            SidebarPosition::Left => {
                let Some(pane_width) = self.pending_resize_cols else {
                    self.resize_deadline = None;
                    return;
                };
                let window_w = self.query_host_window_width();
                let prev_window_w = self.last_window_width;
                self.last_window_width = Some(window_w);
                self.pending_resize_cols = None;
                self.pending_resize_rows = None;
                self.resize_deadline = None;
                let Some(prev_ww) = prev_window_w else { return };
                if prev_ww != window_w {
                    return;
                }
                let actual_width = query_pane_width_for_pane().unwrap_or(pane_width);
                let expected = super::effective_width_for(&config, window_w);
                let delta = (actual_width as i16 - expected as i16).abs();
                if delta > 0 {
                    super::set_sidebar_width(actual_width);
                    if let Some(wid) = self.host_window_id() {
                        super::reflow_all_sidebars_except(wid);
                    }
                }
            }
            SidebarPosition::Top => {
                let Some(pane_height) = self.pending_resize_rows else {
                    self.resize_deadline = None;
                    return;
                };
                let window_h = self.query_host_window_height();
                let prev_window_h = self.last_window_height;
                self.last_window_height = Some(window_h);
                self.pending_resize_cols = None;
                self.pending_resize_rows = None;
                self.resize_deadline = None;
                let Some(prev_wh) = prev_window_h else { return };
                if prev_wh != window_h {
                    return;
                }
                let actual_height = query_pane_height_for_pane().unwrap_or(pane_height);
                let expected = super::effective_height_for(&config, window_h);
                let delta = (actual_height as i16 - expected as i16).abs();
                if delta > 0 {
                    super::set_sidebar_height(actual_height);
                    if let Some(wid) = self.host_window_id() {
                        super::reflow_all_sidebars_except(wid);
                    }
                }
            }
        }
    }

    fn query_host_window_width(&self) -> u16 {
        query_window_width_for_pane().unwrap_or(0)
    }

    fn query_host_window_height(&self) -> u16 {
        query_window_height_for_pane().unwrap_or(0)
    }

    /// Resolve the (primary, secondary) label pair for an agent row.
    ///
    /// Strips the workmux prefix from session/window names so the resolver only
    /// considers user-authored values. The window name is never promoted for
    /// non-tmux backends (signaled by `window_cmd: None`).
    pub fn resolve_agent_labels(&self, agent: &AgentPane) -> (String, String) {
        let project = extract_project_name(&agent.path);
        let (worktree, _is_main) = extract_worktree_name(
            &agent.session,
            &agent.window_name,
            &self.window_prefix,
            &agent.path,
        );

        // Workmux-managed names start with the configured prefix; treat them as
        // not user-authored by clearing them before the resolver sees them.
        let session = if agent.session.starts_with(&self.window_prefix) {
            ""
        } else {
            agent.session.as_str()
        };
        let window = if agent.window_name.starts_with(&self.window_prefix) {
            ""
        } else {
            agent.window_name.as_str()
        };

        resolve_labels(
            &project,
            session,
            &worktree,
            window,
            agent.window_cmd.as_deref(),
        )
    }
}

fn build_display_rows(
    agents: &[AgentPane],
    tree_enabled: bool,
    group_by: SidebarTreeGroupBy,
    collapsed: &HashSet<String>,
    sleeping_pane_ids: &HashSet<String>,
    window_prefix: &str,
) -> Vec<SidebarDisplayRow> {
    if !tree_enabled {
        return agents
            .iter()
            .enumerate()
            .map(|(agent_idx, _)| SidebarDisplayRow::Agent {
                agent_idx,
                depth: 0,
            })
            .collect();
    }

    let mut groups: Vec<(String, String, Vec<usize>, usize)> = Vec::new();
    let mut group_index = BTreeMap::new();

    for (idx, agent) in agents.iter().enumerate() {
        let (key, label) = tree_group_for_agent(agent, group_by, window_prefix);
        let group_idx = if let Some(group_idx) = group_index.get(&key).copied() {
            group_idx
        } else {
            let group_idx = groups.len();
            group_index.insert(key.clone(), group_idx);
            groups.push((key, label, Vec::new(), 0));
            group_idx
        };
        groups[group_idx].2.push(idx);
        if sleeping_pane_ids.contains(&agent.pane_id) {
            groups[group_idx].3 += 1;
        }
    }

    let mut rows = Vec::new();
    for (key, label, agent_indices, sleeping_count) in groups {
        let expanded = !collapsed.contains(&key);
        rows.push(SidebarDisplayRow::Group {
            key,
            label,
            agent_count: agent_indices.len(),
            sleeping_count,
            expanded,
        });
        if expanded {
            rows.extend(
                agent_indices
                    .into_iter()
                    .map(|agent_idx| SidebarDisplayRow::Agent {
                        agent_idx,
                        depth: 1,
                    }),
            );
        }
    }

    rows
}

fn tree_group_for_agent(
    agent: &AgentPane,
    group_by: SidebarTreeGroupBy,
    window_prefix: &str,
) -> (String, String) {
    let raw = match group_by {
        SidebarTreeGroupBy::Project => extract_project_name(&agent.path),
        SidebarTreeGroupBy::Session => strip_workmux_prefix(&agent.session, window_prefix),
        SidebarTreeGroupBy::Window => strip_workmux_prefix(&agent.window_name, window_prefix),
    };
    let label = if raw.trim().is_empty() {
        "Ungrouped".to_string()
    } else {
        raw
    };
    let key = format!("{}:{}", group_by_key(group_by), label);
    (key, label)
}

fn strip_workmux_prefix(value: &str, window_prefix: &str) -> String {
    value
        .strip_prefix(window_prefix)
        .unwrap_or(value)
        .trim()
        .to_string()
}

fn group_by_key(group_by: SidebarTreeGroupBy) -> &'static str {
    match group_by {
        SidebarTreeGroupBy::Project => "project",
        SidebarTreeGroupBy::Session => "session",
        SidebarTreeGroupBy::Window => "window",
    }
}

/// Resolve template strings from config, falling back to defaults.
fn resolved_template_strings(config: &Config) -> (String, Vec<String>, Vec<String>) {
    let compact = config
        .sidebar
        .templates
        .as_ref()
        .and_then(|t| t.compact.clone())
        .unwrap_or_else(|| DEFAULT_COMPACT_TEMPLATE.to_string());
    let tiles = config
        .sidebar
        .templates
        .as_ref()
        .and_then(|t| t.tiles.clone())
        .unwrap_or_else(|| {
            DEFAULT_TILE_TEMPLATES
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
    let horizontal = config
        .sidebar
        .templates
        .as_ref()
        .and_then(|t| t.horizontal.clone())
        .unwrap_or_else(|| {
            DEFAULT_HORIZONTAL_TEMPLATES
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
    (compact, tiles, horizontal)
}

fn default_template_lines(default_lines: &[&str]) -> Vec<Vec<Token>> {
    default_lines
        .iter()
        .map(|s| parse_line(s).expect("default template is valid"))
        .collect()
}

fn parse_template_lines(lines: &[String], kind: &str) -> Result<Vec<Vec<Token>>, TemplateError> {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            parse_line(line).map_err(|e| {
                let location = format!("{kind}[{i}]");
                tracing::warn!("failed to parse {location} template '{}': {}", line, e);
                TemplateError::new(location, &e)
            })
        })
        .collect()
}

fn parse_templates(config: &Config) -> (ParsedTemplates, Option<TemplateError>) {
    let (compact_str, tile_strs, horizontal_strs) = resolved_template_strings(config);
    let mut first_error = None;

    let compact = match parse_line(&compact_str) {
        Ok(tokens) => tokens,
        Err(e) => {
            tracing::warn!("failed to parse compact template: {}, using default", e);
            first_error.get_or_insert_with(|| TemplateError::new("compact", &e));
            parse_line(DEFAULT_COMPACT_TEMPLATE).expect("default template is valid")
        }
    };
    let tiles = match parse_template_lines(&tile_strs, "tiles") {
        Ok(tokens) => tokens,
        Err(e) => {
            first_error.get_or_insert(e);
            default_template_lines(DEFAULT_TILE_TEMPLATES)
        }
    };
    let horizontal = match parse_template_lines(&horizontal_strs, "horizontal") {
        Ok(tokens) => tokens,
        Err(e) => {
            first_error.get_or_insert(e);
            default_template_lines(DEFAULT_HORIZONTAL_TEMPLATES)
        }
    };

    (
        ParsedTemplates {
            compact,
            tiles,
            horizontal,
        },
        first_error,
    )
}

/// Query the window width for the current tmux pane (standalone for use before
/// `Self` exists).
fn query_window_width_for_pane() -> Option<u16> {
    let pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
    let mut args = vec!["display-message", "-p"];
    if !pane_id.is_empty() {
        args.extend_from_slice(&["-t", &pane_id]);
    }
    args.push("#{window_width}");
    Cmd::new("tmux")
        .args(&args)
        .run_and_capture_stdout()
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn query_window_height_for_pane() -> Option<u16> {
    let pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
    let mut args = vec!["display-message", "-p"];
    if !pane_id.is_empty() {
        args.extend_from_slice(&["-t", &pane_id]);
    }
    args.push("#{window_height}");
    Cmd::new("tmux")
        .args(&args)
        .run_and_capture_stdout()
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Query the actual pane width from tmux. Used to verify the sidebar pane
/// size after a manual resize, since crossterm's SIGWINCH-derived cols may
/// differ from what tmux reports via #{pane_width}.
fn query_pane_width_for_pane() -> Option<u16> {
    query_pane_extent_for_pane("#{pane_width}")
}

fn query_pane_height_for_pane() -> Option<u16> {
    query_pane_extent_for_pane("#{pane_height}")
}

fn query_pane_extent_for_pane(format: &str) -> Option<u16> {
    let pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
    let mut args = vec!["display-message", "-p"];
    if !pane_id.is_empty() {
        args.extend_from_slice(&["-t", &pane_id]);
    }
    args.push(format);
    Cmd::new("tmux")
        .args(&args)
        .run_and_capture_stdout()
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&extent| extent > 0)
}

/// Parse new template strings, mutating `templates` and the cached strings.
/// On any parse error, keep `templates` as-is and log a warning. The cached
/// strings are still updated so we don't retry the same broken value on every
/// snapshot.
fn try_reparse_templates(
    templates: &mut ParsedTemplates,
    current_compact_str: &mut String,
    current_tile_strs: &mut Vec<String>,
    current_horizontal_strs: &mut Vec<String>,
    new_compact: &str,
    new_tiles: &[String],
    new_horizontal: &[String],
) -> Option<TemplateError> {
    let mut first_error = None;

    match parse_line(new_compact) {
        Ok(tokens) => templates.compact = tokens,
        Err(e) => {
            tracing::warn!("compact template parse error, keeping previous: {}", e);
            first_error.get_or_insert_with(|| TemplateError::new("compact", &e));
        }
    }

    match parse_template_lines(new_tiles, "tiles") {
        Ok(tokens) => templates.tiles = tokens,
        Err(e) => {
            tracing::warn!(
                "{} template parse error, keeping previous: {}",
                e.location,
                e.message
            );
            first_error.get_or_insert(e);
        }
    }

    match parse_template_lines(new_horizontal, "horizontal") {
        Ok(tokens) => templates.horizontal = tokens,
        Err(e) => {
            tracing::warn!(
                "{} template parse error, keeping previous: {}",
                e.location,
                e.message
            );
            first_error.get_or_insert(e);
        }
    }

    *current_compact_str = new_compact.to_string();
    *current_tile_strs = new_tiles.to_vec();
    *current_horizontal_strs = new_horizontal.to_vec();
    first_error
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentIconConfig, AgentIconDetails};
    use std::path::PathBuf;

    fn test_agent(path: &str, pane_id: &str) -> AgentPane {
        AgentPane {
            session: "wm-feature".to_string(),
            window_name: "wm-feature".to_string(),
            pane_id: pane_id.to_string(),
            window_id: String::new(),
            path: PathBuf::from(path),
            pane_title: None,
            status: None,
            status_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
        }
    }

    #[test]
    fn resolved_icons_legacy_string() {
        let mut map = AgentIcons::new();
        map.insert(
            "claude".to_string(),
            AgentIconConfig::Plain("C".to_string()),
        );
        let r = ResolvedAgentIcons::from_config(Some(&map));
        assert_eq!(r.icons.get("claude").map(String::as_str), Some("C"));
        assert!(r.colors.is_empty());
    }

    #[test]
    fn display_rows_group_agents_by_first_seen_project() {
        let agents = vec![
            test_agent("/tmp/workmux__worktrees/a", "%1"),
            test_agent("/tmp/api__worktrees/b", "%2"),
            test_agent("/tmp/workmux__worktrees/c", "%3"),
        ];

        let rows = build_display_rows(
            &agents,
            true,
            SidebarTreeGroupBy::Project,
            &HashSet::new(),
            &HashSet::new(),
            "wm-",
        );

        assert_eq!(
            rows,
            vec![
                SidebarDisplayRow::Group {
                    key: "project:workmux".to_string(),
                    label: "workmux".to_string(),
                    agent_count: 2,
                    sleeping_count: 0,
                    expanded: true,
                },
                SidebarDisplayRow::Agent {
                    agent_idx: 0,
                    depth: 1,
                },
                SidebarDisplayRow::Agent {
                    agent_idx: 2,
                    depth: 1,
                },
                SidebarDisplayRow::Group {
                    key: "project:api".to_string(),
                    label: "api".to_string(),
                    agent_count: 1,
                    sleeping_count: 0,
                    expanded: true,
                },
                SidebarDisplayRow::Agent {
                    agent_idx: 1,
                    depth: 1,
                },
            ]
        );
    }

    #[test]
    fn display_rows_hide_agents_for_collapsed_group() {
        let agents = vec![
            test_agent("/tmp/workmux__worktrees/a", "%1"),
            test_agent("/tmp/workmux__worktrees/b", "%2"),
        ];
        let collapsed = HashSet::from(["project:workmux".to_string()]);
        let sleeping = HashSet::from(["%2".to_string()]);

        let rows = build_display_rows(
            &agents,
            true,
            SidebarTreeGroupBy::Project,
            &collapsed,
            &sleeping,
            "wm-",
        );

        assert_eq!(
            rows,
            vec![SidebarDisplayRow::Group {
                key: "project:workmux".to_string(),
                label: "workmux".to_string(),
                agent_count: 2,
                sleeping_count: 1,
                expanded: false,
            }]
        );
    }

    #[test]
    fn tree_drag_reorders_within_group_only() {
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: "compact".to_string(),
            message: "test".to_string(),
        });
        app.tree_enabled = true;
        app.tree_group_by = SidebarTreeGroupBy::Project;
        app.agents = vec![
            test_agent("/tmp/workmux__worktrees/a", "%1"),
            test_agent("/tmp/api__worktrees/b", "%2"),
            test_agent("/tmp/workmux__worktrees/c", "%3"),
        ];
        app.rebuild_display_rows();

        // Rows: group(workmux), %1, %3, group(api), %2.
        app.start_drag(1);
        app.update_drag(2);

        let panes: Vec<_> = app
            .agents
            .iter()
            .map(|agent| agent.pane_id.as_str())
            .collect();
        assert_eq!(panes, vec!["%3", "%2", "%1"]);
        assert_eq!(app.list_state.selected(), Some(2));
    }

    #[test]
    fn tree_display_preserves_group_order_after_manual_order_refresh() {
        let agents = vec![
            test_agent("/tmp/workmux__worktrees/c", "%3"),
            test_agent("/tmp/api__worktrees/b", "%2"),
            test_agent("/tmp/workmux__worktrees/a", "%1"),
        ];

        let rows = build_display_rows(
            &agents,
            true,
            SidebarTreeGroupBy::Project,
            &HashSet::new(),
            &HashSet::new(),
            "wm-",
        );

        assert_eq!(
            rows,
            vec![
                SidebarDisplayRow::Group {
                    key: "project:workmux".to_string(),
                    label: "workmux".to_string(),
                    agent_count: 2,
                    sleeping_count: 0,
                    expanded: true,
                },
                SidebarDisplayRow::Agent {
                    agent_idx: 0,
                    depth: 1,
                },
                SidebarDisplayRow::Agent {
                    agent_idx: 2,
                    depth: 1,
                },
                SidebarDisplayRow::Group {
                    key: "project:api".to_string(),
                    label: "api".to_string(),
                    agent_count: 1,
                    sleeping_count: 0,
                    expanded: true,
                },
                SidebarDisplayRow::Agent {
                    agent_idx: 1,
                    depth: 1,
                },
            ]
        );
    }

    #[test]
    fn tree_drag_ignores_group_rows_and_cross_group_targets() {
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: "compact".to_string(),
            message: "test".to_string(),
        });
        app.tree_enabled = true;
        app.tree_group_by = SidebarTreeGroupBy::Project;
        app.agents = vec![
            test_agent("/tmp/workmux__worktrees/a", "%1"),
            test_agent("/tmp/api__worktrees/b", "%2"),
            test_agent("/tmp/workmux__worktrees/c", "%3"),
        ];
        app.rebuild_display_rows();

        app.start_drag(0);
        assert!(app.drag_state.is_none());

        app.start_drag(1);
        app.update_drag(4);
        let panes: Vec<_> = app
            .agents
            .iter()
            .map(|agent| agent.pane_id.as_str())
            .collect();
        assert_eq!(panes, vec!["%1", "%2", "%3"]);
        assert_eq!(app.list_state.selected(), Some(1));
    }

    #[test]
    fn resolved_icons_detailed_with_valid_color() {
        let mut map = AgentIcons::new();
        map.insert(
            "claude".to_string(),
            AgentIconConfig::Detailed(AgentIconDetails {
                icon: Some("X".to_string()),
                color: Some("#00ff00".to_string()),
            }),
        );
        let r = ResolvedAgentIcons::from_config(Some(&map));
        assert_eq!(r.icons.get("claude").map(String::as_str), Some("X"));
        assert_eq!(r.colors.get("claude"), Some(&Some(Color::Rgb(0, 255, 0))));
    }

    #[test]
    fn resolved_icons_blank_color_disables_default() {
        let mut map = AgentIcons::new();
        map.insert(
            "claude".to_string(),
            AgentIconConfig::Detailed(AgentIconDetails {
                icon: None,
                color: Some("   ".to_string()),
            }),
        );
        let r = ResolvedAgentIcons::from_config(Some(&map));
        assert_eq!(r.colors.get("claude"), Some(&None));
    }

    #[test]
    fn resolved_icons_invalid_color_is_dropped() {
        let mut map = AgentIcons::new();
        map.insert(
            "claude".to_string(),
            AgentIconConfig::Detailed(AgentIconDetails {
                icon: None,
                color: Some("not-a-color".to_string()),
            }),
        );
        let r = ResolvedAgentIcons::from_config(Some(&map));
        // No entry: lookup falls through to AgentKind::default_color at use site.
        assert!(!r.colors.contains_key("claude"));
    }

    #[test]
    fn resolved_icons_null_variant_is_no_op() {
        let mut map = AgentIcons::new();
        map.insert("claude".to_string(), AgentIconConfig::Null);
        let r = ResolvedAgentIcons::from_config(Some(&map));
        assert!(r.icons.is_empty());
        assert!(r.colors.is_empty());
    }

    fn parsed_for(s: &str) -> ParsedTemplates {
        ParsedTemplates {
            compact: parse_line(s).unwrap(),
            tiles: vec![parse_line(s).unwrap()],
            horizontal: vec![parse_line(s).unwrap()],
        }
    }

    #[test]
    fn reparse_swaps_templates_on_change() {
        let mut templates = parsed_for("{primary}");
        let mut compact = "{primary}".to_string();
        let mut tiles = vec!["{primary}".to_string()];
        let mut top = vec!["{primary}".to_string()];

        let new_compact = "{secondary} {fill}";
        let new_tiles = vec!["{primary} {fill} {elapsed}".to_string()];
        let new_top = vec!["{secondary} {fill} {git_stats}".to_string()];
        let error = try_reparse_templates(
            &mut templates,
            &mut compact,
            &mut tiles,
            &mut top,
            new_compact,
            &new_tiles,
            &new_top,
        );

        assert_eq!(error, None);
        assert_eq!(compact, new_compact);
        assert_eq!(tiles, new_tiles);
        assert_eq!(top, new_top);
        // 3 tokens: secondary field, literal " ", fill
        assert_eq!(templates.compact.len(), 3);
    }

    #[test]
    fn reparse_keeps_previous_on_compact_parse_error() {
        let original_str = "{primary}".to_string();
        let mut templates = parsed_for(&original_str);
        let original_tokens = templates.compact.clone();
        let mut compact = original_str.clone();
        let mut tiles = vec![original_str.clone()];
        let mut top = vec![original_str.clone()];

        let bad_compact = "{unclosed";
        let error = try_reparse_templates(
            &mut templates,
            &mut compact,
            &mut tiles,
            &mut top,
            bad_compact,
            &[original_str.clone()],
            &[original_str.clone()],
        );

        assert_eq!(
            error,
            Some(TemplateError {
                location: "compact".to_string(),
                message: "unclosed brace at column 1: '{unclosed'".to_string(),
            })
        );
        // Templates unchanged
        assert_eq!(templates.compact, original_tokens);
        // But cached strings updated so we don't retry the broken value
        assert_eq!(compact, bad_compact);
    }

    #[test]
    fn reparse_keeps_previous_on_tile_parse_error() {
        let mut templates = parsed_for("{primary}");
        let original_tiles = templates.tiles.clone();
        let mut compact = "{primary}".to_string();
        let mut tiles = vec!["{primary}".to_string()];
        let mut top = vec!["{primary}".to_string()];

        let error = try_reparse_templates(
            &mut templates,
            &mut compact,
            &mut tiles,
            &mut top,
            "{primary}",
            &["{pr_status}".to_string()],
            &["{primary}".to_string()],
        );

        assert_eq!(templates.tiles, original_tiles);
        assert_eq!(tiles, vec!["{pr_status}".to_string()]);
        assert_eq!(
            error,
            Some(TemplateError {
                location: "tiles[0]".to_string(),
                message: "unknown token 'pr_status' at column 1".to_string(),
            })
        );
    }

    #[test]
    fn parse_templates_reports_invalid_horizontal_template() {
        let mut config = Config::default();
        config.sidebar.templates = Some(crate::config::TemplatesConfig {
            horizontal: Some(vec!["{primary}".to_string(), "{pr_status}".to_string()]),
            ..Default::default()
        });

        let (templates, error) = parse_templates(&config);

        assert_eq!(
            templates.horizontal,
            default_template_lines(DEFAULT_HORIZONTAL_TEMPLATES)
        );
        assert_eq!(
            error,
            Some(TemplateError {
                location: "horizontal[1]".to_string(),
                message: "unknown token 'pr_status' at column 1".to_string(),
            })
        );
    }

    #[test]
    fn parse_templates_reports_first_error() {
        let mut config = Config::default();
        config.sidebar.templates = Some(crate::config::TemplatesConfig {
            compact: Some("{bad_compact}".to_string()),
            tiles: Some(vec!["{pr_status}".to_string()]),
            ..Default::default()
        });

        let (_, error) = parse_templates(&config);

        assert_eq!(
            error,
            Some(TemplateError {
                location: "compact".to_string(),
                message: "unknown token 'bad_compact' at column 1".to_string(),
            })
        );
    }

    #[test]
    fn reparse_updates_valid_sections_when_tile_parse_fails() {
        let mut templates = parsed_for("{primary}");
        let mut compact = "{primary}".to_string();
        let mut tiles = vec!["{primary}".to_string()];
        let mut top = vec!["{primary}".to_string()];

        let error = try_reparse_templates(
            &mut templates,
            &mut compact,
            &mut tiles,
            &mut top,
            "{secondary}",
            &["{pr_status}".to_string()],
            &["{elapsed}".to_string()],
        );

        assert_eq!(templates.compact, parse_line("{secondary}").unwrap());
        assert_eq!(templates.tiles, vec![parse_line("{primary}").unwrap()]);
        assert_eq!(templates.horizontal, vec![parse_line("{elapsed}").unwrap()]);
        assert_eq!(
            error,
            Some(TemplateError {
                location: "tiles[0]".to_string(),
                message: "unknown token 'pr_status' at column 1".to_string(),
            })
        );
    }
}

/// Detect this sidebar's host window using TMUX_PANE (stable, one-time).
/// Returns (session, window_id).
fn detect_host_window() -> (Option<String>, Option<String>) {
    let pane_id = std::env::var("TMUX_PANE").ok().unwrap_or_default();
    let mut args = vec!["display-message", "-p"];
    if !pane_id.is_empty() {
        args.extend_from_slice(&["-t", &pane_id]);
    }
    args.push("#{session_name}\t#{window_id}");
    let output = Cmd::new("tmux")
        .args(&args)
        .run_and_capture_stdout()
        .ok()
        .unwrap_or_default();
    let trimmed = output.trim();
    let mut parts = trimmed
        .split('\t')
        .map(|s| (!s.is_empty()).then(|| s.to_string()));
    let session = parts.next().flatten();
    let window_id = parts.next().flatten();
    (session, window_id)
}
