//! The preview pane: details of the selected result, shown to the right of the list.
//!
//! Apps, settings, windows and calculator results need nothing from the disk, so their details
//! are built on the spot (see `build_extra` in `main.rs`). Files and folders need their metadata
//! read, a text file needs its first lines and a PNG needs decoding: that happens on a worker
//! thread, newest request first, so moving the selection (or typing) never waits on the disk,
//! and never on a slow network drive either.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Vec2};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

/// Past this many cached previews the cache is dropped and refilled on demand.
const MAX_CACHED: usize = 60;
/// Height reserved above the title for the icon or image thumbnail.
const VISUAL_HEIGHT: f32 = 150.0;
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "log", "csv", "json", "toml", "yml", "yaml", "ini", "cfg", "conf", "xml", "html", "htm", "css", "js", "ts",
    "rs", "py", "c", "h", "cpp", "hpp", "java", "cs", "go", "bat", "cmd", "ps1", "sh", "sql", "tex", "srt",
];

/// What the worker found out about one file or folder.
pub struct Details {
    pub kind_label: String,
    pub rows: Vec<(String, String)>,
    pub text: Option<String>,
    pub image: Option<egui::ColorImage>,
}

/// [`Details`] ready to draw (the image has become a texture).
pub struct Loaded {
    pub kind_label: String,
    pub rows: Vec<(String, String)>,
    pub text: Option<String>,
    pub thumb: Option<egui::TextureHandle>,
}

struct Job {
    id: String,
    path: PathBuf,
    is_dir: bool,
}

pub struct Previews {
    jobs: Sender<Job>,
    done: Receiver<(String, Details)>,
    ready: HashMap<String, Loaded>,
    last_requested: String,
    ctx: egui::Context,
}

impl Previews {
    pub fn new(ctx: &egui::Context) -> Self {
        let (jobs, jobs_rx) = channel::<Job>();
        let (done_tx, done) = channel::<(String, Details)>();
        let worker_ctx = ctx.clone();
        let _ = std::thread::Builder::new().name("okra-preview".into()).spawn(move || {
            while let Ok(mut job) = jobs_rx.recv() {
                // Only the newest request matters: the user has already moved on from the others.
                while let Ok(newer) = jobs_rx.try_recv() {
                    job = newer;
                }
                let details = load(&job.path, job.is_dir);
                if done_tx.send((job.id, details)).is_err() {
                    return;
                }
                worker_ctx.request_repaint();
            }
        });
        Self { jobs, done, ready: HashMap::new(), last_requested: String::new(), ctx: ctx.clone() }
    }

    /// Ask for the details of a file or folder. Cheap to call every frame.
    pub fn request(&mut self, id: &str, path: &Path, is_dir: bool) {
        if self.last_requested == id || self.ready.contains_key(id) {
            return;
        }
        self.last_requested = id.to_string();
        let _ = self.jobs.send(Job { id: id.to_string(), path: path.to_path_buf(), is_dir });
    }

    /// Turn finished work into drawable previews. Call once per frame.
    pub fn poll(&mut self) {
        while let Ok((id, details)) = self.done.try_recv() {
            if self.ready.len() >= MAX_CACHED {
                self.ready.clear();
            }
            let thumb = details
                .image
                .map(|image| self.ctx.load_texture(format!("preview:{id}"), image, egui::TextureOptions::LINEAR));
            self.ready.insert(id, Loaded { kind_label: details.kind_label, rows: details.rows, text: details.text, thumb });
        }
    }

    pub fn get(&self, id: &str) -> Option<&Loaded> {
        self.ready.get(id)
    }

    /// Forget everything (the launcher was closed): the next open sees fresh file details.
    pub fn clear(&mut self) {
        self.ready.clear();
        self.last_requested.clear();
    }
}

