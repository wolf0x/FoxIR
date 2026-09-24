//! Browser executable discovery + launch diagnostics for CDP automation.
//!
//! Why this exists instead of relying on `chromiumoxide`'s built-in detection:
//! on Windows its auto-detect consults only `App Paths\chrome.exe` in the
//! registry and hardcodes exactly one fallback path
//! (`Program Files (x86)\Microsoft\Edge\Application\msedge.exe`). A browser
//! installed anywhere else — Edge under `Program Files`, a per-user Chrome
//! install, anything discoverable only through `App Paths\msedge.exe` — is
//! missed outright, and the resulting error
//! ("Could not auto detect a chrome executable") names no path, so the
//! operator cannot tell "not installed" from "installed where we don't look".
//!
//! This module therefore resolves the executable itself and always reports
//! what was tried.

use std::path::{Path, PathBuf};

/// Environment variable we honour for an explicit executable path.
pub const ENV_PRIMARY: &str = "FOXIR_BROWSER";
/// Legacy/industry-standard variable also honoured (Puppeteer uses it).
pub const ENV_LEGACY: &str = "CHROME";

/// How long `Browser::launch` waits for the DevTools endpoint before giving up.
/// Mirrors chromiumoxide's own `LAUNCH_TIMEOUT` (20_000 ms); we set it explicitly
/// so the number quoted in a failure message is ours, not a library default that
/// could change under us.
pub const LAUNCH_WAIT_SECS: u64 = 20;

/// Where a candidate path came from. Surfaces in logs and error text so a
/// failure names its own source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Settings / config.toml override
    Config,
    /// FOXIR_BROWSER or CHROME environment variable
    Env,
    /// Found through %PATH%
    Path,
    /// Windows registry (`App Paths`)
    Registry,
    /// Hardcoded install locations
    WellKnown,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Config => "config",
            Source::Env => "env",
            Source::Path => "PATH",
            Source::Registry => "registry",
            Source::WellKnown => "well-known",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub source: Source,
}

impl Candidate {
    pub fn describe(&self) -> String {
        format!("{} [{}]", self.path.display(), self.source.label())
    }
}

/// Build the ordered candidate list from raw inputs.
///
/// Deliberately pure: it takes the environment/registry/path results rather
/// than reading them, so ordering (the part that actually decides behaviour)
/// is unit-testable without touching the filesystem.
///
/// Order is: explicit override → environment → %PATH% → registry → well-known.
/// The override wins unconditionally so a machine where auto-detect is wrong
/// can always be pinned from Settings without a rebuild.
pub fn build_candidates(
    config_override: Option<&str>,
    env_primary: Option<&str>,
    env_legacy: Option<&str>,
    path_var: Option<&str>,
    registry: Vec<PathBuf>,
    well_known: Vec<PathBuf>,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut push = |p: Option<&str>, source: Source| {
        let p = p.map(str::trim).unwrap_or("");
        if p.is_empty() {
            return;
        }
        let pb = PathBuf::from(p);
        if !out.iter().any(|c| c.path == pb) {
            out.push(Candidate { path: pb, source });
        }
    };

    push(config_override, Source::Config);
    push(env_primary, Source::Env);
    push(env_legacy, Source::Env);

    for exe in executables_in_path(path_var) {
        if !out.iter().any(|c| c.path == exe) {
            out.push(Candidate { path: exe, source: Source::Path });
        }
    }
    for p in registry {
        if !out.iter().any(|c| c.path == p) {
            out.push(Candidate { path: p, source: Source::Registry });
        }
    }
    for p in well_known {
        if !out.iter().any(|c| c.path == p) {
            out.push(Candidate { path: p, source: Source::WellKnown });
        }
    }
    out
}

/// Browser binaries worth looking for on %PATH%, in order.
pub const BROWSER_NAMES: &[&str] = &["msedge.exe", "chrome.exe", "chromium.exe", "brave.exe"];

/// Scan each `%PATH%` directory for a browser binary. Pure (takes the PATH
/// string, does no existence check) so the split/lookup logic is testable.
pub fn executables_in_path(path_var: Option<&str>) -> Vec<PathBuf> {
    let Some(var) = path_var else { return Vec::new() };
    let mut hits = Vec::new();
    for dir in var.split(';') {
        let dir = dir.trim();
        if dir.is_empty() {
            continue;
        }
        for name in BROWSER_NAMES {
            let cand = PathBuf::from(dir).join(name);
            if cand.exists() && !hits.contains(&cand) {
                hits.push(cand);
            }
        }
    }
    hits
}

