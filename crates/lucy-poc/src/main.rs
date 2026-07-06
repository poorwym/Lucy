use std::env;
use std::net::SocketAddr;
use std::process::ExitCode;

use lucy_poc::server::{DEFAULT_POC_ADDR, run_poc_server};
use lucy_poc::subtree::{generate_root_subtree_bytes, generate_root_subtree_json};
use lucy_poc::tile::TileCoord;
use lucy_poc::tileset::{TilesetOptions, generate_tileset_json};
use lucy_poc::{DEFAULT_CONFIG_PATH, SourceCatalog};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let first_arg = args.next();

    if first_arg.as_deref() == Some("serve") {
        let config_path = args
            .next()
            .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());
        let addr_text = args.next().unwrap_or_else(|| DEFAULT_POC_ADDR.to_string());
        let addr = match addr_text.parse::<SocketAddr>() {
            Ok(addr) => addr,
            Err(error) => {
                eprintln!("invalid listen address {addr_text}: {error}");
                return ExitCode::FAILURE;
            }
        };

        return match run_poc_server(&config_path, addr) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }

    let config_path = first_arg.unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());

    match SourceCatalog::load(&config_path) {
        Ok(catalog) => {
            println!(
                "Loaded {} source(s) from {}",
                catalog.sources.len(),
                config_path
            );

            for (source_id, source) in catalog.sources {
                let base_height = source
                    .base_height_column_or_default()
                    .unwrap_or("<default 0.0>");

                println!(
                    "{source_id}: {}.{} geom={} id={} srid={} model={:?} z={}+{} levels={}..{} subtree_levels={}",
                    source.schema,
                    source.table,
                    source.geometry_column,
                    source.id_column,
                    source.srid,
                    source.source_model,
                    base_height,
                    source.height_column,
                    source.min_level,
                    source.max_level,
                    source.subtree_levels
                );

                match TileCoord::root().tiles_region(&source.bounds) {
                    Ok(region) => println!("{source_id}: root_region={:?}", region.as_array()),
                    Err(error) => {
                        eprintln!("{source_id}: failed to calculate root region: {error}")
                    }
                }

                match generate_tileset_json(&source, &TilesetOptions::default()) {
                    Ok(tileset_json) => println!("{source_id}: tileset.json\n{tileset_json}"),
                    Err(error) => eprintln!("{source_id}: failed to generate tileset: {error}"),
                }

                match generate_root_subtree_json(&source) {
                    Ok(subtree_json) => println!("{source_id}: root.subtree.json\n{subtree_json}"),
                    Err(error) => {
                        eprintln!("{source_id}: failed to generate subtree JSON: {error}")
                    }
                }

                match generate_root_subtree_bytes(&source) {
                    Ok(subtree_bytes) => {
                        println!("{source_id}: root.subtree bytes={}", subtree_bytes.len())
                    }
                    Err(error) => {
                        eprintln!("{source_id}: failed to generate subtree bytes: {error}")
                    }
                }
            }

            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
