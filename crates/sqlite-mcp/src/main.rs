use sqlite_mcp::serve_with_transport_and_ct;
use sqlite_mcp_core::Config;
use std::{env, path::PathBuf, process::ExitCode};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Serve { config: Option<PathBuf> },
    Help,
    Version,
}

fn usage() -> &'static str {
    "Usage: sqlite-mcp [--config ABSOLUTE_PATH]\n\nRun the SQLite MCP server over stdio.\n\nOptions:\n    --config PATH   Load and validate TOML configuration\n    -h, --help      Show this help\n    -V, --version   Show version\n"
}

fn parse_args<I>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = String>,
{
    let mut config = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--config" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--config requires a path".to_owned())?;
                config = Some(PathBuf::from(value));
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => return Err(format!("unexpected argument: {value}")),
        }
    }
    Ok(Command::Serve { config })
}

#[tokio::main]
async fn main() -> ExitCode {
    let command = match parse_args(env::args().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("sqlite-mcp: {error}\n{}", usage());
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Help => {
            print!("{}", usage());
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("sqlite-mcp {VERSION}");
            ExitCode::SUCCESS
        }
        Command::Serve { config } => {
            let loaded = match config {
                Some(path) => {
                    if !path.is_absolute() {
                        eprintln!("sqlite-mcp: --config requires an absolute path");
                        return ExitCode::from(2);
                    }
                    match Config::from_path(&path) {
                        Ok(config) => config,
                        Err(error) => {
                            eprintln!("sqlite-mcp: invalid configuration: {error}");
                            return ExitCode::from(2);
                        }
                    }
                }
                None => Config::default(),
            };
            if let Err(error) = loaded.validate() {
                eprintln!("sqlite-mcp: invalid configuration: {error}");
                return ExitCode::from(2);
            }
            let shutdown_ct = tokio_util::sync::CancellationToken::new();
            let serve_ct = shutdown_ct.clone();
            let mut serve_task = tokio::spawn(async move {
                serve_with_transport_and_ct(loaded, rmcp::transport::io::stdio(), Some(serve_ct))
                    .await
            });
            // Ctrl-C joins the same cleanup path: the shutdown token ends the
            // serving session, core cleanup always runs, and a clean
            // cancellation with clean cleanup still exits zero.
            let join_outcome = tokio::select! {
                outcome = &mut serve_task => outcome,
                _ = tokio::signal::ctrl_c() => {
                    shutdown_ct.cancel();
                    serve_task.await
                }
            };
            let outcome = join_outcome.unwrap_or_else(|error| {
                Err(sqlite_mcp::ServeFailure {
                    primary: format!("serve task failed during shutdown: {error}"),
                    cleanup_errors: Vec::new(),
                })
            });
            let code: i32 = match outcome {
                Ok(()) => 0,
                Err(failure) => {
                    eprintln!("sqlite-mcp: {failure}");
                    for error in &failure.cleanup_errors {
                        eprintln!("sqlite-mcp: cleanup failure: {error}");
                    }
                    1
                }
            };
            // Every ordered-cleanup step (service join, transport drain, core
            // rollback/close/join) has completed above, so the runtime
            // destructor has no useful work left. Exit explicitly instead of
            // dropping it: `tokio::io::stdin()` runs its read on the blocking
            // pool, that read cannot be cancelled, and dropping the runtime
            // would block forever waiting for it after SIGINT. Flush the
            // protocol stream first so no buffered stdout is lost.
            use std::io::Write;
            let _ = std::io::stdout().flush();
            std::process::exit(code);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_serve_command() {
        assert_eq!(
            parse_args(Vec::<String>::new()),
            Ok(Command::Serve { config: None })
        );
    }

    #[test]
    fn parses_config_command() {
        assert_eq!(
            parse_args(["--config".to_owned(), "settings.toml".to_owned()]),
            Ok(Command::Serve {
                config: Some(PathBuf::from("settings.toml"))
            })
        );
    }

    #[test]
    fn explicit_help_and_version_are_commands() {
        assert_eq!(parse_args(["--help".to_owned()]), Ok(Command::Help));
        assert_eq!(parse_args(["--version".to_owned()]), Ok(Command::Version));
    }

    #[test]
    fn unknown_options_fail_without_starting_stdio() {
        assert!(parse_args(["--nope".to_owned()]).is_err());
    }
}
