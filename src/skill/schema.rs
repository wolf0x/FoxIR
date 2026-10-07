//! P1 — frontmatter 字段模型、校验分级与来源记录的单一入口。
//!
//! 命名立法（规格 §0）：frontmatter 键名一律规范原词、连字符形式（`allowed-tools`）；
//! Rust 侧字段名用下划线（`allowed_tools`），靠 serde 映射。文档/报错/UI 提到 frontmatter
//! 时只允许出现 `allowed-tools`。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::types::SkillMetadata;

/// Where a skill came from. Legacy state files have no such field, so the
/// default is `Local` (spec §1.1) — never "unknown".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Local,
    Url,
    Path,
}

impl Default for Source {
    fn default() -> Self {
        Source::Local
    }
}

fn default_true() -> bool {
    true
}

/// What the user decided about one skill's declared `allowed-tools`.
/// Bound to the full `skill_md_hash`: new content is a new question.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GrantConsent {
    #[serde(default)]
    pub consented_hash: Option<String>,
    #[serde(default)]
    pub declined_hash: Option<String>,
    /// The tool names the decision covered, so the UI can show what was agreed.
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub granted_at: Option<i64>,
}

/// Per-skill runtime state, persisted in `skills_state.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillState {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub package_sha256: Option<String>,
    #[serde(default)]
    pub skill_md_hash: Option<String>,
    #[serde(default)]
    pub tree_hash: Option<String>,
    /// Filled on the first write-back, not on read.
    #[serde(default)]
    pub installed_at: Option<i64>,
    /// P1 has no writer for this; P3 authorization does.
    #[serde(default)]
    pub reviewed_at: Option<i64>,
    /// The user's decision about this skill's declared `allowed-tools`.
    #[serde(default)]
    pub grants: GrantConsent,
}

/// The `skills_state.json` store, read in two stages so a schema change cannot
/// silently wipe the user's enable/disable flags.
pub struct SkillStateStore {
    path: PathBuf,
    skills: HashMap<String, SkillState>,
    notes: Vec<String>,
    /// True when neither format parsed: the file must then be left untouched.
    unreadable: bool,
}

impl SkillStateStore {
    /// Read the store. Never writes.
    pub fn load(path: &Path) -> Self {
        let mut store = Self {
            path: path.to_path_buf(),
            skills: HashMap::new(),
            notes: Vec::new(),
            unreadable: false,
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(_) => return store, // absent/unreadable-as-bytes: empty defaults, nothing to upgrade
        };
        if let Ok(modern) = serde_json::from_str::<HashMap<String, SkillState>>(&raw) {
            store.skills = modern;
            return store;
        }
        if let Ok(legacy) = serde_json::from_str::<HashMap<String, bool>>(&raw) {
            store.notes.push(format!(
                "upgraded legacy skills_state.json ({} entries) at {}",
                legacy.len(),
                path.display()
            ));
            store.skills = legacy
                .into_iter()
                .map(|(name, enabled)| {
                    (
                        name,
                        SkillState {
                            enabled,
                            ..Default::default()
                        },
                    )
                })
                .collect();
            return store;
        }
        store.notes.push(format!(
            "skills_state.json at {} matches neither the new nor the legacy shape; it will not be rewritten",
            path.display()
        ));
        store.unreadable = true;
        store
    }

    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Full persisted state for one skill.
    pub fn state_of(&self, name: &str) -> Option<&SkillState> {
        self.skills.get(name)
    }

    /// Record the user's decision about a skill's declared `allowed-tools`.
    /// Approval and refusal are both bound to the full `skill_md_hash`, and a
    /// refusal clears any earlier approval (new content is a new question).
    pub fn record_consent(
        &mut self,
        name: &str,
        skill_md_hash: &str,
        tools: &[String],
        approved: bool,
    ) -> Result<(), String> {
        self.guard_writable()?;
        let entry = self.skills.entry(name.to_string()).or_default();
        entry.grants.tools = tools.to_vec();
        if approved {
            entry.grants.consented_hash = Some(skill_md_hash.to_string());
            entry.grants.declined_hash = None;
            entry.grants.granted_at = Some(now_unix());
        } else {
            entry.grants.declined_hash = Some(skill_md_hash.to_string());
            entry.grants.consented_hash = None;
        }
        if entry.installed_at.is_none() {
            entry.installed_at = Some(now_unix());
        }
        self.save()
    }

