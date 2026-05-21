#[derive(Debug, Clone)]
pub struct Options {
    pub dry_run: bool,
    pub fast: bool,
    pub not_utf8: bool,
    pub no_default_exclude: bool,
    pub extra_excludes: Vec<String>,
}

impl Options {
    pub fn is_excluded(&self, raw_name: &[u8]) -> bool {
        if !self.no_default_exclude && is_default_excluded(raw_name) {
            return true;
        }

        let basename = last_component(raw_name);
        for pat in &self.extra_excludes {
            if basename == pat.as_bytes() {
                return true;
            }
        }
        false
    }
}

fn is_default_excluded(raw: &[u8]) -> bool {
    let base = last_component(raw);
    if base == b".DS_Store" || base == b"Thumbs.db" || base == b"desktop.ini" {
        return true;
    }
    raw == b"__MACOSX" || raw.starts_with(b"__MACOSX/")
}

fn last_component(raw: &[u8]) -> &[u8] {
    match raw.iter().rposition(|&b| b == b'/') {
        Some(pos) => &raw[pos + 1..],
        None => raw,
    }
}
