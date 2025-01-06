use std::{path::PathBuf, str::FromStr, time::Duration};

use clap::{parser::ValueSource, ArgMatches, CommandFactory, Parser, Subcommand};
use insanity_core::built_info;
use insanity_native_tui_app::{
    connection_manager::ConnectionManager, connection_manager::IpVersion, update,
};
use insanity_tui_adapter::AppEvent;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

// Update this number if there is a breaking change.
// This will cause the insanity directory to be renewed.
const BREAKING_CHANGE_VERSION: &str = "1";

// Use INSANITY_CONFIG_LOCATION.iter().collect::<PathBuf>() to create cross-platform path.
const INSANITY_CONFIG_LOCATION: [&str; 2] = ["insanity", "config.toml"];

#[derive(Parser, Debug)]
#[clap(version = built_info::GIT_VERSION, author = "Nicolas Chan <nicolas@nicolaschan.com>")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Listen port.
    #[clap(short, long, default_value_t = 0)]
    port: u16,

    /// Disables the terminal user interface.
    #[clap(long)]
    no_tui: bool,

    /// Directory to store insanity data.
    #[clap(long)]
    dir: Option<String>,

    /// Path to config file.
    #[clap(short, long)]
    config_file: Option<String>,

    /// Bridge server.
    #[clap(long)]
    bridge: Vec<String>,

    /// Room name to join.
    #[clap(long)]
    room: Option<String>,

    /// ipv4, ipv6, or dualstack
    #[clap(long, value_enum, default_value_t = IpVersion::Dualstack)]
    ip_version: IpVersion,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Connect to room [default]
    Run,
    /// Update insanity.
    Update {
        #[clap(long, default_value_t = false)]
        dry_run: bool,

        #[clap(long, default_value_t = false)]
        force: bool,
    },
    /// Print contents of the file (if any) being used for configuration.
    PrintConfigFile,
}

#[derive(Debug)]
struct RunOptions {
    port: u16,
    no_tui: bool,
    bridge: Vec<String>,
    room: Option<String>,
    ip_version: IpVersion,
}

// RunOptions that can be specified via config file
// Has to exclude: config file path, dir
#[derive(Deserialize, Debug, Default)]
struct OptionalRunOptions {
    port: Option<u16>,
    no_tui: Option<bool>,
    bridge: Option<Vec<String>>,
    room: Option<Option<String>>,
    ip_version: Option<IpVersion>,
}

/// Merges two configuration options, with priority as follows:
/// 1. Explicitly specified options in primary; 2. Secondary; 3. Default value (stored in primary)
fn merge_values<T>(primary: T, secondary: Option<T>, value_source: Option<ValueSource>) -> T {
    match (value_source, secondary) {
        (_, None) => primary,
        (None | Some(ValueSource::DefaultValue), Some(value)) => value,
        (Some(_), _) => primary,
    }
}

