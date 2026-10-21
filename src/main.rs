#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Window that lists what is directly inside the usual desktop folders.
//!
//! Each folder is read once, on its own thread. Those threads send the names
//! through a channel. [`FolderApp::logic`] reads the channel, and [`FolderApp::ui`]
//! draws one section per folder.

mod copy;
mod format;
mod scan;

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
}

struct FolderSection {
    name: &'static str,
    path: Option<PathBuf>,
    state: SectionState,
    children: Vec<scan::Child>,
    copy_state: CopyState,
}

enum CopyState {
    Idle,
    Copying,
    Copied(PathBuf),
    Failed(String),
}

enum CopyEvent {
    Finished { index: usize, destination: PathBuf },
    Failed { index: usize, message: String },
}

struct ExportDialog {
    choosing: bool,
    selected: Vec<bool>,
    open: bool,
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
        };
        app.spawn_scans();
        app
    }

    fn spawn_scans(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.events = rx;
        self.scanning = false;

        for (index, section) in self.rows.iter_mut().enumerate() {
            section.children.clear();

            let Some(path) = section.path.clone() else {
                section.state = SectionState::Missing;
                continue;
            };

            section.state = SectionState::Scanning;
            self.scanning = true;

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
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            apply_event(&mut self.rows, event);
        }
        while let Ok(event) = self.copy_events.try_recv() {
            apply_copy_event(&mut self.rows, event);
        }
        self.scanning = self
            .rows
            .iter()
            .any(|section| matches!(section.state, SectionState::Scanning));
    }

    fn start_exports(&mut self, indexes: Vec<usize>) {
        let pending: Vec<_> = indexes
            .into_iter()
            .filter(|index| {
                self.rows[*index].path.is_some()
                    && !matches!(self.rows[*index].copy_state, CopyState::Copying)
            })
            .collect();
        if pending.is_empty() {
            return;
        }

        let Some(destination_root) = rfd::FileDialog::new()
            .set_title("Copy folders to")
            .pick_folder()
        else {
            return;
        };

        for index in pending {
            let source = self.rows[index].path.clone().unwrap();
            let name = self.rows[index].name;
            self.rows[index].copy_state = CopyState::Copying;
            let tx = self.copy_tx.clone();
            let ctx = self.ctx.clone();
            let destination_root = destination_root.clone();
            thread::spawn(move || {
                let event = match copy::copy_folder(&source, &destination_root, name) {
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

    fn child_counts(&self) -> (usize, usize) {
        let mut folders = 0;
        let mut files = 0;
        for section in &self.rows {
            for child in &section.children {
                match child.kind {
                    scan::ChildKind::Folder => folders += 1,
                    scan::ChildKind::File => files += 1,
                }
            }
        }
        (folders, files)
    }
}

fn apply_event(rows: &mut [FolderSection], event: ScanEvent) {
    match event {
        ScanEvent::Finished { index, children } => {
            let Some(section) = rows.get_mut(index) else {
                return;
            };
            section.children = children;
            section.state = SectionState::Ready;
        }
        ScanEvent::Failed { index, message } => {
            let Some(section) = rows.get_mut(index) else {
                return;
            };
            section.children.clear();
            section.state = SectionState::Failed(message);
        }
    }
}

fn apply_copy_event(rows: &mut [FolderSection], event: CopyEvent) {
    match event {
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
        let summary = summary_line(folders, files, scanning);
        let mut rescan = false;
        let mut open_export = false;
        let copying = self
            .rows
            .iter()
            .any(|section| matches!(section.copy_state, CopyState::Copying));
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
                });
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for section in &self.rows {
                        folder_section(ui, section);
                        ui.add_space(8.0);
                    }
                });
        });

        if open_export {
            self.export_dialog = Some(ExportDialog {
                choosing: false,
                selected: self
                    .rows
                    .iter()
                    .map(|section| section.path.is_some())
                    .collect(),
                open: true,
            });
        }

        let mut confirmed = None;
        if let Some(dialog) = self.export_dialog.as_mut() {
            confirmed = export_dialog(ui.ctx(), dialog, &self.rows);
            if !dialog.open {
                self.export_dialog = None;
            }
        }

        if rescan {
            self.spawn_scans();
        }
        if let Some(indexes) = confirmed {
            self.export_dialog = None;
            self.start_exports(indexes);
        }
    }
}

fn summary_line(folders: usize, files: usize, scanning: bool) -> String {
    let mut line = format!(
        "{}  ·  {}",
        count_label(folders, "folder", "folders"),
        count_label(files, "file", "files")
    );
    if scanning {
        line.push_str("  ·  scanning");
    }
    line
}

fn count_label(count: usize, one: &str, many: &str) -> String {
    let word = if count == 1 { one } else { many };
    format!("{} {word}", format::format_count(count as u64))
}

