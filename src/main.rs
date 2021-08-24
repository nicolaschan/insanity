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
}

fn main() {
    let opts: Opts = Opts::parse();

    let _id: String = match opts.id {
        Some(id) => id,
        None => nanoid::nanoid!(),
    };

    let (ui_message_sender, ui_message_receiver) = unbounded();

    insanity::server::start_server(opts.bind_address, opts.music, ui_message_sender.clone());

    for peer_address in opts.peer_address {
        insanity::client::start_client(peer_address, opts.output_device, opts.denoise, ui_message_sender.clone());
    }

    insanity::tui::start(ui_message_sender, ui_message_receiver);
}
