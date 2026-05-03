use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use anyhow::{Context, Result, anyhow};
use eframe::egui;
use raphael_data::{
    CrafterStats, Locale, RECIPES, Recipe, get_game_settings, get_initial_quality,
    get_raw_item_name,
};
use raphael_sim::{Action, SimulationState};
use raphael_solver::{AtomicFlag, MacroSolver, SolverSettings};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const WORLD: &str = "Phoenix";
const DATA_CENTER: &str = "Light";
const MAX_MARKET_IDS_PER_REQUEST: usize = 100;
const XIVAPI_CACHE_SCHEMA_VERSION: u32 = 4;

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
    item_metadata: HashMap<u32, ItemMetadata>,
    recipe_materials: HashMap<u32, Vec<RecipeMaterial>>,
    pending_item_filter: Option<u32>,
    applied_item_filter: Option<u32>,
    pending_category_filter: Option<u32>,
    applied_category_filter: Option<u32>,
    selected_item_id: Option<u32>,
    selected_recipe_id: Option<u32>,
    selected_cost_breakdown: Vec<CostNode>,
    selected_hq_ingredients: [u8; 6],
    item_search: String,
    item_filter_search: String,
    pending_only_profitable: bool,
    applied_only_profitable: bool,
    pending_only_recently_sold: bool,
    applied_only_recently_sold: bool,
    sale_value_mode: SaleValueMode,
    ranking_mode: RankingMode,
    status: String,
    activity_log: VecDeque<String>,
    market_rx: Option<Receiver<WorkMessage<MarketSnapshot>>>,
    solve_rx: Option<Receiver<WorkMessage<SolvedMacro>>>,
    update_rx: Option<Receiver<WorkMessage<String>>>,
    solved_macro: Option<SolvedMacro>,
    crafter_stats: CrafterStats,
}

impl Default for ProfitCrafterApp {
    fn default() -> Self {
        let (item_categories, item_metadata, recipe_materials, mut status) =
            match load_xivapi_library() {
                Ok((cache, _)) => (
                    cache.categories,
                    cache.item_metadata,
                    cache.recipe_materials,
                    "Loaded stored XIVAPI item types and recipe materials.".to_string(),
                ),
                Err(_) => (
                    Vec::new(),
                    HashMap::new(),
                    HashMap::new(),
                    "No stored XIVAPI item types or recipe materials loaded.".to_string(),
                ),
            };
        let (market_prices, rows, gather_rows) = match load_market_snapshot() {
            Ok(snapshot) => {
                let rows = rank_profitability(
                    &snapshot.prices,
                    &recipe_materials,
                    SaleValueMode::AverageLastFour,
                    RankingMode::Profit,
                );
                let gather_rows = rank_gathering(
                    &snapshot.prices,
                    SaleValueMode::AverageLastFour,
                    RankingMode::Profit,
                );
                status = format!(
                    "Loaded stored market snapshot with {} prices. Refresh market when you want current data.",
                    snapshot.prices.len()
                );
                (snapshot.prices, rows, gather_rows)
            }
            Err(_) => (HashMap::new(), Vec::new(), Vec::new()),
        };

        let mut activity_log = VecDeque::new();
        activity_log.push_back(status.clone());

        Self {
            mode: AppMode::Craft,
            market_prices,
            rows,
            gather_rows,
            item_categories,
            item_metadata,
            recipe_materials,
            pending_item_filter: None,
            applied_item_filter: None,
            pending_category_filter: None,
            applied_category_filter: None,
            selected_item_id: None,
            selected_recipe_id: None,
            selected_cost_breakdown: Vec::new(),
            selected_hq_ingredients: [0; 6],
            item_search: String::new(),
            item_filter_search: String::new(),
            pending_only_profitable: false,
            applied_only_profitable: false,
            pending_only_recently_sold: false,
            applied_only_recently_sold: false,
            sale_value_mode: SaleValueMode::AverageLastFour,
            ranking_mode: RankingMode::Profit,
            status,
            activity_log,
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
                ui.selectable_value(&mut self.mode, AppMode::Craft, "Craft");
                ui.selectable_value(&mut self.mode, AppMode::Gather, "Gather");
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

        egui::CentralPanel::default().show_inside(ui, |ui| {
            let available_height = ui.available_height();
            let main_height = if available_height > 260.0 {
                (available_height - 150.0).max(260.0)
            } else {
                available_height * 0.72
            };
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), main_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.columns(3, |columns| {
                        egui::ScrollArea::vertical()
                            .id_salt("filters_scroll")
                            .auto_shrink([false, false])
                            .show(&mut columns[0], |ui| {
                                ui.heading("Filters");
                                self.draw_filter_controls(ui);

                                ui.separator();
                                ui.label(format!("Market prices: {}", self.market_prices.len()));
                                ui.label(format!("Ranked recipes: {}", self.rows.len()));
                                ui.label(format!("Gathering rows: {}", self.gather_rows.len()));
                            });

                        self.draw_crafter_inputs(&mut columns[1]);
                        columns[1].separator();

                        match self.mode {
                            AppMode::Craft => self.draw_profit_table(&mut columns[1]),
                            AppMode::Gather => self.draw_gather_table(&mut columns[1]),
                        }

                        egui::ScrollArea::vertical()
                            .id_salt("selected_recipe_scroll")
                            .auto_shrink([false, false])
                            .show(&mut columns[2], |ui| {
                                self.draw_selected_recipe(ui);
                            });
                    });
                },
            );

            ui.separator();
            self.draw_activity_terminal(ui);
        });
    }
}

impl ProfitCrafterApp {
    fn set_status(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.status = message.clone();
        self.activity_log.push_back(message);
        while self.activity_log.len() > 200 {
            self.activity_log.pop_front();
        }
    }

    fn rebuild_rankings(&mut self) {
        self.rows = rank_profitability(
            &self.market_prices,
            &self.recipe_materials,
            self.sale_value_mode,
            self.ranking_mode,
        );
        self.gather_rows =
            rank_gathering(&self.market_prices, self.sale_value_mode, self.ranking_mode);
    }