/// Reads what the pane shows about a file or folder. Runs on the worker thread.
pub fn load(path: &Path, is_dir: bool) -> Details {
    let extension = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    let kind_label = if is_dir {
        "Folder".to_string()
    } else if extension.is_empty() {
        "File".to_string()
    } else {
        format!("{} file", extension.to_ascii_uppercase())
    };
    let mut details = Details { kind_label, rows: Vec::new(), text: None, image: None };
    let Ok(meta) = std::fs::metadata(path) else {
        details.rows.push(("Status".to_string(), "Not found or not accessible".to_string()));
        return details;
    };

    if is_dir {
        let count = std::fs::read_dir(path).map(|entries| entries.take(1000).count()).unwrap_or(0);
        let shown = if count >= 1000 { "1000+".to_string() } else { count.to_string() };
        details.rows.push(("Contains".to_string(), format!("{shown} items")));
    } else {
        details.rows.push(("Size".to_string(), human_size(meta.len())));
    }
    let offset = local_offset_minutes();
    if let Ok(time) = meta.modified() {
        details.rows.push(("Modified".to_string(), format_time(time, offset)));
    }
    if let Ok(time) = meta.created() {
        details.rows.push(("Created".to_string(), format_time(time, offset)));
    }

    if !is_dir {
        if extension == "png" && meta.len() <= MAX_IMAGE_BYTES {
            if let Ok(picture) = image::open(path) {
                details.rows.push(("Dimensions".to_string(), format!("{} \u{d7} {}", picture.width(), picture.height())));
                let small = picture.thumbnail(480, 480).to_rgba8();
                let size = [small.width() as usize, small.height() as usize];
                details.image = Some(egui::ColorImage::from_rgba_unmultiplied(size, small.as_raw()));
            }
        } else if TEXT_EXTENSIONS.contains(&extension.as_str()) {
            details.text = read_snippet(path);
        }
    }
    if let Some(parent) = path.parent() {
        details.rows.push(("Where".to_string(), crate::shorten(&parent.display().to_string(), 52)));
    }
    details
}

/// The first lines of a text file (None for an empty or binary file).
fn read_snippet(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; 4096];
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    if buffer.is_empty() || buffer.contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&buffer);
    let lines: Vec<String> = text
        .lines()
        .take(14)
        .map(|line| line.chars().take(100).collect::<String>().replace('\t', "    "))
        .collect();
    Some(lines.join("\n"))
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `15 Nov 2026, 14:46`, shifted by `offset_minutes` from UTC.
pub fn format_time(time: SystemTime, offset_minutes: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let Ok(since_epoch) = time.duration_since(UNIX_EPOCH) else { return "\u{2014}".to_string() };
    let seconds = since_epoch.as_secs() as i64 + offset_minutes * 60;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let in_day = seconds.rem_euclid(86_400);
    format!("{day} {} {year}, {:02}:{:02}", MONTHS[(month - 1) as usize], in_day / 3600, (in_day % 3600) / 60)
}

/// Minutes to add to UTC to get this PC's local time.
#[cfg(windows)]
fn local_offset_minutes() -> i64 {
    #[repr(C)]
    struct SystemTimeParts {
        parts: [u16; 8],
    }
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTimeParts,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTimeParts,
        daylight_bias: i32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTimeZoneInformation(info: *mut TimeZoneInformation) -> u32;
    }
    unsafe {
        let mut info: TimeZoneInformation = std::mem::zeroed();
        let state = GetTimeZoneInformation(&mut info);
        let extra = match state {
            1 => info.standard_bias,
            2 => info.daylight_bias,
            _ => 0,
        };
        -((info.bias + extra) as i64)
    }
}

#[cfg(not(windows))]
fn local_offset_minutes() -> i64 {
    0
}

/// Everything the pane draws for one result.
pub struct View<'a> {
    pub title: &'a str,
    pub kind_label: &'a str,
    /// Calculator answers: the title is the whole point, shown big, with no icon.
    pub large: bool,
    pub icon: Option<&'a egui::TextureHandle>,
    pub thumb: Option<&'a egui::TextureHandle>,
    /// Colour of the letter tile shown while there is no real icon.
    pub tile: Color32,
    pub rows: &'a [(String, String)],
    pub extra: &'a [(String, String)],
    pub text: Option<&'a str>,
}

