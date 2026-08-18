//! Media player UI widget and data store.

use egui::{Color32, CornerRadius, Rect, Stroke, Ui, Vec2};
use instant::Instant;
use std::collections::HashMap;
use std::time::Duration;
use strom_types::mediaplayer::PlayerState;
use strom_types::FlowId;

/// Time-to-live for media player data before it's considered stale.
const PLAYER_DATA_TTL: Duration = Duration::from_millis(1000);

/// Minimum interval between seek API calls during drag.
const SEEK_THROTTLE_INTERVAL: Duration = Duration::from_millis(150);

/// Jump size for skip forward/backward buttons.
const JUMP_SECONDS: u64 = 15;
const JUMP_NS: u64 = JUMP_SECONDS * 1_000_000_000;

/// Throttle state for seek operations to avoid flooding the API during drags.
#[derive(Debug, Clone, Default)]
pub struct SeekThrottle {
    /// Last time a seek was actually sent to the API.
    last_sent: Option<Instant>,
    /// Pending seek that hasn't been dispatched yet (block_id, position_ns).
    pending: Option<(String, u64)>,
}

impl SeekThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a seek request. Returns `Some(position_ns)` if it should be sent now.
    pub fn request(&mut self, block_id: &str, position_ns: u64) -> Option<u64> {
        let now = Instant::now();
        let should_send = match self.last_sent {
            None => true,
            Some(t) => now.duration_since(t) >= SEEK_THROTTLE_INTERVAL,
        };
        if should_send {
            self.last_sent = Some(now);
            self.pending = None;
            Some(position_ns)
        } else {
            self.pending = Some((block_id.to_string(), position_ns));
            None
        }
    }

    /// Flush any pending seek whose throttle interval has elapsed.
    /// Call this every frame to ensure the final drag position is sent.
    pub fn flush(&mut self) -> Option<(String, u64)> {
        self.pending.as_ref()?;
        let now = Instant::now();
        let should_flush = match self.last_sent {
            None => true,
            Some(t) => now.duration_since(t) >= SEEK_THROTTLE_INTERVAL,
        };
        if should_flush {
            self.last_sent = Some(now);
            self.pending.take()
        } else {
            None
        }
    }
}

/// Media player data for a specific block.
#[derive(Debug, Clone)]
pub struct MediaPlayerData {
    /// Current playback state
    pub state: PlayerState,
    /// Current position in nanoseconds
    pub position_ns: u64,
    /// Total duration in nanoseconds
    pub duration_ns: u64,
    /// Current file index (0-based)
    pub current_file_index: usize,
    /// Total number of files in playlist
    pub total_files: usize,
    /// Current file path (if any)
    pub current_file: Option<String>,
}

impl Default for MediaPlayerData {
    fn default() -> Self {
        Self {
            state: PlayerState::Stopped,
            position_ns: 0,
            duration_ns: 0,
            current_file_index: 0,
            total_files: 0,
            current_file: None,
        }
    }
}

/// Media player data with timestamp for TTL tracking.
#[derive(Debug, Clone)]
struct TimestampedPlayerData {
    data: MediaPlayerData,
    updated_at: Instant,
}

/// Key for identifying media player data (flow + block).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct MediaPlayerKey {
    pub flow_id: FlowId,
    pub block_id: String,
}

/// Storage for all media player data in the application.
#[derive(Debug, Clone, Default)]
pub struct MediaPlayerDataStore {
    data: HashMap<MediaPlayerKey, TimestampedPlayerData>,
}

impl MediaPlayerDataStore {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Update position only (from periodic position events).
    pub fn update_position(
        &mut self,
        flow_id: FlowId,
        block_id: String,
        position_ns: u64,
        duration_ns: u64,
        current_file_index: usize,
        total_files: usize,
    ) {
        let key = MediaPlayerKey {
            flow_id,
            block_id: block_id.clone(),
        };

        if let Some(entry) = self.data.get_mut(&key) {
            entry.data.position_ns = position_ns;
            entry.data.duration_ns = duration_ns;
            entry.data.current_file_index = current_file_index;
            entry.data.total_files = total_files;
            entry.updated_at = Instant::now();
        } else {
            // Create new entry if none exists
            self.data.insert(
                key,
                TimestampedPlayerData {
                    data: MediaPlayerData {
                        state: PlayerState::Stopped,
                        position_ns,
                        duration_ns,
                        current_file_index,
                        total_files,
                        current_file: None,
                    },
                    updated_at: Instant::now(),
                },
            );
        }
    }

