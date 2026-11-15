#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use eframe::egui::{self, Align2, Color32, FontId, Key, Modifiers, Pos2, RichText, Sense, Stroke, Vec2};
use okra_core::matcher::{fuzzy_with, Query};
use okra_core::{Config, Engine, FileBackendKind, Item, Kind, Outcome, ThemeKind};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

mod icons;

/// macOS system blue. Change this one constant to re-tint the selection and section accents.
const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);
const WINDOW_WIDTH: f32 = 680.0;
const BAR_HEIGHT: f32 = 66.0;
const ROW_HEIGHT: f32 = 48.0;
const MAX_LIST_HEIGHT: f32 = 400.0;
const SETTINGS_HEIGHT: f32 = 540.0;

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
    selected: usize,
    status: String,
    gen: u64,
    shown: u64,
    /// Generation of the newest query; the search thread polls it to drop work that is stale.
    latest_gen: Arc<AtomicU64>,
    /// A query is in flight and nothing has come back for it yet (so "No Results" is premature).
    pending: bool,
    q_tx: Sender<(u64, String)>,
    r_rx: Receiver<(u64, Vec<Row>)>,
    settings_open: bool,
    show_requested: Arc<std::sync::atomic::AtomicBool>,
    /// Set by the background app scanner when a fresh application list is available.
    index_refreshed: Arc<std::sync::atomic::AtomicBool>,
    /// Windows wakes us when another window takes the foreground (see `foreground`); if the hook
    /// could not be installed we fall back to a slow poll while the launcher is open.
    #[cfg(windows)]
    foreground_hook: bool,
    start_hidden: bool,
    resident_initialized: bool,
    last_height: f32,
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
    fn new(ctx: &egui::Context, show_requested: Arc<std::sync::atomic::AtomicBool>) -> (Self, String) {
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
                        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
                            if event.id == hotkey_id && event.state == HotKeyState::Pressed {
                                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                                bring_to_front(true);
context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                                context.send_viewport_cmd(egui::ViewportCommand::Focus);
                                context.request_repaint();
                            }
                        }));
                        Some(manager)
                    }
                    Err(error) => {
                        messages.push(format!("Global hotkey unavailable: {error}"));
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
            let icon = Icon::from_rgba(image.into_raw(), width, height).map_err(|e| e.to_string())?;

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
                let should_show = match event {
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { .. } => true,
                    _ => false,
                };
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
        (Self { _hotkey_manager: hotkey_manager, _tray_icon: tray_icon }, status)
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, engine: Arc<Engine>, config: Config, start_hidden: bool) -> Self {
        load_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx, config.theme);

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
                eng.search_progressive(&query, &mut emit, &|| newest.load(Ordering::Relaxed) != generation);
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
            selected: 0,
            status: String::new(),
            gen: 0,
            shown: 0,
            latest_gen,
            pending: false,
            q_tx,
            r_rx,
            settings_open: false,
            show_requested,
            index_refreshed,
            #[cfg(windows)]
            foreground_hook: false,
            start_hidden,
            resident_initialized: false,
            last_height: 0.0,
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
        self.icons.request(rows.iter().filter_map(|row| row.icon.as_ref()));
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
        self.had_focus = false;
        self.reset_results();
        debug_log("show");
        #[cfg(windows)]
        {
            foreground::watch(true);
            bring_to_front(false);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.focus_retry_until = Some(std::time::Instant::now() + std::time::Duration::from_millis(1000));
        ctx.request_repaint();
    }

    fn hide(&mut self, ctx: &egui::Context) {
        debug_log("hide");
        #[cfg(windows)]
        foreground::watch(false);
        self.had_focus = false;
        self.focus_retry_until = None;
        // Leave nothing behind: the next open must show an empty bar at bar height, not flash the
        // previous query's tall window for a frame.
        self.settings_open = false;
        self.query.clear();
        self.reset_results();
        self.last_height = BAR_HEIGHT;
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(WINDOW_WIDTH, BAR_HEIGHT)));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn activate(&mut self, ctx: &egui::Context, idx: usize) {
        let Some(item) = self.rows.get(idx).map(|row| row.item.clone()) else { return };
        match self.engine.activate_for(&item, &self.query) {
            Ok(Outcome::CopyText(text)) => {
                ctx.output_mut(|output| output.copied_text = text);
                self.hide(ctx);
            }
            Ok(Outcome::Done) => self.hide(ctx),
            Err(error) => self.status = error,
        }
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

            section(ui, "File search");
            egui::ComboBox::from_id_source("okra-file-backend")
                .selected_text(self.config.file_backend.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.config.file_backend, FileBackendKind::Auto, "Automatic (recommended)");
                    ui.selectable_value(&mut self.config.file_backend, FileBackendKind::Everything, "Everything only");
                    ui.selectable_value(&mut self.config.file_backend, FileBackendKind::Builtin, "Built-in index only");
                });
            ui.label(RichText::new("Automatic uses the Everything SDK, then es.exe, then the local index.").size(12.0).color(pal.muted));

            section(ui, "Results");
            ui.add(egui::Slider::new(&mut self.config.max_results, 10..=80).text("Maximum results"));

            section(ui, "General");
            ui.checkbox(&mut self.config.start_with_windows, "Open at login");
            ui.add_space(4.0);
            ui.label(RichText::new("Shortcut: Ctrl + Alt + Space   ·   Esc hides the window").size(12.0).color(pal.muted));
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

    /// Window height for the current state: just the search bar when idle, growing with results.
    fn desired_height(&self) -> f32 {
        if self.settings_open {
            return SETTINGS_HEIGHT;
        }
        let status = if self.status.is_empty() { 0.0 } else { 22.0 };
        if self.rows.is_empty() {
            return if self.query.trim().is_empty() || self.pending { BAR_HEIGHT + status } else { BAR_HEIGHT + 64.0 + status };
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
        BAR_HEIGHT + 24.0 + status + total.min(MAX_LIST_HEIGHT)
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
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
                self.status = status;
                self.foreground_hook = foreground::install(ctx);
                debug_log(&format!("foreground hook installed: {}", self.foreground_hook));
                if self.start_hidden && !controls_failed {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                } else {
                    // Started by hand: centre it and arm the same focus and click-away handling
                    // as a hotkey open. Without this a launch that Windows declined to focus
                    // sat on screen forever: no focus, so no Esc, and no click-away to close it.
                    bring_to_front(true);
                    foreground::watch(true);
                    self.focus_retry_until = Some(std::time::Instant::now() + std::time::Duration::from_millis(1000));
                    ctx.request_repaint();
                }
            }
            #[cfg(not(windows))]
            {
                self.status = "Global hotkey and tray are available in the Windows build".into();
            }
            self.resident_initialized = true;
        }

        self.icons.poll();
        while let Ok((generation, rows)) = self.r_rx.try_recv() {
            if generation >= self.shown {
                // A later batch for the same query (the file results joining the instant ones)
                // keeps the selection where the user left it; a new query starts at the top.
                let new_query = generation > self.shown;
                self.shown = generation;
                self.set_rows(rows);
                self.selected = if new_query { 0 } else { self.selected.min(self.rows.len().saturating_sub(1)) };
                if generation == self.gen {
                    self.pending = false;
                }
            }
        }

        if self.show_requested.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.show(ctx);
        }
        if self.index_refreshed.swap(false, std::sync::atomic::Ordering::SeqCst) {
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

        let (down, up, enter, escape) = ctx.input_mut(|input| {
            (
                input.consume_key(Modifiers::NONE, Key::ArrowDown),
                input.consume_key(Modifiers::NONE, Key::ArrowUp),
                input.consume_key(Modifiers::NONE, Key::Enter),
                input.consume_key(Modifiers::NONE, Key::Escape),
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
        }
        if escape {
            if self.settings_open {
                self.settings_open = false;
            } else {
                self.hide(ctx);
            }
        }

        let pal = palette(self.config.theme);
        let height = self.desired_height();
        if (height - self.last_height).abs() > 0.5 {
            self.last_height = height;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(WINDOW_WIDTH, height)));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(pal.bg)
                    .rounding(14.0_f32)
                    .stroke(Stroke::new(1.0_f32, pal.border))
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0)),
            )
            .show(ctx, |ui| {
                if self.settings_open {
                    self.settings_ui(ui);
                    return;
                }

                // Search row: magnifier (also the drag handle), borderless field, settings gear.
                ui.horizontal(|ui| {
                    ui.set_min_height(46.0);
                    let (icon_rect, icon_response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::drag());
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
                        .add(egui::Button::new(RichText::new("\u{2699}").size(18.0).color(pal.muted)).frame(false))
                        .on_hover_text("Settings")
                        .clicked()
                    {
                        self.settings_open = true;
                    }
                });

                if !self.status.is_empty() {
                    ui.label(RichText::new(&self.status).size(12.0).color(pal.muted));
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
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
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
                                ui.label(RichText::new(heading).size(12.0).strong().color(pal.muted));
                                ui.add_space(2.0);
                            }

                            let (rect, response) =
                                ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());
                            let is_selected = index == selected;
                            if is_selected {
                                ui.painter().rect_filled(rect, 8.0_f32, ACCENT);
                            } else if response.hovered() {
                                ui.painter().rect_filled(rect, 8.0_f32, pal.hover);
                            }
                            let (title_color, sub_color) = if is_selected {
                                (Color32::WHITE, Color32::from_rgba_unmultiplied(255, 255, 255, 190))
                            } else {
                                (pal.text, pal.muted)
                            };

                            let tile = egui::Rect::from_min_size(
                                Pos2::new(rect.left() + 8.0, rect.center().y - 17.0),
                                Vec2::splat(34.0),
                            );
                            match row.icon.as_ref().and_then(|(key, _)| self.icons.texture(key)) {
                                Some(texture) => {
                                    ui.painter().image(
                                        texture.id(),
                                        tile,
                                        egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                        Color32::WHITE,
                                    );
                                }
                                None => {
                                    // Until the real icon arrives (or for items that have none).
                                    ui.painter().rect_filled(tile, 8.0_f32, tile_color(item));
                                    let letter = item.title.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
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
                            let text_painter = ui.painter().with_clip_rect(rect.shrink2(Vec2::new(4.0, 0.0)));
                            let title_rect = egui::Rect::from_min_max(
                                Pos2::new(text_x, rect.center().y - 18.0),
                                Pos2::new(rect.right() - 12.0, rect.center().y + 2.0),
                            );
                            let mut title_ui = ui.child_ui(title_rect, egui::Layout::left_to_right(egui::Align::Center));
                            title_ui.add(
                                egui::Label::new(title_job(row, title_color, is_selected, item.kind == Kind::Calculation))
                                    .truncate(true),
                            );
                            text_painter.text(
                                Pos2::new(text_x, rect.center().y + 10.0),
                                Align2::LEFT_CENTER,
                                &row.subtitle,
                                FontId::proportional(12.0),
                                sub_color,
                            );

                            if response.clicked() {
                                clicked = Some(index);
                            }
                            if moved && is_selected {
                                ui.scroll_to_rect(rect, None);
                            }
                            ui.add_space(2.0);
                        }
                    });
                    if let Some(index) = clicked {
                        activate = Some(index);
                    }
                }
            });

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
            let hash = item.title.bytes().fold(5381u32, |acc, byte| acc.wrapping_mul(33).wrapping_add(byte as u32));
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
    let plain = egui::TextFormat { font_id: FontId::proportional(size), color, ..Default::default() };
    let mut emphasised = plain.clone();
    if selected {
        emphasised.underline = Stroke::new(1.0_f32, color);
    } else {
        emphasised.color = ACCENT;
    }
    let mut job = egui::text::LayoutJob::default();
    for &(start, end, matched) in &row.title_runs {
        job.append(&row.item.title[start..end], 0.0, if matched { emphasised.clone() } else { plain.clone() });
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
                fuzzy_with(&highlight, &item.title).map(|m| m.indices).unwrap_or_default()
            };
            let subtitle = if item.subtitle.is_empty() { item.kind.label().to_string() } else { shorten(&item.subtitle, 74) };
            let title_runs = highlight_runs(&item.title, &matched);
            let icon = icons::icon_request(&item);
            Row { item, subtitle, title_runs, icon }
        })
        .collect()
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
    painter.circle_stroke(center + Vec2::new(-2.0, -2.0), 7.5, Stroke::new(2.2_f32, color));
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
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("okra-debug.log")) {
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

    type WinEventProc = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, i32, i32, u32, u32);

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

    unsafe extern "system" fn on_foreground_changed(_: *mut c_void, _: u32, _: *mut c_void, _: i32, _: i32, _: u32, _: u32) {
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
fn load_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "inter".to_owned(),
        egui::FontData::from_static(include_bytes!("../../../assets/fonts/Inter-Regular.otf")),
    );
    let mut order = vec!["inter".to_owned()];
    #[cfg(windows)]
    {
        let dir = std::env::var_os("WINDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        for (key, file) in [("segoe-ui", "segoeui.ttf"), ("nirmala-ui", "Nirmala.ttf")] {
            if let Ok(bytes) = std::fs::read(dir.join(file)) {
                fonts.font_data.insert(key.to_owned(), egui::FontData::from_owned(bytes));
                order.push(key.to_owned());
            }
        }
    }
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        for (position, name) in order.into_iter().enumerate() {
            family.insert(position.min(family.len()), name);
        }
    }
    ctx.set_fonts(fonts);
}

