#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Window that shows how big the usual desktop folders are.
//!
//! Each folder is walked on its own thread. Those threads send running totals
//! through a channel. [`FolderApp::logic`] reads the channel, and [`FolderApp::ui`]
//! draws one bar per folder.

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
            .with_inner_size([720.0, 220.0])
            .with_min_inner_size([560.0, 180.0])
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
    rows: Vec<FolderRow>,
    events: mpsc::Receiver<ScanEvent>,
    scanning: bool,
    cleared_icon: bool,
}

struct FolderRow {
    name: &'static str,
    path: Option<PathBuf>,
    files: u64,
    bytes: u64,
    state: RowState,
}

#[derive(Debug)]
enum RowState {
    Waiting,
    Scanning,
    Ready,
    Missing,
    Failed(String),
}

enum ScanEvent {
    Progress {
        index: usize,
        files: u64,
        bytes: u64,
    },
    Finished {
        index: usize,
        files: u64,
        bytes: u64,
    },
    Failed {
        index: usize,
        message: String,
    },
}

impl FolderApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            rows: default_rows(),
            events: disconnected_channel(),
            scanning: false,
            cleared_icon: false,
        };
        app.spawn_scans();
        app
    }

    fn spawn_scans(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.events = rx;
        self.scanning = false;

        for (index, row) in self.rows.iter_mut().enumerate() {
            row.files = 0;
            row.bytes = 0;

            let Some(path) = row.path.clone() else {
                row.state = RowState::Missing;
                continue;
            };

            row.state = RowState::Scanning;
            self.scanning = true;

            let tx = tx.clone();
            let ctx = self.ctx.clone();
            thread::spawn(move || {
                let result = scan::scan_folder(&path, |files, bytes| {
                    let _ = tx.send(ScanEvent::Progress {
                        index,
                        files,
                        bytes,
                    });
                    ctx.request_repaint();
                });
                let event = match result {
                    Ok(totals) => ScanEvent::Finished {
                        index,
                        files: totals.files,
                        bytes: totals.bytes,
                    },
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
        self.scanning = self
            .rows
            .iter()
            .any(|row| matches!(row.state, RowState::Scanning));
    }

    fn total_bytes(&self) -> u64 {
        self.rows.iter().map(|row| row.bytes).sum()
    }

    fn total_files(&self) -> u64 {
        self.rows.iter().map(|row| row.files).sum()
    }
}

fn apply_event(rows: &mut [FolderRow], event: ScanEvent) {
    match event {
        ScanEvent::Progress {
            index,
            files,
            bytes,
        } => {
            let Some(row) = rows.get_mut(index) else {
                return;
            };
            if matches!(row.state, RowState::Scanning) {
                row.files = files;
                row.bytes = bytes;
            }
        }
        ScanEvent::Finished {
            index,
            files,
            bytes,
        } => {
            let Some(row) = rows.get_mut(index) else {
                return;
            };
            row.files = files;
            row.bytes = bytes;
            row.state = RowState::Ready;
        }
        ScanEvent::Failed { index, message } => {
            let Some(row) = rows.get_mut(index) else {
                return;
            };
            row.state = RowState::Failed(message);
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
        let summary = summary_line(
            self.rows.len(),
            self.total_files(),
            self.total_bytes(),
            scanning,
        );
        let mut rescan = false;

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
                });
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            for row in &self.rows {
                folder_row(ui, row);
            }
        });

        if rescan {
            self.spawn_scans();
        }
    }
}

fn summary_line(folders: usize, files: u64, bytes: u64, scanning: bool) -> String {
    let folder_word = if folders == 1 { "folder" } else { "folders" };
    let mut line = format!(
        "{folders} {folder_word}  ·  {}  ·  {} files",
        format::format_bytes(bytes),
        format::format_count(files)
    );
    if scanning {
        line.push_str("  ·  scanning");
    }
    line
}

fn folder_row(ui: &mut egui::Ui, row: &FolderRow) {
    let frame = egui::Frame::NONE
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(12.0)
        .corner_radius(8.0);
    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(row.name).strong().size(17.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(stats_line(row)).monospace()).truncate(),
                );
            });
        });
        ui.add_space(6.0);

        let (detail, detail_color) = match &row.state {
            RowState::Missing => (
                "This folder is not on this PC".to_string(),
                ui.visuals().weak_text_color(),
            ),
            RowState::Failed(message) => (message.clone(), egui::Color32::from_rgb(255, 150, 140)),
            _ => (
                row.path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                ui.visuals().weak_text_color(),
            ),
        };
        ui.add(
            egui::Label::new(egui::RichText::new(detail).small().color(detail_color)).truncate(),
        );
    });
}

fn stats_line(row: &FolderRow) -> String {
    match &row.state {
        RowState::Missing => "Not available".to_string(),
        RowState::Failed(_) => "Could not read".to_string(),
        RowState::Scanning if row.files == 0 && row.bytes == 0 => "Scanning…".to_string(),
        RowState::Scanning => format!(
            "{}   ·   {} files   ·   scanning",
            format::format_bytes(row.bytes),
            format::format_count(row.files)
        ),
        RowState::Waiting | RowState::Ready => format!(
            "{}   ·   {} files",
            format::format_bytes(row.bytes),
            format::format_count(row.files)
        ),
    }
}

fn default_rows() -> Vec<FolderRow> {
    vec![FolderRow {
        name: "Downloads",
        path: dirs::download_dir(),
        files: 0,
        bytes: 0,
        state: RowState::Waiting,
    }]
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
