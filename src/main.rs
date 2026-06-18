use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn rank(self) -> u8 {
        match self {
            Level::Trace => 0,
            Level::Debug => 1,
            Level::Info => 2,
            Level::Warn => 3,
            Level::Error => 4,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    fn from_str_ci(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Level::Trace),
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" | "err" => Some(Level::Error),
            _ => None,
        }
    }
}

impl Ord for Level {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for Level {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct AppState {
    logs_dir: PathBuf,
}

#[derive(Serialize)]
struct MatchRange {
    start: usize,
    end: usize,
    text: String,
}

#[derive(Serialize)]
struct LogEntry {
    line_no: usize,
    level: String,
    message: String,
    highlights: Vec<MatchRange>,
}

#[derive(Serialize)]
struct LogsResponse {
    file: String,
    level: String,
    exact: bool,
    keyword: Option<String>,
    limit: Option<usize>,
    total_scanned: usize,
    count: usize,
    entries: Vec<LogEntry>,
}

#[derive(Deserialize)]
struct LogQuery {
    level: Option<String>,
    exact: Option<bool>,
    file: Option<String>,
    limit: Option<usize>,
    q: Option<String>,
}

#[tokio::main]
async fn main() {
    let logs_dir_raw = std::env::var("LOGS_DIR").unwrap_or_else(|_| "logs".to_string());
    let logs_dir = PathBuf::from(&logs_dir_raw)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&logs_dir_raw));
    fs::create_dir_all(&logs_dir).ok();
    println!("logs_dir = {}", logs_dir.display());

    let state = Arc::new(AppState { logs_dir });

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/levels", get(list_levels))
        .route("/api/files", get(list_files))
        .route("/api/logs", get(get_logs))
        .with_state(state);

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind failed");
    println!("oplog-reader listening on http://{}", addr);
    axum::serve(listener, app).await.expect("server error");
}

async fn health() -> &'static str {
    "ok"
}

async fn list_levels() -> Json<Vec<&'static str>> {
    Json(vec![
        Level::Trace.as_str(),
        Level::Debug.as_str(),
        Level::Info.as_str(),
        Level::Warn.as_str(),
        Level::Error.as_str(),
    ])
}

async fn list_files(State(state): State<Arc<AppState>>) -> Json<Vec<String>> {
    let mut files = Vec::new();
    if let Ok(rd) = fs::read_dir(&state.logs_dir) {
        for entry in rd.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                let p = entry.path();
                if p.is_file() && (name.ends_with(".log") || name.ends_with(".txt")) {
                    files.push(name.to_string());
                }
            }
        }
    }
    files.sort();
    Json(files)
}

async fn get_logs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LogQuery>,
) -> Result<Json<LogsResponse>, (StatusCode, String)> {
    let file_name = q.file.clone().unwrap_or_else(|| "operations.log".to_string());
    let safe_name = Path::new(&file_name)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .filter(|s| !s.contains(".."))
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "invalid file name".to_string()))?;

    let path = state.logs_dir.join(&safe_name);
    let content = {
        let mut file = fs::File::open(&path).map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                format!("log file not found (possibly rotated): {}", e),
            )
        })?;
        let mut buf = Vec::new();
        if let Ok(meta) = file.metadata() {
            if let Ok(len) = usize::try_from(meta.len()) {
                buf.reserve(len);
            }
        }
        file.read_to_end(&mut buf)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("read log failed: {}", e)))?;
        drop(file);
        String::from_utf8_lossy(&buf).into_owned()
    };

    let requested_level = q.level.as_deref().and_then(Level::from_str_ci);
    if q.level.is_some() && requested_level.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown level: {}", q.level.as_deref().unwrap_or("")),
        ));
    }
    let exact = q.exact.unwrap_or(false);
    let limit = q.limit;
    let needle = q
        .q
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());

    let mut entries: Vec<LogEntry> = Vec::new();
    let mut total_scanned = 0usize;

    for (idx, line) in content.lines().enumerate() {
        total_scanned += 1;
        let line_no = idx + 1;
        let parsed = parse_line_level(line);

        if let Some(req) = requested_level {
            let keep = match parsed {
                Some(l) => {
                    if exact {
                        l == req
                    } else {
                        l >= req
                    }
                }
                None => false,
            };
            if !keep {
                continue;
            }
        }

        let highlights = match needle {
            Some(ref n) => {
                let ranges = find_keyword_matches(line, n);
                if ranges.is_empty() {
                    continue;
                }
                ranges
            }
            None => Vec::new(),
        };

        entries.push(LogEntry {
            line_no,
            level: parsed.map(Level::as_str).unwrap_or("UNKNOWN").to_string(),
            message: line.to_string(),
            highlights,
        });
    }

    if let Some(limit) = limit {
        if limit > 0 && entries.len() > limit {
            let start = entries.len() - limit;
            entries = entries.split_off(start);
        }
    }

    Ok(Json(LogsResponse {
        file: safe_name,
        level: requested_level.map(Level::as_str).unwrap_or("").to_string(),
        exact,
        keyword: q.q.clone().filter(|s| !s.trim().is_empty()),
        limit,
        total_scanned,
        count: entries.len(),
        entries,
    }))
}

fn find_keyword_matches(line: &str, needle: &str) -> Vec<MatchRange> {
    if needle.is_empty() {
        return Vec::new();
    }
    let nlen = needle.len();
    let bytes = line.as_bytes();
    let hlen = bytes.len();
    let mut byte_ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i + nlen <= hlen {
        if bytes[i..i + nlen].eq_ignore_ascii_case(needle.as_bytes()) {
            byte_ranges.push((i, i + nlen));
            i += nlen;
        } else {
            i += 1;
        }
    }
    if byte_ranges.is_empty() {
        return Vec::new();
    }

    let mut result: Vec<MatchRange> = Vec::with_capacity(byte_ranges.len());
    let mut char_count = 0usize;
    let mut last_byte = 0usize;
    for (bs, be) in byte_ranges {
        for _ in line[last_byte..bs].chars() {
            char_count += 1;
        }
        let start = char_count;
        let matched_text = &line[bs..be];
        for _ in matched_text.chars() {
            char_count += 1;
        }
        result.push(MatchRange {
            start,
            end: char_count,
            text: matched_text.to_string(),
        });
        last_byte = be;
    }
    result
}

fn parse_line_level(line: &str) -> Option<Level> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            for key in ["level", "levelname", "severity"] {
                if let Some(lvl) = value.get(key).and_then(|v| v.as_str()) {
                    if let Some(l) = Level::from_str_ci(lvl) {
                        return Some(l);
                    }
                }
            }
        }
    }

    parse_text_level(trimmed)
}

fn parse_text_level(line: &str) -> Option<Level> {
    let upper = line.to_ascii_uppercase();

    for lvl in [
        Level::Error,
        Level::Warn,
        Level::Info,
        Level::Debug,
        Level::Trace,
    ] {
        let name = lvl.as_str();
        if upper.contains(&format!("[{}]", name)) {
            return Some(lvl);
        }
    }
    if upper.contains("[WARNING]") {
        return Some(Level::Warn);
    }

    for token in upper.split_whitespace() {
        let cleaned: String = token
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
            .to_string();
        match cleaned.as_str() {
            "TRACE" => return Some(Level::Trace),
            "DEBUG" => return Some(Level::Debug),
            "INFO" => return Some(Level::Info),
            "WARN" | "WARNING" => return Some(Level::Warn),
            "ERROR" | "ERR" => return Some(Level::Error),
            _ => continue,
        }
    }

    None
}
