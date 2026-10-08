//! file_grep — unified, forensic-safe content search across files.
//!
//! Before this tool existed, "search a keyword inside files" was fragmented:
//! `knowledge_search` only scanned the curated knowledge base, `ir_weblog_scan`
//! only matched fixed attack regexes over web logs, `file_read` dumped a whole
//! file for the model to eyeball, and ad-hoc `findstr`/`Select-String` ran
//! through `shell_exec` — bypassing the tool surface (truncation / audit /
//! permission). This tool closes that gap: give it a directory (or file) plus a
//! keyword/regex and it returns structured `file:line` hits.
//!
//! Design notes (see decisions recorded in project memory):
//! - Traversal uses the `ignore` crate (same walker ripgrep uses) but with
//!   forensic defaults: hidden files INCLUDED and `.gitignore`/`.ignore`
//!   NOT honored — a `.gitignore`d artifact or a dotfile must never be skipped.
//! - Pattern filtering uses `globset` (case-insensitive, `*` crosses `/`) so a
//!   `*.log` filter matches at any depth on a case-insensitive Windows FS.
//! - Matching uses the `regex` crate (the very engine ripgrep matches with).
//! - Decoding sniffs BOMs and auto-handles UTF-16LE/BE, and accepts an explicit
//!   `encoding` label (e.g. `gbk`, `big5`, `shift_jis`) via `encoding_rs`. This
//!   directly addresses the classic Windows-forensics silent-miss: PowerShell
//!   redirects / `reg export` / event-log exports are UTF-16LE *with BOM* and
//!   would otherwise be treated as binary and skipped.
//! - Binary files are detected (NUL scan) and skipped by default, with an
//!   opt-in `treat_binary_as_text`. Unreadable/locked files are NOT silently
//!   swallowed — they are counted and surfaced in the stats.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::Tool;
use crate::context::ToolContext;
use crate::error::AgentResult;
use crate::tool::TimeoutStage;

// ============================================================
// Public core (sync, dependency-light) — reused by Tool + tests + bench
// ============================================================

/// Search mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrepMode {
    /// Return matching lines (with optional context lines).
    Content,
    /// Return only the paths of files that contain at least one match.
    FilesWithMatches,
    /// Return a per-file match count.
    Count,
}

/// A single matching line with optional surrounding context.
#[derive(Debug, Clone)]
pub struct MatchLine {
    pub file: PathBuf,
    pub line: usize, // 1-based
    pub text: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

/// Per-file match count (Count mode).
#[derive(Debug, Clone)]
pub struct FileCount {
    pub file: PathBuf,
    pub matches: usize,
}

/// Owned search request. All fields are owned to keep the core easy to call
/// from tests and the benchmark without lifetime gymnastics.
#[derive(Debug, Clone)]
pub struct GrepRequest {
    pub pattern: String,
    pub root: PathBuf,
    pub mode: GrepMode,
    /// true = `pattern` is a regex; false = literal string (auto-escaped).
    pub regex: bool,
    pub case_insensitive: bool,
    /// Optional include-glob (e.g. `*.log` or `*.{log,txt}`), matched at any depth.
    pub glob: Option<String>,
    /// Context lines before/after each match (Content mode), clamped 0..=10.
    pub context: usize,
    /// Optional forced encoding label (e.g. `gbk`, `utf-16le`, `big5`).
    pub encoding: Option<String>,
    /// Global cap on collected Content matches (also bounds per-file collection).
    pub max_matches: usize,
    pub skip_binary: bool,
    pub treat_binary_as_text: bool,
    /// Skip files larger than this many bytes (0 = no size limit).
    pub max_file_size: u64,
    /// Search directory recursively (false = only `root` if it is a file, or its immediate files).
    pub recursive: bool,
    /// Use rayon to search files in parallel.
    pub parallel: bool,
    /// Truncate each returned line to at most this many chars (0 = no line cap).
    pub max_line_chars: usize,
}

impl Default for GrepRequest {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            root: PathBuf::new(),
            mode: GrepMode::Content,
            regex: true,
            case_insensitive: false,
            glob: None,
            context: 0,
            encoding: None,
            max_matches: 200,
            skip_binary: true,
            treat_binary_as_text: false,
            max_file_size: 64 * 1024 * 1024,
            recursive: true,
            parallel: true,
            max_line_chars: 400,
        }
    }
}

