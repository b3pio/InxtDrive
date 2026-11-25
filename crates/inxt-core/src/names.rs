//! Mapping between remote names (any UTF-8) and names valid on the local
//! filesystem, plus the rules for files that stay local only.

/// Characters Windows forbids in names, and the full-width look-alikes used
/// to display them (the same convention rclone uses).
#[cfg(windows)]
const MAP: [(char, char); 9] = [
    ('\\', '＼'),
    (':', '：'),
    ('*', '＊'),
    ('?', '？'),
    ('"', '＂'),
    ('<', '＜'),
    ('>', '＞'),
    ('|', '｜'),
    ('/', '／'),
];

/// Elsewhere only the path separators need replacing; `\` too, because the
/// engine joins paths with it.
#[cfg(not(windows))]
const MAP: [(char, char); 2] = [('\\', '＼'), ('/', '／')];

const WINDOWS_RULES: bool = cfg!(windows);

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
    "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn to_local(remote: &str) -> String {
    let mut out: String = remote
        .chars()
        .map(|c| match MAP.iter().find(|(a, _)| *a == c) {
            Some((_, b)) => *b,
            None if (c as u32) < 0x20 => char::from_u32(0x2400 + c as u32).unwrap_or('_'),
            None => c,
        })
        .collect();
    if WINDOWS_RULES {
        if out.ends_with('.') || out.ends_with(' ') {
            let last = out.pop().unwrap();
            out.push(if last == '.' { '．' } else { '\u{2002}' });
        }
        let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            out.insert(0, '＿');
        }
    }
    if !WINDOWS_RULES && (out == "." || out == "..") {
        out = out.replace('.', "．");
    }
    if out.is_empty() {
        out.push('＿');
    }
    out
}

pub fn to_remote(local: &str) -> String {
    let mut s: String = local
        .chars()
        .map(|c| match MAP.iter().find(|(_, b)| *b == c) {
            Some((a, _)) => *a,
            None if (0x2400..0x2420).contains(&(c as u32)) => char::from_u32(c as u32 - 0x2400).unwrap_or(c),
            None => c,
        })
        .collect();
    if !WINDOWS_RULES {
        return if s == "．" || s == "．．" { s.replace('．', ".") } else { s };
    }
    if s.ends_with('．') {
        s.pop();
        s.push('.');
    } else if s.ends_with('\u{2002}') {
        s.pop();
        s.push(' ');
    }
    if let Some(rest) = s.strip_prefix('＿') {
        let stem = rest.split('.').next().unwrap_or("").to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) || rest.is_empty() {
            return rest.to_string();
        }
    }
    s
}

/// Case-insensitive key for lookups, matching Windows semantics closely enough.
pub fn key(name: &str) -> String {
    name.to_lowercase()
}

pub fn is_valid_local(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 1024
        && !name.chars().any(|c| MAP.iter().any(|(a, _)| *a == c) || (c as u32) < 0x20)
}

/// Patterns for files that are never uploaded (Explorer metadata, lock and
/// partial-download files). Matched case-insensitively against the name.
#[derive(Debug, Clone)]
pub struct LocalOnlyRules {
    patterns: Vec<String>,
}

impl Default for LocalOnlyRules {
    fn default() -> Self {
        Self::new(
            [
                "desktop.ini",
                "thumbs.db",
                "ehthumbs.db",
                "~$*",
                ".~lock.*",
                "*.crdownload",
                "*.part",
                "*.partial",
                "*.download",
                // Editor swap and save-in-progress files (vim, kate, gedit and other GIO apps).
                ".*.sw?",
                ".*.kate-swp",
                ".goutputstream-*",
                "$recycle.bin",
                "system volume information",
            ]
            .iter()
            .map(|s| s.to_string()),
        )
    }
}

impl LocalOnlyRules {
    pub fn new(patterns: impl IntoIterator<Item = String>) -> Self {
        Self { patterns: patterns.into_iter().map(|p| p.to_lowercase()).collect() }
    }

    pub fn matches(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        self.patterns.iter().any(|p| glob(p, &n))
    }
}

fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_invalid_chars() {
        for remote in ["a:b", "what?", "x*y|z", "trailing.", "space ", "CON", "con.txt", "normal name.txt", "a/b"] {
            let local = to_local(remote);
            assert!(is_valid_local(&local), "{remote} -> {local}");
            assert_eq!(to_remote(&local), remote, "{remote} -> {local}");
        }
    }

    #[test]
    fn local_only() {
        let r = LocalOnlyRules::default();
        assert!(r.matches("Desktop.ini"));
        assert!(r.matches("~$report.docx"));
        assert!(r.matches("movie.mkv.crdownload"));
        assert!(r.matches("$RECYCLE.BIN"));
        assert!(!r.matches("report.docx"));
        assert!(!r.matches("~WRL0001.tmp"));
        assert!(r.matches(".notes.txt.swp"));
        assert!(r.matches(".notes.txt.swo"));
        assert!(r.matches(".notes.txt.kate-swp"));
        assert!(r.matches(".goutputstream-ABC123"));
        assert!(!r.matches("notes.txt.swp.bak"));
    }
}