pub fn show(ui: &mut egui::Ui, view: &View, text: Color32, muted: Color32) {
    egui::ScrollArea::vertical().id_source("okra-preview-scroll").auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(6.0);
        if view.large {
            ui.add_space(18.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Label::new(RichText::new(view.title).size(36.0).color(text)).truncate(true));
            });
        } else {
            let (area, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), VISUAL_HEIGHT), Sense::hover());
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            if let Some(thumb) = view.thumb {
                let size = thumb.size_vec2();
                let scale = ((area.width() - 8.0) / size.x).min(area.height() / size.y).min(1.0);
                let rect = Rect::from_center_size(area.center(), size * scale);
                ui.painter().image(thumb.id(), rect, uv, Color32::WHITE);
            } else {
                let rect = Rect::from_center_size(area.center(), Vec2::splat(64.0));
                match view.icon {
                    Some(icon) => {
                        ui.painter().image(icon.id(), rect, uv, Color32::WHITE);
                    }
                    None => {
                        ui.painter().rect_filled(rect, 14.0_f32, view.tile);
                        let letter = view.title.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
                        ui.painter().text(rect.center(), Align2::CENTER_CENTER, letter, FontId::proportional(30.0), Color32::WHITE);
                    }
                }
            }
            ui.vertical_centered(|ui| {
                ui.add(egui::Label::new(RichText::new(view.title).size(17.0).strong().color(text)).truncate(true));
                ui.label(RichText::new(view.kind_label).size(12.0).color(muted));
            });
        }

        if !view.rows.is_empty() || !view.extra.is_empty() {
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            egui::Grid::new("okra-preview-grid").num_columns(2).spacing(Vec2::new(14.0, 6.0)).show(ui, |ui| {
                for (label, value) in view.rows.iter().chain(view.extra.iter()) {
                    ui.label(RichText::new(label).size(12.0).color(muted));
                    ui.add(egui::Label::new(RichText::new(value).size(13.0).color(text)).truncate(true));
                    ui.end_row();
                }
            });
        }

        if let Some(snippet) = view.text {
            ui.add_space(10.0);
            egui::Frame::none().inner_margin(8.0_f32).rounding(6.0_f32).fill(ui.visuals().faint_bg_color).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                for line in snippet.lines() {
                    ui.add(egui::Label::new(RichText::new(line).monospace().size(11.5).color(muted)).truncate(true));
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_formatted_from_the_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_675), (2023, 11, 14));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29)); // leap day
        let time = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000); // 2023-11-14 22:13:20 UTC
        assert_eq!(format_time(time, 0), "14 Nov 2023, 22:13");
        assert_eq!(format_time(time, 345), "15 Nov 2023, 03:58"); // Nepal, UTC+5:45
        assert_eq!(format_time(time, -480), "14 Nov 2023, 14:13");
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(1023), "1023 bytes");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(300 * 1024 * 1024), "300 MB");
    }

    #[test]
    fn files_and_folders_are_described() {
        let dir = std::env::temp_dir().join(format!("okra-preview-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, "first line\nsecond line\n").unwrap();

        let details = load(&file, false);
        assert_eq!(details.kind_label, "TXT file");
        assert!(details.rows.iter().any(|(label, value)| label == "Size" && value == "23 bytes"));
        assert!(details.rows.iter().any(|(label, _)| label == "Modified"));
        assert_eq!(details.text.as_deref(), Some("first line\nsecond line"));

        let folder = load(&dir, true);
        assert_eq!(folder.kind_label, "Folder");
        assert!(folder.rows.iter().any(|(label, value)| label == "Contains" && value == "1 items"));

        let missing = load(&dir.join("gone.bin"), false);
        assert_eq!(missing.rows[0].0, "Status");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_files_get_no_text_preview() {
        let path = std::env::temp_dir().join(format!("okra-preview-bin-{}.txt", std::process::id()));
        std::fs::write(&path, [b'a', 0, b'b']).unwrap();
        assert!(read_snippet(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }
}
