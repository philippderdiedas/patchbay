// SPDX-License-Identifier: GPL-3.0-or-later
//! Command parser for slash commands (`/pbx …`) and direct messages to the bot.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    /// show own status (DND, forward) with buttons
    Status,
    /// None: toggle
    Dnd(Option<bool>),
    /// None: clear
    Forward(Option<String>),
    /// None: show the status
    NoAnswer(Option<crate::pbx::NoAnswerChoice>),
    /// ring time in seconds; None: show the status
    RingTime(Option<i16>),
    Call(String),
    Mailbox,
    Phonebook(String),
    Presence(String),
    Calls,
    Link(String),
    Unlink,
    LinkGroup(String),
    UnlinkGroup(String),
    /// call groups linked to this channel
    Groups,
    Unknown(String),
}

fn on_off(s: &str) -> Option<bool> {
    match s.to_lowercase().as_str() {
        "on" | "an" | "ein" | "1" | "true" | "yes" | "ja" => Some(true),
        "off" | "aus" | "0" | "false" | "no" | "nein" => Some(false),
        _ => None,
    }
}

/// Phone numbers as typed in chat: digits, + and * only ("040 123-45" → "04012345").
pub fn clean_number(s: &str) -> Option<String> {
    let n: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '+' || *c == '*').collect();
    let junk = s.chars().any(|c| !(c.is_ascii_digit() || " +-/()*.".contains(c)));
    (!n.is_empty() && n.len() <= 20 && !junk).then_some(n)
}

pub fn parse(text: &str) -> Command {
    let text = text.trim().trim_start_matches('/');
    // "/pbx dnd on" typed into a DM
    let text = text.strip_prefix("pbx ").unwrap_or(text).trim();
    let (cmd, rest) = match text.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (text, ""),
    };
    match cmd.to_lowercase().as_str() {
        "" | "help" | "hilfe" | "?" => Command::Help,
        "status" | "me" | "ich" => Command::Status,
        "dnd" | "dnd-on" | "dnd-off" => {
            let arg = if cmd.len() > 3 { &cmd[4..] } else { rest };
            if arg.is_empty() {
                Command::Dnd(None)
            } else {
                on_off(arg).map(|b| Command::Dnd(Some(b))).unwrap_or_else(|| Command::Unknown(text.to_string()))
            }
        }
        "forward" | "fwd" | "umleitung" | "umleiten" => match rest.to_lowercase().as_str() {
            "" => Command::Status,
            "off" | "aus" | "clear" | "none" | "-" => Command::Forward(None),
            _ => match clean_number(rest) {
                Some(n) => Command::Forward(Some(n)),
                None => Command::Unknown(text.to_string()),
            },
        },
        "noanswer" | "na" | "nichtmelden" => match rest.to_lowercase().as_str() {
            "" => Command::NoAnswer(None),
            "mailbox" | "vm" | "mb" | "voicemail" | "mailbox on" => Command::NoAnswer(Some(crate::pbx::NoAnswerChoice::Mailbox)),
            "off" | "aus" | "none" | "nothing" | "nichts" | "-" | "mailbox off" => Command::NoAnswer(Some(crate::pbx::NoAnswerChoice::Nothing)),
            _ => match clean_number(rest) {
                Some(n) => Command::NoAnswer(Some(crate::pbx::NoAnswerChoice::Number(n))),
                None => Command::Unknown(text.to_string()),
            },
        },
        "ringtime" | "ring" | "klingeldauer" => match rest.trim_end_matches(['s', ' ']) {
            "" => Command::RingTime(None),
            r => r.parse().map(|s| Command::RingTime(Some(s))).unwrap_or_else(|_| Command::Unknown(text.to_string())),
        },
        "call" | "dial" | "anrufen" | "wählen" | "waehlen" => match clean_number(rest) {
            Some(n) => Command::Call(n),
            None => Command::Unknown(text.to_string()),
        },
        "mailbox" | "vm" | "voicemail" | "mb" => Command::Mailbox,
        "phonebook" | "pb" | "find" | "search" | "telefonbuch" | "suche" => Command::Phonebook(rest.to_string()),
        "presence" | "who" | "wer" | "präsenz" | "praesenz" => Command::Presence(rest.to_string()),
        "calls" | "anrufe" | "history" => Command::Calls,
        "link" | "verknüpfen" | "verknuepfen" => Command::Link(rest.to_string()),
        "unlink" | "trennen" => Command::Unlink,
        "link-group" | "linkgroup" => Command::LinkGroup(rest.to_string()),
        "unlink-group" | "unlinkgroup" => Command::UnlinkGroup(rest.to_string()),
        "groups" | "gruppen" => Command::Groups,
        _ => Command::Unknown(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(parse(""), Command::Help);
        assert_eq!(parse("help"), Command::Help);
        assert_eq!(parse("dnd on"), Command::Dnd(Some(true)));
        assert_eq!(parse("DND aus"), Command::Dnd(Some(false)));
        assert_eq!(parse("dnd"), Command::Dnd(None));
        assert_eq!(parse("dnd-off"), Command::Dnd(Some(false)));
        assert_eq!(parse("dnd maybe"), Command::Unknown("dnd maybe".into()));
        assert_eq!(parse("forward 1021"), Command::Forward(Some("1021".into())));
        assert_eq!(parse("forward +49 176 123-45"), Command::Forward(Some("+4917612345".into())));
        assert_eq!(parse("forward off"), Command::Forward(None));
        use crate::pbx::NoAnswerChoice as N;
        assert_eq!(parse("noanswer mailbox"), Command::NoAnswer(Some(N::Mailbox)));
        assert_eq!(parse("noanswer off"), Command::NoAnswer(Some(N::Nothing)));
        assert_eq!(parse("noanswer 0176 123"), Command::NoAnswer(Some(N::Number("0176123".into()))));
        assert_eq!(parse("noanswer"), Command::NoAnswer(None));
        assert_eq!(parse("ringtime 30"), Command::RingTime(Some(30)));
        assert_eq!(parse("ringtime 30s"), Command::RingTime(Some(30)));
        assert!(matches!(parse("ringtime x"), Command::Unknown(_)));
        assert_eq!(parse("call 040 1234"), Command::Call("0401234".into()));
        assert_eq!(parse("call; rm -rf"), Command::Unknown("call; rm -rf".into()));
        assert_eq!(parse("call 040;1"), Command::Unknown("call 040;1".into()));
        assert_eq!(parse("/pbx vm"), Command::Mailbox);
        assert_eq!(parse("pb  müller "), Command::Phonebook("müller".into()));
        assert_eq!(parse("who"), Command::Presence(String::new()));
        assert_eq!(parse("link AB12CD34"), Command::Link("AB12CD34".into()));
        assert_eq!(parse("link-group 5110"), Command::LinkGroup("5110".into()));
        assert_eq!(parse("unlink-group 5110"), Command::UnlinkGroup("5110".into()));
        assert_eq!(parse("frobnicate"), Command::Unknown("frobnicate".into()));
    }

    #[test]
    fn numbers() {
        assert_eq!(clean_number("040 / 123 45"), Some("04012345".into()));
        assert_eq!(clean_number("abc"), None);
        assert_eq!(clean_number(""), None);
        assert_eq!(clean_number("123456789012345678901"), None);
    }
}

