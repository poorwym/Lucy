use std::env;
use std::process::ExitCode;

use lucy_poc::{DEFAULT_CONFIG_PATH, SourceCatalog};

fn main() -> ExitCode {
    let config_path = env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());

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
            }

            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