/// Aggregate outcome of a search.
#[derive(Debug, Clone, Default)]
pub struct GrepOutcome {
    pub matches: Vec<MatchLine>,
    pub files_with_matches: Vec<PathBuf>,
    pub counts: Vec<FileCount>,
    pub files_searched: usize,
    pub total_matches: usize,
    pub skipped_large: usize,
    pub skipped_binary: usize,
    pub unreadable: usize,
    pub walk_errors: usize,
    /// The encoding actually applied to the FIRST decoded file (informational);
    /// per-file detection may vary.
    pub encoding_mode: String,
    pub truncated: bool,
    /// Sample of paths that could not be read (max 20) so nothing is silently lost.
    pub unreadable_samples: Vec<String>,
}

/// Per-file decode result.
struct Decoded {
    text: String,
    /// short label, e.g. "utf-8", "utf-16le", "gbk", "utf-8(lossy)".
    encoding: String,
    binary: bool,
}

/// Decode a file's bytes to text with forensic-friendly encoding handling.
///
/// Priority: explicit `label` (via `encoding_rs`) > BOM sniff (UTF-8 BOM,
/// UTF-16LE/BE) > assume UTF-8 (with binary detection + lossy fallback).
fn decode_bytes(bytes: &[u8], label: Option<&str>) -> Decoded {
    use encoding_rs::Encoding;

    // 1) Explicit encoding label wins — user said "this is GBK".
    if let Some(lab) = label {
        let lab_l = lab.to_ascii_lowercase();
        // Fast paths for UTF variants we can also hit via BOM, uniformly handled.
        if lab_l == "utf-8" || lab_l == "utf8" {
            let (enc, _) = strip_utf8_bom(bytes);
            return decode_utf8(enc);
        }
        if let Some(enc) = Encoding::for_label(lab.as_bytes()) {
            let (cow, _used, had_errors) = enc.decode(bytes);
            return Decoded {
                text: cow.into_owned(),
                encoding: if had_errors { format!("{lab}(lossy)") } else { lab.to_string() },
                binary: false,
            };
        }
        // Unknown label: fall back to UTF-8 handling but report it.
        let (enc, _) = strip_utf8_bom(bytes);
        let mut d = decode_utf8(enc);
        d.encoding = format!("utf-8(?label={lab})");
        return d;
    }

    // 2) BOM sniff.
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        let mut d = decode_utf8(&bytes[3..]);
        d.encoding = "utf-8(bom)".into();
        return d;
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let (cow, _, _) = encoding_rs::UTF_16LE.decode(bytes);
        return Decoded { text: cow.into_owned(), encoding: "utf-16le".into(), binary: false };
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let (cow, _, _) = encoding_rs::UTF_16BE.decode(bytes);
        return Decoded { text: cow.into_owned(), encoding: "utf-16be".into(), binary: false };
    }

    // 3) No BOM / no label: binary detect then UTF-8.
    if looks_binary(bytes) {
        return Decoded { text: String::new(), encoding: "binary".into(), binary: true };
    }
    decode_utf8(bytes)
}

fn strip_utf8_bom(bytes: &[u8]) -> (&[u8], bool) {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        (&bytes[3..], true)
    } else {
        (bytes, false)
    }
}

fn decode_utf8(bytes: &[u8]) -> Decoded {
    match std::str::from_utf8(bytes) {
        Ok(s) => Decoded { text: s.to_string(), encoding: "utf-8".into(), binary: false },
        Err(_) => Decoded {
            text: String::from_utf8_lossy(bytes).into_owned(),
            encoding: "utf-8(lossy)".into(),
            binary: false,
        },
    }
}

/// A byte is "binary" if the first 8 KiB contain a NUL that is not a plausible
/// UTF-16 pattern. Cheap heuristic mirroring ripgrep/grep's own detection.
fn looks_binary(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(8192)];
    window.contains(&0u8)
}

