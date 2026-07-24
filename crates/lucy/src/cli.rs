use std::env;
use std::fmt;
use std::net::SocketAddr;

use lucy_core::source::DEFAULT_CONFIG_PATH;
use lucy_server::DEFAULT_ADDR;

pub const HELP: &str = "Lucy 3D Tiles server

Usage: lucy <COMMAND>

Commands:
  serve       Serve 3D Tiles from configured PostGIS sources
  validate    Fully validate one or all configured sources

Options:
  -h, --help       Print help
  -V, --version    Print version

Run `lucy <COMMAND> --help` for command-specific options.";

pub const SERVE_HELP: &str = "Serve 3D Tiles from configured PostGIS sources

Usage: lucy serve [OPTIONS]

Options:
      --config <PATH>    Source catalog [env: LUCY_CONFIG; default: lucy.yaml]
      --bind <ADDRESS>   Listen address [env: LUCY_BIND; default: 127.0.0.1:8080]
  -h, --help             Print help";

pub const VALIDATE_HELP: &str = "Fully validate one or all configured PostGIS sources

Usage: lucy validate [OPTIONS] [SOURCE_ID]

Arguments:
  [SOURCE_ID]    Validate only this source; all sources are validated when omitted

Options:
      --config <PATH>    Source catalog [env: LUCY_CONFIG; default: lucy.yaml]
  -h, --help             Print help";

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Help(&'static str),
    Version,
    Serve {
        config_path: String,
        bind: SocketAddr,
    },
    Validate {
        config_path: String,
        source_id: Option<String>,
    },
    Healthcheck {
        address: SocketAddr,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Defaults {
    config_path: String,
    bind: String,
    health_address: String,
}

impl Defaults {
    pub fn from_env() -> Self {
        Self {
            config_path: env::var("LUCY_CONFIG")
                .unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string()),
            bind: env::var("LUCY_BIND").unwrap_or_else(|_| DEFAULT_ADDR.to_string()),
            health_address: env::var("LUCY_HEALTH_ADDRESS")
                .unwrap_or_else(|_| "127.0.0.1:8080".to_string()),
        }
    }

    #[cfg(test)]
    fn test() -> Self {
        Self {
            config_path: DEFAULT_CONFIG_PATH.to_string(),
            bind: DEFAULT_ADDR.to_string(),
            health_address: "127.0.0.1:8080".to_string(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CliError(String);

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub fn parse(
    args: impl IntoIterator<Item = String>,
    defaults: Defaults,
) -> Result<Action, CliError> {
    let args = args.into_iter().collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        return Err(CliError("a command is required".to_string()));
    };

    match command {
        "-h" | "--help" => no_extra_args(&args, Action::Help(HELP)),
        "-V" | "--version" => no_extra_args(&args, Action::Version),
        "serve" => parse_serve(&args[1..], defaults),
        "validate" => parse_validate(&args[1..], defaults),
        // Reserved for the image HEALTHCHECK and intentionally omitted from
        // the user-facing command list and compatibility contract.
        "__healthcheck" => parse_healthcheck(&args[1..], defaults),
        unknown => Err(CliError(format!("unknown command {unknown:?}"))),
    }
}

fn no_extra_args(args: &[String], action: Action) -> Result<Action, CliError> {
    if let Some(unexpected) = args.get(1) {
        Err(CliError(format!("unexpected argument {unexpected:?}")))
    } else {
        Ok(action)
    }
}

fn parse_serve(args: &[String], defaults: Defaults) -> Result<Action, CliError> {
    if args
        .iter()
        .any(|argument| matches!(argument.as_str(), "-h" | "--help"))
    {
        return Ok(Action::Help(SERVE_HELP));
    }

    let mut config_path = defaults.config_path;
    let mut bind = defaults.bind;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => config_path = option_value(args, &mut index, "--config")?,
            "--bind" => bind = option_value(args, &mut index, "--bind")?,
            argument if argument.starts_with("--config=") => {
                config_path = inline_option_value(argument, "--config")?;
            }
            argument if argument.starts_with("--bind=") => {
                bind = inline_option_value(argument, "--bind")?;
            }
            unexpected => return Err(CliError(format!("unexpected argument {unexpected:?}"))),
        }
        index += 1;
    }

    let bind = parse_address(&bind, "listen address")?;
    Ok(Action::Serve { config_path, bind })
}

fn parse_validate(args: &[String], defaults: Defaults) -> Result<Action, CliError> {
    if args
        .iter()
        .any(|argument| matches!(argument.as_str(), "-h" | "--help"))
    {
        return Ok(Action::Help(VALIDATE_HELP));
    }

    let mut config_path = defaults.config_path;
    let mut source_id = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => config_path = option_value(args, &mut index, "--config")?,
            argument if argument.starts_with("--config=") => {
                config_path = inline_option_value(argument, "--config")?;
            }
            unexpected if unexpected.starts_with('-') => {
                return Err(CliError(format!("unexpected option {unexpected:?}")));
            }
            value if source_id.is_none() => source_id = Some(value.to_string()),
            unexpected => return Err(CliError(format!("unexpected argument {unexpected:?}"))),
        }
        index += 1;
    }

    Ok(Action::Validate {
        config_path,
        source_id,
    })
}