/// First candidate that actually exists on disk.
pub fn first_existing(candidates: &[Candidate]) -> Option<Candidate> {
    candidates.iter().find(|c| c.path.exists()).cloned()
}

/// True for a path segment shaped like a Chromium version directory
/// (digits and dots, at least two dots) — e.g. `152.0.4191.66`.
pub fn is_version_like(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_digit() || c == '.')
        && name.matches('.').count() >= 2
}

/// Version inferred from the install layout.
///
/// Chromium ships a launcher stub at `.../Application/msedge.exe` and keeps the
/// real binaries in a sibling directory named after its version. Either layout
/// yields the version without spawning anything (which on Windows would open a
/// browser window rather than print a version).
///
/// Returns `"unknown"` when the layout doesn't match — never a guess.
pub fn version_from_layout(path: &Path) -> String {
    let Some(parent) = path.parent() else { return "unknown".into() };
    // Case 1: the exe sits inside a version directory.
    if let Some(dir_name) = parent.file_name().and_then(|s| s.to_str()) {
        if is_version_like(dir_name) {
            return dir_name.to_string();
        }
    }
    // Case 2: sibling version directories (pick the highest).
    let mut versions: Vec<String> = match std::fs::read_dir(parent) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| is_version_like(n))
            .collect(),
        Err(_) => return "unknown".into(),
    };
    if versions.is_empty() {
        return "unknown".into();
    }
    versions.sort_by(|a, b| version_key(a).cmp(&version_key(b)));
    versions.pop().unwrap_or_else(|| "unknown".into())
}

/// Numeric sort key so `152.0.4191.66` orders after `92.0.1072.69`.
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect()
}

/// Registry-resolved browser paths (Windows only; empty elsewhere).
///
/// Both `App Paths` keys matter: `msedge.exe` is normally registered there
/// even when it is NOT the path chromiumoxide hardcodes.
pub fn registry_browsers() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        const NAMES: &[&str] = &["msedge.exe", "chrome.exe"];
        const BASES: &[&str] = &[
            "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\",
            "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\App Paths\\",
        ];
        let mut out = Vec::new();
        for base in BASES {
            for name in NAMES {
                let key = format!("{base}{name}");
                // The default value of an App Paths key is the full path.
                if let Some(p) = read_registry_sz(&key, "") {
                    let pb = PathBuf::from(p.trim_matches('"'));
                    if pb.exists() && !out.contains(&pb) {
                        out.push(pb);
                    }
                }
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Read a REG_SZ value. `value_name` empty = the key's default value.
#[cfg(windows)]
fn read_registry_sz(key_path: &str, value_name: &str) -> Option<String> {
    use std::mem::size_of;
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
    };

    let to_wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let key_w = to_wide(key_path);
    let val_w = to_wide(value_name);

    // 520 UTF-16 slots: comfortably above MAX_PATH, and a full path never
    // needs a second round trip.
    let mut buf = [0u16; 520];
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        // `size` is an in/out parameter — it must be reset per root, otherwise
        // a failed HKLM probe leaves a stale length behind for HKCU.
        let mut size = (buf.len() * size_of::<u16>()) as u32;
        let res = unsafe {
            RegGetValueW(
                root,
                PCWSTR(key_w.as_ptr()),
                PCWSTR(val_w.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                Some(&mut size),
            )
        };
        if res.is_ok() && size > 2 {
            let chars = ((size as usize) / size_of::<u16>()).saturating_sub(1).min(buf.len());
            return String::from_utf16(&buf[..chars]).ok();
        }
    }
    None
}

/// Hardcoded install locations, both program-files roots plus per-user Chrome.
pub fn well_known_browsers() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Ok(v) = std::env::var(var) {
            roots.push(PathBuf::from(v));
        }
    }
    for fallback in [r"C:\Program Files (x86)", r"C:\Program Files"] {
        let p = PathBuf::from(fallback);
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        // A per-user Chrome install lives here; Edge never does, but trying
        // both under one root costs nothing.
        roots.push(PathBuf::from(local));
    }
    const RELS: &[&str] = &[
        r"Microsoft\Edge\Application\msedge.exe",
        r"Google\Chrome\Application\chrome.exe",
    ];
    let mut out = Vec::new();
    for root in &roots {
        for rel in RELS {
            let cand = root.join(rel);
            if !out.contains(&cand) {
                out.push(cand);
            }
        }
    }
    out
}

/// Outcome of an executable search: what we picked (if anything) and the full
/// list we looked at. Both halves are needed on the failure path — an error
/// that says only "not found" is what made this bug unreportable in the field.
pub struct Discovery {
    pub chosen: Option<Candidate>,
    pub tried: Vec<Candidate>,
}

