//! Token usage of Claude agents, read from Claude Code's transcripts in
//! `~/.claude/projects/<folder>/<session>.jsonl`. Tokens only: plans differ
//! in how (and whether) tokens are billed, so Hiero shows no prices.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub messages: u64,
}

impl Tokens {
    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.messages += o.messages;
    }
}

/// What to look up for one agent.
#[derive(Debug, Clone)]
pub struct Query {
    pub addr: String,
    pub name: String,
    pub cwd: String,
    pub created_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub addr: String,
    pub name: String,
    pub model: String,
    pub tokens: Tokens,
    /// The transcript used, or why none was found.
    pub source: Result<PathBuf, String>,
}

pub fn claude_projects() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::forms::expand_home("~/.claude"))
        .join("projects")
}

/// Claude Code's folder name for a working directory: every character
/// that isn't a letter, digit or `-` becomes `-`.
pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect()
}

/// When a transcript starts (first timestamp in its opening lines).
fn started_ms(path: &Path) -> Option<u64> {
    let f = std::fs::File::open(path).ok()?;
    for line in BufReader::new(f).lines().take(40).map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
            && let Some(ts) = v.get("timestamp").and_then(|t| t.as_str())
        {
            return parse_rfc3339_ms(ts);
        }
    }
    None
}

/// `2026-11-09T03:41:47.353Z` → ms since the epoch (UTC only, as Claude writes it).
pub fn parse_rfc3339_ms(ts: &str) -> Option<u64> {
    let (date, time) = ts.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    let ms: i64 = format!("{:0<3}", &frac[..frac.len().min(3)]).parse().ok()?;
    // Days from civil date (Howard Hinnant's algorithm).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    u64::try_from(secs * 1000 + ms).ok()
}

/// The session that began closest after the terminal started (within 2 min).
pub fn find_transcript(cwd: &str, created_ms: u64) -> Result<PathBuf, String> {
    let dir = claude_projects().join(project_dir_name(cwd));
    let entries = std::fs::read_dir(&dir).map_err(|_| "no Claude transcripts for this folder".to_string())?;
    let mut best: Option<(u64, PathBuf)> = None;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let Some(start) = started_ms(&p) else { continue };
        if start + 5_000 < created_ms || start > created_ms + 120_000 {
            continue;
        }
        let gap = start.abs_diff(created_ms);
        if best.as_ref().is_none_or(|(g, _)| gap < *g) {
            best = Some((gap, p));
        }
    }
    best.map(|(_, p)| p).ok_or_else(|| "no Claude session started with this terminal".into())
}

/// Sum usage over a transcript, once per message id (streamed messages are
/// written several times; the last copy has the final numbers).
pub fn sum_transcript(path: &Path) -> Result<(Tokens, String), String> {
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut by_id: HashMap<String, Tokens> = HashMap::new();
    let mut model = String::new();
    for (i, line) in BufReader::new(f).lines().map_while(Result::ok).enumerate() {
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let Some(msg) = v.get("message") else { continue };
        let Some(u) = msg.get("usage") else { continue };
        let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let id = msg.get("id").and_then(|x| x.as_str()).map(str::to_string).unwrap_or_else(|| format!("line{i}"));
        if let Some(m) = msg.get("model").and_then(|x| x.as_str()).filter(|m| !m.starts_with('<')) {
            model = m.to_string();
        }
        by_id.insert(
            id,
            Tokens {
                input: n("input_tokens"),
                output: n("output_tokens"),
                cache_write: n("cache_creation_input_tokens"),
                cache_read: n("cache_read_input_tokens"),
                messages: 1,
            },
        );
    }
    let mut total = Tokens::default();
    for t in by_id.values() {
        total.add(t);
    }
    Ok((total, model))
}

pub fn collect(queries: Vec<Query>) -> Vec<Row> {
    queries
        .into_iter()
        .map(|q| {
            let source = find_transcript(&q.cwd, q.created_ms);
            let (tokens, model) = match &source {
                Ok(p) => sum_transcript(p).unwrap_or_default(),
                Err(_) => Default::default(),
            };
            Row { addr: q.addr, name: q.name, model, tokens, source }
        })
        .collect()
}

/// 1234 → "1.2k", 3_400_000 → "3.4M".
pub fn short(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_times() {
        assert_eq!(project_dir_name("/home/cup/project/demo.hiero/x_y"), "-home-cup-project-demo-hiero-x-y");
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_rfc3339_ms("2026-11-09T03:41:47.353Z"), Some(1_794_195_707_353));
        assert_eq!(short(1234), "1.2k");
        assert_eq!(short(3_400_000), "3.4M");
    }

    #[test]
    fn sums_once_per_message() {
        let dir = std::env::temp_dir().join(format!("hiero-usage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        std::fs::write(
            &p,
            [
                r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z"}"#,
                r#"{"type":"assistant","message":{"id":"a","model":"claude-x","usage":{"input_tokens":1,"output_tokens":5}}}"#,
                r#"{"type":"assistant","message":{"id":"a","model":"claude-x","usage":{"input_tokens":1,"output_tokens":9,"cache_read_input_tokens":100}}}"#,
                r#"{"type":"assistant","message":{"id":"b","model":"claude-x","usage":{"input_tokens":2,"output_tokens":3,"cache_creation_input_tokens":50}}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let (t, model) = sum_transcript(&p).unwrap();
        assert_eq!(model, "claude-x");
        assert_eq!(t, Tokens { input: 3, output: 12, cache_write: 50, cache_read: 100, messages: 2 });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
