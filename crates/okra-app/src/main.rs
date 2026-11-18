#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use eframe::egui::{
    self, Align2, Color32, FontId, Key, Modifiers, Pos2, RichText, Sense, Stroke, Vec2,
};
use okra_core::matcher::{fuzzy_with, Query};
use okra_core::providers::files::{ntfs_helper_status, HelperStatus};
use okra_core::{
    Action, Config, Engine, FileBackendKind, FontKind, Item, Kind, Outcome, ThemeKind,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;

mod icons;
mod preview;

/// macOS system blue. Change this one constant to re-tint the selection and section accents.
const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);
/// Wide enough for the result list on the left and the preview pane on the right.
const WINDOW_WIDTH: f32 = 860.0;
const LIST_WIDTH: f32 = 320.0;
/// The preview needs some room even when there are only one or two results.
const PREVIEW_MIN_HEIGHT: f32 = 250.0;
const BAR_HEIGHT: f32 = 66.0;
const ROW_HEIGHT: f32 = 48.0;
const MAX_LIST_HEIGHT: f32 = 400.0;
const SETTINGS_HEIGHT: f32 = 540.0;
/// The title of the launcher window. The app finds its own window by process and this title.
const WINDOW_TITLE: &str = "Okra Brown Search";

/// True while the launcher is on screen. The hotkey handler (it runs outside the UI loop) reads
/// this to decide whether the shortcut should open the launcher or close it.
static LAUNCHER_OPEN: AtomicBool = AtomicBool::new(false);
/// Set by the hotkey handler when the shortcut is pressed while the launcher is open.
static HIDE_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set once the first frame has registered the hotkey and tray icon.
static RESIDENT_READY: AtomicBool = AtomicBool::new(false);
/// The window never changes size: resizing it on every keystroke made the bar jump and flicker.
/// It is always tall enough for the biggest list (or the settings page); the panel is painted
/// only as tall as the content needs, and the rest of the window stays transparent.
const WINDOW_HEIGHT: f32 = SETTINGS_HEIGHT;

/// A result plus everything drawing it needs that depends only on the result and the query that
/// produced it. Built once on the search thread, instead of on every frame for every row.
struct Row {
    item: Item,
    /// Shortened path, or the kind label when there is none.
    subtitle: String,
    /// `(start, end, emphasised)` byte ranges of the title; emphasised = matched by the query.
    title_runs: Vec<(usize, usize, bool)>,
    /// What to ask Windows for this row's real icon (None: keep the coloured letter tile).
    icon: Option<icons::IconRequest>,
}

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    border: Color32,
    text: Color32,
    muted: Color32,
    sep: Color32,
    hover: Color32,
}

struct App {
    engine: Arc<Engine>,
    config: Config,
    initial_backend: FileBackendKind,
    initial_max_results: usize,
    initial_start_with_windows: bool,
    query: String,
    rows: Vec<Row>,
    icons: icons::Icons,
    /// File details for the preview pane, loaded off the UI thread.
    previews: preview::Previews,
    /// Id of the result the cached `preview_extra` belongs to.
    preview_id: String,
    /// Preview rows that need no disk access (location, what a setting opens, usage...).
    preview_extra: Vec<(String, String)>,
    selected: usize,
    status: String,
    gen: u64,
    shown: u64,
    /// Generation of the newest query; the search thread polls it to drop work that is stale.
    latest_gen: Arc<AtomicU64>,
    /// Panel height the window's visible region was last cut to (Windows).
    region_height: f32,
    /// A query is in flight and nothing has come back for it yet (so "No Results" is premature).
    pending: bool,
    q_tx: Sender<(u64, String)>,
    r_rx: Receiver<(u64, Vec<Row>)>,
    /// Helper health is fetched on one dedicated worker because named-pipe reads may stall.
    helper_status: Option<HelperStatus>,
    helper_status_request_tx: SyncSender<()>,
    helper_status_rx: Receiver<HelperStatus>,
    helper_status_last_requested: Option<std::time::Instant>,
    settings_open: bool,
    show_requested: Arc<std::sync::atomic::AtomicBool>,
    /// Set by the background app scanner when a fresh application list is available.
    index_refreshed: Arc<std::sync::atomic::AtomicBool>,
    /// Windows wakes us when another window takes the foreground (see `foreground`); if the hook
    /// could not be installed we fall back to a slow poll while the launcher is open.
    #[cfg(windows)]
    foreground_hook: bool,
    start_hidden: bool,
    /// Startup heads-ups (no Everything, hotkey taken...), shown in the bar for the first few opens.
    notice: String,
    notice_shows_left: u8,
    resident_initialized: bool,
    /// True once the window has actually been focused; losing focus after that hides it.
    had_focus: bool,
    /// After showing, keep asking Windows for focus until it is granted (or this time passes).
    focus_retry_until: Option<std::time::Instant>,
    #[cfg(windows)]
    _resident: Option<ResidentControls>,
}

#[cfg(windows)]
struct ResidentControls {
    _hotkey_manager: Option<global_hotkey::GlobalHotKeyManager>,
    _tray_icon: Option<tray_icon::TrayIcon>,
}

#[cfg(windows)]
impl ResidentControls {
    fn new(
        ctx: &egui::Context,
        show_requested: Arc<std::sync::atomic::AtomicBool>,
    ) -> (Self, String) {
        use global_hotkey::{
            hotkey::{Code, HotKey, Modifiers},
            GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
        };
        use tray_icon::{
            menu::{Menu, MenuEvent, MenuItem},
            Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
        };

        let mut messages = Vec::new();
        let hotkey_manager = match GlobalHotKeyManager::new() {
            Ok(manager) => {
                let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
                let hotkey_id = hotkey.id();
                match manager.register(hotkey) {
                    Ok(()) => {
                        let context = ctx.clone();
                        let flag = show_requested.clone();
                        GlobalHotKeyEvent::set_event_handler(Some(
                            move |event: GlobalHotKeyEvent| {
                                if event.id == hotkey_id && event.state == HotKeyState::Pressed {
                                    if LAUNCHER_OPEN.load(Ordering::SeqCst) {
                                        // Already open: the shortcut closes it again, like a toggle.
                                        HIDE_REQUESTED.store(true, Ordering::SeqCst);
                                        context.request_repaint();
                                    } else {
                                        LAUNCHER_OPEN.store(true, Ordering::SeqCst);
                                        flag.store(true, Ordering::SeqCst);
                                        bring_to_front(true);
                                        context.send_viewport_cmd(egui::ViewportCommand::Visible(
                                            true,
                                        ));
                                        context.send_viewport_cmd(egui::ViewportCommand::Focus);
                                        context.request_repaint();
                                    }
                                }
                            },
                        ));
                        Some(manager)
                    }
                    Err(error) => {
                        // Usually another program already owns Ctrl+Alt+Space.
                        debug_log(&format!("hotkey registration failed: {error}"));
                        messages.push("Global hotkey unavailable (Ctrl+Alt+Space is taken): use the tray icon".to_string());
                        Some(manager)
                    }
                }
            }
            Err(error) => {
                messages.push(format!("Global hotkey unavailable: {error}"));
                None
            }
        };

        let tray_icon = (|| -> Result<tray_icon::TrayIcon, String> {
            // A dedicated 32 px render: the shell scales tray icons to 16/20/24/32 px, and a
            // pre-rendered small icon stays far crisper than downscaling the 256 px window icon.
            let image = image::load_from_memory(include_bytes!("../../../assets/okra-tray.png"))
                .map_err(|e| e.to_string())?
                .into_rgba8();
            let (width, height) = image.dimensions();
            let icon =
                Icon::from_rgba(image.into_raw(), width, height).map_err(|e| e.to_string())?;

            let menu = Menu::new();
            let show_item = MenuItem::new("Show Okra Brown Search", true, None);
            let quit_item = MenuItem::new("Exit", true, None);
            let show_id = show_item.id().clone();
            let quit_id = quit_item.id().clone();
            menu.append(&show_item).map_err(|e| e.to_string())?;
            menu.append(&quit_item).map_err(|e| e.to_string())?;

            let context_for_menu = ctx.clone();
            let flag_for_menu = show_requested.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                if event.id == quit_id {
                    context_for_menu.send_viewport_cmd(egui::ViewportCommand::Close);
                    context_for_menu.request_repaint();
                } else if event.id == show_id {
                    flag_for_menu.store(true, std::sync::atomic::Ordering::SeqCst);
                    bring_to_front(true);
                    context_for_menu.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    context_for_menu.send_viewport_cmd(egui::ViewportCommand::Focus);
                    context_for_menu.request_repaint();
                }
            }));

