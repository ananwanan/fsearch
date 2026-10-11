use crate::model::{move_selection, Phase, ViewState};
use crate::platform::{load_cjk_font, Action, PlatformWorker};
use crate::worker::{EntryFilter, EntryKind, Query, Worker};
use eframe::egui::{self, Align, Color32, FontId, Layout, RichText};
use fsearch_core::SearchMode;
use std::io;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(175);
const ROW_HEIGHT: f32 = 42.0;
const ACCENT: Color32 = Color32::from_rgb(110, 218, 180);

pub struct SearchApp {
    worker: Worker,
    platform: PlatformWorker,
    state: ViewState,
    root: PathBuf,
    root_text: String,
    root_edited: bool,
    query: String,
    mode: SearchMode,
    filter: EntryFilter,
    query_deadline: Option<Instant>,
    focus_search: bool,
    show_settings: bool,
    chinese: bool,
    cjk_available: bool,
    dark: bool,
    zoom: f32,
    notice: Option<String>,
    visible_rows: Range<usize>,
    scroll_selection: bool,
    startup_repaints: u8,
}

impl SearchApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        directory: Option<PathBuf>,
        query: String,
        mode: SearchMode,
    ) -> io::Result<Self> {
        let cjk_available = if let Some(font) = load_cjk_font() {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "system-cjk".to_owned(),
                Arc::new(egui::FontData::from_owned(font)),
            );
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("system-cjk".to_owned());
            }
            cc.egui_ctx.set_fonts(fonts);
            true
        } else {
            false
        };
        configure_style(&cc.egui_ctx, true);
        let worker_ctx = cc.egui_ctx.clone();
        let worker = Worker::new(move || worker_ctx.request_repaint())?;
        let platform_ctx = cc.egui_ctx.clone();
        let platform = PlatformWorker::new(move || platform_ctx.request_repaint())?;
        let auto_scan = directory.is_some();
        let root = match directory {
            Some(root) => root,
            None => std::env::current_dir()?,
        };
        let mut app = Self {
            root_text: root.to_string_lossy().into_owned(),
            root,
            root_edited: false,
            worker,
            platform,
            state: ViewState::default(),
            query,
            mode,
            filter: EntryFilter::All,
            query_deadline: None,
            focus_search: true,
            show_settings: false,
            chinese: cjk_available,
            cjk_available,
            dark: true,
            zoom: 1.0,
            notice: None,
            visible_rows: 0..0,
            scroll_selection: false,
            startup_repaints: 3,
        };
        if auto_scan {
            app.start_scan();
        }
        Ok(app)
    }

    fn query_spec(&self) -> Query {
        Query {
            text: self.query.clone(),
            mode: self.mode,
            filter: self.filter,
        }
    }

    fn start_scan(&mut self) {
        if self.root_text.is_empty() {
            self.notice = Some(
                text(
                    self.chinese,
                    "Enter a search location first.",
                    "请先输入搜索范围。",
                )
                .to_owned(),
            );
            return;
        }
        if self.root_edited {
            self.root = PathBuf::from(&self.root_text);
            self.root_edited = false;
        }
        self.state.begin_scan();
        self.query_deadline = None;
        self.notice = None;
        self.scroll_selection = true;
        self.worker.scan(
            self.state.scan_generation,
            self.state.query_generation,
            self.root.clone(),
            self.query_spec(),
        );
    }

    fn query_changed(&mut self, ctx: &egui::Context) {
        self.state.change_query();
        self.worker.cancel_query();
        self.query_deadline = Some(Instant::now() + DEBOUNCE);
        self.scroll_selection = true;
        ctx.request_repaint_after(DEBOUNCE);
    }

    fn cancel(&mut self) {
        if self.state.phase == Phase::Scanning {
            self.worker.cancel_scan();
        }
        self.worker.cancel_query();
        self.query_deadline = None;
        self.state.cancel();
    }

    fn drain_events(&mut self) {
        // The channels are bounded; processing does not traverse the full snapshot.
        while let Ok(event) = self.worker.events.try_recv() {
            self.state.apply(event);
        }
        while let Ok(event) = self.platform.events.try_recv() {
            self.notice = Some(event.error.unwrap_or_else(|| {
                text(
                    self.chinese,
                    "Opened in the system application.",
                    "已交给系统应用打开。",
                )
                .to_owned()
            }));
        }
    }

    fn submit_debounced_query(&mut self, ctx: &egui::Context) {
        let Some(deadline) = self.query_deadline else {
            return;
        };
        let now = Instant::now();
        if now < deadline {
            ctx.request_repaint_after(deadline - now);
            return;
        }
        self.query_deadline = None;
        if self.state.has_snapshot || self.state.phase == Phase::Scanning {
            self.worker.query(
                self.state.scan_generation,
                self.state.query_generation,
                self.query_spec(),
            );
        }
    }

    fn selected_path(&self) -> Option<PathBuf> {
        let row = self.state.selected?;
        let index = *self.state.indices.get(row)?;
        Some(self.state.entries.get(index)?.path.clone())
    }

    fn open_selected(&mut self, reveal: bool) {
        if let Some(path) = self.selected_path() {
            let action = if reveal {
                Action::Reveal(path)
            } else {
                Action::Open(path)
            };
            if let Err(error) = self.platform.send(action) {
                self.notice = Some(error);
            }
        }
    }

    fn copy_selected(&mut self, ctx: &egui::Context) {
        if let Some(path) = self.selected_path() {
            ctx.copy_text(path.to_string_lossy().into_owned());
            self.notice = Some(text(self.chinese, "Path copied.", "路径已复制。").to_owned());
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::L)) {
            self.show_settings = false;
            self.focus_search = true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::R)) {
            self.start_scan();
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
            self.open_selected(true);
        }
        if ctx.input_mut(consume_path_copy) {
            self.copy_selected(ctx);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            if self.show_settings {
                self.show_settings = false;
            } else if matches!(self.state.phase, Phase::Scanning | Phase::Querying) {
                self.cancel();
            } else if !self.query.is_empty() {
                self.query.clear();
                self.query_changed(ctx);
            }
        }
        let focus = ctx.memory(|memory| memory.focused());
        let search_focused = focus == Some(egui::Id::new("search-query"));
        let list_focused = focus == Some(egui::Id::new("result-list"));
        // Consume result navigation before TextEdit sees the keys. Root editing,
        // popup choices, and setting sliders keep their own keyboard behavior.
        if !self.show_settings && !egui::Popup::is_any_open(ctx) && (search_focused || list_focused)
        {
            for (key, down) in [(egui::Key::ArrowDown, true), (egui::Key::ArrowUp, false)] {
                if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key)) {
                    self.state.selected =
                        move_selection(self.state.selected, self.state.indices.len(), down);
                    self.scroll_selection = self
                        .state
                        .selected
                        .is_some_and(|row| !self.visible_rows.contains(&row));
                }
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                self.open_selected(false);
                self.focus_search = search_focused;
            }
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let zh = self.chinese;
        egui::Panel::top("header")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(22, 16)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("FSearch")
                            .size(28.0)
                            .strong()
                            .color(accent(ui)),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(text(
                            zh,
                            "Find your files. Stay in flow.",
                            "专注搜索，快速找到。",
                        ))
                        .color(ui.visuals().weak_text_color()),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(text(zh, "Settings", "设置")).clicked() {
                            self.show_settings = !self.show_settings;
                        }
                    });
                });
            });
    }

    fn controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let zh = self.chinese;
        egui::Panel::top("search-controls")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(22, 12)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(text(zh, "Location", "搜索范围"));
                    let width = (ui.available_width() - 140.0).max(160.0);
                    let response = ui.add_sized(
                        [width, 30.0],
                        egui::TextEdit::singleline(&mut self.root_text)
                            .id(egui::Id::new("search-root"))
                            .font(egui::TextStyle::Monospace),
                    );
                    if response.changed() {
                        self.root_edited = true;
                    }
                    let enter =
                        response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui
                        .button(text(zh, "Scan / Refresh", "扫描 / 刷新"))
                        .clicked()
                        || enter
                    {
                        ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                        self.start_scan();
                    }
                });
                ui.add_space(12.0);
                let response = ui.add_sized(
                    [ui.available_width(), 42.0],
                    egui::TextEdit::singleline(&mut self.query)
                        .id(egui::Id::new("search-query"))
                        .font(FontId::proportional(21.0))
                        .hint_text(text(
                            zh,
                            "Type to search this location…",
                            "输入名称或路径，搜索当前范围…",
                        )),
                );
                if self.focus_search {
                    response.request_focus();
                    self.focus_search = false;
                }
                if response.changed() {
                    self.query_changed(ctx);
                }
                ui.add_space(10.0);
                let old_mode = self.mode;
                let old_filter = self.filter;
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("match-mode")
                        .selected_text(mode_label(zh, self.mode))
                        .width(165.0)
                        .show_ui(ui, |ui| {
                            for mode in [
                                SearchMode::NameSubstring,
                                SearchMode::ExactName,
                                SearchMode::PathSubstring,
                                SearchMode::ExactPath,
                            ] {
                                ui.selectable_value(&mut self.mode, mode, mode_label(zh, mode));
                            }
                        });
                    egui::ComboBox::from_id_salt("entry-filter")
                        .selected_text(filter_label(zh, self.filter))
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            for filter in [
                                EntryFilter::All,
                                EntryFilter::Files,
                                EntryFilter::Directories,
                            ] {
                                ui.selectable_value(
                                    &mut self.filter,
                                    filter,
                                    filter_label(zh, filter),
                                );
                            }
                        });
                    ui.label(
                        RichText::new(text(
                            zh,
                            "Case-sensitive · literal matching",
                            "区分大小写 · 字面匹配",
                        ))
                        .small()
                        .color(ui.visuals().weak_text_color()),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if matches!(self.state.phase, Phase::Scanning | Phase::Querying)
                            && ui.button(text(zh, "Cancel", "取消任务")).clicked()
                        {
                            self.cancel();
                        }
                    });
                });
                if old_mode != self.mode || old_filter != self.filter {
                    self.query_changed(ctx);
                }
                if self.root_edited {
                    ui.small(text(
                        zh,
                        "Location changed. Scan to use the new location.",
                        "范围已修改，点击扫描后应用新范围。",
                    ));
                }
            });
    }

    fn footer(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let zh = self.chinese;
        egui::Panel::bottom("footer")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(22, 12)))
            .show(ui, |ui| {
                let enabled = self.selected_path().is_some();
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(enabled, |ui| {
                        if ui.button(text(zh, "Open", "打开")).clicked() { self.open_selected(false); }
                        if ui.button(text(zh, "Show in folder", "打开所在目录")).clicked() { self.open_selected(true); }
                        if ui.button(text(zh, "Copy path", "复制路径")).clicked() { self.copy_selected(ctx); }
                    });
                    if let Some(row) = self.state.selected {
                        if let Some(entry) = self.state.indices.get(row).and_then(|i| self.state.entries.get(*i)) {
                            ui.add(egui::Label::new(RichText::new(&entry.display_path).small()).truncate())
                                .on_hover_text(&entry.display_path);
                        }
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.colored_label(if self.state.phase == Phase::Failed { Color32::from_rgb(241, 135, 135) } else { accent(ui) }, self.status_text());
                    if self.state.issues > 0 {
                        ui.colored_label(Color32::from_rgb(233, 183, 105), format!("{} {}", self.state.issues, text(zh, "scan issues · partial results", "处扫描问题 · 结果可能不完整")));
                    }
                });
                if let Some(notice) = &self.notice { ui.small(notice); }
                if !self.state.diagnostics.is_empty() {
                    egui::CollapsingHeader::new(text(zh, "Error details", "错误详情")).show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(110.0).show(ui, |ui| {
                            for diagnostic in &self.state.diagnostics { ui.small(diagnostic); }
                        });
                    });
                }
                let hint = text(zh, "Ctrl+L search · ↑↓ select · Enter open · Ctrl+Shift+C copy · Ctrl+O locate · Ctrl+R rescan", "Ctrl+L 搜索 · ↑↓ 选择 · Enter 打开 · Ctrl+Shift+C 复制 · Ctrl+O 定位 · Ctrl+R 重扫");
                if cfg!(target_os = "macos") { ui.small(hint.replace("Ctrl", "Cmd")); } else { ui.small(hint); }
            });
    }

    fn status_text(&self) -> String {
        let zh = self.chinese;
        match self.state.phase {
            Phase::Idle => text(
                zh,
                "Ready to scan · paste a location or drop a folder",
                "等待扫描 · 输入路径或拖入文件夹",
            )
            .to_owned(),
            Phase::Scanning => format!(
                "{} {} {}",
                text(zh, "Scanning…", "正在扫描…"),
                self.state.visited,
                text(zh, "entries", "个条目")
            ),
            Phase::Querying => {
                text(zh, "Searching the scanned entries…", "正在搜索已扫描条目…").to_owned()
            }
            Phase::Ready => format!(
                "{} {} · {} {}",
                self.state.total_matches,
                text(zh, "matches", "个匹配"),
                self.state.entries.len(),
                text(zh, "scanned entries", "个已扫描条目")
            ),
            Phase::Cancelled => text(
                zh,
                "Task cancelled · scan again or edit your query",
                "任务已取消 · 可重新扫描或修改查询",
            )
            .to_owned(),
            Phase::Failed => text(
                zh,
                "Scan failed · check the location and error details",
                "扫描失败 · 请检查路径和错误详情",
            )
            .to_owned(),
        }
    }

    fn result_list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let zh = self.chinese;
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(22, 12)))
            .show(ui, |ui| {
                ui.interact(
                    ui.max_rect(),
                    egui::Id::new("result-list"),
                    egui::Sense::focusable_noninteractive(),
                );
                ui.horizontal(|ui| {
                    ui.label(RichText::new(text(zh, "RESULTS", "搜索结果")).strong());
                    if self.state.total_matches > self.state.indices.len() {
                        ui.small(format!(
                            "{} {} / {}",
                            text(zh, "Showing", "显示前"),
                            self.state.indices.len(),
                            self.state.total_matches
                        ));
                    }
                });
                ui.separator();
                if self.state.indices.is_empty() {
                    ui.add_space(50.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(match self.state.phase {
                                Phase::Ready => text(zh, "No matching entries", "没有匹配的条目"),
                                Phase::Scanning => {
                                    text(zh, "Scanning your location", "正在扫描搜索范围")
                                }
                                Phase::Querying => text(zh, "Searching…", "正在搜索…"),
                                Phase::Failed => {
                                    text(zh, "This location could not be scanned", "无法扫描此范围")
                                }
                                Phase::Cancelled => text(zh, "Task cancelled", "任务已取消"),
                                Phase::Idle => text(
                                    zh,
                                    "Your next file is a search away",
                                    "从一个搜索范围开始",
                                ),
                            })
                            .size(22.0),
                        );
                        ui.add_space(10.0);
                        ui.label(text(
                            zh,
                            "Choose a location and scan. Typing reuses the scan.",
                            "输入路径并扫描；后续输入会复用扫描结果。",
                        ));
                        ui.small(text(
                            zh,
                            "Exact path uses the original path spelling; fuzzy search is planned.",
                            "精确路径按原始拼写比较；模糊搜索仍在规划中。",
                        ));
                    });
                    return;
                }
                let count = self.state.indices.len();
                let entries = Arc::clone(&self.state.entries);
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt("results")
                    .auto_shrink([false, false]);
                if self.scroll_selection {
                    let row = self.state.selected.unwrap_or(0).saturating_sub(3);
                    scroll = scroll.vertical_scroll_offset(
                        row as f32 * (ROW_HEIGHT + ui.spacing().item_spacing.y),
                    );
                    self.scroll_selection = false;
                }
                scroll.show_rows(ui, ROW_HEIGHT, count, |ui, range| {
                    self.visible_rows = range.clone();
                    for row in range {
                        let Some(entry) = entries.get(self.state.indices[row]) else {
                            continue;
                        };
                        let selected = self.state.selected == Some(row);
                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), ROW_HEIGHT),
                            egui::Sense::click(),
                        );
                        response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::SelectableLabel,
                                true,
                                selected,
                                &entry.name,
                            )
                        });
                        if selected || response.hovered() || row % 2 == 0 {
                            let fill = if selected {
                                ui.visuals().selection.bg_fill
                            } else if response.hovered() {
                                ui.visuals().widgets.hovered.weak_bg_fill
                            } else {
                                ui.visuals().faint_bg_color
                            };
                            ui.painter().rect_filled(rect, 4, fill);
                        }
                        ui.scope_builder(
                            egui::UiBuilder::new()
                                .max_rect(rect.shrink2(egui::vec2(10.0, 5.0)))
                                .layout(Layout::left_to_right(Align::Center)),
                            |ui| {
                                let name_width = (rect.width() * 0.35).max(190.0);
                                ui.allocate_ui_with_layout(
                                    egui::vec2(name_width, ROW_HEIGHT - 10.0),
                                    Layout::left_to_right(Align::Center),
                                    |ui| {
                                        ui.set_min_width(name_width);
                                        ui.label(
                                            RichText::new(kind_label(zh, entry.kind))
                                                .small()
                                                .color(accent(ui)),
                                        );
                                        ui.add(
                                            egui::Label::new(RichText::new(&entry.name).strong())
                                                .truncate(),
                                        );
                                    },
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&entry.display_path)
                                            .color(ui.visuals().weak_text_color()),
                                    )
                                    .truncate(),
                                );
                            },
                        );
                        if response.clicked() {
                            self.state.selected = Some(row);
                            ctx.memory_mut(|memory| {
                                memory.request_focus(egui::Id::new("result-list"))
                            });
                        }
                        if response.double_clicked() {
                            self.state.selected = Some(row);
                            self.open_selected(false);
                        }
                        response
                            .on_hover_text(&entry.display_path)
                            .context_menu(|ui| {
                                self.state.selected = Some(row);
                                if ui.button(text(zh, "Open", "打开")).clicked() {
                                    self.open_selected(false);
                                    ui.close();
                                }
                                if ui
                                    .button(text(zh, "Show in folder", "打开所在目录"))
                                    .clicked()
                                {
                                    self.open_selected(true);
                                    ui.close();
                                }
                                if ui.button(text(zh, "Copy path", "复制路径")).clicked() {
                                    self.copy_selected(ctx);
                                    ui.close();
                                }
                            });
                    }
                });
            });
    }

    fn settings(&mut self, ctx: &egui::Context) {
        let zh = self.chinese;
        let mut open = self.show_settings;
        let old_dark = self.dark;
        let old_zoom = self.zoom;
        egui::Window::new(text(zh, "Settings", "设置"))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.label(text(zh, "Appearance", "外观"));
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.dark, true, text(zh, "Dark", "深色"));
                    ui.selectable_value(&mut self.dark, false, text(zh, "Light", "浅色"));
                });
                ui.add(
                    egui::Slider::new(&mut self.zoom, 0.8..=1.6).text(text(zh, "Scale", "缩放")),
                );
                ui.separator();
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.chinese, false, "English");
                    ui.add_enabled_ui(self.cjk_available, |ui| {
                        ui.selectable_value(&mut self.chinese, true, "中文");
                    });
                });
                if !self.cjk_available {
                    ui.small(
                        "Chinese needs an installed CJK font (Noto Sans CJK / Microsoft YaHei).",
                    );
                }
                ui.separator();
                ui.small(text(
                    zh,
                    "Settings apply to this session. Case-sensitive matching is fixed.",
                    "设置仅用于本次运行。搜索固定区分大小写。",
                ));
                ui.small(text(
                    zh,
                    "Scans do not follow discovered directory links. Rescan after file changes.",
                    "扫描不跟随发现的目录链接；文件变更后请重新扫描。",
                ));
            });
        self.show_settings = open;
        if old_dark != self.dark {
            configure_style(ctx, self.dark);
        }
        if old_zoom != self.zoom {
            ctx.set_zoom_factor(self.zoom);
        }
    }
}

