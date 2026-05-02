# Fortuna

This workspace brings together tools for evaluating crafting profit, gathering value, and macro generation for FINAL FANTASY XIV.

## What this project does

The main app in this workspace is `Fortuna`, a desktop GUI built with Rust and `eframe/egui`.

It helps FFXIV crafters and gatherers by:

- ranking craftable items by profit,
- ranking gatherable items by profit and NPC vendor buy price,
- showing recent sale counts and daily velocity,
- explaining craft cost breakdowns across ingredient trees,
- generating optimized crafting macros using the `raphael-rs` solver.

## Key components

- `Fortuna/`
  - Native GUI application that loads FFXIV recipe data, market prices, and item metadata.
  - Uses `raphael-data`, `raphael-sim`, and `raphael-solver` from the local `raphael-rs` workspace.
  - Fetches market data from Universalis and caches XIVAPI lookup data locally.

- `raphael-rs/`
  - Crafting rotation solver for FFXIV.
  - Provides the game data, simulation engine, and solver logic used by `Fortuna`.
  - Includes a CLI and other Rust workspace crates.

- `Universalis/`
  - Full FFXIV market board aggregator project.
  - `Fortuna` uses the public Universalis API for live market and history data.

- `xivapi-cache/`
  - Local cache area for XIVAPI lookup assets.
  - Stores generated files used by `Fortuna` to reduce network requests and speed startup.

## How information is gathered

### Market data

`Fortuna` refreshes market data from Universalis using the configured world and data center.

- Aggregated current price data is fetched from the Universalis aggregated API endpoint.
- Recent sale history is fetched from the Universalis history API endpoint.
- The app loads all relevant output and ingredient item IDs from `raphael-data` recipes.

### Item metadata and vendor prices

To enrich market data, `Fortuna` loads or updates a local XIVAPI cache in `xivapi-cache/`.

The cache includes:

- `cache_manifest.json`
- `item_categories.json`
- `item_metadata.json`
- `vendor_prices.json`

When the cache is missing, incomplete, or built for an older market-visible item set, the app fetches these from XIVAPI v2. Otherwise, it reuses the stored JSON files and skips the XIVAPI refresh. Fortuna checks Universalis first and only asks XIVAPI for items that currently have market data:

- item search categories and display names,
- item metadata for Universalis-visible recipe and ingredient IDs,
- NPC gil vendor prices from item metadata for those same IDs.

This metadata is merged with live Universalis pricing so the app can:

- bucket items by XIVAPI search categories,
- compare market price vs. vendor price,
- calculate whether to buy ingredients or craft them,
- explain cost sources in the recipe tree.

### Crafting and macro solving

`Fortuna` uses `raphael-rs` to solve macros for selected recipes.

- The app loads recipe and crafter stats from `raphael-data`.
- It ranks items by profit using a combination of market price, ingredient cost, and recipe dependencies.
- When solving a macro, it calls the local Raphael solver to generate optimized craft rotations.

## Usage notes

- The app is configured for the `Phoenix` world and `Light` data center by default.
- The UI includes buttons to clone missing repositories (`raphael-rs` and `Universalis`) and update the local XIVAPI cache.
- Cached XIVAPI files are regenerable and safe to delete; the app will recreate them when refreshed.

## Development

This workspace is primarily a Rust-based toolchain with local workspace dependencies.

- `Fortuna/` depends on the local `raphael-rs` workspace crates.
- `xivapi-cache/` is a local cache directory, not a compiled crate.
- `Universalis/` is included as a reference archive of the market aggregator project.

If you want to run or extend the app, start with `Fortuna/src/main.rs` and the local `raphael-rs` crates.
