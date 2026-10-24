#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Window that lists what is directly inside the usual desktop folders.
//!
//! Each folder is read once, on its own thread. Those threads send the names
//! through a channel. [`FolderApp::logic`] reads the channel, and [`FolderApp::ui`]
//! draws one section per folder.

mod copy;
mod format;
mod scan;
mod size;

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Folders")
            .with_inner_size([860.0, 640.0])
            .with_min_inner_size([680.0, 320.0])
            .with_icon(egui::IconData::default()),
        persist_window: false,
        ..Default::default()
    };

    eframe::run_native(
        "Folders",
        options,
        Box::new(|cc| Ok(Box::new(FolderApp::new(cc)))),
    )
}

struct FolderApp {
    ctx: egui::Context,
    rows: Vec<FolderSection>,
    events: mpsc::Receiver<ScanEvent>,
    copy_events: mpsc::Receiver<CopyEvent>,
    copy_tx: mpsc::Sender<CopyEvent>,
    scanning: bool,
    cleared_icon: bool,
    export_dialog: Option<ExportDialog>,
    anchor: Option<(usize, usize)>,
    drag_origin: Option<(usize, usize)>,
    scroll: SmoothScroll,
    show_hidden: bool,
}

struct FolderSection {
    name: &'static str,
    path: Option<PathBuf>,
    state: SectionState,
    children: Vec<scan::Child>,
    copy_state: CopyState,
    /// Set once the search index has answered. `None` inside means the index
    /// had no sizes, so the row adds up the files sitting directly inside.
    indexed_bytes: Option<u64>,
    size_ready: bool,
}

enum CopyState {
    Idle,
    Copying { copied: u64, total: u64 },
    Copied(PathBuf),
    Failed(String),
}

enum CopyEvent {
    Progress {
        index: usize,
        copied: u64,
        total: u64,
    },
    Finished {
        index: usize,
        destination: PathBuf,
    },
    Failed {
        index: usize,
        message: String,
    },
}

struct ProgressReporter {
    tx: mpsc::Sender<CopyEvent>,
    ctx: egui::Context,
    index: usize,
    last_send: std::time::Instant,
    last_copied: u64,
}

impl ProgressReporter {
    fn new(tx: mpsc::Sender<CopyEvent>, ctx: egui::Context, index: usize) -> Self {
        Self {
            tx,
            ctx,
            index,
            last_send: std::time::Instant::now() - std::time::Duration::from_secs(1),
            last_copied: u64::MAX,
        }
    }

    fn report(&mut self, copied: u64, total: u64) {
        let copied = if total == 0 { 0 } else { copied.min(total) };
        let done = total > 0 && copied >= total;
        let now = std::time::Instant::now();
        if !done
            && copied == self.last_copied
            && now.duration_since(self.last_send) < std::time::Duration::from_millis(80)
        {
            return;
        }
        if !done && now.duration_since(self.last_send) < std::time::Duration::from_millis(80) {
            return;
        }
        self.last_send = now;
        self.last_copied = copied;
        let _ = self.tx.send(CopyEvent::Progress {
            index: self.index,
            copied,
            total,
        });
        self.ctx.request_repaint();
    }
}