            let context_for_tray = ctx.clone();
            let flag_for_tray = show_requested.clone();
            TrayIconEvent::set_event_handler(Some(move |event| {
                let should_show = matches!(
                    event,
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } | TrayIconEvent::DoubleClick { .. }
                );
                if should_show {
                    flag_for_tray.store(true, std::sync::atomic::Ordering::SeqCst);
                    bring_to_front(true);
                    context_for_tray.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    context_for_tray.send_viewport_cmd(egui::ViewportCommand::Focus);
                    context_for_tray.request_repaint();
                }
            }));

            TrayIconBuilder::new()
                .with_tooltip("Okra Brown Search")
                .with_icon(icon)
                .with_menu(Box::new(menu))
                .build()
                .map_err(|e| e.to_string())
        })();

        let tray_icon = match tray_icon {
            Ok(icon) => Some(icon),
            Err(error) => {
                messages.push(format!("System tray unavailable: {error}"));
                None
            }
        };
        let status = messages.join(" · ");
        (
            Self {
                _hotkey_manager: hotkey_manager,
                _tray_icon: tray_icon,
            },
            status,
        )
    }
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        engine: Arc<Engine>,
        config: Config,
        start_hidden: bool,
    ) -> Self {
        load_fonts(&cc.egui_ctx, config.font);
        apply_theme(&cc.egui_ctx, config.theme);

        // The window starts hidden when launched with --background, which relies on the first
        // frame running anyway (that is where the hotkey and tray get registered). If that frame
        // never comes, show the window so a frame is forced and the app cannot stay dead.
        if start_hidden {
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(2500));
                if !RESIDENT_READY.load(Ordering::SeqCst) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.request_repaint();
                }
            });
        }
        let notice = engine.notice().map(str::to_owned).unwrap_or_default();

        // Search runs on a worker thread; rapid typing replaces stale queued queries. Results are
        // streamed: instant sources (apps, settings, windows, calculator) show up at once and the
        // slower file search is added when it finishes, or skipped if the user has typed on.
        let (q_tx, q_rx) = channel::<(u64, String)>();
        let (r_tx, r_rx) = channel::<(u64, Vec<Row>)>();
        let latest_gen = Arc::new(AtomicU64::new(0));
        let (eng, ctx, newest) = (engine.clone(), cc.egui_ctx.clone(), latest_gen.clone());
        std::thread::spawn(move || {
            while let Ok(mut job) = q_rx.recv() {
                while let Ok(newer) = q_rx.try_recv() {
                    job = newer;
                }
                let (generation, query) = job;
                let mut emit = |items: Vec<Item>| {
                    let _ = r_tx.send((generation, build_rows(items, &query)));
                    ctx.request_repaint();
                };
                eng.search_progressive(&query, &mut emit, &|| {
                    newest.load(Ordering::Relaxed) != generation
                });
            }
        });

        // The status pipe is diagnostic only and can block if a helper client stalls. Keep all
        // named-pipe I/O off the UI thread and use a one-slot queue so refresh requests coalesce.
        let (helper_status_request_tx, helper_status_request_rx) =
            std::sync::mpsc::sync_channel::<()>(1);
        let (helper_status_tx, helper_status_rx) = channel::<HelperStatus>();
        let helper_status_ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            while helper_status_request_rx.recv().is_ok() {
                while helper_status_request_rx.try_recv().is_ok() {}
                let status = ntfs_helper_status();
                if helper_status_tx.send(status).is_err() {
                    break;
                }
                helper_status_ctx.request_repaint();
            }
        });

        let show_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // When the background scan finds the (possibly new) list of installed apps, re-run the
        // query on screen so results appear without the user having to type another key.
        let index_refreshed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let (flag, ctx) = (index_refreshed.clone(), cc.egui_ctx.clone());
            engine.set_on_index_refresh(move || {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                ctx.request_repaint();
            });
        }

        Self {
            engine,
            initial_backend: config.file_backend,
            initial_max_results: config.max_results,
            initial_start_with_windows: config.start_with_windows,
            config,
            query: String::new(),
            rows: Vec::new(),
            icons: icons::Icons::new(&cc.egui_ctx),
            previews: preview::Previews::new(&cc.egui_ctx),
            preview_id: String::new(),
            preview_extra: Vec::new(),
            selected: 0,
            status: String::new(),
            gen: 0,
            shown: 0,
            latest_gen,
            region_height: 0.0,
            pending: false,
            q_tx,
            r_rx,
            helper_status: None,
            helper_status_request_tx,
            helper_status_rx,
            helper_status_last_requested: None,
            settings_open: false,
            show_requested,
            index_refreshed,
            #[cfg(windows)]
            foreground_hook: false,
            start_hidden,
            notice,
            notice_shows_left: 0,
            resident_initialized: false,
            had_focus: false,
            focus_retry_until: None,
            #[cfg(windows)]
            _resident: None,
        }
    }

    /// Back to the idle state: just the search bar, no results. Also disowns any query still in
    /// flight on the search thread, so a late answer can never pop a stale list back up.
    fn reset_results(&mut self) {
        self.rows.clear();
        self.selected = 0;
        self.pending = false;
        self.gen += 1;
        self.shown = self.gen;
        self.latest_gen.store(self.gen, Ordering::Relaxed);
    }

    /// Show a new result list and start fetching the icons it needs (rows on screen first).
    fn set_rows(&mut self, rows: Vec<Row>) {
        self.icons
            .request(rows.iter().filter_map(|row| row.icon.as_ref()));
        self.rows = rows;
    }

    fn search_again(&mut self) {
        if self.query.trim().is_empty() {
            self.reset_results();
            return;
        }
        self.gen += 1;
        self.pending = true;
        self.latest_gen.store(self.gen, Ordering::Relaxed);
        let _ = self.q_tx.send((self.gen, self.query.clone()));
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.settings_open = false;
        self.query.clear();
        self.status.clear();
        if self.notice_shows_left > 0 {
            self.notice_shows_left -= 1;
            self.status = self.notice.clone();
        }
        self.had_focus = false;
        self.reset_results();
        LAUNCHER_OPEN.store(true, Ordering::SeqCst);
        debug_log("show");
        #[cfg(windows)]
        {
            foreground::watch(true);
            bring_to_front(false);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.focus_retry_until =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(1000));
        ctx.request_repaint();
    }

    fn hide(&mut self, ctx: &egui::Context) {
        debug_log("hide");
        LAUNCHER_OPEN.store(false, Ordering::SeqCst);
        #[cfg(windows)]
        foreground::watch(false);
        self.had_focus = false;
        self.focus_retry_until = None;
        // Leave nothing behind: the next open starts as an empty bar.
        self.settings_open = false;
        self.query.clear();
        self.reset_results();
        self.previews.clear();
        self.preview_id.clear();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn activate(&mut self, ctx: &egui::Context, idx: usize) {
        let Some(item) = self.rows.get(idx).map(|row| row.item.clone()) else {
            return;
        };
        match self.engine.activate_for(&item, &self.query) {
            Ok(Outcome::CopyText(text)) => {
                ctx.output_mut(|output| output.copied_text = text);
                self.hide(ctx);
            }
            Ok(Outcome::Done) => self.hide(ctx),
            Err(error) => self.status = error,
        }
    }

    fn request_helper_status(&mut self) {
        match self.helper_status_request_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {
                self.helper_status = Some(HelperStatus::NotRunning);
            }
        }
        self.helper_status_last_requested = Some(std::time::Instant::now());
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let pal = palette(self.config.theme);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let section = |ui: &mut egui::Ui, text: &str| {
                ui.add_space(14.0);
                ui.label(RichText::new(text).size(12.0).strong().color(pal.muted));
                ui.add_space(2.0);
            };

            ui.add_space(2.0);
            ui.label(RichText::new("Settings").size(22.0).strong().color(pal.text));

            section(ui, "Appearance");
            let mut theme_changed = false;
            egui::ComboBox::from_id_source("okra-theme")
                .selected_text(self.config.theme.label())
                .show_ui(ui, |ui| {
                    theme_changed |= ui.selectable_value(&mut self.config.theme, ThemeKind::WarmLight, "Light").changed();
                    theme_changed |= ui.selectable_value(&mut self.config.theme, ThemeKind::Dark, "Dark").changed();
                });
            if theme_changed {
                apply_theme(ui.ctx(), self.config.theme);
            }

            section(ui, "Font");
            let mut font_changed = false;
            egui::ComboBox::from_id_source("okra-font")
                .selected_text(self.config.font.label())
                .show_ui(ui, |ui| {
                    font_changed |= ui.selectable_value(&mut self.config.font, FontKind::Inter, FontKind::Inter.label()).changed();
                    font_changed |= ui.selectable_value(&mut self.config.font, FontKind::SegoeUi, FontKind::SegoeUi.label()).changed();
                });
            if font_changed {
                load_fonts(ui.ctx(), self.config.font);
            }

            section(ui, "File search");
            egui::ComboBox::from_id_source("okra-file-backend")
                .selected_text(self.config.file_backend.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.config.file_backend, FileBackendKind::Auto, "Automatic (recommended)");
                    ui.selectable_value(&mut self.config.file_backend, FileBackendKind::Everything, "Everything only");
                    ui.selectable_value(&mut self.config.file_backend, FileBackendKind::Builtin, "Built-in index only");
                });
            ui.label(RichText::new("Automatic uses Everything, es.exe, the optional elevated Okra NTFS helper, then the local index.").size(12.0).color(pal.muted));

            section(ui, "NTFS helper health");
            let helper_description = self
                .helper_status
                .map(|status| status.describe())
                .unwrap_or_else(|| "Checking helper status…".to_string());
            let helper_color = match self.helper_status {
                Some(HelperStatus::Ready { .. } | HelperStatus::Busy) => {
                    Color32::from_rgb(48, 155, 92)
                }
                Some(HelperStatus::Stale { .. } | HelperStatus::Incompatible) => {
                    Color32::from_rgb(205, 135, 35)
                }
                _ => pal.muted,
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(helper_description).size(12.5).color(helper_color));
                if ui.small_button("Refresh").clicked() {
                    self.request_helper_status();
                }
            });
            ui.label(
                RichText::new("This optional index is used in Automatic mode when Everything is unavailable. Status checks never request elevation or start the helper.")
                    .size(12.0)
                    .color(pal.muted),
            );

            section(ui, "Results");
            ui.add(egui::Slider::new(&mut self.config.max_results, 10..=80).text("Maximum results"));

            section(ui, "General");
            ui.checkbox(&mut self.config.start_with_windows, "Open at login");
            ui.add_space(4.0);
            ui.label(RichText::new("Shortcut: Ctrl + Alt + Space opens and closes   ·   Esc hides   ·   Ctrl + 1-9 opens a result").size(12.0).color(pal.muted));
            ui.label(RichText::new("Prefixes: a apps · f files · s settings · win windows").size(12.0).color(pal.muted));
            ui.label(RichText::new("The app stays in the notification area until you choose Exit.").size(12.0).color(pal.muted));

            ui.add_space(16.0);
            let restart_needed = self.config.file_backend != self.initial_backend || self.config.max_results != self.initial_max_results;
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("Save").strong().color(Color32::WHITE)).fill(ACCENT).rounding(6.0_f32)).clicked() {
                    self.save_settings();
                }
                if ui.button("Done").clicked() {
                    self.settings_open = false;
                    self.query.clear();
                    self.search_again();
                }
            });
            if restart_needed {
                ui.add_space(4.0);
                ui.label(RichText::new("File backend and result-count changes apply the next time the app starts.").size(12.0).color(pal.muted));
            }
        });
    }

    fn save_settings(&mut self) {
        if self.config.start_with_windows != self.initial_start_with_windows {
            #[cfg(windows)]
            if let Err(error) = set_startup_registration(self.config.start_with_windows) {
                self.status = error;
                return;
            }
            #[cfg(not(windows))]
            if self.config.start_with_windows {
                self.status = "Windows sign-in launch is available in the Windows build".into();
                return;
            }
        }
        match self.config.save() {
            Ok(()) => {
                self.status = "Settings saved".into();
                // The search engine is already running with its original backend
                // and result limit, so keep the restart notice until relaunch.
                self.initial_start_with_windows = self.config.start_with_windows;
            }
            Err(error) => self.status = error,
        }
    }

    /// Keep the preview pane in step with the selection: build the cheap rows when the selected
    /// result changes, and ask the worker for file details.
    fn refresh_preview(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            self.preview_id.clear();
            return;
        };
        let item = &row.item;
        if item.id != self.preview_id {
            self.preview_id = item.id.clone();
            self.preview_extra = build_extra(&self.engine, item);
        }
        if let (Kind::File | Kind::Folder, Action::OpenPath(path)) = (item.kind, &item.action) {
            self.previews
                .request(&item.id, path, item.kind == Kind::Folder);
        }
    }

    fn draw_preview(&self, ui: &mut egui::Ui, pal: Palette) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let item = &row.item;
        let is_file = matches!(item.kind, Kind::File | Kind::Folder);
        let loaded = if is_file {
            self.previews.get(&item.id)
        } else {
            None
        };
        // Until the worker answers, a file shows the same rows with "..." for the values, so the
        // pane keeps its shape instead of jumping when the details arrive.
        let is_dir = item.kind == Kind::Folder;
        let pending_label = match &item.action {
            Action::OpenPath(path) if is_file && loaded.is_none() => {
                Some(preview::kind_label_for(path, is_dir))
            }
            _ => None,
        };
        let pending: Vec<(String, String)> = if is_file && loaded.is_none() {
            let dots = "\u{2026}".to_string();
            vec![
                (
                    (if is_dir { "Contains" } else { "Size" }).to_string(),
                    dots.clone(),
                ),
                ("Modified".to_string(), dots.clone()),
                ("Created".to_string(), dots),
                ("Where".to_string(), shorten(&item.subtitle, 52)),
            ]
        } else {
            Vec::new()
        };
        let rows: &[(String, String)] = match loaded {
            Some(found) => &found.rows,
            None => &pending,
        };
        let view = preview::View {
            title: &item.title,
            kind_label: loaded
                .map(|found| found.kind_label.as_str())
                .or(pending_label.as_deref())
                .unwrap_or_else(|| item.kind.label()),
            large: item.kind == Kind::Calculation,
            icon: row
                .icon
                .as_ref()
                .and_then(|(key, _)| self.icons.texture(key)),
            thumb: loaded.and_then(|found| found.thumb.as_ref()),
            tile: tile_color(item),
            rows,
            extra: &self.preview_extra,
            text: loaded.and_then(|found| found.text.as_deref()),
        };
        preview::show(ui, &view, pal.text, pal.muted);
    }

    /// Window height for the current state: just the search bar when idle, growing with results.
    fn desired_height(&self) -> f32 {
        if self.settings_open {
            return SETTINGS_HEIGHT;
        }
        let status = if self.status.is_empty() { 0.0 } else { 22.0 };
        if self.rows.is_empty() {
            return if self.query.trim().is_empty() || self.pending {
                BAR_HEIGHT + status
            } else {
                BAR_HEIGHT + 64.0 + status
            };
        }
        let mut total = 0.0;
        let mut prev: Option<Kind> = None;
        for (index, row) in self.rows.iter().enumerate() {
            total += ROW_HEIGHT + 4.0;
            if index == 0 || prev != Some(row.item.kind) {
                total += 28.0;
            }
            prev = Some(row.item.kind);
        }
        BAR_HEIGHT + 24.0 + status + total.clamp(PREVIEW_MIN_HEIGHT, MAX_LIST_HEIGHT)
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Test switch: `OKRA_OPAQUE=1` fills the window with the panel colour instead of clearing
        // it to transparent (some Windows setups flicker when a transparent window redraws).
        if opaque_mode() {
            return palette(self.config.theme).bg.to_normalized_gamma_f32();
        }
        // Transparent window; the panel paints its own rounded background.
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Register native controls on the first UI frame, after eframe has entered
        // the Win32 event loop required by the global-hotkey backend.
        if !self.resident_initialized {
            #[cfg(windows)]
            {
                let (resident, status) = ResidentControls::new(ctx, self.show_requested.clone());
                let controls_failed = status.contains("Global hotkey unavailable")
                    && status.contains("System tray unavailable");
                self._resident = Some(resident);
                if !status.is_empty() {
                    self.notice = if self.notice.is_empty() {
                        status
                    } else {
                        format!("{} · {}", self.notice, status)
                    };
                }
                self.notice_shows_left = if self.notice.is_empty() { 0 } else { 5 };
                self.status = self.notice.clone();
                self.foreground_hook = foreground::install(ctx);
                debug_log(&format!(
                    "foreground hook installed: {}",
                    self.foreground_hook
                ));
                if self.start_hidden && !controls_failed {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                } else {
                    // Started by hand: centre it and arm the same focus and click-away handling
                    // as a hotkey open. Without this a launch that Windows declined to focus
                    // sat on screen forever: no focus, so no Esc, and no click-away to close it.
                    LAUNCHER_OPEN.store(true, Ordering::SeqCst);
                    bring_to_front(true);
                    foreground::watch(true);
                    self.focus_retry_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(1000));
                    ctx.request_repaint();
                }
            }
            #[cfg(not(windows))]
            {
                self.status = "Global hotkey and tray are available in the Windows build".into();
            }
            self.resident_initialized = true;
            RESIDENT_READY.store(true, Ordering::SeqCst);
        }

        self.icons.poll();
        self.previews.poll();

        while let Ok(status) = self.helper_status_rx.try_recv() {
            self.helper_status = Some(status);
        }
        if self.settings_open {
            let refresh_due = self
                .helper_status_last_requested
                .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(5));
            if refresh_due {
                self.request_helper_status();
            }
            // Repaint only while Settings is open; the worker also wakes the UI on replies.
            ctx.request_repaint_after(std::time::Duration::from_secs(5));
        }
        while let Ok((generation, rows)) = self.r_rx.try_recv() {
            if generation >= self.shown {
                // A later batch for the same query (the file results joining the instant ones)
                // keeps the selection where the user left it; a new query starts at the top.
                let new_query = generation > self.shown;
                self.shown = generation;
                self.set_rows(rows);
                self.selected = if new_query {
                    0
                } else {
                    self.selected.min(self.rows.len().saturating_sub(1))
                };
                if generation == self.gen {
                    self.pending = false;
                }
            }
        }

        if HIDE_REQUESTED.swap(false, Ordering::SeqCst) && LAUNCHER_OPEN.load(Ordering::SeqCst) {
            self.hide(ctx);
        }
        if self
            .show_requested
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.show(ctx);
        }
        if self
            .index_refreshed
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.search_again();
        }

        // Click anywhere outside (or switch away) and the launcher goes away, like Spotlight.
        // On Windows ask the OS which window is in the foreground: egui's own focus flag is fed
        // by window messages and can disagree with reality, which kept this from ever closing.
        #[cfg(windows)]
        let focused = foreground::is_ours();
        #[cfg(not(windows))]
        let focused = ctx.input(|input| input.viewport().focused).unwrap_or(false);
        if focused {
            if !self.had_focus {
                debug_log("focus gained");
            }
            self.had_focus = true;
        } else if self.had_focus {
            debug_log("focus lost -> hiding");
            self.hide(ctx);
        }
        if let Some(deadline) = self.focus_retry_until {
            if focused {
                self.focus_retry_until = None;
            } else if std::time::Instant::now() > deadline {
                // Windows refused to hand us the foreground. A launcher that cannot take typing,
                // and that clicking away could never dismiss, is worse than none: step aside.
                debug_log("focus never granted -> hiding");
                self.hide(ctx);
            } else {
                #[cfg(windows)]
                bring_to_front(false);
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint_after(std::time::Duration::from_millis(60));
            }
        }
        // Without the foreground hook, check on a slow timer (only while the launcher is open).
        #[cfg(windows)]
        if !self.foreground_hook && self.had_focus {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        let (down, up, enter, escape, quick) = ctx.input_mut(|input| {
            // Ctrl+1 ... Ctrl+9 open the matching row (the number is drawn on the row).
            let mut quick = None;
            for (slot, key) in [
                Key::Num1,
                Key::Num2,
                Key::Num3,
                Key::Num4,
                Key::Num5,
                Key::Num6,
                Key::Num7,
                Key::Num8,
                Key::Num9,
            ]
            .into_iter()
            .enumerate()
            {
                if input.consume_key(Modifiers::COMMAND, key) {
                    quick = Some(slot);
                }
            }
            (
                input.consume_key(Modifiers::NONE, Key::ArrowDown),
                input.consume_key(Modifiers::NONE, Key::ArrowUp),
                input.consume_key(Modifiers::NONE, Key::Enter),
                input.consume_key(Modifiers::NONE, Key::Escape),
                quick,
            )
        });
        let mut activate = None;
        let mut moved = false;
        if !self.settings_open {
            if down && !self.rows.is_empty() {
                self.selected = (self.selected + 1).min(self.rows.len() - 1);
                moved = true;
            }
            if up {
                self.selected = self.selected.saturating_sub(1);
                moved = true;
            }
            if enter && !self.rows.is_empty() {
                activate = Some(self.selected);
            }
            if let Some(slot) = quick {
                if slot < self.rows.len() {
                    activate = Some(slot);
                }
            }
        }
        if escape {
            if self.settings_open {
                self.settings_open = false;
            } else {
                self.hide(ctx);
            }
        }

        self.refresh_preview();
        let pal = palette(self.config.theme);
        let height = self.desired_height();
        // Windows ignores the window's transparency here (the unused part showed up black), so
        // instead the window is cut to the panel's rounded shape. Same fixed window size, no
        // resizing, and clicks in the cut-away part fall through to whatever is underneath.
        #[cfg(windows)]
        if (height - self.region_height).abs() > 0.5 {
            self.region_height = height;
            set_window_region(height);
        }
        let mut panel_rect: Option<egui::Rect> = None;

        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                // Paint the rounded panel at the height the content needs and lay the content out
                // inside it. The window itself stays one fixed size, so nothing ever resizes.
                let full = ui.max_rect();
                let panel = egui::Rect::from_min_size(full.min, Vec2::new(full.width(), height));
                ui.painter()
                    .rect(panel, 14.0_f32, pal.bg, Stroke::new(1.0_f32, pal.border));
                panel_rect = Some(panel);
                let mut content = ui.child_ui(
                    panel.shrink2(Vec2::new(16.0, 10.0)),
                    egui::Layout::top_down(egui::Align::LEFT),
                );
                let ui = &mut content;
                if self.settings_open {
                    self.settings_ui(ui);
                    return;
                }

                // Search row: magnifier (also the drag handle), borderless field, settings gear.
                ui.horizontal(|ui| {
                    ui.set_min_height(46.0);
                    let (icon_rect, icon_response) =
                        ui.allocate_exact_size(Vec2::splat(28.0), Sense::drag());
                    if icon_response.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    paint_magnifier(ui, icon_rect.center(), pal.muted);

                    let field_width = (ui.available_width() - 36.0).max(100.0);
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text(RichText::new("Search").size(24.0).color(pal.muted))
                            .font(FontId::proportional(24.0))
                            .text_color(pal.text)
                            .frame(false)
                            .desired_width(field_width),
                    );
                    response.request_focus();
                    if response.changed() {
                        self.status.clear();
                        self.search_again();
                    }

                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("\u{2699}").size(18.0).color(pal.muted),
                            )
                            .frame(false),
                        )
                        .on_hover_text("Settings")
                        .clicked()
                    {
                        self.settings_open = true;
                        self.request_helper_status();
                        ctx.request_repaint();
                    }
                });

                if !self.status.is_empty() {
                    // One line only: the panel height assumes it, so a long message is cut, not wrapped.
                    ui.add(
                        egui::Label::new(RichText::new(&self.status).size(12.0).color(pal.muted))
                            .truncate(true),
                    );
                }
                // "No Results" only once the search has actually answered, never in between keystrokes.
                let searching = !self.query.trim().is_empty() && !self.pending;
                if self.rows.is_empty() {
                    if searching {
                        ui.separator();
                        ui.add_space(10.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("No Results").size(16.0).color(pal.muted));
                        });
                    }
                } else {
                    ui.separator();
                    let selected = self.selected;
                    let mut clicked = None;
                    // Left: the result list. Right: the preview of the selected result.
                    let area = ui.available_rect_before_wrap();
                    let list_rect =
                        egui::Rect::from_min_size(area.min, Vec2::new(LIST_WIDTH, area.height()));
                    let preview_rect = egui::Rect::from_min_max(
                        Pos2::new(list_rect.right() + 16.0, area.top()),
                        area.max,
                    );
                    ui.painter().vline(
                        list_rect.right() + 8.0,
                        area.y_range(),
                        Stroke::new(1.0_f32, pal.sep),
                    );
                    let mut list_ui = ui.child_ui_with_id_source(
                        list_rect,
                        egui::Layout::top_down(egui::Align::LEFT),
                        "okra-list",
                    );
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(&mut list_ui, |ui| {
                            let mut prev: Option<Kind> = None;
                            for (index, row) in self.rows.iter().enumerate() {
                                let item = &row.item;
                                let heading = if index == 0 {
                                    Some("Top Hit")
                                } else if prev != Some(item.kind) {
                                    Some(kind_heading(item.kind))
                                } else {
                                    None
                                };
                                prev = Some(item.kind);
                                if let Some(heading) = heading {
                                    ui.add_space(if index == 0 { 2.0 } else { 8.0 });
                                    ui.label(
                                        RichText::new(heading).size(12.0).strong().color(pal.muted),
                                    );
                                    ui.add_space(2.0);
                                }

                                let (rect, response) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), ROW_HEIGHT),
                                    Sense::click(),
                                );
                                let is_selected = index == selected;
                                if is_selected {
                                    ui.painter().rect_filled(rect, 8.0_f32, ACCENT);
                                } else if response.hovered()
                                    || ui.rect_contains_pointer(rect.expand2(Vec2::new(0.0, 1.0)))
                                {
                                    // Also lit in the 2 px gap between rows, so the highlight never blinks off.
                                    ui.painter().rect_filled(rect, 8.0_f32, pal.hover);
                                }
                                let (title_color, sub_color) = if is_selected {
                                    (
                                        Color32::WHITE,
                                        Color32::from_rgba_unmultiplied(255, 255, 255, 190),
                                    )
                                } else {
                                    (pal.text, pal.muted)
                                };

                                let tile = egui::Rect::from_min_size(
                                    Pos2::new(rect.left() + 8.0, rect.center().y - 17.0),
                                    Vec2::splat(34.0),
                                );
                                match row
                                    .icon
                                    .as_ref()
                                    .and_then(|(key, _)| self.icons.texture(key))
                                {
                                    Some(texture) => {
                                        ui.painter().image(
                                            texture.id(),
                                            tile,
                                            egui::Rect::from_min_max(
                                                Pos2::ZERO,
                                                Pos2::new(1.0, 1.0),
                                            ),
                                            Color32::WHITE,
                                        );
                                    }
                                    None => {
                                        // Until the real icon arrives (or for items that have none).
                                        ui.painter().rect_filled(tile, 8.0_f32, tile_color(item));
                                        let letter = item
                                            .title
                                            .chars()
                                            .next()
                                            .map(|c| c.to_uppercase().to_string())
                                            .unwrap_or_default();
                                        ui.painter().text(
                                            tile.center(),
                                            Align2::CENTER_CENTER,
                                            letter,
                                            FontId::proportional(17.0),
                                            Color32::WHITE,
                                        );
                                    }
                                }

                                let text_x = tile.right() + 12.0;
                                let text_painter = ui
                                    .painter()
                                    .with_clip_rect(rect.shrink2(Vec2::new(4.0, 0.0)));
                                // Leave room on the right of the first nine rows for their Ctrl+N hint.
                                let hint_room = if index < 9 { 44.0 } else { 0.0 };
                                let title_rect = egui::Rect::from_min_max(
                                    Pos2::new(text_x, rect.center().y - 18.0),
                                    Pos2::new(
                                        rect.right() - 12.0 - hint_room,
                                        rect.center().y + 2.0,
                                    ),
                                );
                                let mut title_ui = ui.child_ui(
                                    title_rect,
                                    egui::Layout::left_to_right(egui::Align::Center),
                                );
                                title_ui.add(
                                    egui::Label::new(title_job(
                                        row,
                                        title_color,
                                        is_selected,
                                        item.kind == Kind::Calculation,
                                    ))
                                    .truncate(true),
                                );
                                text_painter.text(
                                    Pos2::new(text_x, rect.center().y + 10.0),
                                    Align2::LEFT_CENTER,
                                    &row.subtitle,
                                    FontId::proportional(12.0),
                                    sub_color,
                                );

                                if index < 9 {
                                    text_painter.text(
                                        Pos2::new(rect.right() - 12.0, rect.center().y - 8.0),
                                        Align2::RIGHT_CENTER,
                                        format!("Ctrl+{}", index + 1),
                                        FontId::proportional(11.5),
                                        sub_color,
                                    );
                                }
                                if response.clicked() {
                                    clicked = Some(index);
                                }
                                if moved && is_selected {
                                    ui.scroll_to_rect(rect, None);
                                }
                                ui.add_space(2.0);
                            }
                        });
                    let mut preview_ui = ui.child_ui_with_id_source(
                        preview_rect,
                        egui::Layout::top_down(egui::Align::LEFT),
                        "okra-preview",
                    );
                    self.draw_preview(&mut preview_ui, pal);
                    if let Some(index) = clicked {
                        activate = Some(index);
                    }
                }
            });

        // The window is taller than the panel; a click in the transparent part is a click outside.
        if let Some(rect) = panel_rect {
            let pressed_outside = ctx.input(|input| {
                input.pointer.any_pressed()
                    && input
                        .pointer
                        .interact_pos()
                        .is_some_and(|pos| !rect.contains(pos))
            });
            if pressed_outside {
                self.hide(ctx);
            }
        }

        if let Some(index) = activate {
            self.activate(ctx, index);
        }
    }
}

