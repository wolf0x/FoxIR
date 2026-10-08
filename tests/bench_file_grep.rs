//! Benchmark for `file_grep` (built-in) vs alternative content-search paths.
//!
//! This is an `#[ignore]`d integration test: it is NOT part of the normal suite
//! (it builds a synthetic corpus and runs several engines over it). Run it
//! explicitly, in release, with output shown:
//!
//! ```text
//! cargo test --release --test bench_file_grep -- --ignored --nocapture
//! ```
//!
//! It compares, over the SAME corpus and the SAME literal needle:
//!   1. built-in grep, parallel  (`file_grep` default)
//!   2. built-in grep, serial
//!   3. a naive "read-everything + line scan" baseline (approximates the old
//!      `file_read`-then-eyeball / `shell findstr` approach)
//!   4. `rg.exe` (only if ripgrep is on PATH — external, for reference)
//!   5. PowerShell `Select-String` (only on Windows)
//!
//! It prints wall-clock times and each engine's match count so the numbers can
//! be cross-checked for equivalence. Timings are machine-specific; treat them
//! as relative, not absolute.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use FoxIR::tool::file_grep::{grep_files, GrepMode, GrepRequest};

/// Deterministic synthetic corpus: `files` files, each `lines` lines of ASCII,
/// with the needle sprinkled roughly every 40 lines.
fn build_corpus(root: &Path, files: usize, lines: usize) {
    fs::create_dir_all(root).unwrap();
    let mut state: u64 = 0x1234_5678_9abc_def0; // xorshift, deterministic
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for f in 0..files {
        let dir = if f % 10 == 0 { root.join(format!("sub{f}")) } else { root.to_path_buf() };
        if f % 10 == 0 {
            let _ = fs::create_dir_all(&dir);
        }
        let mut buf = String::with_capacity(lines * 48);
        for l in 0..lines {
            if next() % 40 == 0 {
                buf.push_str("process id 1234 NEEDLE connection to 10.0.0.5 established\n");
            } else {
                buf.push_str("2026-09-13 10:00:00 info worker thread handled a routine request\n");
            }
            let _ = l;
        }
        fs::write(dir.join(format!("log_{f}.txt")), buf).unwrap();
    }
}

/// Naive baseline: read every file fully into memory, lossy-decode, scan lines.
fn naive_scan(root: &Path, needle: &str) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "txt").unwrap_or(false) {
                if let Ok(bytes) = fs::read(&p) {
                    let text = String::from_utf8_lossy(&bytes);
                    count += text.lines().filter(|l| l.contains(needle)).count();
                }
            }
        }
    }
    count
}

/// Returns Some((elapsed_ms, match_count)) if the command exists and ran.
fn run_external(root: &Path, needle: &str, kind: ExtKind) -> Option<(u128, usize)> {
    let (prog, args): (&str, Vec<String>) = match kind {
        ExtKind::Rg => ("rg", vec![
            "--color".into(), "never".into(), "-c".into(), "-F".into(), needle.into(),
            root.to_string_lossy().to_string(),
        ]),
        ExtKind::Powershell => ("powershell", vec![
            "-NoProfile".into(), "-Command".into(), format!(
                "$m = Get-ChildItem -Path '{}' -Recurse -File -Filter *.txt | Select-String -Pattern '{}' -SimpleMatch; $m.Count",
                root.to_string_lossy().replace('\'', "''"),
                needle,
            ),
        ]),
        // findstr is the classic Windows shell content-search an agent would pick.
        // /S recurses, /C: treats the needle as a literal; output is one line per hit.
        ExtKind::Findstr => ("cmd", vec![
            "/c".into(), "findstr".into(), "/S".into(), "/I".into(),
            format!("/C:{needle}"),
            format!("{}\\*.txt", root.to_string_lossy()),
        ]),
    };
    let t = Instant::now();
    let out = Command::new(prog).args(&args).output().ok()?;
    let ms = t.elapsed().as_millis();
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let count = match kind {
        // rg -c prints "path:count" per file → sum the trailing integers.
        ExtKind::Rg => stdout
            .lines()
            .filter_map(|l| l.rsplit(':').next())
            .filter_map(|n| n.trim().parse::<usize>().ok())
            .sum(),
        ExtKind::Powershell => stdout.trim().parse::<usize>().unwrap_or(0),
        // findstr prints one line per matching line -> count the lines.
        ExtKind::Findstr => stdout.lines().count(),
    };
    Some((ms, count))
}

