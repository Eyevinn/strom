//! Media file browser page.

use egui::{Color32, Context, RichText, Ui};
use strom_types::api::{ListMediaResponse, MediaFileEntry};
use strom_types::media_download::{MediaDownloadJob, MediaDownloadState};

use crate::list_navigator::{list_navigator, ListItem};

/// Width of the download URL field and of a download's progress bar.
const DOWNLOAD_FIELD_WIDTH: f32 = 320.0;

/// Media page state.
pub struct MediaPage {
    /// Current directory path (relative to media root)
    pub current_path: String,
    /// Directory listing
    pub entries: Vec<MediaFileEntry>,
    /// Last fetch time
    pub last_fetch: instant::Instant,
    /// Whether we're currently loading
    pub loading: bool,
    /// Error message if any
    pub error: Option<String>,
    /// Success message if any
    pub success_message: Option<String>,
    /// Selected entry path
    pub selected_entry: Option<String>,
    /// Search filter
    pub search_filter: String,
    /// New folder dialog state
    pub show_new_folder_dialog: bool,
    /// New folder name input
    pub new_folder_name: String,
    /// Rename dialog state
    pub rename_target: Option<String>,
    /// Rename input
    pub rename_input: String,
    /// Delete confirmation
    pub delete_confirm: Option<String>,
    /// Request to focus the search box on next frame
    focus_search_requested: bool,
    /// Parent path for navigation
    parent_path: Option<String>,
    /// Whether initial fetch has been done
    initial_fetch_done: bool,
    /// Last clicked entry path (for double-click detection)
    last_click_path: Option<String>,
    /// Time of last click (for double-click detection)
    last_click_time: instant::Instant,
    /// URL typed into "Download from URL"
    download_url: String,
    /// Replace a file of the same name when downloading
    download_overwrite: bool,
    /// A download request is waiting for the server's answer
    download_pending: bool,
    /// Why the last download request was refused
    download_error: Option<String>,
    /// URL downloads seen in this session, oldest first
    downloads: Vec<MediaDownloadJob>,
}

impl MediaPage {
    pub fn new() -> Self {
        Self {
            current_path: String::new(),
            entries: Vec::new(),
            last_fetch: instant::Instant::now(),
            loading: false,
            error: None,
            success_message: None,
            selected_entry: None,
            search_filter: String::new(),
            show_new_folder_dialog: false,
            new_folder_name: String::new(),
            rename_target: None,
            rename_input: String::new(),
            delete_confirm: None,
            focus_search_requested: false,
            parent_path: None,
            initial_fetch_done: false,
            last_click_path: None,
            last_click_time: instant::Instant::now(),
            download_url: String::new(),
            download_overwrite: false,
            download_pending: false,
            download_error: None,
            downloads: Vec::new(),
        }
    }

    /// Apply a download's latest state (from the WebSocket or the server's
    /// list). Returns true when a download just finished into the folder
    /// being shown, so the listing should be refreshed.
    pub fn apply_download(&mut self, job: MediaDownloadJob) -> bool {
        let finished_here =
            job.state == MediaDownloadState::Done && job.directory == self.current_path;
        match self.downloads.iter_mut().find(|j| j.job_id == job.job_id) {
            Some(existing) => {
                // A late progress tick must not undo a final state.
                if !existing.state.is_finished() {
                    *existing = job;
                }
            }
            None => self.downloads.push(job),
        }
        finished_here
    }

    /// The server accepted a download request.
    pub fn download_started(&mut self, job: MediaDownloadJob) {
        self.download_pending = false;
        self.download_error = None;
        self.download_url.clear();
        // Events may already have reported a later state; keep that.
        if !self.downloads.iter().any(|j| j.job_id == job.job_id) {
            self.downloads.push(job);
        }
    }

    /// The server refused a download request.
    pub fn download_failed(&mut self, message: String) {
        self.download_pending = false;
        self.download_error = Some(message);
    }

