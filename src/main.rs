#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
#![cfg_attr(
    target_os = "windows",
    allow(
        clippy::upper_case_acronyms,
        non_camel_case_types,
        non_snake_case,
        unsafe_op_in_unsafe_fn
    )
)]

#[cfg(target_os = "windows")]
fn main() -> eframe::Result {
    win_app::run()
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("screen_locker uses Win32 cursor clipping and only runs on Windows.");
}

#[cfg(target_os = "windows")]
mod win_app {
    use eframe::egui::{
        self, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense,
        Stroke, StrokeKind, Vec2, pos2, vec2,
    };
    use std::fs;
    use std::mem::{size_of, zeroed};
    use std::path::PathBuf;
    use std::ptr::null;
    use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    type BOOL = i32;
    type HWND = isize;
    type HMONITOR = isize;
    type HDC = isize;
    type WPARAM = usize;
    type LPARAM = isize;

    const MOD_ALT: u32 = 0x0001;
    const MOD_CONTROL: u32 = 0x0002;
    const MOD_SHIFT: u32 = 0x0004;
    const MOD_WIN: u32 = 0x0008;
    const MOD_NOREPEAT: u32 = 0x4000;

    const VK_SHIFT: i32 = 0x10;
    const VK_CONTROL: i32 = 0x11;
    const VK_MENU: i32 = 0x12;
    const VK_LWIN: i32 = 0x5b;
    const VK_RWIN: i32 = 0x5c;

    const WM_QUIT: u32 = 0x0012;
    const WM_HOTKEY: u32 = 0x0312;
    const PM_NOREMOVE: u32 = 0x0000;

    const HOTKEY_EMERGENCY_UNLOCK: i32 = 99;
    const VK_ESCAPE: u32 = 0x1b;
    const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4isize;
    const ACTION_POLL_INTERVAL: Duration = Duration::from_millis(16);
    const LOCK_WORKER_INTERVAL: Duration = Duration::from_millis(4);
    const DISPLAY_CHECK_INTERVAL: Duration = Duration::from_secs(2);

