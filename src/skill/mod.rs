pub mod steps;
pub mod types;
pub mod verify;
pub mod self_improve;
pub mod metrics;
pub mod schema;
pub mod grants;
pub mod install;
pub use self::types::{RankedSkill, SkillListingStrategy};

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

/// Reject paths that would escape `base` (rejects absolute paths and any `..`
/// component), returning the safely joined path. Used to stop malicious skill
/// paths (`files[].path`, `content_file`, `source_path`) from writing or reading
/// outside the intended directory.
pub fn resolve_inside(base: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("empty relative path".to_string());
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(format!("absolute path not allowed: {rel}"));
    }
    for comp in rel_path.components() {
        if !matches!(comp, std::path::Component::Normal(_) | std::path::Component::CurDir) {
            return Err(format!("path escapes the target directory: {rel}"));
        }
    }
    let base_canon = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    Ok(base_canon.join(rel_path))
}

/// Bump the patch component of a semantic version (x.y.z -> x.y.(z+1)).
fn bump_patch(v: &str) -> String {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() == 3 {
        if let Ok(n) = parts[2].parse::<u32>() {
            return format!("{}.{}.{}", parts[0], parts[1], n + 1);
        }
    }
    format!("{}.0.1", v)
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
    /// Skills discovered but refused by validation, with the blocking findings.
    rejected: Arc<RwLock<Vec<(String, Vec<schema::Finding>)>>>,
    /// Case-folded name collisions resolved at load time (Warn, not rejection).
    duplicates: Arc<RwLock<Vec<schema::Finding>>>,
    /// Warn-level findings per registered skill.
    warnings: Arc<RwLock<Vec<(String, Vec<schema::Finding>)>>>,
    /// Source manifests that exist but cannot be read.
    manifest_issues: Arc<RwLock<Vec<String>>>,
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
            rejected: Arc::new(RwLock::new(Vec::new())),
            duplicates: Arc::new(RwLock::new(Vec::new())),
            warnings: Arc::new(RwLock::new(Vec::new())),
            manifest_issues: Arc::new(RwLock::new(Vec::new())),
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
        self.rejected.write().unwrap().clear();
        self.duplicates.write().unwrap().clear();
        self.warnings.write().unwrap().clear();
        self.manifest_issues.write().unwrap().clear();

        if !self.skills_dir.exists() {
            let _ = std::fs::create_dir_all(&self.skills_dir);
            return;
        }

        // Load enabled state
        let store = self.read_store();
        for note in store.notes() {
            warn!("{}", note);
        }

        // Canonical base once so dir_depth can strip the prefix reliably
        // (Windows path casing / 8.3 aliases won't break the comparison).
        let canon_base = self.skills_dir.canonicalize().unwrap_or_else(|_| self.skills_dir.clone());

        // Scan directory-based skills recursively (skills/*/*/SKILL.md) so nested
        // child skills are discovered and can be loaded via skill_read_file.
        let dir_pattern = format!("{}/**/SKILL.md", self.skills_dir.display());
        let mut duplicate_findings: Vec<schema::Finding> = Vec::new();
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
                        Ok((mut skill, warns)) => {
                            if !warns.is_empty() {
                                self.warnings
                                    .write()
                                    .unwrap()
                                    .push((skill.metadata.name.clone(), warns));
                            }
                            if let Err(e) = schema::read_manifest(Path::new(&skill.skill_dir)) {
                                warn!("{}", e);
                                self.manifest_issues.write().unwrap().push(e);
                            }
                            if let Some(enabled) = store.enabled_flag(&skill.metadata.name) {
                                skill.metadata.enabled = enabled;
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
                            if let Some(finding) = insert_skill_unique(&mut skills, &canon_base, skill) {
                                duplicate_findings.push(finding);
                            }
                        }
                        Err(findings) => {
                            metrics::record_load_failure();
                            warn!("Skill at {} is not registered: {:?}", path.display(), findings);
                            self.rejected
                                .write()
                                .unwrap()
                                .push((path.display().to_string(), findings));
                        }
                    }
                }
                Err(e) => warn!("Glob error: {}", e),
            }
        }

        drop(skills);
        for finding in duplicate_findings {
            warn!("Duplicate skill name claim resolved: {:?}", finding);
            self.duplicates.write().unwrap().push(finding);
        }
    }

    /// Skills found on disk but refused by validation: `(SKILL.md path, blocking
    /// findings)`. Surfaced so a rejected skill is visible instead of silently absent.
    pub fn rejected(&self) -> Vec<(String, Vec<schema::Finding>)> {
        self.rejected.read().unwrap().clone()
    }

    /// Case-folded name collisions seen at load time: the loser of each pair,
    /// resolved deterministically by directory depth.
    pub fn duplicate_claims(&self) -> Vec<schema::Finding> {
        self.duplicates.read().unwrap().clone()
    }

    /// Warn-level findings grouped by the skill that triggered them.
    pub fn validation_warnings(&self) -> Vec<(String, Vec<schema::Finding>)> {
        self.warnings.read().unwrap().clone()
    }

    /// Human-readable problems reading a skill's source manifest.
    pub fn manifest_issue_reports(&self) -> Vec<String> {
        self.manifest_issues.read().unwrap().clone()
    }

    /// Build the run's grant ledger from the skills loaded so far in it.
    ///
    /// A skill contributes a grant only when all three hold: its content is
    /// bound by a recorded manifest hash, the user consented to *that* hash, and
    /// the file has not changed since. Anything else stays inert — the narrowed
    /// call then goes through the normal prompt.
    pub fn grant_ledger(&self, loaded: &[String], session: &str) -> grants::SkillGrantLedger {
        let mut ledger =
            grants::SkillGrantLedger::new(self.skills_dir.join("_audit").join("grants.jsonl"));
        let store = self.read_store();
        let skills = self.skills.read().unwrap();

        for name in loaded {
            let Some(skill) = skills.iter().find(|s| &s.metadata.name == name) else {
                continue;
            };
            let dir = Path::new(&skill.skill_dir);
            let Ok(bytes) = std::fs::read(dir.join("SKILL.md")) else {
                continue;
            };
            let current_hash = schema::skill_md_hash(&bytes);
            let recorded_hash = schema::read_manifest(dir)
                .ok()
                .flatten()
                .and_then(|manifest| manifest.skill_md_hash);
            let state = store.state_of(name);
            let facts = grants::GrantFacts {
                current_hash: &current_hash,
                recorded_hash: recorded_hash.as_deref(),
                consented_hash: state.and_then(|s| s.grants.consented_hash.as_deref()),
                declined_hash: state.and_then(|s| s.grants.declined_hash.as_deref()),
                tools: skill.metadata.allowed_tools.clone(),
            };
            if let grants::GrantState::Consent { actions, names } = grants::grant_state(&facts) {
                if actions.is_empty() && names.is_empty() {
                    continue;
                }
                let plan = grants::GrantPlan {
                    actions,
                    names: names.clone(),
                    findings: Vec::new(),
                };
                ledger.add(grants::SkillGrant {
                    skill: name.clone(),
                    full_hash: current_hash,
                    session: session.to_string(),
                    names,
                    profile: grants::fragment(name, &plan),
                });
            }
        }
        ledger
    }

    /// Where a package waits while it is being screened: a sibling of `skills/`,
    /// so `reload()`'s `{skills}/**/SKILL.md` glob cannot see it.
    fn quarantine_root(&self) -> PathBuf {
        self.skills_dir
            .parent()
            .map(|parent| parent.join(".skill-quarantine"))
            .unwrap_or_else(|| self.skills_dir.join(".skill-quarantine"))
    }

    /// True when the recycle bin already holds a skill directory with this name
    /// (`<name>` or `<name>-<timestamp>` from `delete_skill`).
    fn recycle_bin_holds(&self, dir_name: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(self.skills_dir.join("_deleted")) else {
            return false;
        };
        let prefixed = format!("{dir_name}-");
        entries.filter_map(|entry| entry.ok()).any(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name == dir_name || name.starts_with(&prefixed)
        })
    }

    /// Install a skill from a folder the user pointed at (P4a, user-initiated).
    ///
    /// The source tree is screened *before* it is copied, staged outside the
    /// scanned tree, screened again, then moved in with a same-volume rename.
    /// A refused install leaves `skills/` untouched.
    pub fn install_from_folder(
        &self,
        source: &Path,
        name: Option<&str>,
    ) -> Result<install::InstallOutcome, String> {
        let requested = name
            .map(str::to_string)
            .or_else(|| {
                source
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .filter(|n| !n.trim().is_empty())
            .ok_or_else(|| "no skill name: pass one, or point at a named folder".to_string())?;
        let dir_name = sanitize_dir_name(&requested);
        if dir_name.is_empty() {
            return Err(format!("'{requested}' has no characters usable as a directory name"));
        }
        if self.recycle_bin_holds(&dir_name) {
            return Err(format!(
                "'{dir_name}' is in skills/_deleted; restore it from the recycle bin instead of installing over it"
            ));
        }

        // Pre-screen the source so oversized or malformed content is never copied.
        let source_entries =
            install::walk_staged(source).map_err(|e| install::errors_text(&e))?;
        install::plan_entries(&source_entries).map_err(|e| install::errors_text(&e))?;

        let staging = self
            .quarantine_root()
            .join(format!("incoming-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&staging)
            .map_err(|e| format!("create staging {}: {}", staging.display(), e))?;
        if let Err(e) = install::copy_tree(source, &staging) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e.to_string());
        }

        match install::install_staged(&self.skills_dir, &staging, &dir_name) {
            Ok(outcome) => {
                // install_staged renamed the tree away; only a leftover is possible on
                // some failure paths, and that leftover is our own scratch copy.
                let _ = std::fs::remove_dir_all(&staging);
                self.reload();
                self.notify_skills_changed();
                Ok(outcome)
            }
            Err(errors) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(install::errors_text(&errors))
            }
        }
    }

    /// Install a fetched single `SKILL.md` (P4a, user-initiated).
    pub fn install_single_file(
        &self,
        content: &[u8],
        name: Option<&str>,
    ) -> Result<install::InstallOutcome, String> {
        let text = std::str::from_utf8(content)
            .map_err(|_| "the downloaded file is not UTF-8 text".to_string())?;
        let requested = name
            .map(str::to_string)
            .or_else(|| {
                schema::parse_frontmatter(text, "probe")
                    .ok()
                    .map(|doc| doc.metadata.name)
            })
            .filter(|n| !n.trim().is_empty())
            .ok_or_else(|| "no skill name: the download has no usable frontmatter name".to_string())?;
        let dir_name = sanitize_dir_name(&requested);
        if dir_name.is_empty() {
            return Err(format!("'{requested}' has no characters usable as a directory name"));
        }
        if self.recycle_bin_holds(&dir_name) {
            return Err(format!(
                "'{dir_name}' is in skills/_deleted; restore it from the recycle bin instead of installing over it"
            ));
        }

        let staging = self
            .quarantine_root()
            .join(format!("incoming-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&staging)
            .map_err(|e| format!("create staging {}: {}", staging.display(), e))?;
        std::fs::write(staging.join("SKILL.md"), content)
            .map_err(|e| format!("write staged SKILL.md: {e}"))?;

        self.finish_staged_install(&staging, &dir_name)
    }

    /// Land a staged tree and refresh the catalog; our own scratch copy is always
    /// cleaned up, and a refused install never reaches `skills/`.
    fn finish_staged_install(
        &self,
        staging: &Path,
        dir_name: &str,
    ) -> Result<install::InstallOutcome, String> {
        match install::install_staged(&self.skills_dir, staging, dir_name) {
            Ok(outcome) => {
                let _ = std::fs::remove_dir_all(staging);
                self.reload();
                self.notify_skills_changed();
                Ok(outcome)
            }
            Err(errors) => {
                let _ = std::fs::remove_dir_all(staging);
                Err(install::errors_text(&errors))
            }
        }
    }

    /// Record the user's decision about one skill's declared `allowed-tools`.
    ///
    /// Deliberately user-initiated: a skill cannot approve itself, and the
    /// decision binds to the full hash of `SKILL.md` as it is on disk right now.
    /// An unbound or drifted file is refused — approving content you cannot
    /// identify is how a stale approval outlives the thing it was given for.
    pub fn record_skill_grant(&self, name: &str, approved: bool) -> Result<String, String> {
        let (skill_name, skill_dir, tools) = {
            let skills = self.skills.read().unwrap();
            let wanted = name.trim().to_lowercase();
            let skill = skills
                .iter()
                .find(|s| s.metadata.name.to_lowercase() == wanted)
                .ok_or_else(|| format!("Skill '{name}' is not registered"))?;
            (
                skill.metadata.name.clone(),
                PathBuf::from(&skill.skill_dir),
                skill.metadata.allowed_tools.clone(),
            )
        };
        let bytes = std::fs::read(skill_dir.join("SKILL.md"))
            .map_err(|e| format!("read {}: {}", skill_dir.display(), e))?;
        let current_hash = schema::skill_md_hash(&bytes);
        let Some(recorded_hash) = schema::read_manifest(&skill_dir)?
            .and_then(|manifest| manifest.skill_md_hash)
        else {
            return Err(format!(
                "cannot bind '{skill_name}' to its content: no {} manifest, so nothing can be approved",
                schema::MANIFEST_FILE
            ));
        };
        if recorded_hash != current_hash {
            return Err(format!(
                "'{skill_name}' changed since it was recorded (manifest {recorded_hash} != current {current_hash}); review the new content first"
            ));
        }
        let mut store = self.read_store();
        store.record_consent(&skill_name, &current_hash, &tools, approved)?;
        Ok(current_hash)
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
    /// Rank enabled skills against a message.
    ///
    /// Scoring weights (inspired by adk-skill's lexical overlap model):
    /// name ×4.0, description ×2.5, triggers ×2.0. Matching is metadata-only —
    /// the returned payload is the skill's **name**, so ranking never reads (or
    /// caches) a skill body.
    pub fn rank(&self, user_message: &str, policy: &SelectionPolicy) -> Vec<RankedSkill> {
        let skills = self.skills.read().unwrap();
        let query_tokens = Self::tokenize(user_message);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<RankedSkill> = skills
            .iter()
            .filter(|s| s.metadata.enabled)
            .map(|s| RankedSkill {
                name: s.metadata.name.clone(),
                score: Self::score_skill(s, &query_tokens),
            })
            .filter(|ranked| ranked.score >= policy.min_score)
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
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

        // Trigger-phrase token overlap (weight: 2.0)
        let trigger_tokens: Vec<String> = skill
            .metadata
            .triggers
            .iter()
            .flat_map(|phrase| Self::tokenize(phrase))
            .collect();
        let trigger_hits = query_tokens.iter().filter(|t| trigger_tokens.contains(t)).count();
        score += trigger_hits as f32 * 2.0;

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
        matching: &[RankedSkill],
        strategy: SkillListingStrategy,
        catalog_max: usize,
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
            "The skill catalog below is names and one-line descriptions only — no \
             instructions are inlined. Review each name:description and decide whether \
             any Skill directly applies to the current task. To use one, load it with \
             `skill_read_file` (skill=\"<name>\", empty path lists its files; \
             path=\"SKILL.md\" returns its instructions). Large supporting/reference \
             files (e.g. 'reference.md') are never auto-injected; read them with \
             `skill_read_file`. Do NOT use generic `file_read`/`shell` to locate \
             skill files.\n\n",
        );

        let mut activated = false;
        match strategy {
            SkillListingStrategy::NamesOnly | SkillListingStrategy::DiscoverToolOnly => {
                let names: Vec<&str> = enabled.iter().map(|s| s.metadata.name.as_str()).collect();
                out.push_str(&format!("Available skills: {}\n", names.join(", ")));
            }
            SkillListingStrategy::Query => {
                // Matched skills first (in score order), then the rest of the catalog.
                let mut listed: Vec<&Skill> = Vec::new();
                for ranked in matching {
                    if let Some(found) = enabled.iter().find(|s| s.metadata.name == ranked.name) {
                        if !listed.iter().any(|l| l.metadata.name == found.metadata.name) {
                            listed.push(found);
                        }
                    }
                }
                for s in &enabled {
                    if !listed.iter().any(|l| l.metadata.name == s.metadata.name) {
                        listed.push(s);
                    }
                }

                for s in listed.iter().take(catalog_max) {
                    // An empty description degrades to "discoverable by name" — do
                    // not emit a trailing empty colon for it.
                    if s.metadata.description.trim().is_empty() {
                        out.push_str(&format!("- **{}**\n", s.metadata.name));
                    } else {
                        let desc = s.metadata.description.chars().take(120).collect::<String>();
                        out.push_str(&format!("- **{}**: {}\n", s.metadata.name, desc));
                    }
                }
                if listed.len() > catalog_max {
                    out.push_str(&format!(
                        "- ... and {} more (use `list_skills` to see them all)\n",
                        listed.len() - catalog_max
                    ));
                }

                if let Some(top) = strong_hit(matching) {
                    metrics::record_suggestion_shown();
                    out.push_str(&format!(
                        "\nLikely applicable: \"{}\" — call skill_read_file(skill=\"{}\") before acting.\n",
                        top.name, top.name
                    ));
                }
            }
            SkillListingStrategy::Disabled => unreachable!("Disabled is short-circuited at the top of build_skills_prompt"),
        }

        metrics::record_catalog_turn();
        (Some(out), activated)
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
        schema::write_manifest(&skill_dir, &schema::SourceManifest::local(md_content.as_bytes()))?;

        // Write optional extra files
        if let Some(extra_files) = files {
            for (rel_path, file_content) in extra_files {
                let file_path = resolve_inside(&skill_dir, &rel_path)?;
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
        let name_lower = name.to_lowercase();
        let skill = skills.iter().find(|s| s.metadata.name.to_lowercase() == name_lower)
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
        let name_lower = name.to_lowercase();
        let skill = skills.iter_mut().find(|s| s.metadata.name.to_lowercase() == name_lower)?;
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
            rejected: Arc::new(RwLock::new(Vec::new())),
            duplicates: Arc::new(RwLock::new(Vec::new())),
            warnings: Arc::new(RwLock::new(Vec::new())),
            manifest_issues: Arc::new(RwLock::new(Vec::new())),
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

    /// Read `skills_state.json` through the two-stage store: a schema change or a
    /// corrupt file can no longer silently wipe the user's enable/disable flags.
    fn read_store(&self) -> schema::SkillStateStore {
        schema::SkillStateStore::load(&self.state_path)
    }

    fn save_state_entry(&self, name: &str, enabled: bool) {
        let mut store = self.read_store();
        if let Err(e) = store.set_enabled(name, enabled) {
            warn!("{}", e);
        }
    }

    fn remove_from_state(&self, name: &str) {
        let mut store = self.read_store();
        if let Err(e) = store.remove(name) {
            warn!("{}", e);
        }
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

/// The `/api/skills` payload: registered skills plus the two ways a skill on
/// disk can be missing from that list (blocked by validation, or lost to a
/// case-folded name collision). Silence here is what makes a vanished skill
/// look like a bug.
pub fn catalog_report(manager: &SkillManager) -> Value {
    let skills = manager.list();
    json!({
        "skills": skills,
        "count": skills.len(),
        "rejected": manager
            .rejected()
            .iter()
            .map(|(path, findings)| json!({ "path": path, "findings": findings }))
            .collect::<Vec<Value>>(),
        "duplicates": Value::Array(
            manager
                .duplicate_claims()
                .iter()
                .map(|finding| json!({ "finding": finding }))
                .collect(),
        ),
        "warnings": Value::Array(
            manager
                .validation_warnings()
                .iter()
                .map(|(name, findings)| json!({ "skill": name, "findings": findings }))
                .collect(),
        ),
        "manifest_issues": Value::Array(
            manager
                .manifest_issue_reports()
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    })
}

/// A top-1 match strong enough to earn the "Likely applicable" line: twice the
/// `min_score` the ranker itself filters with, read from the same
/// `SelectionPolicy` rather than restated as a literal. Shared by the Instant
/// catalog and the Expert brief so the two can never drift apart.
pub fn strong_hit(matching: &[RankedSkill]) -> Option<&RankedSkill> {
    let threshold = SelectionPolicy::default().min_score * 2.0;
    matching.first().filter(|top| top.score >= threshold)
}

/// Insert a skill, de-duplicating by (case-insensitive) name so the skills
/// list never shows two cards with the same name. When the same name is found
/// in several directories (e.g. a versioned copy nested inside the skill
/// folder), keep the shallowest / canonical directory as the single source.
fn insert_skill_unique(
    skills: &mut Vec<Skill>,
    skills_dir: &std::path::Path,
    skill: Skill,
) -> Option<schema::Finding> {
    let name_lower = skill.metadata.name.to_lowercase();
    let skill_depth = dir_depth(&skill.skill_dir, skills_dir);
    if let Some(existing) = skills
        .iter_mut()
        .find(|s| s.metadata.name.to_lowercase() == name_lower)
    {
        if skill_depth < dir_depth(&existing.skill_dir, skills_dir) {
            let displaced = std::mem::replace(existing, skill);
            return Some(schema::Finding::DuplicateNameFolded {
                name: displaced.metadata.name,
                kept: existing.skill_dir.clone(),
                dropped: displaced.skill_dir,
            });
        }
        return Some(schema::Finding::DuplicateNameFolded {
            name: existing.metadata.name.clone(),
            kept: existing.skill_dir.clone(),
            dropped: skill.skill_dir,
        });
    }
    skills.push(skill);
    None
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

fn parse_skill_frontmatter(
    path: &Path,
    skill_dir: String,
) -> Result<(Skill, Vec<schema::Finding>), Vec<schema::Finding>> {
    let content = read_until_frontmatter_end(path)
        .ok_or_else(|| vec![schema::Finding::InvalidYaml {
            error: format!("{}: no closing '---' fence", path.display()),
        }])?;
    let (frontmatter, _body) = split_frontmatter(&content).ok_or_else(|| {
        vec![schema::Finding::InvalidYaml {
            error: format!("{}: no valid frontmatter (--- delimiters)", path.display()),
        }]
    })?;
    let dir_name = Path::new(&skill_dir)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let doc = schema::parse_frontmatter(&frontmatter, &dir_name)?;
    let warnings = doc.warnings;

    Ok((
        Skill {
            metadata: doc.metadata,
            content: SkillContent::Lazy {
                path: path.to_path_buf(),
                cell: Arc::new(OnceLock::new()),
            },
            skill_dir,
            contract: Arc::new(OnceLock::new()),
        },
        warnings,
    ))
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
        let mut version = args["version"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| "1.0.0".to_string());
        // Only one skill per name: if a skill with the same (case-insensitive) name
        // already exists and its version equals the incoming one, bump the patch so
        // the re-install is distinguishable in the catalog and no duplicate card.
        if let Some(existing_v) = self
            .skills
            .read()
            .unwrap()
            .iter()
            .find(|s| s.metadata.name.to_lowercase() == name.trim().to_lowercase())
            .map(|s| s.metadata.version.clone())
        {
            if existing_v == version {
                version = bump_patch(&version);
            }
        }
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
            let full_path = resolve_inside(Path::new(&ctx.working_dir), file_path)?;
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
        if let Err(e) = schema::write_manifest(&skill_dir, &schema::SourceManifest::local(md_content.as_bytes())) {
            warn!("{}", e);
        }

        // Write optional extra files (inline content or read from source_path)
        let mut file_count = 0usize;
        if let Some(files_arr) = args["files"].as_array() {
            for item in files_arr {
                let rel_path = item["path"].as_str().ok_or_else(|| "Missing 'path' in files entry".to_string())?;
                let file_content = if let Some(inline) = item["content"].as_str() {
                    inline.to_string()
                } else if let Some(src) = item["source_path"].as_str() {
                    let full_path = resolve_inside(Path::new(&ctx.working_dir), src)?;
                    std::fs::read_to_string(&full_path)
                        .map_err(|e| format!("Failed to read source_path '{}': {}", full_path.display(), e))?
                } else {
                    return Err(format!("files[{}]: provide either 'content' or 'source_path'", rel_path).into());
                };
                let file_path = resolve_inside(&skill_dir, rel_path)?;
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
        if let Ok((skill, warns)) = parse_skill_frontmatter(&skill_md, dir_str) {
            if !warns.is_empty() {
                warn!("Skill '{}' registered with warnings: {:?}", skill.metadata.name, warns);
            }
            if let Some(finding) = insert_skill_unique(&mut skills, &self.skills_dir, skill) {
                warn!("Duplicate skill name claim resolved: {:?}", finding);
            }
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
        "Progressive skill reading: the catalog in the system prompt is names and \
descriptions only — no instructions are inlined. Load a skill's instructions with \
`skill_read_file` (skill=\"<name>\", path=\"SKILL.md\"), or pass an empty path to list \
the files in its directory; large supporting/reference files (e.g. 'reference.md') are \
read the same way on demand. Do NOT use generic `file_read`/`shell` to locate skill files."
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
                    metrics::record_skill_load();
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
        let name_lower = name.to_lowercase();
        let Some(idx) = skills.iter().position(|s| s.metadata.name.to_lowercase() == name_lower) else {
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
    fn query_catalog_lists_names_and_suggests_only_for_a_strong_hit() {
        // 规格 §2.2：Instant 的注入面是"目录 + 至多一行建议"。建议行必须自己带上
        // 加载方式，并且只在 top-1 分数 ≥ min_score×2 时出现 —— 弱命中不给建议。
        let tmp = std::env::temp_dir().join(format!("rs_skill_sugg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for (dir, name, desc) in [
            ("TriageFlow", "TriageFlow", "phishing triage workflow"),
            ("ColdOne", "ColdOne", "unrelated cold skill"),
        ] {
            std::fs::create_dir_all(tmp.join(dir)).unwrap();
            std::fs::write(
                tmp.join(dir).join("SKILL.md"),
                format!("---\nname: {}\ndescription: {}\n---\n# {} BODY-SENTINEL-7c3b\n", name, desc, name),
            )
            .unwrap();
        }
        let mgr = SkillManager::new(tmp.to_str().unwrap());

        let strong = vec![RankedSkill { name: "TriageFlow".to_string(), score: 2.0 }];
        let out = mgr
            .build_skills_prompt(&strong, SkillListingStrategy::Query, 40)
            .0
            .expect("query strategy emits a section");
        let weak = vec![RankedSkill { name: "ColdOne".to_string(), score: 0.11 }];
        let out2 = mgr
            .build_skills_prompt(&weak, SkillListingStrategy::Query, 40)
            .0
            .expect("query strategy emits a section");
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(out.contains("TriageFlow"), "the hit must be listed: {out}");
        assert!(
            out.contains("Likely applicable: \"TriageFlow\"") && out.contains("skill_read_file"),
            "a strong hit earns exactly one suggestion line telling how to load it: {out}"
        );
        assert!(!out.contains("BODY-SENTINEL-7c3b"), "no body bytes may be injected: {out}");

        assert!(out2.contains("ColdOne"), "weak hits are still catalogued: {out2}");
        assert!(
            !out2.contains("Likely applicable"),
            "a weak hit must not earn a suggestion line: {out2}"
        );
        assert!(!out2.contains("BODY-SENTINEL-7c3b"), "no body bytes may be injected: {out2}");
    }

    #[test]
    fn matching_returns_ranked_names_without_bodies() {
        // 规格 §2.1/§2.2 的核心：排序结果的载荷是 (name, score)，不是正文。
        // 今天唯一读正文的地方就是这里（find_matching_with 用 s.body() 造返回值），
        // 所以改成 name 之后，未加载技能的 OnceLock 必须仍然是空的。
        let tmp = std::env::temp_dir().join(format!("rs_skill_rank_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Ranked")).unwrap();
        std::fs::write(
            tmp.join("Ranked/SKILL.md"),
            "---\nname: Ranked\ndescription: triage workflow\n---\n# Ranked Body SENTINEL-4f7a\n",
        )
        .unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let ranked = mgr.rank("triage", &types::SelectionPolicy::default());
        let sk = mgr.find_skill("Ranked").expect("the skill is registered");
        let body_loaded = match &sk.content {
            SkillContent::Lazy { cell, .. } => cell.get().is_some(),
            SkillContent::Eager(_) => true,
        };
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(ranked.len(), 1, "{:?}", ranked);
        assert_eq!(ranked[0].name, "Ranked", "the payload must be the name, not the body");
        assert_eq!(ranked[0].score, 2.5, "one description hit at x2.5, hand-derived");
        assert!(!body_loaded, "ranking must not materialize the lazy body");
    }

    #[test]
    fn triggers_earn_a_weight_in_matching() {
        // 规格 §2.1：`triggers` 从"文档里声称 ×2.0、代码里没有"变成真的打分面。
        // Phish 的 name/description 都不含查询词，只有 triggers 命中；
        // 另一条 Desc 用 description 命中，作为"别把既有信号改坏"的正对照。
        let tmp = std::env::temp_dir().join(format!("rs_skill_trig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Phish")).unwrap();
        std::fs::create_dir_all(tmp.join("Desc")).unwrap();
        std::fs::write(
            tmp.join("Phish/SKILL.md"),
            "---\nname: netcap\ndescription: network capture tool\ntriggers: [phishing]\n---\n# P\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("Desc/SKILL.md"),
            "---\nname: mailer\ndescription: phishing email triage\n---\n# D\n",
        )
        .unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let ranked = mgr.rank("phishing", &types::SelectionPolicy::default());
        let _ = std::fs::remove_dir_all(&tmp);

        // 期望分数是手算的：Phish 只有 triggers 命中 1 次 → 1 × 2.0；
        // Desc 的 description 命中 1 次 → 1 × 2.5。
        assert_eq!(
            ranked.len(),
            2,
            "the trigger hit and the description hit must both match: {:?}",
            ranked
        );
        assert!(
            ranked.iter().any(|r| r.name == "netcap" && r.score == 2.0),
            "triggers must contribute their own weight: {:?}",
            ranked
        );
        assert!(
            ranked.iter().any(|r| r.name == "mailer" && r.score == 2.5),
            "description scoring must not regress: {:?}",
            ranked
        );
    }

    #[test]
    fn catalog_report_carries_warn_findings_for_registered_skills() {
        // Warn 级必须"照注册 + 说出来"。Quiet 少了 description、Good 干净，
        // 所以恰好一条告警 —— 数量断言同时是"没误报"的正对照。
        let tmp = std::env::temp_dir().join(format!("rs_skill_warns_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Quiet")).unwrap();
        std::fs::create_dir_all(tmp.join("Good")).unwrap();
        std::fs::write(tmp.join("Quiet/SKILL.md"), "---\nname: Quiet\n---\n# Q\n").unwrap();
        std::fs::write(tmp.join("Good/SKILL.md"), "---\nname: Good\ndescription: g\n---\n# G\n").unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let report = catalog_report(&mgr);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(report["count"], 1 + 1, "both skills register despite the warning: {}", report);
        let warnings = report["warnings"].as_array().expect("warnings is an array");
        assert_eq!(warnings.len(), 1, "only the name-only skill warns: {}", report);
        assert!(
            warnings[0]["findings"].to_string().contains("description"),
            "the warning must name the missing key: {}",
            warnings[0]
        );
    }

    #[test]
    fn unreadable_source_manifest_is_reported_not_swallowed() {
        // 清单读失败如果退成"没有清单"，一个被改过的技能就会伪装成 local。
        let tmp = std::env::temp_dir().join(format!("rs_skill_badmanifest_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = SkillManager::new(tmp.to_str().unwrap());
        mgr.create_skill_with_files("Manifested", "d", "# Body\n", None)
            .expect("skill created");
        std::fs::write(tmp.join("Manifested").join(schema::MANIFEST_FILE), "{ broken")
            .expect("manifest clobbered");
        mgr.reload();

        let report = catalog_report(&mgr);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(report["count"], 1, "the skill itself still registers: {}", report);
        let issues = report["manifest_issues"].as_array().expect("manifest_issues is an array");
        assert_eq!(issues.len(), 1, "one unreadable manifest must surface: {}", report);
        assert!(
            issues[0].to_string().contains(".foxir-source.json"),
            "the report must point at the file: {}",
            issues[0]
        );
    }

    #[test]
    fn created_skill_records_a_manifest_hashing_the_written_bytes() {
        // P3 的内容绑定读这份清单；哈希算错一位（只算正文、或换行被归一化）就会
        // 让"文件一改即撤权"失效。期望值用 sha2 直接算，走的是与 skill_md_hash
        // 不同的调用路径，所以"算错了字节范围"这种变异会被抓到。
        use sha2::{Digest, Sha256};
        let tmp = std::env::temp_dir().join(format!("rs_skill_manifest_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = SkillManager::new(tmp.to_str().unwrap());
        mgr.create_skill_with_files("Manifested", "d", "# Body\n", None)
            .expect("skill created");

        let dir = tmp.join("Manifested");
        let raw = std::fs::read(dir.join("SKILL.md")).expect("SKILL.md written");
        let manifest = schema::read_manifest(&dir)
            .expect("manifest read must not error")
            .expect("a created skill must carry a source manifest");
        let mut hasher = Sha256::new();
        hasher.update(&raw);
        let expected: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(
            manifest.skill_md_hash.as_deref(),
            Some(expected.as_str()),
            "the recorded hash must cover the whole file as written"
        );
        assert_eq!(manifest.source, schema::Source::Local);
        assert!(manifest.installed_at.is_some(), "install time is known at creation");
    }

    #[test]
    fn catalog_report_exposes_registered_rejected_and_duplicated() {
        // /api/skills 必须把"被拒"与"落选"说出来，否则用户只看到技能不见了。
        let tmp = std::env::temp_dir().join(format!("rs_skill_report_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Good")).unwrap();
        std::fs::write(tmp.join("Good/SKILL.md"), "---\nname: Good\ndescription: g\n---\n# G\n").unwrap();
        std::fs::create_dir_all(tmp.join("NoName")).unwrap();
        std::fs::write(tmp.join("NoName/SKILL.md"), "---\ndescription: none\n---\n# N\n").unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let report = catalog_report(&mgr);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(report["count"], 1, "registered skills: {}", report);
        let rejected = report["rejected"].as_array().expect("rejected is an array");
        assert_eq!(rejected.len(), 1, "{:}", report);
        assert!(
            rejected[0]["findings"].to_string().contains("name"),
            "the finding must say which key is missing: {}",
            rejected[0]
        );
        assert_eq!(
            report["duplicates"].as_array().expect("duplicates is an array").len(),
            0,
            "no collision here, so nothing may be reported: {}",
            report
        );
    }

    #[test]
    fn folded_name_collision_keeps_shallowest_and_reports_the_loser() {
        // insert_skill_unique 早就把这类冲突决定性地解掉了（最浅目录胜），所以它是
        // "Warn + 上报落选者"，不是规格里我先前裁的"双方全拒"—— 后者会在升级时
        // 凭空删掉今天能正常工作的技能。
        let tmp = std::env::temp_dir().join(format!("rs_skill_dup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Dup/nested")).unwrap();
        std::fs::write(tmp.join("Dup/SKILL.md"), "---\nname: dup\ndescription: shallow\n---\n# S\n").unwrap();
        std::fs::write(
            tmp.join("Dup/nested/SKILL.md"),
            "---\nname: DUP\ndescription: deeper\n---\n# D\n",
        )
        .unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let loaded = mgr.list();
        let reports = mgr.duplicate_claims();
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(loaded.len(), 1, "shallowest copy must win: {:?}", loaded.iter().map(|m| m.name.clone()).collect::<Vec<_>>());
        assert_eq!(loaded[0].name, "dup");
        assert_eq!(reports.len(), 1, "the dropped copy must be reported: {:?}", reports);
        assert!(
            matches!(&reports[0], crate::skill::schema::Finding::DuplicateNameFolded { name, dropped, .. }
                if name == "dup" && dropped.contains("nested")),
            "expected a DuplicateNameFolded naming the dropped dir, got {:?}",
            reports[0]
        );
    }

    #[test]
    fn distinct_skill_names_produce_no_duplicate_report() {
        // 正对照：上面那条断"报了一条"，这条得断"没冲突时不报"。
        let tmp = std::env::temp_dir().join(format!("rs_skill_nodup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Alpha")).unwrap();
        std::fs::create_dir_all(tmp.join("Beta")).unwrap();
        std::fs::write(tmp.join("Alpha/SKILL.md"), "---\nname: alpha\ndescription: a\n---\n# A\n").unwrap();
        std::fs::write(tmp.join("Beta/SKILL.md"), "---\nname: beta\ndescription: b\n---\n# B\n").unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let reports = mgr.duplicate_claims();
        let count = mgr.list().len();
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(count, 2, "both distinct skills register");
        assert!(reports.is_empty(), "no collision, no report: {:?}", reports);
    }

    #[test]
    fn recording_a_grant_needs_content_binding_and_is_what_makes_a_ledger_live() {
        // 同意 → 台账生效；拒绝 → 台账空。这一条同时钉住两件事：
        // 没有清单绑定就不接受同意（§1.3），以及"同意是激活的唯一入口"。
        let tmp = std::env::temp_dir().join(format!("rs_grant_record_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let md = "---\nname: MyTool\ndescription: d\nallowed-tools: sys_process\n---\n# body\n";
        std::fs::create_dir_all(tmp.join("MyTool")).unwrap();
        std::fs::write(tmp.join("MyTool/SKILL.md"), md).unwrap();
        let mgr = SkillManager::new(tmp.to_str().unwrap());

        // (1) 文件还没被清单绑住 → 不接受同意
        let err = mgr
            .record_skill_grant("MyTool", true)
            .expect_err("an unbound skill cannot be consented");
        assert!(err.contains("bind") || err.contains("manifest"), "{err}");

        // (2) 绑定之后同意 → 台账立刻有了一条，且记录的是全长哈希
        schema::write_manifest(
            &tmp.join("MyTool"),
            &schema::SourceManifest::local(md.as_bytes()),
        )
        .unwrap();
        mgr.reload();
        mgr.record_skill_grant("MyTool", true).expect("consent accepted");
        let hash = schema::skill_md_hash(md.as_bytes());
        let persisted = schema::SkillStateStore::load(&tmp.join("skills_state.json"));
        assert_eq!(
            persisted
                .state_of("MyTool")
                .expect("entry")
                .grants
                .consented_hash
                .as_deref(),
            Some(hash.as_str()),
            "the approval must be bound to the full hash"
        );
        assert!(
            !mgr.grant_ledger(&["MyTool".to_string()], "sess-r").is_empty(),
            "a consented, bound skill must be able to authorise"
        );

        // (3) 撤回同意 → 台账变空
        mgr.record_skill_grant("MyTool", false).expect("refusal accepted");
        assert!(
            mgr.grant_ledger(&["MyTool".to_string()], "sess-r").is_empty(),
            "a declined skill grants nothing"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn grant_ledger_needs_consent_content_binding_and_no_drift() {
        // 三条路径一起钉：没有同意 → 不放行；有同意但清单缺失（内容没绑住）→ 不放行；
        // 清单在、同意在、但文件被改过（哈希漂了）→ 撤权。
        let tmp = std::env::temp_dir().join(format!("rs_grants_ledger_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let md = "---\nname: MyTool\ndescription: d\nallowed-tools: sys_process\n---\n# body\n";
        let hash = schema::skill_md_hash(md.as_bytes());

        std::fs::create_dir_all(tmp.join("MyTool")).unwrap();
        std::fs::write(tmp.join("MyTool/SKILL.md"), md).unwrap();

        // (1) 只有同意在册才可能放行 —— 这里还没写任何状态
        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let ledger = mgr.grant_ledger(&["MyTool".to_string()], "sess-1");
        assert!(ledger.is_empty(), "no consent on record yet: {:?}", ledger.is_empty());

        // (3) 同意在册 + 清单缺失 = 内容没绑住，仍然不放行
        std::fs::write(
            tmp.join("skills_state.json"),
            format!("{{\"MyTool\":{{\"enabled\":true,\"grants\":{{\"consented_hash\":\"{hash}\",\"tools\":[\"sys_process\"]}}}}}}"),
        )
        .unwrap();
        mgr.reload();
        let ledger = mgr.grant_ledger(&["MyTool".to_string()], "sess-1");
        assert!(ledger.is_empty(), "an unbound file grants nothing");

        // 补上清单（内容绑住）→ 放行
        schema::write_manifest(
            &tmp.join("MyTool"),
            &schema::SourceManifest::local(md.as_bytes()),
        )
        .expect("manifest written");
        mgr.reload();
        let ledger = mgr.grant_ledger(&["MyTool".to_string()], "sess-1");
        assert!(
            !matches!(
                ledger.authorize_with_audit(
                    "sys_process",
                    &json!({"action": "kill", "name": "evil.exe"}),
                    "终止进程",
                ),
                crate::skill::grants::AuditOutcome::NoGrant
            ),
            "a consented, bound skill must be able to authorise the narrowed call"
        );

        // 内容一改，同一份同意立刻不生效（哈希漂了）
        std::fs::write(tmp.join("MyTool/SKILL.md"), format!("{md}\n# appended by someone else\n")).unwrap();
        let after_edit = mgr.grant_ledger(&["MyTool".to_string()], "sess-1");
        assert!(
            matches!(
                after_edit.authorize_with_audit(
                    "sys_process",
                    &json!({"action": "kill", "name": "evil.exe"}),
                    "终止进程",
                ),
                crate::skill::grants::AuditOutcome::NoGrant
            ),
            "an edited SKILL.md must not keep the old approval"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn installing_a_folder_stages_outside_the_skills_glob_and_registers() {
        // 落位前必须先能被 reload() 扫到？不能 —— 隔离区在 skills/ 之外。
        // 同时钉住：装完立刻可被发现，以及回收站里有同名时不许静默复活。
        let root = std::env::temp_dir().join(format!("rs_install_mgr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let skills = root.join("skills");
        let source = root.join("source/MySkill");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: MySkill\ndescription: installed from a folder\n---\n# M\n",
        )
        .unwrap();
        std::fs::write(source.join("reference.md"), "ref").unwrap();

        let mgr = SkillManager::new(skills.to_str().unwrap());
        let outcome = mgr
            .install_from_folder(&source, None)
            .expect("folder install accepted");
        assert_eq!(outcome.dir, skills.join("MySkill"));
        assert!(skills.join("MySkill/reference.md").is_file());
        let names: Vec<String> = mgr.list().into_iter().map(|m| m.name).collect();
        assert!(
            names.contains(&"MySkill".to_string()),
            "an installed skill must be discoverable: {names:?}"
        );
        assert!(
            !root.join("skills/.skill-quarantine").exists(),
            "quarantine must never live inside the scanned tree"
        );
        assert!(!source.exists() || source.join("SKILL.md").is_file(), "the user's source tree stays put");

        // 删掉后再装：回收站里有同名 → 明确拒绝，不静默复活
        mgr.delete_skill("MySkill").expect("moved to recycle bin");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "---\nname: MySkill\ndescription: d\n---\n# M\n").unwrap();
        let err = mgr
            .install_from_folder(&source, None)
            .expect_err("a trashed name must not be silently resurrected");
        assert!(err.contains("_deleted"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// §4.0 的边界：安装入口只能是用户发起的 HTTP 端点。模型能调到任何一个，
    /// 就等于"它读到的一段文本"可以让 FoxIR 去拉包。
    #[test]
    fn install_entry_points_are_not_model_tools() {
        let names = SkillManager::skill_tool_names();
        for forbidden in ["install_from_url", "install_from_path", "install_from_folder"] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "{forbidden} must not be model-callable: {names:?}"
            );
        }
    }

    #[test]
    fn toggling_one_skill_does_not_re_enable_the_others() {
        // 特征化测试（重构前必须先立住）：状态存储从 HashMap<String,bool> 换成结构体时，
        // "写一条不能把别条刷回默认值"是用户数据的底线 —— Other 故意写成 false，
        // 这样一旦读侧静默退成空表，它就会变成默认的 enabled=true 而被这条抓到。
        let tmp = std::env::temp_dir().join(format!("rs_state_keep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Good")).unwrap();
        std::fs::write(tmp.join("Good/SKILL.md"), "---\nname: Good\ndescription: g\n---\n# G\n").unwrap();
        std::fs::create_dir_all(tmp.join("Other")).unwrap();
        std::fs::write(tmp.join("Other/SKILL.md"), "---\nname: Other\ndescription: o\n---\n# O\n").unwrap();
        std::fs::write(tmp.join("skills_state.json"), "{\"Good\": false, \"Other\": false}").unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        assert!(!mgr.list().iter().any(|m| m.enabled), "both skills start disabled");
        mgr.toggle_skill("Good");
        drop(mgr);

        let mgr2 = SkillManager::new(tmp.to_str().unwrap());
        let got: Vec<(String, bool)> = mgr2.list().iter().map(|m| (m.name.clone(), m.enabled)).collect();
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(got.contains(&("Good".to_string(), true)), "the toggled skill must be enabled: {:?}", got);
        assert!(got.contains(&("Other".to_string(), false)), "writing one entry must not re-enable others: {:?}", got);
    }

    #[test]
    fn reload_registers_named_skill_and_reports_the_nameless_one() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_reject_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Good")).unwrap();
        std::fs::write(
            tmp.join("Good/SKILL.md"),
            "---\nname: Good\ndescription: fine\n---\n# Good\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.join("NoName")).unwrap();
        std::fs::write(
            tmp.join("NoName/SKILL.md"),
            "---\ndescription: no name at all\n---\n# Orphan\n",
        )
        .unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let loaded: Vec<String> = mgr.list().iter().map(|m| m.name.clone()).collect();
        let rejected = mgr.rejected();
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(loaded, vec!["Good".to_string()], "only a named skill registers");
        assert_eq!(rejected.len(), 1, "the blocked skill must be reported: {:?}", rejected);
        assert!(
            matches!(&rejected[0].1[..], [crate::skill::schema::Finding::MissingField { key: "name" }]),
            "expected a missing-name blocking finding, got {:?}",
            rejected[0].1
        );
    }

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
                compatibility: None,
                triggers: vec![],
                metadata: Default::default(),
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

        let (q_opt, q_act) = mgr.build_skills_prompt(&[], SkillListingStrategy::Query, 40);
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

        let (n_opt, _) = mgr.build_skills_prompt(&[], SkillListingStrategy::NamesOnly, 40);
        let n = n_opt.expect("names section present");
        assert!(n.contains("AlwaysSkill"));
        assert!(!n.contains("# always body"), "names-only must not inline bodies: {}", n);

        assert!(mgr.build_skills_prompt(&[], SkillListingStrategy::DiscoverToolOnly, 0).0.is_none());

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
        let (p, active) = mgr.build_skills_prompt(&[], SkillListingStrategy::Disabled, 40);
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

    /// Name-keyed management is case-insensitive, matching the dedup in
    /// `insert_skill_unique` — a delete/toggle by different case hits the
    /// canonical skill (agentskills.io: `name` is the uniqueness key).
    #[test]
    fn delete_and_toggle_are_case_insensitive() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_case_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("MySkill")).unwrap();
        std::fs::write(tmp.join("MySkill/SKILL.md"),
            "---\nname: MySkill\ndescription: d\n---\n# B\n").unwrap();
        let mgr = SkillManager::new(tmp.to_str().unwrap());
        assert!(mgr.list().iter().any(|m| m.name.eq_ignore_ascii_case("myskill")), "skill should load");
        assert!(mgr.toggle_skill("myskill").is_some(), "toggle should match case-insensitively");
        assert!(mgr.delete_skill("myskill").is_ok(), "delete should match case-insensitively");
        assert!(mgr.list().is_empty(), "skill should be gone after delete: {:?}", mgr.list());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The installed skill-creator port must parse as a valid agentskills.io
    /// Skill — required name/description plus optional version/platforms.
    #[test]
    fn load_agentskills_skill_creator_frontmatter() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_creator_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("skill-creator")).unwrap();
        std::fs::write(tmp.join("skill-creator/SKILL.md"),
            "---\nname: skill-creator\ndescription: \"Create, modify and improve agent skills and measure their performance.\"\nversion: \"1.0.0\"\nplatforms: [windows, macos, linux]\n---\n\n# Skill Creator\n").unwrap();
        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let list = mgr.list();
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(list.len(), 1, "skill should load: {:?}", list);
        let m = &list[0];
        assert_eq!(m.name, "skill-creator");
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.platforms, vec!["windows".to_string(), "macos".to_string(), "linux".to_string()]);
        assert!(m.enabled, "newly installed skill should default to enabled");
    }

    /// S1 — a skill simulated as created through user dialogue via
    /// `SkillManager::create_skill` must be written to disk as a valid
    /// agentskills.io SKILL.md (name/description only, no triggers, no
    /// `x-foxir` private extensions), be re-discovered on reload, and load its
    /// body lazily with frontmatter stripped. Adherence score is the fraction
    /// of standard-compliance checks that pass.
    #[test]
    fn s1_user_created_skill_follows_agentskills_io_standard() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_s1_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let body = "# Survey\n\n1. Collect account list\n2. Collect network list\n3. Write summary\n";
        let created_dir = mgr
            .create_skill("UserSurveySkill", "A quick host survey skill", body)
            .expect("create_skill should succeed");
        let _ = created_dir;

        let mut passed = 0usize;
        let mut total = 0usize;

        // 1) On-disk frontmatter is valid agentskills.io: no triggers, no x-foxir.
        let raw = std::fs::read_to_string(tmp.join("UserSurveySkill/SKILL.md")).unwrap();
        total += 1;
        passed += (raw.starts_with("---\n") && raw.contains("name:") && raw.contains("description:")) as usize;
        total += 1;
        passed += (!raw.contains("triggers") && !raw.contains("x-foxir")) as usize;
        // 2) Does not inline the body or private fields.
        total += 1;
        passed += (!raw.contains("platforms:") && !raw.contains("deps:")) as usize;

        // 3) Discoverable after reload via find_skill (case-insensitive).
        total += 1;
        let sk = mgr.find_skill("usersurveyskill").expect("skill should be discoverable");
        passed += 1;

        // 4) Lazy body loads and is frontmatter-stripped.
        total += 1;
        let body_loaded = sk.body().into_owned();
        passed += (body_loaded.to_lowercase().contains("# survey")
            && !body_loaded.contains("name:")) as usize;

        // 5) Step contract compiles from the body for later verification.
        total += 1;
        passed += (sk.step_contract().len() == 3) as usize;

        let score = passed as f64 / total as f64;
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(score, 1.0, "S1 agentskills.io compliance score = {score} (passed {passed}/{total})");
    }

    /// S2 — an `instruction (step1, step2)` styled skill (numbered-list body)
    /// yields a step contract, and full evidence produces a completion ratio of
    /// 1.0 while partial evidence scores below 1.0.
    #[test]
    fn s2_stepwise_instruction_skill_completion_score() {
        // Build the skill on disk exactly as the numbered-list translator would.
        let tmp = std::env::temp_dir().join(format!("rs_skill_s2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("PatchSkill")).unwrap();
        std::fs::write(tmp.join("PatchSkill/SKILL.md"),
            "---\nname: PatchSkill\ndescription: patch windows per instructions\n---\n\n# Patch\n\n1. Check prerequisites\n2. Apply the patch\n3. Reboot and verify\n").unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let sk = mgr.find_skill("PatchSkill").expect("skill found");
        let contract = sk.step_contract();
        assert_eq!(contract.len(), 3, "expected 3 steps from numbered list: {:?}", contract);

        // Full adherence: evidence covers every step -> ratio 1.0.
        let full = "Check prerequisites done. Apply the patch succeeded. Reboot and verify ok.";
        let r_full = verify::verify_completion(&contract, full);
        assert_eq!(r_full.ratio, 1.0, "full adherence should score 1.0, got {}", r_full.ratio);
        assert!(r_full.is_complete());

        // Partial adherence: only step 1 echoed -> ratio 1/3.
        let partial = "I only did Check prerequisites and stopped there.";
        let r_partial = verify::verify_completion(&contract, partial);
        assert_eq!(r_partial.ratio, 1.0 / 3.0, "partial adherence should score 1/3, got {}", r_partial.ratio);
        assert!(!r_partial.is_complete());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// S3 — a parent skill whose step flow invokes a child skill (nested
    /// discovery), and whose parent evidence includes the child's completion
    /// marker. Both must be discovered and the parent contract must verify to
    /// ratio 1.0 with combined evidence.
    #[test]
    fn s3_parent_child_skill_flow_completion_score() {
        let tmp = std::env::temp_dir().join(format!("rs_skill_s3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // Parent skill dir containing a nested Child skill dir.
        std::fs::create_dir_all(tmp.join("IncidentTriage/Collector")).unwrap();
        std::fs::write(tmp.join("IncidentTriage/SKILL.md"),
            "---\nname: IncidentTriage\ndescription: full triage driving a child collector\n---\n\n# Incident Triage\n\n1. Run child Collector skill\n2. Analyze findings\n3. Draft containment steps\n").unwrap();
        std::fs::write(tmp.join("IncidentTriage/Collector/SKILL.md"),
            "---\nname: Collector\ndescription: collect artifacts\n---\n\n# Collector\n\n1. Snapshot processes\n2. Capture network state\n").unwrap();

        let mgr = SkillManager::new(tmp.to_str().unwrap());
        let names: Vec<String> = mgr.list().iter().map(|m| m.name.clone()).collect();
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(names.contains(&"IncidentTriage".to_string()), "parent missing: {:?}", names);
        assert!(names.contains(&"Collector".to_string()), "nested child missing: {:?}", names);
    }

    /// S3b — numeric adherence for the parent/child flow: parent steps 1..3 all
    /// evidenced (step 1 evidence = "ran Collector / step 1 done") => ratio 1.0.
    #[test]
    fn s3_parent_contract_verifies_with_child_evidence() {
        let contract = steps::extract_contract(
            "# Incident Triage\n\n1. Run child Collector skill\n2. Analyze findings\n3. Draft containment steps\n",
        );
        assert_eq!(contract.len(), 3);
        let evidence = "step 1 done: invoked Collector. step 2 done: analyzed. step 3 done: drafted containment.";
        let r = verify::verify_completion(&contract, evidence);
        assert_eq!(r.ratio, 1.0, "parent+child flow should score 1.0, got {}", r.ratio);
        // And without the child-evidence marker the parent step 1 is missing.
        let weak = "analyzed findings and drafted containment.";
        let r2 = verify::verify_completion(&contract, weak);
        assert!(r2.ratio < 1.0, "missing child step should drop score, got {}", r2.ratio);
    }

    /// S4 — a methodology skill (no step/instruction structure) yields an empty
    /// contract, so `verify_completion` treats it as trivially adhered (ratio
    /// 1.0) and `contract_block` returns None (nothing to enforce).
    #[test]
    fn s4_methodology_only_skill_scores_full_adherence() {
        let body = "# Threat Intel Methodology\n\nPrioritize by reachability first, then by asset criticality.\nWeigh exploit maturity and ongoing campaign activity before assigning a patch window.\nAlways mark uncertain judgements explicitly.\n";
        let contract = steps::extract_contract(body);
        assert!(contract.is_empty(), "methodology-only skill should have no step contract: {:?}", contract);
        assert!(steps::contract_block(&contract).is_none(), "nothing to enforce -> no contract block");

        // No contract => nothing missing => ratio 1.0 (adherence by methodology use).
        let r = verify::verify_completion(&contract, "applied the methodology");
        assert_eq!(r.ratio, 1.0, "methodology-only adherence score = {}", r.ratio);
        assert!(r.is_complete());
    }
    #[test]
    fn resolve_inside_rejects_escape_and_allows_subdir() {
        let base = std::env::temp_dir().join(format!("rs_skill_inside_{}", std::process::id()));
        std::fs::create_dir_all(base.join("sub")).unwrap();
        let ok = resolve_inside(&base, "sub/ref.md").unwrap();
        assert!(ok.starts_with(&base.canonicalize().unwrap()), "subdir must resolve inside base");
        assert!(resolve_inside(&base, "../evil.md").is_err(), "parent escape must be rejected");
        assert!(resolve_inside(&base, "a/../../evil.md").is_err(), "nested parent escape rejected");
        assert!(resolve_inside(&base, "/abs.md").is_err(), "absolute path rejected");
        assert!(resolve_inside(&base, "").is_err(), "empty path rejected");
        let _ = std::fs::remove_dir_all(&base.parent().unwrap());
    }

    #[test]
    fn bump_patch_increments_patch_component() {
        assert_eq!(bump_patch("1.0.0"), "1.0.1");
        assert_eq!(bump_patch("2.4.9"), "2.4.10");
        assert_eq!(bump_patch("1.2"), "1.2.0.1");
        assert_eq!(bump_patch("not-a-version"), "not-a-version.0.1");
    }
}
