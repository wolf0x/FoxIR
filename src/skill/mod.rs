pub mod steps;
pub mod types;
pub mod verify;
pub mod self_improve;
pub mod metrics;
pub use self::types::SkillListingStrategy;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use tracing::{info, warn};

use self::steps::extract_contract;
use self::types::{SelectionPolicy, Skill, SkillContent, SkillMetadata, StepItem};
use super::server::NotifyTx;
use super::tool::Tool;
use crate::context::ToolContext;
use crate::error::AgentResult;
use async_trait::async_trait;
use serde_json::{json, Value};

/// Sanitize a skill name for use as a directory name.
/// Preserves the original name exactly — only strips characters that are
/// invalid or problematic in filesystem paths.
///   "VulnerabilityPrioritization" → "VulnerabilityPrioritization"
///   "My Skill" → "My Skill"
///   "bad/name" → "badname"
fn sanitize_dir_name(name: &str) -> String {
    // Strip characters illegal in directory names on Windows/Unix
    let illegal = ['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'];
    let sanitized: String = name.chars().filter(|c| !illegal.contains(c)).collect();
    // Trim whitespace and dots from edges (problematic on some filesystems)
    sanitized.trim_matches(|c| c == ' ' || c == '.').to_string()
}

/// Quote a string for safe inclusion in YAML frontmatter.
/// Wraps in double quotes and escapes internal backslashes and double quotes.
fn yaml_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

/// Strip a leading YAML frontmatter block (--- ... ---) from content.
/// If the content doesn't start with ---, returns it unchanged.
fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content;
    }
    let rest = &trimmed[3..];
    if let Some(end_pos) = rest.find("\n---") {
        // Skip past the closing --- and any trailing newline
        let after = &rest[end_pos + 4..];
        after.strip_prefix('\n').unwrap_or(after)
    } else {
        content
    }
}

pub struct SkillManager {
    skills: Arc<RwLock<Vec<Skill>>>,
    skills_dir: PathBuf,
    state_path: PathBuf,
    skill_self_improve: Arc<AtomicBool>,
    notify_tx: Option<NotifyTx>,
}

impl SkillManager {
    pub fn new(skills_dir: &str) -> Self {
        Self::new_with_notify(skills_dir, None)
    }

    pub fn new_with_notify(skills_dir: &str, notify_tx: Option<NotifyTx>) -> Self {
        let dir = PathBuf::from(skills_dir);
        let state_path = dir.join("skills_state.json");
        let mgr = Self {
            skills: Arc::new(RwLock::new(Vec::new())),
            skills_dir: dir,
            state_path,
            skill_self_improve: Arc::new(AtomicBool::new(false)),
            notify_tx,
        };
        mgr.reload();
        mgr
    }

    /// Enable/disable the skill self-improvement loop (default off).
    pub fn set_skill_self_improve(&self, enabled: bool) {
        self.skill_self_improve.store(enabled, Ordering::SeqCst);
    }