impl Discovery {
    /// Human-readable account of the search, for logs and error text.
    pub fn summary(&self) -> String {
        match &self.chosen {
            Some(c) => format!("selected {}", c.describe()),
            None => {
                let tried: Vec<String> = self.tried.iter().map(Candidate::describe).collect();
                if tried.is_empty() {
                    "no browser candidate could be constructed for this platform".to_string()
                } else {
                    format!("nothing found; searched: {}", tried.join(", "))
                }
            }
        }
    }
}

/// Resolve the executable to launch.
///
/// `override_path` comes from Settings (empty = auto). Never fails: when
/// nothing exists, `chosen` is `None` and `tried` explains where we looked.
pub fn discover(override_path: &str) -> Discovery {
    let env_primary = std::env::var(ENV_PRIMARY).ok();
    let env_legacy = std::env::var(ENV_LEGACY).ok();
    let path_var = std::env::var("PATH").ok();
    let candidates = build_candidates(
        Some(override_path),
        env_primary.as_deref(),
        env_legacy.as_deref(),
        path_var.as_deref(),
        registry_browsers(),
        well_known_browsers(),
    );
    let chosen = first_existing(&candidates);
    Discovery { chosen, tried: candidates }
}

/// Everything a launch failure needs to explain itself.
pub struct LaunchContext {
    /// The executable we actually asked to run (None = discovery already failed)
    pub chosen: Option<Candidate>,
    /// The full list we searched
    pub tried: Vec<Candidate>,
    pub profile_dir: PathBuf,
    pub headless: bool,
}

/// One-line profile state for diagnostics (never recurses — a profile holds
/// thousands of files and this must stay cheap on the failure path).
pub fn describe_profile(dir: &Path) -> String {
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            let entries = rd.count();
            let mtime = std::fs::metadata(dir)
                .and_then(|m| m.modified())
                .map(|t| {
                    chrono::DateTime::<chrono::Local>::from(t)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|_| "unknown".to_string());
            format!("{} ({} top-level entries, last modified {})", dir.display(), entries, mtime)
        }
        Err(_) => format!("{} (missing)", dir.display()),
    }
}

