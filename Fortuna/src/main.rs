use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use eframe::egui;
use raphael_data::{
    CrafterStats, ITEMS, Locale, RECIPES, Recipe, get_game_settings, get_initial_quality,
    get_raw_item_name,
};
use raphael_sim::{Action, SimulationState};
use raphael_solver::{AtomicFlag, MacroSolver, SolverSettings};
use serde::{Deserialize, Serialize};

const WORLD: &str = "Phoenix";
const DATA_CENTER: &str = "Light";
const MAX_MARKET_IDS_PER_REQUEST: usize = 100;
const GAME_DATA_CACHE_SCHEMA_VERSION: u32 = 6;
const UNAVAILABLE_VENDOR_PRICE: u32 = 99_999;
const LODESTONE_BASE_URL: &str = "https://eu.finalfantasyxiv.com";
const LODESTONE_REFRESH_BATCH_ITEMS: usize = 200;
const LODESTONE_REFRESH_BATCH_DUTIES: usize = 200;
const LODESTONE_REQUEST_DELAY_SECS: u64 = 5;
const LODESTONE_DUTY_CATEGORY_IDS: &[u32] = &[2, 4, 5, 28, 37];
const LODESTONE_DROP_REFRESH_STATE_VERSION: u32 = 3;

fn main() -> eframe::Result {
    log_terminal("Starting Fortuna Profit Crafter.");
    if std::env::args()
        .any(|arg| arg == "--refresh-saintcoinach-cache" || arg == "--refresh-game-data-cache")
    {
        let progress = |message: String| log_terminal(&message);
        match refresh_saintcoinach_data(&progress) {
            Ok(cache) => log_terminal(&format!(
                "Game data cache ready: {} item types, {} items, {} vendor prices.",
                cache.categories.len(),
                cache.item_metadata.len(),
                cache.vendor_prices.len()
            )),
            Err(err) => log_terminal(&format!("Game data cache refresh failed: {err:#}")),
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
    market_rows: Vec<MarketRow>,
    item_categories: Vec<ItemCategory>,
    item_metadata: HashMap<u32, ItemMetadata>,
    vendor_sources: HashMap<u32, Vec<VendorSourceInfo>>,
    drop_sources: HashMap<u32, Vec<DropSourceInfo>>,
    recipe_materials: HashMap<u32, Vec<RecipeMaterial>>,
    gathering_items: Vec<GatheringItemInfo>,
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
    pending_market_only_npc: bool,
    applied_market_only_npc: bool,
    pending_market_hide_craftable: bool,
    applied_market_hide_craftable: bool,
    pending_market_hide_gatherable: bool,
    applied_market_hide_gatherable: bool,
    sale_value_mode: SaleValueMode,
    ranking_mode: RankingMode,
    sales_graph_mode: SalesGraphMode,
    status: String,
    activity_log: VecDeque<String>,
    activity_progress: Option<ActivityProgress>,
    market_rx: Option<Receiver<WorkMessage<MarketSnapshot>>>,
    solve_rx: Option<Receiver<WorkMessage<SolvedMacro>>>,
    update_rx: Option<Receiver<WorkMessage<String>>>,
    solved_macro: Option<SolvedMacro>,
    crafter_stats: CrafterStats,
}

impl Default for ProfitCrafterApp {
    fn default() -> Self {
        let (
            item_categories,
            item_metadata,
            vendor_sources,
            drop_sources,
            recipe_materials,
            gathering_items,
            mut status,
        ) = match load_game_data_cache() {
            Ok((cache, _)) => (
                cache.categories,
                cache.item_metadata,
                cache.vendor_sources,
                cache.drop_sources,
                cache.recipe_materials,
                cache.gathering_items,
                "Loaded stored SaintCoinach item types and recipe materials.".to_string(),
            ),
            Err(_) => (
                Vec::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                Vec::new(),
                "No stored SaintCoinach item types or recipe materials loaded.".to_string(),
            ),
        };
        let (market_prices, rows, gather_rows, market_rows) = match load_market_snapshot() {
            Ok(snapshot) => {
                let rows = rank_profitability(
                    &snapshot.prices,
                    &item_metadata,
                    &vendor_sources,
                    &recipe_materials,
                    SaleValueMode::LowestLastFour,
                    RankingMode::RecentSales4d,
                );
                let gather_rows = rank_gathering(
                    &snapshot.prices,
                    &gathering_items,
                    SaleValueMode::LowestLastFour,
                    RankingMode::RecentSales4d,
                );
                let market_rows = rank_market(
                    &snapshot.prices,
                    &item_metadata,
                    &vendor_sources,
                    &recipe_materials,
                    &gathering_items,
                    SaleValueMode::LowestLastFour,
                    RankingMode::RecentSales4d,
                );
                status = format!(
                    "Loaded stored market snapshot with {} prices. Refresh market when you want current data.",
                    snapshot.prices.len()
                );
                (snapshot.prices, rows, gather_rows, market_rows)
            }
            Err(_) => (HashMap::new(), Vec::new(), Vec::new(), Vec::new()),
        };

        let mut activity_log = VecDeque::new();
        activity_log.push_back(status.clone());

        Self {
            mode: AppMode::Market,
            market_prices,
            rows,
            gather_rows,
            market_rows,
            item_categories,
            item_metadata,
            recipe_materials,
            gathering_items,
            vendor_sources,
            drop_sources,
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
            pending_market_only_npc: false,
            applied_market_only_npc: false,
            pending_market_hide_craftable: false,
            applied_market_hide_craftable: false,
            pending_market_hide_gatherable: false,
            applied_market_hide_gatherable: false,
            sale_value_mode: SaleValueMode::LowestLastFour,
            ranking_mode: RankingMode::RecentSales4d,
            sales_graph_mode: SalesGraphMode::Quantity,
            status,
            activity_log,
            activity_progress: None,
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
                ui.selectable_value(&mut self.mode, AppMode::Market, "Market");
                ui.selectable_value(&mut self.mode, AppMode::Craft, "Craft");
                ui.selectable_value(&mut self.mode, AppMode::Gather, "Gather");
                ui.separator();
                ui.label("Sources:");
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Update Universalis market"))
                    .clicked()
                {
                    self.refresh_market(&ctx);
                }
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Refresh SaintCoinach data"))
                    .clicked()
                {
                    self.refresh_saintcoinach_data(&ctx);
                }
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Refresh Lodestone drops"))
                    .clicked()
                {
                    self.refresh_lodestone_drops(&ctx);
                }
                ui.separator();
                ui.label("Repos:");
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Ensure raphael-rs"))
                    .clicked()
                {
                    self.ensure_repository(
                        &ctx,
                        "Source/raphael-rs",
                        "https://github.com/KonaeAkira/raphael-rs.git",
                    );
                }
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Ensure Universalis"))
                    .clicked()
                {
                    self.ensure_repository(
                        &ctx,
                        "Source/Universalis",
                        "https://github.com/Universalis-FFXIV/Universalis.git",
                    );
                }
                if ui
                    .add_enabled(!is_busy, egui::Button::new("Ensure SaintCoinach"))
                    .clicked()
                {
                    self.ensure_repository(
                        &ctx,
                        "Source/SaintCoinach",
                        "https://github.com/ufx/SaintCoinach.git",
                    );
                }
                ui.separator();
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
            let lower_sections_height = if self.solved_macro.is_some() {
                (available_height * 0.52).clamp(420.0, 560.0)
            } else {
                190.0
            };
            let main_height = if available_height > 260.0 {
                (available_height - lower_sections_height).max(260.0)
            } else {
                available_height * 0.55
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
                                ui.label(format!(
                                    "Market prices: {}",
                                    format_count(self.market_prices.len())
                                ));
                                ui.label(format!(
                                    "Ranked recipes: {}",
                                    format_count(self.rows.len())
                                ));
                                ui.label(format!(
                                    "Gathering rows: {}",
                                    format_count(self.gather_rows.len())
                                ));
                                ui.label(format!(
                                    "Market rows: {}",
                                    format_count(self.market_rows.len())
                                ));
                            });

                        if matches!(self.mode, AppMode::Craft) {
                            self.draw_crafter_inputs(&mut columns[1]);
                            columns[1].separator();
                        }

                        match self.mode {
                            AppMode::Craft => self.draw_profit_table(&mut columns[1]),
                            AppMode::Gather => self.draw_gather_table(&mut columns[1]),
                            AppMode::Market => self.draw_market_table(&mut columns[1]),
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
            let activity_height = if self.activity_progress.is_some() {
                144.0
            } else {
                132.0
            };
            let lower_available = ui.available_height();
            let macro_height = if self.solved_macro.is_some() {
                (lower_available - activity_height - 8.0).max(0.0)
            } else {
                32.0
            };
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), macro_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    self.draw_macro_output_section(ui);
                },
            );
            ui.separator();
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), activity_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    self.draw_activity_terminal(ui);
                },
            );
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

    fn set_progress(&mut self, progress: ActivityProgress) {
        self.status = progress.detail.clone();
        self.activity_progress = Some(progress);
    }

    fn clear_progress(&mut self) {
        self.activity_progress = None;
    }

    fn rebuild_rankings(&mut self) {
        self.rows = rank_profitability(
            &self.market_prices,
            &self.item_metadata,
            &self.vendor_sources,
            &self.recipe_materials,
            self.sale_value_mode,
            self.ranking_mode,
        );
        self.gather_rows = rank_gathering(
            &self.market_prices,
            &self.gathering_items,
            self.sale_value_mode,
            self.ranking_mode,
        );
        self.market_rows = rank_market(
            &self.market_prices,
            &self.item_metadata,
            &self.vendor_sources,
            &self.recipe_materials,
            &self.gathering_items,
            self.sale_value_mode,
            self.ranking_mode,
        );
    }

    fn receive_background_work(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.market_rx.take() {
            match rx.try_recv() {
                Ok(WorkMessage::Progress(message)) => {
                    self.handle_market_progress(message);
                    self.market_rx = Some(rx);
                }
                Ok(WorkMessage::Done(Ok(snapshot))) => {
                    self.clear_progress();
                    if let Err(err) = save_market_snapshot(&snapshot) {
                        self.set_status(format!("Could not save market snapshot: {err:#}"));
                    }
                    self.item_categories = snapshot.categories;
                    self.market_prices = snapshot.prices;
                    if let Ok((cache, _)) = load_game_data_cache() {
                        self.recipe_materials = cache.recipe_materials;
                        self.item_metadata = cache.item_metadata;
                        self.vendor_sources = cache.vendor_sources;
                        self.drop_sources = cache.drop_sources;
                        self.gathering_items = cache.gathering_items;
                    }
                    self.rebuild_rankings();
                    self.clear_unavailable_filters();
                    self.set_status(format!(
                        "Loaded {} prices, SaintCoinach item types, NPC vendor prices, and recent sale counts.",
                        self.market_prices.len()
                    ));
                    self.market_rx = None;
                }
                Ok(WorkMessage::Done(Err(err))) => {
                    self.clear_progress();
                    self.set_status(format!("Market refresh failed: {err:#}"));
                    self.market_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => self.market_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.clear_progress();
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
                    self.clear_progress();
                    self.set_status(format!(
                        "Solved {} craft macros with {} total actions.",
                        solved.plans.len(),
                        solved.total_actions()
                    ));
                    self.solved_macro = Some(solved);
                    self.solve_rx = None;
                }
                Ok(WorkMessage::Done(Err(err))) => {
                    self.clear_progress();
                    self.set_status(format!("Macro solve failed: {err:#}"));
                    self.solve_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => self.solve_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.clear_progress();
                    self.set_status("Macro solver stopped unexpectedly.");
                    self.solve_rx = None;
                }
            }
        }

        if let Some(rx) = self.update_rx.take() {
            match rx.try_recv() {
                Ok(WorkMessage::Progress(message)) => {
                    self.handle_update_progress(message);
                    self.update_rx = Some(rx);
                }
                Ok(WorkMessage::Done(Ok(message))) => {
                    self.clear_progress();
                    if let Ok((cache, _)) = load_game_data_cache() {
                        self.item_categories = cache.categories;
                        self.item_metadata = cache.item_metadata;
                        self.vendor_sources = cache.vendor_sources;
                        self.drop_sources = cache.drop_sources;
                        self.recipe_materials = cache.recipe_materials;
                        self.gathering_items = cache.gathering_items;
                        self.rebuild_rankings();
                    }
                    self.set_status(message);
                    self.update_rx = None;
                }
                Ok(WorkMessage::Done(Err(err))) => {
                    self.clear_progress();
                    self.set_status(format!("Update failed: {err:#}"));
                    self.update_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => self.update_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.clear_progress();
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
        let ids = market_lookup_item_ids(market_refresh_preview_item_ids(), &|_| {});
        let pages = ids.chunks(MAX_MARKET_IDS_PER_REQUEST).len();
        self.set_progress(ActivityProgress::new(
            "Universalis refresh",
            format!(
                "Checking {} item IDs across {} price page(s).",
                ids.len(),
                pages
            ),
            0,
            pages.max(1),
        ));
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

    fn refresh_saintcoinach_data(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        self.set_status("Refreshing local SaintCoinach game data...");
        let item_ids = load_marketable_item_ids().unwrap_or_else(|_| relevant_market_item_ids());
        self.set_progress(ActivityProgress::new(
            "SaintCoinach refresh",
            format!("Preparing metadata export for {} item IDs.", item_ids.len()),
            0,
            4,
        ));
        ctx.request_repaint();
        thread::spawn(move || {
            report_progress(&tx, "Refreshing game data cache from SaintCoinach.");
            let progress = |message| report_progress(&tx, message);
            let result = refresh_saintcoinach_data(&progress).map(|cache| {
                format!(
                    "Game data cache updated: {} item types, {} items, {} NPC gil prices, {} recipe material lists.",
                    cache.categories.len(),
                    cache.item_metadata.len(),
                    cache.vendor_prices.len(),
                    cache.recipe_materials.len()
                )
            });
            let _ = tx.send(WorkMessage::Done(result));
        });
    }

    fn refresh_lodestone_drops(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        self.set_status("Refreshing Lodestone drop sources...");
        self.set_progress(ActivityProgress::new(
            "Lodestone drops",
            "Preparing conservative Lodestone scrape.".to_string(),
            0,
            LODESTONE_REFRESH_BATCH_ITEMS,
        ));
        ctx.request_repaint();
        thread::spawn(move || {
            report_progress(&tx, "Preparing Lodestone drop-source refresh.");
            let progress = |message| report_progress(&tx, message);
            let result = refresh_lodestone_drop_sources(&progress).map(|summary| {
                format!(
                    "Lodestone drops updated: {} item candidate(s), {} item page(s), {} duty page(s) of {}, {} new source(s).",
                    summary.candidate_count,
                    summary.checked_count,
                    summary.checked_duty_count,
                    summary.duty_count,
                    summary.new_source_count
                )
            });
            let _ = tx.send(WorkMessage::Done(result));
        });
    }

    fn handle_market_progress(&mut self, message: String) {
        if let Some((current, total)) =
            parse_fraction_progress(&message, "Fetching current market prices (")
        {
            self.set_progress(ActivityProgress::new(
                "Universalis prices",
                format!("Fetching price page {current} of {total}."),
                current,
                total,
            ));
        } else if let Some((current, total)) =
            parse_fraction_progress(&message, "Fetching sale history (")
        {
            self.set_progress(ActivityProgress::new(
                "Universalis sales",
                format!("Fetching sale-history page {current} of {total}."),
                current,
                total,
            ));
        } else {
            self.set_status(message);
        }
    }

    fn handle_update_progress(&mut self, message: String) {
        if let Some(item_count) =
            parse_count_after(&message, "Refreshing SaintCoinach game data for ")
        {
            self.set_progress(ActivityProgress::new(
                "SaintCoinach refresh",
                format!("Preparing metadata export for {item_count} item IDs."),
                1,
                4,
            ));
        } else if let Some(item_count) =
            parse_count_after(&message, "Exporting SaintCoinach metadata for ")
        {
            self.set_progress(ActivityProgress::new(
                "SaintCoinach refresh",
                format!("Exporting metadata for {item_count} item IDs."),
                2,
                4,
            ));
        } else if message.starts_with("Running SaintCoinach exporter against ") {
            self.set_progress(ActivityProgress::new(
                "SaintCoinach refresh",
                "Running SaintCoinach exporter.".to_string(),
                3,
                4,
            ));
        } else if message.starts_with("Derived relevant NPC vendor prices") {
            self.set_progress(ActivityProgress::new(
                "SaintCoinach refresh",
                "Saving refreshed vendor and recipe data.".to_string(),
                4,
                4,
            ));
        } else if let Some((current, total)) =
            parse_fraction_progress(&message, "Lodestone item page ")
        {
            self.set_progress(ActivityProgress::new(
                "Lodestone drops",
                format!(
                    "Checking Lodestone item page {current} of {total}. Requests are delayed by {LODESTONE_REQUEST_DELAY_SECS}s."
                ),
                current,
                total,
            ));
        } else if let Some(item_count) = parse_count_after(&message, "Lodestone candidate items: ")
        {
            self.set_progress(ActivityProgress::new(
                "Lodestone drops",
                format!("Found {item_count} unknown tradable item candidate(s)."),
                0,
                LODESTONE_REFRESH_BATCH_ITEMS,
            ));
        } else if let Some((current, total)) =
            parse_fraction_progress(&message, "Lodestone duty page ")
        {
            self.set_progress(ActivityProgress::new(
                "Lodestone drops",
                format!(
                    "Checking Lodestone duty page {current} of {total}. Requests are delayed by {LODESTONE_REQUEST_DELAY_SECS}s."
                ),
                current,
                total,
            ));
        } else {
            self.set_status(message);
        }
    }

    fn solve_selected_macro(&mut self, ctx: &egui::Context) {
        if self.selected_recipe_id.is_none() {
            match self.mode {
                AppMode::Craft => self.select_first_visible_craft(),
                AppMode::Gather => self.select_first_visible_craftable_gather(),
                AppMode::Market => self.select_first_visible_market_craft(),
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
        let item_metadata = self.item_metadata.clone();
        let vendor_sources = self.vendor_sources.clone();
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
                item_metadata,
                vendor_sources,
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
                AppMode::Craft => format!(
                    "Search matched {} craft rows.",
                    format_count(self.filtered_rows().len())
                ),
                AppMode::Gather => {
                    format!(
                        "Search matched {} gathering rows.",
                        format_count(self.filtered_gather_rows().len())
                    )
                }
                AppMode::Market => format!(
                    "Search matched {} market rows.",
                    format_count(self.filtered_market_rows().len())
                ),
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
        if matches!(self.mode, AppMode::Market) {
            ui.checkbox(&mut self.pending_market_only_npc, "Only NPC-buyable");
            ui.checkbox(&mut self.pending_market_hide_craftable, "Hide craftable");
            ui.checkbox(&mut self.pending_market_hide_gatherable, "Hide gatherable");
        }

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
                ui.selectable_value(
                    &mut self.ranking_mode,
                    RankingMode::RecentQuantity4d,
                    RankingMode::RecentQuantity4d.label(),
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
            AppMode::Market => self
                .market_rows
                .iter()
                .filter(|row| self.pending_market_filters_match(row))
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
                matches!(self.mode, AppMode::Craft | AppMode::Market)
                    || is_gather_category_name(&category.name)
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
            || self.pending_market_only_npc != self.applied_market_only_npc
            || self.pending_market_hide_craftable != self.applied_market_hide_craftable
            || self.pending_market_hide_gatherable != self.applied_market_hide_gatherable
    }

    fn apply_filters(&mut self) {
        self.applied_item_filter = self.pending_item_filter;
        self.applied_category_filter = self.pending_category_filter;
        self.applied_only_profitable = self.pending_only_profitable;
        self.applied_only_recently_sold = self.pending_only_recently_sold;
        self.applied_market_only_npc = self.pending_market_only_npc;
        self.applied_market_hide_craftable = self.pending_market_hide_craftable;
        self.applied_market_hide_gatherable = self.pending_market_hide_gatherable;
        if matches!(self.mode, AppMode::Craft) {
            self.select_first_visible_craft();
        }
        self.set_status(format!(
            "Filters applied: {} craft rows, {} gathering rows, {} market rows.",
            self.filtered_rows().len(),
            self.filtered_gather_rows().len(),
            self.filtered_market_rows().len()
        ));
    }

    fn clear_filters(&mut self) {
        self.item_search.clear();
        self.pending_item_filter = None;
        self.applied_item_filter = None;
        self.item_filter_search.clear();
        self.pending_category_filter = None;
        self.applied_category_filter = None;
        self.pending_only_profitable = false;
        self.applied_only_profitable = false;
        self.pending_only_recently_sold = false;
        self.applied_only_recently_sold = false;
        self.pending_market_only_npc = false;
        self.applied_market_only_npc = false;
        self.pending_market_hide_craftable = false;
        self.applied_market_hide_craftable = false;
        self.pending_market_hide_gatherable = false;
        self.applied_market_hide_gatherable = false;
        self.sale_value_mode = SaleValueMode::LowestLastFour;
        self.ranking_mode = RankingMode::RecentSales4d;
        self.rebuild_rankings();
        self.set_status("Filters cleared.");
    }

    fn clear_unavailable_filters(&mut self) {
        let available_items = self
            .rows
            .iter()
            .map(|row| row.item_id)
            .chain(self.gather_rows.iter().map(|row| row.item_id))
            .chain(self.market_rows.iter().map(|row| row.item_id))
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
        if let Some(progress) = &self.activity_progress {
            ui.add_sized(
                egui::vec2(ui.available_width(), 18.0),
                egui::ProgressBar::new(progress.fraction())
                    .text(format!("{}: {}", progress.label, progress.detail)),
            );
            ui.add_space(4.0);
        }
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(if self.activity_progress.is_some() {
                88.0
            } else {
                112.0
            })
            .show(ui, |ui| {
                for entry in &self.activity_log {
                    ui.monospace(entry);
                }
            });
    }

    fn draw_macro_output_section(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Macros");
            if let Some(solved) = &self.solved_macro {
                ui.label(format!(
                    "{} craft plan(s), {} FFXIV macro block(s), {} total actions",
                    format_count(solved.plans.len()),
                    format_count(solved.total_blocks()),
                    format_count(solved.total_actions())
                ));
            } else {
                ui.label("No macros solved yet.");
            }
        });

        let Some(solved) = &self.solved_macro else {
            return;
        };

        egui::ScrollArea::horizontal()
            .id_salt("macro_output_horizontal_scroll")
            .auto_shrink([false, false])
            .max_height(ui.available_height())
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    for plan in &solved.plans {
                        for (idx, macro_text) in plan.macro_blocks.iter().enumerate() {
                            ui.vertical(|ui| {
                                ui.set_width(320.0);
                                ui.label(egui::RichText::new(&plan.item_name).strong());
                                ui.small(format!(
                                    "Recipe #{}, block {} of {}, {} starting quality, {} actions",
                                    format_count(plan.recipe_id),
                                    format_count(idx + 1),
                                    format_count(plan.macro_blocks.len()),
                                    format_count(plan.initial_quality),
                                    format_count(plan.actions.len())
                                ));
                                ui.add(
                                    egui::ProgressBar::new(progress_fraction(
                                        plan.final_progress,
                                        plan.max_progress,
                                    ))
                                    .text(format!(
                                        "Progress: {} / {}",
                                        format_count(plan.final_progress),
                                        format_count(plan.max_progress)
                                    )),
                                );
                                ui.add(
                                    egui::ProgressBar::new(progress_fraction(
                                        plan.final_quality,
                                        plan.max_quality,
                                    ))
                                    .text(format!(
                                        "Quality: {} / {}",
                                        format_count(plan.final_quality),
                                        format_count(plan.max_quality)
                                    )),
                                );
                                let mut text = macro_text.clone();
                                let response = ui.add(
                                    egui::TextEdit::multiline(&mut text)
                                        .font(egui::TextStyle::Monospace)
                                        .desired_rows(15)
                                        .desired_width(320.0)
                                        .interactive(false),
                                );
                                if ui
                                    .button(format!("Copy block {}", format_count(idx + 1)))
                                    .clicked()
                                {
                                    ui.ctx().copy_text(macro_text.clone());
                                }
                                response.on_hover_text("This block is ready to paste into FFXIV.");
                            });
                            ui.add_space(12.0);
                        }
                    }
                });
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
            &self.vendor_sources,
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
            &self.vendor_sources,
            &self.recipe_materials,
        );
        self.solved_macro = None;
        match row.recipe_id {
            Some(_) => self.set_status(format!("Selected craftable material: {}", row.item_name)),
            None => self.set_status(format!("Selected gather item: {}", row.item_name)),
        }
    }

    fn select_market_row(&mut self, row: &MarketRow) {
        self.selected_item_id = Some(row.item_id);
        self.selected_recipe_id = row.recipe_id;
        self.selected_hq_ingredients = [0; 6];
        self.selected_cost_breakdown = explain_cost(
            row.item_id,
            &self.market_prices,
            &self.item_metadata,
            &self.vendor_sources,
            &self.recipe_materials,
        );
        self.solved_macro = None;
        self.set_status(format!("Selected market item: {}", row.item_name));
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

    fn select_first_visible_market_craft(&mut self) {
        if let Some(row) = self
            .filtered_market_rows()
            .into_iter()
            .find(|row| row.recipe_id.is_some())
        {
            self.select_market_row(&row);
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

    fn pending_market_filters_match(&self, row: &MarketRow) -> bool {
        (!self.pending_only_profitable || row.profit.is_some_and(|profit| profit > 0))
            && (!self.pending_only_recently_sold || row.recent_sales_2d > 4)
            && (!self.pending_market_only_npc || row.npc_price.is_some())
            && (!self.pending_market_hide_craftable
                || row.recipe_id.is_none()
                || row.npc_price.is_some())
            && (!self.pending_market_hide_gatherable || !row.is_gathered || row.npc_price.is_some())
            && self
                .pending_category_filter
                .is_none_or(|category_id| row.search_category_id == Some(category_id))
    }

    fn draw_profit_table(&mut self, ui: &mut egui::Ui) {
        ui.heading("Most profitable crafts");
        let rows = self.filtered_rows();
        ui.label(format!(
            "Showing {} of {} craft rows",
            format_count(rows.len()),
            format_count(self.rows.len())
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
                    let title = format!("#{} {}", format_count(idx + 1), row.item_name);
                    if ui.selectable_label(selected, title).clicked() {
                        self.select_craft_row(&row);
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(format!("Profit {}", format_gil_signed(row.profit)));
                        ui.label(format!("Sell {}", format_gil(row.sell_price)));
                        ui.label(format!("Cost {}", format_gil(row.craft_cost)));
                    });
                    ui.horizontal_wrapped(|ui| {
                        if let Some(price) = self.market_prices.get(&row.item_id) {
                            ui.label(format!(
                                "Avg sale {}",
                                format_gil(average_sale_display(price))
                            ));
                            ui.label(format!("{:.2} sales/day", price.daily_velocity));
                        }
                        ui.label(format!("{} sales/1d", format_count(row.recent_sales_1d)));
                        ui.label(format!("{} sales/2d", format_count(row.recent_sales_2d)));
                        ui.label(format!("{} sales/4d", format_count(row.recent_sales_4d)));
                        ui.label(format!("{} qty/4d", format_count(row.recent_quantity_4d)));
                        ui.label(format!("{:.2}/day", row.daily_velocity));
                    });
                    ui.small(row.category_name.as_deref().unwrap_or("Unknown"));
                    ui.separator();
                }
            });
    }

    fn draw_gather_table(&mut self, ui: &mut egui::Ui) {
        ui.heading("Gatherable items");
        let rows = self.filtered_gather_rows();
        ui.label(format!(
            "Showing {} of {} gathering rows",
            format_count(rows.len()),
            format_count(self.gather_rows.len())
        ));
        if self.gather_rows.is_empty() {
            ui.label("Waiting for SaintCoinach gathering data.");
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
                        Some(_) => {
                            format!("#{} {} (craftable)", format_count(idx + 1), row.item_name)
                        }
                        None => format!("#{} {}", format_count(idx + 1), row.item_name),
                    };
                    if ui.selectable_label(selected, title).clicked() {
                        self.select_gather_row(&row);
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(format!("Unit {}", format_gil(row.sell_price)));
                        ui.label(format!("{} sales/1d", format_count(row.recent_sales_1d)));
                        ui.label(format!("{} sales/2d", format_count(row.recent_sales_2d)));
                        ui.label(format!("{} sales/4d", format_count(row.recent_sales_4d)));
                        ui.label(format!(
                            "{} / {} / {} qty sold",
                            format_count(row.recent_quantity_1d),
                            format_count(row.recent_quantity_2d),
                            format_count(row.recent_quantity_4d)
                        ));
                        ui.label(format!("{:.2}/day", row.daily_velocity));
                    });
                    ui.small(format!("Node: {}", row.node_type));
                    ui.small(format!("Where: {}", row.where_text));
                    if row.timed_windows.is_empty() {
                        ui.small("Always available or timing unknown");
                    } else {
                        ui.small(format!("Timed: {}", row.timed_windows.join(", ")));
                    }
                    ui.separator();
                }
            });
    }

    fn draw_market_table(&mut self, ui: &mut egui::Ui) {
        ui.heading("Market items");
        let rows = self.filtered_market_rows();
        ui.label(format!(
            "Showing {} of {} market rows",
            format_count(rows.len()),
            format_count(self.market_rows.len())
        ));
        if self.market_rows.is_empty() {
            ui.label("Waiting for Universalis market data.");
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("market_results_scroll")
            .auto_shrink([false, false])
            .max_height(ui.available_height())
            .show(ui, |ui| {
                for (idx, row) in rows.into_iter().take(700).enumerate() {
                    let selected = self.selected_item_id == Some(row.item_id);
                    let mut tags = Vec::new();
                    if row.recipe_id.is_some() {
                        tags.push("craftable");
                    }
                    if row.is_gathered {
                        tags.push("gatherable");
                    }
                    if row.npc_price.is_some() {
                        tags.push("NPC");
                    }
                    let title = if tags.is_empty() {
                        format!("#{} {}", format_count(idx + 1), row.item_name)
                    } else {
                        format!(
                            "#{} {} ({})",
                            format_count(idx + 1),
                            row.item_name,
                            tags.join(", ")
                        )
                    };
                    if ui.selectable_label(selected, title).clicked() {
                        self.select_market_row(&row);
                    }
                    ui.horizontal_wrapped(|ui| {
                        if let Some(profit) = row.profit {
                            ui.strong(format!("Profit {}", format_gil_signed(profit)));
                            ui.label(format!("Sell {}", format_gil(row.sell_price)));
                            if let Some(cost) = row.acquisition_cost {
                                ui.label(format!("Cost {}", format_gil(cost)));
                            }
                        } else if row.has_market_price && row.recipe_id.is_some() {
                            ui.strong("Profit n/a");
                            ui.label(format!("Sell {}", format_gil(row.sell_price)));
                            ui.label("Cost n/a");
                        } else if row.has_market_price {
                            ui.strong(format!("Unit {}", format_gil(row.sell_price)));
                        } else {
                            ui.strong("Market n/a");
                            if let Some(cost) = row.acquisition_cost {
                                ui.label(format!("Cost {}", format_gil(cost)));
                            }
                        }
                        if let Some(price) = row.npc_price {
                            ui.label(format!("NPC {}", format_gil(price)));
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if row.has_market_price {
                            ui.label(format!("Avg sale {}", format_gil(row.average_sale)));
                            ui.label(format!("{} sales/1d", format_count(row.recent_sales_1d)));
                            ui.label(format!("{} sales/2d", format_count(row.recent_sales_2d)));
                            ui.label(format!("{} sales/4d", format_count(row.recent_sales_4d)));
                            ui.label(format!("{} qty/4d", format_count(row.recent_quantity_4d)));
                            ui.label(format!("{:.2}/day", row.daily_velocity));
                        } else {
                            ui.label("Avg sale n/a");
                            ui.label("No Universalis price loaded");
                        }
                    });
                    if let Some(vendor) = row.npc_sources.first() {
                        ui.small(format!("NPC: {} - {}", vendor.seller, vendor.location));
                    }
                    if row.is_gathered {
                        if let Some(gather) = self
                            .gather_rows
                            .iter()
                            .find(|gather| gather.item_id == row.item_id)
                        {
                            ui.small(format!("Node: {}", gather.node_type));
                            ui.small(format!("Where: {}", gather.where_text));
                            if !gather.timed_windows.is_empty() {
                                ui.small(format!("Timed: {}", gather.timed_windows.join(", ")));
                            }
                        }
                    }
                    if let Some(drop_sources) = self.drop_sources.get(&row.item_id) {
                        if let Some(where_text) = drop_source_where_summary(drop_sources) {
                            ui.small(format!("Where: {where_text}"));
                        }
                    }
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

    fn filtered_market_rows(&self) -> Vec<MarketRow> {
        self.market_rows
            .iter()
            .filter(|row| self.item_matches_search(&row.item_name))
            .filter(|row| {
                self.applied_item_filter
                    .is_none_or(|item_id| row.item_id == item_id)
            })
            .filter(|row| {
                !self.applied_only_profitable || row.profit.is_some_and(|profit| profit > 0)
            })
            .filter(|row| !self.applied_only_recently_sold || row.recent_sales_2d > 4)
            .filter(|row| !self.applied_market_only_npc || row.npc_price.is_some())
            .filter(|row| {
                !self.applied_market_hide_craftable
                    || row.recipe_id.is_none()
                    || row.npc_price.is_some()
            })
            .filter(|row| {
                !self.applied_market_hide_gatherable || !row.is_gathered || row.npc_price.is_some()
            })
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
            if let Some(price) = self.market_prices.get(&row.item_id) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "Avg sale: {}",
                        format_gil(average_sale_display(price))
                    ));
                    ui.label(format!("{:.2} sales/day", price.daily_velocity));
                    ui.label(format!("{} sales/4d", format_count(row.recent_sales_4d)));
                    ui.label(format!("{} qty/4d", format_count(row.recent_quantity_4d)));
                });
            }
        } else if let Some(row) = self.gather_rows.iter().find(|row| row.item_id == item_id) {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Unit: {}", format_gil(row.sell_price)));
                ui.label(format!("Profit: {}", format_gil_signed(row.profit)));
            });
            if let Some(price) = self.market_prices.get(&row.item_id) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "Avg sale: {}",
                        format_gil(average_sale_display(price))
                    ));
                    ui.label(format!("{:.2} sales/day", price.daily_velocity));
                });
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("{} sales/1d", format_count(row.recent_sales_1d)));
                ui.label(format!("{} sales/2d", format_count(row.recent_sales_2d)));
                ui.label(format!("{} sales/4d", format_count(row.recent_sales_4d)));
                ui.label(format!("{} qty/4d", format_count(row.recent_quantity_4d)));
            });
            ui.small(format!("Node: {}", row.node_type));
            ui.small(format!("Where: {}", row.where_text));
            if !row.timed_windows.is_empty() {
                ui.small(format!("Timed: {}", row.timed_windows.join(", ")));
            }
        } else if let Some(row) = self.market_rows.iter().find(|row| row.item_id == item_id) {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Unit: {}", format_gil(row.sell_price)));
                ui.label(format!("Avg sale: {}", format_gil(row.average_sale)));
                if let Some(cost) = row.craft_cost {
                    ui.label(format!("Cost: {}", format_gil(cost)));
                }
                if let Some(profit) = row.profit {
                    ui.label(format!("Profit: {}", format_gil_signed(profit)));
                }
                if let Some(price) = row.npc_price {
                    ui.label(format!("NPC: {}", format_gil(price)));
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("{} sales/1d", format_count(row.recent_sales_1d)));
                ui.label(format!("{} sales/2d", format_count(row.recent_sales_2d)));
                ui.label(format!("{} sales/4d", format_count(row.recent_sales_4d)));
                ui.label(format!("{} qty/4d", format_count(row.recent_quantity_4d)));
            });
            for source in &row.npc_sources {
                ui.small(format!("NPC: {} - {}", source.seller, source.location));
            }
        }

        if let Some(drop_sources) = self.drop_sources.get(&item_id) {
            if !drop_sources.is_empty() {
                ui.separator();
                ui.label("Drops from");
                for source in drop_sources.iter().take(12) {
                    ui.small(drop_source_display(source));
                }
                if drop_sources.len() > 12 {
                    ui.small(format!("...and {} more sources", drop_sources.len() - 12));
                }
            }
        }

        if let Some(price) = self.market_prices.get(&item_id) {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Past week");
                ui.selectable_value(
                    &mut self.sales_graph_mode,
                    SalesGraphMode::Quantity,
                    SalesGraphMode::Quantity.label(),
                );
                ui.selectable_value(
                    &mut self.sales_graph_mode,
                    SalesGraphMode::Value,
                    SalesGraphMode::Value.label(),
                );
            });
            draw_sales_graph(ui, &price.daily_history, self.sales_graph_mode);
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
                            format_count(ingredient.amount)
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
                        format_count(initial_quality)
                    ));
                }
            }
        }

        ui.separator();
        ui.label("Cost tree");
        egui::Grid::new("cost_tree_grid")
            .striped(true)
            .num_columns(7)
            .show(ui, |ui| {
                ui.strong("Item");
                ui.strong("Qty");
                ui.strong("Market");
                ui.strong("NPC");
                ui.strong("Crafted");
                ui.strong("Chosen");
                ui.strong("Source");
                ui.end_row();

                for node in &self.selected_cost_breakdown {
                    let total_cost = node.unit_cost.saturating_mul(node.amount);
                    let name_width = (ui.available_width() * 0.28).clamp(120.0, 220.0);
                    let name_cell_size = egui::vec2(name_width, ui.spacing().interact_size.y);
                    if node.depth == 0 {
                        draw_cost_tree_item_cell(ui, node, name_cell_size, true);
                        ui.strong(format!("x{}", format_count(node.amount)));
                        ui.strong(cost_cell(node.market_unit_cost, node.amount));
                        ui.strong(cost_cell(node.npc_unit_cost, node.amount));
                        ui.strong(cost_cell(node.craft_unit_cost, node.amount));
                        ui.strong(format_gil(total_cost));
                        ui.strong(&node.source);
                    } else {
                        draw_cost_tree_item_cell(ui, node, name_cell_size, false);
                        ui.label(format!("x{}", format_count(node.amount)));
                        ui.label(cost_cell(node.market_unit_cost, node.amount));
                        ui.label(cost_cell(node.npc_unit_cost, node.amount));
                        ui.label(cost_cell(node.craft_unit_cost, node.amount));
                        ui.label(format_gil(total_cost));
                        ui.label(&node.source);
                    }
                    ui.end_row();
                }
            });

        if self
            .selected_cost_breakdown
            .iter()
            .any(|node| !node.npc_sources.is_empty())
        {
            ui.separator();
            ui.label("NPC sources");
            egui::Grid::new("npc_sources_grid")
                .striped(true)
                .num_columns(4)
                .show(ui, |ui| {
                    ui.strong("Item");
                    ui.strong("NPC cost");
                    ui.strong("Purchasable from");
                    ui.strong("Location");
                    ui.end_row();

                    for node in &self.selected_cost_breakdown {
                        for source in &node.npc_sources {
                            ui.label(&node.name);
                            ui.label(cost_cell(node.npc_unit_cost, node.amount));
                            ui.label(&source.seller);
                            ui.label(&source.location);
                            ui.end_row();
                        }
                    }
                });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppMode {
    Craft,
    Gather,
    Market,
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
    RecentQuantity4d,
}

impl RankingMode {
    fn label(self) -> &'static str {
        match self {
            RankingMode::Profit => "Profit",
            RankingMode::RecentSales4d => "Times sold in past 4 days",
            RankingMode::RecentQuantity4d => "Quantity sold in past 4 days",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SalesGraphMode {
    Quantity,
    Value,
}

impl SalesGraphMode {
    fn label(self) -> &'static str {
        match self {
            SalesGraphMode::Quantity => "Quantity",
            SalesGraphMode::Value => "Value",
        }
    }
}

#[derive(Debug)]
enum WorkMessage<T> {
    Progress(String),
    Done(Result<T>),
}

#[derive(Debug, Clone)]
struct ActivityProgress {
    label: String,
    detail: String,
    current: usize,
    total: usize,
}

impl ActivityProgress {
    fn new(
        label: impl Into<String>,
        detail: impl Into<String>,
        current: usize,
        total: usize,
    ) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            current,
            total: total.max(1),
        }
    }

    fn fraction(&self) -> f32 {
        (self.current as f32 / self.total as f32).clamp(0.0, 1.0)
    }
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
    #[serde(default)]
    is_untradable: bool,
    vendor_price: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GameDataCache {
    categories: Vec<ItemCategory>,
    item_metadata: HashMap<u32, ItemMetadata>,
    vendor_prices: HashMap<u32, u32>,
    #[serde(default)]
    vendor_sources: HashMap<u32, Vec<VendorSourceInfo>>,
    #[serde(default)]
    drop_sources: HashMap<u32, Vec<DropSourceInfo>>,
    recipe_materials: HashMap<u32, Vec<RecipeMaterial>>,
    #[serde(default)]
    gathering_items: Vec<GatheringItemInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GameDataCacheManifest {
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
    recent_sales_1d: usize,
    #[serde(default)]
    recent_sales_2d: usize,
    #[serde(default)]
    recent_sales_4d: usize,
    #[serde(default)]
    recent_quantity_1d: u32,
    #[serde(default)]
    recent_quantity_2d: u32,
    #[serde(default)]
    recent_quantity_4d: u32,
    #[serde(default)]
    daily_history: Vec<DailySaleBucket>,
    metadata: ItemMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SaleHistory {
    recent_sales_1d: usize,
    recent_sales_2d: usize,
    recent_sales_4d: usize,
    recent_quantity_1d: u32,
    recent_quantity_2d: u32,
    recent_quantity_4d: u32,
    recent_sale_prices: Vec<u32>,
    daily_history: Vec<DailySaleBucket>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DailySaleBucket {
    days_ago: u8,
    sales: usize,
    quantity: u32,
    value: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct RecipeMaterial {
    item_id: u32,
    amount: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VendorSourceInfo {
    price: u32,
    npc_name: Option<String>,
    npc_title: Option<String>,
    shop_name: Option<String>,
    map_name: Option<String>,
    place_name: Option<String>,
    territory_name: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DropSourceInfo {
    item_id: u32,
    item_name: Option<String>,
    source_type: String,
    source_name: String,
    location: Option<String>,
    duty_name: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    level_range: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct NpcInfo {
    id: u32,
    name: String,
    title: Option<String>,
    #[serde(default)]
    map_name: Option<String>,
    #[serde(default)]
    place_name: Option<String>,
    #[serde(default)]
    territory_name: Option<String>,
    #[serde(default)]
    x: Option<f64>,
    #[serde(default)]
    y: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ShopInfo {
    name: String,
    #[serde(default)]
    npc_ids: Vec<u32>,
    listings: Vec<ShopListingInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ShopListingInfo {
    rewards: Vec<ShopItemInfo>,
    costs: Vec<ShopItemInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ShopItemInfo {
    item_id: u32,
    count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GatheringItemInfo {
    item_id: u32,
    item_name: Option<String>,
    #[serde(default)]
    locations: Vec<GatheringLocationInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GatheringLocationInfo {
    gathering_point_id: Option<u32>,
    node_type: String,
    folklore_book: Option<String>,
    level: Option<u32>,
    place_id: Option<u32>,
    place_name: Option<String>,
    territory_id: Option<u32>,
    #[serde(default)]
    timed_windows: Vec<String>,
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
    recent_sales_1d: usize,
    recent_sales_2d: usize,
    recent_sales_4d: usize,
    recent_quantity_4d: u32,
}

#[derive(Debug, Clone)]
struct GatherRow {
    item_id: u32,
    recipe_id: Option<u32>,
    item_name: String,
    search_category_id: Option<u32>,
    sell_price: u32,
    profit: i64,
    daily_velocity: f64,
    recent_sales_1d: usize,
    recent_sales_2d: usize,
    recent_sales_4d: usize,
    recent_quantity_1d: u32,
    recent_quantity_2d: u32,
    recent_quantity_4d: u32,
    node_type: String,
    where_text: String,
    timed_windows: Vec<String>,
}

#[derive(Debug, Clone)]
struct MarketRow {
    item_id: u32,
    recipe_id: Option<u32>,
    item_name: String,
    category_name: Option<String>,
    search_category_id: Option<u32>,
    has_market_price: bool,
    sell_price: u32,
    average_sale: u32,
    acquisition_cost: Option<u32>,
    craft_cost: Option<u32>,
    profit: Option<i64>,
    npc_price: Option<u32>,
    npc_sources: Vec<VendorSourceDisplay>,
    is_gathered: bool,
    daily_velocity: f64,
    recent_sales_1d: usize,
    recent_sales_2d: usize,
    recent_sales_4d: usize,
    recent_quantity_4d: u32,
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
    market_unit_cost: Option<u32>,
    npc_unit_cost: Option<u32>,
    craft_unit_cost: Option<u32>,
    npc_sources: Vec<VendorSourceDisplay>,
    source: String,
}

#[derive(Debug, Clone)]
struct VendorSourceDisplay {
    seller: String,
    location: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryEntry {
    timestamp: i64,
    #[serde(rename = "pricePerUnit")]
    price_per_unit: u32,
    #[serde(default = "default_sale_quantity")]
    quantity: u32,
}

fn default_sale_quantity() -> u32 {
    1
}

fn fetch_market_snapshot(progress: &dyn Fn(String)) -> Result<MarketSnapshot> {
    progress("Building HTTP client for Universalis.".to_string());
    let client = reqwest::blocking::Client::builder()
        .user_agent("FortunaProfitCrafter/0.1")
        .build()?;
    let game_data_cache = load_game_data_cache_for_market(progress);
    let saintcoinach_ids = saintcoinach_market_item_ids_from_cache(&game_data_cache);
    let ids = if saintcoinach_ids.is_empty() {
        fetch_marketable_item_ids(&client, progress).unwrap_or_else(|err| {
            progress(format!(
                "Could not load full Universalis marketable item list: {err:#}. Falling back to known craft and material IDs."
            ));
            relevant_market_item_ids()
        })
    } else {
        progress(format!(
            "Using {} SaintCoinach market candidate item IDs.",
            saintcoinach_ids.len()
        ));
        saintcoinach_ids
    };
    let ids = market_lookup_item_ids(ids, progress);
    progress(format!("Checking {} possible market item IDs.", ids.len()));
    let mut prices = fetch_universalis_market_prices(&client, &ids, progress)?;
    progress(format!(
        "Loaded current prices for {} market items.",
        prices.len()
    ));
    let market_item_ids = sorted_market_item_ids(&prices);
    let sales_history = fetch_sales_history(&client, &market_item_ids, progress)?;

    progress("Merging item categories, vendor prices, and sale counts.".to_string());
    for (item_id, price) in &mut prices {
        price.metadata = game_data_cache
            .item_metadata
            .get(item_id)
            .cloned()
            .unwrap_or_default();
        if let Some(vendor_price) = game_data_cache
            .vendor_prices
            .get(item_id)
            .copied()
            .filter(|&price| is_available_vendor_price(price))
        {
            price.metadata.vendor_price = Some(vendor_price);
        }
    }

    for (item_id, history) in sales_history {
        if let Some(price) = prices.get_mut(&item_id) {
            price.recent_sales_1d = history.recent_sales_1d;
            price.recent_sales_2d = history.recent_sales_2d;
            price.recent_sales_4d = history.recent_sales_4d;
            price.recent_quantity_1d = history.recent_quantity_1d;
            price.recent_quantity_2d = history.recent_quantity_2d;
            price.recent_quantity_4d = history.recent_quantity_4d;
            price.recent_sale_prices = history.recent_sale_prices;
            price.daily_history = history.daily_history;
        }
    }

    Ok(MarketSnapshot {
        prices,
        categories: game_data_cache.categories,
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
        recent_sales_1d: 0,
        recent_sales_2d: 0,
        recent_sales_4d: 0,
        recent_quantity_1d: 0,
        recent_quantity_2d: 0,
        recent_quantity_4d: 0,
        daily_history: Vec::new(),
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

fn fetch_marketable_item_ids(
    client: &reqwest::blocking::Client,
    progress: &dyn Fn(String),
) -> Result<Vec<u32>> {
    progress("Loading full Universalis marketable item list.".to_string());
    let response = client
        .get("https://universalis.app/items")
        .send()?
        .error_for_status()?;
    let html = response.text()?;
    let ids = parse_marketable_item_ids(&html);
    if ids.is_empty() {
        return Err(anyhow!("Universalis returned no marketable item IDs"));
    }
    save_marketable_item_ids(&ids, progress);
    progress(format!(
        "Loaded {} marketable item IDs from Universalis.",
        ids.len()
    ));
    Ok(ids)
}

fn parse_marketable_item_ids(html: &str) -> Vec<u32> {
    let mut ids = html
        .split("Item ")
        .skip(1)
        .filter_map(|part| {
            let digits = part
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            (!digits.is_empty())
                .then(|| digits.parse::<u32>().ok())
                .flatten()
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn load_marketable_item_ids() -> Result<Vec<u32>> {
    read_json_file(&marketable_item_ids_path())
}

fn market_lookup_item_ids(ids: Vec<u32>, progress: &dyn Fn(String)) -> Vec<u32> {
    let before = ids.len();
    let ids = ids
        .into_iter()
        .filter(|&item_id| !is_collectable_item(item_id))
        .collect::<Vec<_>>();
    let skipped = before.saturating_sub(ids.len());
    if skipped > 0 {
        progress(format!(
            "Skipping {} collectable item IDs for Universalis lookup.",
            skipped
        ));
    }
    ids
}

fn market_refresh_preview_item_ids() -> Vec<u32> {
    load_game_data_cache()
        .map(|(cache, _)| saintcoinach_market_item_ids_from_cache(&cache))
        .ok()
        .filter(|ids| !ids.is_empty())
        .or_else(|| load_marketable_item_ids().ok())
        .unwrap_or_else(relevant_market_item_ids)
}

fn saintcoinach_market_item_ids_from_cache(cache: &GameDataCache) -> Vec<u32> {
    let mut ids = cache
        .item_metadata
        .iter()
        .filter_map(|(&item_id, metadata)| {
            is_saintcoinach_market_candidate(metadata).then_some(item_id)
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn is_saintcoinach_market_candidate(metadata: &ItemMetadata) -> bool {
    metadata.search_category_id.is_some() && !metadata.is_untradable
}

fn is_collectable_item(item_id: u32) -> bool {
    ITEMS
        .get(item_id)
        .is_some_and(|item| item.always_collectable)
}

fn save_marketable_item_ids(ids: &[u32], progress: &dyn Fn(String)) {
    if let Some(parent) = marketable_item_ids_path().parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            progress(format!(
                "Could not create marketable item cache directory: {err:#}"
            ));
            return;
        }
    }
    if let Err(err) = write_json_file(&marketable_item_ids_path(), &ids) {
        progress(format!("Could not save marketable item ID cache: {err:#}"));
    }
    if let Err(err) = write_marketable_item_ids_csv(ids) {
        progress(format!(
            "Could not save marketable item ID CSV cache: {err:#}"
        ));
    }
}

fn marketable_item_ids_path() -> PathBuf {
    universalis_cache_dir().join("marketable_item_ids.json")
}

fn marketable_item_ids_csv_path() -> PathBuf {
    universalis_cache_dir().join("marketable_item_ids.csv")
}

fn write_marketable_item_ids_csv(ids: &[u32]) -> Result<()> {
    let rows = std::iter::once("item_id".to_string())
        .chain(ids.iter().map(u32::to_string))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(marketable_item_ids_csv_path(), rows)
        .context("failed to write marketable_item_ids.csv")
}

fn sorted_market_item_ids(prices: &HashMap<u32, MarketPrice>) -> Vec<u32> {
    let mut item_ids = prices.keys().copied().collect::<Vec<_>>();
    item_ids.sort_unstable();
    item_ids
}

fn load_game_data_cache_for_market(progress: &dyn Fn(String)) -> GameDataCache {
    match load_game_data_cache() {
        Ok((cache, _)) => {
            progress(format!(
                "Using existing SaintCoinach cache: {} item types, {} items, {} vendor prices, {} recipe material lists.",
                cache.categories.len(),
                cache.item_metadata.len(),
                cache.vendor_prices.len(),
                cache.recipe_materials.len()
            ));
            cache
        }
        Err(err) => {
            progress(format!(
                "No SaintCoinach cache loaded for this market refresh: {err:#}. Use Update SaintCoinach cache separately."
            ));
            GameDataCache::default()
        }
    }
}

fn load_game_data_cache() -> Result<(GameDataCache, GameDataCacheManifest)> {
    let dir = saintcoinach_cache_dir();
    let manifest = read_json_file(&dir.join("cache_manifest.json"))?;
    Ok((load_game_data_cache_files(&dir)?, manifest))
}

fn load_game_data_cache_files(dir: &Path) -> Result<GameDataCache> {
    let categories = read_json_file(&dir.join("item_categories.json"))?;
    let item_metadata = read_json_file(&dir.join("item_metadata.json"))?;
    let vendor_prices = read_json_file::<HashMap<u32, u32>>(&dir.join("vendor_prices.json"))?
        .into_iter()
        .filter(|(_, price)| is_available_vendor_price(*price))
        .collect::<HashMap<_, _>>();
    let npcs: Vec<NpcInfo> = read_json_file(&dir.join("npcs.json")).unwrap_or_default();
    let shops: Vec<ShopInfo> = read_json_file(&dir.join("shops.json")).unwrap_or_default();
    let vendor_sources = read_json_file(&dir.join("vendor_sources.json"))
        .unwrap_or_else(|_| derive_vendor_sources(&vendor_prices, &npcs, &shops));
    let drop_sources = read_json_file(&dir.join("drop_sources.json")).unwrap_or_default();
    let recipe_materials = read_json_file(&dir.join("recipe_materials.json")).unwrap_or_default();
    let gathering_items = read_json_file(&dir.join("gathering_items.json")).unwrap_or_default();
    Ok(GameDataCache {
        categories,
        item_metadata,
        vendor_prices,
        vendor_sources,
        drop_sources,
        recipe_materials,
        gathering_items,
    })
}

fn refresh_saintcoinach_data(progress: &dyn Fn(String)) -> Result<GameDataCache> {
    let item_ids = full_marketable_item_ids_for_metadata(progress);
    progress(format!(
        "Refreshing SaintCoinach game data for {} marketable item IDs.",
        item_ids.len()
    ));
    refresh_saintcoinach_data_with_item_ids(item_ids, progress)
}

fn full_marketable_item_ids_for_metadata(progress: &dyn Fn(String)) -> Vec<u32> {
    load_marketable_item_ids().unwrap_or_else(|_| {
        let client = reqwest::blocking::Client::builder()
            .user_agent("FortunaProfitCrafter/0.1")
            .build();
        match client
            .ok()
            .and_then(|client| fetch_marketable_item_ids(&client, progress).ok())
        {
            Some(ids) => ids,
            None => {
                progress(
                    "Using known craft and material IDs because the full Universalis item list is unavailable."
                        .to_string(),
                );
                relevant_market_item_ids()
            }
        }
    })
}

fn refresh_saintcoinach_data_with_item_ids(
    item_ids: Vec<u32>,
    progress: &dyn Fn(String),
) -> Result<GameDataCache> {
    progress(format!(
        "Exporting SaintCoinach metadata for {} item IDs.",
        item_ids.len()
    ));
    export_saintcoinach_cache(&item_ids, progress)
        .context("failed to export SaintCoinach game data")?;
    let cache = load_game_data_cache_files(&saintcoinach_cache_dir())?;
    progress(format!(
        "Derived relevant NPC vendor prices for {} items and recipe materials for {} crafted outputs.",
        cache.vendor_prices.len(),
        cache.recipe_materials.len()
    ));
    save_game_data_cache(&cache, &game_data_cache_manifest(&item_ids))?;
    Ok(cache)
}

fn save_game_data_cache(cache: &GameDataCache, manifest: &GameDataCacheManifest) -> Result<()> {
    let dir = saintcoinach_cache_dir();
    fs::create_dir_all(&dir)?;
    write_json_file(&dir.join("cache_manifest.json"), manifest)?;
    write_json_file(&dir.join("item_categories.json"), &cache.categories)?;
    write_json_file(&dir.join("item_metadata.json"), &cache.item_metadata)?;
    write_json_file(&dir.join("vendor_prices.json"), &cache.vendor_prices)?;
    write_json_file(&dir.join("vendor_sources.json"), &cache.vendor_sources)?;
    write_json_file(&dir.join("drop_sources.json"), &cache.drop_sources)?;
    write_json_file(&dir.join("recipe_materials.json"), &cache.recipe_materials)?;
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LodestoneDropRefreshState {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    checked_item_ids: HashSet<u32>,
    #[serde(default)]
    checked_duty_paths: HashSet<String>,
}

#[derive(Debug, Clone, Default)]
struct LodestoneDropRefreshSummary {
    candidate_count: usize,
    checked_count: usize,
    duty_count: usize,
    checked_duty_count: usize,
    new_source_count: usize,
}

#[derive(Debug, Clone)]
struct LodestoneItemCandidate {
    item_id: u32,
    item_name: String,
}

#[derive(Debug, Clone)]
struct LodestoneDutyCandidate {
    path: String,
    name: String,
}

fn refresh_lodestone_drop_sources(
    progress: &dyn Fn(String),
) -> Result<LodestoneDropRefreshSummary> {
    let (mut cache, manifest) = load_game_data_cache()?;
    fs::create_dir_all(lodestone_cache_dir())?;
    fs::create_dir_all(lodestone_raw_cache_dir())?;
    let state_path = lodestone_cache_dir().join("drop_refresh_state.json");
    let mut state = read_json_file::<LodestoneDropRefreshState>(&state_path).unwrap_or_default();
    if state.version != LODESTONE_DROP_REFRESH_STATE_VERSION {
        progress(format!(
            "Lodestone candidate rules changed; resetting checked item state from v{} to v{}.",
            state.version, LODESTONE_DROP_REFRESH_STATE_VERSION
        ));
        state = LodestoneDropRefreshState {
            version: LODESTONE_DROP_REFRESH_STATE_VERSION,
            checked_item_ids: HashSet::new(),
            checked_duty_paths: state.checked_duty_paths,
        };
        write_json_file(&state_path, &state)?;
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent("FortunaProfitCrafter/0.1 polite cached Lodestone source lookup")
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build Lodestone HTTP client")?;

    let mut new_source_count = 0;

    let duties = lodestone_duty_candidates(&client, progress).unwrap_or_else(|err| {
        progress(format!("Lodestone duty list skipped: {err:#}"));
        Vec::new()
    });
    let duty_batch = duties
        .iter()
        .filter(|duty| !state.checked_duty_paths.contains(&duty.path))
        .take(LODESTONE_REFRESH_BATCH_DUTIES)
        .cloned()
        .collect::<Vec<_>>();
    let item_ids_by_name = lodestone_sourceable_item_ids_by_name(&cache);

    for (index, duty) in duty_batch.iter().enumerate() {
        progress(format!(
            "Lodestone duty page ({}/{}): {}",
            index + 1,
            duty_batch.len(),
            duty.name
        ));
        match fetch_lodestone_duty_sources(&client, duty, &item_ids_by_name, progress) {
            Ok(sources) => {
                for source in sources {
                    let item_sources = cache.drop_sources.entry(source.item_id).or_default();
                    if !item_sources
                        .iter()
                        .any(|existing| drop_source_key(existing) == drop_source_key(&source))
                    {
                        item_sources.push(source);
                        item_sources.sort_by(|left, right| {
                            drop_source_display(left).cmp(&drop_source_display(right))
                        });
                        new_source_count += 1;
                    }
                }
                state.checked_duty_paths.insert(duty.path.clone());
                write_json_file(&state_path, &state)?;
                write_json_file(
                    &saintcoinach_cache_dir().join("drop_sources.json"),
                    &cache.drop_sources,
                )?;
            }
            Err(err) => {
                progress(format!(
                    "Lodestone duty lookup skipped {}: {err:#}",
                    duty.name
                ));
            }
        }
    }

    let candidates = lodestone_drop_candidates(&cache);
    progress(format!("Lodestone candidate items: {}", candidates.len()));
    let unchecked_candidates = candidates
        .iter()
        .filter(|candidate| !state.checked_item_ids.contains(&candidate.item_id))
        .cloned()
        .collect::<Vec<_>>();
    let mut checked_count = 0;
    let mut failed_count = 0;

    if unchecked_candidates.is_empty() {
        progress(
            "Lodestone refresh has no unchecked item candidates in this filtered set.".to_string(),
        );
        write_json_file(&state_path, &state)?;
    }

    for (batch_index, batch) in unchecked_candidates
        .chunks(LODESTONE_REFRESH_BATCH_ITEMS)
        .enumerate()
    {
        progress(format!(
            "Lodestone item batch {}: checking {} item page(s).",
            batch_index + 1,
            batch.len()
        ));
        for candidate in batch {
            progress(format!(
                "Lodestone item page ({}/{}): {}",
                checked_count + failed_count + 1,
                unchecked_candidates.len(),
                candidate.item_name
            ));

            match fetch_lodestone_item_sources(&client, candidate, progress) {
                Ok(sources) => {
                    let item_sources = cache.drop_sources.entry(candidate.item_id).or_default();
                    let mut seen = item_sources
                        .iter()
                        .map(drop_source_key)
                        .collect::<HashSet<_>>();
                    for source in sources {
                        if seen.insert(drop_source_key(&source)) {
                            item_sources.push(source);
                            new_source_count += 1;
                        }
                    }
                    item_sources.sort_by(|left, right| {
                        drop_source_display(left).cmp(&drop_source_display(right))
                    });
                    state.checked_item_ids.insert(candidate.item_id);
                    checked_count += 1;
                    write_json_file(&state_path, &state)?;
                    write_json_file(
                        &saintcoinach_cache_dir().join("drop_sources.json"),
                        &cache.drop_sources,
                    )?;
                }
                Err(err) => {
                    failed_count += 1;
                    progress(format!(
                        "Lodestone lookup skipped {}: {err:#}",
                        candidate.item_name
                    ));
                }
            }
        }
    }

    if failed_count > 0 {
        progress(format!(
            "Lodestone item lookup finished with {} failed item page(s); they were not marked checked.",
            failed_count
        ));
    }

    save_game_data_cache(&cache, &manifest)?;
    Ok(LodestoneDropRefreshSummary {
        candidate_count: candidates.len(),
        checked_count,
        duty_count: duties.len(),
        checked_duty_count: duty_batch.len(),
        new_source_count,
    })
}

fn lodestone_drop_candidates(cache: &GameDataCache) -> Vec<LodestoneItemCandidate> {
    let candidate_item_ids = lodestone_locationless_market_item_ids(cache);
    let mut candidates = candidate_item_ids
        .into_iter()
        .filter(|item_id| {
            !cache
                .drop_sources
                .get(item_id)
                .is_some_and(|sources| !sources.is_empty())
        })
        .filter_map(|item_id| {
            let metadata = cache.item_metadata.get(&item_id)?;
            let item_name = metadata.name.clone().unwrap_or_else(|| item_name(item_id));
            (!item_name.trim().is_empty()).then_some(LodestoneItemCandidate { item_id, item_name })
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| left.item_id.cmp(&right.item_id));
    candidates
}

fn lodestone_locationless_market_item_ids(cache: &GameDataCache) -> HashSet<u32> {
    let crafted_outputs = cache
        .recipe_materials
        .keys()
        .copied()
        .chain(RECIPES.values.iter().map(|recipe| recipe.item_id))
        .collect::<HashSet<_>>();
    let gathered = cache
        .gathering_items
        .iter()
        .map(|item| item.item_id)
        .collect::<HashSet<_>>();

    cache
        .item_metadata
        .iter()
        .filter_map(|(&item_id, metadata)| {
            if !is_saintcoinach_market_candidate(metadata)
                || crafted_outputs.contains(&item_id)
                || gathered.contains(&item_id)
                || cache.vendor_sources.contains_key(&item_id)
            {
                return None;
            }
            Some(item_id)
        })
        .collect()
}

fn lodestone_sourceable_item_ids_by_name(cache: &GameDataCache) -> HashMap<String, u32> {
    let mut counts = HashMap::<String, usize>::new();
    let mut values = HashMap::<String, u32>::new();
    for item_id in lodestone_locationless_market_item_ids(cache) {
        let item_name = cache
            .item_metadata
            .get(&item_id)
            .and_then(|metadata| metadata.name.clone())
            .unwrap_or_else(|| item_name(item_id));
        let key = item_name.to_ascii_lowercase();
        *counts.entry(key.clone()).or_default() += 1;
        values.insert(key, item_id);
    }
    values
        .into_iter()
        .filter(|(name, _)| counts.get(name).copied().unwrap_or_default() == 1)
        .collect()
}

fn fetch_lodestone_item_sources(
    client: &reqwest::blocking::Client,
    candidate: &LodestoneItemCandidate,
    progress: &dyn Fn(String),
) -> Result<Vec<DropSourceInfo>> {
    let search_html = lodestone_cached_get(
        client,
        &format!(
            "{LODESTONE_BASE_URL}/lodestone/playguide/db/search/?q={}",
            url_encode_component(&candidate.item_name)
        ),
        &lodestone_raw_cache_dir().join(format!("item_{}_search.html", candidate.item_id)),
        progress,
    )?;

    let Some(item_path) = lodestone_search_item_path(&search_html, &candidate.item_name) else {
        return Ok(Vec::new());
    };

    let item_html = lodestone_cached_get(
        client,
        &format!("{LODESTONE_BASE_URL}{item_path}"),
        &lodestone_raw_cache_dir().join(format!("item_{}.html", candidate.item_id)),
        progress,
    )?;

    Ok(parse_lodestone_item_drop_sources(candidate, &item_html))
}

fn lodestone_duty_candidates(
    client: &reqwest::blocking::Client,
    progress: &dyn Fn(String),
) -> Result<Vec<LodestoneDutyCandidate>> {
    let mut duties = Vec::new();
    let mut seen = HashSet::new();
    for category_id in LODESTONE_DUTY_CATEGORY_IDS {
        let html = lodestone_cached_get(
            client,
            &format!("{LODESTONE_BASE_URL}/lodestone/playguide/db/duty/?category2={category_id}"),
            &lodestone_raw_cache_dir().join(format!("duty_category_{category_id}.html")),
            progress,
        )?;
        for duty in parse_lodestone_duty_links(&html) {
            if seen.insert(duty.path.clone()) {
                duties.push(duty);
            }
        }
    }
    duties.sort_by(|left, right| left.name.cmp(&right.name).then(left.path.cmp(&right.path)));
    Ok(duties)
}

fn fetch_lodestone_duty_sources(
    client: &reqwest::blocking::Client,
    duty: &LodestoneDutyCandidate,
    item_ids_by_name: &HashMap<String, u32>,
    progress: &dyn Fn(String),
) -> Result<Vec<DropSourceInfo>> {
    let html = lodestone_cached_get(
        client,
        &format!("{LODESTONE_BASE_URL}{}", duty.path),
        &lodestone_raw_cache_dir().join(format!(
            "duty_{}.html",
            duty.path
                .trim_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("unknown")
        )),
        progress,
    )?;
    Ok(parse_lodestone_duty_drop_sources(
        duty,
        item_ids_by_name,
        &html,
    ))
}

fn lodestone_cached_get(
    client: &reqwest::blocking::Client,
    url: &str,
    cache_path: &Path,
    progress: &dyn Fn(String),
) -> Result<String> {
    if cache_path.exists() {
        return fs::read_to_string(cache_path)
            .with_context(|| format!("failed to read {}", cache_path.display()));
    }

    progress(format!(
        "Lodestone safe delay: waiting {LODESTONE_REQUEST_DELAY_SECS}s before fetching {url}"
    ));
    thread::sleep(Duration::from_secs(LODESTONE_REQUEST_DELAY_SECS));

    let response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("Lodestone returned an error for {url}"))?;
    let html = response
        .text()
        .with_context(|| format!("failed to read Lodestone response for {url}"))?;
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(cache_path, &html)
        .with_context(|| format!("failed to write {}", cache_path.display()))?;
    Ok(html)
}

fn lodestone_search_item_path(html: &str, item_name: &str) -> Option<String> {
    let marker = "class=\"db_popup db-table__txt--detail_link\"";
    let mut offset = 0;
    while let Some(relative) = html[offset..].find(marker) {
        let marker_index = offset + relative;
        let anchor_start = html[..marker_index].rfind("<a ")?;
        let anchor_end = html[marker_index..].find("</a>")? + marker_index;
        let anchor = &html[anchor_start..anchor_end];
        let name = html_text(anchor).trim().to_string();
        if name.eq_ignore_ascii_case(item_name.trim()) {
            return html_attr(anchor, "href");
        }
        offset = anchor_end + 4;
    }
    None
}

fn parse_lodestone_duty_links(html: &str) -> Vec<LodestoneDutyCandidate> {
    let marker = "/lodestone/playguide/db/duty/";
    let mut duties = Vec::new();
    let mut offset = 0;
    while let Some(relative) = html[offset..].find(marker) {
        let href_start = offset + relative;
        let Some(anchor_start) = html[..href_start].rfind("<a ") else {
            offset = href_start + marker.len();
            continue;
        };
        let Some(anchor_end_relative) = html[href_start..].find("</a>") else {
            break;
        };
        let anchor_end = href_start + anchor_end_relative;
        let anchor = &html[anchor_start..anchor_end];
        let Some(path) = html_attr(anchor, "href") else {
            offset = anchor_end + 4;
            continue;
        };
        let path = path.split('?').next().unwrap_or_default().to_string();
        if !path.starts_with(marker)
            || path == marker
            || !path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .is_some_and(|id| id.len() >= 8 && id.chars().all(|ch| ch.is_ascii_hexdigit()))
        {
            offset = anchor_end + 4;
            continue;
        }
        let name = html_text(anchor).trim().to_string();
        if !name.is_empty() {
            duties.push(LodestoneDutyCandidate { path, name });
        }
        offset = anchor_end + 4;
    }
    duties
}

fn parse_lodestone_item_drop_sources(
    candidate: &LodestoneItemCandidate,
    html: &str,
) -> Vec<DropSourceInfo> {
    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for table in lodestone_source_tables(html) {
        let headings = lodestone_table_headings(table);
        if headings.iter().any(|heading| heading == "Dropped By") {
            for row in lodestone_table_rows(table) {
                let cells = lodestone_table_cells(row);
                if cells.len() < 2 {
                    continue;
                }
                let source_name = html_text(&cells[0]).trim().to_string();
                if source_name.is_empty() {
                    continue;
                }
                let area_text = html_text(&cells[1]);
                let (location, x, y) = parse_lodestone_location(&area_text);
                let source = DropSourceInfo {
                    item_id: candidate.item_id,
                    item_name: Some(candidate.item_name.clone()),
                    source_type: "Mob".to_string(),
                    source_name,
                    location,
                    duty_name: None,
                    x,
                    y,
                    level_range: None,
                };
                if seen.insert(drop_source_key(&source)) {
                    sources.push(source);
                }
            }
        } else if headings.iter().any(|heading| {
            heading.contains("Duty") || heading.contains("Treasure") || heading.contains("Coffer")
        }) {
            for row in lodestone_table_rows(table) {
                let cells = lodestone_table_cells(row);
                let source_name = cells
                    .iter()
                    .map(|cell| html_text(cell).trim().to_string())
                    .find(|text| !text.is_empty())
                    .unwrap_or_default();
                if source_name.is_empty() {
                    continue;
                }
                let source = DropSourceInfo {
                    item_id: candidate.item_id,
                    item_name: Some(candidate.item_name.clone()),
                    source_type: "Treasure".to_string(),
                    source_name: "Treasure Coffer".to_string(),
                    location: None,
                    duty_name: Some(source_name),
                    x: None,
                    y: None,
                    level_range: None,
                };
                if seen.insert(drop_source_key(&source)) {
                    sources.push(source);
                }
            }
        }
    }
    sources
}

fn parse_lodestone_duty_drop_sources(
    duty: &LodestoneDutyCandidate,
    item_ids_by_name: &HashMap<String, u32>,
    html: &str,
) -> Vec<DropSourceInfo> {
    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for popup in lodestone_treasure_box_popups(html) {
        for item_name in lodestone_treasure_item_names(popup) {
            let Some(&item_id) = item_ids_by_name.get(&item_name.to_ascii_lowercase()) else {
                continue;
            };
            let source = DropSourceInfo {
                item_id,
                item_name: Some(item_name),
                source_type: "Treasure".to_string(),
                source_name: "Treasure Coffer".to_string(),
                location: None,
                duty_name: Some(duty.name.clone()),
                x: None,
                y: None,
                level_range: None,
            };
            if seen.insert(drop_source_key(&source)) {
                sources.push(source);
            }
        }
    }
    sources
}

fn lodestone_treasure_box_popups(html: &str) -> Vec<&str> {
    let mut popups = Vec::new();
    let mut offset = 0;
    while let Some(relative) = html[offset..].find("db-view__treasure_box_popup") {
        let marker_index = offset + relative;
        let Some(popup_start) = html[..marker_index].rfind("<div") else {
            offset = marker_index + 1;
            continue;
        };
        let next_popup = html[marker_index + 1..]
            .find("db-view__treasure_box_popup")
            .map(|next| marker_index + 1 + next)
            .unwrap_or(html.len());
        popups.push(&html[popup_start..next_popup]);
        offset = next_popup;
    }
    popups
}

fn lodestone_treasure_item_names(popup: &str) -> Vec<String> {
    let mut names = Vec::new();
    let marker = "/lodestone/playguide/db/item/";
    let mut offset = 0;
    while let Some(relative) = popup[offset..].find(marker) {
        let href_start = offset + relative;
        let Some(anchor_start) = popup[..href_start].rfind("<a ") else {
            offset = href_start + marker.len();
            continue;
        };
        let Some(anchor_end_relative) = popup[href_start..].find("</a>") else {
            break;
        };
        let anchor_end = href_start + anchor_end_relative;
        let anchor = &popup[anchor_start..anchor_end];
        let name = html_text(anchor).trim().to_string();
        if !name.is_empty() && !names.iter().any(|existing| existing == &name) {
            names.push(name);
        }
        offset = anchor_end + 4;
    }
    names
}

fn lodestone_source_tables(html: &str) -> Vec<&str> {
    let mut tables = Vec::new();
    let mut offset = 0;
    while let Some(relative) = html[offset..].find("db-table__item_source") {
        let marker_index = offset + relative;
        let Some(table_start) = html[..marker_index].rfind("<table") else {
            offset = marker_index + 1;
            continue;
        };
        let Some(table_end_relative) = html[marker_index..].find("</table>") else {
            break;
        };
        let table_end = marker_index + table_end_relative + "</table>".len();
        tables.push(&html[table_start..table_end]);
        offset = table_end;
    }
    tables
}

fn lodestone_table_headings(table: &str) -> Vec<String> {
    lodestone_tag_contents(table, "th")
        .into_iter()
        .map(|cell| html_text(cell).trim().to_string())
        .filter(|text| !text.is_empty())
        .collect()
}

fn lodestone_table_rows(table: &str) -> Vec<&str> {
    lodestone_tag_contents(table, "tr")
}

fn lodestone_table_cells(row: &str) -> Vec<&str> {
    lodestone_tag_contents(row, "td")
}

fn lodestone_tag_contents<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
    let open_prefix = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut values = Vec::new();
    let mut offset = 0;
    while let Some(open_relative) = html[offset..].find(&open_prefix) {
        let open_start = offset + open_relative;
        let Some(open_end_relative) = html[open_start..].find('>') else {
            break;
        };
        let content_start = open_start + open_end_relative + 1;
        let Some(close_relative) = html[content_start..].find(&close) else {
            break;
        };
        let content_end = content_start + close_relative;
        values.push(&html[content_start..content_end]);
        offset = content_end + close.len();
    }
    values
}

fn parse_lodestone_location(text: &str) -> (Option<String>, Option<f64>, Option<f64>) {
    let collapsed = collapse_whitespace(text).replace(" Other Locations", "");
    let x = parse_after_marker(&collapsed, "X:");
    let y = parse_after_marker(&collapsed, "Y:");
    let location = collapsed
        .split("(X:")
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    (location, x, y)
}

fn parse_after_marker(text: &str, marker: &str) -> Option<f64> {
    let start = text.find(marker)? + marker.len();
    let value = text[start..]
        .chars()
        .skip_while(|ch| ch.is_whitespace())
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect::<String>();
    value.parse().ok()
}

fn html_attr(html: &str, attr: &str) -> Option<String> {
    let marker = format!("{attr}=\"");
    let start = html.find(&marker)? + marker.len();
    let end = html[start..].find('"')? + start;
    Some(html_entity_decode(&html[start..end]))
}

fn html_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    html_entity_decode(&collapse_whitespace(&text))
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn html_entity_decode(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&ndash;", "-")
        .replace("&mdash;", "-")
}

fn url_encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            b' ' => encoded.push('+'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn drop_source_key(source: &DropSourceInfo) -> String {
    format!(
        "{}|{}|{}|{}|{}|{:.1}|{:.1}",
        source.item_id,
        source.source_type,
        source.source_name,
        source.location.as_deref().unwrap_or_default(),
        source.duty_name.as_deref().unwrap_or_default(),
        source.x.unwrap_or_default(),
        source.y.unwrap_or_default()
    )
}

fn derive_vendor_sources(
    vendor_prices: &HashMap<u32, u32>,
    npcs: &[NpcInfo],
    shops: &[ShopInfo],
) -> HashMap<u32, Vec<VendorSourceInfo>> {
    let npc_by_id = npcs
        .iter()
        .map(|npc| (npc.id, npc))
        .collect::<HashMap<_, _>>();
    let mut sources: HashMap<u32, Vec<VendorSourceInfo>> = HashMap::new();

    for shop in shops {
        for listing in &shop.listings {
            let Some(price) = gil_listing_price(listing) else {
                continue;
            };
            for reward in &listing.rewards {
                if reward.item_id == 0 || reward.count == 0 {
                    continue;
                }
                let unit_price = price / reward.count.max(1);
                if !is_available_vendor_price(unit_price) {
                    continue;
                }
                if vendor_prices
                    .get(&reward.item_id)
                    .is_some_and(|&vendor_price| vendor_price != unit_price)
                {
                    continue;
                }

                let item_sources = sources.entry(reward.item_id).or_default();
                for npc_id in &shop.npc_ids {
                    if let Some(npc) = npc_by_id.get(npc_id).copied() {
                        push_vendor_source(item_sources, unit_price, shop, npc);
                    }
                }
            }
        }
    }

    for item_sources in sources.values_mut() {
        item_sources.sort_by(|a, b| vendor_source_label(a).cmp(&vendor_source_label(b)));
        item_sources.dedup_by(|a, b| vendor_source_label(a) == vendor_source_label(b));
    }

    sources
}

fn gil_listing_price(listing: &ShopListingInfo) -> Option<u32> {
    listing
        .costs
        .iter()
        .find(|cost| cost.item_id == 1 && cost.count > 0)
        .map(|cost| cost.count)
}

fn is_available_vendor_price(price: u32) -> bool {
    price > 0 && price < UNAVAILABLE_VENDOR_PRICE
}

fn push_vendor_source(
    sources: &mut Vec<VendorSourceInfo>,
    price: u32,
    shop: &ShopInfo,
    npc: &NpcInfo,
) {
    sources.push(VendorSourceInfo {
        price,
        npc_name: (!npc.name.trim().is_empty()).then(|| npc.name.clone()),
        npc_title: npc.title.clone(),
        shop_name: (!shop.name.trim().is_empty()).then(|| shop.name.clone()),
        map_name: npc.map_name.clone(),
        place_name: npc.place_name.clone(),
        territory_name: npc.territory_name.clone(),
        x: npc.x,
        y: npc.y,
    });
}

fn load_market_snapshot() -> Result<MarketSnapshot> {
    read_json_file(&universalis_cache_dir().join("market_snapshot.json"))
}

fn save_market_snapshot(snapshot: &MarketSnapshot) -> Result<()> {
    let dir = universalis_cache_dir();
    fs::create_dir_all(&dir)?;
    write_json_file(&dir.join("market_snapshot.json"), snapshot)
}

fn export_saintcoinach_cache(item_ids: &[u32], progress: &dyn Fn(String)) -> Result<()> {
    let root = workspace_root();
    let saintcoinach_dir = root.join("Source").join("SaintCoinach");
    if !saintcoinach_dir
        .join("SaintCoinach")
        .join("SaintCoinach.csproj")
        .exists()
    {
        return Err(anyhow!(
            "SaintCoinach is missing. Clone the SaintCoinach repository into {} first.",
            saintcoinach_dir.display()
        ));
    }

    let exporter_dir = root.join("Source").join("Fortuna.SaintCoinachExporter");
    if !exporter_dir
        .join("Fortuna.SaintCoinachExporter.csproj")
        .exists()
    {
        return Err(anyhow!(
            "SaintCoinach exporter project is missing at {}.",
            exporter_dir.display()
        ));
    }

    ensure_dotnet_sdk_available()?;

    let game_dir = find_ffxiv_game_dir().ok_or_else(|| {
        anyhow!(
            "Could not find your FFXIV install. Set FORTUNA_FFXIV_GAME_DIR to the folder containing game\\ffxivgame.ver."
        )
    })?;

    let cache_dir = saintcoinach_cache_dir();
    fs::create_dir_all(&cache_dir)?;
    let item_ids_path = cache_dir.join("saintcoinach_item_ids.json");
    write_json_file(&item_ids_path, &item_ids)?;

    progress(format!(
        "Running SaintCoinach exporter against {}.",
        game_dir.display()
    ));
    let output = Command::new("dotnet")
        .arg("run")
        .arg("--project")
        .arg(&exporter_dir)
        .arg("--")
        .arg("--game-path")
        .arg(&game_dir)
        .arg("--output-dir")
        .arg(&cache_dir)
        .arg("--item-ids-file")
        .arg(&item_ids_path)
        .current_dir(&root)
        .output()
        .context("failed to launch dotnet for the SaintCoinach exporter")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!(
            "SaintCoinach exporter failed.\nstdout:\n{}\nstderr:\n{}",
            stdout.trim(),
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().filter(|line| {
        line.contains("No Libra drop database loaded") || line.contains("Using Libra drop database")
    }) {
        progress(line.to_string());
    }
    let line_count = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    if line_count > 0 {
        progress(format!(
            "SaintCoinach exporter completed with {} status line(s).",
            line_count
        ));
    }

    Ok(())
}

fn ensure_dotnet_sdk_available() -> Result<()> {
    let output = Command::new("dotnet")
        .arg("--list-sdks")
        .output()
        .context("failed to launch dotnet. Install the .NET SDK, then restart Fortuna")?;

    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        return Err(anyhow!(
            "No .NET SDK is installed. SaintCoinach export needs the .NET SDK, not just the runtime. Install it with: winget install Microsoft.DotNet.SDK.7"
        ));
    }

    Ok(())
}

fn find_ffxiv_game_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("FORTUNA_FFXIV_GAME_DIR") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(program_files_x86) = std::env::var("ProgramFiles(x86)") {
        candidates.push(
            PathBuf::from(&program_files_x86)
                .join("SquareEnix")
                .join("FINAL FANTASY XIV - A Realm Reborn"),
        );
        candidates.push(
            PathBuf::from(program_files_x86)
                .join("Steam")
                .join("steamapps")
                .join("common")
                .join("FINAL FANTASY XIV Online"),
        );
    }
    if let Ok(program_files) = std::env::var("ProgramFiles") {
        candidates.push(
            PathBuf::from(program_files)
                .join("SquareEnix")
                .join("FINAL FANTASY XIV - A Realm Reborn"),
        );
    }
    candidates.push(PathBuf::from(
        r"D:\Steam\steamapps\common\FINAL FANTASY XIV Online",
    ));
    candidates.push(PathBuf::from(
        r"D:\Games\SteamApps\common\FINAL FANTASY XIV Online",
    ));
    candidates.push(PathBuf::from(
        r"D:\SteamLibrary\steamapps\common\FINAL FANTASY XIV Online",
    ));

    candidates
        .into_iter()
        .find(|path| path.join("game").join("ffxivgame.ver").exists())
}

fn game_data_cache_manifest(item_ids: &[u32]) -> GameDataCacheManifest {
    let mut hasher = DefaultHasher::new();
    GAME_DATA_CACHE_SCHEMA_VERSION.hash(&mut hasher);
    item_ids.hash(&mut hasher);
    GameDataCacheManifest {
        schema_version: GAME_DATA_CACHE_SCHEMA_VERSION,
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

fn saintcoinach_cache_dir() -> PathBuf {
    workspace_root().join("Cache").join("SaintCoinach")
}

fn universalis_cache_dir() -> PathBuf {
    workspace_root().join("Cache").join("Universalis")
}

fn lodestone_cache_dir() -> PathBuf {
    workspace_root().join("Cache").join("Lodestone")
}

fn lodestone_raw_cache_dir() -> PathBuf {
    lodestone_cache_dir().join("raw")
}

fn fetch_sales_history(
    client: &reqwest::blocking::Client,
    ids: &[u32],
    progress: &dyn Fn(String),
) -> Result<HashMap<u32, SaleHistory>> {
    let mut fetched_entries = HashMap::new();

    let chunks = ids.chunks(MAX_MARKET_IDS_PER_REQUEST).len();
    for (index, chunk) in ids.chunks(MAX_MARKET_IDS_PER_REQUEST).enumerate() {
        progress(format!("Fetching sale history ({}/{chunks}).", index + 1));
        fetch_sales_history_chunk(client, chunk, &mut fetched_entries, progress)?;
    }

    let mut cached_entries = load_sale_history_entries(progress);
    let fetched_count = fetched_entries.values().map(Vec::len).sum::<usize>();
    for (item_id, mut entries) in fetched_entries {
        cached_entries
            .entry(item_id)
            .or_default()
            .append(&mut entries);
    }
    dedupe_sale_history_entries(&mut cached_entries);
    save_sale_history_entries(&cached_entries, progress);

    let sales = ids
        .iter()
        .filter_map(|item_id| {
            cached_entries
                .get(item_id)
                .map(|entries| (*item_id, sale_history_from_entries(entries)))
        })
        .collect::<HashMap<_, _>>();

    progress(format!(
        "Loaded sale history for {} market items; cached {} new entries.",
        sales.len(),
        fetched_count
    ));
    Ok(sales)
}

fn fetch_sales_history_chunk(
    client: &reqwest::blocking::Client,
    ids: &[u32],
    entries_by_item: &mut HashMap<u32, Vec<HistoryEntry>>,
    progress: &dyn Fn(String),
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }

    let joined = ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let url = format!("https://universalis.app/api/history/{WORLD}/{joined}?entries=500");
    let response = client.get(url).send();
    let response = match response {
        Ok(response) => response,
        Err(err) if ids.len() > 1 && (err.is_timeout() || err.is_connect()) => {
            progress(format!(
                "Universalis history request for {} item IDs timed out; splitting the batch.",
                ids.len()
            ));
            let midpoint = ids.len() / 2;
            fetch_sales_history_chunk(client, &ids[..midpoint], entries_by_item, progress)?;
            fetch_sales_history_chunk(client, &ids[midpoint..], entries_by_item, progress)?;
            return Ok(());
        }
        Err(err) if ids.len() == 1 && (err.is_timeout() || err.is_connect()) => {
            progress(format!(
                "Skipping sale history for item {} after Universalis timed out.",
                ids[0]
            ));
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };

    if response.status().is_server_error()
        || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        if ids.len() == 1 {
            progress(format!(
                "Skipping sale history for item {} after Universalis returned {}.",
                ids[0],
                response.status()
            ));
            return Ok(());
        }

        progress(format!(
            "Universalis returned {} for {} sale-history item IDs; splitting the batch.",
            response.status(),
            ids.len()
        ));
        let midpoint = ids.len() / 2;
        fetch_sales_history_chunk(client, &ids[..midpoint], entries_by_item, progress)?;
        fetch_sales_history_chunk(client, &ids[midpoint..], entries_by_item, progress)?;
        return Ok(());
    }

    match response.error_for_status()?.json::<HistoryResponse>()? {
        HistoryResponse::Single(item) => {
            entries_by_item.insert(item.item_id, item.entries);
        }
        HistoryResponse::Multi { items } => {
            for item in items {
                entries_by_item.insert(item.item_id, item.entries);
            }
        }
    }

    Ok(())
}

fn load_sale_history_entries(progress: &dyn Fn(String)) -> HashMap<u32, Vec<HistoryEntry>> {
    let path = sale_history_entries_path();
    match read_json_file(&path) {
        Ok(entries) => {
            progress("Using cached Universalis sale history.".to_string());
            entries
        }
        Err(_) => HashMap::new(),
    }
}

fn save_sale_history_entries(
    entries_by_item: &HashMap<u32, Vec<HistoryEntry>>,
    progress: &dyn Fn(String),
) {
    if let Some(parent) = sale_history_entries_path().parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            progress(format!(
                "Could not create sale history cache directory: {err:#}"
            ));
            return;
        }
    }

    if let Err(err) = write_json_file(&sale_history_entries_path(), entries_by_item) {
        progress(format!("Could not save sale history cache: {err:#}"));
    }
}

fn sale_history_entries_path() -> PathBuf {
    universalis_cache_dir().join("sale_history_entries.json")
}

fn dedupe_sale_history_entries(entries_by_item: &mut HashMap<u32, Vec<HistoryEntry>>) {
    for entries in entries_by_item.values_mut() {
        entries.sort_by(|a, b| {
            b.timestamp
                .cmp(&a.timestamp)
                .then_with(|| a.price_per_unit.cmp(&b.price_per_unit))
                .then_with(|| a.quantity.cmp(&b.quantity))
        });
        entries.dedup_by(|a, b| {
            a.timestamp == b.timestamp
                && a.price_per_unit == b.price_per_unit
                && a.quantity == b.quantity
        });
    }
}

fn sale_history_from_entries(entries: &[HistoryEntry]) -> SaleHistory {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let cutoff_1d = now - 86_400;
    let cutoff_2d = now - 172_800;
    let cutoff_4d = now - 345_600;
    let recent_sales_1d = entries
        .iter()
        .filter(|entry| entry.timestamp >= cutoff_1d)
        .count();
    let recent_sales_2d = entries
        .iter()
        .filter(|entry| entry.timestamp >= cutoff_2d)
        .count();
    let recent_sales_4d = entries
        .iter()
        .filter(|entry| entry.timestamp >= cutoff_4d)
        .count();
    let recent_quantity_1d = entries
        .iter()
        .filter(|entry| entry.timestamp >= cutoff_1d)
        .map(|entry| entry.quantity.max(1))
        .sum();
    let recent_quantity_2d = entries
        .iter()
        .filter(|entry| entry.timestamp >= cutoff_2d)
        .map(|entry| entry.quantity.max(1))
        .sum();
    let recent_quantity_4d = entries
        .iter()
        .filter(|entry| entry.timestamp >= cutoff_4d)
        .map(|entry| entry.quantity.max(1))
        .sum();
    let mut daily_history = (0..7)
        .rev()
        .map(|days_ago| DailySaleBucket {
            days_ago,
            ..Default::default()
        })
        .collect::<Vec<_>>();
    for entry in entries {
        let age_seconds = now.saturating_sub(entry.timestamp);
        if age_seconds < 7 * 86_400 {
            let days_ago = (age_seconds / 86_400) as u8;
            if let Some(bucket) = daily_history
                .iter_mut()
                .find(|bucket| bucket.days_ago == days_ago)
            {
                let quantity = entry.quantity.max(1);
                bucket.sales += 1;
                bucket.quantity = bucket.quantity.saturating_add(quantity);
                bucket.value = bucket.value.saturating_add(u64::from(entry.price_per_unit));
            }
        }
    }
    for bucket in &mut daily_history {
        if bucket.sales > 0 {
            bucket.value /= bucket.sales as u64;
        }
    }
    let mut sorted_entries = entries.iter().collect::<Vec<_>>();
    sorted_entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    let recent_sale_prices = sorted_entries
        .into_iter()
        .filter_map(|entry| (entry.price_per_unit > 0).then_some(entry.price_per_unit))
        .take(4)
        .collect();
    SaleHistory {
        recent_sales_1d,
        recent_sales_2d,
        recent_sales_4d,
        recent_quantity_1d,
        recent_quantity_2d,
        recent_quantity_4d,
        recent_sale_prices,
        daily_history,
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
    if let Ok((cache, _)) = load_game_data_cache() {
        ids.extend(cache.gathering_items.iter().map(|item| item.item_id));
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
    item_metadata: &HashMap<u32, ItemMetadata>,
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
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
                item_metadata,
                vendor_sources,
                &recipe_materials,
                &mut memo,
                &mut HashSet::new(),
            )
            .or_else(|| {
                craft_only_cost(
                    recipe.item_id,
                    market_prices,
                    item_metadata,
                    vendor_sources,
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
                recent_sales_1d: sell.recent_sales_1d,
                recent_sales_2d: sell.recent_sales_2d,
                recent_sales_4d: sell.recent_sales_4d,
                recent_quantity_4d: sell.recent_quantity_4d,
            })
        })
        .collect::<Vec<_>>();
    dedupe_profit_rows_by_name(&mut rows);
    sort_profit_rows(&mut rows, ranking_mode);
    rows
}

fn dedupe_profit_rows_by_name(rows: &mut Vec<ProfitRow>) {
    let mut best_by_name: HashMap<String, ProfitRow> = HashMap::new();
    for row in rows.drain(..) {
        let key = normalized_item_name(&row.item_name);
        match best_by_name.get(&key) {
            Some(existing) if profit_row_is_better(existing, &row) => {}
            _ => {
                best_by_name.insert(key, row);
            }
        }
    }
    rows.extend(best_by_name.into_values());
}

fn normalized_item_name(name: &str) -> String {
    name.trim().to_lowercase()
}

fn profit_row_is_better(current: &ProfitRow, candidate: &ProfitRow) -> bool {
    current
        .profit
        .cmp(&candidate.profit)
        .then_with(|| candidate.craft_cost.cmp(&current.craft_cost))
        .then_with(|| {
            current
                .recent_quantity_4d
                .cmp(&candidate.recent_quantity_4d)
        })
        .then_with(|| current.recent_sales_4d.cmp(&candidate.recent_sales_4d))
        .then_with(|| candidate.recipe_id.cmp(&current.recipe_id))
        .is_ge()
}

fn rank_gathering(
    market_prices: &HashMap<u32, MarketPrice>,
    gathering_items: &[GatheringItemInfo],
    sale_value_mode: SaleValueMode,
    ranking_mode: RankingMode,
) -> Vec<GatherRow> {
    let recipe_by_item = RECIPES
        .entries()
        .map(|(recipe_id, recipe)| (recipe.item_id, recipe_id))
        .collect::<HashMap<_, _>>();
    let gather_source = if gathering_items.is_empty() {
        market_prices
            .iter()
            .filter(|(_, price)| {
                price
                    .metadata
                    .search_category_name
                    .as_deref()
                    .or(price.metadata.ui_category_name.as_deref())
                    .is_some_and(is_gather_category_name)
            })
            .map(|(&item_id, price)| GatheringItemInfo {
                item_id,
                item_name: price.metadata.name.clone(),
                locations: Vec::new(),
            })
            .collect::<Vec<_>>()
    } else {
        gathering_items.to_vec()
    };

    let mut rows = gather_source
        .into_iter()
        .map(|gather_item| {
            let price = market_prices.get(&gather_item.item_id);
            let sale_basis = price
                .map(|price| sale_value(price, sale_value_mode))
                .unwrap_or(0);
            let acquisition_cost = 0;
            let locations = summarize_gathering_locations(&gather_item.locations);
            GatherRow {
                item_id: gather_item.item_id,
                recipe_id: recipe_by_item.get(&gather_item.item_id).copied(),
                item_name: gather_item
                    .item_name
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| item_name(gather_item.item_id)),
                search_category_id: price.and_then(|price| price.metadata.search_category_id),
                sell_price: sale_basis,
                profit: i64::from(sale_basis) - i64::from(acquisition_cost),
                daily_velocity: price.map(|price| price.daily_velocity).unwrap_or_default(),
                recent_sales_1d: price.map(|price| price.recent_sales_1d).unwrap_or_default(),
                recent_sales_2d: price.map(|price| price.recent_sales_2d).unwrap_or_default(),
                recent_sales_4d: price.map(|price| price.recent_sales_4d).unwrap_or_default(),
                recent_quantity_1d: price
                    .map(|price| price.recent_quantity_1d)
                    .unwrap_or_default(),
                recent_quantity_2d: price
                    .map(|price| price.recent_quantity_2d)
                    .unwrap_or_default(),
                recent_quantity_4d: price
                    .map(|price| price.recent_quantity_4d)
                    .unwrap_or_default(),
                node_type: locations.node_type,
                where_text: locations.where_text,
                timed_windows: locations.timed_windows,
            }
        })
        .collect::<Vec<_>>();
    sort_gather_rows(&mut rows, ranking_mode);
    rows
}

fn rank_market(
    market_prices: &HashMap<u32, MarketPrice>,
    item_metadata: &HashMap<u32, ItemMetadata>,
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
    xiv_recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
    gathering_items: &[GatheringItemInfo],
    sale_value_mode: SaleValueMode,
    ranking_mode: RankingMode,
) -> Vec<MarketRow> {
    let recipe_by_item = RECIPES
        .entries()
        .map(|(recipe_id, recipe)| (recipe.item_id, recipe_id))
        .collect::<HashMap<_, _>>();
    let gathered_items = gathering_items
        .iter()
        .map(|item| item.item_id)
        .collect::<HashSet<_>>();
    let recipe_materials = recipe_materials_with_raphael_fallback(xiv_recipe_materials);
    let mut memo = HashMap::new();

    let mut item_ids = market_prices.keys().copied().collect::<HashSet<_>>();
    item_ids.extend(vendor_sources.keys().copied());

    let mut rows = item_ids
        .into_iter()
        .filter(|item_id| !is_collectable_item(*item_id))
        .filter_map(|item_id| {
            let price = market_prices.get(&item_id);
            let has_market_price = price.is_some();
            let craft_cost = recipe_by_item.get(&item_id).and_then(|_| {
                craft_only_cost(
                    item_id,
                    market_prices,
                    item_metadata,
                    vendor_sources,
                    &recipe_materials,
                    &mut memo,
                    &mut HashSet::new(),
                )
            });
            let sell_price = price
                .map(|price| sale_value(price, sale_value_mode))
                .unwrap_or_default();
            let npc_price = vendor_cost(item_id, market_prices, item_metadata, vendor_sources);
            let acquisition_cost = match (craft_cost, npc_price) {
                (Some(craft), Some(npc)) => Some(craft.min(npc)),
                (Some(craft), None) => Some(craft),
                (None, Some(npc)) => Some(npc),
                (None, None) => None,
            };
            if !has_market_price && acquisition_cost.is_none() {
                return None;
            }
            let metadata = price
                .map(|price| &price.metadata)
                .or_else(|| item_metadata.get(&item_id));
            Some(MarketRow {
                item_id,
                recipe_id: recipe_by_item.get(&item_id).copied(),
                item_name: item_display_name(item_id, market_prices, item_metadata),
                category_name: metadata.and_then(|metadata| {
                    metadata
                        .search_category_name
                        .clone()
                        .or_else(|| metadata.ui_category_name.clone())
                }),
                search_category_id: metadata.and_then(|metadata| metadata.search_category_id),
                has_market_price,
                sell_price,
                average_sale: price.map(average_sale_display).unwrap_or_default(),
                acquisition_cost,
                craft_cost,
                profit: price
                    .and(acquisition_cost)
                    .map(|cost| i64::from(sell_price) - i64::from(cost)),
                npc_price,
                npc_sources: vendor_source_rows(item_id, vendor_sources),
                is_gathered: gathered_items.contains(&item_id),
                daily_velocity: price.map(|price| price.daily_velocity).unwrap_or_default(),
                recent_sales_1d: price.map(|price| price.recent_sales_1d).unwrap_or_default(),
                recent_sales_2d: price.map(|price| price.recent_sales_2d).unwrap_or_default(),
                recent_sales_4d: price.map(|price| price.recent_sales_4d).unwrap_or_default(),
                recent_quantity_4d: price
                    .map(|price| price.recent_quantity_4d)
                    .unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    sort_market_rows(&mut rows, ranking_mode);
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
        RankingMode::RecentQuantity4d => b
            .recent_quantity_4d
            .cmp(&a.recent_quantity_4d)
            .then_with(|| b.recent_sales_4d.cmp(&a.recent_sales_4d))
            .then_with(|| b.profit.cmp(&a.profit)),
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
        RankingMode::RecentQuantity4d => b
            .recent_quantity_4d
            .cmp(&a.recent_quantity_4d)
            .then_with(|| b.recent_sales_4d.cmp(&a.recent_sales_4d))
            .then_with(|| b.profit.cmp(&a.profit)),
    });
}

fn sort_market_rows(rows: &mut [MarketRow], ranking_mode: RankingMode) {
    rows.sort_by(|a, b| match ranking_mode {
        RankingMode::Profit => b
            .profit
            .unwrap_or(i64::MIN)
            .cmp(&a.profit.unwrap_or(i64::MIN))
            .then_with(|| b.recent_sales_4d.cmp(&a.recent_sales_4d))
            .then_with(|| b.daily_velocity.total_cmp(&a.daily_velocity)),
        RankingMode::RecentSales4d => b
            .recent_sales_4d
            .cmp(&a.recent_sales_4d)
            .then_with(|| b.recent_sales_2d.cmp(&a.recent_sales_2d))
            .then_with(|| b.daily_velocity.total_cmp(&a.daily_velocity)),
        RankingMode::RecentQuantity4d => b
            .recent_quantity_4d
            .cmp(&a.recent_quantity_4d)
            .then_with(|| b.recent_sales_4d.cmp(&a.recent_sales_4d))
            .then_with(|| b.daily_velocity.total_cmp(&a.daily_velocity)),
    });
}

struct GatheringLocationSummary {
    node_type: String,
    where_text: String,
    timed_windows: Vec<String>,
}

fn summarize_gathering_locations(locations: &[GatheringLocationInfo]) -> GatheringLocationSummary {
    let mut node_types = locations
        .iter()
        .map(|location| location.node_type.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    node_types.sort();
    node_types.dedup();

    let mut places = locations
        .iter()
        .filter_map(|location| location.place_name.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    places.sort();
    places.dedup();

    let mut timed_windows = locations
        .iter()
        .flat_map(|location| location.timed_windows.iter().cloned())
        .collect::<Vec<_>>();
    timed_windows.sort();
    timed_windows.dedup();

    GatheringLocationSummary {
        node_type: if node_types.is_empty() {
            "Unknown node".to_string()
        } else {
            node_types.join(", ")
        },
        where_text: if places.is_empty() {
            "Unknown location".to_string()
        } else {
            places.join(", ")
        },
        timed_windows,
    }
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
    item_metadata: &HashMap<u32, ItemMetadata>,
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
    recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
    memo: &mut HashMap<u32, Option<u32>>,
    visiting: &mut HashSet<u32>,
) -> Option<u32> {
    if let Some(cost) = memo.get(&item_id) {
        return *cost;
    }
    if !visiting.insert(item_id) {
        return buy_cost(item_id, market_prices, item_metadata, vendor_sources);
    }

    let craft_cost = recipe_materials.get(&item_id).and_then(|ingredients| {
        ingredients
            .iter()
            .map(|ingredient| {
                let unit = craft_component_cost(
                    ingredient.item_id,
                    market_prices,
                    item_metadata,
                    vendor_sources,
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

fn craft_component_cost(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    item_metadata: &HashMap<u32, ItemMetadata>,
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
    recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
    memo: &mut HashMap<u32, Option<u32>>,
    visiting: &mut HashSet<u32>,
) -> Option<u32> {
    if recipe_materials
        .get(&item_id)
        .is_some_and(|ingredients| !ingredients.is_empty())
    {
        craft_only_cost(
            item_id,
            market_prices,
            item_metadata,
            vendor_sources,
            recipe_materials,
            memo,
            visiting,
        )
    } else {
        buy_cost(item_id, market_prices, item_metadata, vendor_sources)
    }
}

fn explain_cost(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    item_metadata: &HashMap<u32, ItemMetadata>,
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
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
        vendor_sources,
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
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
    recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
    memo: &mut HashMap<u32, Option<u32>>,
    visiting: &mut HashSet<u32>,
    nodes: &mut Vec<CostNode>,
) {
    let market = market_cost(item_id, market_prices);
    let ingredient_market = ingredient_market_cost(item_id, market_prices, recipe_materials);
    let display_market = ingredient_market.or(market);
    let vendor = vendor_cost(item_id, market_prices, item_metadata, vendor_sources);
    let vendor_rows = vendor_source_rows(item_id, vendor_sources);
    let craft = craft_only_cost(
        item_id,
        market_prices,
        item_metadata,
        vendor_sources,
        recipe_materials,
        memo,
        visiting,
    );
    let ingredients = recipe_materials.get(&item_id).cloned().unwrap_or_default();
    let (unit_cost, source) = if craft.is_some() {
        match (vendor, craft) {
            (Some(vendor), Some(craft)) if vendor <= craft => (vendor, "NPC".to_string()),
            (_, Some(craft)) => (craft, "Crafted".to_string()),
            _ => (0, "Missing".to_string()),
        }
    } else {
        match (market, vendor) {
            (Some(market), Some(vendor)) if vendor <= market => (vendor, "NPC".to_string()),
            (Some(market), _) => (market, "Market".to_string()),
            (None, Some(vendor)) => (vendor, "NPC".to_string()),
            (None, None) => (0, "Missing".to_string()),
        }
    };
    nodes.push(CostNode {
        depth,
        name: item_display_name(item_id, market_prices, item_metadata),
        amount,
        unit_cost,
        market_unit_cost: display_market,
        npc_unit_cost: vendor,
        craft_unit_cost: craft,
        npc_sources: vendor_rows,
        source: source.clone(),
    });

    let should_expand_craft = source == "Crafted";
    if !ingredients.is_empty() && (depth == 0 || should_expand_craft) {
        for ingredient in ingredients {
            explain_cost_inner(
                ingredient.item_id,
                ingredient.amount,
                depth + 1,
                market_prices,
                item_metadata,
                vendor_sources,
                recipe_materials,
                memo,
                visiting,
                nodes,
            );
        }
    }
}

fn buy_cost(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    item_metadata: &HashMap<u32, ItemMetadata>,
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
) -> Option<u32> {
    match (
        market_cost(item_id, market_prices),
        vendor_cost(item_id, market_prices, item_metadata, vendor_sources),
    ) {
        (Some(market), Some(vendor)) => Some(market.min(vendor)),
        (Some(market), None) => Some(market),
        (None, Some(vendor)) => Some(vendor),
        (None, None) => None,
    }
}

fn market_cost(item_id: u32, market_prices: &HashMap<u32, MarketPrice>) -> Option<u32> {
    market_prices.get(&item_id).map(|price| price.min_listing)
}

fn ingredient_market_cost(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    recipe_materials: &HashMap<u32, Vec<RecipeMaterial>>,
) -> Option<u32> {
    let ingredients = recipe_materials.get(&item_id)?;
    if ingredients.is_empty() {
        return None;
    }

    ingredients
        .iter()
        .map(|ingredient| {
            let unit = market_cost(ingredient.item_id, market_prices)?;
            Some(unit.saturating_mul(ingredient.amount))
        })
        .sum()
}

fn vendor_cost(
    item_id: u32,
    market_prices: &HashMap<u32, MarketPrice>,
    item_metadata: &HashMap<u32, ItemMetadata>,
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
) -> Option<u32> {
    if vendor_source_rows(item_id, vendor_sources).is_empty() {
        return None;
    }
    market_prices
        .get(&item_id)
        .and_then(|price| price.metadata.vendor_price)
        .or_else(|| {
            item_metadata
                .get(&item_id)
                .and_then(|metadata| metadata.vendor_price)
        })
        .or_else(|| vendor_source_unit_cost(item_id, vendor_sources))
        .filter(|&price| is_available_vendor_price(price))
}

fn vendor_source_unit_cost(
    item_id: u32,
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
) -> Option<u32> {
    vendor_sources
        .get(&item_id)?
        .iter()
        .filter(|source| is_known_vendor_source(source))
        .map(|source| source.price)
        .filter(|&price| is_available_vendor_price(price))
        .min()
}

fn vendor_source_rows(
    item_id: u32,
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
) -> Vec<VendorSourceDisplay> {
    vendor_sources
        .get(&item_id)
        .map(|sources| {
            sources
                .iter()
                .filter(|source| is_known_vendor_source(source))
                .take(3)
                .map(vendor_source_display)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn is_known_vendor_source(source: &VendorSourceInfo) -> bool {
    source
        .npc_name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
}

fn vendor_source_display(source: &VendorSourceInfo) -> VendorSourceDisplay {
    VendorSourceDisplay {
        seller: vendor_source_seller(source),
        location: vendor_source_location(source),
    }
}

fn drop_source_display(source: &DropSourceInfo) -> String {
    let mut text = match source
        .duty_name
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(duty) => format!("{}: {} - {}", source.source_type, source.source_name, duty),
        None => format!("{}: {}", source.source_type, source.source_name),
    };

    if let Some(location) = source.location.as_deref().filter(|value| !value.is_empty()) {
        text.push_str(" - ");
        text.push_str(location);
    }
    if let (Some(x), Some(y)) = (source.x, source.y) {
        text.push_str(&format!(" ({x:.1}, {y:.1})"));
    }
    if let Some(level_range) = source
        .level_range
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        text.push_str(&format!(" Lv. {level_range}"));
    }
    text
}

fn drop_source_where_summary(sources: &[DropSourceInfo]) -> Option<String> {
    if sources.is_empty() {
        return None;
    }

    let mut values = sources
        .iter()
        .map(drop_source_where_label)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();

    if values.is_empty() {
        return None;
    }

    let shown = values.iter().take(3).cloned().collect::<Vec<_>>();
    let remaining = values.len().saturating_sub(shown.len());
    let mut text = shown.join("; ");
    if remaining > 0 {
        text.push_str(&format!("; +{} more", format_count(remaining)));
    }
    Some(text)
}

fn drop_source_where_label(source: &DropSourceInfo) -> String {
    let mut text = match source
        .duty_name
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(duty) => format!("{}: {}", source.source_name, duty),
        None => source.source_name.clone(),
    };

    if let Some(location) = source.location.as_deref().filter(|value| !value.is_empty()) {
        if text.is_empty() {
            text.push_str(location);
        } else {
            text.push_str(" - ");
            text.push_str(location);
        }
    }
    if let (Some(x), Some(y)) = (source.x, source.y) {
        text.push_str(&format!(" ({x:.1}, {y:.1})"));
    }
    text
}

fn vendor_source_label(source: &VendorSourceInfo) -> String {
    let display = vendor_source_display(source);
    format!("{} - {}", display.seller, display.location)
}

fn vendor_source_seller(source: &VendorSourceInfo) -> String {
    let npc = source
        .npc_name
        .as_deref()
        .or(source.shop_name.as_deref())
        .unwrap_or("unknown vendor");
    let mut label = title_case_words(npc);
    if let Some(title) = source
        .npc_title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
    {
        label.push_str(&format!(" ({})", title_case_words(title)));
    }
    label
}

fn vendor_source_location(source: &VendorSourceInfo) -> String {
    let place = source
        .place_name
        .as_deref()
        .or(source.map_name.as_deref())
        .or(source.territory_name.as_deref());
    let mut label = title_case_words(
        place
            .filter(|place| !place.trim().is_empty())
            .unwrap_or("Unknown location"),
    );
    if let (Some(x), Some(y)) = (source.x, source.y) {
        label.push_str(&format!(" ({x:.1}, {y:.1})"));
    }
    label
}

fn title_case_words(text: &str) -> String {
    text.split_whitespace()
        .map(title_case_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn title_case_word(word: &str) -> String {
    word.split('-')
        .map(capitalize_first_char)
        .collect::<Vec<_>>()
        .join("-")
}

fn capitalize_first_char(text: &str) -> String {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };

    first.to_uppercase().chain(chars).collect::<String>()
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

fn draw_sales_graph(ui: &mut egui::Ui, history: &[DailySaleBucket], mode: SalesGraphMode) {
    let desired_size = egui::vec2(ui.available_width().max(240.0), 140.0);
    let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );

    if history.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No sale history loaded",
            egui::TextStyle::Small.resolve(ui.style()),
            ui.visuals().weak_text_color(),
        );
        return;
    }

    let values = history
        .iter()
        .map(|bucket| match mode {
            SalesGraphMode::Quantity => f64::from(bucket.quantity),
            SalesGraphMode::Value => bucket.value as f64,
        })
        .collect::<Vec<_>>();
    let max_value = values.iter().copied().fold(0.0, f64::max).max(1.0);
    let plot = rect.shrink2(egui::vec2(10.0, 18.0));
    let step = if values.len() > 1 {
        plot.width() / (values.len() - 1) as f32
    } else {
        0.0
    };
    let points = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let x = plot.left() + step * index as f32;
            let y = plot.bottom() - ((*value / max_value) as f32 * plot.height());
            egui::pos2(x, y)
        })
        .collect::<Vec<_>>();

    if points.len() > 1 {
        painter.add(egui::Shape::line(
            points.clone(),
            egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
        ));
    }
    let hovered_index = response.hover_pos().and_then(|pos| {
        points
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                (a.x - pos.x)
                    .abs()
                    .partial_cmp(&(b.x - pos.x).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(index, _)| index)
    });

    if let Some(index) = hovered_index {
        if let Some(point) = points.get(index) {
            painter.line_segment(
                [
                    egui::pos2(point.x, plot.top()),
                    egui::pos2(point.x, plot.bottom()),
                ],
                egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
            );
            painter.circle_filled(*point, 5.0, ui.visuals().selection.stroke.color);
        }
    }

    for (index, point) in points.iter().enumerate() {
        if Some(index) == hovered_index {
            continue;
        }
        painter.circle_filled(*point, 3.0, ui.visuals().selection.stroke.color);
    }

    if let Some(index) = hovered_index {
        if let (Some(bucket), Some(point)) = (history.get(index), points.get(index)) {
            let amount = match mode {
                SalesGraphMode::Quantity => format!("{} sold", format_count(bucket.quantity)),
                SalesGraphMode::Value => format!("{} average", format_gil(bucket.value as u32)),
            };
            let hover_text = format!(
                "{}\n{}\n{} sales",
                day_label(bucket.days_ago),
                amount,
                format_count(bucket.sales)
            );
            response.on_hover_text(hover_text);

            let text_pos = egui::pos2(
                (point.x + 8.0).min(rect.right() - 8.0),
                (point.y - 8.0).max(rect.top() + 22.0),
            );
            painter.text(
                text_pos,
                egui::Align2::RIGHT_BOTTOM,
                amount,
                egui::TextStyle::Small.resolve(ui.style()),
                ui.visuals().text_color(),
            );
        }
    }

    let label = match mode {
        SalesGraphMode::Quantity => {
            format!("max {} qty/day", format_count(max_value.round() as u64))
        }
        SalesGraphMode::Value => format!("max {} avg/day", format_gil(max_value.round() as u32)),
    };
    painter.text(
        rect.left_top() + egui::vec2(8.0, 6.0),
        egui::Align2::LEFT_TOP,
        label,
        egui::TextStyle::Small.resolve(ui.style()),
        ui.visuals().weak_text_color(),
    );
    painter.text(
        rect.left_bottom() + egui::vec2(8.0, -4.0),
        egui::Align2::LEFT_BOTTOM,
        "6d ago",
        egui::TextStyle::Small.resolve(ui.style()),
        ui.visuals().weak_text_color(),
    );
    painter.text(
        rect.right_bottom() + egui::vec2(-8.0, -4.0),
        egui::Align2::RIGHT_BOTTOM,
        "today",
        egui::TextStyle::Small.resolve(ui.style()),
        ui.visuals().weak_text_color(),
    );
}

fn day_label(days_ago: u8) -> String {
    match days_ago {
        0 => "today".to_string(),
        1 => "1d ago".to_string(),
        days => format!("{days}d ago"),
    }
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
    item_metadata: HashMap<u32, ItemMetadata>,
    vendor_sources: HashMap<u32, Vec<VendorSourceInfo>>,
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
        &item_metadata,
        &vendor_sources,
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
    item_metadata: &HashMap<u32, ItemMetadata>,
    vendor_sources: &HashMap<u32, Vec<VendorSourceInfo>>,
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
        let buy = buy_cost(
            ingredient.item_id,
            market_prices,
            item_metadata,
            vendor_sources,
        );
        let craft = craft_only_cost(
            ingredient.item_id,
            market_prices,
            item_metadata,
            vendor_sources,
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
                item_metadata,
                vendor_sources,
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

fn parse_fraction_progress(message: &str, prefix: &str) -> Option<(usize, usize)> {
    let fraction = message.strip_prefix(prefix)?.split(')').next()?;
    let (current, total) = fraction.split_once('/')?;
    Some((current.trim().parse().ok()?, total.trim().parse().ok()?))
}

fn parse_count_after(message: &str, prefix: &str) -> Option<usize> {
    message
        .strip_prefix(prefix)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
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
        .unwrap_or_else(|| format!("Item #{}", format_count(item_id)))
}

fn format_gil(value: u32) -> String {
    format!("{} gil", format_number(value))
}

fn cost_cell(unit_cost: Option<u32>, amount: u32) -> String {
    unit_cost
        .map(|cost| format_gil(cost.saturating_mul(amount)))
        .unwrap_or_else(|| "-".to_string())
}

fn draw_cost_tree_item_cell(
    ui: &mut egui::Ui,
    node: &CostNode,
    cell_size: egui::Vec2,
    strong: bool,
) {
    const TREE_INDENT_WIDTH: f32 = 18.0;
    const TREE_MARKER_WIDTH: f32 = 9.0;
    const TREE_MARKER_GAP: f32 = 1.0;

    let row_height = ui.spacing().interact_size.y;
    let mut tree_width = 0.0;
    if node.depth > 0 {
        let max_tree_width = (cell_size.x - 80.0).max(0.0);
        let tree_indent = (node.depth as f32 * TREE_INDENT_WIDTH).min(max_tree_width);
        tree_width = tree_indent + TREE_MARKER_WIDTH + TREE_MARKER_GAP;
    }

    let label_width = (cell_size.x - tree_width).max(20.0);
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let color = if strong {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    let mut layout_job = egui::text::LayoutJob::single_section(
        node.name.clone(),
        egui::text::TextFormat::simple(font_id, color),
    );
    layout_job.wrap.max_width = label_width;
    layout_job.halign = egui::Align::Min;
    layout_job.justify = false;

    let galley = ui.painter().layout_job(layout_job);
    let desired_height = row_height.max(galley.size().y);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(cell_size.x, desired_height),
        egui::Sense::hover(),
    );
    let label_pos = egui::pos2(
        rect.left() + tree_width,
        rect.top() + ((row_height - galley.size().y).max(0.0) * 0.5),
    );

    if node.depth > 0 {
        let marker_left = rect.left() + tree_width - TREE_MARKER_GAP - TREE_MARKER_WIDTH;
        let marker_right = marker_left + TREE_MARKER_WIDTH;
        let center_y = rect.top() + row_height * 0.5;
        let left = marker_left + 1.0;
        let right = marker_right - 1.0;
        let tip = egui::pos2(right, center_y);
        let stroke = egui::Stroke::new(1.0, ui.visuals().weak_text_color());
        ui.painter().line_segment(
            [
                egui::pos2(left, rect.top() + 4.0),
                egui::pos2(left, center_y),
            ],
            stroke,
        );
        ui.painter()
            .line_segment([egui::pos2(left, center_y), tip], stroke);
        ui.painter()
            .line_segment([tip, egui::pos2(right - 3.0, center_y - 3.0)], stroke);
        ui.painter()
            .line_segment([tip, egui::pos2(right - 3.0, center_y + 3.0)], stroke);
    }

    ui.painter().galley(label_pos, galley, color);
}

fn average_sale_display(price: &MarketPrice) -> u32 {
    if !price.recent_sale_prices.is_empty() {
        let total = price
            .recent_sale_prices
            .iter()
            .map(|&value| u64::from(value))
            .sum::<u64>();
        (total / price.recent_sale_prices.len() as u64) as u32
    } else {
        price
            .average_sale
            .map(|value| value.round() as u32)
            .unwrap_or(price.min_listing)
    }
}

fn format_gil_signed(value: i64) -> String {
    if value >= 0 {
        format!("+{} gil", format_number(value as u64))
    } else {
        format!("-{} gil", format_number(value.unsigned_abs()))
    }
}

fn format_count(value: impl ToString) -> String {
    format_number(value)
}

fn format_number(value: impl ToString) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped.chars().rev().collect()
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