    pub fn reload(&self) {
        let mut skills = self.skills.write().unwrap();
        skills.clear();

        if !self.skills_dir.exists() {
            let _ = std::fs::create_dir_all(&self.skills_dir);
            return;
        }

        // Load enabled state
        let state = self.load_state();

        // Canonical base once so dir_depth can strip the prefix reliably
        // (Windows path casing / 8.3 aliases won't break the comparison).
        let canon_base = self.skills_dir.canonicalize().unwrap_or_else(|_| self.skills_dir.clone());

        // Scan directory-based skills recursively (skills/*/*/SKILL.md) so nested
        // child skills are discovered and can be loaded via skill_read_file.
        let dir_pattern = format!("{}/**/SKILL.md", self.skills_dir.display());
        for entry in glob::glob(&dir_pattern).ok().into_iter().flatten() {
            match entry {
                Ok(path) => {
                    // 跳过回收站：skills/_deleted/ 下的已删技能绝不能再次被发现，
                    // 否则 delete_skill 移到 _deleted 后 reload() 会把它"复活"。
                    if path.components().any(|c| c.as_os_str() == "_deleted") {
                        continue;
                    }
                    let skill_dir = path.parent()
                        .map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_string())
                        .unwrap_or_default();
                    match parse_skill_frontmatter(&path, skill_dir) {
                        Ok(mut skill) => {
                            if let Some(enabled) = state.get(&skill.metadata.name) {
                                skill.metadata.enabled = *enabled;
                            }
                            // Availability gate (agentskills.io): a skill whose
                            // `platforms` excludes this OS, or whose `deps` are
                            // not present on PATH, is not registered.
                            if let Some(reason) = unavailable_reason(&skill.metadata) {
                                warn!(
                                    "Skill '{}' unavailable on this host ({}); not registered (platforms={:?}, deps={:?})",
                                    skill.metadata.name, reason, skill.metadata.platforms, skill.metadata.deps
                                );
                                continue;
                            }
                            info!("Loaded skill: {} from {} (enabled={})", skill.metadata.name, path.display(), skill.metadata.enabled);
                            insert_skill_unique(&mut skills, &canon_base, skill);
                        }
                        Err(e) => {
                            metrics::record_load_failure();
                            warn!("{}", e);
                        }
                    }
                }
                Err(e) => warn!("Glob error: {}", e),
            }
        }
    }

    pub fn list(&self) -> Vec<SkillMetadata> {
        self.skills
            .read()
            .unwrap()
            .iter()
            .map(|s| s.metadata.clone())
            .collect()
    }

    /// Resolve a skill by name for runtime use (case-insensitive, then by
    /// directory name). Returns a clone of the matched [`Skill`] or `None`.
    pub fn find_skill(&self, name: &str) -> Option<Skill> {
        let name = name.trim().trim_start_matches('@');
        if name.is_empty() {
            return None;
        }
        let skills = self.skills.read().unwrap();
        let name_lower = name.to_lowercase();
        skills.iter().find(|s| s.metadata.name == name)
            .or_else(|| skills.iter().find(|s| s.metadata.name.to_lowercase() == name_lower))
            .or_else(|| {
                let dir_name = sanitize_dir_name(name).to_lowercase();
                skills.iter().find(|s| Path::new(&s.skill_dir).file_name()
                    .map(|n| n.to_string_lossy().to_lowercase() == dir_name).unwrap_or(false))
            })
            .cloned()
    }
    pub fn find_matching(&self, user_message: &str) -> Vec<(String, f32)> {
        self.find_matching_with(user_message, &SelectionPolicy::default())
    }

    /// Score and rank skills by weighted token overlap with the user message.
    ///
    /// Scoring weights (inspired by adk-skill's lexical overlap model):
    /// - Name match:         ×4.0
    /// - Description match:  ×2.5
    ///
    /// The raw score is normalized by `sqrt(body_token_count)` to prevent
    /// large documents (e.g. 33KB VPS skill) from dominating via sheer token volume.
    pub fn find_matching_with(&self, user_message: &str, policy: &SelectionPolicy) -> Vec<(String, f32)> {
        let skills = self.skills.read().unwrap();
        let query_tokens = Self::tokenize(user_message);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(String, f32)> = skills
            .iter()
            .filter(|s| s.metadata.enabled)
            .filter_map(|s| {
                let score = Self::score_skill(s, &query_tokens);
                if score >= policy.min_score {
                    Some((s.body().into_owned(), score))
                } else {
                    None
                }
            })
            .collect();

        // Sort by score descending, take top-K
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(policy.top_k);
        scored
    }

    /// Compute weighted relevance score for a single skill against query tokens.
    fn score_skill(skill: &Skill, query_tokens: &[String]) -> f32 {
        let mut score: f32 = 0.0;

        // Name token overlap (weight: 4.0)
        let name_tokens = Self::tokenize(&skill.metadata.name);
        let name_hits = query_tokens.iter().filter(|t| name_tokens.contains(t)).count();
        score += name_hits as f32 * 4.0;

        // Description token overlap (weight: 2.5)
        let desc_tokens = Self::tokenize(&skill.metadata.description);
        let desc_hits = query_tokens.iter().filter(|t| desc_tokens.contains(t)).count();
        score += desc_hits as f32 * 2.5;

        // (Body-token overlap and length normalization removed on purpose:
        // scoring is metadata-only so matching never forces a lazy body load.)

        score
    }

    /// Tokenize text into lowercase matchable units.
    ///
    /// ASCII words (≥3 chars) are extracted as whole tokens.
    /// CJK characters are emitted individually so that unsegmented
    /// Chinese/Japanese/Korean text still produces meaningful overlap.
    /// Build the "Active Skills Context" section of the system prompt.
    ///
    /// Pure model self-routing: every enabled skill is listed as a compact
    /// `name: description` catalog (`catalog_max` entries); the model decides
    /// which skill applies and loads its body on demand via `skill_read_file`.
    /// No lexical scoring, no auto-inlining. `NamesOnly` only emits a name
    /// list; `DiscoverToolOnly`/`Disabled` emit nothing.
    pub fn build_skills_prompt(
        &self,
        _matching_context: &str,
        strategy: SkillListingStrategy,
        _max_inline_chars: usize,
        catalog_max: usize,
        _hot_top_k: usize,
    ) -> (Option<String>, bool) {
        if matches!(strategy, SkillListingStrategy::DiscoverToolOnly | SkillListingStrategy::Disabled) {
            return (None, false);
        }

        let skills = self.skills.read().unwrap();
        let enabled: Vec<&Skill> = skills.iter().filter(|s| s.metadata.enabled).collect();
        if enabled.is_empty() {
            return (None, false);
        }

        let mut out = String::new();
        out.push_str("## Active Skills Context\n");
        out.push_str(
            "The following skill(s) are available. Review each name:description \
             and decide whether any Skill directly applies to the current task. \
             To use one, load it with `skill_read_file` (skill=\"<name>\", empty \
             path lists its files) so its instructions are injected. Large \
             supporting/reference files (e.g. 'reference.md') are never \
             auto-injected; read them with `skill_read_file`. Do NOT use generic \
             `file_read`/`shell` to locate skill files.\n\n",
        );

        match strategy {
            SkillListingStrategy::NamesOnly | SkillListingStrategy::DiscoverToolOnly => {
                let names: Vec<&str> = enabled.iter().map(|s| s.metadata.name.as_str()).collect();
                out.push_str(&format!("Available skills: {}\n", names.join(", ")));
            }
            SkillListingStrategy::Query => {
                let mut n = 0usize;
                let total = enabled.len();
                for s in &enabled {
                    if n >= catalog_max {
                        let omitted = total - n;
                        out.push_str(&format!(
                            "- ... and {} more (use `list_skills` to see them all)\n",
                            omitted
                        ));
                        break;
                    }
                    let desc = s.metadata.description.chars().take(120).collect::<String>();
                    out.push_str(&format!("- **{}**: {}\n", s.metadata.name, desc));
                    n += 1;
                }
            }
            SkillListingStrategy::Disabled => unreachable!("Disabled is short-circuited at the top of build_skills_prompt"),
        }

        metrics::record_catalog_turn();
        (Some(out), false)
    }

    fn tokenize(text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();

        for ch in text.chars() {
            if ch.is_ascii_alphanumeric() {
                current.push(ch.to_ascii_lowercase());
            } else if !ch.is_ascii() && ch.is_alphanumeric() {
                // CJK and other non-ASCII alphabetic: emit as individual tokens
                if current.len() >= 3 {
                    tokens.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
                tokens.push(ch.to_lowercase().to_string());
            } else {
                if current.len() >= 3 {
                    tokens.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
        }
        if current.len() >= 3 {
            tokens.push(current);
        }
        tokens
    }

    #[allow(dead_code)]
    pub fn skills_dir(&self) -> &Path {
        self.skills_dir.as_path()
    }

    /// Create a new skill as a directory: skills/{name}/SKILL.md + optional extra files.
    pub fn create_skill(&self, name: &str, description: &str, content: &str) -> Result<String, String> {
        self.create_skill_with_files(name, description, content, None)
    }

    /// Create a skill directory with SKILL.md and optional additional files.
    pub fn create_skill_with_files(
        &self,
        name: &str,
        description: &str,
        content: &str,
        files: Option<Vec<(String, String)>>,
    ) -> Result<String, String> {
        let dir_name = sanitize_dir_name(name);
        std::fs::create_dir_all(&self.skills_dir)
            .map_err(|e| format!("Failed to create dir: {}", e))?;

        let clean_content = strip_frontmatter(content);
        let md_content = format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n",
            yaml_quote(name), yaml_quote(description), clean_content
        );

        // Always create directory: skills/{dir_name}/SKILL.md
        let skill_dir = self.skills_dir.join(&dir_name);
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| format!("Failed to create skill dir: {}", e))?;
        std::fs::write(skill_dir.join("SKILL.md"), &md_content)
            .map_err(|e| format!("Failed to write SKILL.md: {}", e))?;

        // Write optional extra files
        if let Some(extra_files) = files {
            for (rel_path, file_content) in extra_files {
                let file_path = skill_dir.join(&rel_path);
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create subdirectory: {}", e))?;
                }
                std::fs::write(&file_path, &file_content)
                    .map_err(|e| format!("Failed to write {}: {}", rel_path, e))?;
            }
        }

        self.reload();
        self.notify_skills_changed();
        Ok(dir_name)
    }

    /// Delete a skill by name (removes the skill directory and reloads).
    pub fn delete_skill(&self, name: &str) -> Result<(), String> {
        let skills = self.skills.read().unwrap();
        let skill = skills.iter().find(|s| s.metadata.name == name)
            .ok_or_else(|| format!("Skill '{}' not found", name))?;
        let skill_dir = skill.skill_dir.clone();
        drop(skills);

        // Move the skill directory into the _deleted recycle bin so it can be
        // restored, instead of permanently deleting it.
        let dir_name = Path::new(&skill_dir)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| sanitize_dir_name(name));
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let trash_dir = self.skills_dir.join("_deleted");
        std::fs::create_dir_all(&trash_dir)
            .map_err(|e| format!("Failed to create _deleted dir: {}", e))?;
        let dest = trash_dir.join(format!("{}-{}", dir_name, ts));
        if std::fs::rename(&skill_dir, &dest).is_err() {
            // Fallback: permanent delete only if rename fails (e.g. cross-device).
            std::fs::remove_dir_all(&skill_dir)
                .map_err(|e| format!("Failed to remove skill directory: {}", e))?;
        }
        // Remove from state
        self.remove_from_state(name);
        self.reload();
        self.notify_skills_changed();
        Ok(())
    }

    /// Toggle a skill's enabled state.
    pub fn toggle_skill(&self, name: &str) -> Option<bool> {
        let mut skills = self.skills.write().unwrap();
        let skill = skills.iter_mut().find(|s| s.metadata.name == name)?;
        skill.metadata.enabled = !skill.metadata.enabled;
        let enabled = skill.metadata.enabled;
        drop(skills);
        // Persist
        self.save_state_entry(name, enabled);
        Some(enabled)
    }

    /// Build meta-tools for skill management (install_skill, list_skills, remove_skill)
    pub fn build_meta_tools(&self) -> Vec<Arc<dyn Tool>> {
        let skills_dir = self.skills_dir.clone();
        let skills_ref = self.skills.clone();

        vec![
            Arc::new(InstallSkillTool {
                skills_dir: skills_dir.clone(),
                skills: skills_ref.clone(),
            }) as Arc<dyn Tool>,
            Arc::new(ImproveSkillTool {
                skills_dir: skills_dir.clone(),
                skills: skills_ref.clone(),
                enabled: self.skill_self_improve.clone(),
            }) as Arc<dyn Tool>,
            Arc::new(ListSkillsTool {
                skills: skills_ref.clone(),
            }) as Arc<dyn Tool>,
            Arc::new(RemoveSkillTool {
                skills_dir: skills_dir.clone(),
                skills: skills_ref.clone(),
            }) as Arc<dyn Tool>,
            Arc::new(SkillReadFileTool {
                skills: skills_ref.clone(),
            }) as Arc<dyn Tool>,
        ]
    }

    /// Return the union of skill tool names derived dynamically from
    /// `build_meta_tools()` (SDD v1.5 §20.3 B4.2 / V.4, includes `improve_skill`).
    /// Builds a throwaway manager (no disk reload) so the name set can never
    /// drift from the tools actually registered.
    pub fn skill_tool_names() -> Vec<String> {
        let dummy = SkillManager {
            skills: Arc::new(RwLock::new(Vec::new())),
            skills_dir: PathBuf::new(),
            state_path: PathBuf::new(),
            skill_self_improve: Arc::new(AtomicBool::new(false)),
            notify_tx: None,
        };
        dummy.build_meta_tools().iter().map(|t| t.name().to_string()).collect()
    }

    // --- Notifications ---

    fn notify_skills_changed(&self) {
        if let Some(tx) = &self.notify_tx {
            let count = self.skills.read().map(|s| s.len()).unwrap_or(0);
            let msg = json!({"type": "skills_changed", "count": count}).to_string();
            let _ = tx.send(msg);
        }
    }

    // --- State persistence ---

    fn load_state(&self) -> HashMap<String, bool> {
        if !self.state_path.exists() {
            return HashMap::new();
        }
        match std::fs::read_to_string(&self.state_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => HashMap::new(),
        }
    }

    fn save_state(&self, state: &HashMap<String, bool>) {
        match serde_json::to_string_pretty(state) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.state_path, json) {
                    warn!("Failed to save skills state: {}", e);
                }
            }
            Err(e) => warn!("Failed to serialize skills state: {}", e),
        }
    }

    fn save_state_entry(&self, name: &str, enabled: bool) {
        let mut state = self.load_state();
        state.insert(name.to_string(), enabled);
        self.save_state(&state);
    }

    fn remove_from_state(&self, name: &str) {
        let mut state = self.load_state();
        state.remove(name);
        self.save_state(&state);
    }
}