    fn guard_writable(&self) -> Result<(), String> {
        if self.unreadable {
            return Err(format!(
                "refusing to write {}: it matches neither the new nor the legacy shape",
                self.path.display()
            ));
        }
        Ok(())
    }

    /// Toggle a skill and persist. Refuses to write when the file matched neither
    /// known shape, so a corrupt file is never overwritten by an empty structure.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), String> {
        self.guard_writable()?;
        let entry = self.skills.entry(name.to_string()).or_default();
        entry.enabled = enabled;
        if entry.installed_at.is_none() {
            entry.installed_at = Some(now_unix());
        }
        self.save()
    }

    /// Drop a skill's persisted entry (used when the skill directory is removed).
    pub fn remove(&mut self, name: &str) -> Result<(), String> {
        self.guard_writable()?;
        self.skills.remove(name);
        self.save()
    }

    /// The persisted enabled flag, if any.
    pub fn enabled_flag(&self, name: &str) -> Option<bool> {
        self.state_of(name).map(|state| state.enabled)
    }

    /// Atomic write of the current map; refuses when the last read was unreadable.
    pub fn save(&self) -> Result<(), String> {
        if self.unreadable {
            return Err(format!(
                "refusing to write {}: it matches neither the new nor the legacy shape",
                self.path.display()
            ));
        }
        write_json_atomic(&self.path, &self.skills)
    }
}

/// Provenance sidecar written next to a skill's `SKILL.md`.
pub const MANIFEST_FILE: &str = ".foxir-source.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceManifest {
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub url: Option<String>,
    /// Hash of the downloaded package. `None` for a single-file install, which
    /// has no package to hash.
    #[serde(default)]
    pub package_sha256: Option<String>,
    #[serde(default)]
    pub skill_md_hash: Option<String>,
    /// Reserved for resource-tree integrity (later phase); never guessed here.
    #[serde(default)]
    pub tree_hash: Option<String>,
    #[serde(default)]
    pub installed_at: Option<i64>,
}

impl SourceManifest {
    /// Manifest for a skill FoxIR wrote itself: local provenance, hash over the
    /// exact bytes just written, and a known install time.
    pub fn local(skill_md_bytes: &[u8]) -> Self {
        Self {
            source: Source::Local,
            skill_md_hash: Some(skill_md_hash(skill_md_bytes)),
            installed_at: Some(now_unix()),
            ..Default::default()
        }
    }
}

pub fn write_manifest(skill_dir: &Path, manifest: &SourceManifest) -> Result<(), String> {
    write_json_atomic(&skill_dir.join(MANIFEST_FILE), manifest)
}

/// `Ok(None)` means no manifest exists (a skill written before P1). An `Err`
/// means a manifest is there but unreadable — callers must surface it rather
/// than fall back to defaults, or a tampered file would look like a local skill.
pub fn read_manifest(skill_dir: &Path) -> Result<Option<SourceManifest>, String> {
    let path = skill_dir.join(MANIFEST_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("parse {}: {}", path.display(), e))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Serialize to a sibling `*.tmp` then rename, so a crash mid-write cannot leave
/// a half-written state file (which the old `fs::write` could).
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "skills_state.json".to_string());
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    let text = serde_json::to_string_pretty(value).map_err(|e| format!("serialize failed: {e}"))?;
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename onto {}: {}", path.display(), e))
}

