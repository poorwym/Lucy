use std::env;
use std::net::SocketAddr;
use std::process::ExitCode;

use lucy_core::source::DEFAULT_CONFIG_PATH;
use lucy_core::subtree::{generate_root_subtree_bytes, generate_root_subtree_json};
use lucy_core::tile::TileCoord;
use lucy_core::tileset::{TilesetOptions, generate_tileset_json};
use lucy_server::{DEFAULT_ADDR, load_source_catalog, run_server};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let mut args = env::args().skip(1);
    let first_arg = args.next();

    if first_arg.as_deref() == Some("serve") {
        let config_path = args
            .next()
            .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());
        let addr_text = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
        let addr = match addr_text.parse::<SocketAddr>() {
            Ok(addr) => addr,
            Err(error) => {
                error!(address = %addr_text, error = %error, "invalid listen address");
                return ExitCode::FAILURE;
            }
        };

        return match run_server(&config_path, addr).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                error!(error = %error, "server stopped with an error");
                ExitCode::FAILURE
            }
        };
    }

    let config_path = first_arg.unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());

    match load_source_catalog(&config_path) {
        Ok(catalog) => {
            info!(
                config_path = %config_path,
                source_count = catalog.sources.len(),
                "source catalog loaded"
            );
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
                        warn!(source_id, error = %error, "failed to calculate root region")
                    }
                }

                match generate_tileset_json(&source, &TilesetOptions::default()) {
                    Ok(tileset_json) => println!("{source_id}: tileset.json\n{tileset_json}"),
                    Err(error) => warn!(source_id, error = %error, "failed to generate tileset"),
                }

                match generate_root_subtree_json(&source) {
                    Ok(subtree_json) => println!("{source_id}: root.subtree.json\n{subtree_json}"),
                    Err(error) => {
                        warn!(source_id, error = %error, "failed to generate subtree JSON")
                    }
                }

                match generate_root_subtree_bytes(&source) {
                    Ok(subtree_bytes) => {
                        println!("{source_id}: root.subtree bytes={}", subtree_bytes.len())
                    }
                    Err(error) => {
                        warn!(source_id, error = %error, "failed to generate subtree bytes")
                    }
                }
            }

            ExitCode::SUCCESS
        }
        Err(error) => {
            error!(error = %error, config_path = %config_path, "failed to load source catalog");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("lucy_poc=info,lucy_server=info,lucy_core=warn,tower_http=info")
    });

    if std::env::var("LUCY_LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .flatten_event(true)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .compact()
            .init();
    }
}