/// Build the matcher. Fixed strings are escaped into a literal regex (still
/// accelerated by the regex crate's memchr literal fast path).
fn build_matcher(req: &GrepRequest) -> Result<regex::Regex, String> {
    let pat = if req.regex {
        req.pattern.clone()
    } else {
        regex::escape(&req.pattern)
    };
    regex::RegexBuilder::new(&pat)
        .case_insensitive(req.case_insensitive)
        .build()
        .map_err(|e| {
            if req.regex {
                format!("invalid regex '{pat}': {e} (set regex=false to search it literally)")
            } else {
                format!("invalid pattern: {e}")
            }
        })
}

/// Compile the optional include-glob into a matcher (case-insensitive, `*` crosses `/`).
fn build_glob(req: &GrepRequest) -> Result<Option<globset::GlobMatcher>, String> {
    match &req.glob {
        Some(g) => {
            let glob = globset::GlobBuilder::new(g)
                .case_insensitive(true)
                .literal_separator(false)
                .build()
                .map_err(|e| format!("invalid glob '{g}': {e}"))?;
            Ok(Some(glob.compile_matcher()))
        }
        None => Ok(None),
    }
}

/// Enumerate candidate files under `root` honoring forensic walk defaults.
fn collect_files(req: &GrepRequest, globm: Option<&globset::GlobMatcher>) -> (Vec<PathBuf>, usize) {
    let mut files = Vec::new();
    let mut walk_errors = 0usize;

    // A single-file root: evaluate it directly.
    if req.root.is_file() {
        if globm.map(|m| m.is_match(&req.root)).unwrap_or(true) {
            files.push(req.root.clone());
        }
        return (files, 0);
    }

    let mut builder = ignore::WalkBuilder::new(&req.root);
    // Forensic defaults: never skip hidden files, never honor ignore files.
    builder
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .follow_links(false)
        .require_git(false);
    if !req.recursive {
        builder.max_depth(Some(1));
    }

    for result in builder.build() {
        match result {
            Ok(entry) => {
                let path = entry.path();
                // Skip the root dir itself and any directories.
                let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
                if !is_file {
                    continue;
                }
                if let Some(m) = globm {
                    if !m.is_match(path) {
                        continue;
                    }
                }
                files.push(path.to_path_buf());
            }
            Err(_) => walk_errors += 1,
        }
    }
    (files, walk_errors)
}

/// Search a single file; returns per-file contribution to the outcome.
fn search_one_file(path: &Path, re: &regex::Regex, req: &GrepRequest) -> FileResult {
    let mut out = FileResult::default();

    // Size gate before reading (avoids dragging a 5 GB dump into memory).
    if req.max_file_size > 0 {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > req.max_file_size {
                out.skipped_large = true;
                return out;
            }
        }
    }

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            out.unreadable = true;
            return out;
        }
    };

    let decoded = decode_bytes(&bytes, req.encoding.as_deref());
    if decoded.binary {
        if req.treat_binary_as_text {
            // Explicit override: search the raw bytes as (lossy) text so the
            // user's request actually matches something instead of being skipped.
            let text = String::from_utf8_lossy(&bytes).into_owned();
            return scan_text(&text, re, path, req, out, "binary-as-text");
        }
        out.skipped_binary = true;
        return out;
    }

    scan_text(&decoded.text, re, path, req, out, &decoded.encoding)
}