/// Parse only the frontmatter of a [`Skill`] whose instruction body is
/// `Lazy` - the body is not read from disk or tokenized at startup, keeping
/// boot cheap even with many (or very large) skills. It is loaded on first
/// use via [`Skill::body`].
/// Number of path components between `base` and `dir`; larger = deeper.
/// Used to prefer the canonical (top-level) skill directory over a nested
/// versioned copy when the same skill name is found in more than one place.
fn dir_depth(dir: &str, base: &std::path::Path) -> usize {
    std::path::Path::new(dir)
        .strip_prefix(base)
        .map(|p| p.components().count())
        .unwrap_or(usize::MAX)
}

/// Insert a skill, de-duplicating by (case-insensitive) name so the skills
/// list never shows two cards with the same name. When the same name is found
/// in several directories (e.g. a versioned copy nested inside the skill
/// folder), keep the shallowest / canonical directory as the single source.
fn insert_skill_unique(skills: &mut Vec<Skill>, skills_dir: &std::path::Path, skill: Skill) {
    let name_lower = skill.metadata.name.to_lowercase();
    let skill_depth = dir_depth(&skill.skill_dir, skills_dir);
    if let Some(existing) = skills
        .iter_mut()
        .find(|s| s.metadata.name.to_lowercase() == name_lower)
    {
        if skill_depth < dir_depth(&existing.skill_dir, skills_dir) {
            *existing = skill;
        }
        return;
    }
    skills.push(skill);
}

