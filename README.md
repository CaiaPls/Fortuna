# Fortuna

This workspace brings together tools for evaluating crafting profit, gathering value, and macro generation for FINAL FANTASY XIV.

## What this project does

The main app in this workspace is `Fortuna`, a desktop GUI built with Rust and `eframe/egui`.

It helps FFXIV crafters and gatherers by:

- ranking craftable items by profit,
- ranking gatherable items by profit and NPC vendor buy price,
- showing recent sale counts and daily velocity,
- ranking by profit or by sales in the past 4 days,
- explaining craft cost breakdowns across ingredient trees,
- generating optimized crafting macros using the `raphael-rs` solver.

## Key components

- `Fortuna/`
  - Native GUI application that loads FFXIV recipe data, market prices, and item metadata.
  - Uses `raphael-data`, `raphael-sim`, and `raphael-solver` from the local `raphael-rs` workspace.
  - Fetches market data from Universalis and caches SaintCoinach lookup data locally.

- `Source/raphael-rs/`
  - Crafting rotation solver for FFXIV.
  - Provides the game data, simulation engine, and solver logic used by `Fortuna`.
  - Includes a CLI and other Rust workspace crates.

- `Source/Universalis/`
  - Full FFXIV market board aggregator project.
  - `Fortuna` uses the public Universalis API for live market and history data.

- `Source/SaintCoinach/`
  - Game-data reader used by the local Fortuna SaintCoinach exporter.

- `Cache/SaintCoinach/`
  - Local cache area for SaintCoinach-exported game lookup assets.
  - Stores generated files used by `Fortuna` to reduce network requests and speed startup.

- `Cache/Universalis/`
  - Local cache area for Universalis market snapshots.

- `Source/Fortuna.SaintCoinachExporter/`
  - Small .NET exporter that reads the local FFXIV install through SaintCoinach and writes Fortuna's JSON cache files.

## How information is gathered

### Market data

`Fortuna` refreshes market data from Universalis using the configured world and data center.

- Aggregated current price data is fetched from the Universalis aggregated API endpoint.
- Recent sale history is fetched from the Universalis history API endpoint.
- The app loads all relevant output and ingredient item IDs from `raphael-data` recipes.

### Item metadata, game data, and vendor prices

To enrich market data, `Fortuna` loads or updates local SaintCoinach exports in `Cache/SaintCoinach/`.

The cache includes:

- `cache_manifest.json`
- `gathering_items.json`
- `item_categories.json`
- `item_metadata.json`
- `maps.json`
- `market_snapshot.json`
- `npcs.json`
- `places.json`
- `recipe_materials.json`
- `drop_sources.json`
- `shops.json`
- `territories.json`
- `vendor_prices.json`

When the cache is missing, incomplete, or built for an older market-visible item set, the app uses the local SaintCoinach checkout and your local FFXIV install to regenerate these files. Otherwise, it reuses the stored JSON files and skips the export. Fortuna checks Universalis first and only asks the exporter for items that currently have market data, plus recipe materials:

- item search categories and display names,
- item metadata for Universalis-visible recipe and ingredient IDs,
- recipe material lists,
- optional mob and treasure drop sources when a Libra Eorzea database is supplied with `FORTUNA_LIBRA_DB`,
- gathering items,
- NPC, shop, map, territory, and place names,
- NPC gil vendor prices from item metadata.

Universalis market data is stored locally in `Cache/Universalis/market_snapshot.json` after a market refresh. Fortuna loads that snapshot on startup and only contacts Universalis again when you click the Universalis refresh button.

Drop sources can also be enriched from the Lodestone Eorzea Database with the separate `Refresh Lodestone drops` button. This is intentionally conservative: it only considers tradable market candidates that are not final crafted items, already gathered, or NPC-buyable, caches raw Lodestone pages under ignored `Cache/Lodestone/`, waits 5 seconds before uncached requests, checks up to 200 duty pages first, then drains unchecked item candidates in 200-page chunks until no unchecked candidates remain for that run. Checked pages are recorded so later refreshes resume instead of starting over. Duty-page scraping reads treasure coffer item lists from Lodestone duty categories such as dungeons, trials, raids, ultimate raids, and chaotic alliance raids; item-page scraping also catches mob `Dropped By` tables with overworld coordinates when Lodestone provides them.

This metadata is merged with live Universalis pricing so the app can:

- bucket items by item search categories,
- compare market price vs. vendor price,
- calculate whether to buy ingredients or craft them using SaintCoinach recipe materials when cached,
- explain cost sources in the recipe tree.

### Crafting and macro solving

`Fortuna` uses `raphael-rs` to solve macros for selected recipes.

- The app loads recipe and crafter stats from `raphael-data`.
- It ranks items by profit using a combination of market price, ingredient cost, and recipe dependencies.
- When solving a macro, it calls the local Raphael solver to generate optimized craft rotations.

## Usage notes

- The app is configured for the `Phoenix` world and `Light` data center by default.
- The UI includes buttons to clone missing repositories (`raphael-rs`, `Universalis`, and `SaintCoinach`) and update the local game data cache.
- Cached game data files are regenerable and safe to delete; the app will recreate them when refreshed.
- The SaintCoinach exporter needs the .NET SDK and a local FFXIV install. If Fortuna cannot auto-detect the install, set `FORTUNA_FFXIV_GAME_DIR` to the folder containing `game\ffxivgame.ver`.

## Development

This workspace is primarily a Rust-based toolchain with local workspace dependencies.

- `Fortuna/` depends on the local `Source/raphael-rs` workspace crates.
- `Cache/SaintCoinach/` and `Cache/Universalis/` are local cache directories, not compiled crates.
- External source checkouts live under `Source/`.

If you want to run or extend the app, start with `Fortuna/src/main.rs` and the local `Source/raphael-rs` crates.