fn apply_theme(ctx: &egui::Context, theme: ThemeKind) {
    let pal = palette(theme);
    let mut visuals = match theme {
        ThemeKind::WarmLight => egui::Visuals::light(),
        ThemeKind::Dark => egui::Visuals::dark(),
    };
    visuals.window_rounding = 12.0.into();
    visuals.panel_fill = pal.bg;
    visuals.window_fill = pal.bg;
    visuals.override_text_color = Some(pal.text);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, pal.sep);
    visuals.widgets.noninteractive.bg_fill = pal.bg;
    visuals.selection.bg_fill = ACCENT.linear_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 4.0);
        style.interaction.selectable_labels = false;
    });
}

fn palette(theme: ThemeKind) -> Palette {
    match theme {
        ThemeKind::WarmLight => Palette {
            bg: Color32::from_rgb(246, 246, 248),
            border: Color32::from_rgb(214, 214, 219),
            text: Color32::from_rgb(28, 28, 30),
            muted: Color32::from_rgb(120, 120, 128),
            sep: Color32::from_rgb(223, 223, 228),
            hover: Color32::from_rgb(232, 232, 237),
        },
        ThemeKind::Dark => Palette {
            bg: Color32::from_rgb(38, 38, 41),
            border: Color32::from_rgb(70, 70, 75),
            text: Color32::from_rgb(245, 245, 247),
            muted: Color32::from_rgb(152, 152, 158),
            sep: Color32::from_rgb(62, 62, 66),
            hover: Color32::from_rgb(54, 54, 58),
        },
    }
}