/// Current host OS platform token (agentskills.io platform name).
fn current_os_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// True when `cmd` is resolvable on PATH (checks bare name + `.exe`/`.cmd`).
fn command_on_path(cmd: &str) -> bool {
    let cmd_lower = cmd.to_lowercase();
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| {
        for name in [
            cmd_lower.clone().into(),
            format!("{}.exe", cmd_lower),
            format!("{}.cmd", cmd_lower),
            format!("{}.bat", cmd_lower),
        ] {
            if dir.join(&name).is_file() {
                return true;
            }
        }
        false
    })
}

/// Return a human-readable reason when a skill is not usable on this host,
/// or `None` when it passes the agentskills.io availability gate
/// (`platforms` ⊆ current OS, every `deps` resolvable on PATH).
pub fn unavailable_reason(meta: &SkillMetadata) -> Option<String> {
    if !meta.platforms.is_empty() {
        let os = current_os_platform();
        let hit = meta.platforms.iter().any(|p| p.to_lowercase() == os);
        if !hit {
            return Some(format!(
                "declared platforms {:?} do not include this OS '{}'",
                meta.platforms, os
            ));
        }
    }
    for dep in &meta.deps {
        if !command_on_path(dep) {
            return Some(format!("required dependency '{}' not found on PATH", dep));
        }
    }
    None
}

fn parse_skill_frontmatter(path: &Path, skill_dir: String) -> Result<Skill, String> {
    let content = read_until_frontmatter_end(path)
        .ok_or_else(|| format!("Failed to read frontmatter of {}: no closing '---' fence", path.display()))?;
    let (frontmatter, _body) = split_frontmatter(&content)
        .ok_or_else(|| format!("No valid frontmatter (--- delimiters) in {}", path.display()))?;
    let metadata: SkillMetadata = serde_yaml::from_str(&frontmatter)
        .map_err(|e| format!("YAML parse error in {}: {} | frontmatter: {}", path.display(), e, frontmatter.chars().take(200).collect::<String>()))?;

    Ok(Skill {
        metadata,
        content: SkillContent::Lazy {
            path: path.to_path_buf(),
            cell: Arc::new(OnceLock::new()),
        },
        skill_dir,
        contract: Arc::new(OnceLock::new()),
    })
}

fn split_frontmatter(content: &str) -> Option<(String, String)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let rest = &trimmed[3..];
    if let Some(end_pos) = rest.find("\n---") {
        let frontmatter = rest[..end_pos].trim().to_string();
        let body = rest[end_pos + 4..].trim().to_string();
        Some((frontmatter, body))
    } else {
        None
    }
}

/// Maximum bytes of frontmatter scanned before giving up (16 KiB).
const MAX_FRONTMATTER_BYTES: usize = 16 * 1024;

/// True when `s` contains a closing `---` YAML fence.
fn has_closing_fence(s: &str) -> bool {
    s.contains("\n---") || s.contains("\r\n---")
}

/// Read only up to the closing frontmatter fence so startup never loads a
/// large skill body purely to parse its metadata.
fn read_until_frontmatter_end(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 4096];
    let mut acc = Vec::new();
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        acc.extend_from_slice(&buf[..n]);
        if acc.len() > MAX_FRONTMATTER_BYTES * 2 {
            return None;
        }
        let s = String::from_utf8_lossy(&acc);
        if has_closing_fence(&s) {
            return Some(s.into_owned());
        }
    }
}

/// Substitute `{skill_dir}` (native Windows path) and `{skill_dir_url}`
/// (forward-slash form, safer inside strings/commands) with the skill's
/// on-disk directory. Returns the body unchanged when no placeholder is used.
fn substitute_skill_dir(body: &str, skill_dir: &str) -> String {
    if !body.contains("{skill_dir") {
        return body.to_string();
    }
    let url_form = skill_dir.replace('\\', "/");
    body.replace("{skill_dir}", skill_dir)
        .replace("{skill_dir_url}", &url_form)
}

/// Read the instruction body of a SKILL.md lazily (skip frontmatter) and
/// resolve any `{skill_dir}` / `{skill_dir_url}` placeholders against the
/// skill's canonical on-disk directory.
fn read_skill_body(path: &Path, skill_dir: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    split_frontmatter(&content)
        .map(|(_fm, body)| substitute_skill_dir(&body, skill_dir))
}

impl Skill {
    /// Return the instruction body of the skill.
    ///
    /// `Eager` bodies (in-memory built skills) are borrowed directly. `Lazy`
    /// bodies read + strip frontmatter from disk on first access, then cache
    /// the result in an internal `OnceLock` so repeats don't re-read disk.
    pub fn body(&self) -> Cow<'_, str> {
        match &self.content {
            SkillContent::Eager(s) => Cow::Borrowed(s.as_str()),
            SkillContent::Lazy { path, cell } => Cow::Owned(
                cell.get_or_init(|| {
                    read_skill_body(path, &self.skill_dir).unwrap_or_else(|| {
                        warn!("Failed to lazy-load skill body from {}", path.display());
                        String::new()
                    })
                })
                .clone(),
            ),
        }
    }

    /// Lazily compiled linear step contract (empty when nothing parseable).
    /// Non-destructive: derived from the body once and cached.
    pub fn step_contract(&self) -> Vec<StepItem> {
        self.contract.get_or_init(|| extract_contract(&self.body())).clone()
    }
}

// --- Meta Tools ---

struct InstallSkillTool {
    skills_dir: PathBuf,
    skills: Arc<RwLock<Vec<Skill>>>,
}

