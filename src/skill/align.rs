//! Runtime compatibility alignment for third-party skill bodies.
//!
//! FoxIR standards the injected notes cover (as of v1.0.21):
//! 1. Windows forensics cold-set tools (`WIN_IR_COLD_TOOLS`) are on-demand:
//!    a skill that names one without pointing at `load_tool_schema` gets a
//!    pointer note, so the model pulls the schema instead of failing the call.
//! 2. Dangling bundled-file references (`scripts/x.py`, `REFERENCE.md`, ...)
//!    are detected against the real skill directory and surfaced as "verify /
//!    fall back" guidance instead of a mid-run file-not-found.
//! 3. Bash-isms (`python3`, heredocs, `grep`) get a PowerShell translation note.
//! 4. Legacy brand `RustAgent` is rewritten to `FoxIR` (product brand).
//!
//! Design rules — kept deliberately conservative:
//! - **Read-time only**: applied in [`read_skill_body`] and the `skill_read_file`
//!   SKILL.md branch. Every install path (install_skill tool, zip unpack, manual
//!   copy into skills/) funnels through these, so alignment covers third-party
//!   skills regardless of how they arrived — while the on-disk source stays
//!   byte-identical (updatable, hash-checkable, no duplicate notes in history).
//! - **Idempotent**: a body already carrying the `FOXIR-COMPAT` marker is
//!   returned untouched.
//! - **Additive**: the only in-place rewrite is the brand string; everything
//!   else is a prepended note block. Content is never deleted.
//! - **Silent when clean**: no notes triggered → no block, zero noise.

use std::path::Path;

/// Marker embedded in the note block; doubles as the idempotency guard.
pub const MARKER: &str = "FOXIR-COMPAT";

/// Align a SKILL.md instruction body for the FoxIR runtime.
/// `skill_dir` enables the dangling-bundled-file check (skipped when `None`).
pub fn align_skill_body(body: &str, skill_dir: Option<&Path>) -> String {
    // Idempotency: an already-aligned body (or a hand-written one that copied
    // the marker) is left exactly as-is.
    if body.contains(MARKER) {
        return body.to_string();
    }
    // Brand normalization (the only in-place rewrite).
    let text = body.replace("RustAgent", "FoxIR");

    let mut notes: Vec<String> = Vec::new();

    // --- 1) On-demand cold tools referenced without a load_tool_schema pointer ---
    if !text.contains("load_tool_schema") {
        let hits: Vec<&str> = crate::tool::ir_win::WIN_IR_COLD_TOOLS
            .iter()
            .copied()
            .filter(|t| contains_token(&text, t))
            .collect();
        if !hits.is_empty() {
            let list = hits
                .iter()
                .map(|t| format!("`{}`", t))
                .collect::<Vec<_>>()
                .join(", ");
            notes.push(format!(
                "**On-demand tools**: {} are Windows forensics cold-set tools and may NOT appear in the current tool list. If a tool is missing, first call `load_tool_schema` with {{\"name\": \"<tool>\"}} to pull its schema, then invoke it normally — they stay executable.",
                list
            ));
        }
    }

    // --- 2) Dangling bundled-file references ---
    if let Some(dir) = skill_dir {
        let missing = dangling_refs(&text, dir);
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|p| format!("`{}`", p))
                .collect::<Vec<_>>()
                .join(", ");
            notes.push(format!(
                "**Missing bundled files**: this skill references {} file(s) not found in its directory: {}. Verify what actually ships with `skill_read_file` (path=\"\" lists the directory); if a referenced file is genuinely absent, do NOT stall — write an equivalent script/document to the workspace `output/` directory with `file_write` and proceed.",
                missing.len(),
                list
            ));
        }
    }

    // --- 3) PowerShell translation notes for common bash-isms ---
    let mut bashisms: Vec<&str> = Vec::new();
    if contains_token(&text, "python3") || text.contains("python3 ") {
        bashisms.push("`python3` may not exist on Windows — use `python` or `py -3`");
    }
    if text.contains("<<'") || text.contains("<<\"") || text.contains("<<PY") || text.contains("<<EOF") {
        bashisms.push("bash heredocs (`<<'PY'`...) are not supported in PowerShell — write the script to a temp `.py` file under `output/` with `file_write`, then run it");
    }
    if contains_token(&text, "grep") {
        bashisms.push("`grep` is not available in PowerShell — use `Select-String` (or run the search through Python)");
    }
    if !bashisms.is_empty() {
        notes.push(format!(
            "**Windows shell**: the agent shell is PowerShell. {}",
            bashisms.join("; ")
        ));
    }

    if notes.is_empty() {
        return text;
    }
    let mut block = format!(
        "<!-- {} (auto): note block injected at skill-load time; the source file on disk is unchanged -->\n\
         > **FoxIR runtime notes (auto-injected — ignore if already covered above)**\n",
        MARKER
    );
    for n in &notes {
        block.push_str("> - ");
        block.push_str(n);
        block.push('\n');
    }
    block.push('\n');
    block.push_str(&text);
    block
}