    fn receive_background_work(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.market_rx.take() {
            match rx.try_recv() {
                Ok(WorkMessage::Progress(message)) => {
                    self.set_status(message);
                    self.market_rx = Some(rx);
                }
                Ok(WorkMessage::Done(Ok(snapshot))) => {
                    if let Err(err) = save_market_snapshot(&snapshot) {
                        self.set_status(format!("Could not save market snapshot: {err:#}"));
                    }
                    self.item_categories = snapshot.categories;
                    self.market_prices = snapshot.prices;
                    if let Ok((cache, _)) = load_xivapi_library() {
                        self.recipe_materials = cache.recipe_materials;
                        self.item_metadata = cache.item_metadata;
                    }
                    self.rebuild_rankings();
                    self.clear_unavailable_filters();
                    self.set_status(format!(
                        "Loaded {} prices, XIVAPI item types, NPC vendor prices, and recent sale counts.",
                        self.market_prices.len()
                    ));
                    self.market_rx = None;
                }
                Ok(WorkMessage::Done(Err(err))) => {
                    self.set_status(format!("Market refresh failed: {err:#}"));
                    self.market_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => self.market_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.set_status("Market refresh worker stopped unexpectedly.");
                    self.market_rx = None;
                }
            }
        }

        if let Some(rx) = self.solve_rx.take() {
            match rx.try_recv() {
                Ok(WorkMessage::Progress(message)) => {
                    self.set_status(message);
                    self.solve_rx = Some(rx);
                }
                Ok(WorkMessage::Done(Ok(solved))) => {
                    self.set_status(format!(
                        "Solved {} craft macros with {} total actions.",
                        solved.plans.len(),
                        solved.total_actions()
                    ));
                    self.solved_macro = Some(solved);
                    self.solve_rx = None;
                }
                Ok(WorkMessage::Done(Err(err))) => {
                    self.set_status(format!("Macro solve failed: {err:#}"));
                    self.solve_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => self.solve_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.set_status("Macro solver stopped unexpectedly.");
                    self.solve_rx = None;
                }
            }
        }

        if let Some(rx) = self.update_rx.take() {
            match rx.try_recv() {
                Ok(WorkMessage::Progress(message)) => {
                    self.set_status(message);
                    self.update_rx = Some(rx);
                }
                Ok(WorkMessage::Done(Ok(message))) => {
                    if let Ok((cache, _)) = load_xivapi_library() {
                        self.item_categories = cache.categories;
                        self.item_metadata = cache.item_metadata;
                        self.recipe_materials = cache.recipe_materials;
                        self.rebuild_rankings();
                    }
                    self.set_status(message);
                    self.update_rx = None;
                }
                Ok(WorkMessage::Done(Err(err))) => {
                    self.set_status(format!("Update failed: {err:#}"));
                    self.update_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => self.update_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.set_status("Repository updater stopped unexpectedly.");
                    self.update_rx = None;
                }
            }
        }

        if self.market_rx.is_some() || self.solve_rx.is_some() || self.update_rx.is_some() {
            ctx.request_repaint();
        }
    }

    fn refresh_market(&mut self, ctx: &egui::Context) {
        if self.market_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.market_rx = Some(rx);
        self.set_status("Connecting to Universalis for current market data...");
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
        self.set_status(format!("Ensuring {repo_dir} repository exists..."));
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
        self.set_status("Updating local XIVAPI cache...");
        ctx.request_repaint();
        thread::spawn(move || {
            report_progress(&tx, "Refreshing XIVAPI cache from v2 sheets.");
            let progress = |message| report_progress(&tx, message);
            let result = update_xivapi_library(&progress).map(|cache| {
                format!(
                    "XIVAPI cache updated: {} item types, {} items, {} NPC gil prices, {} recipe material lists.",
                    cache.categories.len(),
                    cache.item_metadata.len(),
                    cache.vendor_prices.len(),
                    cache.recipe_materials.len()
                )
            });
            let _ = tx.send(WorkMessage::Done(result));
        });
    }

    fn solve_selected_macro(&mut self, ctx: &egui::Context) {
        if self.selected_recipe_id.is_none() {
            match self.mode {
                AppMode::Craft => self.select_first_visible_craft(),
                AppMode::Gather => self.select_first_visible_craftable_gather(),
            }
        }
        let Some(recipe_id) = self.selected_recipe_id else {
            self.set_status("Select a craftable item from the profitable craft list first.");
            return;
        };
        let Some(recipe) = RECIPES.get(recipe_id).copied() else {
            self.set_status("Selected recipe is no longer available.");
            return;
        };
        let stats = self.crafter_stats;
        let hq_ingredients = self.selected_hq_ingredients;
        let market_prices = self.market_prices.clone();
        let (tx, rx) = mpsc::channel();
        self.solve_rx = Some(rx);
        self.set_status("Solving macros with Raphael...");
        ctx.request_repaint();
        thread::spawn(move || {
            report_progress(&tx, "Building craft dependency plan for selected recipe.");
            let progress = |message| report_progress(&tx, message);
            let result = solve_macro_plan(
                recipe_id,
                recipe,
                stats,
                hq_ingredients,
                market_prices,
                &progress,
            );
            let _ = tx.send(WorkMessage::Done(result));
        });
    }