fn kind_heading(kind: Kind) -> &'static str {
    match kind {
        Kind::App => "Applications",
        Kind::File => "Documents",
        Kind::Folder => "Folders",
        Kind::Setting => "System Settings",
        Kind::Window => "Open Windows",
        Kind::Calculation => "Calculator",
    }
}

/// Apps get a stable per-name colour so the list reads as distinct icons rather than a grey wall.
fn tile_color(item: &Item) -> Color32 {
    const APP_COLORS: [(u8, u8, u8); 8] = [
        (88, 86, 214),
        (255, 149, 0),
        (52, 199, 89),
        (255, 69, 58),
        (50, 173, 230),
        (175, 82, 222),
        (255, 55, 95),
        (48, 176, 199),
    ];
    let (r, g, b) = match item.kind {
        Kind::App => {
            let hash = item.title.bytes().fold(5381u32, |acc, byte| {
                acc.wrapping_mul(33).wrapping_add(byte as u32)
            });
            APP_COLORS[hash as usize % APP_COLORS.len()]
        }
        Kind::File => (142, 142, 147),
        Kind::Folder => (50, 173, 230),
        Kind::Setting => (99, 99, 102),
        Kind::Window => (52, 199, 89),
        Kind::Calculation => (255, 149, 0),
    };
    Color32::from_rgb(r, g, b)
}

