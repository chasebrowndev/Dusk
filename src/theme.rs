use ratatui::style::Color;

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: &'static str,
    pub border_dim: Color,
    pub border_focus: Color,
    pub room_active: Color,
    pub room_inactive: Color,
    pub msg_nick: Color,
    pub msg_timestamp: Color,
    pub msg_system: Color,
    pub msg_text: Color,
    pub hint_text: Color,
}

pub static CYBERPUNK: Theme = Theme {
    name: "cyberpunk",
    border_dim: Color::DarkGray,
    border_focus: Color::Cyan,
    room_active: Color::Cyan,
    room_inactive: Color::DarkGray,
    msg_nick: Color::Magenta,
    msg_timestamp: Color::DarkGray,
    msg_system: Color::DarkGray,
    msg_text: Color::White,
    hint_text: Color::DarkGray,
};

pub static NORD: Theme = Theme {
    name: "nord",
    border_dim: Color::Indexed(238),
    border_focus: Color::Indexed(110),
    room_active: Color::Indexed(110),
    room_inactive: Color::Indexed(238),
    msg_nick: Color::Indexed(109),
    msg_timestamp: Color::Indexed(238),
    msg_system: Color::Indexed(238),
    msg_text: Color::Indexed(253),
    hint_text: Color::Indexed(238),
};

pub static GRUVBOX: Theme = Theme {
    name: "gruvbox",
    border_dim: Color::Indexed(239),
    border_focus: Color::Indexed(214),
    room_active: Color::Indexed(214),
    room_inactive: Color::Indexed(239),
    msg_nick: Color::Indexed(208),
    msg_timestamp: Color::Indexed(239),
    msg_system: Color::Indexed(239),
    msg_text: Color::Indexed(223),
    hint_text: Color::Indexed(239),
};

pub static DRACULA: Theme = Theme {
    name: "dracula",
    border_dim: Color::Indexed(239),
    border_focus: Color::Indexed(141),
    room_active: Color::Indexed(212),
    room_inactive: Color::Indexed(239),
    msg_nick: Color::Indexed(212),
    msg_timestamp: Color::Indexed(239),
    msg_system: Color::Indexed(239),
    msg_text: Color::Indexed(253),
    hint_text: Color::Indexed(239),
};

pub static TOKYO_NIGHT: Theme = Theme {
    name: "tokyo-night",
    border_dim: Color::Indexed(237),
    border_focus: Color::Indexed(111), // blue
    room_active: Color::Indexed(111),
    room_inactive: Color::Indexed(237),
    msg_nick: Color::Indexed(175),     // soft pink
    msg_timestamp: Color::Indexed(237),
    msg_system: Color::Indexed(237),
    msg_text: Color::Indexed(189),     // fg
    hint_text: Color::Indexed(237),
};

pub static CATPPUCCIN: Theme = Theme {
    name: "catppuccin",
    border_dim: Color::Indexed(238),
    border_focus: Color::Indexed(147), // lavender
    room_active: Color::Indexed(147),
    room_inactive: Color::Indexed(238),
    msg_nick: Color::Indexed(203),     // red/flamingo
    msg_timestamp: Color::Indexed(238),
    msg_system: Color::Indexed(238),
    msg_text: Color::Indexed(252),     // text
    hint_text: Color::Indexed(238),
};

pub static SOLARIZED: Theme = Theme {
    name: "solarized",
    border_dim: Color::Indexed(240),
    border_focus: Color::Indexed(37),  // cyan
    room_active: Color::Indexed(37),
    room_inactive: Color::Indexed(240),
    msg_nick: Color::Indexed(136),     // yellow
    msg_timestamp: Color::Indexed(240),
    msg_system: Color::Indexed(240),
    msg_text: Color::Indexed(246),     // base1
    hint_text: Color::Indexed(240),
};

pub static ROSE_PINE: Theme = Theme {
    name: "rose-pine",
    border_dim: Color::Indexed(237),
    border_focus: Color::Indexed(183), // iris/purple
    room_active: Color::Indexed(183),
    room_inactive: Color::Indexed(237),
    msg_nick: Color::Indexed(217),     // rose
    msg_timestamp: Color::Indexed(237),
    msg_system: Color::Indexed(237),
    msg_text: Color::Indexed(253),     // text
    hint_text: Color::Indexed(237),
};

pub static AYU: Theme = Theme {
    name: "ayu",
    border_dim: Color::Indexed(237),
    border_focus: Color::Indexed(214), // orange
    room_active: Color::Indexed(214),
    room_inactive: Color::Indexed(237),
    msg_nick: Color::Indexed(73),      // teal
    msg_timestamp: Color::Indexed(237),
    msg_system: Color::Indexed(237),
    msg_text: Color::Indexed(252),
    hint_text: Color::Indexed(237),
};

pub static MATRIX: Theme = Theme {
    name: "matrix",
    border_dim: Color::Indexed(22),    // dark green
    border_focus: Color::Indexed(46),  // bright green
    room_active: Color::Indexed(46),
    room_inactive: Color::Indexed(22),
    msg_nick: Color::Indexed(40),      // medium green
    msg_timestamp: Color::Indexed(22),
    msg_system: Color::Indexed(22),
    msg_text: Color::Indexed(34),      // green
    hint_text: Color::Indexed(22),
};