struct ExportDialog {
    open: bool,
    destination: PathBuf,
    /// `true` when the list is selected files and folders, not the whole-folder list.
    item_export: bool,
    /// One entry per main folder. `true` means it is still in the export.
    folders: Vec<bool>,
    /// Selected files and folders, with any that would nest already removed.
    items: Vec<(usize, &'static str, Vec<scan::Child>)>,
    removed: Vec<String>,
}

#[derive(Debug)]
enum SectionState {
    Waiting,
    Scanning,
    Ready,
    Missing,
    Failed(String),
}

enum ScanEvent {
    Finished {
        index: usize,
        children: Vec<scan::Child>,
    },
    Failed {
        index: usize,
        message: String,
    },
    Sized {
        index: usize,
        bytes: Option<u64>,
    },
}

impl FolderApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (copy_tx, copy_events) = mpsc::channel();
        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            rows: default_sections(),
            events: disconnected_channel(),
            copy_events,
            copy_tx,
            scanning: false,
            cleared_icon: false,
            export_dialog: None,
            anchor: None,
            drag_origin: None,
            scroll: SmoothScroll::default(),
            show_hidden: false,
        };
        app.spawn_scans();
        app
    }

    fn spawn_scans(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.events = rx;
        self.scanning = false;
        let mut sized_paths = Vec::new();

        for (index, section) in self.rows.iter_mut().enumerate() {
            section.children.clear();
            section.indexed_bytes = None;
            section.size_ready = false;

            let Some(path) = section.path.clone() else {
                section.state = SectionState::Missing;
                continue;
            };

            section.state = SectionState::Scanning;
            self.scanning = true;
            sized_paths.push((index, path.clone()));

            let tx = tx.clone();
            let ctx = self.ctx.clone();
            thread::spawn(move || {
                let event = match scan::list_folder(&path) {
                    Ok(children) => ScanEvent::Finished { index, children },
                    Err(err) => ScanEvent::Failed {
                        index,
                        message: err.to_string(),
                    },
                };
                let _ = tx.send(event);
                ctx.request_repaint();
            });
        }

        if !sized_paths.is_empty() {
            let tx = tx.clone();
            let ctx = self.ctx.clone();
            thread::spawn(move || {
                let paths: Vec<_> = sized_paths.iter().map(|(_, path)| path).collect();
                let sizes = size::indexed_sizes(&paths);
                for ((index, _), bytes) in sized_paths.into_iter().zip(sizes) {
                    let _ = tx.send(ScanEvent::Sized { index, bytes });
                }
                ctx.request_repaint();
            });
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            if let Some(index) = apply_event(&mut self.rows, event) {
                self.clear_selection_marks(index);
            }
        }
        while let Ok(event) = self.copy_events.try_recv() {
            apply_copy_event(&mut self.rows, event);
        }
        self.scanning = self
            .rows
            .iter()
            .any(|section| matches!(section.state, SectionState::Scanning));
    }

    fn start_exports(&mut self, indexes: Vec<usize>, destination_root: PathBuf) {
        let pending: Vec<_> = indexes
            .into_iter()
            .filter(|index| {
                self.rows[*index].path.is_some()
                    && !matches!(self.rows[*index].copy_state, CopyState::Copying { .. })
                    && !self.rows[*index].path.as_ref().is_some_and(|path| {
                        copy::backup_would_nest(path, &destination_root, self.rows[*index].name)
                    })
            })
            .collect();
        if pending.is_empty() {
            return;
        }

        for index in pending {
            let source = self.rows[index].path.clone().unwrap();
            let name = self.rows[index].name;
            self.rows[index].copy_state = CopyState::Copying {
                copied: 0,
                total: 0,
            };
            let tx = self.copy_tx.clone();
            let ctx = self.ctx.clone();
            let destination_root = destination_root.clone();
            thread::spawn(move || {
                let mut reporter = ProgressReporter::new(tx.clone(), ctx.clone(), index);
                let event = match copy::copy_folder(
                    &source,
                    &destination_root,
                    name,
                    &mut |copied, total| {
                        reporter.report(copied, total);
                    },
                ) {
                    Ok(destination) => CopyEvent::Finished { index, destination },
                    Err(err) => CopyEvent::Failed {
                        index,
                        message: err.to_string(),
                    },
                };
                let _ = tx.send(event);
                ctx.request_repaint();
            });
        }
    }

    fn start_selected_exports(&mut self) {
        let jobs: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, section)| {
                if matches!(section.copy_state, CopyState::Copying { .. }) {
                    return None;
                }
                let items: Vec<_> = section
                    .children
                    .iter()
                    .filter(|child| child.selected && listed(child, self.show_hidden))
                    .cloned()
                    .collect();
                if items.is_empty() {
                    None
                } else {
                    Some((index, section.name, items))
                }
            })
            .collect();
        if jobs.is_empty() {
            return;
        }

        let Some(destination_root) = rfd::FileDialog::new()
            .set_title("Copy selected items to")
            .pick_folder()
        else {
            return;
        };

        let (jobs, removed) = without_nesting_items(jobs, &destination_root);
        if !removed.is_empty() {
            self.export_dialog = Some(ExportDialog {
                open: true,
                destination: destination_root,
                item_export: true,
                folders: Vec::new(),
                items: jobs,
                removed,
            });
            return;
        }

        self.launch_item_exports(jobs, destination_root);
    }

    fn launch_item_exports(
        &mut self,
        jobs: Vec<(usize, &'static str, Vec<scan::Child>)>,
        destination_root: PathBuf,
    ) {
        for (index, name, items) in jobs {
            self.rows[index].copy_state = CopyState::Copying {
                copied: 0,
                total: 0,
            };
            let tx = self.copy_tx.clone();
            let ctx = self.ctx.clone();
            let destination_root = destination_root.clone();
            thread::spawn(move || {
                let mut reporter = ProgressReporter::new(tx.clone(), ctx.clone(), index);
                let event =
                    match copy::copy_items(name, &destination_root, &items, &mut |copied, total| {
                        reporter.report(copied, total);
                    }) {
                        Ok(destination) => CopyEvent::Finished { index, destination },
                        Err(err) => CopyEvent::Failed {
                            index,
                            message: err.to_string(),
                        },
                    };
                let _ = tx.send(event);
                ctx.request_repaint();
            });
        }
    }

    fn clear_selection_marks(&mut self, index: usize) {
        if self.anchor.is_some_and(|(section, _)| section == index) {
            self.anchor = None;
        }
        if self
            .drag_origin
            .is_some_and(|(section, _)| section == index)
        {
            self.drag_origin = None;
        }
    }

    fn copy_progress(&self) -> Option<(u64, u64)> {
        let mut copied: u64 = 0;
        let mut total: u64 = 0;
        let mut any = false;
        for section in &self.rows {
            if let CopyState::Copying {
                copied: done,
                total: job,
            } = section.copy_state
            {
                any = true;
                copied = copied.saturating_add(done);
                total = total.saturating_add(job);
            }
        }
        any.then_some((copied, total))
    }

    fn any_child_selected(&self) -> bool {
        self.rows.iter().any(|section| {
            section
                .children
                .iter()
                .any(|child| child.selected && listed(child, self.show_hidden))
        })
    }

    fn known_size(&self) -> Option<u64> {
        let mut total: u64 = 0;
        let mut any = false;
        for section in &self.rows {
            if let Some(bytes) = section.shown_bytes() {
                total = total.saturating_add(bytes);
                any = true;
            }
        }
        any.then_some(total)
    }

    fn child_counts(&self) -> (usize, usize) {
        let mut folders = 0;
        let mut files = 0;
        for section in &self.rows {
            for child in section
                .children
                .iter()
                .filter(|child| listed(child, self.show_hidden))
            {
                match child.kind {
                    scan::ChildKind::Folder => folders += 1,
                    scan::ChildKind::File => files += 1,
                }
            }
        }
        (folders, files)
    }

    /// Destination first, then the folder list with anything that would nest removed.
    fn open_folder_export(&mut self) {
        let Some(destination) = rfd::FileDialog::new()
            .set_title("Copy folders to")
            .pick_folder()
        else {
            return;
        };

        let mut removed = Vec::new();
        let folders = self
            .rows
            .iter()
            .map(|section| {
                let Some(path) = section.path.as_ref() else {
                    return false;
                };
                if copy::backup_would_nest(path, &destination, section.name) {
                    removed.push(section.name.to_string());
                    false
                } else if matches!(section.copy_state, CopyState::Copying { .. }) {
                    false
                } else {
                    true
                }
            })
            .collect();
        self.export_dialog = Some(ExportDialog {
            open: true,
            destination,
            item_export: false,
            folders,
            items: Vec::new(),
            removed,
        });
    }
}