fn parse_healthcheck(args: &[String], defaults: Defaults) -> Result<Action, CliError> {
    let address = match args {
        [] => defaults.health_address,
        [option, value] if option == "--address" => value.clone(),
        [option] if option.starts_with("--address=") => inline_option_value(option, "--address")?,
        [unexpected, ..] => {
            return Err(CliError(format!(
                "unexpected healthcheck argument {unexpected:?}"
            )));
        }
    };
    Ok(Action::Healthcheck {
        address: parse_address(&address, "healthcheck address")?,
    })
}

fn option_value(args: &[String], index: &mut usize, option: &str) -> Result<String, CliError> {
    *index += 1;
    args.get(*index)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| CliError(format!("{option} requires a value")))
}

fn inline_option_value(argument: &str, option: &str) -> Result<String, CliError> {
    let value = argument
        .split_once('=')
        .map(|(_, value)| value)
        .unwrap_or_default();
    if value.is_empty() {
        Err(CliError(format!("{option} requires a value")))
    } else {
        Ok(value.to_string())
    }
}

fn parse_address(value: &str, label: &str) -> Result<SocketAddr, CliError> {
    value
        .parse()
        .map_err(|error| CliError(format!("invalid {label} {value:?}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_serve_contract_and_defaults() {
        assert_eq!(
            parse(args(&["serve"]), Defaults::test()).unwrap(),
            Action::Serve {
                config_path: "lucy.yaml".to_string(),
                bind: "127.0.0.1:8080".parse().unwrap(),
            }
        );
        assert_eq!(
            parse(
                args(&["serve", "--config", "sources.yaml", "--bind=0.0.0.0:9000"]),
                Defaults::test(),
            )
            .unwrap(),
            Action::Serve {
                config_path: "sources.yaml".to_string(),
                bind: "0.0.0.0:9000".parse().unwrap(),
            }
        );
    }

    #[test]
    fn parses_validate_contract() {
        assert_eq!(
            parse(
                args(&["validate", "--config", "sources.yaml", "buildings"]),
                Defaults::test(),
            )
            .unwrap(),
            Action::Validate {
                config_path: "sources.yaml".to_string(),
                source_id: Some("buildings".to_string()),
            }
        );
    }

    #[test]
    fn rejects_legacy_positional_serve_arguments() {
        let error = parse(
            args(&["serve", "sources.yaml", "0.0.0.0:8080"]),
            Defaults::test(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unexpected argument"));
    }

    #[test]
    fn validates_addresses_before_startup() {
        let error = parse(
            args(&["serve", "--bind", "not-an-address"]),
            Defaults::test(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid listen address"));
    }
}