/// The text after a provider prefix such as `f ` or `win `, which is what is actually matched.
fn strip_route_prefix(query: &str) -> &str {
    let query = query.trim_start();
    for prefix in ["win ", "a ", "f ", "s "] {
        if let Some(rest) = query.strip_prefix(prefix) {
            return rest.trim();
        }
    }
    query.trim()
}

/// Title text with the matched letters emphasised: a handful of text runs per title, built
/// from the ranges computed once in [`build_rows`].
fn title_job(row: &Row, color: Color32, selected: bool, large: bool) -> egui::text::LayoutJob {
    let size = if large { 20.0 } else { 15.5 };
    let plain = egui::TextFormat {
        font_id: FontId::proportional(size),
        color,
        ..Default::default()
    };
    let mut emphasised = plain.clone();
    if selected {
        emphasised.underline = Stroke::new(1.0_f32, color);
    } else {
        emphasised.color = ACCENT;
    }
    let mut job = egui::text::LayoutJob::default();
    for &(start, end, matched) in &row.title_runs {
        job.append(
            &row.item.title[start..end],
            0.0,
            if matched {
                emphasised.clone()
            } else {
                plain.clone()
            },
        );
    }
    job
}

/// Merges per-character matches (`matched` = ascending char indices) into byte-range runs.
fn highlight_runs(title: &str, matched: &[usize]) -> Vec<(usize, usize, bool)> {
    let mut runs = Vec::new();
    let mut current: Option<(usize, bool)> = None;
    let mut next = 0;
    for (char_index, (byte_index, _)) in title.char_indices().enumerate() {
        let hit = next < matched.len() && matched[next] == char_index;
        if hit {
            next += 1;
        }
        current = match current {
            Some((start, flag)) if flag != hit => {
                runs.push((start, byte_index, flag));
                Some((byte_index, hit))
            }
            Some(run) => Some(run),
            None => Some((byte_index, hit)),
        };
    }
    if let Some((start, flag)) = current {
        runs.push((start, title.len(), flag));
    }
    runs
}