fn scan_text(
    text: &str,
    re: &regex::Regex,
    path: &Path,
    req: &GrepRequest,
    mut out: FileResult,
    encoding: &str,
) -> FileResult {
    // Split lines preserving 1-based numbering. `split_inclusive` keeps a stable
    // index; we trim trailing CR/LF for display.
    let lines: Vec<&str> = text.split('\n').collect();
    out.searched = true;

    match req.mode {
        GrepMode::FilesWithMatches => {
            for line in &lines {
                if re.is_match(line) {
                    out.files_with_matches = true;
                    break;
                }
            }
        }
        GrepMode::Count => {
            let mut n = 0usize;
            for line in &lines {
                n += re.find_iter(line).count();
            }
            out.count = n;
            if n > 0 {
                out.files_with_matches = true;
            }
        }
        GrepMode::Content => {
            let ctx_before = req.context.min(10);
            for (idx, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    out.total_matches += 1;
                    if out.matches.len() < req.max_matches {
                        let text = clip(line.trim_end_matches('\r'), req.max_line_chars);
                        let before: Vec<String> = if ctx_before > 0 {
                            let start = idx.saturating_sub(ctx_before);
                            lines[start..idx].iter().map(|l| clip(l.trim_end_matches('\r'), req.max_line_chars)).collect()
                        } else {
                            Vec::new()
                        };
                        let after: Vec<String> = if ctx_before > 0 && idx + 1 < lines.len() {
                            let end = (idx + 1 + ctx_before).min(lines.len());
                            lines[idx + 1..end].iter().map(|l| clip(l.trim_end_matches('\r'), req.max_line_chars)).collect()
                        } else {
                            Vec::new()
                        };
                        out.matches.push(MatchLine {
                            file: path.to_path_buf(),
                            line: idx + 1,
                            text,
                            before,
                            after,
                        });
                    }
                }
            }
        }
    }
    out.encoding = encoding.to_string();
    out
}

/// Per-file intermediate result (folded into the aggregate).
#[derive(Debug, Clone, Default)]
struct FileResult {
    searched: bool,
    skipped_large: bool,
    skipped_binary: bool,
    unreadable: bool,
    files_with_matches: bool,
    count: usize,
    total_matches: usize,
    matches: Vec<MatchLine>,
    encoding: String,
}

/// Run a search. Synchronous: callers wrap in `spawn_blocking`.
pub fn grep_files(req: &GrepRequest) -> Result<GrepOutcome, String> {
    if req.pattern.is_empty() {
        return Err("Missing 'pattern'".into());
    }
    if !(req.root.exists()) {
        return Err(format!("Path not found: {}", req.root.display()));
    }
    let re = build_matcher(req)?;
    let globm = build_glob(req)?;
    let (files, walk_errors) = collect_files(req, globm.as_ref());

    // Search each file (optionally in parallel), then fold deterministically.
    let search = |p: &PathBuf| search_one_file(p, &re, req);
    let results: Vec<FileResult> = if req.parallel && files.len() > 1 {
        use rayon::prelude::*;
        files.par_iter().map(search).collect()
    } else {
        files.iter().map(search).collect()
    };

    let mut outcome = GrepOutcome { walk_errors, ..Default::default() };
    for (path, r) in files.iter().zip(results.into_iter()) {
        if r.searched {
            outcome.files_searched += 1;
        }
        if r.skipped_large {
            outcome.skipped_large += 1;
        }
        if r.skipped_binary {
            outcome.skipped_binary += 1;
        }
        if r.unreadable {
            outcome.unreadable += 1;
            if outcome.unreadable_samples.len() < 20 {
                outcome.unreadable_samples.push(path.display().to_string());
            }
        }
        if r.encoding != "binary" && outcome.encoding_mode.is_empty() {
            outcome.encoding_mode = r.encoding.clone();
        }
        outcome.total_matches += r.total_matches + if matches!(req.mode, GrepMode::Count) { r.count } else { 0 };
        if r.files_with_matches {
            outcome.files_with_matches.push(path.clone());
        }
        if r.count > 0 {
            outcome.counts.push(FileCount { file: path.clone(), matches: r.count });
        }
        outcome.matches.extend(r.matches);
    }

    // Deterministic ordering for stable output.
    let by = |a: &PathBuf, b: &PathBuf| a.cmp(b);
    outcome.matches.sort_by(|x, y| by(&x.file, &y.file).then(x.line.cmp(&y.line)));
    if outcome.matches.len() > req.max_matches {
        outcome.matches.truncate(req.max_matches);
    }
    outcome.files_with_matches.sort();
    outcome.counts.sort_by(|a, b| by(&a.file, &b.file));
    outcome.truncated = outcome.total_matches > outcome.matches.len();
    if outcome.encoding_mode.is_empty() {
        outcome.encoding_mode = "utf-8".into();
    }
    Ok(outcome)
}

