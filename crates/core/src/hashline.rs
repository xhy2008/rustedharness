use xxhash_rust::xxh32::xxh32;

/// 16-character zero-confusion alphabet (no 0/O/1/I/L/U).
pub const ALPHABET: &[u8; 16] = b"ZPMQVRWSNKTXJBYH";
pub const MIN_WIDTH: usize = 2;
pub const MAX_WIDTH: usize = 4;

/// Normalise one line for hashing: drop CR and trailing whitespace.
pub fn normalize_line(line: &str) -> &str {
    line.trim_end_matches('\r').trim_end()
}

/// Hash of one line's normalized content. Anchors deliberately ignore
/// neighbouring lines: the line number selects the position, and the anchor
/// only verifies that the line still has the content the model saw.
pub fn line_hash(line: &str) -> u32 {
    xxh32(normalize_line(line).as_bytes(), 0)
}

fn encode(mut hash: u32, width: usize) -> String {
    let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
    let mut out = String::with_capacity(width);
    for i in 0..width {
        // Rotate/mix so short anchors still depend on all hash bits.
        hash ^= hash >> 13;
        hash = hash.wrapping_mul(0x9E37_79B1);
        let nibble = ((hash >> (i * 5)) & 0x0f) as usize;
        out.push(ALPHABET[nibble] as char);
    }
    out
}

/// Anchor for one line's content.
pub fn anchor(line: &str, width: usize) -> String {
    encode(line_hash(line), width)
}

/// Anchors for all lines.
pub fn anchors(lines: &[&str], width: usize) -> Vec<String> {
    lines.iter().map(|line| anchor(line, width)).collect()
}

/// Anchors for a 1-based inclusive line range. Returns `(line_no, anchor)`.
pub fn anchors_range(
    lines: &[&str],
    start: usize,
    end: usize,
    width: usize,
) -> Vec<(usize, String)> {
    if lines.is_empty() {
        return Vec::new();
    }
    let start = start.max(1).min(lines.len());
    let end = end.max(start).min(lines.len());
    (start..=end)
        .map(|n| (n, anchor(lines[n - 1], width)))
        .collect()
}

pub fn format_line(line_no: usize, anchor: &str, content: &str) -> String {
    format!("{line_no}#{anchor}|{content}")
}

/// A position accepted by the edit tool: `12` or `12#AB`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PosSpec {
    pub line: usize,
    pub anchor: Option<String>,
}

impl PosSpec {
    pub fn parse(value: &serde_json::Value) -> Result<Self, String> {
        match value {
            serde_json::Value::Number(n) => n
                .as_u64()
                .filter(|v| *v > 0)
                .map(|v| PosSpec {
                    line: v as usize,
                    anchor: None,
                })
                .ok_or_else(|| "line number must be a positive integer".to_string()),
            serde_json::Value::String(s) => {
                let s = s.trim();
                let (line, anchor) = match s.split_once('#') {
                    Some((l, a)) => (l.trim(), Some(a.trim().to_ascii_uppercase())),
                    None => (s, None),
                };
                let line: usize = line
                    .parse()
                    .map_err(|_| format!("invalid line number `{line}`"))?;
                if line == 0 {
                    return Err("line numbers are 1-based".to_string());
                }
                if let Some(a) = &anchor {
                    if !(MIN_WIDTH..=MAX_WIDTH).contains(&a.len())
                        || !a.bytes().all(|b| ALPHABET.contains(&b))
                    {
                        return Err(format!("invalid anchor `{a}`"));
                    }
                }
                Ok(PosSpec {
                    line,
                    anchor: anchor.filter(|a| !a.is_empty()),
                })
            }
            _ => Err("pos must be a number or a string like \"12#AB\"".to_string()),
        }
    }

    pub fn display(&self) -> String {
        match &self.anchor {
            Some(a) => format!("{}#{}", self.line, a),
            None => self.line.to_string(),
        }
    }
}