/// Word-boundary token check: `needle` matches only when not glued to word
/// characters (alnum, `_`, `-`, `.`) on either side — so `ir_eml` does not
/// match inside `ir_emlx`, but does inside `` `ir_eml` `` or `run ir_eml to`.
fn contains_token(hay: &str, needle: &str) -> bool {
    let hb: Vec<char> = hay.chars().collect();
    let nb: Vec<char> = needle.chars().collect();
    if nb.is_empty() || hb.len() < nb.len() {
        return false;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '-' || c == '.';
    for i in 0..=(hb.len() - nb.len()) {
        if hb[i..i + nb.len()] == nb[..] {
            let before_ok = i == 0 || !is_word(hb[i - 1]);
            let after_ok = i + nb.len() == hb.len() || !is_word(hb[i + nb.len()]);
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

/// Extract path-like file references from the body and return the ones that do
/// not exist under `skill_dir` (sorted, deduped, capped). Only concrete,
/// template-free references are checked: must carry a `/`, end in a known
/// bundled-file extension, and contain no placeholder characters.
fn dangling_refs(body: &str, skill_dir: &Path) -> Vec<String> {
    const EXTS: &[&str] = &[".py", ".md", ".html", ".json"];
    // Tokens are split on shell/markdown separators; backticks and quotes are
    // trimmed afterwards.
    let is_sep = |c: char| c.is_whitespace() || c == '`' || c == '"' || c == '\'' || c == '(' || c == ')' || c == '[' || c == ']' || c == '<' || c == '>' || c == '|' || c == ',' || c == ':' || c == ';' || c == '=';
    let mut seen: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for rawtok in body.split(is_sep) {
        // Reject absolute/URL forms BEFORE trimming, so a leading `/` is not
        // stripped into a false relative reference.
        if rawtok.starts_with('/') || rawtok.starts_with("\\\\") || rawtok.contains("://") || rawtok.contains(":\\") {
            continue;
        }
        let tok = rawtok.trim_matches(|c: char| c == '`' || c == '"' || c == '\'');
        if tok.is_empty()
            || tok.len() > 160
            || !tok.contains('/')
            || tok.contains('{')
            || tok.contains('<')
            || tok.contains('*')
        {
            continue;
        }
        let lower = tok.to_lowercase();
        if lower == "skill.md" || !EXTS.iter().any(|e| lower.ends_with(e)) {
            continue;
        }
        if seen.iter().any(|s| s.as_str() == tok) {
            continue;
        }
        seen.push(tok.to_string());
        // Windows rejects paths with backslash segments differently; join() handles forward slashes fine.
        let p = skill_dir.join(tok.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !p.exists() {
            missing.push(tok.to_string());
        }
    }
    missing.sort();
    missing.dedup();
    missing.truncate(12);
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_tool_reference_gets_pointer_note() {
        let body = "# Pcap\n\nCall `ir_pcap_analyze` on the file, then `malware_deep` if needed.\n";
        let out = align_skill_body(body, None);
        assert!(out.contains(MARKER));
        assert!(out.contains("load_tool_schema"));
        assert!(out.contains("ir_pcap_analyze"));
        assert!(out.contains("malware_deep"));
        assert!(out.contains("Call `ir_pcap_analyze` on the file, then `malware_deep` if needed."), "original body must be preserved verbatim after the note block");
    }

    #[test]
    fn cold_tool_note_skipped_when_pointer_already_documented() {
        let body = "Call `load_tool_schema` for `ir_pcap_analyze` first, then invoke it.";
        let out = align_skill_body(body, None);
        assert!(!out.contains(MARKER), "skill already explains on-demand loading; no note expected");
    }

    #[test]
    fn brand_is_normalized() {
        let out = align_skill_body("Analyst: RustAgent (report by RustAgent)", None);
        assert!(!out.contains("RustAgent"));
        assert_eq!(out.matches("FoxIR").count(), 2);
        // Brand-only fix carries no note block.
        assert!(!out.contains(MARKER));
    }

    #[test]
    fn idempotent_when_marker_present() {
        let body = "<!-- FOXIR-COMPAT --> already aligned, mentions ir_pcap_analyze and python3";
        assert_eq!(align_skill_body(body, None), body);
    }

    #[test]
    fn plain_body_gets_no_notes_at_all() {
        let body = "# Mermaid\n\nProduce a mermaid code block. No tools involved.\n";
        assert_eq!(align_skill_body(body, None), body);
    }

    #[test]
    fn bashism_notes_fire_for_python3_heredoc_and_grep() {
        let out = align_skill_body("Run `python3 - <<'PY'` then `grep -i foo` on results.", None);
        assert!(out.contains("py -3"));
        assert!(out.contains("heredoc"));
        assert!(out.contains("Select-String"));
    }

    #[test]
    fn dangling_and_present_bundled_refs_are_checked_against_dir() {
        let dir = std::env::temp_dir().join(format!("foxir_align_test_{}", std::process::id()));
        let scripts = dir.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("present.py"), "print(1)").unwrap();
        let body = "Use scripts/present.py and scripts/missing.py; also references/gone.md — ignore http://x.com/a/b.py and templates/{{name}}.py.";
        let out = align_skill_body(body, Some(&dir));
        assert!(out.contains("`scripts/missing.py`"));
        assert!(out.contains("`references/gone.md`"));
        assert!(!out.contains("`scripts/present.py`"), "existing file must not be flagged");
        assert!(!out.contains("b.py`"), "URL must not appear in the backticked missing list");
        assert!(!out.contains("`templates/"), "placeholder path must not appear in the backticked missing list");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_boundary_prevents_glued_false_positives() {
        // `ir_eml` is a cold tool but must NOT match inside a glued longer word;
        // the backticked standalone name must match.
        let glued = align_skill_body("custom pipeline named myir_emlx tool", None);
        assert!(!glued.contains(MARKER), "glued substring must not count as a tool reference");
        let standalone = align_skill_body("parse the mail with `ir_eml` please", None);
        assert!(standalone.contains(MARKER));
        assert!(standalone.contains("ir_eml"));
    }
}
