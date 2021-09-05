use std::thread;

use clap::{AppSettings, Clap};
use crossbeam::channel::unbounded;

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

    #[clap(short, long)]
    output_device: Option<usize>,

    #[clap(long)]
    id: Option<String>,
    
    #[clap(long)]
    no_tui: bool,
}

fn main() {
    let opts: Opts = Opts::parse();

    let _id: String = match opts.id {
        Some(id) => id,
        None => nanoid::nanoid!(),
    };

    let (ui_message_sender, ui_message_receiver) = unbounded();

    let bind_address = opts.bind_address;
    let denoise_clone = opts.denoise.clone();
    let music_clone = opts.music.clone();
    let ui_message_sender_clone = ui_message_sender.clone();
    thread::spawn(move || {
        insanity::server::start_server(bind_address, denoise_clone, music_clone, ui_message_sender_clone);
    });

    for peer_address in opts.peer_address {
        let output_device_clone = opts.output_device.clone();
        let denoise_clone = opts.denoise.clone();
        let ui_message_sender_clone = ui_message_sender.clone();
        thread::spawn(move || {
            insanity::client::start_client(peer_address, output_device_clone, denoise_clone, ui_message_sender_clone);
        });
    }

    if opts.no_tui {
        loop {}
    } else {
        insanity::tui::start(ui_message_sender, ui_message_receiver);
    }
}