#[async_trait]
impl Tool for InstallSkillTool {
    fn name(&self) -> &str { "install_skill" }
    fn description(&self) -> &str {
        "Install a new skill as a directory (skills/{name}/SKILL.md). \
         The name is preserved exactly as provided — use the exact name the user requested. \
         For large skills, write the content to a file first with file_write, then pass 'content_file' \
         (workspace-relative path) instead of inline 'content'. \
         Similarly, use 'source_path' in files[] entries to read additional files from the workspace."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name identifier — preserved exactly as provided (also used as directory name)." },
                "description": { "type": "string", "description": "Skill description for matching and display" },
                "version": { "type": "string", "description": "Optional semantic version (defaults to 1.0.0)." },
                "license": { "type": "string", "description": "Optional SPDX license identifier (agentskills.io)." },
                "platforms": { "type": "array", "items": { "type": "string" }, "description": "Optional target platforms: windows / macos / linux / ..." },
                "deps": { "type": "array", "items": { "type": "string" }, "description": "Optional required external commands / packages." },
                "allowed_tools": { "type": "array", "items": { "type": "string" }, "description": "Optional pre-approved tool names the skill may use (serialized as 'allowed-tools')." },
                "content": { "type": "string", "description": "Skill instructions inline (markdown body of SKILL.md). Use only for small skills; for large ones use content_file." },
                "content_file": { "type": "string", "description": "Workspace-relative path to a file containing the skill instructions (e.g. 'output/my_skill.md'). Alternative to 'content' for large skills." },
                "dir_name": { "type": "string", "description": "Override skill directory name (optional, rarely needed — defaults to the skill name)." },
                "files": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Relative path within skill directory (e.g., 'reference.md', 'templates/report.html')" },
                            "content": { "type": "string", "description": "File content inline (for small files)" },
                            "source_path": { "type": "string", "description": "Workspace-relative path to read file content from (for large files)" }
                        },
                        "required": ["path"]
                    },
                    "description": "Additional files within the skill directory. Provide either 'content' (inline) or 'source_path' (workspace file) for each."
                }
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        let name = args["name"].as_str().ok_or_else(|| "Missing 'name'".to_string())?;
        let desc = args["description"].as_str().unwrap_or("");
        let version = args["version"].as_str().map(String::from).unwrap_or_else(|| "1.0.0".to_string());
        let license = args["license"].as_str().map(String::from);
        let str_list = |key: &str| {
            args[key]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let platforms = str_list("platforms");
        let deps = str_list("deps");
        let allowed_tools = str_list("allowed_tools");

        // Resolve skill body: inline 'content' or read from 'content_file'
        let content = if let Some(inline) = args["content"].as_str() {
            inline.to_string()
        } else if let Some(file_path) = args["content_file"].as_str() {
            let full_path = Path::new(&ctx.working_dir).join(file_path);
            std::fs::read_to_string(&full_path)
                .map_err(|e| format!("Failed to read content_file '{}': {}", full_path.display(), e))?
        } else {
            return Err("Provide either 'content' (inline) or 'content_file' (workspace path) with the skill instructions.".into());
        };

        // Auto-derive directory name from 'name' (or use explicit override), always normalized
        let dir_name = args["dir_name"].as_str()
            .map(|s| sanitize_dir_name(s))
            .unwrap_or_else(|| sanitize_dir_name(name));

        let list_yaml = |items: &[String]| -> Vec<String> {
            items.iter().map(|t| format!("  - {}", yaml_quote(t))).collect()
        };
        let clean_content = strip_frontmatter(&content);
        let mut fm_lines = vec![
            format!("name: {}", yaml_quote(name)),
            format!("description: {}", yaml_quote(desc)),
            format!("version: {}", yaml_quote(&version)),
        ];
        if let Some(lic) = &license {
            fm_lines.push(format!("license: {}", yaml_quote(lic)));
        }
        if !platforms.is_empty() {
            fm_lines.push("platforms:".to_string());
            fm_lines.extend(list_yaml(&platforms));
        }
        if !deps.is_empty() {
            fm_lines.push("deps:".to_string());
            fm_lines.extend(list_yaml(&deps));
        }
        if !allowed_tools.is_empty() {
            fm_lines.push("allowed-tools:".to_string());
            fm_lines.extend(list_yaml(&allowed_tools));
        }
        let md_content = format!(
            "---\n{}\n---\n\n{}\n",
            fm_lines.join("\n"),
            clean_content
        );

        std::fs::create_dir_all(&self.skills_dir)
            .map_err(|e| format!("Failed to create dir: {}", e))?;

        // Always create directory: skills/{dir_name}/SKILL.md
        let skill_dir = self.skills_dir.join(&dir_name);
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| format!("Failed to create skill dir: {}", e))?;
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(&skill_md, &md_content)
            .map_err(|e| format!("Failed to write SKILL.md: {}", e))?;

        // Write optional extra files (inline content or read from source_path)
        let mut file_count = 0usize;
        if let Some(files_arr) = args["files"].as_array() {
            for item in files_arr {
                let rel_path = item["path"].as_str().ok_or_else(|| "Missing 'path' in files entry".to_string())?;
                let file_content = if let Some(inline) = item["content"].as_str() {
                    inline.to_string()
                } else if let Some(src) = item["source_path"].as_str() {
                    let full_path = Path::new(&ctx.working_dir).join(src);
                    std::fs::read_to_string(&full_path)
                        .map_err(|e| format!("Failed to read source_path '{}': {}", full_path.display(), e))?
                } else {
                    return Err(format!("files[{}]: provide either 'content' or 'source_path'", rel_path).into());
                };
                let file_path = skill_dir.join(rel_path);
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create subdirectory: {}", e))?;
                }
                std::fs::write(&file_path, &file_content)
                    .map_err(|e| format!("Failed to write {}: {}", rel_path, e))?;
                file_count += 1;
            }
        }

        // Reload skills
        let mut skills = self.skills.write().unwrap();
        let dir_str = skill_dir.to_string_lossy().to_string();
        if let Ok(skill) = parse_skill_frontmatter(&skill_md, dir_str) {
            skills.push(skill);
        }

        Ok(json!({
            "status": "installed",
            "name": name,
            "dir_name": dir_name,
            "skill_dir": skill_dir.to_string_lossy(),
            "files": file_count + 1
        }))
    }
}

struct SkillReadFileTool {
    skills: Arc<RwLock<Vec<Skill>>>,
}