/// One entry of the command reference.
pub struct CommandHelp {
    /// what to type after `/pbx` (or in a direct message), placeholders in `<…>`
    pub syntax: &'static str,
    /// English description (translated with `i18n::Lang::t`)
    pub text: &'static str,
    /// only for admins (call group channels)
    pub admin: bool,
}

/// The command reference: the bot's `help` answer and the commands list in the web interface
/// (patchbay-web chat page) both come from here.
pub const COMMANDS: &[CommandHelp] = &[
    CommandHelp { syntax: "help", text: "Shows this list.", admin: false },
    CommandHelp { syntax: "status", text: "Shows whether do not disturb or a forward is on.", admin: false },
    CommandHelp { syntax: "dnd on|off", text: "Switches do not disturb on or off: your phones don't ring, callers get your no-answer rule (e.g. your mailbox).", admin: false },
    CommandHelp { syntax: "forward <number>|off", text: "Forwards all your calls right away to another number (internal or external), or switches the forward off.", admin: false },
    CommandHelp { syntax: "noanswer mailbox|<number>|off", text: "What happens when you don't answer: your mailbox, another number or nothing.", admin: false },
    CommandHelp { syntax: "ringtime <seconds>", text: "How long your phones ring before that (10–120 seconds).", admin: false },
    CommandHelp { syntax: "call <number>", text: "Starts a call: your phone rings first, when you pick up the number is dialled.", admin: false },
    CommandHelp { syntax: "mailbox", text: "Shows your voicemails.", admin: false },
    CommandHelp { syntax: "pb <name>", text: "Searches the phone book.", admin: false },
    CommandHelp { syntax: "who [name]", text: "Shows who is available, on the phone or on do not disturb – everybody or one person.", admin: false },
    CommandHelp { syntax: "calls", text: "Lists your recent calls.", admin: false },
    CommandHelp { syntax: "link <code>", text: "Links your chat account (code from the web interface).", admin: false },
    CommandHelp { syntax: "unlink", text: "Removes the link between your chat account and the phone system.", admin: false },
    CommandHelp { syntax: "link-group <number>", text: "In a channel: the calls of this call group are posted there (who calls, who answered, missed calls).", admin: true },
    CommandHelp { syntax: "unlink-group <number>", text: "Stops that again.", admin: true },
    CommandHelp { syntax: "groups", text: "Shows which call groups are linked to the channel.", admin: true },
];