fn build_rows(items: Vec<Item>, query: &str) -> Vec<Row> {
    let highlight = Query::new(strip_route_prefix(query));
    items
        .into_iter()
        .map(|item| {
            let matched = if highlight.is_empty() || item.kind == Kind::Calculation {
                Vec::new()
            } else {
                fuzzy_with(&highlight, &item.title)
                    .map(|m| m.indices)
                    .unwrap_or_default()
            };
            let subtitle = if item.subtitle.is_empty() {
                item.kind.label().to_string()
            } else {
                shorten(&item.subtitle, 40)
            };
            let title_runs = highlight_runs(&item.title, &matched);
            let icon = icons::icon_request(&item);
            Row {
                item,
                subtitle,
                title_runs,
                icon,
            }
        })
        .collect()
}

/// Preview rows that need no disk access: where an app lives, what a setting opens, how often the
/// result was opened before.
fn build_extra(engine: &Engine, item: &Item) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    match (&item.kind, &item.action) {
        (Kind::App, Action::Launch { path, .. }) => rows.push((
            "Where".to_string(),
            shorten(&path.display().to_string(), 52),
        )),
        (Kind::App, Action::LaunchAppId(_)) => {
            rows.push(("Source".to_string(), "Microsoft Store app".to_string()))
        }
        (Kind::Setting, Action::OpenUri(uri)) => rows.push(("Opens".to_string(), uri.clone())),
        (Kind::Setting, Action::Run { program, args }) => {
            let command = if args.is_empty() {
                program.clone()
            } else {
                format!("{program} {}", args.join(" "))
            };
            rows.push(("Runs".to_string(), shorten(&command, 52)));
        }
        (Kind::Window, _) => rows.push(("Details".to_string(), item.subtitle.clone())),
        (Kind::Calculation, _) => {
            let expression = item
                .subtitle
                .split('\u{b7}')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            rows.push(("Expression".to_string(), expression));
            rows.push(("Press Enter".to_string(), "to copy the answer".to_string()));
        }
        _ => {}
    }
    if item.kind != Kind::Calculation {
        if let Some((count, last)) = engine.usage(&item.id) {
            let times = if count == 1 {
                "1 time".to_string()
            } else {
                format!("{count} times")
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            rows.push((
                "Opened".to_string(),
                if last > 0 {
                    format!("{times}, last {}", ago(now.saturating_sub(last)))
                } else {
                    times
                },
            ));
        }
    }
    rows
}

/// `just now`, `5 min ago`, `3 h ago`, `2 days ago`.
fn ago(seconds: u64) -> String {
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{} min ago", seconds / 60),
        3_600..=86_399 => format!("{} h ago", seconds / 3_600),
        _ => {
            let days = seconds / 86_400;
            if days == 1 {
                "1 day ago".to_string()
            } else {
                format!("{days} days ago")
            }
        }
    }
}

/// Keeps the tail of long paths, which is the informative part.
fn shorten(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let tail: String = text.chars().skip(count - (max_chars - 1)).collect();
    format!("\u{2026}{tail}")
}

fn paint_magnifier(ui: &egui::Ui, center: Pos2, color: Color32) {
    let painter = ui.painter();
    painter.circle_stroke(
        center + Vec2::new(-2.0, -2.0),
        7.5,
        Stroke::new(2.2_f32, color),
    );
    painter.line_segment(
        [center + Vec2::new(3.5, 3.5), center + Vec2::new(9.5, 9.5)],
        Stroke::new(2.4_f32, color),
    );
}

/// Set `OKRA_DEBUG=1` to append show/hide/focus transitions to `%TEMP%\okra-debug.log`
/// (the release build has no console). Does nothing otherwise.
fn debug_log(message: &str) {
    use std::io::Write;
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let started = *START.get_or_init(std::time::Instant::now);
    if std::env::var_os("OKRA_DEBUG").is_none() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("okra-debug.log"))
    {
        let _ = writeln!(file, "{:>9.3}s {message}", started.elapsed().as_secs_f64());
    }
}

