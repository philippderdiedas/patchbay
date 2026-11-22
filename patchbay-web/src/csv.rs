// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal CSV export: ';' separated with UTF-8 BOM, so German Excel opens it directly.

use axum::{
    http::header,
    response::{IntoResponse, Response},
};

pub struct Csv(String);

impl Csv {
    pub fn new(header: &[&str]) -> Self {
        let mut c = Csv(String::from("\u{feff}"));
        c.row(header);
        c
    }

    pub fn row(&mut self, fields: &[&str]) {
        let line: Vec<String> = fields.iter().map(|f| field(f)).collect();
        self.0 += &line.join(";");
        self.0 += "\r\n";
    }

    pub fn into_response(self, name: &str) -> Response {
        let file = format!("{name}-{}.csv", chrono::Local::now().format("%Y-%m-%d"));
        (
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file}\"")),
            ],
            self.0,
        )
            .into_response()
    }
}

/// Quotes when needed; a leading = + - @ gets a ' so spreadsheets do not run it as a formula
/// (phone numbers like +49… are kept readable by quoting them as text instead).
fn field(s: &str) -> String {
    let s = if s.starts_with(['=', '@']) || (s.starts_with(['+', '-']) && !s[1..].chars().all(|c| c.is_ascii_digit() || c == ' ')) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if s.contains([';', '"', '\n', '\r']) || s.starts_with(['+', '-']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(field("abc"), "abc");
        assert_eq!(field("a;b"), "\"a;b\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(field("=cmd()"), "'=cmd()");
        assert_eq!(field("+4917612345"), "\"+4917612345\"");
        assert_eq!(field("-2+3"), "'-2+3");
    }
}
