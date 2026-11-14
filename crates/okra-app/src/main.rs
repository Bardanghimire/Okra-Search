#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use eframe::egui::{self, Color32, Key, Modifiers, RichText, Stroke, Vec2};
use okra_core::{Config, Engine, FileBackendKind, Item, Outcome, ThemeKind};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

const BG_LIGHT: Color32 = Color32::from_rgb(247, 240, 224);
const PANEL_LIGHT: Color32 = Color32::from_rgb(255, 251, 241);
const TEXT_LIGHT: Color32 = Color32::from_rgb(64, 43, 31);
const MUTED_LIGHT: Color32 = Color32::from_rgb(133, 112, 91);
const GREEN: Color32 = Color32::from_rgb(104, 133, 51);
const BG_DARK: Color32 = Color32::from_rgb(35, 29, 26);
const PANEL_DARK: Color32 = Color32::from_rgb(49, 41, 36);
const TEXT_DARK: Color32 = Color32::from_rgb(246, 237, 220);
const MUTED_DARK: Color32 = Color32::from_rgb(185, 167, 146);

struct App {
    engine: Arc<Engine>,
    config: Config,
    initial_backend: FileBackendKind,
    initial_max_results: usize,
    initial_start_with_windows: bool,
    query: String,
    results: Vec<Item>,
    selected: usize,
    status: String,
    gen: u64,
    shown: u64,
    q_tx: Sender<(u64, String)>,
    r_rx: Receiver<(u64, Vec<Item>)>,
    settings_open: bool,
    show_requested: Arc<std::sync::atomic::AtomicBool>,
    start_hidden: bool,
    resident_initialized: bool,
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
            let image = image::load_from_memory(include_bytes!("../../../assets/okra-brown-search.png"))
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
        apply_theme(&cc.egui_ctx, config.theme);

        // Search runs on a worker thread; rapid typing replaces stale queued queries.
        let (q_tx, q_rx) = channel::<(u64, String)>();
        let (r_tx, r_rx) = channel::<(u64, Vec<Item>)>();
        let (eng, ctx) = (engine.clone(), cc.egui_ctx.clone());
        std::thread::spawn(move || {
            while let Ok(mut latest) = q_rx.recv() {
                while let Ok(newer) = q_rx.try_recv() {
                    latest = newer;
                }
                let items = eng.search(&latest.1);
                if r_tx.send((latest.0, items)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });

        let show_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut app = Self {
            engine,
            initial_backend: config.file_backend,
            initial_max_results: config.max_results,
            initial_start_with_windows: config.start_with_windows,
            config,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            status: "Starting resident search controls…".into(),
            gen: 0,
            shown: 0,
            q_tx,
            r_rx,
            settings_open: false,
            show_requested,
            start_hidden,
            resident_initialized: false,
            #[cfg(windows)]
            _resident: None,
        };
        app.search_again();
        app
    }

    fn search_again(&mut self) {
        self.gen += 1;
        let _ = self.q_tx.send((self.gen, self.query.clone()));
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.settings_open = false;
        self.query.clear();
        self.search_again();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    fn activate(&mut self, ctx: &egui::Context, idx: usize) {
        let Some(item) = self.results.get(idx).cloned() else { return };
        match self.engine.activate(&item) {
            Ok(Outcome::CopyText(text)) => {
                ctx.output_mut(|output| output.copied_text = text);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            Ok(Outcome::Done) => ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false)),
            Err(error) => self.status = error,
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.heading(RichText::new("Settings").size(23.0).strong());
        ui.label(RichText::new("Configure search behavior and appearance.").color(muted_color(self.config.theme)));
        ui.add_space(16.0);

        ui.label(RichText::new("APPEARANCE").small().strong().color(GREEN));
        let mut theme_changed = false;
        egui::ComboBox::from_id_source("okra-theme")
            .selected_text(self.config.theme.label())
            .show_ui(ui, |ui| {
                theme_changed |= ui.selectable_value(&mut self.config.theme, ThemeKind::WarmLight, "Warm light").changed();
                theme_changed |= ui.selectable_value(&mut self.config.theme, ThemeKind::Dark, "Dark").changed();
            });
        if theme_changed {
            apply_theme(ui.ctx(), self.config.theme);
        }

        ui.add_space(18.0);
        ui.label(RichText::new("FILE SEARCH").small().strong().color(GREEN));
        egui::ComboBox::from_id_source("okra-file-backend")
            .selected_text(self.config.file_backend.label())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.config.file_backend, FileBackendKind::Auto, "Automatic (recommended)");
                ui.selectable_value(&mut self.config.file_backend, FileBackendKind::Everything, "Everything only");
                ui.selectable_value(&mut self.config.file_backend, FileBackendKind::Builtin, "Built-in index only");
            });
        ui.label(RichText::new("Automatic uses the Everything SDK, then es.exe, then the local index.").small().color(muted_color(self.config.theme)));