impl FolderSection {
    /// Indexed total when Windows has one. Otherwise the files directly inside
    /// this folder, once the index has answered and the names are listed.
    fn shown_bytes(&self) -> Option<u64> {
        if !self.size_ready {
            return None;
        }
        if let Some(bytes) = self.indexed_bytes {
            return Some(bytes);
        }
        if matches!(self.state, SectionState::Ready) {
            return Some(self.children.iter().map(|child| child.bytes).sum());
        }
        None
    }
}

fn apply_event(rows: &mut [FolderSection], event: ScanEvent) -> Option<usize> {
    match event {
        ScanEvent::Finished { index, children } => {
            let Some(section) = rows.get_mut(index) else {
                return None;
            };
            section.children = children;
            section.state = SectionState::Ready;
            Some(index)
        }
        ScanEvent::Failed { index, message } => {
            let Some(section) = rows.get_mut(index) else {
                return None;
            };
            section.children.clear();
            section.state = SectionState::Failed(message);
            Some(index)
        }
        ScanEvent::Sized { index, bytes } => {
            let Some(section) = rows.get_mut(index) else {
                return None;
            };
            section.indexed_bytes = bytes;
            section.size_ready = true;
            None
        }
    }
}

fn apply_copy_event(rows: &mut [FolderSection], event: CopyEvent) {
    match event {
        CopyEvent::Progress {
            index,
            copied,
            total,
        } => {
            let Some(section) = rows.get_mut(index) else {
                return;
            };
            if matches!(section.copy_state, CopyState::Copying { .. }) {
                section.copy_state = CopyState::Copying { copied, total };
            }
        }
        CopyEvent::Finished { index, destination } => {
            let Some(section) = rows.get_mut(index) else {
                return;
            };
            section.copy_state = CopyState::Copied(destination);
        }
        CopyEvent::Failed { index, message } => {
            let Some(section) = rows.get_mut(index) else {
                return;
            };
            section.copy_state = CopyState::Failed(message);
        }
    }
}