    #[repr(C)]
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    struct RECT {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct POINT {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MSG {
        hwnd: HWND,
        message: u32,
        wParam: WPARAM,
        lParam: LPARAM,
        time: u32,
        pt: POINT,
        lPrivate: u32,
    }

    #[repr(C)]
    struct MONITORINFOEXW {
        cbSize: u32,
        rcMonitor: RECT,
        rcWork: RECT,
        dwFlags: u32,
        szDevice: [u16; 32],
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn ClipCursor(lpRect: *const RECT) -> BOOL;
        fn EnumDisplayMonitors(
            hdc: HDC,
            lprcClip: *const RECT,
            lpfnEnum: Option<unsafe extern "system" fn(HMONITOR, HDC, *mut RECT, LPARAM) -> BOOL>,
            dwData: LPARAM,
        ) -> BOOL;
        fn GetKeyState(nVirtKey: i32) -> i16;
        fn GetCursorPos(lpPoint: *mut POINT) -> BOOL;
        fn GetMessageW(lpMsg: *mut MSG, hWnd: HWND, wMsgFilterMin: u32, wMsgFilterMax: u32)
        -> BOOL;
        fn GetMonitorInfoW(hMonitor: HMONITOR, lpmi: *mut MONITORINFOEXW) -> BOOL;
        fn PeekMessageW(
            lpMsg: *mut MSG,
            hWnd: HWND,
            wMsgFilterMin: u32,
            wMsgFilterMax: u32,
            wRemoveMsg: u32,
        ) -> BOOL;
        fn PostThreadMessageW(idThread: u32, Msg: u32, wParam: WPARAM, lParam: LPARAM) -> BOOL;
        fn RegisterHotKey(hWnd: HWND, id: i32, fsModifiers: u32, vk: u32) -> BOOL;
        fn GetAsyncKeyState(vKey: i32) -> i16;
        fn SetProcessDpiAwarenessContext(value: isize) -> BOOL;
        fn SetCursorPos(X: i32, Y: i32) -> BOOL;
        fn UnregisterHotKey(hWnd: HWND, id: i32) -> BOOL;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn GetLastError() -> u32;
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Action {
        Toggle,
        Unlock,
    }

    impl Action {
        fn label(self) -> &'static str {
            match self {
                Self::Toggle => "Toggle Lock/Unlock",
                Self::Unlock => "Unlock",
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Hotkey {
        modifiers: u32,
        vk: u32,
    }

    impl Hotkey {
        fn new(modifiers: u32, vk: u32) -> Self {
            Self { modifiers, vk }
        }

        fn display(self) -> String {
            let mut parts = Vec::new();
            if self.modifiers & MOD_CONTROL != 0 {
                parts.push("Ctrl".to_string());
            }
            if self.modifiers & MOD_ALT != 0 {
                parts.push("Alt".to_string());
            }
            if self.modifiers & MOD_SHIFT != 0 {
                parts.push("Shift".to_string());
            }
            if self.modifiers & MOD_WIN != 0 {
                parts.push("Win".to_string());
            }
            parts.push(vk_to_name(self.vk));
            parts.join("+")
        }
    }

    #[derive(Default)]
    struct Settings {
        selected_monitor: usize,
        toggle_hotkey: Option<Hotkey>,
        load_warnings: Vec<String>,
    }

    impl Settings {
        fn load() -> Self {
            let mut settings = Self::default();
            let Some(path) = config_path() else {
                return settings;
            };
            let Ok(contents) = fs::read_to_string(path) else {
                return settings;
            };

            for raw_line in contents.lines() {
                let line = raw_line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }

                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };

                match key.trim() {
                    "selected_monitor" => {
                        if let Ok(index) = value.trim().parse::<usize>() {
                            settings.selected_monitor = index;
                        }
                    }
                    "toggle_hotkey" => {
                        if let Some(hotkey) = parse_hotkey(value) {
                            settings.toggle_hotkey = Some(hotkey);
                        } else {
                            settings.load_warnings.push("toggle_hotkey".to_string());
                        }
                    }
                    _ => {}
                }
            }

            settings
        }

        fn save(&self) -> std::io::Result<PathBuf> {
            let path = config_path().unwrap_or_else(|| PathBuf::from("screen_locker.ini"));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }

            let hotkey = self
                .toggle_hotkey
                .map(|hotkey| hotkey.display())
                .unwrap_or_default();
            let contents = format!(
                "# screen_locker settings\nselected_monitor={}\ntoggle_hotkey={}\n",
                self.selected_monitor, hotkey
            );
            fs::write(&path, contents)?;
            Ok(path)
        }

        fn hotkey(&self, action: Action) -> Option<Hotkey> {
            match action {
                Action::Toggle => self.toggle_hotkey,
                Action::Unlock => Some(Hotkey::new(MOD_CONTROL | MOD_ALT, VK_ESCAPE)),
            }
        }

        fn set_hotkey(&mut self, action: Action, hotkey: Hotkey) {
            if action == Action::Toggle {
                self.toggle_hotkey = Some(hotkey);
            }
        }
    }

    #[derive(Clone)]
    struct Monitor {
        rect: RECT,
        work_rect: RECT,
        device: String,
        primary: bool,
    }

    impl Monitor {
        fn width(&self) -> i32 {
            self.rect.right - self.rect.left
        }

        fn height(&self) -> i32 {
            self.rect.bottom - self.rect.top
        }

        fn short_label(&self, index: usize) -> String {
            if self.primary {
                format!("Monitor {} - Primary", index + 1)
            } else {
                format!("Monitor {}", index + 1)
            }
        }
    }

    struct HotkeyRegistration {
        manager: HotkeyManager,
        receiver: Receiver<Action>,
        failures: Vec<String>,
        registered_actions: Vec<Action>,
        emergency_unlock_registered: bool,
    }

    struct HotkeyManager {
        thread_id: u32,
        handle: Option<JoinHandle<()>>,
    }

    struct LockWorker {
        stop_tx: Sender<()>,
        handle: Option<JoinHandle<()>>,
    }

    impl LockWorker {
        fn start(rect: RECT) -> Self {
            let (stop_tx, stop_rx) = mpsc::channel();
            let handle = thread::spawn(move || {
                loop {
                    match stop_rx.try_recv() {
                        Ok(()) | Err(TryRecvError::Disconnected) => break,
                        Err(TryRecvError::Empty) => {}
                    }

                    let _ = enforce_cursor_rect(rect);
                    thread::sleep(LOCK_WORKER_INTERVAL);
                }
            });

            Self {
                stop_tx,
                handle: Some(handle),
            }
        }

        fn stop(&mut self) {
            let _ = self.stop_tx.send(());
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    impl Drop for LockWorker {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn normalize_modifiers(modifiers: u32) -> u32 {
        modifiers & (MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN)
    }

    fn is_letter_vk(vk: u32) -> bool {
        (b'A' as u32..=b'Z' as u32).contains(&vk)
    }

    impl HotkeyManager {
        fn start(_settings: &Settings, repaint: egui::Context) -> HotkeyRegistration {
            let (ready_tx, ready_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();

            let handle = thread::spawn(move || unsafe {
                let thread_id = GetCurrentThreadId();
                let mut msg: MSG = zeroed();
                PeekMessageW(&mut msg, 0, 0, 0, PM_NOREMOVE);

                let mut failures = Vec::new();
                let registered_actions = vec![Action::Toggle];

                let emergency_unlock_registered = RegisterHotKey(
                    0,
                    HOTKEY_EMERGENCY_UNLOCK,
                    MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                    VK_ESCAPE,
                ) != 0;
                if !emergency_unlock_registered {
                    failures.push("Emergency unlock (Ctrl+Alt+Esc)".to_string());
                }

                let _ = ready_tx.send((
                    thread_id,
                    failures,
                    registered_actions,
                    emergency_unlock_registered,
                ));

                loop {
                    let result = GetMessageW(&mut msg, 0, 0, 0);
                    if result <= 0 {
                        break;
                    }

                    if msg.message == WM_HOTKEY && msg.wParam as i32 == HOTKEY_EMERGENCY_UNLOCK {
                        let _ = action_tx.send(Action::Unlock);
                        repaint.request_repaint();
                    }
                }

                if emergency_unlock_registered {
                    UnregisterHotKey(0, HOTKEY_EMERGENCY_UNLOCK);
                }
            });

            let (thread_id, failures, registered_actions, emergency_unlock_registered) = ready_rx
                .recv()
                .unwrap_or((0, vec!["Hotkey thread".to_string()], Vec::new(), false));

            HotkeyRegistration {
                manager: HotkeyManager {
                    thread_id,
                    handle: Some(handle),
                },
                receiver: action_rx,
                failures,
                registered_actions,
                emergency_unlock_registered,
            }
        }

        fn stop(&mut self) {
            if self.thread_id != 0 {
                unsafe {
                    PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
                }
                self.thread_id = 0;
            }

            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    impl Drop for HotkeyManager {
        fn drop(&mut self) {
            self.stop();
        }
    }

    struct ScreenLockerApp {
        monitors: Vec<Monitor>,
        settings: Settings,
        locked: bool,
        capturing: Option<Action>,
        status: String,
        hotkey_manager: Option<HotkeyManager>,
        hotkey_rx: Receiver<Action>,
        registered_actions: Vec<Action>,
        emergency_unlock_registered: bool,
        lock_worker: Option<LockWorker>,
        toggle_bind_down: bool,
        last_display_check: Instant,
        settings_dirty: bool,
    }

    impl ScreenLockerApp {
        fn new(cc: &eframe::CreationContext<'_>) -> Self {
            configure_style(&cc.egui_ctx);

            let mut settings = Settings::load();
            let monitors = enumerate_monitors();
            clamp_selected_monitor(&mut settings, monitors.len());
            let load_warnings = settings.load_warnings.clone();

            let registration = HotkeyManager::start(&settings, cc.egui_ctx.clone());
            let status = if !registration.failures.is_empty() {
                format!("Could not register: {}", registration.failures.join(", "))
            } else if !load_warnings.is_empty() {
                format!("Ignored invalid settings: {}", load_warnings.join(", "))
            } else {
                "Ready".to_string()
            };

            Self {
                monitors,
                settings,
                locked: false,
                capturing: None,
                status,
                hotkey_manager: Some(registration.manager),
                hotkey_rx: registration.receiver,
                registered_actions: registration.registered_actions,
                emergency_unlock_registered: registration.emergency_unlock_registered,
                lock_worker: None,
                toggle_bind_down: false,
                last_display_check: Instant::now(),
                settings_dirty: false,
            }
        }

        fn monitor_count_label(&self) -> String {
            match self.monitors.len() {
                1 => "1 monitor found".to_string(),
                count => format!("{count} monitors found"),
            }
        }

        fn selected_monitor_label(&self) -> String {
            self.monitors
                .get(self.settings.selected_monitor)
                .map(|monitor| monitor.short_label(self.settings.selected_monitor))
                .unwrap_or_else(|| "No monitor".to_string())
        }

        fn set_selected_monitor(&mut self, index: usize) {
            if index >= self.monitors.len() {
                return;
            }

            self.settings.selected_monitor = index;
            if self.locked {
                self.lock_selected_monitor();
            } else {
                self.status = format!("Selected {}", self.selected_monitor_label());
            }
            self.mark_settings_dirty();
        }

        fn set_locked(&mut self, locked: bool) {
            if locked {
                if !self.has_unlock_path() {
                    self.locked = false;
                    self.status =
                        "Cannot lock: set a lock/unlock key or free Ctrl+Alt+Esc.".to_string();
                    return;
                }

                if self.lock_selected_monitor() {
                    self.locked = true;
                } else {
                    self.stop_lock_worker();
                    self.locked = false;
                    self.status = "Could not lock cursor - no monitor is available".to_string();
                }
            } else {
                self.stop_lock_worker();
                release_cursor();
                self.locked = false;
                self.status = "Cursor unlocked".to_string();
            }
        }

        fn lock_selected_monitor(&mut self) -> bool {
            let Some((rect, label, width, height)) = self
                .monitors
                .get(self.settings.selected_monitor)
                .map(|monitor| {
                    (
                        monitor.rect,
                        monitor.short_label(self.settings.selected_monitor),
                        monitor.width(),
                        monitor.height(),
                    )
                })
            else {
                self.stop_lock_worker();
                return false;
            };

            match enforce_cursor_rect(rect) {
                Ok(()) => {
                    self.start_lock_worker(rect);
                    self.status = format!("Locked to {label} ({width}x{height})");
                    true
                }
                Err(error) => {
                    self.stop_lock_worker();
                    self.status = format!("ClipCursor failed - Win32 error {error}");
                    false
                }
            }
        }

        fn start_lock_worker(&mut self, rect: RECT) {
            self.stop_lock_worker();
            self.lock_worker = Some(LockWorker::start(rect));
        }

        fn stop_lock_worker(&mut self) {
            if let Some(mut worker) = self.lock_worker.take() {
                worker.stop();
            }
        }

        fn has_unlock_path(&self) -> bool {
            self.emergency_unlock_registered
                || (self.registered_actions.contains(&Action::Toggle)
                    && self.settings.hotkey(Action::Toggle).is_some())
                || self.registered_actions.contains(&Action::Unlock)
        }

        fn check_display_changes(&mut self) {
            let fresh = enumerate_monitors();
            if monitors_equal(&self.monitors, &fresh) {
                return;
            }

            let lock_target_changed = !lock_targets_equal(&self.monitors, &fresh);
            self.monitors = fresh;
            let previous_selected = self.settings.selected_monitor;
            clamp_selected_monitor(&mut self.settings, self.monitors.len());
            if self.settings.selected_monitor != previous_selected {
                self.mark_settings_dirty();
            }

            if self.monitors.is_empty() {
                self.stop_lock_worker();
                release_cursor();
                self.locked = false;
                self.status = "Display changed - no monitors detected, cursor unlocked".to_string();
            } else if self.locked && lock_target_changed {
                self.status = "Display changed - lock target refreshed".to_string();
                self.lock_selected_monitor();
            } else if self.locked {
                self.status = "Display work area changed - lock target unchanged".to_string();
            } else {
                self.status = format!("Display changed - {}", self.monitor_count_label());
            }
        }

        fn apply_action(&mut self, action: Action) {
            match action {
                Action::Toggle => self.set_locked(!self.locked),
                Action::Unlock => self.set_locked(false),
            }
        }

        fn start_capture(&mut self, action: Action) {
            self.stop_hotkeys();
            self.capturing = Some(action);
            self.status = format!("Recording {} bind", action.label());
        }

        fn cancel_capture(&mut self, ctx: &egui::Context) {
            self.capturing = None;
            self.restart_hotkeys(ctx);
            self.status = "Bind recording canceled".to_string();
        }

        fn complete_capture(&mut self, ctx: &egui::Context, action: Action, hotkey: Hotkey) {
            self.settings.set_hotkey(action, hotkey);
            self.capturing = None;
            self.toggle_bind_down = self
                .settings
                .hotkey(Action::Toggle)
                .is_some_and(hotkey_is_down);
            self.mark_settings_dirty();
            self.restart_hotkeys(ctx);
            self.status = format!("{} bind set to {}", action.label(), hotkey.display());
        }

        fn try_capture_bind(&mut self, ctx: &egui::Context) {
            let Some(action) = self.capturing else {
                return;
            };

            let events = ctx.input(|input| input.events.clone());
            for event in events {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } = event
                else {
                    continue;
                };

                if key == egui::Key::Escape && !modifiers.any() {
                    self.cancel_capture(ctx);
                    return;
                }

                if is_egui_modifier_key(key) {
                    continue;
                }

                let Some(vk) = egui_key_to_vk(key) else {
                    self.status = "Unsupported key for bind".to_string();
                    return;
                };

                // Combine the live key state with the modifiers egui reports on the
                // event so a plain key or a combo like Ctrl+F both record reliably.
                let mut mods = pressed_modifiers();
                if modifiers.ctrl || modifiers.command {
                    mods |= MOD_CONTROL;
                }
                if modifiers.alt {
                    mods |= MOD_ALT;
                }
                if modifiers.shift {
                    mods |= MOD_SHIFT;
                }
                // Letters are case-insensitive: Shift only changes case, so don't
                // bake it into a letter bind.
                if is_letter_vk(vk) {
                    mods &= !MOD_SHIFT;
                }

                self.complete_capture(ctx, action, Hotkey::new(mods, vk));
                return;
            }
        }

        fn restart_hotkeys(&mut self, ctx: &egui::Context) {
            self.stop_hotkeys();
            let registration = HotkeyManager::start(&self.settings, ctx.clone());
            if !registration.failures.is_empty() {
                self.status = format!("Could not register: {}", registration.failures.join(", "));
            }
            self.hotkey_rx = registration.receiver;
            self.registered_actions = registration.registered_actions;
            self.emergency_unlock_registered = registration.emergency_unlock_registered;
            self.hotkey_manager = Some(registration.manager);
        }

        fn stop_hotkeys(&mut self) {
            if let Some(mut manager) = self.hotkey_manager.take() {
                manager.stop();
            }
            self.registered_actions.clear();
            self.emergency_unlock_registered = false;
        }

        fn poll_toggle_bind(&mut self) {
            if self.capturing.is_some() {
                self.toggle_bind_down = false;
                return;
            }

            let down = self
                .settings
                .hotkey(Action::Toggle)
                .is_some_and(hotkey_is_down);
            if down && !self.toggle_bind_down {
                self.apply_action(Action::Toggle);
            }
            self.toggle_bind_down = down;
        }

        fn mark_settings_dirty(&mut self) {
            self.settings_dirty = true;
        }

        fn ui_top_bar(&mut self, ui: &mut egui::Ui) {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("Simple Cursor Locker by FPSHEAVEN")
                            .font(FontId::proportional(17.0))
                            .strong()
                            .color(Color32::from_rgb(238, 241, 235)),
                    );
                    ui.label(
                        RichText::new(self.monitor_count_label())
                            .font(FontId::proportional(11.5))
                            .color(Color32::from_rgb(154, 160, 151)),
                    );
                });
            });
        }

        fn ui_lock_dashboard(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
            ui.spacing_mut().item_spacing = vec2(8.0, 5.0);
            ui.columns(2, |columns| {
                columns[0].vertical(|ui| {
                    panel_frame().show(ui, |ui| {
                        section_title(ui, "Monitors");
                        ui.add_space(4.0);
                        self.ui_monitor_layout(ui);
                    });
                });

                columns[1].vertical(|ui| {
                    panel_frame().show(ui, |ui| {
                        self.ui_bind_row(ui, ctx, Action::Toggle);

                        ui.add_space(6.0);
                        let (lock_text, lock_color) = if self.locked {
                            (
                                format!("Locked Monitor {}", self.settings.selected_monitor + 1),
                                Color32::from_rgb(146, 214, 174),
                            )
                        } else {
                            ("Not locked".to_string(), Color32::from_rgb(139, 145, 136))
                        };
                        ui.label(
                            RichText::new(lock_text)
                                .font(FontId::proportional(13.0))
                                .strong()
                                .color(lock_color),
                        );
                    });
                });
            });
        }

        fn ui_monitor_layout(&mut self, ui: &mut egui::Ui) {
            let size = vec2(ui.available_width(), 112.0);
            let (rect, response) = ui.allocate_exact_size(size, Sense::click());
            let painter = ui.painter_at(rect);

            if self.monitors.is_empty() {
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    "No monitors detected",
                    FontId::proportional(13.0),
                    Color32::from_rgb(154, 160, 151),
                );
                return;
            }

            let bounds = virtual_bounds(&self.monitors);
            let virtual_width = (bounds.right - bounds.left).max(1) as f32;
            let virtual_height = (bounds.bottom - bounds.top).max(1) as f32;
            let padding = 8.0;
            let scale = ((rect.width() - padding * 2.0) / virtual_width)
                .min((rect.height() - padding * 2.0) / virtual_height)
                .max(0.01);
            let drawn = vec2(virtual_width * scale, virtual_height * scale);
            let origin = pos2(
                rect.left() + (rect.width() - drawn.x) / 2.0,
                rect.top() + (rect.height() - drawn.y) / 2.0,
            );
            let pointer = response.interact_pointer_pos();
            let mut clicked_index = None;

            for (index, monitor) in self.monitors.iter().enumerate() {
                let min = pos2(
                    origin.x + (monitor.rect.left - bounds.left) as f32 * scale,
                    origin.y + (monitor.rect.top - bounds.top) as f32 * scale,
                );
                let max = pos2(
                    origin.x + (monitor.rect.right - bounds.left) as f32 * scale,
                    origin.y + (monitor.rect.bottom - bounds.top) as f32 * scale,
                );
                let monitor_rect = egui::Rect::from_min_max(min, max);
                let selected = index == self.settings.selected_monitor;
                let fill = if selected {
                    Color32::from_rgb(42, 126, 83)
                } else {
                    Color32::from_rgb(54, 57, 52)
                };
                let stroke = if selected {
                    Stroke::new(2.0, Color32::from_rgb(146, 214, 174))
                } else {
                    Stroke::new(1.0, Color32::from_rgb(88, 94, 84))
                };

                painter.rect_filled(monitor_rect, CornerRadius::same(6), fill);
                painter.rect_stroke(
                    monitor_rect,
                    CornerRadius::same(6),
                    stroke,
                    StrokeKind::Inside,
                );
                painter.text(
                    monitor_rect.center(),
                    Align2::CENTER_CENTER,
                    format!("{}", index + 1),
                    FontId::proportional(14.0),
                    Color32::from_rgb(238, 241, 235),
                );

                if response.clicked() && pointer.is_some_and(|pos| monitor_rect.contains(pos)) {
                    clicked_index = Some(index);
                }
            }

            if let Some(index) = clicked_index {
                self.set_selected_monitor(index);
            }
        }

        fn ui_bind_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, action: Action) {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(action.label())
                        .font(FontId::proportional(13.5))
                        .strong()
                        .color(Color32::from_rgb(224, 228, 219)),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let recording = self.capturing == Some(action);
                    let text = if recording {
                        "Recording".to_string()
                    } else {
                        self.settings
                            .hotkey(action)
                            .map(|hotkey| hotkey.display())
                            .unwrap_or_else(|| "Set key".to_string())
                    };
                    let fill = if recording {
                        Color32::from_rgb(139, 98, 42)
                    } else {
                        Color32::from_rgb(39, 42, 38)
                    };
                    if ui
                        .add(
                            egui::Button::new(RichText::new(text).strong())
                                .fill(fill)
                                .stroke(Stroke::new(1.0, Color32::from_rgb(75, 81, 72)))
                                .min_size(vec2(120.0, 24.0)),
                        )
                        .clicked()
                    {
                        if recording {
                            self.cancel_capture(ctx);
                        } else {
                            self.start_capture(action);
                        }
                    }
                });
            });
        }
    }

    impl eframe::App for ScreenLockerApp {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            while let Ok(action) = self.hotkey_rx.try_recv() {
                self.apply_action(action);
            }
            self.try_capture_bind(ctx);
            self.poll_toggle_bind();

            if self.locked {
                let now = Instant::now();
                if now.duration_since(self.last_display_check) >= DISPLAY_CHECK_INTERVAL {
                    self.last_display_check = now;
                    self.check_display_changes();
                }
            }

            ctx.request_repaint_after(ACTION_POLL_INTERVAL);
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            let ctx = ui.ctx().clone();
            egui::Frame::new()
                .fill(Color32::from_rgb(14, 15, 13))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    self.ui_top_bar(ui);
                    ui.add_space(5.0);

                    egui::ScrollArea::vertical().show(ui, |ui| {
                        self.ui_lock_dashboard(ui, &ctx);
                    });
                });
        }
    }

    impl Drop for ScreenLockerApp {
        fn drop(&mut self) {
            self.stop_lock_worker();
            release_cursor();
            self.stop_hotkeys();
            if self.settings_dirty {
                let _ = self.settings.save();
            }
        }
    }

    pub fn run() -> eframe::Result {
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }

        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size(Vec2::new(700.0, 235.0))
                .with_min_inner_size(Vec2::new(700.0, 235.0))
                .with_resizable(false)
                .with_app_id("fpsheaven.screen_locker"),
            renderer: eframe::Renderer::Glow,
            ..Default::default()
        };

        eframe::run_native(
            "Simple Cursor Locker by FPSHEAVEN",
            options,
            Box::new(|cc| Ok(Box::new(ScreenLockerApp::new(cc)))),
        )
    }

    unsafe extern "system" fn enum_monitor_proc(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let monitors = &mut *(lparam as *mut Vec<Monitor>);
        let mut info = MONITORINFOEXW {
            cbSize: size_of::<MONITORINFOEXW>() as u32,
            rcMonitor: RECT::default(),
            rcWork: RECT::default(),
            dwFlags: 0,
            szDevice: [0; 32],
        };

        if GetMonitorInfoW(hmonitor, &mut info) != 0 {
            monitors.push(Monitor {
                rect: info.rcMonitor,
                work_rect: info.rcWork,
                device: wide_z_to_string(&info.szDevice),
                primary: info.dwFlags & 1 != 0,
            });
        }

        1
    }

    fn enumerate_monitors() -> Vec<Monitor> {
        let mut monitors = Vec::new();
        unsafe {
            EnumDisplayMonitors(
                0,
                null(),
                Some(enum_monitor_proc),
                &mut monitors as *mut Vec<Monitor> as LPARAM,
            );
        }
        monitors
    }

    fn virtual_bounds(monitors: &[Monitor]) -> RECT {
        monitors.iter().fold(
            RECT {
                left: i32::MAX,
                top: i32::MAX,
                right: i32::MIN,
                bottom: i32::MIN,
            },
            |acc, monitor| RECT {
                left: acc.left.min(monitor.rect.left),
                top: acc.top.min(monitor.rect.top),
                right: acc.right.max(monitor.rect.right),
                bottom: acc.bottom.max(monitor.rect.bottom),
            },
        )
    }

    fn monitors_equal(left: &[Monitor], right: &[Monitor]) -> bool {
        left.len() == right.len()
            && left.iter().zip(right).all(|(left, right)| {
                left.rect == right.rect
                    && left.work_rect == right.work_rect
                    && left.device == right.device
                    && left.primary == right.primary
            })
    }

    fn lock_targets_equal(left: &[Monitor], right: &[Monitor]) -> bool {
        left.len() == right.len()
            && left.iter().zip(right).all(|(left, right)| {
                left.rect == right.rect
                    && left.device == right.device
                    && left.primary == right.primary
            })
    }

    fn clamp_selected_monitor(settings: &mut Settings, monitor_count: usize) {
        if monitor_count == 0 || settings.selected_monitor >= monitor_count {
            settings.selected_monitor = 0;
        }
    }

    fn clip_cursor_to(rect: RECT) -> Result<(), u32> {
        unsafe {
            if ClipCursor(&rect) != 0 {
                Ok(())
            } else {
                Err(GetLastError())
            }
        }
    }

    fn enforce_cursor_rect(rect: RECT) -> Result<(), u32> {
        clip_cursor_to(rect)?;

        if let Some(point) = cursor_position()
            && !rect_contains_point(rect, point)
        {
            let x = point.x.clamp(rect.left, rect.right.saturating_sub(1));
            let y = point.y.clamp(rect.top, rect.bottom.saturating_sub(1));
            unsafe {
                SetCursorPos(x, y);
            }
        }

        Ok(())
    }

    fn release_cursor() {
        unsafe {
            ClipCursor(null());
        }
    }

    fn cursor_position() -> Option<POINT> {
        unsafe {
            let mut point = POINT::default();
            if GetCursorPos(&mut point) != 0 {
                Some(point)
            } else {
                None
            }
        }
    }

    fn rect_contains_point(rect: RECT, point: POINT) -> bool {
        point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
    }

    fn configure_style(ctx: &egui::Context) {
        ctx.set_theme(egui::Theme::Dark);
        let mut style = (*ctx.global_style()).clone();
        style.spacing.item_spacing = vec2(6.0, 5.0);
        style.spacing.button_padding = vec2(8.0, 4.0);
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = Color32::from_rgb(14, 15, 13);
        style.visuals.window_fill = Color32::from_rgb(22, 24, 21);
        style.visuals.selection.bg_fill = Color32::from_rgb(42, 126, 83);
        style.visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(238, 241, 235));
        style.visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(28, 30, 27);
        style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(36, 39, 35);
        style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(36, 39, 35);
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(48, 54, 47);
        style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(48, 54, 47);
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(42, 126, 83);
        style.visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.active.corner_radius = CornerRadius::same(8);
        ctx.set_global_style(style);
    }

    fn panel_frame() -> egui::Frame {
        egui::Frame::new()
            .fill(Color32::from_rgb(24, 26, 23))
            .stroke(Stroke::new(1.0, Color32::from_rgb(58, 63, 56)))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::same(8))
    }

    fn section_title(ui: &mut egui::Ui, text: &str) {
        ui.label(
            RichText::new(text)
                .font(FontId::proportional(13.5))
                .strong()
                .color(Color32::from_rgb(235, 239, 229)),
        );
    }

    fn config_path() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("screen_locker").join("settings.ini"))
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|path| path.join("screen_locker.ini"))
            })
    }

    fn wide_z_to_string(value: &[u16]) -> String {
        let len = value.iter().position(|ch| *ch == 0).unwrap_or(value.len());
        String::from_utf16_lossy(&value[..len])
    }

    fn pressed_modifiers() -> u32 {
        let mut modifiers = 0;
        unsafe {
            if key_down(VK_CONTROL) {
                modifiers |= MOD_CONTROL;
            }
            if key_down(VK_MENU) {
                modifiers |= MOD_ALT;
            }
            if key_down(VK_SHIFT) {
                modifiers |= MOD_SHIFT;
            }
            if key_down(VK_LWIN) || key_down(VK_RWIN) {
                modifiers |= MOD_WIN;
            }
        }
        modifiers
    }

    fn hotkey_is_down(hotkey: Hotkey) -> bool {
        if !async_key_down(hotkey.vk) {
            return false;
        }

        let mut required = normalize_modifiers(hotkey.modifiers);
        let mut held = pressed_modifiers_async();
        if is_letter_vk(hotkey.vk) {
            required &= !MOD_SHIFT;
            held &= !MOD_SHIFT;
        }

        held & required == required
    }

    fn pressed_modifiers_async() -> u32 {
        let mut modifiers = 0;
        if async_key_down(VK_CONTROL as u32) {
            modifiers |= MOD_CONTROL;
        }
        if async_key_down(VK_MENU as u32) {
            modifiers |= MOD_ALT;
        }
        if async_key_down(VK_SHIFT as u32) {
            modifiers |= MOD_SHIFT;
        }
        if async_key_down(VK_LWIN as u32) || async_key_down(VK_RWIN as u32) {
            modifiers |= MOD_WIN;
        }
        modifiers
    }

    fn async_key_down(vk: u32) -> bool {
        unsafe { GetAsyncKeyState(vk as i32) < 0 }
    }

    unsafe fn key_down(vk: i32) -> bool {
        GetKeyState(vk) < 0
    }

    fn is_egui_modifier_key(key: egui::Key) -> bool {
        matches!(
            key,
            egui::Key::ShiftLeft
                | egui::Key::ShiftRight
                | egui::Key::ControlLeft
                | egui::Key::ControlRight
                | egui::Key::AltLeft
                | egui::Key::AltRight
                | egui::Key::SuperLeft
                | egui::Key::SuperRight
        )
    }

    fn parse_hotkey(value: &str) -> Option<Hotkey> {
        let mut modifiers = 0;
        let mut key = None;

        for part in value.split('+') {
            let token = part.trim();
            if token.eq_ignore_ascii_case("ctrl") || token.eq_ignore_ascii_case("control") {
                modifiers |= MOD_CONTROL;
            } else if token.eq_ignore_ascii_case("alt") {
                modifiers |= MOD_ALT;
            } else if token.eq_ignore_ascii_case("shift") {
                modifiers |= MOD_SHIFT;
            } else if token.eq_ignore_ascii_case("win") || token.eq_ignore_ascii_case("windows") {
                modifiers |= MOD_WIN;
            } else {
                key = vk_from_name(token);
            }
        }

        key.filter(|vk| !is_modifier_vk(*vk))
            .map(|vk| Hotkey::new(modifiers, vk))
    }

    fn vk_from_name(value: &str) -> Option<u32> {
        let upper = value.trim().to_ascii_uppercase();
        if upper.len() == 1 {
            let byte = upper.as_bytes()[0];
            if byte.is_ascii_alphanumeric() {
                return Some(byte as u32);
            }
        }

        if let Some(number) = upper.strip_prefix('F').and_then(|n| n.parse::<u32>().ok())
            && (1..=24).contains(&number)
        {
            return Some(0x70 + number - 1);
        }

        match upper.as_str() {
            "ESC" | "ESCAPE" => Some(0x1b),
            "SPACE" => Some(0x20),
            "TAB" => Some(0x09),
            "ENTER" | "RETURN" => Some(0x0d),
            "BACKSPACE" => Some(0x08),
            "INSERT" | "INS" => Some(0x2d),
            "DELETE" | "DEL" => Some(0x2e),
            "HOME" => Some(0x24),
            "END" => Some(0x23),
            "PAGEUP" | "PGUP" => Some(0x21),
            "PAGEDOWN" | "PGDN" => Some(0x22),
            "LEFT" => Some(0x25),
            "UP" => Some(0x26),
            "RIGHT" => Some(0x27),
            "DOWN" => Some(0x28),
            "SEMICOLON" => Some(0xba),
            "PLUS" => Some(0xbb),
            "COMMA" => Some(0xbc),
            "MINUS" => Some(0xbd),
            "PERIOD" => Some(0xbe),
            "SLASH" => Some(0xbf),
            "BACKTICK" => Some(0xc0),
            "OPENBRACKET" => Some(0xdb),
            "BACKSLASH" => Some(0xdc),
            "CLOSEBRACKET" => Some(0xdd),
            "QUOTE" => Some(0xde),
            _ => None,
        }
    }

    fn vk_to_name(vk: u32) -> String {
        if (b'A' as u32..=b'Z' as u32).contains(&vk) || (b'0' as u32..=b'9' as u32).contains(&vk) {
            return (vk as u8 as char).to_string();
        }

        if (0x70..=0x87).contains(&vk) {
            return format!("F{}", vk - 0x70 + 1);
        }

        match vk {
            0x1b => "Esc",
            0x20 => "Space",
            0x09 => "Tab",
            0x0d => "Enter",
            0x08 => "Backspace",
            0x2d => "Insert",
            0x2e => "Delete",
            0x24 => "Home",
            0x23 => "End",
            0x21 => "PageUp",
            0x22 => "PageDown",
            0x25 => "Left",
            0x26 => "Up",
            0x27 => "Right",
            0x28 => "Down",
            0xba => "Semicolon",
            0xbb => "Plus",
            0xbc => "Comma",
            0xbd => "Minus",
            0xbe => "Period",
            0xbf => "Slash",
            0xc0 => "Backtick",
            0xdb => "OpenBracket",
            0xdc => "Backslash",
            0xdd => "CloseBracket",
            0xde => "Quote",
            _ => return format!("VK{vk}"),
        }
        .to_string()
    }

    fn is_modifier_vk(vk: u32) -> bool {
        matches!(vk, 0x10 | 0x11 | 0x12 | 0x5b | 0x5c | 0xa0..=0xa5)
    }

    fn egui_key_to_vk(key: egui::Key) -> Option<u32> {
        match key {
            egui::Key::A => Some(b'A' as u32),
            egui::Key::B => Some(b'B' as u32),
            egui::Key::C => Some(b'C' as u32),
            egui::Key::D => Some(b'D' as u32),
            egui::Key::E => Some(b'E' as u32),
            egui::Key::F => Some(b'F' as u32),
            egui::Key::G => Some(b'G' as u32),
            egui::Key::H => Some(b'H' as u32),
            egui::Key::I => Some(b'I' as u32),
            egui::Key::J => Some(b'J' as u32),
            egui::Key::K => Some(b'K' as u32),
            egui::Key::L => Some(b'L' as u32),
            egui::Key::M => Some(b'M' as u32),
            egui::Key::N => Some(b'N' as u32),
            egui::Key::O => Some(b'O' as u32),
            egui::Key::P => Some(b'P' as u32),
            egui::Key::Q => Some(b'Q' as u32),
            egui::Key::R => Some(b'R' as u32),
            egui::Key::S => Some(b'S' as u32),
            egui::Key::T => Some(b'T' as u32),
            egui::Key::U => Some(b'U' as u32),
            egui::Key::V => Some(b'V' as u32),
            egui::Key::W => Some(b'W' as u32),
            egui::Key::X => Some(b'X' as u32),
            egui::Key::Y => Some(b'Y' as u32),
            egui::Key::Z => Some(b'Z' as u32),
            egui::Key::Num0 => Some(b'0' as u32),
            egui::Key::Num1 => Some(b'1' as u32),
            egui::Key::Num2 => Some(b'2' as u32),
            egui::Key::Num3 => Some(b'3' as u32),
            egui::Key::Num4 => Some(b'4' as u32),
            egui::Key::Num5 => Some(b'5' as u32),
            egui::Key::Num6 => Some(b'6' as u32),
            egui::Key::Num7 => Some(b'7' as u32),
            egui::Key::Num8 => Some(b'8' as u32),
            egui::Key::Num9 => Some(b'9' as u32),
            egui::Key::F1 => Some(0x70),
            egui::Key::F2 => Some(0x71),
            egui::Key::F3 => Some(0x72),
            egui::Key::F4 => Some(0x73),
            egui::Key::F5 => Some(0x74),
            egui::Key::F6 => Some(0x75),
            egui::Key::F7 => Some(0x76),
            egui::Key::F8 => Some(0x77),
            egui::Key::F9 => Some(0x78),
            egui::Key::F10 => Some(0x79),
            egui::Key::F11 => Some(0x7a),
            egui::Key::F12 => Some(0x7b),
            egui::Key::F13 => Some(0x7c),
            egui::Key::F14 => Some(0x7d),
            egui::Key::F15 => Some(0x7e),
            egui::Key::F16 => Some(0x7f),
            egui::Key::F17 => Some(0x80),
            egui::Key::F18 => Some(0x81),
            egui::Key::F19 => Some(0x82),
            egui::Key::F20 => Some(0x83),
            egui::Key::F21 => Some(0x84),
            egui::Key::F22 => Some(0x85),
            egui::Key::F23 => Some(0x86),
            egui::Key::F24 => Some(0x87),
            egui::Key::Escape => Some(0x1b),
            egui::Key::Space => Some(0x20),
            egui::Key::Tab => Some(0x09),
            egui::Key::Enter => Some(0x0d),
            egui::Key::Backspace => Some(0x08),
            egui::Key::Insert => Some(0x2d),
            egui::Key::Delete => Some(0x2e),
            egui::Key::Home => Some(0x24),
            egui::Key::End => Some(0x23),
            egui::Key::PageUp => Some(0x21),
            egui::Key::PageDown => Some(0x22),
            egui::Key::ArrowLeft => Some(0x25),
            egui::Key::ArrowUp => Some(0x26),
            egui::Key::ArrowRight => Some(0x27),
            egui::Key::ArrowDown => Some(0x28),
            egui::Key::Semicolon => Some(0xba),
            egui::Key::Plus => Some(0xbb),
            egui::Key::Comma => Some(0xbc),
            egui::Key::Minus => Some(0xbd),
            egui::Key::Period => Some(0xbe),
            egui::Key::Slash | egui::Key::Questionmark => Some(0xbf),
            egui::Key::Backtick => Some(0xc0),
            egui::Key::OpenBracket | egui::Key::OpenCurlyBracket => Some(0xdb),
            egui::Key::Backslash | egui::Key::Pipe => Some(0xdc),
            egui::Key::CloseBracket | egui::Key::CloseCurlyBracket => Some(0xdd),
            egui::Key::Quote => Some(0xde),
            _ => None,
        }
    }
}