    fn draw_filter_controls(&mut self, ui: &mut egui::Ui) {
        ui.label("Search items");
        let mut search_changed = false;
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.item_search)
                    .hint_text("Type part of an item name")
                    .desired_width((ui.available_width() - 92.0).max(120.0)),
            );
            search_changed |= response.changed();
            if ui
                .add_enabled(
                    !self.item_search.is_empty(),
                    egui::Button::new("Clear search"),
                )
                .clicked()
            {
                self.item_search.clear();
                search_changed = true;
            }
        });
        if search_changed {
            self.set_status(match self.mode {
                AppMode::Craft => {
                    format!("Search matched {} craft rows.", self.filtered_rows().len())
                }
                AppMode::Gather => {
                    format!(
                        "Search matched {} gathering rows.",
                        self.filtered_gather_rows().len()
                    )
                }
            });
        }
        ui.small("Search updates the visible list immediately. Dropdown filters apply only when you click Apply filters.");

        ui.separator();
        ui.label("Optional exact item filter");
        ui.add(
            egui::TextEdit::singleline(&mut self.item_filter_search)
                .hint_text("Search exact item choices")
                .desired_width(ui.available_width()),
        );
        let item_options = self.item_filter_options();
        egui::ComboBox::from_id_salt("item_filter")
            .selected_text(short_label(&self.selected_item_filter_text()))
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.pending_item_filter, None, "All items");
                for &(item_id, ref name) in item_options.iter().take(250) {
                    ui.selectable_value(&mut self.pending_item_filter, Some(item_id), name);
                }
                if item_options.len() > 250 {
                    ui.label(format!(
                        "Showing first 250 of {} matches. Keep typing to narrow choices.",
                        item_options.len()
                    ));
                }
            });

        ui.label("Optional type filter");
        if self.item_categories.is_empty() {
            ui.add_enabled(false, egui::Button::new("No item types loaded"));
        } else {
            let category_options = self.category_filter_options();
            egui::ComboBox::from_id_salt("item_type_filter")
                .selected_text(short_label(&self.selected_category_filter_text()))
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.pending_category_filter, None, "All item types");
                    for category in category_options {
                        ui.selectable_value(
                            &mut self.pending_category_filter,
                            Some(category.id),
                            &category.name,
                        );
                    }
                });
        }

        ui.checkbox(&mut self.pending_only_profitable, "Only profitable");
        ui.checkbox(
            &mut self.pending_only_recently_sold,
            "Only >4 sales in 2 days",
        );

        ui.separator();
        ui.label("Sale value");
        let previous_sale_value_mode = self.sale_value_mode;
        egui::ComboBox::from_id_salt("sale_value_mode")
            .selected_text(self.sale_value_mode.label())
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.sale_value_mode,
                    SaleValueMode::AverageLastFour,
                    SaleValueMode::AverageLastFour.label(),
                );
                ui.selectable_value(
                    &mut self.sale_value_mode,
                    SaleValueMode::LowestLastFour,
                    SaleValueMode::LowestLastFour.label(),
                );
            });
        if self.sale_value_mode != previous_sale_value_mode {
            self.rebuild_rankings();
            self.set_status(format!(
                "Using {} for item values.",
                self.sale_value_mode.label()
            ));
        }

        ui.label("Rank by");
        let previous_ranking_mode = self.ranking_mode;
        egui::ComboBox::from_id_salt("ranking_mode")
            .selected_text(self.ranking_mode.label())
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.ranking_mode,
                    RankingMode::Profit,
                    RankingMode::Profit.label(),
                );
                ui.selectable_value(
                    &mut self.ranking_mode,
                    RankingMode::RecentSales4d,
                    RankingMode::RecentSales4d.label(),
                );
            });
        if self.ranking_mode != previous_ranking_mode {
            self.rebuild_rankings();
            self.set_status(format!("Ranking by {}.", self.ranking_mode.label()));
        }

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
            AppMode::Craft if self.rows.is_empty() => RECIPES
                .values()
                .map(|recipe| (recipe.item_id, item_name(recipe.item_id)))
                .collect::<Vec<_>>(),
            AppMode::Craft => self
                .rows
                .iter()
                .filter(|row| self.pending_row_filters_match(row))
                .map(|row| (row.item_id, row.item_name.clone()))
                .collect::<Vec<_>>(),
            AppMode::Gather => self
                .gather_rows
                .iter()
                .filter(|row| self.pending_gather_filters_match(row))
                .map(|row| (row.item_id, row.item_name.clone()))
                .collect::<Vec<_>>(),
        };
        let needle = self.item_filter_search.trim().to_lowercase();
        if !needle.is_empty() {
            items.retain(|(_, name)| name.to_lowercase().contains(&needle));
        }
        items.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        items.dedup_by_key(|(item_id, _)| *item_id);
        items
    }

    fn category_filter_options(&self) -> Vec<ItemCategory> {
        self.item_categories
            .iter()
            .filter(|category| {
                matches!(self.mode, AppMode::Craft) || is_gather_category_name(&category.name)
            })
            .cloned()
            .collect()
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
        if matches!(self.mode, AppMode::Craft) {
            self.select_first_visible_craft();
        }
        self.set_status(format!(
            "Filters applied: {} craft rows, {} gathering rows.",
            self.filtered_rows().len(),
            self.filtered_gather_rows().len()
        ));
    }

    fn clear_filters(&mut self) {
        self.pending_item_filter = None;
        self.applied_item_filter = None;
        self.item_filter_search.clear();
        self.pending_category_filter = None;
        self.applied_category_filter = None;
        self.pending_only_profitable = false;
        self.applied_only_profitable = false;
        self.pending_only_recently_sold = false;
        self.applied_only_recently_sold = false;
        self.set_status("Filters cleared.");
    }

    fn clear_unavailable_filters(&mut self) {
        let available_items = self
            .rows
            .iter()
            .map(|row| row.item_id)
            .chain(self.gather_rows.iter().map(|row| row.item_id))
            .collect::<HashSet<_>>();
        for item_filter in [&mut self.pending_item_filter, &mut self.applied_item_filter] {
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

    fn draw_crafter_inputs(&mut self, ui: &mut egui::Ui) {
        ui.heading("Inputs");
        ui.horizontal_wrapped(|ui| {
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
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.crafter_stats.manipulation, "Manipulation");
            ui.checkbox(&mut self.crafter_stats.heart_and_soul, "Heart and Soul");
            ui.checkbox(&mut self.crafter_stats.quick_innovation, "Quick Innovation");
        });
    }

    fn draw_activity_terminal(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Activity");
            if self.is_busy() {
                ui.spinner();
            }
            ui.label(&self.status);
        });
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for entry in &self.activity_log {
                    ui.monospace(entry);
                }
            });
    }

    fn select_craft_row(&mut self, row: &ProfitRow) {
        self.selected_item_id = Some(row.item_id);
        self.selected_recipe_id = Some(row.recipe_id);
        self.selected_hq_ingredients = [0; 6];
        self.selected_cost_breakdown = explain_cost(
            row.item_id,
            &self.market_prices,
            &self.item_metadata,
            &self.recipe_materials,
        );
        self.solved_macro = None;
        self.set_status(format!("Selected craft: {}", row.item_name));
    }

    fn select_gather_row(&mut self, row: &GatherRow) {
        self.selected_item_id = Some(row.item_id);
        self.selected_recipe_id = row.recipe_id;
        self.selected_hq_ingredients = [0; 6];
        self.selected_cost_breakdown = explain_cost(
            row.item_id,
            &self.market_prices,
            &self.item_metadata,
            &self.recipe_materials,
        );
        self.solved_macro = None;
        match row.recipe_id {
            Some(_) => self.set_status(format!("Selected craftable material: {}", row.item_name)),
            None => self.set_status(format!("Selected gather item: {}", row.item_name)),
        }
    }

    fn select_first_visible_craft(&mut self) {
        if let Some(row) = self.filtered_rows().into_iter().next() {
            self.select_craft_row(&row);
        }
    }

    fn select_first_visible_craftable_gather(&mut self) {
        if let Some(row) = self
            .filtered_gather_rows()
            .into_iter()
            .find(|row| row.recipe_id.is_some())
        {
            self.select_gather_row(&row);
        }
    }

    fn pending_row_filters_match(&self, row: &ProfitRow) -> bool {
        (!self.pending_only_profitable || row.profit > 0)
            && (!self.pending_only_recently_sold || row.recent_sales_2d > 4)
            && self
                .pending_category_filter
                .is_none_or(|category_id| row.search_category_id == Some(category_id))
    }

    fn pending_gather_filters_match(&self, row: &GatherRow) -> bool {
        (!self.pending_only_profitable || row.profit > 0)
            && (!self.pending_only_recently_sold || row.recent_sales_2d > 4)
            && self
                .pending_category_filter
                .is_none_or(|category_id| row.search_category_id == Some(category_id))
    }

    fn draw_profit_table(&mut self, ui: &mut egui::Ui) {
        ui.heading("Most profitable crafts");
        let rows = self.filtered_rows();
        ui.label(format!(
            "Showing {} of {} craft rows",
            rows.len(),
            self.rows.len()
        ));
        if self.rows.is_empty() {
            ui.label("Waiting for Universalis market data.");
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("profit_results_scroll")
            .auto_shrink([false, false])
            .max_height(ui.available_height())
            .show(ui, |ui| {
                for (idx, row) in rows.into_iter().take(300).enumerate() {
                    let selected = self.selected_item_id == Some(row.item_id);
                    let title = format!("#{} {}", idx + 1, row.item_name);
                    if ui.selectable_label(selected, title).clicked() {
                        self.select_craft_row(&row);
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(format!("Profit {}", format_gil_signed(row.profit)));
                        ui.label(format!("Sell {}", format_gil(row.sell_price)));
                        ui.label(format!("Cost {}", format_gil(row.craft_cost)));
                        ui.label(format!("{} sales/2d", row.recent_sales_2d));
                        ui.label(format!("{} sales/4d", row.recent_sales_4d));
                        ui.label(format!("{:.2}/day", row.daily_velocity));
                    });
                    ui.small(row.category_name.as_deref().unwrap_or("Unknown"));
                    ui.separator();
                }
            });
    }

    fn draw_gather_table(&mut self, ui: &mut egui::Ui) {
        ui.heading("Most profitable gathering");
        let rows = self.filtered_gather_rows();
        ui.label(format!(
            "Showing {} of {} gathering rows",
            rows.len(),
            self.gather_rows.len()
        ));
        if self.gather_rows.is_empty() {
            ui.label("Waiting for Universalis market data.");
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("gather_results_scroll")
            .auto_shrink([false, false])
            .max_height(ui.available_height())
            .show(ui, |ui| {
                for (idx, row) in rows.into_iter().take(500).enumerate() {
                    let selected = self.selected_item_id == Some(row.item_id);
                    let title = match row.recipe_id {
                        Some(_) => format!("#{} {} (craftable)", idx + 1, row.item_name),
                        None => format!("#{} {}", idx + 1, row.item_name),
                    };
                    if ui.selectable_label(selected, title).clicked() {
                        self.select_gather_row(&row);
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(format!("Profit {}", format_gil_signed(row.profit)));
                        ui.label(format!("Sell {}", format_gil(row.sell_price)));
                        ui.label(format!(
                            "NPC {}",
                            row.vendor_price
                                .map(format_gil)
                                .unwrap_or_else(|| "No".to_string())
                        ));
                        ui.label(format!("{} sales/2d", row.recent_sales_2d));
                        ui.label(format!("{} sales/4d", row.recent_sales_4d));
                        ui.label(format!("{:.2}/day", row.daily_velocity));
                    });
                    ui.small(row.category_name.as_deref().unwrap_or("Unknown"));
                    ui.separator();
                }
            });
    }

    fn filtered_rows(&self) -> Vec<ProfitRow> {
        self.rows
            .iter()
            .filter(|row| self.item_matches_search(&row.item_name))
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
            .filter(|row| self.item_matches_search(&row.item_name))
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

    fn item_matches_search(&self, item_name: &str) -> bool {
        let needle = self.item_search.trim();
        needle.is_empty() || item_name.to_lowercase().contains(&needle.to_lowercase())
    }

    fn draw_selected_recipe(&mut self, ui: &mut egui::Ui) {
        ui.heading("Selected craft");
        ui.separator();
        let Some(item_id) = self.selected_item_id else {
            ui.label("Select a ranked recipe to inspect cost, craft tree, and macros.");
            return;
        };

        let selected_item_name = item_name(item_id);
        ui.label(egui::RichText::new(selected_item_name).strong());
        if let Some(row) = self.rows.iter().find(|row| row.item_id == item_id) {
            ui.horizontal(|ui| {
                ui.label(format!("Sell: {}", format_gil(row.sell_price)));
                ui.label(format!("Cost: {}", format_gil(row.craft_cost)));
                ui.label(format!("Profit: {}", format_gil_signed(row.profit)));
            });
        }

        if let Some(recipe_id) = self.selected_recipe_id {
            if let Some(recipe) = RECIPES.get(recipe_id).copied() {
                ui.separator();
                ui.label("HQ ingredients for solver");
                for (index, ingredient) in recipe
                    .ingredients
                    .iter()
                    .filter(|ingredient| ingredient.item_id != 0 && ingredient.amount != 0)
                    .enumerate()
                {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!(
                            "{} x{}",
                            item_name(ingredient.item_id),
                            ingredient.amount
                        ));
                        ui.add(
                            egui::DragValue::new(&mut self.selected_hq_ingredients[index])
                                .range(0..=ingredient.amount as u8)
                                .prefix("HQ "),
                        );
                    });
                }
                let initial_quality =
                    get_initial_quality(self.crafter_stats, recipe, self.selected_hq_ingredients);
                if initial_quality > 0 {
                    ui.small(format!(
                        "Starting quality from selected HQ ingredients: {}",
                        initial_quality
                    ));
                }
            }
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
        ui.label("Macros");
        if let Some(solved) = &self.solved_macro {
            let total_blocks = solved.total_blocks();
            ui.label(format!(
                "{} craft plan(s), {} FFXIV macro block(s), {} total actions",
                solved.plans.len(),
                total_blocks,
                solved.total_actions()
            ));
            egui::ScrollArea::vertical()
                .id_salt("macro_output_scroll")
                .auto_shrink([false, false])
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    for plan in &solved.plans {
                        ui.separator();
                        ui.label(egui::RichText::new(&plan.item_name).strong());
                        ui.label(format!(
                            "Recipe #{}, {} starting quality, {} actions, {} block(s)",
                            plan.recipe_id,
                            plan.initial_quality,
                            plan.actions.len(),
                            plan.macro_blocks.len()
                        ));
                        ui.add(
                            egui::ProgressBar::new(progress_fraction(
                                plan.final_progress,
                                plan.max_progress,
                            ))
                            .text(format!(
                                "Progress: {} / {}",
                                plan.final_progress, plan.max_progress
                            )),
                        );
                        ui.add(
                            egui::ProgressBar::new(progress_fraction(
                                plan.final_quality,
                                plan.max_quality,
                            ))
                            .text(format!(
                                "Quality: {} / {}",
                                plan.final_quality, plan.max_quality
                            )),
                        );
                        for (idx, macro_text) in plan.macro_blocks.iter().enumerate() {
                            ui.label(format!(
                                "Macro block {} of {} - max 15 lines",
                                idx + 1,
                                plan.macro_blocks.len()
                            ));
                            let mut text = macro_text.clone();
                            let response = ui.add(
                                egui::TextEdit::multiline(&mut text)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_rows(15)
                                    .desired_width(ui.available_width())
                                    .interactive(false),
                            );
                            if ui.button(format!("Copy block {}", idx + 1)).clicked() {
                                ui.ctx().copy_text(macro_text.clone());
                            }
                            response.on_hover_text("This block is ready to paste into FFXIV.");
                        }
                    }
                });
        } else {
            ui.label("No macros solved yet. Select a craft row, then click Solve selected macro.");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppMode {
    Craft,
    Gather,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaleValueMode {
    AverageLastFour,
    LowestLastFour,
}

impl SaleValueMode {
    fn label(self) -> &'static str {
        match self {
            SaleValueMode::AverageLastFour => "Average of last 4 sales",
            SaleValueMode::LowestLastFour => "Lowest of last 4 sales",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RankingMode {
    Profit,
    RecentSales4d,
}

impl RankingMode {
    fn label(self) -> &'static str {
        match self {
            RankingMode::Profit => "Profit",
            RankingMode::RecentSales4d => "Sales in past 4 days",
        }
    }
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
    name: Option<String>,
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
    recipe_materials: HashMap<u32, Vec<RecipeMaterial>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct XivApiCacheManifest {
    schema_version: u32,
    relevant_item_count: usize,
    relevant_item_hash: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MarketPrice {
    min_listing: u32,
    average_sale: Option<f64>,
    recent_sale_prices: Vec<u32>,
    daily_velocity: f64,
    #[serde(default)]
    recent_sales_2d: usize,
    #[serde(default)]
    recent_sales_4d: usize,
    metadata: ItemMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SaleHistory {
    recent_sales_2d: usize,
    recent_sales_4d: usize,
    recent_sale_prices: Vec<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct RecipeMaterial {
    item_id: u32,
    amount: u32,
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
    recent_sales_4d: usize,
}

#[derive(Debug, Clone)]
struct GatherRow {
    item_id: u32,
    recipe_id: Option<u32>,
    item_name: String,
    category_name: Option<String>,
    search_category_id: Option<u32>,
    sell_price: u32,
    vendor_price: Option<u32>,
    profit: i64,
    daily_velocity: f64,
    recent_sales_2d: usize,
    recent_sales_4d: usize,
}

#[derive(Debug, Serialize, Deserialize)]
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

    fn total_blocks(&self) -> usize {
        self.plans.iter().map(|plan| plan.macro_blocks.len()).sum()
    }
}

#[derive(Debug)]
struct MacroPlan {
    recipe_id: u32,
    item_name: String,
    initial_quality: u16,
    final_progress: u16,
    max_progress: u16,
    final_quality: u16,
    max_quality: u16,
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
    #[serde(rename = "pricePerUnit")]
    price_per_unit: u32,
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
    progress(format!(
        "Loaded current prices for {} market items.",
        prices.len()
    ));
    let market_item_ids = sorted_market_item_ids(&prices);
    let xivapi_cache = load_or_update_xivapi_library(&market_item_ids, progress)?;
    let sales_history = fetch_sales_history(&client, &market_item_ids, progress)?;

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

    for (item_id, history) in sales_history {
        if let Some(price) = prices.get_mut(&item_id) {
            price.recent_sales_2d = history.recent_sales_2d;
            price.recent_sales_4d = history.recent_sales_4d;
            price.recent_sale_prices = history.recent_sale_prices;
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
        recent_sale_prices: Vec::new(),
        daily_velocity,
        recent_sales_2d: 0,
        recent_sales_4d: 0,
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
        fetch_universalis_market_chunk(client, chunk, &mut prices, progress)?;
    }
    Ok(prices)
}

fn fetch_universalis_market_chunk(
    client: &reqwest::blocking::Client,
    ids: &[u32],
    prices: &mut HashMap<u32, MarketPrice>,
    progress: &dyn Fn(String),
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }

    let joined = ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let url = format!("https://universalis.app/api/v2/aggregated/{WORLD}/{joined}");
    let response = client.get(url).send()?;
    if response.status() == reqwest::StatusCode::BAD_REQUEST {
        if ids.len() == 1 {
            progress(format!(
                "Skipping item {} because Universalis rejected it.",
                ids[0]
            ));
            return Ok(());
        }

        let midpoint = ids.len() / 2;
        progress(format!(
            "Universalis rejected a batch of {} item IDs; splitting it to find valid items.",
            ids.len()
        ));
        fetch_universalis_market_chunk(client, &ids[..midpoint], prices, progress)?;
        fetch_universalis_market_chunk(client, &ids[midpoint..], prices, progress)?;
        return Ok(());
    }

    let data: AggregatedResponse = response.error_for_status()?.json()?;
    for result in data.results {
        if let Some(price) = market_price_from_result(&result) {
            prices.insert(result.item_id, price);
        }
    }
    Ok(())
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
                "Using cached XIVAPI data: {} item types, {} items, {} vendor prices, {} recipe material lists.",
                cache.categories.len(),
                cache.item_metadata.len(),
                cache.vendor_prices.len(),
                cache.recipe_materials.len()
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
        && !cache.recipe_materials.is_empty()
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
    let recipe_materials = read_json_file(&dir.join("recipe_materials.json")).unwrap_or_default();
    Ok((
        XivApiCache {
            categories,
            item_metadata,
            vendor_prices,
            recipe_materials,
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
    let recipe_materials = fetch_xivapi_recipe_materials(&client, progress)
        .context("failed to fetch XIVAPI recipe materials")?;
    let metadata_item_ids = extend_item_ids_with_recipe_materials(item_ids, &recipe_materials);
    progress(format!(
        "Fetching item metadata for {} market-visible outputs and materials.",
        metadata_item_ids.len()
    ));
    let item_metadata = fetch_xivapi_item_metadata(&client, &metadata_item_ids, progress)
        .context("failed to fetch XIVAPI item metadata")?;
    let vendor_prices = vendor_prices_from_item_metadata(&item_metadata);
    progress(format!(
        "Derived relevant NPC vendor prices for {} items and recipe materials for {} crafted outputs.",
        vendor_prices.len(),
        recipe_materials.len()
    ));
    let cache = XivApiCache {
        categories,
        item_metadata,
        vendor_prices,
        recipe_materials,
    };
    progress("Saving XIVAPI cache files.".to_string());
    save_xivapi_library(&cache, &xivapi_cache_manifest(&metadata_item_ids))?;
    Ok(cache)
}

fn save_xivapi_library(cache: &XivApiCache, manifest: &XivApiCacheManifest) -> Result<()> {
    let dir = xivapi_cache_dir();
    fs::create_dir_all(&dir)?;
    write_json_file(&dir.join("cache_manifest.json"), manifest)?;
    write_json_file(&dir.join("item_categories.json"), &cache.categories)?;
    write_json_file(&dir.join("item_metadata.json"), &cache.item_metadata)?;
    write_json_file(&dir.join("vendor_prices.json"), &cache.vendor_prices)?;
    write_json_file(&dir.join("recipe_materials.json"), &cache.recipe_materials)?;
    Ok(())
}

fn load_market_snapshot() -> Result<MarketSnapshot> {
    read_json_file(&xivapi_cache_dir().join("market_snapshot.json"))
}

fn save_market_snapshot(snapshot: &MarketSnapshot) -> Result<()> {
    let dir = xivapi_cache_dir();
    fs::create_dir_all(&dir)?;
    write_json_file(&dir.join("market_snapshot.json"), snapshot)
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
            "https://v2.xivapi.com/api/sheet/Item?rows={rows}&fields=Name,ItemSearchCategory.Name,ItemSearchCategory@as(raw),ItemUICategory.Name,PriceMid"
        );
        let data: XivSheetRows = client.get(url).send()?.error_for_status()?.json()?;
        for row in data.rows {
            let name = row
                .fields
                .get("Name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(ToOwned::to_owned);
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
                    name,
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

fn fetch_xivapi_recipe_materials(
    client: &reqwest::blocking::Client,
    progress: &dyn Fn(String),
) -> Result<HashMap<u32, Vec<RecipeMaterial>>> {
    let recipe_ids = RECIPES
        .entries()
        .map(|(recipe_id, _)| recipe_id)
        .collect::<Vec<_>>();
    let fields = "ItemResult@as(raw),Ingredient@as(raw),AmountIngredient";
    let chunks = recipe_ids.chunks(MAX_MARKET_IDS_PER_REQUEST).len();
    let mut materials_by_output = HashMap::new();

    for (index, chunk) in recipe_ids.chunks(MAX_MARKET_IDS_PER_REQUEST).enumerate() {
        progress(format!(
            "Fetching XIVAPI recipe materials ({}/{chunks}).",
            index + 1
        ));
        let rows = chunk
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let url = format!("https://v2.xivapi.com/api/sheet/Recipe?rows={rows}&fields={fields}");
        let data: XivSheetRows = client.get(url).send()?.error_for_status()?.json()?;
        for row in data.rows {
            let Some(output_item_id) = raw_u32(&row.fields, "ItemResult@as(raw)") else {
                continue;
            };
            if output_item_id == 0 {
                continue;
            }
            let ingredient_ids = raw_u32_array(&row.fields, "Ingredient@as(raw)");
            let amounts = raw_u32_array(&row.fields, "AmountIngredient");
            let mut materials = Vec::new();
            for (item_id, amount) in ingredient_ids.into_iter().zip(amounts) {
                if item_id != 0 && amount != 0 {
                    materials.push(RecipeMaterial { item_id, amount });
                }
            }
            if !materials.is_empty() {
                materials_by_output.insert(output_item_id, materials);
            }
        }
    }

    progress(format!(
        "Loaded XIVAPI recipe materials for {} crafted outputs.",
        materials_by_output.len()
    ));
    Ok(materials_by_output)
}

fn raw_u32(fields: &Value, key: &str) -> Option<u32> {
    fields
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn raw_u32_array(fields: &Value, key: &str) -> Vec<u32> {
    fields
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_u64)
                .filter_map(|value| u32::try_from(value).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn extend_item_ids_with_recipe_materials(
    mut item_ids: Vec<u32>,
    recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
) -> Vec<u32> {
    for (&item_id, materials) in recipe_materials {
        item_ids.push(item_id);
        item_ids.extend(materials.iter().map(|material| material.item_id));
    }
    item_ids.sort_unstable();
    item_ids.dedup();
    item_ids
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

fn fetch_sales_history(
    client: &reqwest::blocking::Client,
    ids: &[u32],
    progress: &dyn Fn(String),
) -> Result<HashMap<u32, SaleHistory>> {
    let mut sales = HashMap::new();

    let chunks = ids.chunks(MAX_MARKET_IDS_PER_REQUEST).len();
    for (index, chunk) in ids.chunks(MAX_MARKET_IDS_PER_REQUEST).enumerate() {
        progress(format!("Fetching sale history ({}/{chunks}).", index + 1));
        let joined = chunk
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let url = format!("https://universalis.app/api/history/{WORLD}/{joined}?entries=100");
        let response = client.get(url).send()?.error_for_status()?;
        match response.json::<HistoryResponse>()? {
            HistoryResponse::Single(item) => {
                sales.insert(item.item_id, sale_history_from_entries(&item.entries));
            }
            HistoryResponse::Multi { items } => {
                for item in items {
                    sales.insert(item.item_id, sale_history_from_entries(&item.entries));
                }
            }
        }
    }

    progress(format!(
        "Loaded sale history for {} market items.",
        sales.len()
    ));
    Ok(sales)
}

fn sale_history_from_entries(entries: &[HistoryEntry]) -> SaleHistory {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let cutoff_2d = now - 172_800;
    let cutoff_4d = now - 345_600;
    let recent_sales_2d = entries
        .iter()
        .filter(|entry| entry.timestamp >= cutoff_2d)
        .count();
    let recent_sales_4d = entries
        .iter()
        .filter(|entry| entry.timestamp >= cutoff_4d)
        .count();
    let mut sorted_entries = entries.iter().collect::<Vec<_>>();
    sorted_entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    let recent_sale_prices = sorted_entries
        .into_iter()
        .filter_map(|entry| (entry.price_per_unit > 0).then_some(entry.price_per_unit))
        .take(4)
        .collect();
    SaleHistory {
        recent_sales_2d,
        recent_sales_4d,
        recent_sale_prices,
    }
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
    if let Ok((cache, _)) = load_xivapi_library() {
        for (&item_id, materials) in &cache.recipe_materials {
            ids.insert(item_id);
            for material in materials {
                ids.insert(material.item_id);
            }
        }
    }
    let mut ids = ids.into_iter().collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

fn rank_profitability(
    market_prices: &HashMap<u32, MarketPrice>,
    xiv_recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
    sale_value_mode: SaleValueMode,
    ranking_mode: RankingMode,
) -> Vec<ProfitRow> {
    let recipe_materials = recipe_materials_with_raphael_fallback(xiv_recipe_materials);
    let raphael_recipe_materials = raphael_recipe_material_map();
    let mut memo = HashMap::new();
    let mut fallback_memo = HashMap::new();
    let mut rows = RECIPES
        .entries()
        .filter_map(|(recipe_id, recipe)| {
            let sell = market_prices.get(&recipe.item_id)?;
            let craft_cost = craft_only_cost(
                recipe.item_id,
                market_prices,
                &recipe_materials,
                &mut memo,
                &mut HashSet::new(),
            )
            .or_else(|| {
                craft_only_cost(
                    recipe.item_id,
                    market_prices,
                    &raphael_recipe_materials,
                    &mut fallback_memo,
                    &mut HashSet::new(),
                )
            })?;
            let sale_basis = sale_value(sell, sale_value_mode);
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
                recent_sales_4d: sell.recent_sales_4d,
            })
        })
        .collect::<Vec<_>>();
    sort_profit_rows(&mut rows, ranking_mode);
    rows
}

fn rank_gathering(
    market_prices: &HashMap<u32, MarketPrice>,
    sale_value_mode: SaleValueMode,
    ranking_mode: RankingMode,
) -> Vec<GatherRow> {
    let recipe_by_item = RECIPES
        .entries()
        .map(|(recipe_id, recipe)| (recipe.item_id, recipe_id))
        .collect::<HashMap<_, _>>();
    let mut rows = market_prices
        .iter()
        .filter(|(_, price)| {
            price
                .metadata
                .search_category_name
                .as_deref()
                .or(price.metadata.ui_category_name.as_deref())
                .is_some_and(is_gather_category_name)
        })
        .map(|(&item_id, price)| {
            let sale_basis = sale_value(price, sale_value_mode);
            let acquisition_cost = 0;
            GatherRow {
                item_id,
                recipe_id: recipe_by_item.get(&item_id).copied(),
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
                recent_sales_4d: price.recent_sales_4d,
            }
        })
        .collect::<Vec<_>>();
    sort_gather_rows(&mut rows, ranking_mode);
    rows
}

fn sort_profit_rows(rows: &mut [ProfitRow], ranking_mode: RankingMode) {
    rows.sort_by(|a, b| match ranking_mode {
        RankingMode::Profit => b
            .profit
            .cmp(&a.profit)
            .then_with(|| b.recent_sales_4d.cmp(&a.recent_sales_4d))
            .then_with(|| b.recent_sales_2d.cmp(&a.recent_sales_2d))
            .then_with(|| b.daily_velocity.total_cmp(&a.daily_velocity)),
        RankingMode::RecentSales4d => b
            .recent_sales_4d
            .cmp(&a.recent_sales_4d)
            .then_with(|| b.profit.cmp(&a.profit))
            .then_with(|| b.daily_velocity.total_cmp(&a.daily_velocity)),
    });
}

fn sort_gather_rows(rows: &mut [GatherRow], ranking_mode: RankingMode) {
    rows.sort_by(|a, b| match ranking_mode {
        RankingMode::Profit => b
            .profit
            .cmp(&a.profit)
            .then_with(|| b.recent_sales_4d.cmp(&a.recent_sales_4d))
            .then_with(|| b.recent_sales_2d.cmp(&a.recent_sales_2d))
            .then_with(|| b.daily_velocity.total_cmp(&a.daily_velocity)),
        RankingMode::RecentSales4d => b
            .recent_sales_4d
            .cmp(&a.recent_sales_4d)
            .then_with(|| b.profit.cmp(&a.profit))
            .then_with(|| b.daily_velocity.total_cmp(&a.daily_velocity)),
    });
}

fn recipe_materials_with_raphael_fallback(
    xiv_recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
) -> HashMap<u32, Vec<RecipeMaterial>> {
    let mut materials = raphael_recipe_material_map();
    for (&item_id, ingredients) in xiv_recipe_materials {
        if !ingredients.is_empty() {
            materials.insert(item_id, ingredients.clone());
        }
    }
    materials
}

fn raphael_recipe_material_map() -> HashMap<u32, Vec<RecipeMaterial>> {
    RECIPES
        .values()
        .map(|recipe| (recipe.item_id, raphael_recipe_materials(recipe)))
        .filter(|(_, ingredients)| !ingredients.is_empty())
        .collect()
}

fn is_gather_category_name(name: &str) -> bool {
    matches!(
        name,
        "Materials"
            | "Metal"
            | "Cloth"
            | "Lumber"
            | "Reagents"
            | "Stone"
            | "Leather"
            | "Ingredients"
            | "Crystals"
            | "Catalysts"
            | "Bone"
    )
}

fn sale_value(price: &MarketPrice, mode: SaleValueMode) -> u32 {
    if !price.recent_sale_prices.is_empty() {
        match mode {
            SaleValueMode::AverageLastFour => {
                let total = price
                    .recent_sale_prices
                    .iter()
                    .map(|&value| u64::from(value))
                    .sum::<u64>();
                (total / price.recent_sale_prices.len() as u64) as u32
            }
            SaleValueMode::LowestLastFour => price
                .recent_sale_prices
                .iter()
                .min()
                .copied()
                .unwrap_or(price.min_listing),
        }
    } else {
        price
            .average_sale
            .map(|value| value.round() as u32)
            .unwrap_or(price.min_listing)
    }
}

fn craft_only_cost(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
    memo: &mut HashMap<u32, Option<u32>>,
    visiting: &mut HashSet<u32>,
) -> Option<u32> {
    if let Some(cost) = memo.get(&item_id) {
        return *cost;
    }
    if !visiting.insert(item_id) {
        return buy_cost(item_id, market_prices);
    }

    let craft_cost = recipe_materials.get(&item_id).and_then(|ingredients| {
        ingredients
            .iter()
            .map(|ingredient| {
                let unit = cheapest_cost(
                    ingredient.item_id,
                    market_prices,
                    recipe_materials,
                    memo,
                    visiting,
                )?;
                Some(unit.saturating_mul(ingredient.amount))
            })
            .sum::<Option<u32>>()
    });

    visiting.remove(&item_id);
    memo.insert(item_id, craft_cost);
    craft_cost
}

fn cheapest_cost(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
    memo: &mut HashMap<u32, Option<u32>>,
    visiting: &mut HashSet<u32>,
) -> Option<u32> {
    let buy = buy_cost(item_id, market_prices);
    let craft = craft_only_cost(item_id, market_prices, recipe_materials, memo, visiting);
    match (buy, craft) {
        (Some(buy), Some(craft)) => Some(buy.min(craft)),
        (Some(buy), None) => Some(buy),
        (None, Some(craft)) => Some(craft),
        (None, None) => None,
    }
}

fn explain_cost(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    item_metadata: &HashMap<u32, ItemMetadata>,
    xiv_recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
) -> Vec<CostNode> {
    let recipe_materials = recipe_materials_with_raphael_fallback(xiv_recipe_materials);
    let mut memo = HashMap::new();
    let mut nodes = Vec::new();
    explain_cost_inner(
        item_id,
        1,
        0,
        market_prices,
        item_metadata,
        &recipe_materials,
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
    item_metadata: &HashMap<u32, ItemMetadata>,
    recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
    memo: &mut HashMap<u32, Option<u32>>,
    visiting: &mut HashSet<u32>,
    nodes: &mut Vec<CostNode>,
) {
    let buy = buy_cost(item_id, market_prices);
    let craft = craft_only_cost(item_id, market_prices, recipe_materials, memo, visiting);
    let ingredients = recipe_materials.get(&item_id).cloned().unwrap_or_default();
    let (unit_cost, source) = match (buy, craft) {
        (Some(buy), Some(craft)) if craft < buy => (craft, "craft".to_string()),
        (Some(buy), Some(craft)) => (
            buy,
            format!("market lowest listing, craft {}", format_gil(craft)),
        ),
        (Some(buy), None) => (buy, "market lowest listing".to_string()),
        (None, Some(craft)) => (craft, "craft".to_string()),
        (None, None) => (0, "missing Universalis price".to_string()),
    };
    nodes.push(CostNode {
        depth,
        name: item_display_name(item_id, market_prices, item_metadata),
        amount,
        unit_cost,
        source: source.clone(),
    });

    if !ingredients.is_empty() && (depth == 0 || source == "craft") {
        for ingredient in ingredients {
            explain_cost_inner(
                ingredient.item_id,
                ingredient.amount,
                depth + 1,
                market_prices,
                item_metadata,
                recipe_materials,
                memo,
                visiting,
                nodes,
            );
        }
    }
}

fn buy_cost(item_id: u32, market_prices: &HashMap<u32, MarketPrice>) -> Option<u32> {
    let price = market_prices.get(&item_id)?;
    Some(price.min_listing)
}

fn item_display_name(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    item_metadata: &HashMap<u32, ItemMetadata>,
) -> String {
    market_prices
        .get(&item_id)
        .and_then(|price| price.metadata.name.clone())
        .or_else(|| {
            item_metadata
                .get(&item_id)
                .and_then(|metadata| metadata.name.clone())
        })
        .unwrap_or_else(|| item_name(item_id))
}

fn raphael_recipe_materials(recipe: &Recipe) -> Vec<RecipeMaterial> {
    recipe
        .ingredients
        .iter()
        .filter(|ingredient| ingredient.item_id != 0 && ingredient.amount != 0)
        .map(|ingredient| RecipeMaterial {
            item_id: ingredient.item_id,
            amount: ingredient.amount,
        })
        .collect()
}

fn solve_macro_plan(
    final_recipe_id: u32,
    final_recipe: Recipe,
    crafter_stats: CrafterStats,
    final_hq_ingredients: [u8; 6],
    market_prices: HashMap<u32, MarketPrice>,
    progress: &dyn Fn(String),
) -> Result<SolvedMacro> {
    let recipes_by_item_id = RECIPES
        .entries()
        .map(|(recipe_id, recipe)| (recipe.item_id, (recipe_id, *recipe)))
        .collect::<HashMap<_, _>>();
    let cost_recipes = recipes_by_item_id
        .iter()
        .map(|(&item_id, &(_, recipe))| (item_id, raphael_recipe_materials(&recipe)))
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
        let hq_ingredients = if recipe_id == final_recipe_id {
            final_hq_ingredients
        } else {
            [0; 6]
        };
        plans.push(solve_one_macro(
            recipe_id,
            recipe,
            crafter_stats,
            hq_ingredients,
        )?);
    }
    Ok(SolvedMacro { plans })
}

fn collect_crafted_dependencies(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    recipes_by_item_id: &HashMap<u32, (u32, Recipe)>,
    cost_recipes: &HashMap<u32, Vec<RecipeMaterial>>,
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
    hq_ingredients: [u8; 6],
) -> Result<MacroPlan> {
    let original_simulator_settings = get_game_settings(recipe, None, crafter_stats, None, None);
    let mut simulator_settings = original_simulator_settings;
    let initial_quality = get_initial_quality(crafter_stats, recipe, hq_ingredients);
    simulator_settings.max_quality = simulator_settings
        .max_quality
        .saturating_sub(initial_quality);
    if simulator_settings.max_quality == 0 {
        simulator_settings.max_quality = 1;
    }
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
    let final_state = SimulationState::from_macro(&simulator_settings, &actions)
        .map_err(|err| anyhow!("solved macro did not simulate cleanly: {err:?}"))?;
    let final_progress = final_state
        .progress
        .min(original_simulator_settings.max_progress);
    let final_quality = final_state
        .quality
        .saturating_add(initial_quality)
        .min(original_simulator_settings.max_quality);
    let macro_blocks = create_macro_blocks(&actions);
    Ok(MacroPlan {
        recipe_id,
        item_name: item_name(recipe.item_id),
        initial_quality,
        final_progress,
        max_progress: original_simulator_settings.max_progress,
        final_quality,
        max_quality: original_simulator_settings.max_quality,
        actions,
        macro_blocks,
    })
}

fn progress_fraction(value: u16, max_value: u16) -> f32 {
    if max_value == 0 {
        0.0
    } else {
        (f32::from(value) / f32::from(max_value)).clamp(0.0, 1.0)
    }
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

fn short_label(text: &str) -> String {
    const MAX_CHARS: usize = 34;
    if text.chars().count() <= MAX_CHARS {
        text.to_string()
    } else {
        format!(
            "{}...",
            text.chars().take(MAX_CHARS - 3).collect::<String>()
        )
    }
}