    /// Update state (from state change events).
    pub fn update_state(
        &mut self,
        flow_id: FlowId,
        block_id: String,
        state: PlayerState,
        current_file: Option<String>,
    ) {
        let key = MediaPlayerKey {
            flow_id,
            block_id: block_id.clone(),
        };

        if let Some(entry) = self.data.get_mut(&key) {
            entry.data.state = state;
            entry.data.current_file = current_file;
            entry.updated_at = Instant::now();
        } else {
            // Create new entry if none exists
            self.data.insert(
                key,
                TimestampedPlayerData {
                    data: MediaPlayerData {
                        state,
                        position_ns: 0,
                        duration_ns: 0,
                        current_file_index: 0,
                        total_files: 0,
                        current_file,
                    },
                    updated_at: Instant::now(),
                },
            );
        }
    }

    /// Get media player data for a specific block.
    /// Returns None if the data is stale (older than TTL).
    pub fn get(&self, flow_id: &FlowId, block_id: &str) -> Option<&MediaPlayerData> {
        let key = MediaPlayerKey {
            flow_id: *flow_id,
            block_id: block_id.to_string(),
        };
        self.data.get(&key).and_then(|timestamped| {
            if timestamped.updated_at.elapsed() < PLAYER_DATA_TTL {
                Some(&timestamped.data)
            } else {
                None
            }
        })
    }
}

/// Format nanoseconds as MM:SS.
fn format_time(ns: u64) -> String {
    let total_seconds = ns / 1_000_000_000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{:02}:{:02}", minutes, seconds)
}

/// Calculate the height needed for the compact media player widget.
pub fn calculate_compact_height() -> f32 {
    // Play/pause + prev/next row + seek bar row + time display
    50.0
}