        ui.add_space(18.0);
        ui.label(RichText::new("RESULTS").small().strong().color(GREEN));
        ui.add(egui::Slider::new(&mut self.config.max_results, 10..=80).text("Maximum results"));

        ui.add_space(18.0);
        ui.label(RichText::new("QUICK ACCESS").small().strong().color(GREEN));
        ui.checkbox(&mut self.config.start_with_windows, "Launch when I sign in to Windows");
        ui.label("Global shortcut: Ctrl + Alt + Space");
        ui.label("Escape: hide the search window");
        ui.label("The app stays in the notification area until you choose Exit.");

        ui.add_space(18.0);
        let restart_needed = self.config.file_backend != self.initial_backend || self.config.max_results != self.initial_max_results;
        if ui.button(RichText::new("Save settings").strong()).clicked() {
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
        if restart_needed {
            ui.label(RichText::new("File backend and result-count changes apply the next time the app starts.").small().color(muted_color(self.config.theme)));
        }
        if ui.button("Back to search").clicked() {
            self.settings_open = false;
            self.query.clear();
            self.search_again();
        }
    }
}

impl eframe::App for App {
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
                if self.start_hidden && !controls_failed {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            }
            #[cfg(not(windows))]
            {
                self.status = "Global hotkey and tray are available in the Windows build".into();
            }
            self.resident_initialized = true;
        }

        while let Ok((generation, items)) = self.r_rx.try_recv() {
            if generation >= self.shown {
                self.shown = generation;
                self.results = items;
                self.selected = 0;
            }
        }

        if self.show_requested.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.show(ctx);
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
            if down && !self.results.is_empty() {
                self.selected = (self.selected + 1).min(self.results.len() - 1);
                moved = true;
            }
            if up {
                self.selected = self.selected.saturating_sub(1);
                moved = true;
            }
            if enter && !self.results.is_empty() {
                activate = Some(self.selected);
            }
        }
        if escape {
            if self.settings_open {
                self.settings_open = false;
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(background_color(self.config.theme)).inner_margin(egui::Margin::same(18.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    draw_brand_mark(ui);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Okra Brown Search").size(19.0).strong().color(text_color(self.config.theme)));
                        ui.label(RichText::new("Find anything on this PC").small().color(muted_color(self.config.theme)));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Hide").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                        }
                        if ui.small_button("Settings").clicked() {
                            self.settings_open = true;
                        }
                    });
                });

                ui.add_space(16.0);
                if self.settings_open {
                    self.settings_ui(ui);
                } else {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Search apps, files, settings, and open windows…")
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Heading),
                    );
                    response.request_focus();
                    if response.changed() {
                        self.search_again();
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new("Prefixes: a apps  ·  f files  ·  s settings  ·  win windows")
                        .small().color(muted_color(self.config.theme)));
                    ui.add_space(5.0);

                    let selected = self.selected;
                    let mut clicked = None;
                    if self.results.is_empty() {
                        ui.add_space(18.0);
                        ui.label(RichText::new("No matching results").color(muted_color(self.config.theme)));
                    }
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        for (index, item) in self.results.iter().enumerate() {
                            let fill = if index == selected { selected_color(self.config.theme) } else { panel_color(self.config.theme) };
                            let row = egui::Frame::none()
                                .fill(fill)
                                .rounding(8.0)
                                .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(RichText::new(&item.title).size(15.0).strong().color(text_color(self.config.theme)));
                                    ui.label(RichText::new(format!("{}  ·  {}", item.kind.label(), item.subtitle))
                                        .small().color(muted_color(self.config.theme)));
                                });
                            let response = ui.interact(row.response.rect, egui::Id::new(("result", index)), egui::Sense::click());
                            if response.clicked() {
                                clicked = Some(index);
                            }
                            if moved && index == selected {
                                ui.scroll_to_rect(row.response.rect, None);
                            }
                            ui.add_space(5.0);
                        }
                    });
                    if let Some(index) = clicked {
                        activate = Some(index);
                    }
                }

                if !self.status.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(&self.status).small().color(muted_color(self.config.theme)));
                }
                ui.add_space(6.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Ctrl + Alt + Space  ·  Open launcher").small().color(muted_color(self.config.theme)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("Esc  ·  Hide").small().color(muted_color(self.config.theme)));
                    });
                });
            });

        if let Some(index) = activate {
            self.activate(ctx, index);
        }
    }
}