/// Windows' own idea of "which window is in the foreground", independent of egui/winit bookkeeping.
#[cfg(windows)]
mod foreground {
    use eframe::egui;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;

    type WinEventProc =
        unsafe extern "system" fn(*mut c_void, u32, *mut c_void, i32, i32, u32, u32);

    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> *mut c_void;
        fn GetWindowThreadProcessId(hwnd: *mut c_void, process_id: *mut u32) -> u32;
        fn SetWinEventHook(
            event_min: u32,
            event_max: u32,
            module: *mut c_void,
            callback: Option<WinEventProc>,
            process_id: u32,
            thread_id: u32,
            flags: u32,
        ) -> *mut c_void;
    }
    const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
    const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
    const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;

    static CONTEXT: OnceLock<egui::Context> = OnceLock::new();
    static WATCHING: AtomicBool = AtomicBool::new(false);

    /// Only wake the UI while the launcher is open; the decision itself is made in `update`.
    pub fn watch(on: bool) {
        WATCHING.store(on, Ordering::Relaxed);
    }

    unsafe extern "system" fn on_foreground_changed(
        _: *mut c_void,
        _: u32,
        _: *mut c_void,
        _: i32,
        _: i32,
        _: u32,
        _: u32,
    ) {
        if WATCHING.load(Ordering::Relaxed) {
            if let Some(ctx) = CONTEXT.get() {
                ctx.request_repaint();
            }
        }
    }

    /// Asks Windows to tell this (UI) thread whenever a window of another process becomes the
    /// foreground, so clicking away closes the launcher instantly with no polling. The callback
    /// is delivered through the thread's message loop, which winit is already running.
    pub fn install(ctx: &egui::Context) -> bool {
        let _ = CONTEXT.set(ctx.clone());
        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                std::ptr::null_mut(),
                Some(on_foreground_changed),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        !hook.is_null()
    }

    /// Is the current foreground window ours? A momentarily null foreground (mid-switch) counts as
    /// "yes" so a transient state never closes the launcher.
    pub fn is_ours() -> bool {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return true;
            }
            let mut process_id = 0u32;
            GetWindowThreadProcessId(hwnd, &mut process_id);
            process_id == std::process::id()
        }
    }
}

/// Inter (SIL Open Font License, see `assets/fonts/`): a clean, modern UI typeface in the spirit of
/// the macOS system font. It is bundled, so it looks the same everywhere and costs no file read at
/// startup. On Windows, Segoe UI and Nirmala UI are kept as fallbacks for scripts Inter lacks
/// (for example Devanagari file names).
fn load_fonts(ctx: &egui::Context, kind: FontKind) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "inter".to_owned(),
        egui::FontData::from_static(include_bytes!("../../../assets/fonts/Inter-Regular.otf")),
    );
    // System fonts found on this machine, in fallback order (empty off Windows).
    #[allow(unused_mut)]
    let mut system: Vec<String> = Vec::new();
    #[cfg(windows)]
    {
        let dir = std::env::var_os("WINDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        for (key, file) in [("segoe-ui", "segoeui.ttf"), ("nirmala-ui", "Nirmala.ttf")] {
            if let Ok(bytes) = std::fs::read(dir.join(file)) {
                fonts
                    .font_data
                    .insert(key.to_owned(), egui::FontData::from_owned(bytes));
                system.push(key.to_owned());
            }
        }
    }
    // Whichever is chosen goes first; the other stays behind it for characters it lacks.
    let mut order: Vec<String> = Vec::new();
    match kind {
        FontKind::Inter => {
            order.push("inter".to_owned());
            order.extend(system);
        }
        FontKind::SegoeUi => {
            order.extend(system);
            order.push("inter".to_owned());
        }
    }
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        for (position, name) in order.into_iter().enumerate() {
            family.insert(position.min(family.len()), name);
        }
    }
    ctx.set_fonts(fonts);
}

/// The launcher's own top-level window. Looked up among this process's windows (so another
/// program's window that happens to have the same title can never be mistaken for it) and
/// remembered, so the lookup is not repeated on every show.
#[cfg(windows)]
mod own_window {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicIsize, Ordering};

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(
            callback: Option<unsafe extern "system" fn(*mut c_void, isize) -> i32>,
            data: isize,
        ) -> i32;
        fn GetWindowThreadProcessId(hwnd: *mut c_void, process_id: *mut u32) -> u32;
        fn GetWindowTextW(hwnd: *mut c_void, text: *mut u16, max_count: i32) -> i32;
        fn IsWindow(hwnd: *mut c_void) -> i32;
    }

    static CACHED: AtomicIsize = AtomicIsize::new(0);

    struct Search {
        process_id: u32,
        found: isize,
    }

    unsafe extern "system" fn visit(hwnd: *mut c_void, data: isize) -> i32 {
        let search = &mut *(data as *mut Search);
        let mut process_id = 0u32;
        GetWindowThreadProcessId(hwnd, &mut process_id);
        if process_id != search.process_id {
            return 1;
        }
        let mut buffer = [0u16; 64];
        let length = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        if length > 0 && String::from_utf16_lossy(&buffer[..length as usize]) == super::WINDOW_TITLE
        {
            search.found = hwnd as isize;
            return 0; // stop enumerating
        }
        1
    }

    /// Null if the window does not exist (yet).
    pub fn handle() -> *mut c_void {
        let cached = CACHED.load(Ordering::Relaxed);
        if cached != 0 && unsafe { IsWindow(cached as *mut c_void) } != 0 {
            return cached as *mut c_void;
        }
        let mut search = Search {
            process_id: std::process::id(),
            found: 0,
        };
        unsafe {
            EnumWindows(Some(visit), &mut search as *mut Search as isize);
        }
        CACHED.store(search.found, Ordering::Relaxed);
        search.found as *mut c_void
    }
}

/// Clip the window to a rounded rectangle `height` points tall, anchored at its top edge.
#[cfg(windows)]
fn set_window_region(height: f32) {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetWindowRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
        fn SetWindowRgn(hwnd: *mut c_void, region: *mut c_void, redraw: i32) -> i32;
    }
    #[link(name = "gdi32")]
    extern "system" {
        fn CreateRoundRectRgn(
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
            ellipse_width: i32,
            ellipse_height: i32,
        ) -> *mut c_void;
    }

    unsafe {
        let hwnd = own_window::handle();
        if hwnd.is_null() {
            return;
        }
        let mut rect = Rect::default();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return;
        }
        let width = rect.right - rect.left;
        if width <= 0 {
            return;
        }
        let scale = width as f32 / WINDOW_WIDTH;
        let bottom = (height * scale).round() as i32;
        let diameter = (28.0 * scale).round() as i32; // 14 pt corner radius, as the panel is painted
        let region = CreateRoundRectRgn(0, 0, width + 1, bottom + 1, diameter, diameter);
        if !region.is_null() {
            // The system owns the region after this call.
            SetWindowRgn(hwnd, region, 1);
        }
    }
}