impl eframe::App for SearchApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe also invokes logic for worker wakeups while the window is
        // hidden. Drain bounded queues here so background completion never
        // waits for a visible frame or repeatedly wakes a minimized window.
        self.drain_events();
        self.submit_debounced_query(ctx);
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Settle the native window and newly loaded font atlas with a bounded
        // number of startup frames; normal idle rendering remains event-driven.
        if self.startup_repaints > 0 {
            self.startup_repaints -= 1;
            ctx.request_repaint_after(Duration::from_millis(30));
        }
        if let Some(path) = ctx.input(|i| {
            i.raw
                .dropped_files
                .first()
                .map(|file| file.path().to_path_buf())
        }) {
            self.root_text = path.to_string_lossy().into_owned();
            self.root = path;
            self.root_edited = false;
            self.start_scan();
        }
        self.shortcuts(&ctx);
        self.header(ui);
        self.controls(ui, &ctx);
        self.footer(ui, &ctx);
        self.result_list(ui, &ctx);
        if self.show_settings {
            self.settings(&ctx);
        }
    }
}

fn text<'a>(chinese: bool, english: &'a str, chinese_text: &'a str) -> &'a str {
    if chinese {
        chinese_text
    } else {
        english
    }
}

fn mode_label(chinese: bool, mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::ExactName => text(chinese, "Name · exact", "文件名 · 完全相同"),
        SearchMode::ExactPath => text(chinese, "Path · exact", "路径 · 完全相同"),
        SearchMode::NameSubstring => text(chinese, "Name · contains", "文件名 · 包含"),
        SearchMode::PathSubstring => text(chinese, "Path · contains", "路径 · 包含"),
    }
}

