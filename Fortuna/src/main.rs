use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use anyhow::{Context, Result, anyhow};
use eframe::egui;
use raphael_data::{CrafterStats, Locale, RECIPES, Recipe, get_game_settings, get_raw_item_name};
use raphael_sim::Action;
use raphael_solver::{AtomicFlag, MacroSolver, SolverSettings};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const WORLD: &str = "Phoenix";
const DATA_CENTER: &str = "Light";
const MAX_MARKET_IDS_PER_REQUEST: usize = 100;
const XIVAPI_CACHE_SCHEMA_VERSION: u32 = 3;

fn main() -> eframe::Result {
    log_terminal("Starting Fortuna Profit Crafter.");
    if std::env::args().any(|arg| arg == "--refresh-xivapi-cache") {
        let progress = |message: String| log_terminal(&message);
        match update_xivapi_library(&progress) {
            Ok(cache) => log_terminal(&format!(
                "XIVAPI cache ready: {} item types, {} items, {} vendor prices.",
                cache.categories.len(),
                cache.item_metadata.len(),
                cache.vendor_prices.len()
            )),
            Err(err) => log_terminal(&format!("XIVAPI cache refresh failed: {err:#}")),
        }
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 820.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Fortuna Profit Crafter",
        options,
        Box::new(|_cc| Ok(Box::<ProfitCrafterApp>::default())),
    )
}

struct ProfitCrafterApp {
    mode: AppMode,
    market_prices: HashMap<u32, MarketPrice>,
    rows: Vec<ProfitRow>,
    gather_rows: Vec<GatherRow>,
    item_categories: Vec<ItemCategory>,
    pending_item_filter: Option<u32>,
    applied_item_filter: Option<u32>,
    pending_category_filter: Option<u32>,
    applied_category_filter: Option<u32>,
    selected_item_id: Option<u32>,
    selected_recipe_id: Option<u32>,
    selected_cost_breakdown: Vec<CostNode>,
    pending_only_profitable: bool,
    applied_only_profitable: bool,
    pending_only_recently_sold: bool,
    applied_only_recently_sold: bool,
    status: String,
    market_rx: Option<Receiver<WorkMessage<MarketSnapshot>>>,
    solve_rx: Option<Receiver<WorkMessage<SolvedMacro>>>,
    update_rx: Option<Receiver<WorkMessage<String>>>,
    solved_macro: Option<SolvedMacro>,
    crafter_stats: CrafterStats,
}

impl Default for ProfitCrafterApp {
    fn default() -> Self {
        Self {
            mode: AppMode::Craft,
            market_prices: HashMap::new(),
            rows: Vec::new(),
            gather_rows: Vec::new(),
            item_categories: Vec::new(),
            pending_item_filter: None,
            applied_item_filter: None,
            pending_category_filter: None,
            applied_category_filter: None,
            selected_item_id: None,
            selected_recipe_id: None,
            selected_cost_breakdown: Vec::new(),
            pending_only_profitable: true,
            applied_only_profitable: true,
            pending_only_recently_sold: true,
            applied_only_recently_sold: true,
            status: String::new(),
            market_rx: None,
            solve_rx: None,
            update_rx: None,
            solved_macro: None,
            crafter_stats: CrafterStats::default(),
        }
    }
}

impl eframe::App for ProfitCrafterApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.receive_background_work(&ctx);

        egui::Panel::top("toolbar").show_inside(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let is_busy = self.is_busy();
                ui.heading("Fortuna Profit Crafter");
                ui.separator();
                ui.label(format!("{DATA_CENTER} / {WORLD}"));
                ui.separator();
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Refresh market"))
                    .clicked()
                {
                    self.refresh_market(&ctx);
                }
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Clone raphael-rs if missing"))
                    .clicked()
                {
                    self.ensure_repository(
                        &ctx,
                        "raphael-rs",
                        "https://github.com/KonaeAkira/raphael-rs.git",
                    );
                }
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Clone Universalis if missing"))
                    .clicked()
                {
                    self.ensure_repository(
                        &ctx,
                        "Universalis",
                        "https://github.com/Universalis-FFXIV/Universalis.git",
                    );
                }
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Update XIVAPI cache"))
                    .clicked()
                {
                    self.update_xivapi_cache(&ctx);
                }
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Solve selected macro"))
                    .clicked()
                {
                    self.solve_selected_macro(&ctx);
                }
                if !self.status.is_empty() {
                    ui.separator();
                    if is_busy {
                        ui.spinner();
                    }
                    ui.label(&self.status);
                }
            });
        });

        egui::Panel::left("controls")
            .resizable(false)
            .default_size(280.0)
            .show_inside(ui, |ui| {
                ui.heading("Inputs");
                ui.label("Crafter stats");
                ui.add(
                    egui::DragValue::new(&mut self.crafter_stats.level)
                        .range(1..=100)
                        .prefix("Level "),
                );
                ui.add(
                    egui::DragValue::new(&mut self.crafter_stats.craftsmanship)
                        .range(1..=9999)
                        .prefix("Craftsmanship "),
                );
                ui.add(
                    egui::DragValue::new(&mut self.crafter_stats.control)
                        .range(1..=9999)
                        .prefix("Control "),
                );
                ui.add(
                    egui::DragValue::new(&mut self.crafter_stats.cp)
                        .range(1..=9999)
                        .prefix("CP "),
                );
                ui.checkbox(&mut self.crafter_stats.manipulation, "Manipulation");
                ui.checkbox(&mut self.crafter_stats.heart_and_soul, "Heart and Soul");
                ui.checkbox(&mut self.crafter_stats.quick_innovation, "Quick Innovation");

                ui.separator();
                ui.label("Filters");
                self.draw_filter_controls(ui);

                ui.separator();
                ui.label(format!("Market prices: {}", self.market_prices.len()));
                ui.label(format!("Ranked recipes: {}", self.rows.len()));
                ui.label(format!("Gathering rows: {}", self.gather_rows.len()));
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.mode, AppMode::Craft, "Craft mode");
                ui.selectable_value(&mut self.mode, AppMode::Gather, "Gathering mode");
            });
            ui.separator();

            match self.mode {
                AppMode::Craft => {
                    ui.columns(2, |columns| {
                        self.draw_profit_table(&mut columns[0]);
                        self.draw_selected_recipe(&mut columns[1]);
                    });
                }
                AppMode::Gather => {
                    self.draw_gather_table(ui);
                }
            }
        });
    }
}