/// Windows only lets the process that currently owns the foreground steal focus, so after another
/// app was launched from here a plain "focus" request is silently refused and the launcher shows
/// up unfocused. This borrows the foreground thread's input right (the usual launcher trick).
#[cfg(windows)]
fn bring_to_front(reposition: bool) {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn ShowWindow(hwnd: *mut c_void, command: i32) -> i32;
        fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
        fn GetForegroundWindow() -> *mut c_void;
        fn GetWindowThreadProcessId(hwnd: *mut c_void, process_id: *mut u32) -> u32;
        fn AttachThreadInput(from: u32, to: u32, attach: i32) -> i32;
        fn keybd_event(vk: u8, scan: u8, flags: u32, extra: usize);
        fn GetCursorPos(point: *mut Point) -> i32;
        fn MonitorFromPoint(point: Point, flags: u32) -> *mut c_void;
        fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
        fn GetWindowRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
        fn SetWindowPos(
            hwnd: *mut c_void,
            after: *mut c_void,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    const SW_SHOW: i32 = 5;
    const VK_MENU: u8 = 0x12;
    /// An unassigned virtual key: counts as input, but does nothing in the app underneath.
    const VK_UNASSIGNED: u8 = 0xE8;
    const KEYEVENTF_KEYUP: u32 = 2;
    const MONITOR_DEFAULTTONEAREST: u32 = 2;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;

    unsafe {
        let hwnd = own_window::handle();
        if hwnd.is_null() {
            return;
        }

        if reposition {
            // Centre the search bar on the screen the mouse is on. Everything here is in physical
            // pixels, so the window's own width tells us the DPI scale. The bar sits at the exact
            // centre; the result list is drawn below it inside the fixed-size window. On a short
            // screen the top edge is lifted just enough to keep the whole window on screen.
            let mut cursor = Point::default();
            let mut window = Rect::default();
            let mut info = MonitorInfo {
                size: std::mem::size_of::<MonitorInfo>() as u32,
                monitor: Rect::default(),
                work: Rect::default(),
                flags: 0,
            };
            if GetCursorPos(&mut cursor) != 0 {
                let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
                if !monitor.is_null()
                    && GetMonitorInfoW(monitor, &mut info) != 0
                    && GetWindowRect(hwnd, &mut window) != 0
                {
                    let width = window.right - window.left;
                    if width > 0 {
                        let scale = width as f32 / WINDOW_WIDTH;
                        let bar = (BAR_HEIGHT * scale).round() as i32;
                        let tallest = (WINDOW_HEIGHT * scale).round() as i32;
                        let work = info.work;
                        let x = work.left + ((work.right - work.left) - width) / 2;
                        let centred_y = work.top + ((work.bottom - work.top) - bar) / 2;
                        let y = centred_y.min(work.bottom - tallest).max(work.top);
                        SetWindowPos(
                            hwnd,
                            std::ptr::null_mut(),
                            x,
                            y,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
            }
        }

        ShowWindow(hwnd, SW_SHOW);
        if GetForegroundWindow() == hwnd {
            return;
        }
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, std::ptr::null_mut())
        };
        let ours = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != ours
            && AttachThreadInput(ours, foreground_thread, 1) != 0;
        if reposition {
            // A synthetic key event marks this process as having just received input, which is
            // what lets SetForegroundWindow succeed. Only on the first attempt: repeating it on
            // every retry would keep poking the app underneath. The key is an unassigned one, so
            // unlike an Alt tap it cannot open the menu bar of the app that had the focus.
            keybd_event(VK_UNASSIGNED, 0, 0, 0);
            keybd_event(VK_UNASSIGNED, 0, KEYEVENTF_KEYUP, 0);
        }
        SetForegroundWindow(hwnd);
        if reposition && GetForegroundWindow() != hwnd {
            // Last resort, as before: an Alt tap. It only runs when the quiet way was refused.
            keybd_event(VK_MENU, 0, 0, 0);
            keybd_event(VK_MENU, 0, KEYEVENTF_KEYUP, 0);
            SetForegroundWindow(hwnd);
        }
        if attached {
            AttachThreadInput(ours, foreground_thread, 0);
        }
    }
}

#[cfg(windows)]
fn already_running() -> bool {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::OnceLock;
    const ERROR_ALREADY_EXISTS: u32 = 183;
    static INSTANCE_MUTEX: OnceLock<isize> = OnceLock::new();
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(
            attributes: *mut c_void,
            initial_owner: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn GetLastError() -> u32;
        fn SetLastError(error: u32);
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    let name: Vec<u16> = std::ffi::OsStr::new("Local\\OkraBrownSearch")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        SetLastError(0);
        let handle = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
        if handle.is_null() {
            return false;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            true
        } else {
            let _ = INSTANCE_MUTEX.set(handle as isize);
            false
        }
    }
}

#[cfg(windows)]
fn set_startup_registration(enabled: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let mut command = std::process::Command::new("reg.exe");
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    if enabled {
        let executable = std::env::current_exe()
            .map_err(|e| format!("Could not locate the app executable: {e}"))?;
        let value = format!("\"{}\" --background", executable.display());
        command.args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "OkraBrownSearch",
            "/t",
            "REG_SZ",
            "/d",
            value.as_str(),
            "/f",
        ]);
    } else {
        command.args([
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "OkraBrownSearch",
            "/f",
        ]);
    }
    let output = command
        .output()
        .map_err(|e| format!("Could not update Windows sign-in startup: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Windows did not accept the startup setting: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// True when the launcher was started with the environment variable `OKRA_OPAQUE` set.
fn opaque_mode() -> bool {
    static OPAQUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OPAQUE.get_or_init(|| std::env::var_os("OKRA_OPAQUE").is_some())
}

fn main() -> eframe::Result<()> {
    #[cfg(windows)]
    if already_running() {
        return Ok(());
    }
    let start_hidden = std::env::args().any(|arg| arg == "--background");
    let config = Config::load_or_default();
    let engine = Arc::new(Engine::new(&config));
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(WINDOW_TITLE)
        .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
        .with_decorations(false)
        .with_transparent(!opaque_mode())
        .with_min_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
        .with_max_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
        .with_always_on_top()
        // Launched with --background (sign-in start): never show the window, so there is no flash.
        .with_visible(!start_hidden);

    if let Ok(icon) =
        image::load_from_memory(include_bytes!("../../../assets/okra-brown-search.png"))
    {
        let rgba = icon.to_rgba8();
        let (width, height) = rgba.dimensions();
        viewport = viewport.with_icon(Arc::new(egui::IconData {
            rgba: rgba.into_raw(),
            width,
            height,
        }));
    }

    let options = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Okra Brown Search",
        options,
        Box::new(move |cc| Box::new(App::new(cc, engine, config, start_hidden))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use okra_core::{Action, Kind};

    fn item(title: &str, kind: Kind) -> Item {
        Item {
            id: title.into(),
            title: title.into(),
            subtitle: String::new(),
            kind,
            action: Action::Copy(String::new()),
            score: 0,
        }
    }

    #[test]
    fn runs_cover_the_title_exactly_and_flag_matches() {
        let title = "Workbench 2023 R1";
        let runs = highlight_runs(title, &[0, 1, 2, 3]);
        assert_eq!(runs, vec![(0, 4, true), (4, title.len(), false)]);
        // No gaps or overlaps, whatever the match pattern (incl. multi-byte characters).
        for (title, matched) in [
            ("Éclair café", vec![0usize, 3, 4, 10]),
            ("abc", vec![]),
            ("abc", vec![0, 1, 2]),
            ("", vec![]),
        ] {
            let runs = highlight_runs(title, &matched);
            let rebuilt: String = runs.iter().map(|&(a, b, _)| &title[a..b]).collect();
            assert_eq!(rebuilt, title);
            let flagged: usize = runs
                .iter()
                .filter(|r| r.2)
                .map(|&(a, b, _)| title[a..b].chars().count())
                .sum();
            assert_eq!(flagged, matched.len());
        }
    }

    #[test]
    fn rows_highlight_the_query_that_produced_them() {
        let rows = build_rows(
            vec![
                item("Workbench 2023 R1", Kind::App),
                item("2+2", Kind::Calculation),
            ],
            "a work",
        );
        assert_eq!(
            rows[0].title_runs.iter().filter(|r| r.2).count(),
            1,
            "prefix `a ` is not highlighted"
        );
        assert!(
            rows[0].title_runs[0].2 && rows[0].title_runs[0].1 == 4,
            "\"work\" highlighted"
        );
        assert!(
            rows[1].title_runs.iter().all(|r| !r.2),
            "calculations are never highlighted"
        );
        assert_eq!(
            rows[0].subtitle,
            Kind::App.label(),
            "empty subtitle falls back to the kind label"
        );
        assert!(build_rows(vec![item("Notepad", Kind::App)], "")[0]
            .title_runs
            .iter()
            .all(|r| !r.2));
    }
}