    /// Request focus on the search box.
    pub fn focus_search(&mut self) {
        self.focus_search_requested = true;
    }

    /// Set the directory entries from API response.
    pub fn set_entries(&mut self, response: ListMediaResponse) {
        self.current_path = response.current_path;
        self.parent_path = response.parent_path;
        self.entries = response.entries;
        self.loading = false;
    }

    /// Render the media page.
    pub fn render(
        &mut self,
        ui: &mut Ui,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        // Initial fetch on first render
        if !self.initial_fetch_done && !self.loading {
            self.initial_fetch_done = true;
            self.refresh(api, ctx, tx);
            self.fetch_downloads(api, ctx, tx);
        }

        // Auto-refresh every 3 seconds
        if self.last_fetch.elapsed().as_secs() > 3 && !self.loading {
            self.refresh(api, ctx, tx);
        }

        // Handle dialogs
        self.render_new_folder_dialog(ui, api, ctx, tx);
        self.render_rename_dialog(ui, api, ctx, tx);
        self.render_delete_confirm_dialog(ui, api, ctx, tx);

        // Show messages
        if let Some(error) = &self.error {
            ui.colored_label(Color32::RED, format!("Error: {}", error));
        }
        if let Some(success) = &self.success_message {
            ui.colored_label(Color32::GREEN, success);
        }

        // Split view: file list on left, details on right
        egui::Panel::left("media_file_list")
            .default_size(400.0)
            .resizable(true)
            .show_inside(ui, |ui| {
                self.render_toolbar(ui, api, ctx, tx);
                ui.separator();
                self.render_download_bar(ui, api, ctx, tx);
                ui.separator();
                self.render_file_list(ui, api, ctx, tx);
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.render_details_panel(ui, api);
        });
    }