fn filter_label(chinese: bool, filter: EntryFilter) -> &'static str {
    match filter {
        EntryFilter::All => text(chinese, "All entries", "全部条目"),
        EntryFilter::Files => text(chinese, "Files", "仅文件"),
        EntryFilter::Directories => text(chinese, "Directories", "仅目录"),
    }
}

fn kind_label(chinese: bool, kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => text(chinese, "FILE", "文件"),
        EntryKind::Directory => text(chinese, "DIR", "目录"),
        EntryKind::Link => text(chinese, "LINK", "链接"),
        EntryKind::Other => text(chinese, "OTHER", "其他"),
    }
}

fn configure_style(ctx: &egui::Context, dark: bool) {
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    if dark {
        visuals.panel_fill = Color32::from_rgb(21, 28, 37);
        visuals.window_fill = Color32::from_rgb(25, 34, 44);
        visuals.extreme_bg_color = Color32::from_rgb(14, 21, 29);
        visuals.faint_bg_color = Color32::from_rgb(25, 33, 43);
        visuals.selection.bg_fill = Color32::from_rgb(32, 75, 69);
    } else {
        visuals.selection.bg_fill = Color32::from_rgb(198, 232, 221);
    }
    visuals.selection.stroke = egui::Stroke::new(
        1.0,
        if dark {
            ACCENT
        } else {
            Color32::from_rgb(35, 109, 88)
        },
    );
    ctx.set_visuals(visuals);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(10.0, 5.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(12.0));
        style
            .text_styles
            .insert(egui::TextStyle::Monospace, FontId::monospace(13.0));
    });
}