fn clip(s: &str, max_chars: usize) -> String {
    if max_chars == 0 || s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("…[truncated]");
    out
}

// ============================================================
// Tool wrapper
// ============================================================

pub struct FileGrepTool;

/// Legacy inline cap; scaled up by ctx.inline_limit when context scaling is on.
const LEGACY_INLINE_LIMIT: usize = 8000;
/// Preview matches kept inline when the full result is auto-saved to disk.
const PREVIEW_MATCHES: usize = 30;

impl FileGrepTool {
    /// Resolve a search root. Relative paths resolve against `working_dir`
    /// (mirrors `file_list`); an empty/omitted path defaults to the workspace
    /// so a bare keyword grep does NOT accidentally fan out over `C:\`.
    fn resolve_root(ctx: &ToolContext, path: Option<&str>) -> PathBuf {
        match path {
            None | Some("") => PathBuf::from(&ctx.workspace_dir),
            Some(p) => {
                let pb = PathBuf::from(p);
                if pb.is_absolute() {
                    pb
                } else {
                    PathBuf::from(&ctx.working_dir).join(pb)
                }
            }
        }
    }

    /// Format a path for display: relative to the workspace when under it,
    /// otherwise absolute, always with forward slashes (never a host/URL form).
    fn display_path(workspace_dir: &str, file: &Path) -> String {
        let full = file.to_string_lossy().replace('\\', "/");
        let ws = workspace_dir.replace('\\', "/");
        if let Some(rel) = full.strip_prefix(&ws) {
            return rel.trim_start_matches('/').to_string();
        }
        full
    }

    fn outcome_to_json(ctx: &ToolContext, o: &GrepOutcome, root_disp: &str) -> Value {
        let ws = &ctx.workspace_dir;
        let matches: Vec<Value> = o
            .matches
            .iter()
            .map(|m| {
                json!({
                    "file": Self::display_path(ws, &m.file),
                    "line": m.line,
                    "text": m.text,
                    "before": m.before,
                    "after": m.after,
                })
            })
            .collect();
        let fwm: Vec<Value> = o.files_with_matches.iter().map(|p| json!(Self::display_path(ws, p))).collect();
        let counts: Vec<Value> = o
            .counts
            .iter()
            .map(|c| json!({ "file": Self::display_path(ws, &c.file), "matches": c.matches }))
            .collect();
        json!({
            "scope_root": root_disp,
            "matches": matches,
            "files_with_matches": fwm,
            "counts": counts,
            "stats": {
                "files_searched": o.files_searched,
                "files_with_matches": o.files_with_matches.len(),
                "total_matches": o.total_matches,
                "skipped_large": o.skipped_large,
                "skipped_binary": o.skipped_binary,
                "unreadable": o.unreadable,
                "walk_errors": o.walk_errors,
            },
            "encoding_mode": o.encoding_mode,
            "truncated": o.truncated,
            "unreadable_samples": o.unreadable_samples,
        })
    }
}

#[async_trait]
impl Tool for FileGrepTool {
    fn name(&self) -> &str {
        "file_grep"
    }

    fn description(&self) -> &str {
        "Search the contents of files for a keyword or regex, like ripgrep but built in. \
         Give a directory (or file) and a pattern; returns file:line hits. \
         Defaults to searching the workspace; pass an absolute path to widen scope. \
         Auto-detects UTF-8/UTF-16(BOM); pass encoding=\"gbk\" (or big5/shift_jis/utf-16le) for legacy \
         Chinese/foreign logs. Regex by default; set regex=false for a literal keyword. \
         Modes: content (default), files_with_matches, count. Filters: glob (e.g. *.log), \
         case_insensitive, context lines. Binary files are skipped by default (treat_binary_as_text to override). \
         Forensic behavior: hidden files ARE searched and .gitignore is NOT honored; unreadable/locked files \
         are counted in stats (never silently dropped). Prefer this over shell findstr/Select-String."
    }

    fn is_builtin(&self) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn category(&self) -> &str {
        "read"
    }