impl eframe::App for FolderApp {
    fn logic(&mut self, _ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.drain_events();
        if !self.cleared_icon {
            self.cleared_icon = clear_window_icon(frame);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let scanning = self.scanning;
        let (folders, files) = self.child_counts();
        let summary = summary_line(self.known_size(), folders, files, scanning);
        let mut rescan = false;
        let mut open_export = false;
        let copy_progress = self.copy_progress();
        let copying = copy_progress.is_some();
        let can_export = !copying && self.rows.iter().any(|section| section.path.is_some());

        egui::Panel::top("header").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(summary);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!scanning, egui::Button::new("Scan again"))
                        .clicked()
                    {
                        rescan = true;
                    }
                    if ui
                        .add_enabled(can_export, egui::Button::new("Export"))
                        .clicked()
                    {
                        open_export = true;
                    }
                    if ui.checkbox(&mut self.show_hidden, "Show hidden").changed()
                        && !self.show_hidden
                    {
                        for section in &mut self.rows {
                            for child in &mut section.children {
                                if child.hidden {
                                    child.selected = false;
                                }
                            }
                        }
                    }
                });
            });
            if let Some((copied, total)) = copy_progress {
                ui.add_space(8.0);
                ui.add(copy_progress_bar(copied, total));
            }
        });

        let mut anchor = self.anchor;
        let mut drag_origin = self.drag_origin;
        let mut scroll = self.scroll;
        let show_hidden = self.show_hidden;
        egui::CentralPanel::default().show(ui, |ui| {
            scroll.advance(ui);
            let output = egui::ScrollArea::vertical()
                .id_salt("folder_list")
                .auto_shrink([false, false])
                .scroll_source(egui::containers::scroll_area::ScrollSource {
                    scroll_bar: true,
                    drag: egui::containers::scroll_area::DragScroll::OnTouch,
                    mouse_wheel: false,
                })
                .vertical_scroll_offset(scroll.current)
                .show(ui, |ui| {
                    for (index, section) in self.rows.iter_mut().enumerate() {
                        folder_section(
                            ui,
                            index,
                            section,
                            show_hidden,
                            &mut anchor,
                            &mut drag_origin,
                        );
                        ui.add_space(8.0);
                    }
                });
            scroll.follow(ui, &output);
            let dragging = ui.input(|input| {
                input.modifiers.shift && input.pointer.button_down(egui::PointerButton::Primary)
            });
            if dragging {
                ui.ctx().request_repaint();
            } else {
                drag_origin = None;
            }
        });
        self.anchor = anchor;
        self.drag_origin = drag_origin;
        self.scroll = scroll;

        if open_export && self.any_child_selected() {
            self.start_selected_exports();
        } else if open_export {
            self.open_folder_export();
        }

        let mut copy_folders = None;
        let mut copy_items = None;
        if let Some(dialog) = self.export_dialog.as_mut() {
            let action = export_dialog(ui.ctx(), dialog, &self.rows);
            if !dialog.open {
                self.export_dialog = None;
            } else if let Some(action) = action {
                match action {
                    ExportAction::CopyFolders(indexes) => {
                        copy_folders = Some((indexes, dialog.destination.clone()));
                    }
                    ExportAction::CopyItems => {
                        copy_items = Some((dialog.items.clone(), dialog.destination.clone()));
                    }
                }
            }
        }

        if rescan {
            self.spawn_scans();
        }
        if copy_folders.is_some() || copy_items.is_some() {
            self.export_dialog = None;
        }
        if let Some((indexes, destination)) = copy_folders {
            self.start_exports(indexes, destination);
        }
        if let Some((jobs, destination)) = copy_items {
            self.launch_item_exports(jobs, destination);
        }
    }
}

/// Glides the folder list toward each wheel notch instead of jumping there.
#[derive(Clone, Copy)]
struct SmoothScroll {
    current: f32,
    target: f32,
    viewport: egui::Rect,
}

impl Default for SmoothScroll {
    fn default() -> Self {
        Self {
            current: 0.0,
            target: 0.0,
            viewport: egui::Rect::NOTHING,
        }
    }
}