fn apply_theme(ctx: &egui::Context, theme: ThemeKind) {
    let mut visuals = match theme {
        ThemeKind::WarmLight => egui::Visuals::light(),
        ThemeKind::Dark => egui::Visuals::dark(),
    };
    visuals.window_rounding = 12.0.into();
    visuals.panel_fill = background_color(theme);
    visuals.window_fill = panel_color(theme);
    visuals.widgets.noninteractive.bg_fill = panel_color(theme);
    visuals.selection.bg_fill = selected_color(theme);
    visuals.selection.stroke = Stroke::new(1.0_f32, GREEN);
    ctx.set_visuals(visuals);
}

fn background_color(theme: ThemeKind) -> Color32 {
    match theme { ThemeKind::WarmLight => BG_LIGHT, ThemeKind::Dark => BG_DARK }
}
fn panel_color(theme: ThemeKind) -> Color32 {
    match theme { ThemeKind::WarmLight => PANEL_LIGHT, ThemeKind::Dark => PANEL_DARK }
}
fn text_color(theme: ThemeKind) -> Color32 {
    match theme { ThemeKind::WarmLight => TEXT_LIGHT, ThemeKind::Dark => TEXT_DARK }
}
fn muted_color(theme: ThemeKind) -> Color32 {
    match theme { ThemeKind::WarmLight => MUTED_LIGHT, ThemeKind::Dark => MUTED_DARK }
}
fn selected_color(theme: ThemeKind) -> Color32 {
    match theme {
        ThemeKind::WarmLight => Color32::from_rgb(222, 231, 202),
        ThemeKind::Dark => Color32::from_rgb(70, 83, 53),
    }
}

fn draw_brand_mark(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(38.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let center = rect.center();
    painter.circle_filled(center, 18.0, Color32::from_rgb(238, 225, 199));
    painter.circle_stroke(center, 13.0, Stroke::new(4.0_f32, Color32::from_rgb(91, 54, 35)));
    painter.line_segment(
        [center + Vec2::new(9.0, 9.0), center + Vec2::new(16.0, 16.0)],
        Stroke::new(5.0_f32, Color32::from_rgb(91, 54, 35)),
    );
    let pod = vec![
        center + Vec2::new(-10.0, 9.0),
        center + Vec2::new(-4.0, 1.0),
        center + Vec2::new(2.0, -8.0),
        center + Vec2::new(8.0, -12.0),
        center + Vec2::new(10.0, -5.0),
        center + Vec2::new(7.0, 4.0),
        center + Vec2::new(-1.0, 12.0),
    ];
    painter.add(egui::Shape::convex_polygon(pod, GREEN, Stroke::new(0.5_f32, Color32::from_rgb(63, 91, 34))));
    painter.line_segment(
        [center + Vec2::new(-7.0, 7.0), center + Vec2::new(7.0, -8.0)],
        Stroke::new(1.4_f32, Color32::from_rgb(190, 207, 95)),
    );
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
        .with_inner_size([760.0, 560.0])
        .with_min_inner_size([560.0, 390.0])
        .with_decorations(false)
        .with_always_on_top()
        .with_visible(true);

    if let Ok(icon) = image::load_from_memory(include_bytes!("../../../assets/okra-brown-search.png")) {
        let rgba = icon.to_rgba8();
        let (width, height) = rgba.dimensions();
        viewport = viewport.with_icon(Arc::new(egui::IconData { rgba: rgba.into_raw(), width, height }));
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "Okra Brown Search",
        options,
        Box::new(move |cc| Box::new(App::new(cc, engine, config, start_hidden))),
    )
}