fn folder_section(ui: &mut egui::Ui, section: &FolderSection) {
    let frame = egui::Frame::NONE
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(12.0)
        .corner_radius(8.0);
    frame.show(ui, |ui| {
        let can_expand =
            matches!(section.state, SectionState::Ready) && !section.children.is_empty();
        if can_expand {
            let id = ui.make_persistent_id(section.name);
            let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                id,
                false,
            );
            let header = ui.vertical(|ui| {
                ui.set_min_width(ui.available_width());
                let mut name_clicked = false;
                ui.horizontal(|ui| {
                    state.show_toggle_button(ui, egui::collapsing_header::paint_default_icon);
                    let name = egui::RichText::new(section.name).strong().size(17.0);
                    if ui.add(egui::Button::new(name).frame(false)).clicked() {
                        name_clicked = true;
                    }
                    section_status_label(ui, section);
                });
                section_path(ui, section);
                name_clicked
            });
            if header.inner {
                state.toggle(ui);
            }
            state.show_body_indented(&header.response, ui, |ui| {
                ui.add_space(6.0);
                for child in &section.children {
                    child_row(ui, child);
                }
            });
        } else {
            section_heading(ui, section);
            section_path(ui, section);
        }
    });
}

fn section_heading(ui: &mut egui::Ui, section: &FolderSection) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(section.name).strong().size(17.0));
        section_status_label(ui, section);
    });
}

fn section_status_label(ui: &mut egui::Ui, section: &FolderSection) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(egui::RichText::new(section_status(section)).monospace());
    });
}

fn export_dialog(
    ctx: &egui::Context,
    dialog: &mut ExportDialog,
    rows: &[FolderSection],
) -> Option<Vec<usize>> {
    let mut confirmed = None;
    let response = egui::Window::new("Export")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .open(&mut dialog.open)
        .show(ctx, |ui| {
            ui.label("Copy every folder, or choose which ones.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("All folders").clicked() {
                    confirmed = Some(available_indexes(rows));
                }
                if ui.button("Choose folders").clicked() {
                    dialog.choosing = true;
                }
            });
            if dialog.choosing {
                ui.add_space(8.0);
                for (index, section) in rows.iter().enumerate() {
                    ui.add_enabled(
                        section.path.is_some(),
                        egui::Checkbox::new(&mut dialog.selected[index], section.name),
                    );
                }
                ui.add_space(8.0);
                let any_selected = dialog
                    .selected
                    .iter()
                    .enumerate()
                    .any(|(index, selected)| *selected && rows[index].path.is_some());
                if ui
                    .add_enabled(any_selected, egui::Button::new("Continue"))
                    .clicked()
                {
                    confirmed = Some(
                        dialog
                            .selected
                            .iter()
                            .enumerate()
                            .filter(|(index, selected)| **selected && rows[*index].path.is_some())
                            .map(|(index, _)| index)
                            .collect(),
                    );
                }
            }
        });
    if response.is_none() {
        dialog.open = false;
    }
    confirmed
}

fn available_indexes(rows: &[FolderSection]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, section)| section.path.is_some())
        .map(|(index, _)| index)
        .collect()
}

fn section_path(ui: &mut egui::Ui, section: &FolderSection) {
    ui.add_space(4.0);
    let detail = section_detail(section);
    let detail_color = section_detail_color(ui, section);
    ui.label(egui::RichText::new(detail).small().color(detail_color));
    match &section.copy_state {
        CopyState::Idle => {}
        CopyState::Copying => {
            ui.label(egui::RichText::new("Copying…").small().weak());
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

fn section_status(section: &FolderSection) -> String {
    match &section.state {
        SectionState::Missing => "Not available".to_string(),
        SectionState::Failed(_) => "Could not read".to_string(),
        SectionState::Waiting | SectionState::Scanning => "Reading…".to_string(),
        SectionState::Ready if section.children.is_empty() => "Empty".to_string(),
        SectionState::Ready => {
            let folders = section
                .children
                .iter()
                .filter(|child| child.kind == scan::ChildKind::Folder)
                .count();
            let files = section.children.len() - folders;
            format!(
                "{}  ·  {}",
                count_label(folders, "folder", "folders"),
                count_label(files, "file", "files")
            )
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

fn child_row(ui: &mut egui::Ui, child: &scan::Child) {
    ui.horizontal(|ui| {
        ui.label(&child.name)
            .on_hover_text(child.path.display().to_string());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = match child.kind {
                scan::ChildKind::Folder => "Folder".to_string(),
                scan::ChildKind::File => format::format_bytes(child.bytes),
            };
            ui.label(egui::RichText::new(label).monospace().weak());
        });
    });
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