impl ProfitCrafterApp {
    fn receive_background_work(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.market_rx.take() {
            match rx.try_recv() {
                Ok(WorkMessage::Progress(message)) => {
                    self.status = message;
                    self.market_rx = Some(rx);
                }
                Ok(WorkMessage::Done(Ok(snapshot))) => {
                    self.item_categories = snapshot.categories;
                    self.market_prices = snapshot.prices;
                    self.rows = rank_profitability(&self.market_prices);
                    self.gather_rows = rank_gathering(&self.market_prices);
                    self.clear_unavailable_filters();
                    self.status = format!(
                        "Loaded {} prices, XIVAPI item types, NPC vendor prices, and recent sale counts.",
                        self.market_prices.len()
                    );
                    self.market_rx = None;
                }
                Ok(WorkMessage::Done(Err(err))) => {
                    self.status = format!("Market refresh failed: {err:#}");
                    self.market_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => self.market_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = "Market refresh worker stopped unexpectedly.".to_string();
                    self.market_rx = None;
                }
            }
        }

        if let Some(rx) = self.solve_rx.take() {
            match rx.try_recv() {
                Ok(WorkMessage::Progress(message)) => {
                    self.status = message;
                    self.solve_rx = Some(rx);
                }
                Ok(WorkMessage::Done(Ok(solved))) => {
                    self.status = format!(
                        "Solved {} craft macros with {} total actions.",
                        solved.plans.len(),
                        solved.total_actions()
                    );
                    self.solved_macro = Some(solved);
                    self.solve_rx = None;
                }
                Ok(WorkMessage::Done(Err(err))) => {
                    self.status = format!("Macro solve failed: {err:#}");
                    self.solve_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => self.solve_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = "Macro solver stopped unexpectedly.".to_string();
                    self.solve_rx = None;
                }
            }
        }

        if let Some(rx) = self.update_rx.take() {
            match rx.try_recv() {
                Ok(WorkMessage::Progress(message)) => {
                    self.status = message;
                    self.update_rx = Some(rx);
                }
                Ok(WorkMessage::Done(Ok(message))) => {
                    self.status = message;
                    self.update_rx = None;
                }
                Ok(WorkMessage::Done(Err(err))) => {
                    self.status = format!("Update failed: {err:#}");
                    self.update_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => self.update_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = "Repository updater stopped unexpectedly.".to_string();
                    self.update_rx = None;
                }
            }
        }

        if self.market_rx.is_some() || self.solve_rx.is_some() || self.update_rx.is_some() {
            ctx.request_repaint();
        }
    }

    fn refresh_market(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.market_rx = Some(rx);
        self.status = "Refreshing Universalis market data...".to_string();
        ctx.request_repaint();
        thread::spawn(move || {
            report_progress(&tx, "Preparing market refresh.");
            let progress = |message| report_progress(&tx, message);
            let result = fetch_market_snapshot(&progress);
            let _ = tx.send(WorkMessage::Done(result));
        });
    }

    fn ensure_repository(
        &mut self,
        ctx: &egui::Context,
        repo_dir: &'static str,
        url: &'static str,
    ) {
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        self.status = format!("Ensuring {repo_dir} repository exists...");
        ctx.request_repaint();
        thread::spawn(move || {
            report_progress(&tx, format!("Checking {repo_dir} repository."));
            let root = workspace_root();
            let result = ensure_repo(&root.join(repo_dir), url)
                .map(|message| format!("{repo_dir}: {message}"));
            let _ = tx.send(WorkMessage::Done(result));
        });
    }

    fn update_xivapi_cache(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        self.status = "Updating local XIVAPI cache...".to_string();
        ctx.request_repaint();
        thread::spawn(move || {
            report_progress(&tx, "Refreshing XIVAPI cache from v2 sheets.");
            let progress = |message| report_progress(&tx, message);
            let result = update_xivapi_library(&progress).map(|cache| {
                format!(
                    "XIVAPI cache updated: {} item types, {} items, {} NPC gil prices.",
                    cache.categories.len(),
                    cache.item_metadata.len(),
                    cache.vendor_prices.len()
                )
            });
            let _ = tx.send(WorkMessage::Done(result));
        });
    }

    fn solve_selected_macro(&mut self, ctx: &egui::Context) {
        let Some(recipe_id) = self.selected_recipe_id else {
            self.status = "Select a recipe first.".to_string();
            return;
        };
        let Some(recipe) = RECIPES.get(recipe_id).copied() else {
            self.status = "Selected recipe is no longer available.".to_string();
            return;
        };
        let stats = self.crafter_stats;
        let market_prices = self.market_prices.clone();
        let (tx, rx) = mpsc::channel();
        self.solve_rx = Some(rx);
        self.status = "Solving macros with Raphael...".to_string();
        ctx.request_repaint();
        thread::spawn(move || {
            report_progress(&tx, "Building craft dependency plan for selected recipe.");
            let progress = |message| report_progress(&tx, message);
            let result = solve_macro_plan(recipe_id, recipe, stats, market_prices, &progress);
            let _ = tx.send(WorkMessage::Done(result));
        });
    }