impl SmoothScroll {
    fn advance(&mut self, ui: &egui::Ui) {
        let area = if self.viewport == egui::Rect::NOTHING {
            ui.max_rect()
        } else {
            self.viewport
        };
        let pointer = ui.input(|input| input.pointer.hover_pos());
        let over_list = pointer.is_some_and(|pos| area.contains(pos));
        if over_list {
            let height = area.height();
            let wheel = ui.input(|input| {
                if input.modifiers.shift {
                    return 0.0;
                }
                let mut points = 0.0;
                for event in &input.events {
                    let egui::Event::MouseWheel {
                        unit, delta, phase, ..
                    } = event
                    else {
                        continue;
                    };
                    if matches!(phase, egui::TouchPhase::End | egui::TouchPhase::Cancel) {
                        continue;
                    }
                    let distance = match unit {
                        egui::MouseWheelUnit::Point => delta.y,
                        egui::MouseWheelUnit::Line => delta.y * 64.0,
                        egui::MouseWheelUnit::Page => delta.y * height,
                    };
                    // A trackpad already sends a stream of tiny steps.
                    let already_smooth =
                        matches!(unit, egui::MouseWheelUnit::Point) && delta.length() < 8.0;
                    if already_smooth {
                        self.current -= distance;
                    }
                    points += distance;
                }
                points
            });
            self.target -= wheel;
        }

        let dt = ui.input(|input| input.stable_dt).clamp(0.0, 0.1);
        let blend = 1.0 - (-dt / 0.15).exp();
        self.current += (self.target - self.current) * blend;
        if (self.target - self.current).abs() < 0.4 {
            self.current = self.target;
        } else {
            ui.ctx().request_repaint();
        }
    }

    fn follow(
        &mut self,
        ui: &egui::Ui,
        output: &egui::containers::scroll_area::ScrollAreaOutput<()>,
    ) {
        let shown = output.state.clamped_offset().y;
        if (shown - self.current).abs() > 1.0 {
            self.current = shown;
            self.target = shown;
        }
        let max_offset = (output.content_size.y - output.inner_rect.height()).max(0.0);
        self.target = self.target.clamp(0.0, max_offset);
        self.current = self.current.clamp(0.0, max_offset);
        self.viewport = output.inner_rect;
        if (self.target - self.current).abs() >= 0.4 {
            ui.ctx().request_repaint();
        }
    }
}

fn summary_line(bytes: Option<u64>, folders: usize, files: usize, scanning: bool) -> String {
    let mut parts = Vec::new();
    if let Some(bytes) = bytes {
        parts.push(approximate_size(bytes));
    }
    parts.push(count_label(folders, "folder", "folders"));
    parts.push(count_label(files, "file", "files"));
    if scanning {
        parts.push("scanning".to_string());
    }
    parts.join("  ·  ")
}

fn copy_progress_bar(copied: u64, total: u64) -> egui::ProgressBar {
    let fraction = if total == 0 {
        0.0
    } else {
        (copied as f32 / total as f32).clamp(0.0, 1.0)
    };
    let label = if total == 0 {
        "Copying…".to_string()
    } else {
        let left = total.saturating_sub(copied);
        format!(
            "{} of {}   ·   {} left",
            format::format_bytes(copied),
            format::format_bytes(total),
            format::format_bytes(left)
        )
    };
    egui::ProgressBar::new(fraction).text(label)
}

fn approximate_size(bytes: u64) -> String {
    format!("~{}", format::format_bytes(bytes))
}

fn count_label(count: usize, one: &str, many: &str) -> String {
    let word = if count == 1 { one } else { many };
    format!("{} {word}", format::format_count(count as u64))
}

fn listed(child: &scan::Child, show_hidden: bool) -> bool {
    show_hidden || !child.hidden
}