fn accent(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        ACCENT
    } else {
        Color32::from_rgb(35, 109, 88)
    }
}

fn consume_path_copy(input: &mut egui::InputState) -> bool {
    let modifiers = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
    if input.consume_key(modifiers, egui::Key::C) {
        return true;
    }
    // egui-winit translates command-C into Copy even with Shift. On a fast
    // chord the current modifier state may already be released, but its C-up
    // event still records the chord. Preserve ordinary text-copy events.
    let shifted = input.modifiers.matches_logically(modifiers)
        || input.events.iter().any(|event| {
            matches!(event,
            egui::Event::Key { key: egui::Key::C, modifiers: held, .. }
                if held.matches_logically(modifiers))
        });
    if shifted
        && input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::Copy))
    {
        input
            .events
            .retain(|event| !matches!(event, egui::Event::Copy));
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converted_shift_copy_is_a_path_shortcut() {
        let mut input = egui::InputState::default();
        input.modifiers = egui::Modifiers::CTRL | egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        input.events.push(egui::Event::Copy);
        assert!(consume_path_copy(&mut input));
        assert!(!input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::Copy)));
    }

    #[test]
    fn ordinary_copy_remains_available_to_text_edit() {
        let mut input = egui::InputState::default();
        input.modifiers = egui::Modifiers::CTRL | egui::Modifiers::COMMAND;
        input.events.push(egui::Event::Copy);
        assert!(!consume_path_copy(&mut input));
        assert!(input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::Copy)));
    }

    #[test]
    fn fast_released_chord_still_copies_the_path() {
        let mut input = egui::InputState::default();
        input.events.extend([
            egui::Event::Copy,
            egui::Event::Key {
                key: egui::Key::C,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::CTRL
                    | egui::Modifiers::COMMAND
                    | egui::Modifiers::SHIFT,
            },
        ]);
        assert!(consume_path_copy(&mut input));
    }
}
