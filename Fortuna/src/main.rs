use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver},
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
const MAX_XIVAPI_ROWS_PER_REQUEST: usize = 500;

fn main() -> eframe::Result {
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
    selected_categories: HashSet<u32>,
    selected_item_id: Option<u32>,
    selected_recipe_id: Option<u32>,
    selected_cost_breakdown: Vec<CostNode>,
    recipe_search: String,
    only_profitable: bool,
    only_recently_sold: bool,
    status: String,
    market_rx: Option<Receiver<Result<MarketSnapshot>>>,
    solve_rx: Option<Receiver<Result<SolvedMacro>>>,
    update_rx: Option<Receiver<Result<String>>>,
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
            selected_categories: HashSet::new(),
            selected_item_id: None,
            selected_recipe_id: None,
            selected_cost_breakdown: Vec::new(),
            recipe_search: String::new(),
            only_profitable: true,
            only_recently_sold: true,
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
                ui.heading("Fortuna Profit Crafter");
                ui.separator();
                ui.label(format!("{DATA_CENTER} / {WORLD}"));
                ui.separator();
                if ui.button("Refresh market").clicked() {
                    self.refresh_market(&ctx);
                }
                if ui.button("Clone raphael-rs if missing").clicked() {
                    self.ensure_repository(
                        &ctx,
                        "raphael-rs",
                        "https://github.com/KonaeAkira/raphael-rs.git",
                    );
                }
                if ui.button("Clone Universalis if missing").clicked() {
                    self.ensure_repository(
                        &ctx,
                        "Universalis",
                        "https://github.com/Universalis-FFXIV/Universalis.git",
                    );
                }
                if ui.button("Update XIVAPI cache").clicked() {
                    self.update_xivapi_cache(&ctx);
                }
                if ui.button("Solve selected macro").clicked() {
                    self.solve_selected_macro(&ctx);
                }
                if !self.status.is_empty() {
                    ui.separator();
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
                ui.label("Recipe filter");
                ui.text_edit_singleline(&mut self.recipe_search);
                ui.checkbox(&mut self.only_profitable, "Only profitable");
                ui.checkbox(&mut self.only_recently_sold, "Only >4 sales in 2 days");

                if !self.item_categories.is_empty() {
                    ui.separator();
                    ui.label("XIVAPI item types");
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            for category in &self.item_categories {
                                let mut selected = self.selected_categories.contains(&category.id);
                                if ui.checkbox(&mut selected, &category.name).changed() {
                                    if selected {
                                        self.selected_categories.insert(category.id);
                                    } else {
                                        self.selected_categories.remove(&category.id);
                                    }
                                }
                            }
                        });
                }

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
                Ok(Ok(snapshot)) => {
                    self.item_categories = snapshot.categories;
                    self.market_prices = snapshot.prices;
                    self.rows = rank_profitability(&self.market_prices);
                    self.gather_rows = rank_gathering(&self.market_prices);
                    self.status = format!(
                        "Loaded {} prices, XIVAPI item types, NPC vendor prices, and recent sale counts.",
                        self.market_prices.len()
                    );
                    self.market_rx = None;
                }
                Ok(Err(err)) => {
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
                Ok(Ok(solved)) => {
                    self.status = format!(
                        "Solved {} craft macros with {} total actions.",
                        solved.plans.len(),
                        solved.total_actions()
                    );
                    self.solved_macro = Some(solved);
                    self.solve_rx = None;
                }
                Ok(Err(err)) => {
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
                Ok(Ok(message)) => {
                    self.status = message;
                    self.update_rx = None;
                }
                Ok(Err(err)) => {
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
            let _ = tx.send(fetch_market_snapshot());
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
            let root = workspace_root();
            let result = ensure_repo(&root.join(repo_dir), url)
                .map(|message| format!("{repo_dir}: {message}"));
            let _ = tx.send(result);
        });
    }

    fn update_xivapi_cache(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        self.status = "Updating local XIVAPI cache...".to_string();
        ctx.request_repaint();
        thread::spawn(move || {
            let result = update_xivapi_library().map(|cache| {
                format!(
                    "XIVAPI cache updated: {} item types, {} items, {} NPC gil prices.",
                    cache.categories.len(),
                    cache.item_metadata.len(),
                    cache.vendor_prices.len()
                )
            });
            let _ = tx.send(result);
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
            let _ = tx.send(solve_macro_plan(recipe_id, recipe, stats, market_prices));
        });
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
        let needle = self.recipe_search.trim().to_lowercase();
        self.rows
            .iter()
            .filter(|row| !self.only_profitable || row.profit > 0)
            .filter(|row| !self.only_recently_sold || row.recent_sales_2d > 4)
            .filter(|row| {
                self.selected_categories.is_empty()
                    || row
                        .search_category_id
                        .is_some_and(|id| self.selected_categories.contains(&id))
            })
            .filter(|row| needle.is_empty() || row.item_name.to_lowercase().contains(&needle))
            .cloned()
            .collect()
    }

    fn filtered_gather_rows(&self) -> Vec<GatherRow> {
        let needle = self.recipe_search.trim().to_lowercase();
        self.gather_rows
            .iter()
            .filter(|row| !self.only_profitable || row.profit > 0)
            .filter(|row| !self.only_recently_sold || row.recent_sales_2d > 4)
            .filter(|row| {
                self.selected_categories.is_empty()
                    || row
                        .search_category_id
                        .is_some_and(|id| self.selected_categories.contains(&id))
            })
            .filter(|row| needle.is_empty() || row.item_name.to_lowercase().contains(&needle))
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

fn fetch_market_snapshot() -> Result<MarketSnapshot> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("FortunaProfitCrafter/0.1")
        .build()?;
    let ids = relevant_market_item_ids();
    let mut prices = HashMap::new();
    let xivapi_cache = load_or_update_xivapi_library()?;

    for chunk in ids.chunks(MAX_MARKET_IDS_PER_REQUEST) {
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

    let recent_sales = fetch_recent_sales_2d(&client)?;

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

fn load_or_update_xivapi_library() -> Result<XivApiCache> {
    match load_xivapi_library() {
        Ok(cache) if cache_is_usable(&cache) => Ok(cache),
        _ => update_xivapi_library(),
    }
}

fn cache_is_usable(cache: &XivApiCache) -> bool {
    !cache.categories.is_empty()
        && !cache.item_metadata.is_empty()
        && !cache.vendor_prices.is_empty()
}

fn load_xivapi_library() -> Result<XivApiCache> {
    let dir = xivapi_cache_dir();
    let categories = read_json_file(&dir.join("item_categories.json"))?;
    let item_metadata = read_json_file(&dir.join("item_metadata.json"))?;
    let vendor_prices = read_json_file(&dir.join("vendor_prices.json"))?;
    Ok(XivApiCache {
        categories,
        item_metadata,
        vendor_prices,
    })
}

fn update_xivapi_library() -> Result<XivApiCache> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("FortunaProfitCrafter/0.1")
        .build()?;
    let item_ids = relevant_market_item_ids();
    let cache = XivApiCache {
        categories: fetch_xivapi_item_categories(&client)
            .context("failed to fetch XIVAPI item categories")?,
        item_metadata: fetch_xivapi_item_metadata(&client, &item_ids)
            .context("failed to fetch XIVAPI item metadata")?,
        vendor_prices: fetch_xivapi_vendor_prices(&client)
            .context("failed to fetch XIVAPI vendor prices")?,
    };
    save_xivapi_library(&cache)?;
    Ok(cache)
}

fn save_xivapi_library(cache: &XivApiCache) -> Result<()> {
    let dir = xivapi_cache_dir();
    fs::create_dir_all(&dir)?;
    write_json_file(&dir.join("item_categories.json"), &cache.categories)?;
    write_json_file(&dir.join("item_metadata.json"), &cache.item_metadata)?;
    write_json_file(&dir.join("vendor_prices.json"), &cache.vendor_prices)?;
    Ok(())
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

fn fetch_xivapi_item_categories(client: &reqwest::blocking::Client) -> Result<Vec<ItemCategory>> {
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
    Ok(categories)
}

fn fetch_xivapi_item_metadata(
    client: &reqwest::blocking::Client,
    item_ids: &[u32],
) -> Result<HashMap<u32, ItemMetadata>> {
    let mut metadata = HashMap::new();
    for chunk in item_ids.chunks(MAX_MARKET_IDS_PER_REQUEST) {
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

fn fetch_xivapi_vendor_prices(client: &reqwest::blocking::Client) -> Result<HashMap<u32, u32>> {
    let mut vendor_prices = HashMap::new();
    let mut after = None;

    loop {
        let url = match after {
            Some(after) => format!(
                "https://v2.xivapi.com/api/sheet/GilShopItem?limit={MAX_XIVAPI_ROWS_PER_REQUEST}&after={after}&fields=Item@as(raw),Item.PriceMid"
            ),
            None => format!(
                "https://v2.xivapi.com/api/sheet/GilShopItem?limit={MAX_XIVAPI_ROWS_PER_REQUEST}&fields=Item@as(raw),Item.PriceMid"
            ),
        };
        let data: XivSheetRows = client.get(url).send()?.error_for_status()?.json()?;
        if data.rows.is_empty() {
            break;
        }
        for row in &data.rows {
            let item_id = row
                .fields
                .get("Item@as(raw)")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            let price = row
                .fields
                .get("Item")
                .and_then(|item| item.get("fields"))
                .and_then(|fields| fields.get("PriceMid"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0);
            if let (Some(item_id), Some(price)) = (item_id, price) {
                vendor_prices
                    .entry(item_id)
                    .and_modify(|current: &mut u32| *current = (*current).min(price))
                    .or_insert(price);
            }
        }
        after = data.rows.last().map(|row| row.row_id);
    }

    Ok(vendor_prices)
}

fn fetch_recent_sales_2d(client: &reqwest::blocking::Client) -> Result<HashMap<u32, usize>> {
    let ids = recipe_output_item_ids();
    let mut sales = HashMap::new();

    for chunk in ids.chunks(MAX_MARKET_IDS_PER_REQUEST) {
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

    let mut plans = Vec::new();
    for (recipe_id, recipe) in craft_plan {
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
            fs::remove_dir_all(&git_dir)
                .with_context(|| format!("failed to remove {}", git_dir.display()))?;
        }

        return Ok("cloned and detached".to_string());
    }

    Ok("already present".to_string())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("Fortuna should live inside Fortuna")
        .to_path_buf()
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
