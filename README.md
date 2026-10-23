# Fortuna

This app brings together tools for evaluating crafting profit, gathering value, and macro generation for FINAL FANTASY XIV.

This was created with the assistance of AI, but tested throughly by humans.

## What this project does

The main app in this workspace is `Fortuna`, a desktop GUI built with Rust and `eframe/egui`.

It helps FFXIV crafters and gatherers by:

- Ranking craftable items by profit.
- Ranking gatherable items by profit and NPC vendor buy price.
- Showing recent sale counts and daily velocity.
- Ranking by profit or by sales in the past 4 days.
- Explaining craft cost breakdowns across ingredient trees.
- Generating optimized crafting macros using the `raphael-rs` solver.

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

`Fortuna` lets you select a Logical Data Center and World from the catalog based on the [Lodestone World Status page](https://eu.finalfantasyxiv.com/lodestone/worldstatus/), then refreshes Universalis data only for that selected world. The first-run default is Light / Phoenix, and the selected combination is saved for the next launch.

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

Universalis market snapshots and sale-history caches are stored separately for each combination under `Cache/Universalis/<Data Center>/<World>/`. Fortuna loads the selected world’s saved snapshot on startup or when you switch worlds, and only contacts Universalis for the selected world when you click the refresh button. The selected combination is stored in `Cache/Universalis/selected_server.json`. The existing top-level Phoenix snapshot and sale history are used as a fallback for Light / Phoenix until that server is refreshed. On refresh, Fortuna requests sale history across Universalis' 10-day availability window and archives fetched entries locally for that server, while the app's existing sales metrics and chart continue to use up to 7 days. Refresh regularly to capture data before it expires from Universalis.

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

- To use Fortuna, open `Fortuna - Shortcut.lnk`. You do not need to run commands or set up the development environment.
- The app defaults to the `Phoenix` world on the `Light` data center and remembers the selected data center and world between launches.
- The UI includes optional buttons to refresh market/game data or ensure supporting repositories when needed; these are not required just to launch the app.
- When a new patch is released with new items, you will want to refresh the SaintCoinach and Lodestone data through the buttons on the app.
- Cached game data files are regenerable and safe to delete; the app will recreate them when refreshed.
- The SaintCoinach exporter needs the .NET SDK and a local FFXIV install. Fortuna auto-detects common install locations; if it cannot, choose the install folder in the Sources section. The selected folder is saved in `Cache/SaintCoinach/selected_game_dir.json`.

### Be mindful of API usage

Please refresh only when you need current data. Universalis and Lodestone are external services, and their availability and usage limits are outside Fortuna's control. If requests are blocked or limited because of excessive usage, wait before trying again and follow the service's guidance; Fortuna cannot remove or bypass those limits.

## Development

This workspace is primarily a Rust-based toolchain with local workspace dependencies.

- `Fortuna/` depends on the local `Source/raphael-rs` workspace crates.
- `Cache/SaintCoinach/` and `Cache/Universalis/` are local cache directories, not compiled crates.
- External source checkouts live under `Source/`.

If you want to extend the app, start with `Fortuna/src/main.rs` and the local `Source/raphael-rs` crates.