pub static SYNTHWAVE: Theme = Theme {
    name: "synthwave",
    border_dim: Color::Indexed(54),
    border_focus: Color::Indexed(201),  // hot magenta
    room_active: Color::Indexed(201),
    room_inactive: Color::Indexed(54),
    msg_nick: Color::Indexed(51),       // electric cyan
    msg_timestamp: Color::Indexed(54),
    msg_system: Color::Indexed(54),
    msg_text: Color::Indexed(225),      // pale pink
    hint_text: Color::Indexed(54),
};

pub static VAPORWAVE: Theme = Theme {
    name: "vaporwave",
    border_dim: Color::Indexed(60),
    border_focus: Color::Indexed(219),  // pastel pink
    room_active: Color::Indexed(219),
    room_inactive: Color::Indexed(60),
    msg_nick: Color::Indexed(159),      // pale cyan
    msg_timestamp: Color::Indexed(60),
    msg_system: Color::Indexed(60),
    msg_text: Color::Indexed(255),
    hint_text: Color::Indexed(60),
};

pub static ABYSS: Theme = Theme {
    name: "abyss",
    border_dim: Color::Indexed(23),
    border_focus: Color::Indexed(39),   // deep sky blue
    room_active: Color::Indexed(39),
    room_inactive: Color::Indexed(23),
    msg_nick: Color::Indexed(80),       // turquoise
    msg_timestamp: Color::Indexed(23),
    msg_system: Color::Indexed(23),
    msg_text: Color::Indexed(195),
    hint_text: Color::Indexed(23),
};

pub static EMBER: Theme = Theme {
    name: "ember",
    border_dim: Color::Indexed(52),
    border_focus: Color::Indexed(208),  // orange
    room_active: Color::Indexed(208),
    room_inactive: Color::Indexed(52),
    msg_nick: Color::Indexed(196),      // red
    msg_timestamp: Color::Indexed(52),
    msg_system: Color::Indexed(52),
    msg_text: Color::Indexed(223),      // warm cream
    hint_text: Color::Indexed(52),
};

pub static MINT: Theme = Theme {
    name: "mint",
    border_dim: Color::Indexed(65),
    border_focus: Color::Indexed(121),  // mint green
    room_active: Color::Indexed(121),
    room_inactive: Color::Indexed(65),
    msg_nick: Color::Indexed(79),       // aquamarine
    msg_timestamp: Color::Indexed(65),
    msg_system: Color::Indexed(65),
    msg_text: Color::Indexed(194),      // pale green
    hint_text: Color::Indexed(65),
};

pub static MONOKAI: Theme = Theme {
    name: "monokai",
    border_dim: Color::Indexed(240),
    border_focus: Color::Indexed(197),  // pink-red
    room_active: Color::Indexed(197),
    room_inactive: Color::Indexed(240),
    msg_nick: Color::Indexed(148),      // lime
    msg_timestamp: Color::Indexed(240),
    msg_system: Color::Indexed(240),
    msg_text: Color::Indexed(253),
    hint_text: Color::Indexed(240),
};

pub static BUBBLEGUM: Theme = Theme {
    name: "bubblegum",
    border_dim: Color::Indexed(95),
    border_focus: Color::Indexed(218),  // light pink
    room_active: Color::Indexed(218),
    room_inactive: Color::Indexed(95),
    msg_nick: Color::Indexed(117),      // sky blue
    msg_timestamp: Color::Indexed(95),
    msg_system: Color::Indexed(95),
    msg_text: Color::Indexed(255),
    hint_text: Color::Indexed(95),
};

pub static SAKURA: Theme = Theme {
    name: "sakura",
    border_dim: Color::Indexed(95),
    border_focus: Color::Indexed(217),  // cherry blossom
    room_active: Color::Indexed(217),
    room_inactive: Color::Indexed(95),
    msg_nick: Color::Indexed(151),      // soft leaf green
    msg_timestamp: Color::Indexed(95),
    msg_system: Color::Indexed(95),
    msg_text: Color::Indexed(224),      // pale blossom
    hint_text: Color::Indexed(95),
};

pub fn by_name(name: &str) -> &'static Theme {
    match name.to_lowercase().as_str() {
        "nord" => &NORD,
        "gruvbox" => &GRUVBOX,
        "dracula" => &DRACULA,
        "tokyo-night" | "tokyo_night" | "tokyo" => &TOKYO_NIGHT,
        "catppuccin" | "cat" => &CATPPUCCIN,
        "solarized" => &SOLARIZED,
        "rose-pine" | "rose_pine" | "rosepine" => &ROSE_PINE,
        "ayu" => &AYU,
        "matrix" => &MATRIX,
        "synthwave" | "synth" => &SYNTHWAVE,
        "vaporwave" | "vapor" => &VAPORWAVE,
        "abyss" | "ocean" => &ABYSS,
        "ember" => &EMBER,
        "mint" => &MINT,
        "monokai" => &MONOKAI,
        "bubblegum" | "gum" => &BUBBLEGUM,
        "sakura" | "cherry" => &SAKURA,
        _ => &CYBERPUNK,
    }
}

pub const ALL: &[&str] = &[
    "cyberpunk", "nord", "gruvbox", "dracula",
    "tokyo-night", "catppuccin", "solarized", "rose-pine", "ayu", "matrix",
    "synthwave", "vaporwave", "abyss", "ember", "mint", "monokai",
    "bubblegum", "sakura",
];