    fn render_toolbar(
        &mut self,
        ui: &mut Ui,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        // Breadcrumb navigation
        ui.horizontal(|ui| {
            ui.label("Path:");

            // Root button
            if ui.small_button("media").clicked() {
                self.navigate_to("", api, ctx, tx);
            }

            // Path segments - collect first to avoid borrow issues
            if !self.current_path.is_empty() {
                let segments: Vec<_> = self
                    .current_path
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .collect();
                let mut accumulated_path = String::new();
                let mut clicked_path: Option<String> = None;

                for segment in &segments {
                    ui.label("/");
                    if !accumulated_path.is_empty() {
                        accumulated_path.push('/');
                    }
                    accumulated_path.push_str(segment);
                    if ui.small_button(*segment).clicked() {
                        clicked_path = Some(accumulated_path.clone());
                    }
                }

                if let Some(path) = clicked_path {
                    self.navigate_to(&path, api, ctx, tx);
                }
            }
        });

        ui.add_space(4.0);

        // Action buttons
        ui.horizontal(|ui| {
            // Back button
            let can_go_back = self.parent_path.is_some();
            if ui
                .add_enabled(can_go_back, egui::Button::new("⬆ Up"))
                .clicked()
            {
                if let Some(parent) = self.parent_path.clone() {
                    self.navigate_to(&parent, api, ctx, tx);
                }
            }

            if ui
                .button(egui_phosphor::regular::ARROWS_CLOCKWISE)
                .clicked()
            {
                self.refresh(api, ctx, tx);
            }

            if ui
                .button(format!(
                    "{} New Folder",
                    egui_phosphor::regular::FOLDER_PLUS
                ))
                .clicked()
            {
                self.show_new_folder_dialog = true;
                self.new_folder_name.clear();
            }

            #[cfg(target_arch = "wasm32")]
            if ui
                .button(format!("{} Upload", egui_phosphor::regular::UPLOAD_SIMPLE))
                .clicked()
            {
                self.trigger_file_upload(api, ctx, tx);
            }
        });

        ui.add_space(4.0);

        // Search filter
        ui.horizontal(|ui| {
            ui.label("Filter:");
            let filter_id = egui::Id::new("media_search_filter");
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.search_filter)
                    .id(filter_id)
                    .desired_width(150.0),
            );
            if self.focus_search_requested {
                self.focus_search_requested = false;
                response.request_focus();
            }
            if !self.search_filter.is_empty()
                && ui
                    .small_button(egui_phosphor::regular::X)
                    .on_hover_text("Clear search")
                    .clicked()
            {
                self.search_filter.clear();
            }
        });
    }

    /// "Download from URL": a URL field that saves into the current folder,
    /// and one row per download with its progress or outcome.
    fn render_download_bar(
        &mut self,
        ui: &mut Ui,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let folder = format!("media/{}", self.current_path);
        ui.horizontal(|ui| {
            let can_start = !self.download_pending && !self.download_url.trim().is_empty();
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.download_url)
                    .hint_text("Download from URL: https://...")
                    .desired_width(DOWNLOAD_FIELD_WIDTH),
            );
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui
                .add_enabled(
                    can_start,
                    egui::Button::new(format!(
                        "{} Download",
                        egui_phosphor::regular::DOWNLOAD_SIMPLE
                    )),
                )
                .on_hover_text(format!("Save the file into {}", folder))
                .clicked();
            ui.checkbox(&mut self.download_overwrite, "Replace")
                .on_hover_text("Replace a file with the same name in this folder");
            if self.download_pending {
                ui.spinner();
            }
            if can_start && (clicked || enter) {
                self.start_download(api, ctx, tx);
            }
        });

        if let Some(error) = self.download_error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(Color32::RED, error);
                if ui
                    .small_button(egui_phosphor::regular::X)
                    .on_hover_text("Dismiss")
                    .clicked()
                {
                    self.download_error = None;
                }
            });
        }

        let mut cancel: Option<String> = None;
        let mut dismiss: Option<String> = None;
        for job in &self.downloads {
            ui.horizontal(|ui| {
                match job.state {
                    MediaDownloadState::Downloading => {
                        let (fraction, text) = match job.total {
                            Some(total) if total > 0 => {
                                let fraction = job.bytes as f32 / total as f32;
                                (
                                    fraction,
                                    format!(
                                        "{}  {} / {} ({:.0}%)",
                                        job.filename,
                                        format_size(job.bytes),
                                        format_size(total),
                                        fraction * 100.0
                                    ),
                                )
                            }
                            _ => (0.0, format!("{}  {}", job.filename, format_size(job.bytes))),
                        };
                        ui.add(
                            egui::ProgressBar::new(fraction)
                                .text(text)
                                .animate(job.total.is_none())
                                .desired_width(DOWNLOAD_FIELD_WIDTH),
                        )
                        .on_hover_text(&job.url);
                        if ui
                            .small_button(egui_phosphor::regular::X)
                            .on_hover_text("Cancel download")
                            .clicked()
                        {
                            cancel = Some(job.job_id.clone());
                        }
                    }
                    MediaDownloadState::Done => {
                        ui.colored_label(Color32::GREEN, egui_phosphor::regular::CHECK_CIRCLE);
                        ui.label(format!("{} ({})", job.path, format_size(job.bytes)))
                            .on_hover_text(&job.url);
                    }
                    MediaDownloadState::Failed => {
                        ui.colored_label(Color32::RED, egui_phosphor::regular::WARNING);
                        let error = job.error.as_deref().unwrap_or("failed");
                        ui.colored_label(Color32::RED, format!("{}: {}", job.filename, error))
                            .on_hover_text(&job.url);
                    }
                    MediaDownloadState::Cancelled => {
                        ui.colored_label(Color32::GRAY, egui_phosphor::regular::PROHIBIT);
                        ui.colored_label(Color32::GRAY, format!("{}: cancelled", job.filename));
                    }
                }
                if job.state.is_finished()
                    && ui
                        .small_button(egui_phosphor::regular::X)
                        .on_hover_text("Dismiss")
                        .clicked()
                {
                    dismiss = Some(job.job_id.clone());
                }
            });
        }
        if let Some(job_id) = cancel {
            self.cancel_download(&job_id, api, ctx, tx);
        }
        if let Some(job_id) = dismiss {
            self.downloads.retain(|j| j.job_id != job_id);
        }
    }

    fn render_file_list(
        &mut self,
        ui: &mut Ui,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let filter = self.search_filter.to_lowercase();

        if self.loading {
            ui.spinner();
            ui.label("Loading...");
            return;
        }

        if self.entries.is_empty() {
            ui.label("This directory is empty.");
            return;
        }

        // Build list items
        let items_data: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| filter.is_empty() || entry.name.to_lowercase().contains(&filter))
            .map(|entry| {
                let icon = if entry.is_directory { "📁" } else { "📄" };
                let size_str = if entry.is_directory {
                    String::new()
                } else {
                    format_size(entry.size)
                };
                let tag_color = if entry.is_directory {
                    Color32::from_rgb(255, 200, 100)
                } else {
                    Color32::from_rgb(150, 150, 150)
                };
                (
                    entry.path.clone(),
                    format!("{} {}", icon, entry.name),
                    entry.mime_type.clone().unwrap_or_default(),
                    size_str,
                    tag_color,
                    entry.is_directory,
                )
            })
            .collect();

        let result = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let items = items_data
                    .iter()
                    .map(|(path, name, mime, size, _color, _is_dir)| {
                        ListItem::new(path, name)
                            .with_secondary(mime.clone())
                            .with_right_text(size.clone())
                    });

                list_navigator(ui, "media_entries", items, self.selected_entry.as_deref())
            });

        // Handle selection and double-click
        if let Some(new_path) = result.inner.selected.clone() {
            // Check for double-click (same path clicked within 500ms)
            let is_double_click = self.last_click_path.as_ref() == Some(&new_path)
                && self.last_click_time.elapsed().as_millis() < 500;

            if is_double_click {
                // Double-click: navigate into directory if it's a folder
                if let Some((path, _, _, _, _, is_dir)) =
                    items_data.iter().find(|(p, _, _, _, _, _)| p == &new_path)
                {
                    if *is_dir {
                        let path = path.clone();
                        self.navigate_to(&path, api, ctx, tx);
                    }
                }
                self.last_click_path = None;
            } else {
                // Single click: select and record for potential double-click
                self.selected_entry = Some(new_path.clone());
                self.last_click_path = Some(new_path);
                self.last_click_time = instant::Instant::now();
            }
        }

        // Handle Enter key to navigate into directory
        let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
        if enter_pressed && result.inner.has_focus {
            if let Some(selected) = &self.selected_entry {
                if let Some((path, _, _, _, _, is_dir)) =
                    items_data.iter().find(|(p, _, _, _, _, _)| p == selected)
                {
                    if *is_dir {
                        let path = path.clone();
                        self.navigate_to(&path, api, ctx, tx);
                    }
                }
            }
        }
    }

    fn render_details_panel(&mut self, ui: &mut Ui, api: &crate::api::ApiClient) {
        ui.heading("File Details");
        ui.separator();

        let Some(selected_path) = &self.selected_entry else {
            ui.label("Select a file or folder to view details");
            return;
        };

        let Some(entry) = self.entries.iter().find(|e| &e.path == selected_path) else {
            ui.label("Entry not found");
            return;
        };

        // Clone to avoid borrow issues
        let entry = entry.clone();

        egui::Grid::new("file_details_grid")
            .num_columns(2)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                ui.label("Name:");
                ui.label(&entry.name);
                ui.end_row();

                ui.label("Type:");
                if entry.is_directory {
                    ui.label("Directory");
                } else {
                    ui.label(entry.mime_type.as_deref().unwrap_or("Unknown"));
                }
                ui.end_row();

                if !entry.is_directory {
                    ui.label("Size:");
                    ui.label(format_size(entry.size));
                    ui.end_row();
                }

                ui.label("Modified:");
                ui.label(format_timestamp(entry.modified));
                ui.end_row();

                ui.label("Path:");
                ui.label(&entry.path);
                ui.end_row();
            });

        ui.separator();

        // Action buttons
        ui.horizontal(|ui| {
            if !entry.is_directory {
                // Download link
                let download_url = api.get_media_download_url(&entry.path);
                ui.hyperlink_to(
                    format!("{} Download", egui_phosphor::regular::DOWNLOAD_SIMPLE),
                    download_url,
                );
            }

            if ui
                .button(format!("{} Rename", egui_phosphor::regular::PENCIL_SIMPLE))
                .clicked()
            {
                self.rename_target = Some(entry.path.clone());
                self.rename_input = entry.name.clone();
            }

            let delete_label = if entry.is_directory {
                format!("{} Delete Folder", egui_phosphor::regular::TRASH)
            } else {
                format!("{} Delete", egui_phosphor::regular::TRASH)
            };
            if ui.button(delete_label).clicked() {
                self.delete_confirm = Some(entry.path.clone());
            }
        });
    }

    fn render_new_folder_dialog(
        &mut self,
        ui: &mut Ui,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        if !self.show_new_folder_dialog {
            return;
        }

        egui::Window::new("New Folder")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label("Folder name:");
                ui.text_edit_singleline(&mut self.new_folder_name);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Create").clicked() {
                        let path = if self.current_path.is_empty() {
                            self.new_folder_name.clone()
                        } else {
                            format!("{}/{}", self.current_path, self.new_folder_name)
                        };
                        self.create_directory(&path, api, ctx, tx);
                        self.show_new_folder_dialog = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_new_folder_dialog = false;
                    }
                });
            });
    }

    fn render_rename_dialog(
        &mut self,
        ui: &mut Ui,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let Some(target) = &self.rename_target.clone() else {
            return;
        };

        egui::Window::new("Rename")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label("New name:");
                ui.text_edit_singleline(&mut self.rename_input);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() {
                        self.rename_entry(target, &self.rename_input.clone(), api, ctx, tx);
                        self.rename_target = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.rename_target = None;
                    }
                });
            });
    }

    fn render_delete_confirm_dialog(
        &mut self,
        ui: &mut Ui,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let Some(target) = &self.delete_confirm.clone() else {
            return;
        };

        let is_dir = self
            .entries
            .iter()
            .find(|e| &e.path == target)
            .map(|e| e.is_directory)
            .unwrap_or(false);

        egui::Window::new("Confirm Delete")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label(
                    RichText::new("Are you sure you want to delete this item?").color(Color32::RED),
                );
                ui.label(target);
                if is_dir {
                    ui.label(RichText::new("Note: Directory must be empty.").small());
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        self.delete_entry(target, is_dir, api, ctx, tx);
                        self.delete_confirm = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.delete_confirm = None;
                    }
                });
            });
    }

    /// Navigate to a directory.
    pub fn navigate_to(
        &mut self,
        path: &str,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        self.current_path = path.to_string();
        self.selected_entry = None;
        self.search_filter.clear();
        self.refresh(api, ctx, tx);
    }

    /// Refresh the directory listing.
    pub fn refresh(
        &mut self,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        self.loading = true;
        self.last_fetch = instant::Instant::now();
        self.error = None;
        self.success_message = None;

        let api = api.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();
        let path = self.current_path.clone();

        crate::app::spawn_task(async move {
            match api.list_media(&path).await {
                Ok(listing) => {
                    let _ = tx.send(crate::state::AppMessage::MediaListLoaded(listing));
                    ctx.request_repaint();
                }
                Err(e) => {
                    let _ = tx.send(crate::state::AppMessage::MediaError(e.to_string()));
                    ctx.request_repaint();
                }
            }
        });
    }

    /// Create a directory.
    fn create_directory(
        &mut self,
        path: &str,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let api = api.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();
        let path = path.to_string();

        crate::app::spawn_task(async move {
            match api.create_media_directory(&path).await {
                Ok(result) => {
                    let _ = tx.send(crate::state::AppMessage::MediaSuccess(result.message));
                    let _ = tx.send(crate::state::AppMessage::MediaRefresh);
                    ctx.request_repaint();
                }
                Err(e) => {
                    let _ = tx.send(crate::state::AppMessage::MediaError(e.to_string()));
                    ctx.request_repaint();
                }
            }
        });
    }

    /// Rename an entry.
    fn rename_entry(
        &mut self,
        old_path: &str,
        new_name: &str,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let api = api.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();
        let old_path = old_path.to_string();
        let new_name = new_name.to_string();

        crate::app::spawn_task(async move {
            match api.rename_media(&old_path, &new_name).await {
                Ok(result) => {
                    let _ = tx.send(crate::state::AppMessage::MediaSuccess(result.message));
                    let _ = tx.send(crate::state::AppMessage::MediaRefresh);
                    ctx.request_repaint();
                }
                Err(e) => {
                    let _ = tx.send(crate::state::AppMessage::MediaError(e.to_string()));
                    ctx.request_repaint();
                }
            }
        });
    }

    /// Delete an entry.
    fn delete_entry(
        &mut self,
        path: &str,
        is_dir: bool,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let api = api.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();
        let path = path.to_string();

        crate::app::spawn_task(async move {
            let result = if is_dir {
                api.delete_media_directory(&path).await
            } else {
                api.delete_media_file(&path).await
            };

            match result {
                Ok(res) => {
                    let _ = tx.send(crate::state::AppMessage::MediaSuccess(res.message));
                    let _ = tx.send(crate::state::AppMessage::MediaRefresh);
                    ctx.request_repaint();
                }
                Err(e) => {
                    let _ = tx.send(crate::state::AppMessage::MediaError(e.to_string()));
                    ctx.request_repaint();
                }
            }
        });
    }

    /// Ask the server to download the typed URL into the current folder.
    fn start_download(
        &mut self,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        self.download_pending = true;
        self.download_error = None;

        let api = api.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();
        let url = self.download_url.trim().to_string();
        let path = self.current_path.clone();
        let overwrite = self.download_overwrite;

        crate::app::spawn_task(async move {
            let message = match api.download_media_url(&url, &path, overwrite).await {
                Ok(job) => crate::state::AppMessage::MediaDownloadStarted(job),
                Err(crate::api::ApiError::Http(_, message)) => {
                    crate::state::AppMessage::MediaDownloadFailed(message)
                }
                Err(e) => crate::state::AppMessage::MediaDownloadFailed(e.to_string()),
            };
            let _ = tx.send(message);
            ctx.request_repaint();
        });
    }

    /// Ask the server to stop a running download.
    fn cancel_download(
        &self,
        job_id: &str,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let api = api.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();
        let job_id = job_id.to_string();

        crate::app::spawn_task(async move {
            if let Err(e) = api.cancel_media_download(&job_id).await {
                let _ = tx.send(crate::state::AppMessage::MediaDownloadFailed(e.to_string()));
                ctx.request_repaint();
            }
        });
    }

    /// Pick up downloads that are still running (after a page reload).
    fn fetch_downloads(
        &self,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let api = api.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();

        crate::app::spawn_task(async move {
            match api.list_media_downloads().await {
                Ok(list) => {
                    let _ = tx.send(crate::state::AppMessage::MediaDownloadsLoaded(
                        list.downloads,
                    ));
                    ctx.request_repaint();
                }
                Err(e) => tracing::debug!("Could not list media downloads: {}", e),
            }
        });
    }

    /// Trigger file upload via browser file picker (WASM only).
    #[cfg(target_arch = "wasm32")]
    fn trigger_file_upload(
        &self,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        use wasm_bindgen::prelude::*;
        use wasm_bindgen::JsCast;
        use web_sys::{FileReader, HtmlInputElement};

        let document = web_sys::window()
            .and_then(|w| w.document())
            .expect("No document");

        // Create hidden file input
        let input: HtmlInputElement = document
            .create_element("input")
            .expect("Failed to create input")
            .dyn_into()
            .expect("Not an input element");

        input.set_type("file");
        input.set_attribute("multiple", "").ok();
        input.style().set_property("display", "none").ok();

        // Add to document temporarily
        document.body().unwrap().append_child(&input).ok();

        let api = api.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();
        let current_path = self.current_path.clone();

        // Set up change handler
        let input_clone = input.clone();
        let closure = Closure::wrap(Box::new(move |_event: web_sys::Event| {
            let files = input_clone.files();
            if let Some(files) = files {
                for i in 0..files.length() {
                    if let Some(file) = files.get(i) {
                        let api = api.clone();
                        let ctx = ctx.clone();
                        let tx = tx.clone();
                        let path = current_path.clone();
                        let filename = file.name();

                        // Read file content
                        let reader = FileReader::new().expect("Failed to create FileReader");
                        let reader_clone = reader.clone();

                        let onload = Closure::wrap(Box::new(move |_: web_sys::Event| {
                            if let Ok(result) = reader_clone.result() {
                                if let Some(array_buffer) = result.dyn_ref::<js_sys::ArrayBuffer>()
                                {
                                    let uint8_array = js_sys::Uint8Array::new(array_buffer);
                                    let data = uint8_array.to_vec();

                                    let api = api.clone();
                                    let ctx = ctx.clone();
                                    let tx = tx.clone();
                                    let path = path.clone();
                                    let filename = filename.clone();

                                    wasm_bindgen_futures::spawn_local(async move {
                                        match api.upload_media(&path, &filename, data).await {
                                            Ok(result) => {
                                                let _ = tx.send(
                                                    crate::state::AppMessage::MediaSuccess(
                                                        result.message,
                                                    ),
                                                );
                                                let _ =
                                                    tx.send(crate::state::AppMessage::MediaRefresh);
                                                ctx.request_repaint();
                                            }
                                            Err(e) => {
                                                let _ =
                                                    tx.send(crate::state::AppMessage::MediaError(
                                                        e.to_string(),
                                                    ));
                                                ctx.request_repaint();
                                            }
                                        }
                                    });
                                }
                            }
                        }) as Box<dyn FnMut(_)>);

                        reader.set_onload(Some(onload.as_ref().unchecked_ref()));
                        onload.forget();

                        reader.read_as_array_buffer(&file).ok();
                    }
                }
            }
        }) as Box<dyn FnMut(_)>);

        input.set_onchange(Some(closure.as_ref().unchecked_ref()));
        closure.forget();

        // Trigger file picker
        input.click();

        // Clean up (will be done after selection)
        // Note: In a real app, we'd want to remove the input element after use
    }
}

impl Default for MediaPage {
    fn default() -> Self {
        Self::new()
    }
}

/// Format a file size in human-readable form.
fn format_size(bytes: u64) -> String {
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

/// Get current Unix timestamp in seconds.
fn current_timestamp_secs() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        // js_sys::Date::now() returns milliseconds since epoch
        (js_sys::Date::now() / 1000.0) as u64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Native fallback (only used in tests, not production WASM build)
        #[allow(clippy::disallowed_types)]
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// Format a Unix timestamp as a human-readable string.
fn format_timestamp(timestamp: u64) -> String {
    // Simple relative time formatting
    let now = current_timestamp_secs();

    if timestamp == 0 {
        return "Unknown".to_string();
    }

    let diff = now.saturating_sub(timestamp);

    if diff < 60 {
        "Just now".to_string()
    } else if diff < 3600 {
        format!("{} min ago", diff / 60)
    } else if diff < 86400 {
        format!("{} hours ago", diff / 3600)
    } else {
        format!("{} days ago", diff / 86400)
    }
}