/// Render a compact media player widget (for graph nodes).
///
/// Returns a tuple of (action, seek_position) if user interacted with controls.
/// Action can be: "play", "pause", "prev", "next", "seek", or "playlist".
pub fn show_compact(ui: &mut Ui, player_data: &MediaPlayerData) -> Option<(String, Option<u64>)> {
    // Show current file name (if any), truncated with hover for full path
    if let Some(ref file) = player_data.current_file {
        let filename = std::path::Path::new(file)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| file.clone());
        let label = egui::Label::new(filename)
            .truncate()
            .sense(egui::Sense::hover());
        ui.add(label).on_hover_text(file);
    }

    // Show playback state and file count
    ui.label(format!(
        "State: {} | Files: {}",
        player_data.state, player_data.total_files
    ));

    // Control buttons row: Playlist | Prev | Play/Pause | Next | file count
    let button_action = ui
        .horizontal(|ui| {
            // Playlist button
            if ui
                .button(egui_phosphor::regular::PLUS)
                .on_hover_text("Edit playlist")
                .clicked()
            {
                tracing::debug!("Playlist button clicked");
                return Some(("playlist".to_string(), None));
            }

            // Previous button
            if ui
                .button(egui_phosphor::regular::SKIP_BACK)
                .on_hover_text("Previous file")
                .clicked()
            {
                tracing::debug!("Previous button clicked");
                return Some(("previous".to_string(), None));
            }

            // Play/Pause button
            let play_pause_text = if player_data.state == PlayerState::Playing {
                egui_phosphor::regular::PAUSE
            } else {
                egui_phosphor::regular::PLAY
            };
            let play_hover = if player_data.state == PlayerState::Playing {
                "Pause"
            } else {
                "Play"
            };
            if ui
                .button(play_pause_text)
                .on_hover_text(play_hover)
                .clicked()
            {
                tracing::debug!("Play/Pause button clicked, state={}", player_data.state);
                if player_data.state == PlayerState::Playing {
                    return Some(("pause".to_string(), None));
                } else {
                    return Some(("play".to_string(), None));
                }
            }

            // Next button
            if ui
                .button(egui_phosphor::regular::SKIP_FORWARD)
                .on_hover_text("Next file")
                .clicked()
            {
                tracing::debug!("Next button clicked");
                return Some(("next".to_string(), None));
            }

            // File info
            if player_data.total_files > 0 {
                ui.label(format!(
                    "{}/{}",
                    player_data.current_file_index + 1,
                    player_data.total_files
                ));
            } else {
                ui.label("-");
            }

            None
        })
        .inner;

    // If button was clicked, return that action
    if button_action.is_some() {
        return button_action;
    }

    // Seek row: -15s | [progress bar] | +15s
    let seek_action = ui
        .horizontal(|ui| {
            // Jump back 15s
            if ui
                .button(egui_phosphor::regular::REWIND)
                .on_hover_text(format!("-{}s", JUMP_SECONDS))
                .clicked()
            {
                let pos = player_data.position_ns.saturating_sub(JUMP_NS);
                return Some(("seek".to_string(), Some(pos)));
            }

            // Progress bar
            let progress = if player_data.duration_ns > 0 {
                player_data.position_ns as f32 / player_data.duration_ns as f32
            } else {
                0.0
            };

            let bar_width = ui.available_width() - 30.0; // leave room for +15s button
            let (rect, response) = ui.allocate_exact_size(
                Vec2::new(bar_width.max(20.0), 12.0),
                egui::Sense::click_and_drag(),
            );

            let painter = ui.painter();

            // Background
            painter.rect_filled(rect, CornerRadius::same(2), Color32::from_gray(40));

            // Progress fill
            if progress > 0.0 {
                let fill_rect = Rect::from_min_size(
                    rect.min,
                    Vec2::new(rect.width() * progress, rect.height()),
                );
                painter.rect_filled(
                    fill_rect,
                    CornerRadius::same(2),
                    Color32::from_rgb(80, 120, 200),
                );
            }

            // Border
            painter.rect(
                rect,
                CornerRadius::same(2),
                Color32::TRANSPARENT,
                Stroke::new(1.0_f32, Color32::from_gray(80)),
                egui::epaint::StrokeKind::Inside,
            );

            // Seek: click or drag on the progress bar
            if response.clicked() || response.dragged() {
                if let Some(pointer_pos) = response.interact_pointer_pos() {
                    let seek_progress =
                        ((pointer_pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                    let seek_ns = (seek_progress as f64 * player_data.duration_ns as f64) as u64;
                    return Some(("seek".to_string(), Some(seek_ns)));
                }
            }

            // Jump forward 15s
            if ui
                .button(egui_phosphor::regular::FAST_FORWARD)
                .on_hover_text(format!("+{}s", JUMP_SECONDS))
                .clicked()
            {
                let pos = player_data
                    .position_ns
                    .saturating_add(JUMP_NS)
                    .min(player_data.duration_ns);
                return Some(("seek".to_string(), Some(pos)));
            }

            None
        })
        .inner;

    if seek_action.is_some() {
        return seek_action;
    }

    // Time display
    ui.horizontal(|ui| {
        ui.label(format!(
            "{} / {}",
            format_time(player_data.position_ns),
            format_time(player_data.duration_ns)
        ));
    });

    None
}

/// Render a full media player widget (for property inspector).
pub fn show_full(ui: &mut Ui, player_data: &MediaPlayerData) -> Option<(String, Option<u64>)> {
    let mut action: Option<(String, Option<u64>)> = None;

    ui.heading("Media Player");
    ui.separator();

    // Status
    ui.horizontal(|ui| {
        ui.label("Status:");
        let status_color = match player_data.state {
            PlayerState::Playing => Color32::GREEN,
            PlayerState::Paused => Color32::YELLOW,
            PlayerState::Stopped => Color32::GRAY,
        };
        ui.colored_label(status_color, player_data.state.to_string());
    });

    // Current file
    if let Some(ref file) = player_data.current_file {
        ui.horizontal(|ui| {
            ui.label("File:");
            // Show just the filename
            let filename = std::path::Path::new(file)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| file.clone());
            ui.label(filename);
        });
    }

    // Playlist position
    if player_data.total_files > 0 {
        ui.horizontal(|ui| {
            ui.label("Playlist:");
            ui.label(format!(
                "{} / {}",
                player_data.current_file_index + 1,
                player_data.total_files
            ));
        });
    }

    ui.add_space(10.0);

    // Transport buttons row: Prev | Play/Pause | Stop | Next
    ui.horizontal(|ui| {
        if ui
            .button(format!("{} Prev", egui_phosphor::regular::SKIP_BACK))
            .clicked()
        {
            action = Some(("previous".to_string(), None));
        }

        let play_pause_text = if player_data.state == PlayerState::Playing {
            format!("{} Pause", egui_phosphor::regular::PAUSE)
        } else {
            format!("{} Play", egui_phosphor::regular::PLAY)
        };
        if ui.button(play_pause_text).clicked() {
            if player_data.state == PlayerState::Playing {
                action = Some(("pause".to_string(), None));
            } else {
                action = Some(("play".to_string(), None));
            }
        }

        if ui
            .button(format!("{} Stop", egui_phosphor::regular::STOP))
            .clicked()
        {
            action = Some(("stop".to_string(), None));
        }

        if ui
            .button(format!("Next {}", egui_phosphor::regular::SKIP_FORWARD))
            .clicked()
        {
            action = Some(("next".to_string(), None));
        }
    });

    // Seek row: -15s | seek slider | +15s
    ui.horizontal(|ui| {
        if ui
            .button(format!(
                "{} -{}s",
                egui_phosphor::regular::REWIND,
                JUMP_SECONDS
            ))
            .clicked()
        {
            let pos = player_data.position_ns.saturating_sub(JUMP_NS);
            action = Some(("seek".to_string(), Some(pos)));
        }

        let mut progress = if player_data.duration_ns > 0 {
            player_data.position_ns as f32 / player_data.duration_ns as f32
        } else {
            0.0
        };

        let slider = egui::Slider::new(&mut progress, 0.0..=1.0)
            .show_value(false)
            .text("");
        if ui.add(slider).changed() && player_data.duration_ns > 0 {
            let seek_ns = (progress as f64 * player_data.duration_ns as f64) as u64;
            action = Some(("seek".to_string(), Some(seek_ns)));
        }

        if ui
            .button(format!(
                "+{}s {}",
                JUMP_SECONDS,
                egui_phosphor::regular::FAST_FORWARD
            ))
            .clicked()
        {
            let pos = player_data
                .position_ns
                .saturating_add(JUMP_NS)
                .min(player_data.duration_ns);
            action = Some(("seek".to_string(), Some(pos)));
        }
    });

    // Time display
    ui.horizontal(|ui| {
        ui.label(format!(
            "{} / {}",
            format_time(player_data.position_ns),
            format_time(player_data.duration_ns)
        ));
    });

    action
}

