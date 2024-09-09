use std::{path::PathBuf, str::FromStr, time::Duration};

use clap::{Parser, Subcommand};
use insanity_tui_adapter::AppEvent;
use insanity_tui_app::connection_manager::ConnectionManager;
use tokio_util::sync::CancellationToken;

// Update this number if there is a breaking change.
// This will cause the insanity directory to be renewed.
static BREAKING_CHANGE_VERSION: &str = "1";

#[derive(Parser, Debug)]
#[clap(author = "Nicolas Chan <nicolas@nicolaschan.com>")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Run(RunOptions),
    Update,
}

#[derive(Parser, Debug)]
struct RunOptions {
    #[clap(short, long, default_value = "1337")]
    port: u16,

    /// Address of peer to connect to.
    #[clap(short, long)]
    peer: Vec<String>,

    /// Disables the terminal user interface.
    #[clap(long)]
    no_tui: bool,

    /// Directory to store insanity data.
    #[clap(long)]
    dir: Option<String>,

    /// Bridge server.
    #[clap(long)]
    bridge: String,

    /// Room name to join.
    #[clap(long)]
    room: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts: Cli = Cli::parse();

    match opts.command {
        Commands::Run(run_opts) => run(run_opts).await,
        Commands::Update => update().await,
    }
}

async fn update() -> anyhow::Result<()> {
    todo!()
}

async fn run(opts: RunOptions) -> anyhow::Result<()> {
    let main_cancellation_token = CancellationToken::new();

    let insanity_dir = match opts.dir {
        Some(dir) => PathBuf::from_str(&dir).unwrap(),
        None => dirs::data_local_dir()
            .expect("no data directory!?")
            .join("insanity"),
    };
    renew_dir(&insanity_dir)?;

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
        ConnectionManager::builder(insanity_dir, opts.port, &opts.bridge)
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