// TODO: Can this be cleaned up using a macro?
fn merge_configs(primary: Cli, secondary: OptionalRunOptions, matches: &ArgMatches) -> RunOptions {
    RunOptions {
        port: merge_values(primary.port, secondary.port, matches.value_source("port")),
        no_tui: merge_values(
            primary.no_tui,
            secondary.no_tui,
            matches.value_source("no_tui"),
        ),
        bridge: merge_values(
            primary.bridge,
            secondary.bridge,
            matches.value_source("bridge"),
        ),
        room: merge_values(primary.room, secondary.room, matches.value_source("room")),
        ip_version: merge_values(
            primary.ip_version,
            secondary.ip_version,
            matches.value_source("ip_version"),
        ),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli_opts: Cli = Cli::parse();

    match cli_opts.command {
        None | Some(Commands::Run) => run(cli_opts).await,
        Some(Commands::Update { dry_run, force }) => update::update(dry_run, force).await,
        Some(Commands::PrintConfigFile) => Ok(print_config_file(cli_opts.config_file)),
    }
}

fn get_config_file_path(config_file_path_arg: Option<&String>) -> PathBuf {
    match config_file_path_arg {
        Some(ref path) => PathBuf::from_str(&path).unwrap(),
        None => dirs::config_local_dir()
            .expect("No config directory!?")
            .join(INSANITY_CONFIG_LOCATION.iter().collect::<PathBuf>()),
    }
}

fn print_config_file(config_file_path: Option<String>) {
    let config_file_path = get_config_file_path(config_file_path.as_ref());
    match std::fs::read_to_string(config_file_path) {
        Ok(string) => {
            println!("{}", string);
        }
        Err(e) => {
            println!("Error reading config file: {e}");
        }
    };
}

async fn run(unprocessed_opts: Cli) -> anyhow::Result<()> {
    let main_cancellation_token = CancellationToken::new();

    // Configure insanity data directory
    let insanity_dir = match unprocessed_opts.dir {
        Some(ref dir) => PathBuf::from_str(&dir).unwrap(),
        None => dirs::data_local_dir()
            .expect("no data directory!?")
            .join("insanity"),
    };
    renew_dir(&insanity_dir)?;

    // Setup logging
    let log_path = insanity_dir.join("insanity.log");
    println!("Logging to {:?}", log_path);
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%Y-%m-%d %H:%M:%S]"),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Debug)
        .chain(fern::log_file(log_path).expect("could not create insanity log file"))
        .apply()
        .expect("could not setup logging");

    log::info!("Starting insanity");

    // Read config file.
    let config_file_path = get_config_file_path(unprocessed_opts.config_file.as_ref());
    log::debug!("Config file path: {:?}", config_file_path);
    let config_file: OptionalRunOptions = match std::fs::read_to_string(config_file_path) {
        Ok(string) => toml::from_str(&string).expect("Failed to deserialize config file."),
        Err(e) => {
            log::debug!("Error reading config file: {e}");
            OptionalRunOptions::default()
        }
    };

    // Merge configs
    let opts = merge_configs(unprocessed_opts, config_file, &Cli::command().get_matches());

    let display_name = format!(
        "{} [{}]",
        whoami::realname(),
        whoami::fallible::hostname().unwrap_or(String::from("unknown"))
    );

    let (app_event_sender, user_action_receiver, handle) = if !opts.no_tui {
        let (x, y, z) = insanity_tui_adapter::start_tui().await.unwrap();
        x.send(AppEvent::SetServer(opts.bridge.clone()))?;
        if let Some(room) = opts.room.clone() {
            x.send(AppEvent::SetRoom(room))?;
        }
        x.send(AppEvent::SetOwnDisplayName(display_name.clone()))?;
        (Some(x), Some(y), Some(z))
    } else {
        (None, None, None)
    };

    // Start connection manager
    let mut conn_manager_builder =
        ConnectionManager::builder(insanity_dir, opts.port, opts.bridge, opts.ip_version)
            .display_name(display_name)
            .cancellation_token(main_cancellation_token.clone());
    if let Some(room) = opts.room {
        conn_manager_builder = conn_manager_builder.room(room);
    }
    if let Some(app_event_sender) = app_event_sender {
        conn_manager_builder = conn_manager_builder.app_event_sender(app_event_sender)
    }
    let connection_manager = conn_manager_builder.start().await?;

    if let Some(mut user_action_rx) = user_action_receiver {
        // Forward user actions to connection manager.
        tokio::spawn(async move {
            while let Some(action) = user_action_rx.recv().await {
                if let Err(e) = connection_manager.send_user_action(action) {
                    log::debug!("Failed to send user action to connection manager: {:?}", e);
                }
            }
        });
    }

    if let Some(handle) = handle {
        insanity_tui_adapter::stop_tui(handle).await.unwrap();
    } else {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    // TODO: Maybe should wait for tasks to shutdown, but who cares?
    main_cancellation_token.cancel();
    tokio::time::sleep(Duration::from_millis(10)).await;

    Ok(())
}

fn renew_dir(dir: &PathBuf) -> anyhow::Result<()> {
    let version_file = dir.join("version");
    let version = match std::fs::read_to_string(&version_file) {
        Ok(v) => v,
        Err(_) => String::from("0"),
    };

    if version != BREAKING_CHANGE_VERSION {
        log::info!("Renewing insanity directory: found version {version} but code uses {BREAKING_CHANGE_VERSION}");
        if let Err(e) = std::fs::remove_dir_all(dir) {
            log::debug!("Error on removing directory. Continuing anyway. Error {e}");
        }
    }

    std::fs::create_dir_all(dir)?;
    std::fs::write(&version_file, BREAKING_CHANGE_VERSION)?;
    Ok(())
}