fn folder_section(
    ui: &mut egui::Ui,
    section_index: usize,
    section: &mut FolderSection,
    show_hidden: bool,
    anchor: &mut Option<(usize, usize)>,
    drag_origin: &mut Option<(usize, usize)>,
) {
    let frame = egui::Frame::NONE
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(12.0)
        .corner_radius(8.0);
    frame.show(ui, |ui| {
        let can_expand = matches!(section.state, SectionState::Ready)
            && section
                .children
                .iter()
                .any(|child| listed(child, show_hidden));
        if can_expand {
            let id = ui.make_persistent_id(section.name);
            let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                id,
                false,
            );
            let status = section_status(section, show_hidden);
            let name = section.name;
            let header = ui.vertical(|ui| {
                ui.set_min_width(ui.available_width());
                let mut name_clicked = false;
                ui.horizontal(|ui| {
                    state.show_toggle_button(ui, egui::collapsing_header::paint_default_icon);
                    select_all_checkbox(ui, &mut section.children, show_hidden);
                    let title = egui::RichText::new(name).strong().size(17.0);
                    if ui.add(egui::Button::new(title).frame(false)).clicked() {
                        name_clicked = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(&status).monospace());
                    });
                });
                section_path(ui, section);
                name_clicked
            });
            if header.inner {
                state.toggle(ui);
            }
            state.show_body_indented(&header.response, ui, |ui| {
                ui.add_space(6.0);
                let shift = ui.input(|input| input.modifiers.shift);
                let mut hits = Vec::new();
                for (child_index, child) in section.children.iter_mut().enumerate() {
                    if !listed(child, show_hidden) {
                        continue;
                    }
                    hits.push((child_index, child_row(ui, child)));
                }
                let rects: Vec<_> = hits.iter().map(|(_, hit)| hit.rect).collect();
                let shown: Vec<_> = hits.iter().map(|(index, _)| *index).collect();
                for (child_index, hit) in hits {
                    if hit.checkbox_clicked {
                        *anchor = Some((section_index, child_index));
                    } else if hit.clicked {
                        if shift {
                            let from = anchor
                                .filter(|(section, _)| *section == section_index)
                                .map(|(_, index)| index)
                                .unwrap_or(child_index);
                            select_range(&mut section.children, from, child_index, show_hidden);
                        } else {
                            select_only(&mut section.children, child_index, show_hidden);
                            *anchor = Some((section_index, child_index));
                        }
                    }
                    if hit.drag_started && shift {
                        *drag_origin = Some((section_index, child_index));
                        *anchor = Some((section_index, child_index));
                    }
                }
                apply_drag(
                    ui,
                    section_index,
                    &mut section.children,
                    show_hidden,
                    drag_origin,
                    &shown,
                    &rects,
                );
            });
        } else {
            section_heading(ui, section, show_hidden);
            section_path(ui, section);
        }
    });
}

fn section_heading(ui: &mut egui::Ui, section: &FolderSection, show_hidden: bool) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(section.name).strong().size(17.0));
        section_status_label(ui, section, show_hidden);
    });
}

fn section_status_label(ui: &mut egui::Ui, section: &FolderSection, show_hidden: bool) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(egui::RichText::new(section_status(section, show_hidden)).monospace());
    });
}

enum ExportAction {
    CopyFolders(Vec<usize>),
    CopyItems,
}

fn export_dialog(
    ctx: &egui::Context,
    dialog: &mut ExportDialog,
    rows: &[FolderSection],
) -> Option<ExportAction> {
    let mut action = None;
    let response = egui::Window::new("Export")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .open(&mut dialog.open)
        .show(ctx, |ui| {
            ui.label(format!("Copy to {}", dialog.destination.display()));
            if !dialog.removed.is_empty() {
                ui.add_space(8.0);
                ui.label(removed_note(&dialog.removed));
            }
            ui.add_space(8.0);
            if !dialog.item_export {
                for (index, section) in rows.iter().enumerate() {
                    if dialog.removed.iter().any(|name| name == section.name) {
                        continue;
                    }
                    if section.path.is_none() {
                        continue;
                    }
                    let copying = matches!(section.copy_state, CopyState::Copying { .. });
                    ui.add_enabled(
                        !copying,
                        egui::Checkbox::new(&mut dialog.folders[index], section.name),
                    );
                }
                ui.add_space(8.0);
                let keep: Vec<_> = dialog
                    .folders
                    .iter()
                    .enumerate()
                    .filter(|(index, selected)| **selected && rows[*index].path.is_some())
                    .map(|(index, _)| index)
                    .collect();
                if ui
                    .add_enabled(!keep.is_empty(), egui::Button::new("Copy"))
                    .clicked()
                {
                    action = Some(ExportAction::CopyFolders(keep));
                }
            } else {
                for (_, parent, items) in &dialog.items {
                    ui.label(egui::RichText::new(*parent).strong());
                    for item in items {
                        ui.label(format!("  {}", item.name));
                    }
                }
                ui.add_space(8.0);
                if ui
                    .add_enabled(!dialog.items.is_empty(), egui::Button::new("Copy"))
                    .clicked()
                {
                    action = Some(ExportAction::CopyItems);
                }
            }
        });
    if response.is_none() {
        dialog.open = false;
    }
    action
}