/// Reject content that still carries read/diff decorations. Feeding
/// `12#AB|...` or unified-diff lines back into `edit` is a common failure mode.
pub fn looks_decorated(lines: &[String]) -> Option<String> {
    let anchor_re = regex::Regex::new(r"^\s*\d+#[A-Za-z]{2,4}\|").unwrap();
    if let Some(line) = lines.iter().find(|l| anchor_re.is_match(l)) {
        return Some(format!("line looks like a `read` anchor line: {line:?}"));
    }
    let plus = lines.iter().filter(|l| l.starts_with('+')).count();
    let minus = lines.iter().filter(|l| l.starts_with('-')).count();
    let nonempty = lines.iter().filter(|l| !l.trim().is_empty()).count();
    if nonempty >= 2 && (plus + minus) == nonempty && (plus > 0 || minus > 0) {
        return Some("lines contain unified-diff +/- markers".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(text: &str) -> Vec<&str> {
        text.lines().collect()
    }

    #[test]
    fn anchor_is_stable_and_two_chars() {
        let lines = lines_of("alpha\nbeta\ngamma");
        let a = anchors(&lines, 2);
        assert_eq!(a.len(), 3);
        assert!(a.iter().all(|x| x.len() == 2));
        assert_eq!(a, anchors(&lines, 2));
    }

    #[test]
    fn width_is_configurable_and_clamped() {
        let lines = lines_of("a\nb");
        for w in 2..=4 {
            assert_eq!(anchor(lines[1], w).len(), w);
        }
        assert_eq!(anchor(lines[1], 1).len(), 2);
        assert_eq!(anchor(lines[1], 9).len(), 4);
    }

    #[test]
    fn crlf_and_trailing_space_do_not_change_anchor() {
        let unix = lines_of("alpha\nbeta\ngamma");
        let crlf = lines_of("alpha\r\nbeta   \r\ngamma\t\r");
        assert_eq!(anchors(&unix, 2), anchors(&crlf, 2));
    }

    #[test]
    fn utf8_and_tabs_are_hashed_by_content() {
        let a = lines_of("fn main() {\n\tlet 世界 = \"🌍\";\n}");
        let b = lines_of("fn main() {\n\tlet 世界 = \"🌍\";\n}");
        let c = lines_of("fn main() {\n\tlet 世界 = \"🌎\";\n}");
        assert_eq!(anchors(&a, 2), anchors(&b, 2));
        assert_ne!(anchor(a[1], 2), anchor(c[1], 2));
    }

    #[test]
    fn identical_lines_share_an_anchor() {
        let lines = lines_of("x\nsame\ny\nsame\nz");
        assert_eq!(anchor(lines[1], 2), anchor(lines[3], 2));
        assert_ne!(anchor(lines[0], 2), anchor(lines[1], 2));
    }

    #[test]
    fn editing_a_line_invalidates_only_that_line() {
        let before = lines_of("one\ntwo\nthree\nfour\nfive");
        let after = lines_of("one\ntwo\nTHREE!\nfour\nfive");
        let a = anchors(&before, 4);
        let b = anchors(&after, 4);
        assert_eq!(a[0], b[0]);
        assert_eq!(a[1], b[1]);
        assert_ne!(a[2], b[2]);
        assert_eq!(a[3], b[3]);
        assert_eq!(a[4], b[4]);
    }

    #[test]
    fn anchors_range_is_one_based_inclusive() {
        let lines = lines_of("a\nb\nc\nd");
        let got = anchors_range(&lines, 2, 3, 2);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, 2);
        assert_eq!(got[0].1, anchor(lines[1], 2));
        assert_eq!(got[1].1, anchor(lines[2], 2));
    }

    #[test]
    fn pos_spec_parses_number_and_anchor() {
        let p = PosSpec::parse(&serde_json::json!(12)).unwrap();
        assert_eq!(p.line, 12);
        assert!(p.anchor.is_none());
        let p = PosSpec::parse(&serde_json::json!("7#zp")).unwrap();
        assert_eq!(p.line, 7);
        assert_eq!(p.anchor.as_deref(), Some("ZP"));
        assert!(PosSpec::parse(&serde_json::json!("0")).is_err());
        assert!(PosSpec::parse(&serde_json::json!("3#0O")).is_err());
    }

    #[test]
    fn decorative_prefixes_are_rejected() {
        assert!(looks_decorated(&["1#AB|hello".to_string()]).is_some());
        assert!(looks_decorated(&["+added".into(), "-removed".into()]).is_some());
        assert!(looks_decorated(&["normal".into()]).is_none());
        assert!(looks_decorated(&["+1".into()]).is_none());
    }

    #[test]
    fn format_line_matches_spec() {
        assert_eq!(format_line(3, "ZP", "hello"), "3#ZP|hello");
    }
}
