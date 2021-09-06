use std::{thread, time::Duration};

use clap::{AppSettings, Clap};
use crossbeam::channel::{unbounded, Sender};
use insanity::{tui::TuiEvent, InsanityConfig};

#[derive(Clap)]
#[clap(version = "0.1.0", author = "Nicolas Chan <nicolas@nicolaschan.com>")]
#[clap(setting = AppSettings::ColoredHelp)]
struct Opts {
    #[clap(short, long)]
    denoise: bool,

    #[clap(long)]
    music: Option<String>,

    #[clap(short, long, default_value = "0.0.0.0:1337")]
    bind_address: String,

    #[clap(short, long)]
    peer_address: Vec<String>,

    #[clap(long)]
    id: Option<String>,

    #[clap(long)]
    no_tui: bool,

    #[clap(long, default_value = "48000")]
    sample_rate: usize,

    #[clap(long, default_value = "2")]
    channels: usize,
}

fn main() {
    let opts: Opts = Opts::parse();

    let _id: String = match opts.id {
        Some(id) => id,
        None => nanoid::nanoid!(),
    };

    let (ui_message_sender, ui_message_receiver) = unbounded();

    let config = InsanityConfig {
        denoise: opts.denoise,
        ui_message_sender: ui_message_sender.clone(),
        music: opts.music,
        sample_rate: opts.sample_rate,
        channels: opts.channels,
    };

    let config_clone = config.clone();
    let bind_address = opts.bind_address;
    thread::spawn(move || {
        insanity::server::start_server(bind_address, config_clone);
    });

    for peer_address in opts.peer_address {
        let config_clone = config.clone();
        thread::spawn(move || {
            insanity::client::start_client(peer_address, config_clone);
        });
    }

    if opts.no_tui {
        loop {
            std::thread::sleep(Duration::from_millis(1000));
        }
    } else {
        insanity::tui::start(ui_message_sender, ui_message_receiver);
    }
}