/// A media file or folder entry for browsing.
#[derive(Debug, Clone)]
pub struct MediaEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// State for the playlist editor window.
#[derive(Debug, Clone)]
pub struct PlaylistEditor {
    /// Flow ID this editor is for
    pub flow_id: FlowId,
    /// Block ID this editor is for
    pub block_id: String,
    /// Whether the editor window is open
    pub open: bool,
    /// Current playlist being edited (file URIs)
    pub playlist: Vec<String>,
    /// Whether we need to save changes
    pub dirty: bool,
    /// Current browser path (relative to media folder)
    pub browser_path: String,
    /// Parent path for "up" navigation
    pub browser_parent: Option<String>,
    /// Available files/folders in current path
    pub browser_entries: Vec<MediaEntry>,
    /// Whether we're waiting for browser data
    pub browser_loading: bool,
    /// Whether we need to refresh the file list
    pub browser_needs_refresh: bool,
    /// Index of the currently playing file (for highlighting)
    pub current_playing_index: Option<usize>,
    /// Width in pixels of the file browser left pane (draggable)
    pub browser_width_px: f32,
}

impl PlaylistEditor {
    pub fn new(flow_id: FlowId, block_id: String) -> Self {
        Self {
            flow_id,
            block_id,
            open: true,
            playlist: Vec::new(),
            dirty: false,
            browser_path: String::new(),
            browser_parent: None,
            browser_entries: Vec::new(),
            browser_loading: false,
            browser_needs_refresh: true, // Load on first show
            current_playing_index: None,
            browser_width_px: 350.0,
        }
    }

    /// Set the playlist from the current player data.
    pub fn set_playlist(&mut self, playlist: Vec<String>) {
        self.playlist = playlist;
        self.dirty = false;
    }

    /// Update browser with file listing results.
    pub fn set_browser_entries(
        &mut self,
        current_path: String,
        parent_path: Option<String>,
        entries: Vec<MediaEntry>,
    ) {
        self.browser_path = current_path;
        self.browser_parent = parent_path;
        self.browser_entries = entries;
        self.browser_loading = false;
        self.browser_needs_refresh = false;
    }

    /// Request to navigate to a path in the browser.
    /// Returns the path to load if refresh is needed.
    pub fn get_browser_path_to_load(&mut self) -> Option<String> {
        if self.browser_needs_refresh && !self.browser_loading {
            self.browser_loading = true;
            Some(self.browser_path.clone())
        } else {
            None
        }
    }