enum ExtKind {
    Rg,
    Powershell,
    Findstr,
}

#[test]
#[ignore]
fn bench_file_grep() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("corpus");
    let (nfiles, nlines) = (800usize, 200usize);
    let setup = Instant::now();
    build_corpus(&root, nfiles, nlines);
    let setup_ms = setup.elapsed().as_millis();

    let needle = "NEEDLE";

    // 1. built-in parallel
    let mut rp = GrepRequest {
        pattern: needle.into(),
        root: root.clone(),
        mode: GrepMode::Count, // count mode: no per-line Vec, apples-to-apples CPU
        regex: false,
        parallel: true,
        ..Default::default()
    };
    let t = Instant::now();
    let op = grep_files(&rp).unwrap();
    let par_ms = t.elapsed().as_millis();

    // 2. built-in serial
    rp.parallel = false;
    let t = Instant::now();
    let os = grep_files(&rp).unwrap();
    let ser_ms = t.elapsed().as_millis();

    // 3. naive read-all baseline
    let t = Instant::now();
    let naive_count = naive_scan(&root, needle);
    let naive_ms = t.elapsed().as_millis();

    // 4/5. external references (best-effort)
    let rg = run_external(&root, needle, ExtKind::Rg);
    let ps = run_external(&root, needle, ExtKind::Powershell);
    let fs_ = run_external(&root, needle, ExtKind::Findstr);

    println!("\n================ file_grep benchmark ================");
    println!("machine-specific; corpus = {nfiles} files x {nlines} lines (~{} files built in {setup_ms} ms)", nfiles);
    println!("-----------------------------------------------------");
    println!("{:<26}{:>12}{:>14}", "engine", "ms", "matches");
    println!("{:<26}{:>12}{:>14}", "built-in parallel", par_ms, op.total_matches);
    println!("{:<26}{:>12}{:>14}", "built-in serial", ser_ms, os.total_matches);
    println!("{:<26}{:>12}{:>14}", "naive read-all (baseline)", naive_ms, naive_count);
    match rg {
        Some((ms, c)) => println!("{:<26}{:>12}{:>14}", "rg.exe (external)", ms, c),
        None => println!("{:<26}{:>12}{:>14}", "rg.exe (external)", "-", "not on PATH"),
    }
    match ps {
        Some((ms, c)) => println!("{:<26}{:>12}{:>14}", "PowerShell Select-String", ms, c),
        None => println!("{:<26}{:>12}{:>14}", "PowerShell Select-String", "-", "n/a"),
    }
    match fs_ {
        Some((ms, c)) => println!("{:<26}{:>12}{:>14}", "shell findstr", ms, c),
        None => println!("{:<26}{:>12}{:>14}", "shell findstr", "-", "n/a"),
    }
    println!("-----------------------------------------------------");

    // Equivalence sanity check: all text engines must agree on the match count.
    assert_eq!(op.total_matches, os.total_matches, "parallel vs serial mismatch");
    assert_eq!(op.total_matches, naive_count, "built-in vs naive mismatch");
    if let Some((_, c)) = rg {
        assert_eq!(op.total_matches, c, "built-in vs rg.exe mismatch");
    }
    if let Some((_, c)) = ps {
        assert_eq!(op.total_matches, c, "built-in vs Select-String mismatch");
    }
    if let Some((_, c)) = fs_ {
        assert_eq!(op.total_matches, c, "built-in vs findstr mismatch");
    }
    println!("equivalence check: OK (all engines agree on match count)");
    println!("=====================================================\n");
}