/// SHA-256 as lowercase hex, over the exact bytes given (no normalization).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// SHA-256 of a whole SKILL.md, over its raw bytes — frontmatter and body, with
/// no newline normalization. Resource files are deliberately out of scope here
/// (that is what `tree_hash` is reserved for).
pub fn skill_md_hash(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

/// A validation outcome that carries its own fields, so a report can be acted on
/// rather than merely read. Blocking findings prevent registration; warn-level
/// findings are returned alongside a registered skill.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum Finding {
    /// A required key is absent, empty, or not a string.
    MissingField { key: &'static str },
    /// The YAML block cannot be parsed at all, so nothing can be loaded safely.
    InvalidYaml { error: String },
    /// Warn-level: `name` differs from the directory it lives in. FoxIR's own
    /// writers produce this shape, so it must never block registration.
    NameNotMatchingDir { name: String, dir: String },
    /// Warn-level: two skill directories claim the same case-folded name.
    /// `insert_skill_unique` already resolves this deterministically (shallowest
    /// directory wins), so the loser is reported rather than both being dropped.
    DuplicateNameFolded {
        name: String,
        kept: String,
        dropped: String,
    },
    /// Warn-level: a field exceeds the standard's limit. Nothing is rewritten —
    /// silently truncating would break the content binding that later phases rely on.
    TooLong { field: String, len: usize, limit: usize },
    /// Warn-level: a top-level key FoxIR does not know. Kept separate from
    /// `UnknownMetadataKey` because the risk levels differ.
    UnknownTopLevelKey { key: String },
    /// Warn-level: a legacy top-level `x-foxir.*` alias. Accepted, but the report
    /// says where it belongs now.
    DeprecatedTopLevel {
        key: String,
        value_preview: String,
        migrate_to: String,
    },
    /// P3, warn-level: a declared `allowed-tools` entry outside the grantable
    /// vocabulary. The skill still registers; the entry grants nothing.
    GrantUnsupported { tool: String, reason: &'static str },
    /// P3, warn-level: another host's grant syntax (e.g. `Bash(git commit)`).
    /// Not ours to interpret, so it is reported rather than guessed at.
    UnsupportedGrantSyntax { raw: String },
}

/// The six frontmatter keys the agentskills.io standard defines.
const STANDARD_KEYS: [&str; 6] = [
    "name",
    "description",
    "license",
    "compatibility",
    "metadata",
    "allowed-tools",
];

/// FoxIR's own registered top-level fields (a documented deviation from the
/// standard — see the spec's field-alignment table).
const FOXIR_KEYS: [&str; 4] = ["version", "platforms", "deps", "triggers"];

fn value_preview(value: &serde_yaml::Value) -> String {
    let rendered = serde_yaml::to_string(value).unwrap_or_default();
    rendered.trim().chars().take(80).collect()
}

fn classify_unknown_keys(mapping: &serde_yaml::Mapping, warnings: &mut Vec<Finding>) {
    for (key, value) in mapping.iter() {
        let Some(key) = key.as_str() else { continue };
        if let Some(suffix) = key.strip_prefix("x-foxir.") {
            warnings.push(Finding::DeprecatedTopLevel {
                migrate_to: format!("metadata.foxir.{suffix}"),
                key: key.to_string(),
                value_preview: value_preview(value),
            });
            continue;
        }
        if STANDARD_KEYS.contains(&key) || FOXIR_KEYS.contains(&key) {
            continue;
        }
        warnings.push(Finding::UnknownTopLevelKey { key: key.to_string() });
    }
}

/// The standard's frontmatter limits, in characters.
const NAME_LIMIT: usize = 64;
const DESCRIPTION_LIMIT: usize = 1024;
const COMPATIBILITY_LIMIT: usize = 500;

fn warn_if_too_long(warnings: &mut Vec<Finding>, field: &str, text: &str, limit: usize) {
    let len = text.chars().count();
    if len > limit {
        warnings.push(Finding::TooLong {
            field: field.to_string(),
            len,
            limit,
        });
    }
}

/// A parsed + validated skill document.
#[derive(Debug)]
pub struct SkillDoc {
    pub metadata: SkillMetadata,
    /// Warn-level findings: the skill is registered, and these are reported.
    pub warnings: Vec<Finding>,
}

/// A list-valued frontmatter key: YAML sequence, or (per the standard's
/// `allowed-tools` shape) a space-delimited string.
fn canonicalize_list(value: &serde_yaml::Value) -> Vec<String> {
    match value {
        serde_yaml::Value::String(raw) => raw.split_whitespace().map(str::to_string).collect(),
        serde_yaml::Value::Sequence(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse a SKILL.md frontmatter block into metadata.
///
/// `Err` carries the blocking findings (the skill is not registered); a returned
/// `Ok` document is registered.
pub fn parse_frontmatter(frontmatter: &str, dir_name: &str) -> Result<SkillDoc, Vec<Finding>> {
    let value: serde_yaml::Value = serde_yaml::from_str(frontmatter).map_err(|e| {
        vec![Finding::InvalidYaml { error: e.to_string() }]
    })?;
    let mapping = match &value {
        serde_yaml::Value::Mapping(map) => map,
        _ => {
            return Err(vec![Finding::InvalidYaml {
                error: "frontmatter is not a mapping".to_string(),
            }])
        }
    };
    let get = |key: &str| mapping.get(&serde_yaml::Value::String(key.to_string()));
    let text = |key: &str| get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let name = text("name");
    let description = text("description");
    if name.trim().is_empty() {
        return Err(vec![Finding::MissingField { key: "name" }]);
    }
    let metadata_map: std::collections::BTreeMap<String, serde_yaml::Value> = match get("metadata") {
        Some(serde_yaml::Value::Mapping(map)) => map
            .iter()
            .filter_map(|(k, v)| k.as_str().map(|k| (k.to_string(), v.clone())))
            .collect(),
        _ => Default::default(),
    };
    // `triggers` has no standard key: prefer metadata.foxir.triggers, fall back to
    // the top-level alias that existing SKILL.md files already use.
    let triggers = match metadata_map.get("foxir.triggers") {
        Some(value) if !canonicalize_list(value).is_empty() => canonicalize_list(value),
        _ => get("triggers").map(canonicalize_list).unwrap_or_default(),
    };

    let mut warnings = Vec::new();
    if name.trim() != dir_name {
        warnings.push(Finding::NameNotMatchingDir { name: name.clone(), dir: dir_name.to_string() });
    }
    if description.trim().is_empty() {
        warnings.push(Finding::MissingField { key: "description" });
    }
    warn_if_too_long(&mut warnings, "name", &name, NAME_LIMIT);
    warn_if_too_long(&mut warnings, "description", &description, DESCRIPTION_LIMIT);
    if let Some(compatibility) = get("compatibility").and_then(|v| v.as_str()) {
        warn_if_too_long(&mut warnings, "compatibility", compatibility, COMPATIBILITY_LIMIT);
    }
    classify_unknown_keys(mapping, &mut warnings);
    // Which declared tools can actually be honoured is decided by the ceiling in
    // `grants`; asking at parse time is what makes an inert declaration visible.
    let allowed_tools = get("allowed-tools")
        .map(canonicalize_list)
        .unwrap_or_default();
    warnings.extend(super::grants::plan_grants(&allowed_tools).findings);

    Ok(SkillDoc {
        warnings,
        metadata: SkillMetadata {
            name,
            description,
            license: get("license").and_then(|v| v.as_str()).map(str::to_string),
            version: get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("1.0.0")
                .to_string(),
            platforms: get("platforms").map(canonicalize_list).unwrap_or_default(),
            deps: get("deps").map(canonicalize_list).unwrap_or_default(),
            allowed_tools,
            compatibility: get("compatibility").and_then(|v| v.as_str()).map(str::to_string),
            metadata: metadata_map,
            triggers,
            enabled: true,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::{
        parse_frontmatter, skill_md_hash, Finding,
        SkillStateStore, Source,
    };

    fn temp_state_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rs_skill_state_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn foreign_top_level_keys_are_classified_not_merged() {
        // 两类键的风险等级不同，所以必须分开报：`when:` 是别人的私有扩展（Unknown），
        // `x-foxir.*` 是我们自己的历史别名（Deprecated，给出去处），
        // 而 version/platforms/deps/triggers 是 FoxIR 已登记的顶层字段，不该算未知。
        let doc = parse_frontmatter(
            "name: foo\ndescription: d\nwhen: monday\nx-foxir.triggers: [a]\n",
            "foo",
        )
        .expect("unknown keys are warn-level only");
        assert!(
            doc.warnings
                .iter()
                .any(|f| matches!(f, Finding::UnknownTopLevelKey { key } if key == "when")),
            "expected UnknownTopLevelKey(when), got {:?}",
            doc.warnings
        );
        assert!(
            matches!(
                doc.warnings.iter().find(|f| matches!(f, Finding::DeprecatedTopLevel { .. })),
                Some(Finding::DeprecatedTopLevel { key, migrate_to, .. })
                    if key == "x-foxir.triggers" && migrate_to == "metadata.foxir.triggers"
            ),
            "expected a DeprecatedTopLevel pointing at metadata.foxir.triggers, got {:?}",
            doc.warnings
        );

        let known = parse_frontmatter(
            "name: foo\ndescription: d\nversion: \"1.2.3\"\nplatforms: [windows]\ndeps: [rg]\ntriggers: [x]\n",
            "foo",
        )
        .expect("FoxIR's own top-level fields are known");
        assert!(
            !known
                .warnings
                .iter()
                .any(|f| matches!(f, Finding::UnknownTopLevelKey { .. })),
            "FoxIR-registered keys must not be reported as unknown: {:?}",
            known.warnings
        );
        assert_eq!(known.metadata.version, "1.2.3");
    }

    #[test]
    fn oversized_fields_warn_with_their_limits() {
        // 标准的上限是 name ≤64、description ≤1024、compatibility ≤500。
        // 超限只报 Warn（不改数据），所以 action 必须是 AsIs —— 悄悄截断会让
        // 哈希与内容对不上，那是比超长更糟的失败。
        let long_name = "n".repeat(65);
        let doc = parse_frontmatter(&format!("name: {long_name}\ndescription: ok\n"), &long_name)
            .expect("an oversized name must still register");
        assert!(
            doc.warnings.iter().any(|f| matches!(
                f,
                Finding::TooLong { field, len: 65, limit: 64 }
                if field == "name"
            )),
            "expected a name TooLong(65/64, AsIs), got {:?}",
            doc.warnings
        );

        let long_desc = "d".repeat(1025);
        let doc2 = parse_frontmatter(&format!("name: foo\ndescription: {long_desc}\n"), "foo")
            .expect("an oversized description must still register");
        assert!(
            doc2.warnings.iter().any(|f| matches!(
                f,
                Finding::TooLong { field, len: 1025, limit: 1024 }
                if field == "description"
            )),
            "expected a description TooLong(1025/1024), got {:?}",
            doc2.warnings
        );
    }

    #[test]
    fn empty_description_warns_and_still_registers() {
        // 标准把 description 列为必填，但 FoxIR 的存量与自写路径都允许空描述。
        // 分级裁决是 Warn（退化成"仅目录可发现"），Blocking 会一升级就批量拒掉技能。
        let doc = parse_frontmatter("name: foo\n", "foo")
            .expect("a missing description must not block registration");
        assert_eq!(doc.metadata.description, "");
        assert!(
            matches!(&doc.warnings[..], [Finding::MissingField { key: "description" }]),
            "expected one missing-description warning, got {:?}",
            doc.warnings
        );
    }

    #[test]
    fn consent_records_round_trip_and_keep_the_enable_flag() {
        // P3 的同意必须落在同一个状态文件里，而且不能顺手把 §1.2 守住的
        // 启停开关刷掉 —— 这是"新字段带 #[serde(default)] 才能加"的现场验证。
        let dir = temp_state_dir("consent");
        let path = dir.join("skills_state.json");
        std::fs::write(&path, "{\"S\": false}").unwrap();

        let mut store = SkillStateStore::load(&path);
        store
            .record_consent("S", "aaa", &["sys_process".to_string()], true)
            .expect("consent must persist");

        let reopened = SkillStateStore::load(&path);
        let state = reopened.state_of("S").expect("entry survives");
        assert!(!state.enabled, "recording consent must not re-enable the skill");
        assert_eq!(state.grants.consented_hash.as_deref(), Some("aaa"));
        assert_eq!(
            state.grants.tools,
            vec!["sys_process".to_string()],
            "what was approved has to be recorded, not just that something was"
        );
        assert!(state.grants.declined_hash.is_none());
        assert!(state.grants.granted_at.is_some());

        // 拒绝走同一张表，且把之前的同意清掉
        let mut store2 = SkillStateStore::load(&path);
        store2
            .record_consent("S", "bbb", &["sys_process".to_string()], false)
            .expect("refusal must persist");
        let after = SkillStateStore::load(&path);
        let grants = &after.state_of("S").expect("entry").grants;
        assert_eq!(grants.declined_hash.as_deref(), Some("bbb"));
        assert_eq!(grants.consented_hash, None, "a refusal for other content clears the old approval");

        // 受害者样本：状态文件读不动时不许写回（与 §1.2 同一条底线）
        let broken = dir.join("broken").join("skills_state.json");
        std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
        std::fs::write(&broken, "{ nope").unwrap();
        let mut guarded = SkillStateStore::load(&broken);
        let err = guarded
            .record_consent("S", "aaa", &[], true)
            .expect_err("an unreadable store refuses to write");
        assert!(err.contains("skills_state.json"), "{err}");
        assert_eq!(std::fs::read_to_string(&broken).unwrap(), "{ nope");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_file_is_reported_and_never_written_over() {
        // 现状是 `from_str(...).unwrap_or_default()`：解析失败 = 静默当成空表，
        // 下一次保存就把用户的状态文件覆成空结构。这里要求"报出来 + 不落盘"。
        let dir = temp_state_dir("corrupt");
        let path = dir.join("skills_state.json");
        let garbage = "{ not json at all }";
        std::fs::write(&path, garbage).unwrap();

        let mut store = SkillStateStore::load(&path);
        assert!(
            store.notes().iter().any(|n| n.contains("neither")),
            "a corrupt store must say so, got {:?}",
            store.notes()
        );

        let err = store
            .set_enabled("AnySkill", true)
            .expect_err("an unreadable store must refuse to write");
        assert!(err.contains("skills_state.json"), "got {}", err);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            garbage,
            "the corrupt file must survive a refused write byte for byte"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_bool_state_upgrades_without_losing_disabled_flags() {
        // 旧文件是 {"name": bool}。换成结构体后如果按新格式解析失败就退成空表，
        // 用户手工关掉的技能会全部悄悄重新启用 —— 这条断言就是拦这个的。
        let dir = temp_state_dir("legacy");
        let path = dir.join("skills_state.json");
        std::fs::write(&path, "{\"OldSkill\": false}").unwrap();

        let store = SkillStateStore::load(&path);
        let got = store
            .state_of("OldSkill")
            .expect("a legacy entry must survive the schema change");
        assert!(!got.enabled, "a disabled skill must stay disabled");
        assert_eq!(got.source, Source::Local, "legacy entries have no install source");
        assert!(got.installed_at.is_none(), "installed_at is filled on first write-back only");
        assert!(got.reviewed_at.is_none(), "P1 has no writer for reviewed_at");
        assert!(
            store.notes().iter().any(|n| n.contains("upgraded")),
            "the upgrade must be logged, got {:?}",
            store.notes()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"OldSkill\": false}",
            "load must not write the file back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 天花板外的 `allowed-tools` 必须在**校验期**就说出来（规格 §3.1），
    /// 而不是等到运行时静默忽略 —— 否则技能作者永远不知道自己写的授权没生效。
    #[test]
    fn allowed_tools_outside_the_ceiling_warn_at_parse_time() {
        let doc = parse_frontmatter(
            "name: foo\ndescription: d\nallowed-tools: sys_process winrm\n",
            "foo",
        )
        .expect("an ungrantable name is warn-level");
        assert_eq!(
            doc.metadata.allowed_tools,
            vec!["sys_process".to_string(), "winrm".to_string()],
            "the declaration is kept verbatim, not rewritten"
        );
        assert!(
            doc.warnings
                .iter()
                .any(|f| matches!(f, Finding::GrantUnsupported { tool, .. } if tool == "winrm")),
            "winrm must be reported as ungrantable: {:?}",
            doc.warnings
        );
        assert!(
            !doc.warnings
                .iter()
                .any(|f| matches!(f, Finding::GrantUnsupported { tool, .. } if tool == "sys_process")),
            "the narrowed tool is inside the ceiling: {:?}",
            doc.warnings
        );
    }

    #[test]
    fn skill_md_hash_is_sha256_of_every_raw_byte() {
        // P3 的内容绑定与撤权全压在这个哈希上，所以它的定义必须是"全文原始字节、
        // 含 frontmatter、不做换行归一化"。两个逐字面值是用 sha256sum 单独算出来的，
        // 不是本函数自己算的。
        let crlf = "---\r\nname: foo\r\n---\r\n\r\nbody\r\n";
        let lf = "---\nname: foo\n---\n\nbody\n";
        assert_eq!(
            skill_md_hash(crlf.as_bytes()),
            "0fe06237ce01efc6e9c656f853ab1f02ee4089038818d347666f4fb8e1d39d74"
        );
        assert_eq!(
            skill_md_hash(lf.as_bytes()),
            "af2b502c8e3e6ce08de8b0e6bd71f91372e534d8d4af1f704eb93c406a39584b"
        );
    }

    #[test]
    fn name_different_from_directory_only_warns() {
        // FoxIR 自己的写入路径就是这个形状：目录名走 sanitize_dir_name，frontmatter 写原始
        // name。所以 name ≠ 目录名绝不能是 Blocking，否则一升级就批量弄坏存量技能。
        let doc = parse_frontmatter(
            "name: \"Mermaid: Diagrams\"\ndescription: d\n",
            "Mermaid  Diagrams",
        )
        .expect("a name/dir mismatch must still register");
        assert_eq!(doc.metadata.name, "Mermaid: Diagrams");
        assert!(
            matches!(
                &doc.warnings[..],
                [Finding::NameNotMatchingDir { name, dir }]
                    if name == "Mermaid: Diagrams" && dir == "Mermaid  Diagrams"
            ),
            "expected one NameNotMatchingDir warning, got {:?}",
            doc.warnings
        );
    }

    #[test]
    fn unparseable_yaml_is_blocking_and_not_reported_as_a_missing_name() {
        // 归因必须准：YAML 坏了和 name 缺失是两件事，报错了就没人能修对。
        let findings = parse_frontmatter("name: [unclosed\n  bad: true\n", "foo")
            .expect_err("unparseable frontmatter must not register");
        assert!(
            matches!(&findings[..], [Finding::InvalidYaml { .. }]),
            "expected an InvalidYaml blocking finding, got {:?}",
            findings
        );
    }

    #[test]
    fn skill_without_a_name_is_not_registered() {
        // Blocking 只剩"无法安全加载"的那几类，name 缺失是第一条：没有名字就无从寻址。
        let findings = parse_frontmatter("description: d\n", "foo")
            .expect_err("a skill with no name must not register");
        assert!(
            matches!(&findings[..], [Finding::MissingField { key: "name" }]),
            "expected exactly a missing-name blocking finding, got {:?}",
            findings
        );
    }

    #[test]
    fn triggers_come_from_metadata_or_top_level_alias() {
        // P2 的打分要第一次真正拿到 `triggers`。标准里没有这个键，所以正规位置是
        // `metadata.foxir.triggers`；顶层 `triggers:` 是既有夹具与存量技能在用的形状，必须继续读得到。
        let want = vec!["process".to_string(), "triage".to_string()];
        let from_meta = parse_frontmatter(
            "name: foo\ndescription: d\nmetadata:\n  foxir.triggers: [process, triage]\n",
            "foo",
        )
        .expect("metadata.foxir.triggers must register");
        let from_alias = parse_frontmatter(
            "name: foo\ndescription: d\ntriggers: [process, triage]\n",
            "foo",
        )
        .expect("top-level triggers must still register (existing fixtures use it)");

        assert_eq!(from_meta.metadata.triggers, want, "metadata.foxir.triggers not read");
        assert_eq!(from_alias.metadata.triggers, want, "top-level triggers not read");
    }

    #[test]
    fn standard_compatibility_and_metadata_are_not_dropped() {
        // 标准把"环境前置"放在 `compatibility`、把自定义扩展放在 `metadata` 之下。
        // 今天这两个键在 FoxIR 里既没有字段承接、也没有 deny_unknown_fields，
        // 于是别人按标准写的技能会被静默读成空 —— 这里断言它们必须真的进结构。
        let doc = parse_frontmatter(
            "name: foo\ndescription: d\ncompatibility: Requires Edge on PATH\nmetadata:\n  foxir.triggers: [diagram]\n  target: anything\n",
            "foo",
        )
        .expect("a standard-shaped frontmatter must register");

        assert_eq!(
            doc.metadata.compatibility.as_deref(),
            Some("Requires Edge on PATH"),
            "compatibility must be captured, not dropped"
        );
        assert!(
            doc.metadata.metadata.contains_key("target"),
            "third-party metadata keys must be preserved: {:?}",
            doc.metadata.metadata.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn allowed_tools_accepts_space_delimited_and_sequence_forms() {
        // 标准原文（agentskills.io/specification）的 `allowed-tools` 是空格分隔字符串；
        // FoxIR 自己的写入路径产出 YAML sequence。两种都必须解析成同一个表，
        // 否则别人按标准写的技能会被静默读成空表。
        let space = parse_frontmatter(
            "name: foo\ndescription: d\nallowed-tools: file_read shell_exec\n",
            "foo",
        )
        .expect("space-delimited allowed-tools must register");
        let seq = parse_frontmatter(
            "name: foo\ndescription: d\nallowed-tools: [file_read, shell_exec]\n",
            "foo",
        )
        .expect("sequence allowed-tools must register");

        assert_eq!(
            space.metadata.allowed_tools,
            vec!["file_read".to_string(), "shell_exec".to_string()],
            "space-delimited form must split into the two tool names"
        );
        assert_eq!(
            seq.metadata.allowed_tools, space.metadata.allowed_tools,
            "both shapes must canonicalize to the same list"
        );
    }
}