/// Compose the actionable launch-failure message.
///
/// The distinguishing question on a machine where the browser "fails to
/// launch" is *which* of two very different causes it is — nothing installed
/// where we look, versus an instance that will not die. Naming the executable,
/// its version, the profile state and how long we waited separates them in one
/// log line instead of a support round-trip.
pub fn describe_failure(ctx: &LaunchContext, raw_error: &str, waited_secs: u64) -> String {
    let mut s = String::new();
    match &ctx.chosen {
        Some(c) => s.push_str(&format!(
            "Browser launch failed: {} (version {})",
            c.describe(),
            version_from_layout(&c.path)
        )),
        None => s.push_str("Browser launch failed: no browser executable found"),
    }
    s.push_str(&format!(
        "\n  mode: {} | profile: {}",
        if ctx.headless { "headless" } else { "visible window" },
        describe_profile(&ctx.profile_dir)
    ));
    if ctx.chosen.is_none() {
        let tried: Vec<String> = ctx.tried.iter().map(Candidate::describe).collect();
        if !tried.is_empty() {
            s.push_str(&format!("\n  searched: {}", tried.join(", ")));
        }
    }
    if waited_secs > 0 {
        s.push_str(&format!(
            "\n  waited up to {}s for the DevTools endpoint to appear.",
            waited_secs
        ));
    }
    s.push_str(&format!("\n  raw error: {}", raw_error.trim()));
    s.push_str("\n  Likely causes, in order: (1) a previous browser instance is still \
        shutting down and holds the profile — close any leftover msedge/chrome windows \
        or switch this agent to a visible window once; (2) the browser is installed \
        somewhere auto-detection does not look — set the explicit path in Settings; \
        (3) the installed browser is too old to expose the DevTools endpoint — update it.");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf { PathBuf::from(s) }
    fn cands(v: &[(&str, Source)]) -> Vec<Candidate> {
        v.iter().map(|(s, src)| Candidate { path: p(s), source: *src }).collect()
    }

    #[test]
    fn config_override_is_first_and_deduplicated() {
        let out = build_candidates(
            Some(r"C:\Edge\msedge.exe"),
            Some(r"C:\Edge\msedge.exe"),
            Some(r"C:\Chrome\chrome.exe"),
            None,
            vec![p(r"C:\Windows\msedge.exe")],
            vec![p(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe")],
        );
        assert_eq!(out[0].source, Source::Config, "explicit override must win");
        assert_eq!(out[0].path, p(r"C:\Edge\msedge.exe"));
        // The same path from CHROME/FOXIR_BROWSER must not appear twice.
        let dupe = out.iter().filter(|c| c.path == p(r"C:\Edge\msedge.exe")).count();
        assert_eq!(dupe, 1, "duplicate paths must collapse, got {:?}", out);
    }

    #[test]
    fn precedence_order_is_config_env_registry_wellknown() {
        let out = build_candidates(
            Some("/cfg"),
            Some("/env1"),
            Some("/env2"),
            None,
            vec![p("/reg")],
            vec![p("/wk")],
        );
        let order: Vec<&str> = out.iter().map(|c| c.path.to_str().unwrap()).collect();
        assert_eq!(order, vec!["/cfg", "/env1", "/env2", "/reg", "/wk"]);
    }

    #[test]
    fn blank_override_and_empty_env_are_ignored() {
        let out = build_candidates(Some("   "), Some(""), None, None, vec![], vec![p("/wk")]);
        assert_eq!(out.len(), 1, "blank strings must not become candidates: {:?}", out);
        assert_eq!(out[0].source, Source::WellKnown);
    }

    #[test]
    fn version_directory_layout_is_recognised() {
        assert!(is_version_like("152.0.4191.66"));
        assert!(is_version_like("92.0.1072.69"));
        assert!(!is_version_like("Application"), "not a version dir");
        assert!(!is_version_like("12.3"), "needs at least two dots");
        assert!(!is_version_like("beta.1.2.3"));
    }

    #[test]
    fn version_key_sorts_numerically_not_lexically() {
        let mut v = vec!["92.0.1072.69", "152.0.4191.66", "111.0.1661.54"];
        v.sort_by(|a, b| version_key(a).cmp(&version_key(b)));
        assert_eq!(v, vec!["92.0.1072.69", "111.0.1661.54", "152.0.4191.66"],
                   "lexicographic sort would put 111 before 92");
    }

    #[test]
    fn version_from_layout_returns_unknown_rather_than_guessing() {
        // A path that cannot exist: no parent listing, no version sibling.
        assert_eq!(version_from_layout(Path::new(r"Z:\nope\msedge.exe")), "unknown");
        assert_eq!(version_from_layout(Path::new(r"Z:\nope\13.0.2785.0\msedge.exe")),
                   "13.0.2785.0", "an exe inside a version dir reports that dir");
    }

    #[test]
    fn discovery_summary_names_every_tried_location() {
        let d = Discovery {
            chosen: None,
            tried: cands(&[
                (r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe", Source::WellKnown),
                (r"C:\Edge\msedge.exe", Source::Config),
            ]),
        };
        let s = d.summary();
        assert!(s.contains("nothing found"), "{s}");
        assert!(s.contains("[config]"), "must name the override source: {s}");
        assert!(s.contains("[well-known]"), "must name the searched fallback: {s}");

        let d2 = Discovery { chosen: Some(Candidate { path: p("/x"), source: Source::Env }), tried: vec![] };
        assert_eq!(d2.summary(), "selected /x [env]");
    }

    #[test]
    fn launch_failure_separates_missing_browser_from_stuck_instance() {
        let chosen = Some(Candidate { path: p("Z:\\nothing\\msedge.exe"), source: Source::Registry });
        let ctx = LaunchContext {
            chosen,
            tried: vec![],
            profile_dir: PathBuf::from(r"Z:\definitely\not\here"),
            headless: true,
        };
        let msg = describe_failure(&ctx, "timed out", 20);
        assert!(msg.contains("[registry]"), "must say where the chosen path came from: {msg}");
        assert!(msg.contains("headless"), "{msg}");
        assert!(msg.contains("(missing)"), "profile state must be reported: {msg}");
        assert!(msg.contains("20s"), "wait budget must be visible: {msg}");
        assert!(msg.contains("previous browser instance"), "must list the top cause: {msg}");
    }

    #[test]
    fn profile_description_handles_absent_directory_without_recursion() {
        let s = describe_profile(Path::new(r"Z:\no\such\profile"));
        assert!(s.contains("(missing)"), "{s}");
    }

    #[test]
    fn first_existing_skips_missing_paths() {
        let tmp = std::env::temp_dir();
        let list = cands(&[
            (r"Z:\missing\msedge.exe", Source::Config),
            (tmp.to_str().unwrap(), Source::WellKnown),
        ]);
        let hit = first_existing(&list).expect("temp dir exists");
        assert_eq!(hit.source, Source::WellKnown);
    }
}