    /// Show the playlist editor window.
    /// Returns Some(playlist) if the user clicked Save.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Vec<String>> {
        let mut result = None;
        let mut is_open = self.open;

        egui::Window::new("Playlist Editor")
            .id(egui::Id::new(format!(
                "playlist_editor_{}_{}",
                self.flow_id, self.block_id
            )))
            .open(&mut is_open)
            .default_width(600.0)
            .default_height(400.0)
            .resizable(true)
            .vscroll(false)
            .hscroll(false)
            .show(ctx, |ui| {
                const DIVIDER_WIDTH: f32 = 6.0;
                let left_width = self.browser_width_px.clamp(80.0, 800.0);

                // StripBuilder cells don't grow with children — no feedback loop.
                // clip(true) prevents content overflow from affecting the Resize container.
                egui_extras::StripBuilder::new(ui)
                    .size(egui_extras::Size::remainder())
                    .clip(true)
                    .vertical(|mut strip| {
                        strip.cell(|ui| {
                            egui_extras::StripBuilder::new(ui)
                                .size(egui_extras::Size::exact(left_width))
                                .size(egui_extras::Size::exact(DIVIDER_WIDTH))
                                .size(egui_extras::Size::remainder().at_least(120.0))
                                .clip(true)
                                .horizontal(|mut strip| {
                                    // Left pane — file browser
                                    strip.cell(|ui| {
                                        ui.heading("Server Media Files");
                                        self.show_browser_panel(ui);
                                    });

                                    // Draggable divider
                                    strip.cell(|ui| {
                                        let rect = ui.available_rect_before_wrap();
                                        let resp = ui.interact(
                                            rect,
                                            egui::Id::new("playlist_divider").with(&self.block_id),
                                            egui::Sense::drag(),
                                        );
                                        let color = if resp.hovered() || resp.dragged() {
                                            ui.ctx().set_cursor_icon(
                                                egui::CursorIcon::ResizeHorizontal,
                                            );
                                            Color32::from_gray(120)
                                        } else {
                                            Color32::from_gray(60)
                                        };
                                        ui.painter().line_segment(
                                            [rect.center_top(), rect.center_bottom()],
                                            egui::Stroke::new(1.0_f32, color),
                                        );
                                        if resp.dragged() {
                                            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                                                self.browser_width_px = (pos.x - rect.left()
                                                    + left_width / 2.0)
                                                    .clamp(80.0, 800.0);
                                            }
                                        }
                                    });

                                    // Right pane — playlist
                                    strip.cell(|ui| {
                                        ui.heading("Playlist");
                                        self.show_playlist_panel(ui, &mut result);
                                    });
                                });
                        });
                    });
            });

        // Sync open state back
        self.open = is_open;

        result
    }

    fn show_browser_panel(&mut self, ui: &mut Ui) {
        // Path display and navigation
        ui.horizontal(|ui| {
            // Up button
            if self.browser_parent.is_some()
                && ui
                    .button(egui_phosphor::regular::ARROW_BEND_UP_LEFT)
                    .on_hover_text("Go up")
                    .clicked()
            {
                self.browser_path = self.browser_parent.clone().unwrap_or_default();
                self.browser_needs_refresh = true;
            }
            // Current path
            let path_display = if self.browser_path.is_empty() {
                "media/".to_string()
            } else {
                format!("media/{}/", self.browser_path)
            };
            ui.label(path_display);

            // Refresh button
            if ui
                .button(egui_phosphor::regular::ARROWS_CLOCKWISE)
                .on_hover_text("Refresh")
                .clicked()
            {
                self.browser_needs_refresh = true;
            }
        });

        ui.separator();
        ui.label("Click a file to add it to the playlist.");

        // File list (scroll area LAST so it fills the remaining cell height)
        if self.browser_loading {
            ui.spinner();
            ui.label("Loading...");
        } else if self.browser_entries.is_empty() {
            ui.label("(empty folder or no media files)");
        } else {
            egui::ScrollArea::vertical()
                .id_salt("media_browser_scroll")
                .auto_shrink(false)
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    let mut nav_to_folder: Option<String> = None;
                    let mut add_file: Option<String> = None;

                    for entry in &self.browser_entries {
                        ui.horizontal(|ui| {
                            if entry.is_dir {
                                // Folder - click to navigate
                                let folder_btn = ui
                                    .button(format!(
                                        "{} {}",
                                        egui_phosphor::regular::FOLDER,
                                        entry.name
                                    ))
                                    .on_hover_text("Open folder");
                                if folder_btn.clicked() {
                                    nav_to_folder = Some(entry.path.clone());
                                }
                            } else {
                                // File - click to add to playlist
                                let size_str = format_file_size(entry.size);
                                let file_btn = ui
                                    .button(format!(
                                        "{} {}",
                                        egui_phosphor::regular::FILE_VIDEO,
                                        entry.name
                                    ))
                                    .on_hover_text(format!("Add to playlist ({})", size_str));
                                if file_btn.clicked() {
                                    add_file = Some(entry.path.clone());
                                }
                            }
                        });
                    }

                    // Handle navigation
                    if let Some(folder) = nav_to_folder {
                        self.browser_path = folder;
                        self.browser_needs_refresh = true;
                    }

                    // Handle file add (duplicates allowed — same file can appear multiple times)
                    if let Some(file_path) = add_file {
                        self.playlist.push(file_path);
                        self.dirty = true;
                    }
                });
        }
    }

    fn show_playlist_panel(&mut self, ui: &mut Ui, result: &mut Option<Vec<String>>) {
        // Action buttons BEFORE scroll area so they don't overflow the cell
        ui.horizontal(|ui| {
            if ui
                .button(format!(
                    "{} Save & Apply",
                    egui_phosphor::regular::FLOPPY_DISK
                ))
                .clicked()
            {
                *result = Some(self.playlist.clone());
                self.dirty = false;
            }

            if ui
                .button(format!("{} Clear All", egui_phosphor::regular::TRASH))
                .clicked()
            {
                self.playlist.clear();
                self.dirty = true;
            }

            if self.dirty {
                ui.label("*");
            }
        });
        ui.separator();

        if self.playlist.is_empty() {
            ui.label("(empty - click files on the left or enter path above)");
        }

        // Scrollable playlist (LAST so it fills the remaining cell height)
        egui::ScrollArea::vertical()
            .id_salt("playlist_scroll")
            .max_height(ui.available_height())
            .show(ui, |ui| {
                let mut to_remove = None;
                let mut to_move_up = None;
                let mut to_move_down = None;

                for (i, file) in self.playlist.iter().enumerate() {
                    let is_playing = self.current_playing_index == Some(i);
                    ui.horizontal(|ui| {
                        // Fixed-width left: playing indicator + index number
                        if is_playing {
                            ui.colored_label(Color32::GREEN, egui_phosphor::regular::PLAY);
                        }
                        ui.label(format!("{}.", i + 1));

                        // Remaining space: buttons on the right, filename fills the middle
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Buttons — added right-to-left (X is rightmost)
                            if ui
                                .button(egui_phosphor::regular::X)
                                .on_hover_text("Remove")
                                .clicked()
                            {
                                to_remove = Some(i);
                            }

                            let can_move_down = i < self.playlist.len() - 1;
                            if ui
                                .add_enabled(
                                    can_move_down,
                                    egui::Button::new(egui_phosphor::regular::ARROW_DOWN),
                                )
                                .on_hover_text("Move down")
                                .clicked()
                            {
                                to_move_down = Some(i);
                            }

                            let can_move_up = i > 0;
                            if ui
                                .add_enabled(
                                    can_move_up,
                                    egui::Button::new(egui_phosphor::regular::ARROW_UP),
                                )
                                .on_hover_text("Move up")
                                .clicked()
                            {
                                to_move_up = Some(i);
                            }

                            // Filename fills the remaining space with truncation
                            let display_name = std::path::Path::new(file)
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| file.clone());
                            let color = if is_playing {
                                Color32::GREEN
                            } else {
                                ui.style().visuals.text_color()
                            };
                            ui.add(
                                egui::Label::new(egui::RichText::new(&display_name).color(color))
                                    .truncate()
                                    .sense(egui::Sense::hover()),
                            )
                            .on_hover_text(file);
                        });
                    });
                }

                // Apply moves/removes
                if let Some(i) = to_remove {
                    self.playlist.remove(i);
                    self.dirty = true;
                }
                if let Some(i) = to_move_up {
                    self.playlist.swap(i, i - 1);
                    self.dirty = true;
                }
                if let Some(i) = to_move_down {
                    self.playlist.swap(i, i + 1);
                    self.dirty = true;
                }
            });
    }
}

/// Format file size for display.
fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