fn removed_note(names: &[String]) -> String {
    let list = match names {
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        many => {
            let (last, rest) = many.split_last().unwrap();
            format!("{} and {last}", rest.join(", "))
        }
    };
    let verb = if names.len() == 1 { "was" } else { "were" };
    let those = if names.len() == 1 {
        "that folder"
    } else {
        "those folders"
    };
    format!("{list} {verb} removed from this export. The destination is inside {those}.")
}

fn without_nesting_items(
    mut jobs: Vec<(usize, &'static str, Vec<scan::Child>)>,
    destination: &std::path::Path,
) -> (Vec<(usize, &'static str, Vec<scan::Child>)>, Vec<String>) {
    let mut removed = Vec::new();
    for (_, parent, items) in &mut jobs {
        let mut kept = Vec::new();
        for item in items.drain(..) {
            let nests = item.kind == scan::ChildKind::Folder
                && copy::item_would_nest(&item.path, destination, parent, &item.name);
            if nests {
                removed.push(format!("{parent}\\{}", item.name));
            } else {
                kept.push(item);
            }
        }
        *items = kept;
    }
    jobs.retain(|(_, _, items)| !items.is_empty());
    (jobs, removed)
}

fn section_path(ui: &mut egui::Ui, section: &FolderSection) {
    ui.add_space(4.0);
    let detail = section_detail(section);
    let detail_color = section_detail_color(ui, section);
    ui.label(egui::RichText::new(detail).small().color(detail_color));
    match &section.copy_state {
        CopyState::Idle => {}
        CopyState::Copying { copied, total } => {
            let label = if *total == 0 {
                "Copying…".to_string()
            } else {
                format!(
                    "Copying  {} of {}",
                    format::format_bytes(*copied),
                    format::format_bytes(*total)
                )
            };
            ui.label(egui::RichText::new(label).small().weak());
        }
        CopyState::Copied(path) => {
            ui.label(
                egui::RichText::new(format!("Copied to {}", path.display()))
                    .small()
                    .weak(),
            );
        }
        CopyState::Failed(message) => {
            ui.label(
                egui::RichText::new(message)
                    .small()
                    .color(egui::Color32::from_rgb(255, 150, 140)),
            );
        }
    }
}

fn section_status(section: &FolderSection, show_hidden: bool) -> String {
    let visible: Vec<_> = section
        .children
        .iter()
        .filter(|child| listed(child, show_hidden))
        .collect();
    match &section.state {
        SectionState::Missing => "Not available".to_string(),
        SectionState::Failed(_) => "Could not read".to_string(),
        SectionState::Waiting | SectionState::Scanning => "Reading…".to_string(),
        SectionState::Ready if visible.is_empty() => match section.shown_bytes() {
            Some(bytes) => format!("{}  ·  Empty", approximate_size(bytes)),
            None => "Empty".to_string(),
        },
        SectionState::Ready => {
            let folders = visible
                .iter()
                .filter(|child| child.kind == scan::ChildKind::Folder)
                .count();
            let files = visible.len() - folders;
            let counts = format!(
                "{}  ·  {}",
                count_label(folders, "folder", "folders"),
                count_label(files, "file", "files")
            );
            match section.shown_bytes() {
                Some(bytes) => format!("{}  ·  {counts}", approximate_size(bytes)),
                None => counts,
            }
        }
    }
}

fn section_detail(section: &FolderSection) -> String {
    match &section.state {
        SectionState::Missing => "This folder is not on this PC".to_string(),
        SectionState::Failed(message) => message.clone(),
        _ => section
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    }
}

fn section_detail_color(ui: &egui::Ui, section: &FolderSection) -> egui::Color32 {
    match &section.state {
        SectionState::Failed(_) => egui::Color32::from_rgb(255, 150, 140),
        _ => ui.visuals().weak_text_color(),
    }
}

struct RowHit {
    clicked: bool,
    drag_started: bool,
    checkbox_clicked: bool,
    rect: egui::Rect,
}

fn child_row(ui: &mut egui::Ui, child: &mut scan::Child) -> RowHit {
    let mut checkbox_clicked = false;
    let mut clicked = false;
    let mut drag_started = false;
    let mut rect = egui::Rect::NOTHING;
    ui.horizontal(|ui| {
        if ui
            .add(egui::Checkbox::new(&mut child.selected, ""))
            .clicked()
        {
            checkbox_clicked = true;
        }
        let response = ui
            .allocate_response(
                egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
                egui::Sense::click_and_drag(),
            )
            .on_hover_text(child.path.display().to_string());
        rect = response.rect;
        if child.selected {
            ui.painter()
                .rect_filled(response.rect, 3.0, ui.visuals().selection.bg_fill);
        }
        let name_color = ui.visuals().text_color();
        let size_color = ui.visuals().weak_text_color();
        let name_font = egui::TextStyle::Body.resolve(ui.style());
        let size_font = egui::TextStyle::Monospace.resolve(ui.style());
        let size_label = match child.kind {
            scan::ChildKind::Folder => "Folder".to_string(),
            scan::ChildKind::File => format::format_bytes(child.bytes),
        };
        let size_galley = ui
            .painter()
            .layout_no_wrap(size_label, size_font, size_color);
        let name_rect = egui::Rect::from_min_max(
            response.rect.left_top(),
            egui::pos2(
                response.rect.right() - size_galley.size().x - 12.0,
                response.rect.bottom(),
            ),
        );
        ui.painter().with_clip_rect(name_rect).text(
            response.rect.left_center() + egui::vec2(4.0, 0.0),
            egui::Align2::LEFT_CENTER,
            &child.name,
            name_font,
            name_color,
        );
        ui.painter().galley(
            egui::pos2(
                response.rect.right() - size_galley.size().x - 4.0,
                response.rect.center().y - size_galley.size().y * 0.5,
            ),
            size_galley,
            size_color,
        );
        clicked = response.clicked();
        drag_started = response.drag_started();
    });
    RowHit {
        clicked,
        drag_started,
        checkbox_clicked,
        rect,
    }
}

fn select_all_checkbox(ui: &mut egui::Ui, children: &mut [scan::Child], show_hidden: bool) {
    let visible: Vec<_> = children
        .iter()
        .enumerate()
        .filter(|(_, child)| listed(child, show_hidden))
        .map(|(index, _)| index)
        .collect();
    if visible.is_empty() {
        return;
    }
    let mut all = visible.iter().all(|index| children[*index].selected);
    let mixed = visible.iter().any(|index| children[*index].selected) && !all;
    if ui
        .add(egui::Checkbox::new(&mut all, "").indeterminate(mixed))
        .changed()
    {
        for index in visible {
            children[index].selected = all;
        }
    }
}

fn select_only(children: &mut [scan::Child], chosen: usize, show_hidden: bool) {
    for (index, child) in children.iter_mut().enumerate() {
        child.selected = listed(child, show_hidden) && index == chosen;
    }
}

fn select_range(children: &mut [scan::Child], from: usize, to: usize, show_hidden: bool) {
    let start = from.min(to);
    let end = from.max(to);
    for (index, child) in children.iter_mut().enumerate() {
        child.selected = listed(child, show_hidden) && (start..=end).contains(&index);
    }
}

fn apply_drag(
    ui: &mut egui::Ui,
    section_index: usize,
    children: &mut [scan::Child],
    show_hidden: bool,
    drag_origin: &Option<(usize, usize)>,
    shown: &[usize],
    rects: &[egui::Rect],
) {
    let Some((origin_section, origin_index)) = *drag_origin else {
        return;
    };
    if origin_section != section_index {
        return;
    }
    let Some(pos) = ui.input(|input| input.pointer.interact_pos()) else {
        return;
    };
    let Some(slot) = rects.iter().position(|rect| rect.contains(pos)) else {
        return;
    };
    let Some(hover) = shown.get(slot).copied() else {
        return;
    };
    select_range(children, origin_index, hover, show_hidden);
}

fn default_sections() -> Vec<FolderSection> {
    [
        ("Downloads", dirs::download_dir()),
        ("Documents", dirs::document_dir()),
        ("Videos", dirs::video_dir()),
        ("Music", dirs::audio_dir()),
        ("Pictures", dirs::picture_dir()),
        ("Desktop", dirs::desktop_dir()),
    ]
    .into_iter()
    .map(|(name, path)| FolderSection {
        name,
        path,
        state: SectionState::Waiting,
        children: Vec::new(),
        copy_state: CopyState::Idle,
        indexed_bytes: None,
        size_ready: false,
    })
    .collect()
}

fn disconnected_channel() -> mpsc::Receiver<ScanEvent> {
    let (tx, rx) = mpsc::channel();
    drop(tx);
    rx
}

/// Drop the title-bar icon. An empty [`egui::IconData`] stops eframe from
/// installing its own logo; this also clears the icon Windows would show instead.
fn clear_window_icon(frame: &eframe::Frame) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = frame.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(window) = handle.as_raw() else {
        return true;
    };

    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, SWP_FRAMECHANGED,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_DLGMODALFRAME,
    };

    unsafe {
        let hwnd = window.hwnd.get() as *mut _;
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_DLGMODALFRAME as isize);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
    true
}
