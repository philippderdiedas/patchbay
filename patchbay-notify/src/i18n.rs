// SPDX-License-Identifier: GPL-3.0-or-later
//! Bot texts. English is the source and the key; German translations below (missing → English).

use std::{collections::HashMap, sync::LazyLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    De,
}

impl Lang {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "en" => Some(Lang::En),
            "de" => Some(Lang::De),
            _ => None,
        }
    }

    pub fn t(self, en: &'static str) -> &'static str {
        match self {
            Lang::En => en,
            Lang::De => DE.get(en).copied().unwrap_or(en),
        }
    }
}

static DE: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| de_pairs().into_iter().collect());

/// English → German. A key listed twice would silently keep only its last entry (test below).
fn de_pairs() -> Vec<(&'static str, &'static str)> {
    vec![
        // command reference (commands::COMMANDS)
        ("Shows this list.", "Zeigt diese Liste."),
        ("Shows whether do not disturb or a forward is on.", "Zeigt, ob „Nicht stören“ oder eine Weiterleitung an ist."),
        ("Switches do not disturb on or off: your phones don't ring, callers get your no-answer rule (e.g. your mailbox).", "Schaltet „Nicht stören“ ein oder aus: Deine Telefone klingeln nicht, Anrufer bekommen deine Regel für Nichtannahme (z. B. die Mailbox)."),
        ("Forwards all your calls right away to another number (internal or external), or switches the forward off.", "Leitet alle deine Anrufe sofort an eine andere Nummer weiter (intern oder extern) oder schaltet die Weiterleitung aus."),
        ("What happens when you don't answer: your mailbox, another number or nothing.", "Was passiert, wenn du nicht abnimmst: deine Mailbox, eine andere Nummer oder nichts."),
        ("How long your phones ring before that (10–120 seconds).", "Wie lange deine Telefone vorher klingeln (10–120 Sekunden)."),
        ("Starts a call: your phone rings first, when you pick up the number is dialled.", "Startet einen Anruf: Erst klingelt dein Telefon, nach dem Abnehmen wird die Nummer gewählt."),
        ("Shows your voicemails.", "Zeigt deine Mailbox-Nachrichten."),
        ("Searches the phone book.", "Sucht im Telefonbuch."),
        ("Shows who is available, on the phone or on do not disturb – everybody or one person.", "Zeigt, wer erreichbar ist, telefoniert oder „Nicht stören“ an hat – alle oder eine Person."),
        ("Lists your recent calls.", "Listet deine letzten Anrufe."),
        ("Links your chat account (code from the web interface).", "Verknüpft dein Chat-Konto (Code aus der Web-Oberfläche)."),
        ("Removes the link between your chat account and the phone system.", "Trennt die Verbindung zwischen deinem Chat-Konto und der Telefonanlage."),
        ("In a channel: the calls of this call group are posted there (who calls, who answered, missed calls).", "In einem Kanal: Die Anrufe dieser Rufgruppe werden dort gepostet (wer anruft, wer abgenommen hat, verpasste Anrufe)."),
        ("Stops that again.", "Beendet das wieder."),
        ("Shows which call groups are linked to the channel.", "Zeigt, welche Rufgruppen mit dem Kanal verbunden sind."),
        ("admins: ", "Admins: "),
        ("and send", "und sende"),
        ("answered by", "angenommen von"),
        ("Call back", "Zurückrufen"),
        ("caller hung up", "Anrufer hat aufgelegt"),
        ("Call for", "Anruf für"),
        ("Call group unlinked from this channel.", "Gruppe von diesem Kanal gelöst."),
        ("Calling", "Rufe an:"),
        ("Call not possible", "Anruf nicht möglich"),
        ("Calls for", "Anrufe für"),
        ("Create a code at", "Erzeuge einen Code unter"),
        ("Delete", "Löschen"),
        ("deleted by", "gelöscht von"),
        ("Dialled number", "Gewählte Nummer"),
        ("DND off", "DND aus"),
        ("DND on", "DND an"),
        ("do not disturb", "nicht stören"),
        ("Do not disturb", "Nicht stören"),
        ("Forward", "Umleitung"),
        ("forwarded", "umgeleitet"),
        ("Forwarding not set", "Umleitung nicht gesetzt"),
        ("Not saved", "Nicht gespeichert"),
        ("you may not call this number", "diese Nummer darfst du nicht anrufen"),
        ("database error", "Datenbankfehler"),
        ("forwarding to yourself", "Umleitung auf dich selbst"),
        ("invalid number", "ungültige Nummer"),
        ("none of your phones is registered", "keines deiner Telefone ist angemeldet"),
        ("not a forwarding target", "kein mögliches Umleitungsziel"),
        ("not a valid phone number", "keine gültige Rufnummer"),
        ("not registered", "nicht angemeldet"),
        ("ring time must be 10–120 seconds", "die Klingeldauer muss 10–120 Sekunden sein"),
        ("set by the admins at this site", "an diesem Standort legen das die Admins fest"),
        ("unknown device", "unbekanntes Gerät"),
        ("unknown internal number", "unbekannte interne Nummer"),
        ("not supported", "nicht unterstützt"),
        ("mailbox", "Mailbox"),
        ("nothing", "nichts"),
        ("If not answered", "Wenn ich nicht rangehe"),
        ("after", "nach"),
        ("Forward off", "Umleitung aus"),
        ("from", "von"),
        ("Heard", "Gehört"),
        ("heard by", "gehört von"),
        ("Incoming call for", "Anruf für"),
        ("Linked call groups", "Verknüpfte Gruppen"),
        ("Linked with", "Verknüpft mit"),
        ("Mailbox", "Mailbox"),
        ("Missed call for", "Verpasster Anruf für"),
        ("Missed call from", "Verpasster Anruf von"),
        ("no answer", "nicht angenommen"),
        ("No call groups are linked to this channel.", "Mit diesem Kanal ist keine Gruppe verknüpft."),
        ("No calls.", "Keine Anrufe."),
        ("No phone account with your chat e-mail address was found.", "Zu deiner Chat-E-Mail-Adresse gibt es kein Telefonkonto."),
        ("Nothing found.", "Nichts gefunden."),
        (
            "now appear in this channel. Make sure the bot is a member of the channel (/invite).",
            "erscheinen jetzt in diesem Kanal. Der Bot muss Mitglied des Kanals sein (/invite).",
        ),
        ("on", "an"),
        ("off", "aus"),
        ("Only phone system admins can link call groups.", "Nur Admins der Telefonanlage können Gruppen verknüpfen."),
        ("PBX bot commands", "Befehle des Telefon-Bots"),
        ("Phone book", "Telefonbuch"),
        ("Play", "Abspielen"),
        ("Presence", "Anwesenheit"),
        ("Recent calls", "Letzte Anrufe"),
        ("Run this command in the channel the call group should be linked to.", "Führe den Befehl in dem Kanal aus, mit dem die Gruppe verknüpft werden soll."),
        ("See", "Siehe"),
        ("Sent to you as a direct message.", "Als Direktnachricht an dich geschickt."),
        ("Something went wrong, please try again later.", "Da ist etwas schiefgegangen, bitte später noch einmal versuchen."),
        ("This call group is not linked to this channel.", "Diese Gruppe ist nicht mit diesem Kanal verknüpft."),
        (
            "This code is invalid or has expired. Create a new one in the web interface.",
            "Der Code ist ungültig oder abgelaufen. Erzeuge in der Web-Oberfläche einen neuen.",
        ),
        ("unknown caller", "unbekannter Anrufer"),
        ("Unknown call group.", "Unbekannte Gruppe."),
        ("Unknown command", "Unbekannter Befehl"),
        ("Usage", "Aufruf"),
        ("Voicemail for", "Sprachnachricht für"),
        ("voicemail left", "Sprachnachricht hinterlassen"),
        ("Voicemail not found.", "Sprachnachricht nicht gefunden."),
        ("Your chat account is no longer linked to the phone system.", "Dein Chat-Konto ist nicht mehr mit der Telefonanlage verknüpft."),
        ("Your chat account is not linked to the phone system yet.", "Dein Chat-Konto ist noch nicht mit der Telefonanlage verknüpft."),
        ("Your mailbox is empty.", "Deine Mailbox ist leer."),
        ("your phone rings first.", "zuerst klingelt dein Telefon."),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn german() {
        assert_eq!(super::Lang::De.t("Call back"), "Zurückrufen");
        assert_eq!(super::Lang::En.t("Call back"), "Call back");
        assert_eq!(super::Lang::De.t("not translated"), "not translated");
    }

    #[test]
    fn no_duplicate_keys() {
        let mut seen = std::collections::HashSet::new();
        let dups: Vec<&str> = super::de_pairs().into_iter().map(|(en, _)| en).filter(|en| !seen.insert(*en)).collect();
        assert!(dups.is_empty(), "listed twice: {dups:?}");
    }
}