#[async_trait]
impl Tool for SkillReadFileTool {
    fn name(&self) -> &str { "skill_read_file" }
    fn description(&self) -> &str {
        "Progressive skill reading: read a supporting/reference file inside an already-loaded skill \
directory (e.g. its 'reference.md' HTML template). Skills load their SKILL.md instructions \
automatically; use this tool to fetch large companion files on demand. Pass an empty 'path' to \
list the files available in the skill directory."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Name of the loaded skill (e.g. 'VulnerabilityPrioritization')" },
                "path": { "type": "string", "description": "Relative path within the skill directory, e.g. 'reference.md'. Empty string lists the directory contents." }
            },
            "required": ["skill", "path"]
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> AgentResult<Value> {
        metrics::record_read_call();
        let name = args["skill"].as_str().unwrap_or_default().trim();
        if name.is_empty() { return Err("Missing 'skill'".into()); }
        let rel = args["path"].as_str().unwrap_or("").trim().to_string();
        let skills = self.skills.read().unwrap();
        let name_lower = name.to_lowercase();
        let found = skills.iter().find(|s| s.metadata.name == name)
            .or_else(|| skills.iter().find(|s| s.metadata.name.to_lowercase() == name_lower))
            .or_else(|| {
                let dir_name = sanitize_dir_name(name).to_lowercase();
                skills.iter().find(|s| Path::new(&s.skill_dir).file_name().map(|n| n.to_string_lossy().to_lowercase() == dir_name).unwrap_or(false))
            });
        let Some(skill) = found else {
            metrics::record_read_failure();
            let available: Vec<&str> = skills.iter().map(|s| s.metadata.name.as_str()).collect();
            return Err(format!("Skill '{}' not found. Available skills: {:?}", name, available).into());
        };
        let skill_dir = PathBuf::from(&skill.skill_dir);
        let Ok(skill_canon) = skill_dir.canonicalize() else {
            metrics::record_read_failure();
            return Err(format!("Skill directory not accessible: {}", skill_dir.display()).into());
        };
        // Empty path -> list directory entries so the model can discover reference files.
        if rel.is_empty() {
            let mut files: Vec<Value> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&skill_dir) {
                for e in rd.filter_map(|e| e.ok()) {
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    files.push(json!({"name": e.file_name().to_string_lossy().into_owned(), "is_dir": is_dir}));
                }
            }
            files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            return Ok(json!({"skill": name, "files": files}));
        }
        let target = skill_dir.join(&rel);
        let Ok(target_canon) = target.canonicalize() else {
            return Err(format!("File not found: '{}' in skill '{}'", rel, name).into());
        };
        if !target_canon.starts_with(&skill_canon) || !target_canon.is_file() {
            return Err(format!("Refusing to read outside the skill directory: {}", rel).into());
        }
        let is_instruction = rel == "SKILL.md" || rel.eq_ignore_ascii_case("skill.md");
        match std::fs::read_to_string(&target_canon) {
            Ok(raw) => {
                let content = if is_instruction {
                    let mut b = strip_frontmatter(&raw).to_string();
                    if let Some(block) = steps::contract_block(&skill.step_contract()) { b.push_str(&block); }
                    b
                } else { raw };
                const CAP: usize = 120_000;
                let preview = if content.len() > CAP {
                    let mut end = CAP;
                    while end > 0 && !content.is_char_boundary(end) { end -= 1; }
                    format!("{}...\n[truncated at {} chars - ask for a specific section to read more]", &content[..end], end)
                } else { content };
                Ok(json!({"skill": name, "path": rel, "content": preview}))
            }
            Err(e) => Err(format!("Failed to read {}: {}", target_canon.display(), e).into()),
        }
    }
}
struct ImproveSkillTool {
    skills_dir: PathBuf,
    skills: Arc<RwLock<Vec<Skill>>>,
    enabled: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for ImproveSkillTool {
    fn name(&self) -> &str { "improve_skill" }
    fn description(&self) -> &str {
        "Propose a self-improvement patch to an existing skill's instruction body. \
         Bumps the skill's version, backs up the previous SKILL.md to the _audit dir, \
         and records the change (cooldown-gated). Refused for curated/human skills and \
         when skill self-improvement is disabled (skill_self_improve). Returns the new \
         version and the audit backup path for rollback."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Exact skill name to improve" },
                "new_content": { "type": "string", "description": "New instruction body (markdown, WITHOUT frontmatter) for the SKILL.md" },
                "reason": { "type": "string", "description": "Why this change improves the skill" }
            },
            "required": ["name", "new_content"]
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> AgentResult<Value> {
        use crate::skill::self_improve;
        if !self.enabled.load(Ordering::SeqCst) {
            return Err("Skill self-improvement is disabled (skill_self_improve=false).".into());
        }
        let name = args["name"].as_str().unwrap_or("");
        let new_content = args["new_content"].as_str().unwrap_or("");
        let reason = args["reason"].as_str().unwrap_or("");
        if name.is_empty() || new_content.is_empty() {
            return Err("Provide both 'name' and 'new_content'".into());
        }
        let mut skills = self.skills.write().unwrap();
        let Some(idx) = skills.iter().position(|s| s.metadata.name == name) else {
            return Err(format!("Skill '{}' not found.", name).into());
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if !self_improve::should_improve(&self.skills_dir, &skills[idx], now) {
            return Err(format!("Skill '{}' was recently improved; cooldown ({:?}s) not elapsed.", name, self_improve::IMPROVE_COOLDOWN_SECS).into());
        }
        let backup = self_improve::backup_skill(&self.skills_dir, &skills[idx])
            .map_err(|e| format!("Backup failed: {}", e))?;
        // Objective validation: if recent outcomes have regressed, surface a
        // rollback hint to the user rather than silently reverting (human gate).
        let regressed = self_improve::should_suggest_rollback(&self.skills_dir, name, 10, 0.5);
        let new_version = self_improve::apply_patch(&self.skills_dir, &skills[idx], new_content)?;
        metrics::record_improvement();
        let path = std::path::Path::new(&skills[idx].skill_dir).join("SKILL.md");
        skills[idx].content = SkillContent::Lazy { path: path.clone(), cell: Arc::new(OnceLock::new()) };
        skills[idx].metadata.version = new_version.clone();
        Ok(json!({
            "status": "ok",
            "name": name,
            "new_version": new_version,
            "backup_path": backup.to_string_lossy().to_string(),
            "reason": reason,
            "rollback_hint": regressed,
        }))
    }
}

struct ListSkillsTool {
    skills: Arc<RwLock<Vec<Skill>>>,
}

#[async_trait]
impl Tool for ListSkillsTool {
    fn name(&self) -> &str { "list_skills" }
    fn description(&self) -> &str { "List all currently installed skills with their names, descriptions, versions, and platforms." }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> AgentResult<Value> {
        let skills = self.skills.read().unwrap();
        let list: Vec<Value> = skills
            .iter()
            .map(|s| {
                json!({
                    "name": s.metadata.name,
                    "description": s.metadata.description,
                    "version": s.metadata.version,
                    "license": s.metadata.license,
                    "platforms": s.metadata.platforms,
                    "deps": s.metadata.deps,
                    "allowed_tools": s.metadata.allowed_tools,
                    "enabled": s.metadata.enabled,
                    "skill_dir": s.skill_dir,
                })
            })
            .collect();
        Ok(json!({ "skills": list, "count": list.len() }))
    }
}

struct RemoveSkillTool {
    #[allow(dead_code)]
    skills_dir: PathBuf,
    skills: Arc<RwLock<Vec<Skill>>>,
}

#[async_trait]
impl Tool for RemoveSkillTool {
    fn name(&self) -> &str { "remove_skill" }
    fn description(&self) -> &str { "Remove an installed skill by name. Removes the skill directory and all its contents." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name to remove" }
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> AgentResult<Value> {
        let name = args["name"].as_str().ok_or_else(|| "Missing 'name'".to_string())?;
        let mut skills = self.skills.write().unwrap();
        let name_lower = name.to_lowercase();

        // Try exact match first, then case-insensitive, then by directory name
        let pos = skills.iter().position(|s| s.metadata.name == name)
            .or_else(|| skills.iter().position(|s| s.metadata.name.to_lowercase() == name_lower))
            .or_else(|| {
                let dir_name = sanitize_dir_name(name).to_lowercase();
                skills.iter().position(|s| {
                    Path::new(&s.skill_dir).file_name()
                        .map(|n| n.to_string_lossy().to_lowercase() == dir_name)
                        .unwrap_or(false)
                })
            });

        if let Some(pos) = pos {
            let skill_dir = skills[pos].skill_dir.clone();

            // Move the skill directory into the _deleted recycle bin instead of
            // permanently deleting it, so it can be restored if removed by mistake.
            let dir_name = Path::new(&skill_dir)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| sanitize_dir_name(name));
            let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let trash_dir = self.skills_dir.join("_deleted");
            let _ = std::fs::create_dir_all(&trash_dir);
            let dest = trash_dir.join(format!("{}-{}", dir_name, ts));
            let moved = match std::fs::rename(&skill_dir, &dest) {
                Ok(()) => dest,
                Err(_) => {
                    // Fallback: permanent delete only if rename fails (e.g. cross-device).
                    let _ = std::fs::remove_dir_all(&skill_dir);
                    std::path::PathBuf::from(skill_dir)
                }
            };
            skills.remove(pos);
            Ok(json!({ "status": "removed", "name": name, "dir": moved }))
        } else {
            let available: Vec<&str> = skills.iter().map(|s| s.metadata.name.as_str()).collect();
            Err(format!("Skill '{}' not found. Available skills: {:?}", name, available).into())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_discovery_loads_nested_child_skills() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_discover_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Parent/Child")).unwrap();
        std::fs::write(tmp.join("Parent/SKILL.md"),
            "---\nname: Parent\ndescription: parent skill\ntriggers: [parent]\n---\n# Parent\n").unwrap();
        std::fs::write(tmp.join("Parent/Child/SKILL.md"),
            "---\nname: Child\ndescription: child skill\ntriggers: [child]\n---\n# Child\n").unwrap();
        std::fs::write(tmp.join("Parent/Child/reference.md"), "ref").unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let loaded: Vec<String> = mgr.list().iter().map(|m| m.name.clone()).collect();
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(loaded.contains(&"Parent".to_string()), "parent not loaded: {:?}", loaded);
        assert!(loaded.contains(&"Child".to_string()), "nested child not loaded: {:?}", loaded);
        assert_eq!(loaded.len(), 2, "expected Parent+Child, got {:?}", loaded);
    }

    #[test]
    fn flat_skills_still_discovered() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_flat_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Alpha")).unwrap();
        std::fs::write(tmp.join("Alpha/SKILL.md"),
            "---\nname: Alpha\ndescription: alpha\ntriggers: [alpha]\n---\n# Alpha\n").unwrap();
        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let names: Vec<String> = mgr.list().iter().map(|m| m.name.clone()).collect();
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(names.contains(&"Alpha".to_string()), "flat skill not loaded: {:?}", names);
    }

    #[test]
    fn skill_dir_placeholder_substitution_in_body() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_dir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("DirSkill")).unwrap();
        std::fs::write(tmp.join("DirSkill/SKILL.md"),
            "---\nname: DirSkill\ndescription: d\ntriggers: [d]\n---\n# DirSkill\nUse {skill_dir}/scripts/a.ps1 and {skill_dir_url}/refs.\n").unwrap();
        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let sk = mgr.find_skill("DirSkill").expect("skill found");
        let body = sk.body().into_owned();
        let native = sk.skill_dir.replace('\\', "\\\\");
        assert!(body.contains(&format!("Use {}/scripts/a.ps1", sk.skill_dir)), "native missing: {}", body);
        let url = sk.skill_dir.replace('\\', "/");
        assert!(body.contains(&format!("{}/refs", url)), "url form missing: {}", body);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn strip_frontmatter_removes_metadata_for_instruction_load() {
        let md = "---\nname: Child\ndescription: child\ntriggers: [x]\n---\n\n# Child Body\nStep 1: do thing\n";
        let body = strip_frontmatter(md);
        let bl = body.to_lowercase();
        assert!(bl.contains("# child body"), "body missing: {}", body);
        assert!(!bl.contains("name:"), "frontmatter not stripped: {}", body);
        // No frontmatter -> unchanged.
        assert_eq!(strip_frontmatter("# Plain\n"), "# Plain\n");
    }
    #[test]
    fn lazy_body_loads_instruction_and_is_cached() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_lazy_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("LazySkill")).unwrap();
        let md = "---\nname: LazySkill\ndescription: lazy\ntriggers: [lazy]\n---\n\n# Lazy Body\nStep 1: do it\n";
        std::fs::write(tmp.join("LazySkill/SKILL.md"), md).unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let skills = mgr.skills.read().unwrap();
        let sk = skills.iter().find(|s| s.metadata.name == "LazySkill").expect("lazy skill loaded");
        assert!(sk.content.is_lazy(), "disk-loaded skill should be lazy");