    fn timeout_stage(&self) -> TimeoutStage {
        // Whole-evidence-directory content scans can take a while.
        TimeoutStage::Long
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex (default) or literal keyword to search for." },
                "path": { "type": "string", "description": "File or directory to search. Relative → working_dir; omitted → workspace root." },
                "mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Output mode. Default content." },
                "regex": { "type": "boolean", "description": "true = regex, false = literal string. Default true." },
                "case_insensitive": { "type": "boolean", "description": "Default false." },
                "glob": { "type": "string", "description": "Include-glob, e.g. '*.log' or '*.{log,txt}'. Matches at any depth. Optional." },
                "context": { "type": "integer", "description": "Lines of context before/after each hit (content mode, 0-10). Default 0." },
                "encoding": { "type": "string", "description": "Force a codec (gbk, big5, shift_jis, utf-16le, utf-16be, windows-1252, ...). Default: auto-detect (BOM/UTF-8)." },
                "max_matches": { "type": "integer", "description": "Cap on returned content matches. Default 200." },
                "skip_binary": { "type": "boolean", "description": "Skip binary files. Default true." },
                "treat_binary_as_text": { "type": "boolean", "description": "Override skip_binary and grep binary files as text. Default false." },
                "max_file_size_mb": { "type": "integer", "description": "Skip files larger than this many MB. Default 64. 0 = no limit." },
                "recursive": { "type": "boolean", "description": "Recurse into subdirectories. Default true." },
                "parallel": { "type": "boolean", "description": "Search files in parallel. Default true." }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        let pattern = args["pattern"].as_str().map(|s| s.trim().to_string()).unwrap_or_default();
        if pattern.is_empty() {
            return Err("Missing 'pattern'".into());
        }

        let root = Self::resolve_root(ctx, args["path"].as_str());
        let mode = match args["mode"].as_str().unwrap_or("content") {
            "files_with_matches" => GrepMode::FilesWithMatches,
            "count" => GrepMode::Count,
            _ => GrepMode::Content,
        };
        let max_matches = args["max_matches"].as_u64().unwrap_or(200).clamp(1, 5000) as usize;
        let max_file_size_mb = args["max_file_size_mb"].as_u64().unwrap_or(64);

        let req = GrepRequest {
            pattern,
            root: root.clone(),
            mode,
            regex: args["regex"].as_bool().unwrap_or(true),
            case_insensitive: args["case_insensitive"].as_bool().unwrap_or(false),
            glob: args["glob"].as_str().map(|s| s.to_string()),
            context: args["context"].as_u64().unwrap_or(0) as usize,
            encoding: args["encoding"].as_str().map(|s| s.to_string()),
            max_matches,
            skip_binary: args["skip_binary"].as_bool().unwrap_or(true),
            treat_binary_as_text: args["treat_binary_as_text"].as_bool().unwrap_or(false),
            max_file_size: max_file_size_mb.saturating_mul(1024 * 1024),
            recursive: args["recursive"].as_bool().unwrap_or(true),
            parallel: args["parallel"].as_bool().unwrap_or(true),
            max_line_chars: 400,
        };

        let root_disp = Self::display_path(&ctx.workspace_dir, &root);
        let outcome = tokio::task::spawn_blocking(move || grep_files(&req))
            .await
            .map_err(|e| format!("file_grep task failed: {e}"))??;

        let mut result = Self::outcome_to_json(ctx, &outcome, &root_disp);
        result["engine"] = json!("regex+ignore (built-in)");

        // Large-result auto-save (mirrors web_fetch): hand the model a preview +
        // a saved path instead of blowing the context with thousands of lines.
        let inline_limit = ctx.inline_limit(LEGACY_INLINE_LIMIT);
        let serialized = serde_json::to_string(&result).unwrap_or_default();
        if serialized.chars().count() > inline_limit {
            let name = format!(
                "grep_{}_{}.json",
                ctx.base.base.session_id.get(..8).unwrap_or("sess"),
                chrono::Utc::now().format("%Y%m%d_%H%M%S%3f")
            );
            let dir = Path::new(&ctx.output_dir()).join("grep");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(&name);
            match std::fs::write(&path, &serialized) {
                Ok(_) => {
                    // Trim inline preview to the first PREVIEW_MATCHES hits.
                    if let Some(arr) = result.get_mut("matches").and_then(|v| v.as_array_mut()) {
                        if arr.len() > PREVIEW_MATCHES {
                            arr.truncate(PREVIEW_MATCHES);
                        }
                    }
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert("saved_path".into(), json!(path.to_string_lossy()));
                        obj.insert(
                            "note".into(),
                            json!("Result exceeded the inline limit; full matches saved to saved_path. Read it with file_grep again on a narrower scope or use file_read."),
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("file_grep: failed to save large result to {}: {}", path.display(), e);
                }
            }
        }

        Ok(result)
    }
}

