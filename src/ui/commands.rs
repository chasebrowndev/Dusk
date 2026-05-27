//! Slash-command parsing. Pure: takes a string, returns a `Command`.
//!
//! Buttons and slash commands both produce `Command` values, then are dispatched
//! by `ui::mod::handle_cmd`. Keep this module side-effect-free so it remains testable.

use crate::protocol::ShareKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Join(String),
    Create(String),
    Rooms,
    Theme(Option<String>),
    Share(ShareKind),
    ShareStop,
    Watch(Option<String>),
    Help,
    Unknown(String),
}

impl Command {
    pub fn parse(input: &str) -> Self {
        let body = input.strip_prefix('/').unwrap_or(input);
        let mut parts = body.splitn(2, ' ');
        let verb = parts.next().unwrap_or("").to_ascii_lowercase();
        let arg = parts.next().map(str::trim).filter(|s| !s.is_empty());

        match verb.as_str() {
            "join" | "j" => match arg {
                Some(room) => Self::Join(room.to_string()),
                None => Self::Unknown(verb),
            },
            "create" | "new" => match arg {
                Some(name) => Self::Create(name.to_string()),
                None => Self::Unknown(verb),
            },
            "rooms" | "list" => Self::Rooms,
            "theme" => Self::Theme(arg.map(str::to_string)),
            "share" => match arg {
                Some("stop") => Self::ShareStop,
                Some("cam") | Some("camera") | Some("webcam") => Self::Share(ShareKind::Cam),
                _ => Self::Share(ShareKind::Screen),
            },
            "watch" => Self::Watch(arg.map(str::to_string)),
            "help" | "?" => Self::Help,
            _ => Self::Unknown(verb),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_join_aliases() {
        assert_eq!(Command::parse("/join general"), Command::Join("general".into()));
        assert_eq!(Command::parse("j  general"), Command::Join("general".into()));
    }

    #[test]
    fn parses_create_aliases() {
        assert_eq!(Command::parse("/create quotes"), Command::Create("quotes".into()));
        assert_eq!(Command::parse("new quotes"), Command::Create("quotes".into()));
    }

    #[test]
    fn parses_rooms() {
        assert_eq!(Command::parse("/rooms"), Command::Rooms);
        assert_eq!(Command::parse("/list"), Command::Rooms);
    }

    #[test]
    fn parses_theme_with_and_without_arg() {
        assert_eq!(Command::parse("/theme"), Command::Theme(None));
        assert_eq!(Command::parse("/theme kaneki"), Command::Theme(Some("kaneki".into())));
    }

    #[test]
    fn parses_share_kinds() {
        assert_eq!(Command::parse("/share"), Command::Share(ShareKind::Screen));
        assert_eq!(Command::parse("/share screen"), Command::Share(ShareKind::Screen));
        assert_eq!(Command::parse("/share cam"), Command::Share(ShareKind::Cam));
        assert_eq!(Command::parse("/share camera"), Command::Share(ShareKind::Cam));
        assert_eq!(Command::parse("/share stop"), Command::ShareStop);
    }

    #[test]
    fn parses_watch() {
        assert_eq!(Command::parse("/watch"), Command::Watch(None));
        assert_eq!(Command::parse("/watch alice"), Command::Watch(Some("alice".into())));
    }

    #[test]
    fn parses_help() {
        assert_eq!(Command::parse("/help"), Command::Help);
        assert_eq!(Command::parse("/?"), Command::Help);
    }

    #[test]
    fn unknown_verb_is_preserved() {
        assert_eq!(Command::parse("/bogus arg"), Command::Unknown("bogus".into()));
    }

    #[test]
    fn join_without_arg_is_unknown() {
        // join requires a room name — surface as unknown so the user gets feedback
        assert_eq!(Command::parse("/join"), Command::Unknown("join".into()));
    }

    #[test]
    fn slash_prefix_optional() {
        assert_eq!(Command::parse("rooms"), Command::Rooms);
        assert_eq!(Command::parse("/rooms"), Command::Rooms);
    }
}