/// Windows only lets the process that currently owns the foreground steal focus, so after another
/// app was launched from here a plain "focus" request is silently refused and the launcher shows
/// up unfocused. This borrows the foreground thread's input right (the usual launcher trick).
#[cfg(windows)]
fn bring_to_front(reposition: bool) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

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
        fn FindWindowW(class: *const u16, title: *const u16) -> *mut c_void;
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
        fn SetWindowPos(hwnd: *mut c_void, after: *mut c_void, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    const SW_SHOW: i32 = 5;
    const VK_MENU: u8 = 0x12;
    const KEYEVENTF_KEYUP: u32 = 2;
    const MONITOR_DEFAULTTONEAREST: u32 = 2;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;

    let title: Vec<u16> = std::ffi::OsStr::new("Okra Brown Search")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd.is_null() {
            return;
        }
        let mut process_id = 0u32;
        GetWindowThreadProcessId(hwnd, &mut process_id);
        if process_id != std::process::id() {
            return;
        }

        if reposition {
            // Centre the search bar on the screen the mouse is on. Everything here is in physical
            // pixels, so the window's own width tells us the DPI scale. The bar sits at the exact
            // centre; the result list grows downwards from it. On a short screen the top edge is
            // lifted just enough to keep the tallest list on screen.
            let mut cursor = Point::default();
            let mut window = Rect::default();
            let mut info = MonitorInfo { size: std::mem::size_of::<MonitorInfo>() as u32, monitor: Rect::default(), work: Rect::default(), flags: 0 };
            if GetCursorPos(&mut cursor) != 0 {
                let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
                if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 && GetWindowRect(hwnd, &mut window) != 0 {
                    let width = window.right - window.left;
                    if width > 0 {
                        let scale = width as f32 / WINDOW_WIDTH;
                        let bar = (BAR_HEIGHT * scale).round() as i32;
                        let tallest = ((BAR_HEIGHT + 24.0 + 22.0 + MAX_LIST_HEIGHT) * scale).round() as i32;
                        let work = info.work;
                        let x = work.left + ((work.right - work.left) - width) / 2;
                        let centred_y = work.top + ((work.bottom - work.top) - bar) / 2;
                        let y = centred_y.min(work.bottom - tallest).max(work.top);
                        SetWindowPos(hwnd, std::ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                    }
                }
            }
        }

        ShowWindow(hwnd, SW_SHOW);
        if GetForegroundWindow() == hwnd {
            return;
        }
        if reposition {
            // A synthetic Alt tap marks this process as having just received input. Only on the
            // first attempt: repeating it on every retry would keep poking the app underneath.
            keybd_event(VK_MENU, 0, 0, 0);
            keybd_event(VK_MENU, 0, KEYEVENTF_KEYUP, 0);
        }
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, std::ptr::null_mut())
        };
        let ours = GetCurrentThreadId();
        let attached = foreground_thread != 0 && foreground_thread != ours && AttachThreadInput(ours, foreground_thread, 1) != 0;
        SetForegroundWindow(hwnd);
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
        fn CreateMutexW(attributes: *mut c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
        fn GetLastError() -> u32;
        fn SetLastError(error: u32);
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    let name: Vec<u16> = std::ffi::OsStr::new("Local\\OkraBrownSearch")
        .encode_wide().chain(std::iter::once(0)).collect();
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
        let executable = std::env::current_exe().map_err(|e| format!("Could not locate the app executable: {e}"))?;
        let value = format!("\"{}\" --background", executable.display());
        command.args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v", "OkraBrownSearch", "/t", "REG_SZ", "/d", value.as_str(), "/f",
        ]);
    } else {
        command.args([
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v", "OkraBrownSearch", "/f",
        ]);
    }
    let output = command.output().map_err(|e| format!("Could not update Windows sign-in startup: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("Windows did not accept the startup setting: {}", String::from_utf8_lossy(&output.stderr).trim()))
    }
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
        .with_title("Okra Brown Search")
        .with_inner_size([WINDOW_WIDTH, BAR_HEIGHT])
        .with_decorations(false)
        .with_transparent(true)
        .with_min_inner_size([WINDOW_WIDTH, BAR_HEIGHT])
        .with_max_inner_size([WINDOW_WIDTH, 800.0])
        .with_always_on_top()
        .with_visible(true);

    if let Ok(icon) = image::load_from_memory(include_bytes!("../../../assets/okra-brown-search.png")) {
        let rgba = icon.to_rgba8();
        let (width, height) = rgba.dimensions();
        viewport = viewport.with_icon(Arc::new(egui::IconData { rgba: rgba.into_raw(), width, height }));
    }

    let options = eframe::NativeOptions { viewport, centered: true, ..Default::default() };
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
        Item { id: title.into(), title: title.into(), subtitle: String::new(), kind, action: Action::Copy(String::new()), score: 0 }
    }

    #[test]
    fn runs_cover_the_title_exactly_and_flag_matches() {
        let title = "Workbench 2023 R1";
        let runs = highlight_runs(title, &[0, 1, 2, 3]);
        assert_eq!(runs, vec![(0, 4, true), (4, title.len(), false)]);
        // No gaps or overlaps, whatever the match pattern (incl. multi-byte characters).
        for (title, matched) in [("Éclair café", vec![0usize, 3, 4, 10]), ("abc", vec![]), ("abc", vec![0, 1, 2]), ("", vec![])] {
            let runs = highlight_runs(title, &matched);
            let rebuilt: String = runs.iter().map(|&(a, b, _)| &title[a..b]).collect();
            assert_eq!(rebuilt, title);
            let flagged: usize = runs.iter().filter(|r| r.2).map(|&(a, b, _)| title[a..b].chars().count()).sum();
            assert_eq!(flagged, matched.len());
        }
    }

    #[test]
    fn rows_highlight_the_query_that_produced_them() {
        let rows = build_rows(vec![item("Workbench 2023 R1", Kind::App), item("2+2", Kind::Calculation)], "a work");
        assert_eq!(rows[0].title_runs.iter().filter(|r| r.2).count(), 1, "prefix `a ` is not highlighted");
        assert!(rows[0].title_runs[0].2 && rows[0].title_runs[0].1 == 4, "\"work\" highlighted");
        assert!(rows[1].title_runs.iter().all(|r| !r.2), "calculations are never highlighted");
        assert_eq!(rows[0].subtitle, Kind::App.label(), "empty subtitle falls back to the kind label");
        assert!(build_rows(vec![item("Notepad", Kind::App)], "")[0].title_runs.iter().all(|r| !r.2));
    }
}