// ============================================================
// Unit tests (core `grep_files` only — no ToolContext required)
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn utf16le_with_bom(s: &str) -> Vec<u8> {
        let mut v: Vec<u8> = vec![0xFF, 0xFE];
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    fn req(pattern: &str, root: &Path) -> GrepRequest {
        GrepRequest {
            pattern: pattern.to_string(),
            root: root.to_path_buf(),
            ..Default::default()
        }
    }

    #[test]
    fn basic_regex_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello world\nfoo bar\nworld again\n").unwrap();
        let r = req(r"world", dir.path());
        let o = grep_files(&r).unwrap();
        assert_eq!(o.total_matches, 2);
        assert_eq!(o.matches.len(), 2);
        assert_eq!(o.matches[0].line, 1);
        assert_eq!(o.matches[1].line, 3);
    }

    #[test]
    fn fixed_string_literal() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "price is 5 (usd)\nno match here\n").unwrap();
        let mut r = req("5 (usd)", dir.path());
        r.regex = false; // parens must be treated literally
        let o = grep_files(&r).unwrap();
        assert_eq!(o.total_matches, 1);
    }

    #[test]
    fn invalid_regex_errors_with_hint() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        let r = req(r"(", dir.path());
        let e = grep_files(&r).unwrap_err();
        assert!(e.contains("regex=false"), "error should suggest literal mode: {e}");
    }

    #[test]
    fn case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "Hello\nHELLO\nhey\n").unwrap();
        let mut r = req("hello", dir.path());
        r.case_insensitive = true;
        r.regex = false;
        let o = grep_files(&r).unwrap();
        assert_eq!(o.total_matches, 2);
    }

    #[test]
    fn files_with_matches_mode() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "needle here").unwrap();
        fs::write(dir.path().join("b.txt"), "nothing").unwrap();
        let mut r = req("needle", dir.path());
        r.mode = GrepMode::FilesWithMatches;
        let o = grep_files(&r).unwrap();
        assert_eq!(o.files_with_matches.len(), 1);
        assert!(o.files_with_matches[0].ends_with("a.txt"));
    }

    #[test]
    fn count_mode() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "cat\ncat\ndog\ncat\n").unwrap();
        let mut r = req("cat", dir.path());
        r.mode = GrepMode::Count;
        let o = grep_files(&r).unwrap();
        assert_eq!(o.counts[0].matches, 3);
    }

    #[test]
    fn glob_filter_at_depth() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("logs");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("app.log"), "token=abc").unwrap();
        fs::write(sub.join("app.txt"), "token=abc").unwrap();
        let mut r = req("token", dir.path());
        r.glob = Some("*.log".into());
        let o = grep_files(&r).unwrap();
        assert_eq!(o.files_searched, 1, "only the .log file should be searched");
    }

    #[test]
    fn context_lines() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "l1\nl2\nTARGET\nl4\nl5\n").unwrap();
        let mut r = req("TARGET", dir.path());
        r.context = 1;
        let o = grep_files(&r).unwrap();
        assert_eq!(o.matches[0].line, 3);
        assert_eq!(o.matches[0].before, vec!["l2".to_string()]);
        assert_eq!(o.matches[0].after, vec!["l4".to_string()]);
    }

    #[test]
    fn hidden_files_are_searched_forensic() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".secret.log"), "backdoor").unwrap();
        let o = grep_files(&req("backdoor", dir.path())).unwrap();
        assert_eq!(o.total_matches, 1, "hidden dotfiles must be searched");
    }

    #[test]
    fn gitignore_not_honored_forensic() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        fs::write(dir.path().join("evil.log"), "payload").unwrap();
        let o = grep_files(&req("payload", dir.path())).unwrap();
        assert_eq!(o.total_matches, 1, ".gitignore must NOT hide a matching file");
    }

    #[test]
    fn utf16le_with_bom_is_decoded() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ps_out.txt"), utf16le_with_bom("Error occurred here\nsecond line\n")).unwrap();
        let o = grep_files(&req("Error", dir.path())).unwrap();
        assert_eq!(o.total_matches, 1, "UTF-16LE BOM file must be searched, not skipped as binary");
    }

    #[test]
    fn gbk_explicit_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let (cow, _, _) = encoding_rs::GBK.encode("恶意连接 检测\n普通行\n");
        fs::write(dir.path().join("cn.log"), &cow).unwrap();
        let mut r = req("检测", dir.path());
        r.encoding = Some("gbk".into());
        let o = grep_files(&r).unwrap();
        assert_eq!(o.total_matches, 1, "GBK content must match when encoding=gbk is given");
    }

    #[test]
    fn binary_skipped_by_default_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("blob.bin"), [0x00u8, 0x01, 0x00, 0x41, 0x42]).unwrap();
        let mut r = req("AB", dir.path());
        r.max_line_chars = 400;
        let o = grep_files(&r).unwrap();
        assert_eq!(o.skipped_binary, 1);
        assert_eq!(o.total_matches, 0);
    }

    #[test]
    fn treat_binary_as_text_overrides() {
        let dir = tempfile::tempdir().unwrap();
        // NUL bytes make it "binary"; the ASCII string is still present.
        fs::write(dir.path().join("blob.bin"), [0x00u8, 0x00, b'M', b'a', b'r', b'k', 0x00]).unwrap();
        let mut r = req("Mark", dir.path());
        r.treat_binary_as_text = true;
        let o = grep_files(&r).unwrap();
        assert_eq!(o.total_matches, 1, "explicit override must grep through binary");
    }

    #[test]
    fn max_matches_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let body = "hit\n".repeat(50);
        fs::write(dir.path().join("a.txt"), body).unwrap();
        let mut r = req("hit", dir.path());
        r.max_matches = 10;
        let o = grep_files(&r).unwrap();
        assert_eq!(o.matches.len(), 10);
        assert_eq!(o.total_matches, 50);
        assert!(o.truncated);
    }

    #[test]
    fn recursive_off_only_top_level() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("top.txt"), "findme").unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("deep.txt"), "findme").unwrap();
        let mut r = req("findme", dir.path());
        r.recursive = false;
        let o = grep_files(&r).unwrap();
        assert_eq!(o.total_matches, 1, "non-recursive must not descend into sub/");
    }

    #[test]
    fn serial_matches_parallel_results() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            fs::write(dir.path().join(format!("f{i}.txt")), format!("line1\nneedle {i}\n")).unwrap();
        }
        let mut rp = req("needle", dir.path());
        rp.parallel = true;
        let mut rs = req("needle", dir.path());
        rs.parallel = false;
        let op = grep_files(&rp).unwrap();
        let os = grep_files(&rs).unwrap();
        assert_eq!(op.total_matches, os.total_matches);
        assert_eq!(op.matches.len(), os.matches.len());
        // deterministic ordering means identical files+lines
        let a: Vec<(String, usize)> = op.matches.iter().map(|m| (m.file.display().to_string(), m.line)).collect();
        let b: Vec<(String, usize)> = os.matches.iter().map(|m| (m.file.display().to_string(), m.line)).collect();
        assert_eq!(a, b);
    }

    #[test]
    fn single_file_root() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("only.txt");
        fs::write(&f, "alpha\nbeta\n").unwrap();
        let o = grep_files(&req("beta", &f)).unwrap();
        assert_eq!(o.total_matches, 1);
    }

    #[test]
    fn missing_pattern_and_root_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = req("", dir.path());
        assert!(grep_files(&r).is_err());
        r.pattern = "x".into();
        r.root = dir.path().join("nope_missing_dir");
        assert!(grep_files(&r).is_err());
    }
}
