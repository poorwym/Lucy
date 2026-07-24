mod cli;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

use cli::{Action, Defaults, HELP};
use lucy_core::source::StartupValidation;
use lucy_server::{load_source_catalog, run_server, validate_catalog_sources_with_mode};
use tracing::error;
use tracing_subscriber::EnvFilter;

const EXIT_RUNTIME_ERROR: u8 = 1;
const EXIT_USAGE_ERROR: u8 = 2;

#[tokio::main]
async fn main() -> ExitCode {
    let action = match cli::parse(std::env::args().skip(1), Defaults::from_env()) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("error: {error}\n\n{HELP}");
            return ExitCode::from(EXIT_USAGE_ERROR);
        }
    };

    match action {
        Action::Help(help) => {
            println!("{help}");
            ExitCode::SUCCESS
        }
        Action::Version => {
            println!("lucy {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Action::Serve { config_path, bind } => {
            init_tracing();
            match run_server(&config_path, bind).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    error!(error = %error, "server stopped with an error");
                    ExitCode::from(EXIT_RUNTIME_ERROR)
                }
            }
        }
        Action::Validate {
            config_path,
            source_id,
        } => {
            init_tracing();
            validate(config_path, source_id).await
        }
        Action::Healthcheck { address } => match healthcheck(address) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("healthcheck failed: {error}");
                ExitCode::from(EXIT_RUNTIME_ERROR)
            }
        },
    }
}

async fn validate(config_path: String, source_id: Option<String>) -> ExitCode {
    let catalog = match load_source_catalog(&config_path) {
        Ok(catalog) => catalog,
        Err(error) => {
            error!(error = %error, config_path, "failed to load source catalog");
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    };

    match validate_catalog_sources_with_mode(
        &catalog,
        StartupValidation::Full,
        source_id.as_deref(),
    )
    .await
    {
        Ok(()) => {
            println!(
                "Fully validated {} source(s) from {}",
                if source_id.is_some() {
                    1
                } else {
                    catalog.sources.len()
                },
                config_path
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            error!(error = %error, config_path, ?source_id, "source validation failed");
            ExitCode::from(EXIT_RUNTIME_ERROR)
        }
    }
}

fn healthcheck(address: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let timeout = Duration::from_secs(2);
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;

    let mut response = [0_u8; 64];
    let bytes_read = stream.read(&mut response)?;
    let status = &response[..bytes_read];
    if is_healthy_response(status) {
        Ok(())
    } else {
        Err(format!(
            "GET http://{address}/health returned {:?}",
            String::from_utf8_lossy(status)
        )
        .into())
    }
}

fn is_healthy_response(response: &[u8]) -> bool {
    response.starts_with(b"HTTP/1.1 200") || response.starts_with(b"HTTP/1.0 200")
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("lucy=info,lucy_server=info,lucy_core=warn,tower_http=info")
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

#[cfg(test)]
mod tests {
    use super::is_healthy_response;

    #[test]
    fn image_healthcheck_accepts_only_http_success() {
        assert!(is_healthy_response(b"HTTP/1.1 200 OK\r\n"));
        assert!(is_healthy_response(b"HTTP/1.0 200 OK\r\n"));
        assert!(!is_healthy_response(
            b"HTTP/1.1 503 Service Unavailable\r\n"
        ));
    }
}
