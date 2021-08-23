use std::thread;
use std::time::Duration;

use clap::{AppSettings, Clap};

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

    #[clap(short, long, default_value = "127.0.0.1:1338")]
    peer_address: Vec<String>,

    #[clap(short, long)]
    output_device: Option<usize>,
}

fn main() {
    let opts: Opts = Opts::parse();
    insanity::server::start_server(opts.bind_address, opts.music);

    for peer_address in opts.peer_address {
        insanity::client::start_client(peer_address, opts.output_device, opts.denoise);
    }

    loop {
        thread::sleep(Duration::new(1900000, 0));
    }
}
