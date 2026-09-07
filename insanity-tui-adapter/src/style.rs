use tui::style::Color;

pub const BG_GRAY: Color = Color::Rgb(50, 50, 50);
pub const SELECTED: Color = Color::Rgb(80, 80, 80);
pub const CONNECTED: Color = Color::Green; //Color::Rgb(0, 255, 0);

// Gruvbox (mostly) dark theme
pub const COLOR_RED: Color = Color::Rgb(0xfb, 0x49, 0x34); // Color::Rgb(0xcc, 0x24, 0x1d);
pub const COLOR_GREEN: Color = Color::Rgb(0x98, 0x98, 0x1a);
pub const COLOR_YELLOW: Color = Color::Rgb(0xd7, 0x99, 0x21);
pub const COLOR_BLUE: Color = Color::Rgb(0x45, 0x85, 0x88);
pub const COLOR_PURPLE: Color = Color::Rgb(0xb1, 0x62, 0x86);
pub const COLOR_AQUA: Color = Color::Rgb(0x68, 0x96, 0x6a);
pub const COLOR_ORANGE: Color = Color::Rgb(0xd6, 0x5d, 0x0e);
pub const NUM_CHAT_COLORS: usize = 7;
pub const CHAT_COLORS: [Color; NUM_CHAT_COLORS] = [
    COLOR_RED,
    COLOR_GREEN,
    COLOR_YELLOW,
    COLOR_BLUE,
    COLOR_PURPLE,
    COLOR_AQUA,
    COLOR_ORANGE,
];