    fn draw_filter_controls(&mut self, ui: &mut egui::Ui) {
        let item_options = self.item_filter_options();
        egui::ComboBox::from_label("Item")
            .selected_text(self.selected_item_filter_text())
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.pending_item_filter, None, "All items");
                for (item_id, name) in item_options {
                    ui.selectable_value(&mut self.pending_item_filter, Some(item_id), name);
                }
            });

        egui::ComboBox::from_label("Item type")
            .selected_text(self.selected_category_filter_text())
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.pending_category_filter, None, "All item types");
                for category in &self.item_categories {
                    ui.selectable_value(
                        &mut self.pending_category_filter,
                        Some(category.id),
                        &category.name,
                    );
                }
            });

        ui.checkbox(&mut self.pending_only_profitable, "Only profitable");
        ui.checkbox(
            &mut self.pending_only_recently_sold,
            "Only >4 sales in 2 days",
        );

        ui.horizontal(|ui| {
            let changed = self.filters_have_pending_changes();
            if ui
                .add_enabled(changed, egui::Button::new("Apply filters"))
                .clicked()
            {
                self.apply_filters();
            }
            if ui.button("Clear").clicked() {
                self.clear_filters();
            }
        });

        if self.filters_have_pending_changes() {
            ui.small("Filter changes are staged. Click Apply filters to update the ranking.");
        }
    }

    fn item_filter_options(&self) -> Vec<(u32, String)> {
        let mut items = match self.mode {
            AppMode::Craft => self
                .rows
                .iter()
                .map(|row| (row.item_id, row.item_name.clone()))
                .collect::<Vec<_>>(),
            AppMode::Gather => self
                .gather_rows
                .iter()
                .map(|row| (row.item_id, row.item_name.clone()))
                .collect::<Vec<_>>(),
        };
        items.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        items.dedup_by_key(|(item_id, _)| *item_id);
        items
    }

    fn selected_item_filter_text(&self) -> String {
        self.pending_item_filter
            .map(|item_id| item_name(item_id))
            .unwrap_or_else(|| "All items".to_string())
    }

    fn selected_category_filter_text(&self) -> String {
        self.pending_category_filter
            .and_then(|id| {
                self.item_categories
                    .iter()
                    .find(|category| category.id == id)
                    .map(|category| category.name.clone())
            })
            .unwrap_or_else(|| "All item types".to_string())
    }

    fn filters_have_pending_changes(&self) -> bool {
        self.pending_item_filter != self.applied_item_filter
            || self.pending_category_filter != self.applied_category_filter
            || self.pending_only_profitable != self.applied_only_profitable
            || self.pending_only_recently_sold != self.applied_only_recently_sold
    }

    fn apply_filters(&mut self) {
        self.applied_item_filter = self.pending_item_filter;
        self.applied_category_filter = self.pending_category_filter;
        self.applied_only_profitable = self.pending_only_profitable;
        self.applied_only_recently_sold = self.pending_only_recently_sold;
        self.status = format!(
            "Filters applied: {} craft rows, {} gathering rows.",
            self.filtered_rows().len(),
            self.filtered_gather_rows().len()
        );
    }

    fn clear_filters(&mut self) {
        self.pending_item_filter = None;
        self.applied_item_filter = None;
        self.pending_category_filter = None;
        self.applied_category_filter = None;
        self.pending_only_profitable = false;
        self.applied_only_profitable = false;
        self.pending_only_recently_sold = false;
        self.applied_only_recently_sold = false;
        self.status = "Filters cleared.".to_string();
    }

    fn clear_unavailable_filters(&mut self) {
        let available_items = self
            .rows
            .iter()
            .map(|row| row.item_id)
            .chain(self.gather_rows.iter().map(|row| row.item_id))
            .collect::<HashSet<_>>();
        for item_filter in [
            &mut self.pending_item_filter,
            &mut self.applied_item_filter,
        ] {
            if item_filter.is_some_and(|item_id| !available_items.contains(&item_id)) {
                *item_filter = None;
            }
        }

        let available_categories = self
            .item_categories
            .iter()
            .map(|category| category.id)
            .collect::<HashSet<_>>();
        for category_filter in [
            &mut self.pending_category_filter,
            &mut self.applied_category_filter,
        ] {
            if category_filter.is_some_and(|id| !available_categories.contains(&id)) {
                *category_filter = None;
            }
        }
    }

    fn is_busy(&self) -> bool {
        self.market_rx.is_some() || self.solve_rx.is_some() || self.update_rx.is_some()
    }

    fn draw_profit_table(&mut self, ui: &mut egui::Ui) {
        ui.heading("Profit ranking");
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("profit_grid")
                .striped(true)
                .num_columns(7)
                .show(ui, |ui| {
                    ui.strong("Item");
                    ui.strong("Type");
                    ui.strong("Sell");
                    ui.strong("Cost");
                    ui.strong("Profit");
                    ui.strong("2d sales");
                    ui.strong("Velocity");
                    ui.end_row();

                    for row in self.filtered_rows().into_iter().take(300) {
                        let selected = self.selected_item_id == Some(row.item_id);
                        if ui.selectable_label(selected, &row.item_name).clicked() {
                            self.selected_item_id = Some(row.item_id);
                            self.selected_recipe_id = Some(row.recipe_id);
                            self.selected_cost_breakdown =
                                explain_cost(row.item_id, &self.market_prices);
                            self.solved_macro = None;
                        }
                        ui.label(row.category_name.as_deref().unwrap_or("Unknown"));
                        ui.label(format_gil(row.sell_price));
                        ui.label(format_gil(row.craft_cost));
                        ui.label(format_gil_signed(row.profit));
                        ui.label(row.recent_sales_2d.to_string());
                        ui.label(format!("{:.2}/day", row.daily_velocity));
                        ui.end_row();
                    }
                });
        });
    }

    fn draw_gather_table(&mut self, ui: &mut egui::Ui) {
        ui.heading("Gathering / collecting ranking");
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("gather_grid")
                .striped(true)
                .num_columns(7)
                .show(ui, |ui| {
                    ui.strong("Item");
                    ui.strong("Type");
                    ui.strong("Sell");
                    ui.strong("NPC");
                    ui.strong("Profit");
                    ui.strong("2d sales");
                    ui.strong("Velocity");
                    ui.end_row();

                    for row in self.filtered_gather_rows().into_iter().take(500) {
                        ui.label(&row.item_name);
                        ui.label(row.category_name.as_deref().unwrap_or("Unknown"));
                        ui.label(format_gil(row.sell_price));
                        ui.label(
                            row.vendor_price
                                .map(format_gil)
                                .unwrap_or_else(|| "No".to_string()),
                        );
                        ui.label(format_gil_signed(row.profit));
                        ui.label(row.recent_sales_2d.to_string());
                        ui.label(format!("{:.2}/day", row.daily_velocity));
                        ui.end_row();
                    }
                });
        });
    }

    fn filtered_rows(&self) -> Vec<ProfitRow> {
        self.rows
            .iter()
            .filter(|row| {
                self.applied_item_filter
                    .is_none_or(|item_id| row.item_id == item_id)
            })
            .filter(|row| !self.applied_only_profitable || row.profit > 0)
            .filter(|row| !self.applied_only_recently_sold || row.recent_sales_2d > 4)
            .filter(|row| {
                self.applied_category_filter.is_none()
                    || row
                        .search_category_id
                        .is_some_and(|id| Some(id) == self.applied_category_filter)
            })
            .cloned()
            .collect()
    }

    fn filtered_gather_rows(&self) -> Vec<GatherRow> {
        self.gather_rows
            .iter()
            .filter(|row| {
                self.applied_item_filter
                    .is_none_or(|item_id| row.item_id == item_id)
            })
            .filter(|row| !self.applied_only_profitable || row.profit > 0)
            .filter(|row| !self.applied_only_recently_sold || row.recent_sales_2d > 4)
            .filter(|row| {
                self.applied_category_filter.is_none()
                    || row
                        .search_category_id
                        .is_some_and(|id| Some(id) == self.applied_category_filter)
            })
            .cloned()
            .collect()
    }

    fn draw_selected_recipe(&mut self, ui: &mut egui::Ui) {
        ui.heading("Selected craft");
        ui.separator();
        let Some(item_id) = self.selected_item_id else {
            ui.label("Select a ranked recipe to inspect cost, craft tree, and macros.");
            return;
        };

        let item_name = item_name(item_id);
        ui.label(egui::RichText::new(item_name).strong());
        if let Some(row) = self.rows.iter().find(|row| row.item_id == item_id) {
            ui.horizontal(|ui| {
                ui.label(format!("Sell: {}", format_gil(row.sell_price)));
                ui.label(format!("Cost: {}", format_gil(row.craft_cost)));
                ui.label(format!("Profit: {}", format_gil_signed(row.profit)));
            });
        }

        ui.separator();
        ui.label("Cost tree");
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .show(ui, |ui| {
                for node in &self.selected_cost_breakdown {
                    ui.horizontal(|ui| {
                        ui.add_space((node.depth * 16) as f32);
                        ui.label(format!(
                            "{} x{} - {} via {}",
                            node.name,
                            node.amount,
                            format_gil(node.unit_cost),
                            node.source
                        ));
                    });
                }
            });

        ui.separator();
        ui.label("Macro");
        if let Some(solved) = &self.solved_macro {
            ui.label(format!(
                "{} craft macros, {} total actions",
                solved.plans.len(),
                solved.total_actions()
            ));
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    for plan in &solved.plans {
                        ui.separator();
                        ui.label(egui::RichText::new(&plan.item_name).strong());
                        ui.label(format!(
                            "Recipe #{}, {} steps",
                            plan.recipe_id,
                            plan.actions.len()
                        ));
                        for (idx, macro_text) in plan.macro_blocks.iter().enumerate() {
                            ui.label(format!("Block {}", idx + 1));
                            let mut text = macro_text.clone();
                            ui.add(
                                egui::TextEdit::multiline(&mut text)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_rows(15),
                            );
                        }
                    }
                });
        } else {
            ui.label("No macro solved yet.");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppMode {
    Craft,
    Gather,
}

#[derive(Debug)]
enum WorkMessage<T> {
    Progress(String),
    Done(Result<T>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ItemCategory {
    id: u32,
    name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ItemMetadata {
    search_category_id: Option<u32>,
    search_category_name: Option<String>,
    ui_category_name: Option<String>,
    vendor_price: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct XivApiCache {
    categories: Vec<ItemCategory>,
    item_metadata: HashMap<u32, ItemMetadata>,
    vendor_prices: HashMap<u32, u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct XivApiCacheManifest {
    schema_version: u32,
    relevant_item_count: usize,
    relevant_item_hash: u64,
}

#[derive(Debug, Clone)]
struct MarketPrice {
    min_listing: u32,
    average_sale: Option<f64>,
    daily_velocity: f64,
    recent_sales_2d: usize,
    metadata: ItemMetadata,
}

#[derive(Debug, Clone)]
struct ProfitRow {
    recipe_id: u32,
    item_id: u32,
    item_name: String,
    category_name: Option<String>,
    search_category_id: Option<u32>,
    sell_price: u32,
    craft_cost: u32,
    profit: i64,
    daily_velocity: f64,
    recent_sales_2d: usize,
}

#[derive(Debug, Clone)]
struct GatherRow {
    item_id: u32,
    item_name: String,
    category_name: Option<String>,
    search_category_id: Option<u32>,
    sell_price: u32,
    vendor_price: Option<u32>,
    profit: i64,
    daily_velocity: f64,
    recent_sales_2d: usize,
}

#[derive(Debug)]
struct MarketSnapshot {
    prices: HashMap<u32, MarketPrice>,
    categories: Vec<ItemCategory>,
}

#[derive(Debug, Clone)]
struct CostNode {
    depth: usize,
    name: String,
    amount: u32,
    unit_cost: u32,
    source: String,
}

#[derive(Debug)]
struct SolvedMacro {
    plans: Vec<MacroPlan>,
}

impl SolvedMacro {
    fn total_actions(&self) -> usize {
        self.plans.iter().map(|plan| plan.actions.len()).sum()
    }
}

#[derive(Debug)]
struct MacroPlan {
    recipe_id: u32,
    item_name: String,
    actions: Vec<Action>,
    macro_blocks: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AggregatedResponse {
    results: Vec<AggregatedResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AggregatedResult {
    item_id: u32,
    nq: AggregatedBucket,
    hq: AggregatedBucket,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AggregatedBucket {
    min_listing: PriceScopes,
    average_sale_price: PriceScopes,
    daily_sale_velocity: QuantityScopes,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriceScopes {
    world: Option<PriceEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuantityScopes {
    world: Option<QuantityEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriceEntry {
    price: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuantityEntry {
    quantity: f64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HistoryResponse {
    Single(HistoryItem),
    Multi { items: Vec<HistoryItem> },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryItem {
    #[serde(rename = "itemID")]
    item_id: u32,
    entries: Vec<HistoryEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryEntry {
    timestamp: i64,
}

#[derive(Debug, Deserialize)]
struct XivSheetRows {
    rows: Vec<XivRow>,
}

#[derive(Debug, Deserialize)]
struct XivRow {
    row_id: u32,
    fields: Value,
}

fn fetch_market_snapshot(progress: &dyn Fn(String)) -> Result<MarketSnapshot> {
    progress("Building HTTP client for Universalis and XIVAPI.".to_string());
    let client = reqwest::blocking::Client::builder()
        .user_agent("FortunaProfitCrafter/0.1")
        .build()?;
    let ids = relevant_market_item_ids();
    progress(format!("Checking {} possible market item IDs.", ids.len()));
    let mut prices = fetch_universalis_market_prices(&client, &ids, progress)?;
    progress(format!("Loaded current prices for {} market items.", prices.len()));
    let market_item_ids = sorted_market_item_ids(&prices);
    let xivapi_cache = load_or_update_xivapi_library(&market_item_ids, progress)?;
    let recent_sales = fetch_recent_sales_2d(&client, progress)?;

    progress("Merging item categories, vendor prices, and sale counts.".to_string());
    for (item_id, price) in &mut prices {
        price.metadata = xivapi_cache
            .item_metadata
            .get(item_id)
            .cloned()
            .unwrap_or_default();
        if let Some(vendor_price) = xivapi_cache.vendor_prices.get(item_id).copied() {
            price.metadata.vendor_price = Some(vendor_price);
        }
    }

    for (item_id, sale_count) in recent_sales {
        if let Some(price) = prices.get_mut(&item_id) {
            price.recent_sales_2d = sale_count;
        }
    }

    Ok(MarketSnapshot {
        prices,
        categories: xivapi_cache.categories,
    })
}

fn market_price_from_result(result: &AggregatedResult) -> Option<MarketPrice> {
    let nq_min = result
        .nq
        .min_listing
        .world
        .as_ref()
        .map(|entry| entry.price);
    let hq_min = result
        .hq
        .min_listing
        .world
        .as_ref()
        .map(|entry| entry.price);
    let min_listing = [nq_min, hq_min]
        .into_iter()
        .flatten()
        .filter(|price| *price > 0.0)
        .min_by(|a, b| a.total_cmp(b))? as u32;
    let average_sale = [
        result.nq.average_sale_price.world.as_ref(),
        result.hq.average_sale_price.world.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|entry| entry.price)
    .filter(|price| *price > 0.0)
    .max_by(|a, b| a.total_cmp(b));
    let daily_velocity = [
        result.nq.daily_sale_velocity.world.as_ref(),
        result.hq.daily_sale_velocity.world.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|entry| entry.quantity)
    .sum();

    Some(MarketPrice {
        min_listing,
        average_sale,
        daily_velocity,
        recent_sales_2d: 0,
        metadata: ItemMetadata::default(),
    })
}

fn fetch_universalis_market_prices(
    client: &reqwest::blocking::Client,
    ids: &[u32],
    progress: &dyn Fn(String),
) -> Result<HashMap<u32, MarketPrice>> {
    let mut prices = HashMap::new();
    let market_chunks = ids.chunks(MAX_MARKET_IDS_PER_REQUEST).len();
    for (index, chunk) in ids.chunks(MAX_MARKET_IDS_PER_REQUEST).enumerate() {
        progress(format!(
            "Fetching current market prices ({}/{market_chunks}).",
            index + 1
        ));
        let joined = chunk
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let url = format!("https://universalis.app/api/v2/aggregated/{WORLD}/{joined}");
        let response = client.get(url).send()?.error_for_status()?;
        let data: AggregatedResponse = response.json()?;
        for result in data.results {
            if let Some(price) = market_price_from_result(&result) {
                prices.insert(result.item_id, price);
            }
        }
    }
    Ok(prices)
}

fn sorted_market_item_ids(prices: &HashMap<u32, MarketPrice>) -> Vec<u32> {
    let mut item_ids = prices.keys().copied().collect::<Vec<_>>();
    item_ids.sort_unstable();
    item_ids
}

fn load_or_update_xivapi_library(
    item_ids: &[u32],
    progress: &dyn Fn(String),
) -> Result<XivApiCache> {
    let expected_manifest = xivapi_cache_manifest(item_ids);
    match load_xivapi_library() {
        Ok((cache, manifest)) if cache_is_usable(&cache, &manifest, &expected_manifest) => {
            progress(format!(
                "Using cached XIVAPI data: {} item types, {} items, {} vendor prices.",
                cache.categories.len(),
                cache.item_metadata.len(),
                cache.vendor_prices.len()
            ));
            Ok(cache)
        }
        _ => {
            progress(
                "Local XIVAPI cache is missing, stale, or for an older item set; updating it now."
                    .to_string(),
            );
            update_xivapi_library_with_item_ids(item_ids.to_vec(), progress)
        }
    }
}

fn cache_is_usable(
    cache: &XivApiCache,
    manifest: &XivApiCacheManifest,
    expected_manifest: &XivApiCacheManifest,
) -> bool {
    !cache.categories.is_empty()
        && !cache.item_metadata.is_empty()
        && !cache.vendor_prices.is_empty()
        && manifest.schema_version == expected_manifest.schema_version
        && manifest.relevant_item_count == expected_manifest.relevant_item_count
        && manifest.relevant_item_hash == expected_manifest.relevant_item_hash
}

fn load_xivapi_library() -> Result<(XivApiCache, XivApiCacheManifest)> {
    let dir = xivapi_cache_dir();
    let manifest = read_json_file(&dir.join("cache_manifest.json"))?;
    let categories = read_json_file(&dir.join("item_categories.json"))?;
    let item_metadata = read_json_file(&dir.join("item_metadata.json"))?;
    let vendor_prices = read_json_file(&dir.join("vendor_prices.json"))?;
    Ok((
        XivApiCache {
            categories,
            item_metadata,
            vendor_prices,
        },
        manifest,
    ))
}

fn update_xivapi_library(progress: &dyn Fn(String)) -> Result<XivApiCache> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("FortunaProfitCrafter/0.1")
        .build()?;
    let ids = relevant_market_item_ids();
    progress(format!(
        "Checking Universalis for {} possible market item IDs before refreshing XIVAPI.",
        ids.len()
    ));
    let prices = fetch_universalis_market_prices(&client, &ids, progress)?;
    let item_ids = sorted_market_item_ids(&prices);
    progress(format!(
        "Universalis returned market data for {} items; refreshing XIVAPI for only those.",
        item_ids.len()
    ));
    update_xivapi_library_with_client(&client, item_ids, progress)
}

fn update_xivapi_library_with_item_ids(
    item_ids: Vec<u32>,
    progress: &dyn Fn(String),
) -> Result<XivApiCache> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("FortunaProfitCrafter/0.1")
        .build()?;
    update_xivapi_library_with_client(&client, item_ids, progress)
}

fn update_xivapi_library_with_client(
    client: &reqwest::blocking::Client,
    item_ids: Vec<u32>,
    progress: &dyn Fn(String),
) -> Result<XivApiCache> {
    progress(format!(
        "Fetching XIVAPI metadata for {} Universalis-visible item IDs.",
        item_ids.len()
    ));
    let categories = fetch_xivapi_item_categories(&client, progress)
        .context("failed to fetch XIVAPI item categories")?;
    let item_metadata = fetch_xivapi_item_metadata(&client, &item_ids, progress)
        .context("failed to fetch XIVAPI item metadata")?;
    let vendor_prices = vendor_prices_from_item_metadata(&item_metadata);
    progress(format!(
        "Derived relevant NPC vendor prices for {} items from item metadata.",
        vendor_prices.len()
    ));
    let cache = XivApiCache {
        categories,
        item_metadata,
        vendor_prices,
    };
    progress("Saving XIVAPI cache files.".to_string());
    save_xivapi_library(&cache, &xivapi_cache_manifest(&item_ids))?;
    Ok(cache)
}

fn save_xivapi_library(cache: &XivApiCache, manifest: &XivApiCacheManifest) -> Result<()> {
    let dir = xivapi_cache_dir();
    fs::create_dir_all(&dir)?;
    write_json_file(&dir.join("cache_manifest.json"), manifest)?;
    write_json_file(&dir.join("item_categories.json"), &cache.categories)?;
    write_json_file(&dir.join("item_metadata.json"), &cache.item_metadata)?;
    write_json_file(&dir.join("vendor_prices.json"), &cache.vendor_prices)?;
    Ok(())
}

fn xivapi_cache_manifest(item_ids: &[u32]) -> XivApiCacheManifest {
    let mut hasher = DefaultHasher::new();
    XIVAPI_CACHE_SCHEMA_VERSION.hash(&mut hasher);
    item_ids.hash(&mut hasher);
    XivApiCacheManifest {
        schema_version: XIVAPI_CACHE_SCHEMA_VERSION,
        relevant_item_count: item_ids.len(),
        relevant_item_hash: hasher.finish(),
    }
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let data =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let data = serde_json::to_string_pretty(value)?;
    fs::write(path, data).with_context(|| format!("failed to write {}", path.display()))
}

fn xivapi_cache_dir() -> PathBuf {
    workspace_root().join("xivapi-cache")
}

fn fetch_xivapi_item_categories(
    client: &reqwest::blocking::Client,
    progress: &dyn Fn(String),
) -> Result<Vec<ItemCategory>> {
    progress("Fetching XIVAPI item search categories.".to_string());
    let url = "https://v2.xivapi.com/api/sheet/ItemSearchCategory?limit=200&fields=Name";
    let data: XivSheetRows = client.get(url).send()?.error_for_status()?.json()?;
    let mut categories = data
        .rows
        .into_iter()
        .filter_map(|row| {
            let name = row.fields.get("Name")?.as_str()?.trim();
            if name.is_empty() {
                None
            } else {
                Some(ItemCategory {
                    id: row.row_id,
                    name: name.to_string(),
                })
            }
        })
        .collect::<Vec<_>>();
    categories.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    progress(format!("Loaded {} XIVAPI item types.", categories.len()));
    Ok(categories)
}

fn fetch_xivapi_item_metadata(
    client: &reqwest::blocking::Client,
    item_ids: &[u32],
    progress: &dyn Fn(String),
) -> Result<HashMap<u32, ItemMetadata>> {
    let mut metadata = HashMap::new();
    let chunks = item_ids.chunks(MAX_MARKET_IDS_PER_REQUEST).len();
    for (index, chunk) in item_ids.chunks(MAX_MARKET_IDS_PER_REQUEST).enumerate() {
        progress(format!(
            "Fetching XIVAPI item metadata ({}/{chunks}).",
            index + 1
        ));
        let rows = chunk
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let url = format!(
            "https://v2.xivapi.com/api/sheet/Item?rows={rows}&fields=ItemSearchCategory.Name,ItemSearchCategory@as(raw),ItemUICategory.Name,PriceMid"
        );
        let data: XivSheetRows = client.get(url).send()?.error_for_status()?.json()?;
        for row in data.rows {
            let search_category_id = row
                .fields
                .get("ItemSearchCategory@as(raw)")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value != 0);
            let search_category_name = relationship_name(&row.fields, "ItemSearchCategory");
            let ui_category_name = relationship_name(&row.fields, "ItemUICategory");
            let vendor_price = row
                .fields
                .get("PriceMid")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0);
            metadata.insert(
                row.row_id,
                ItemMetadata {
                    search_category_id,
                    search_category_name,
                    ui_category_name,
                    vendor_price,
                },
            );
        }
    }
    progress(format!("Loaded metadata for {} items.", metadata.len()));
    Ok(metadata)
}

fn relationship_name(fields: &Value, key: &str) -> Option<String> {
    fields
        .get(key)?
        .get("fields")?
        .get("Name")?
        .as_str()
        .map(ToOwned::to_owned)
        .filter(|name| !name.trim().is_empty())
}

fn vendor_prices_from_item_metadata(metadata: &HashMap<u32, ItemMetadata>) -> HashMap<u32, u32> {
    metadata
        .iter()
        .filter_map(|(&item_id, metadata)| metadata.vendor_price.map(|price| (item_id, price)))
        .collect()
}

fn fetch_recent_sales_2d(
    client: &reqwest::blocking::Client,
    progress: &dyn Fn(String),
) -> Result<HashMap<u32, usize>> {
    let ids = recipe_output_item_ids();
    let mut sales = HashMap::new();

    let chunks = ids.chunks(MAX_MARKET_IDS_PER_REQUEST).len();
    for (index, chunk) in ids.chunks(MAX_MARKET_IDS_PER_REQUEST).enumerate() {
        progress(format!(
            "Fetching recent sale history ({}/{chunks}).",
            index + 1
        ));
        let joined = chunk
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let url = format!(
            "https://universalis.app/api/history/{WORLD}/{joined}?entries=100&entriesWithin=172800"
        );
        let response = client.get(url).send()?.error_for_status()?;
        match response.json::<HistoryResponse>()? {
            HistoryResponse::Single(item) => {
                sales.insert(item.item_id, recent_sale_count(&item.entries));
            }
            HistoryResponse::Multi { items } => {
                for item in items {
                    sales.insert(item.item_id, recent_sale_count(&item.entries));
                }
            }
        }
    }

    progress(format!("Loaded recent sale counts for {} recipes.", sales.len()));
    Ok(sales)
}

fn recent_sale_count(entries: &[HistoryEntry]) -> usize {
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64 - 172_800)
        .unwrap_or(0);
    entries
        .iter()
        .filter(|entry| entry.timestamp >= cutoff)
        .count()
}

fn relevant_market_item_ids() -> Vec<u32> {
    let mut ids = HashSet::new();
    for recipe in RECIPES.values() {
        ids.insert(recipe.item_id);
        for ingredient in recipe.ingredients {
            if ingredient.item_id != 0 {
                ids.insert(ingredient.item_id);
            }
        }
    }
    let mut ids = ids.into_iter().collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

fn recipe_output_item_ids() -> Vec<u32> {
    let mut ids = RECIPES
        .values()
        .map(|recipe| recipe.item_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

fn rank_profitability(market_prices: &HashMap<u32, MarketPrice>) -> Vec<ProfitRow> {
    let recipe_by_item = recipes_by_item();
    let mut memo = HashMap::new();
    let mut rows = RECIPES
        .entries()
        .filter_map(|(recipe_id, recipe)| {
            let sell = market_prices.get(&recipe.item_id)?;
            let craft_cost = craft_only_cost(
                recipe.item_id,
                market_prices,
                &recipe_by_item,
                &mut memo,
                &mut HashSet::new(),
            )?;
            let sale_basis = sell
                .average_sale
                .map(|value| value.round() as u32)
                .unwrap_or(sell.min_listing);
            Some(ProfitRow {
                recipe_id,
                item_id: recipe.item_id,
                item_name: item_name(recipe.item_id),
                category_name: sell
                    .metadata
                    .search_category_name
                    .clone()
                    .or_else(|| sell.metadata.ui_category_name.clone()),
                search_category_id: sell.metadata.search_category_id,
                sell_price: sale_basis,
                craft_cost,
                profit: i64::from(sale_basis) - i64::from(craft_cost),
                daily_velocity: sell.daily_velocity,
                recent_sales_2d: sell.recent_sales_2d,
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.profit
            .cmp(&a.profit)
            .then_with(|| b.recent_sales_2d.cmp(&a.recent_sales_2d))
            .then_with(|| b.daily_velocity.total_cmp(&a.daily_velocity))
    });
    rows
}

fn rank_gathering(market_prices: &HashMap<u32, MarketPrice>) -> Vec<GatherRow> {
    let crafted_items = RECIPES
        .values()
        .map(|recipe| recipe.item_id)
        .collect::<HashSet<_>>();
    let mut rows = market_prices
        .iter()
        .filter(|(item_id, _)| !crafted_items.contains(item_id))
        .map(|(&item_id, price)| {
            let sale_basis = price
                .average_sale
                .map(|value| value.round() as u32)
                .unwrap_or(price.min_listing);
            let acquisition_cost = price.metadata.vendor_price.unwrap_or(0);
            GatherRow {
                item_id,
                item_name: item_name(item_id),
                category_name: price
                    .metadata
                    .search_category_name
                    .clone()
                    .or_else(|| price.metadata.ui_category_name.clone()),
                search_category_id: price.metadata.search_category_id,
                sell_price: sale_basis,
                vendor_price: price.metadata.vendor_price,
                profit: i64::from(sale_basis) - i64::from(acquisition_cost),
                daily_velocity: price.daily_velocity,
                recent_sales_2d: price.recent_sales_2d,
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.profit
            .cmp(&a.profit)
            .then_with(|| b.recent_sales_2d.cmp(&a.recent_sales_2d))
            .then_with(|| b.daily_velocity.total_cmp(&a.daily_velocity))
    });
    rows
}

fn recipes_by_item() -> HashMap<u32, Recipe> {
    RECIPES
        .values()
        .map(|recipe| (recipe.item_id, *recipe))
        .collect()
}

fn craft_only_cost(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    recipes_by_item: &HashMap<u32, Recipe>,
    memo: &mut HashMap<u32, Option<u32>>,
    visiting: &mut HashSet<u32>,
) -> Option<u32> {
    if let Some(cost) = memo.get(&item_id) {
        return *cost;
    }
    if !visiting.insert(item_id) {
        return buy_cost(item_id, market_prices);
    }

    let recipe = recipes_by_item.get(&item_id);
    let craft_cost = recipe
        .map(|recipe| {
            recipe
                .ingredients
                .iter()
                .filter(|ingredient| ingredient.item_id != 0 && ingredient.amount != 0)
                .map(|ingredient| {
                    let unit = cheapest_cost(
                        ingredient.item_id,
                        market_prices,
                        recipes_by_item,
                        memo,
                        visiting,
                    )?;
                    Some(unit.saturating_mul(ingredient.amount))
                })
                .sum::<Option<u32>>()
        })
        .flatten();

    visiting.remove(&item_id);
    memo.insert(item_id, craft_cost);
    craft_cost
}

fn cheapest_cost(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    recipes_by_item: &HashMap<u32, Recipe>,
    memo: &mut HashMap<u32, Option<u32>>,
    visiting: &mut HashSet<u32>,
) -> Option<u32> {
    let buy = buy_cost(item_id, market_prices);
    let craft = craft_only_cost(item_id, market_prices, recipes_by_item, memo, visiting);
    match (buy, craft) {
        (Some(buy), Some(craft)) => Some(buy.min(craft)),
        (Some(buy), None) => Some(buy),
        (None, Some(craft)) => Some(craft),
        (None, None) => None,
    }
}

fn explain_cost(item_id: u32, market_prices: &HashMap<u32, MarketPrice>) -> Vec<CostNode> {
    let recipes_by_item = recipes_by_item();
    let mut memo = HashMap::new();
    let mut nodes = Vec::new();
    explain_cost_inner(
        item_id,
        1,
        0,
        market_prices,
        &recipes_by_item,
        &mut memo,
        &mut HashSet::new(),
        &mut nodes,
    );
    nodes
}

fn explain_cost_inner(
    item_id: u32,
    amount: u32,
    depth: usize,
    market_prices: &HashMap<u32, MarketPrice>,
    recipes_by_item: &HashMap<u32, Recipe>,
    memo: &mut HashMap<u32, Option<u32>>,
    visiting: &mut HashSet<u32>,
    nodes: &mut Vec<CostNode>,
) {
    let buy = buy_cost(item_id, market_prices);
    let craft = craft_only_cost(item_id, market_prices, recipes_by_item, memo, visiting);
    let buy_source = market_prices
        .get(&item_id)
        .and_then(|price| match price.metadata.vendor_price {
            Some(vendor) if vendor <= price.min_listing => Some("NPC vendor"),
            _ => Some("market"),
        })
        .unwrap_or("market");
    let (unit_cost, source) = match (buy, craft) {
        (Some(buy), Some(craft)) if craft < buy => (craft, "craft".to_string()),
        (Some(buy), Some(craft)) => (buy, format!("{buy_source}, craft {}", format_gil(craft))),
        (Some(buy), None) => (buy, buy_source.to_string()),
        (None, Some(craft)) => (craft, "craft".to_string()),
        (None, None) => (0, "unknown".to_string()),
    };
    nodes.push(CostNode {
        depth,
        name: item_name(item_id),
        amount,
        unit_cost,
        source: source.clone(),
    });

    if source.starts_with("craft") {
        if let Some(recipe) = recipes_by_item.get(&item_id) {
            for ingredient in recipe.ingredients {
                if ingredient.item_id != 0 && ingredient.amount != 0 {
                    explain_cost_inner(
                        ingredient.item_id,
                        ingredient.amount,
                        depth + 1,
                        market_prices,
                        recipes_by_item,
                        memo,
                        visiting,
                        nodes,
                    );
                }
            }
        }
    }
}

fn buy_cost(item_id: u32, market_prices: &HashMap<u32, MarketPrice>) -> Option<u32> {
    let price = market_prices.get(&item_id)?;
    Some(match price.metadata.vendor_price {
        Some(vendor_price) => price.min_listing.min(vendor_price),
        None => price.min_listing,
    })
}

fn solve_macro_plan(
    final_recipe_id: u32,
    final_recipe: Recipe,
    crafter_stats: CrafterStats,
    market_prices: HashMap<u32, MarketPrice>,
    progress: &dyn Fn(String),
) -> Result<SolvedMacro> {
    let recipes_by_item_id = RECIPES
        .entries()
        .map(|(recipe_id, recipe)| (recipe.item_id, (recipe_id, *recipe)))
        .collect::<HashMap<_, _>>();
    let cost_recipes = recipes_by_item_id
        .iter()
        .map(|(&item_id, &(_, recipe))| (item_id, recipe))
        .collect::<HashMap<_, _>>();
    let mut craft_plan = Vec::new();
    let mut seen = HashSet::new();
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();

    collect_crafted_dependencies(
        final_recipe.item_id,
        &market_prices,
        &recipes_by_item_id,
        &cost_recipes,
        &mut memo,
        &mut visiting,
        &mut seen,
        &mut craft_plan,
    );
    if seen.insert(final_recipe.item_id) {
        craft_plan.push((final_recipe_id, final_recipe));
    }

    progress(format!(
        "Solving {} craft macro(s) with Raphael.",
        craft_plan.len()
    ));
    let mut plans = Vec::new();
    let total = craft_plan.len();
    for (index, (recipe_id, recipe)) in craft_plan.into_iter().enumerate() {
        progress(format!(
            "Solving macro {}/{}: {}.",
            index + 1,
            total,
            item_name(recipe.item_id)
        ));
        plans.push(solve_one_macro(recipe_id, recipe, crafter_stats)?);
    }
    Ok(SolvedMacro { plans })
}

fn collect_crafted_dependencies(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    recipes_by_item_id: &HashMap<u32, (u32, Recipe)>,
    cost_recipes: &HashMap<u32, Recipe>,
    memo: &mut HashMap<u32, Option<u32>>,
    visiting: &mut HashSet<u32>,
    seen: &mut HashSet<u32>,
    craft_plan: &mut Vec<(u32, Recipe)>,
) {
    let Some(&(_recipe_id, recipe)) = recipes_by_item_id.get(&item_id) else {
        return;
    };

    for ingredient in recipe.ingredients {
        if ingredient.item_id == 0 || ingredient.amount == 0 {
            continue;
        }
        let buy = market_prices
            .get(&ingredient.item_id)
            .map(|price| price.min_listing);
        let craft = craft_only_cost(
            ingredient.item_id,
            market_prices,
            cost_recipes,
            memo,
            visiting,
        );
        let should_craft = matches!((buy, craft), (Some(buy), Some(craft)) if craft < buy)
            || matches!((buy, craft), (None, Some(_)));
        if should_craft {
            collect_crafted_dependencies(
                ingredient.item_id,
                market_prices,
                recipes_by_item_id,
                cost_recipes,
                memo,
                visiting,
                seen,
                craft_plan,
            );
            if seen.insert(ingredient.item_id) {
                if let Some(&(dependency_recipe_id, dependency_recipe)) =
                    recipes_by_item_id.get(&ingredient.item_id)
                {
                    craft_plan.push((dependency_recipe_id, dependency_recipe));
                }
            }
        }
    }
}

fn solve_one_macro(
    recipe_id: u32,
    recipe: Recipe,
    crafter_stats: CrafterStats,
) -> Result<MacroPlan> {
    let simulator_settings = get_game_settings(recipe, None, crafter_stats, None, None);
    let settings = SolverSettings {
        simulator_settings,
        allow_non_max_quality_solutions: true,
    };
    let mut solver = MacroSolver::new(
        settings,
        Box::new(|_| {}),
        Box::new(|_| {}),
        AtomicFlag::new(),
    );
    let actions = solver.solve().map_err(|err| anyhow!("{err:?}"))?;
    let macro_blocks = create_macro_blocks(&actions);
    Ok(MacroPlan {
        recipe_id,
        item_name: item_name(recipe.item_id),
        actions,
        macro_blocks,
    })
}

fn create_macro_blocks(actions: &[Action]) -> Vec<String> {
    actions
        .chunks(15)
        .map(|chunk| {
            chunk
                .iter()
                .map(|action| {
                    format!(
                        "/ac \"{}\" <wait.{}>",
                        raphael_data::macro_name(*action, Locale::EN),
                        action.time_cost()
                    )
                })
                .collect::<Vec<_>>()
                .join("\r\n")
        })
        .collect()
}

fn ensure_repo(path: &Path, url: &str) -> Result<String> {
    if !path.join(".git").exists() {
        log_terminal(&format!("Cloning {url} into {}.", path.display()));
        let output = Command::new("git")
            .arg("clone")
            .arg(url)
            .arg(path)
            .output()
            .with_context(|| format!("failed to clone {url} into {}", path.display()))?;
        if !output.status.success() {
            return Err(anyhow!(
                "{}",
                String::from_utf8_lossy(&output.stderr).trim().to_string()
            ));
        }

        let git_dir = path.join(".git");
        if git_dir.exists() {
            log_terminal(&format!(
                "Removing nested git metadata from {}.",
                git_dir.display()
            ));
            fs::remove_dir_all(&git_dir)
                .with_context(|| format!("failed to remove {}", git_dir.display()))?;
        }

        return Ok("cloned and detached".to_string());
    }

    log_terminal(&format!("Repository already exists at {}.", path.display()));
    Ok("already present".to_string())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("Fortuna should live inside Fortuna")
        .to_path_buf()
}

fn report_progress<T>(tx: &Sender<WorkMessage<T>>, message: impl Into<String>) {
    let message = message.into();
    log_terminal(&message);
    let _ = tx.send(WorkMessage::Progress(message));
}

fn log_terminal(message: &str) {
    println!("[Fortuna] {message}");
}

fn item_name(item_id: u32) -> String {
    get_raw_item_name(item_id, Locale::EN)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("Item #{item_id}"))
}

fn format_gil(value: u32) -> String {
    format!("{value} gil")
}

fn format_gil_signed(value: i64) -> String {
    if value >= 0 {
        format!("+{value} gil")
    } else {
        format!("{value} gil")
    }
}