        let body = sk.body();
        let bl = body.to_lowercase();
        assert!(bl.contains("# lazy body"), "body missing: {}", body);
        assert!(!bl.contains("name:"), "frontmatter leaked: {}", body);

        let body2 = sk.body();
        assert_eq!(body.as_ref(), body2.as_ref(), "lazy body should be cached");
        drop(skills);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn eager_body_borrows_in_memory_content() {
        let sk = Skill {
            metadata: SkillMetadata {
                name: "Eager".to_string(),
                description: "e".to_string(),
                license: None,
                version: String::new(),
                platforms: vec![],
                deps: vec![],
                allowed_tools: vec![],
                enabled: true,
            },
            content: SkillContent::Eager("# Eager Body\n".to_string()),
            skill_dir: String::new(),
            contract: Arc::new(OnceLock::new()),
        };
        assert!(!sk.content.is_lazy(), "eager skill should not be lazy");
        assert!(sk.body().contains("# Eager Body"));
    }

    #[test]
    fn build_skills_prompt_catalog_only() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_prompt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("AlwaysSkill")).unwrap();
        std::fs::create_dir_all(tmp.join("ProcessSkill")).unwrap();
        std::fs::create_dir_all(tmp.join("ColdSkill")).unwrap();
        std::fs::create_dir_all(tmp.join("NoisySkill")).unwrap();
        std::fs::write(tmp.join("AlwaysSkill/SKILL.md"),
            "---\nname: AlwaysSkill\ndescription: always available\n---\n# Always Body\n").unwrap();
        std::fs::write(tmp.join("ProcessSkill/SKILL.md"),
            "---\nname: ProcessSkill\ndescription: process report and triage\ntriggers: [process]\n---\n# Process Body\nStep here\n").unwrap();
        std::fs::write(tmp.join("ColdSkill/SKILL.md"),
            "---\nname: ColdSkill\ndescription: unrelated cold skill\n---\n# Cold Body\n").unwrap();
        // Weak match: generic browser-ish description shares no tokens with the
        // query -- must stay in the on-demand catalog, not inline.
        std::fs::write(tmp.join("NoisySkill/SKILL.md"),
            "---\nname: NoisySkill\ndescription: browser automation page rendering\n---\n# Noisy Body\n").unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());

        let (q_opt, q_act) = mgr.build_skills_prompt("process report", SkillListingStrategy::Query, 20_000, 40, 3);
        let q = q_opt.expect("query section present");
        let ql = q.to_lowercase();
        // Model self-routing: no body is auto-inlined; every enabled skill is a
        // catalogue entry the model loads on demand via skill_read_file.
        assert!(!ql.contains("# process body"), "no body should be inlined: {}", q);
        assert!(!ql.contains("# always body"), "no body should be inlined: {}", q);
        assert!(!ql.contains("# cold body"), "no body should be inlined: {}", q);
        assert!(!ql.contains("# noisy body"), "no body should be inlined: {}", q);
        assert!(ql.contains("processskill"), "skill should be catalogued: {}", q);
        assert!(ql.contains("alwaysskill"), "skill should be catalogued: {}", q);
        assert!(ql.contains("coldskill"), "skill should be catalogued: {}", q);
        assert!(ql.contains("noisyskill"), "skill should be catalogued: {}", q);
        assert!(!q_act, "catalog-only routing must NOT mark a task skill active");

        let (n_opt, _) = mgr.build_skills_prompt("process", SkillListingStrategy::NamesOnly, 20_000, 40, 3);
        let n = n_opt.expect("names section present");
        assert!(n.contains("AlwaysSkill"));
        assert!(!n.contains("# always body"), "names-only must not inline bodies: {}", n);

        assert!(mgr.build_skills_prompt("x", SkillListingStrategy::DiscoverToolOnly, 0, 0, 0).0.is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// G-no-skill-injection: `Disabled` listing returns no skill prompt at all
    /// (same short-circuit as `DiscoverToolOnly`).
    #[test]
    fn gate_disabled_listing_emits_nothing() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_gate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Alpha")).unwrap();
        std::fs::write(tmp.join("Alpha/SKILL.md"),
            "---\nname: Alpha\ndescription: alpha\n---\n# Alpha\n").unwrap();
        let mgr = SkillManager::new(tmp.to_str().unwrap());
        assert!(!mgr.list().is_empty(), "Alpha skill should load");
        let (p, active) = mgr.build_skills_prompt("alpha", SkillListingStrategy::Disabled, 20_000, 40, 3);
        assert!(p.is_none(), "Disabled must not inject a skill prompt");
        assert!(!active, "Disabled must not mark a task skill active");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// G-no-skill-tools: the skill tool-name set is exactly the 5 meta tools and
    /// maps to a stable set used by `.minus(skill_tool_names())`.
    #[test]
    fn gate_skill_tool_names_are_the_five_meta_tools() {
        let names = SkillManager::skill_tool_names();
        assert_eq!(names.len(), 5, "expected exactly 5 skill tools, got {:?}", names);
        for n in ["install_skill", "skill_read_file", "improve_skill", "list_skills", "remove_skill"] {
            assert!(names.iter().any(|s| s == n), "missing skill tool {}", n);
        }
    }

    /// G-availability: skills whose `platforms` exclude the host OS, or whose
    /// `deps` are missing from PATH, are not registered (agentskills.io gate).
    #[test]
    fn availability_gate_excludes_incompatible_platforms() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_avail_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("HostSkill")).unwrap();
        std::fs::create_dir_all(tmp.join("ForeignSkill")).unwrap();
        std::fs::create_dir_all(tmp.join("DepSkill")).unwrap();
        let os = current_os_platform();
        let other = if os == "linux" { "windows" } else { "linux" };
        std::fs::write(tmp.join("HostSkill/SKILL.md"),
            format!("---\nname: HostSkill\ndescription: host-native\nplatforms: [{}]\n---\n# H\n", os)).unwrap();
        std::fs::write(tmp.join("ForeignSkill/SKILL.md"),
            format!("---\nname: ForeignSkill\ndescription: foreign\nplatforms: [{}]\n---\n# F\n", other)).unwrap();
        std::fs::write(tmp.join("DepSkill/SKILL.md"),
            "---\nname: DepSkill\ndescription: needs a tool\ndeps: [zz_foxir_nonexistent_tool_xyz]\n---\n# D\n").unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let loaded: Vec<String> = mgr.list().iter().map(|m| m.name.clone()).collect();
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(loaded.contains(&"HostSkill".to_string()), "native-platform skill should load: {:?}", loaded);
        assert!(!loaded.contains(&"ForeignSkill".to_string()), "foreign-platform skill must NOT load: {:?}", loaded);
        assert!(!loaded.contains(&"DepSkill".to_string()), "missing-dep skill must NOT load: {:?}", loaded);
    }

}



