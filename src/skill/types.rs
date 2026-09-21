use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// Skill metadata following the agentskills.io open schema (no FoxIR-private
/// `x-foxir` extension fields).
///
/// Required: `name`. Optional (agentskills.io): `description`, `license`,
/// `version`, `platforms`, `deps`, `allowed-tools`. `enabled` is a *runtime*
/// switch managed via `skills_state.json` and is not part of the YAML
/// frontmatter contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// Skill identifier (kebab/lower-case is conventional but not enforced).
    pub name: String,
    /// Human/agent-readable purpose. Surfaced in catalogs so the model can
    /// self-route to the skill and load its body via `skill_read_file`.
    #[serde(default)]
    pub description: String,
    /// SPDX license identifier (agentskills.io, optional).
    #[serde(default)]
    pub license: Option<String>,
    /// Semantic version (e.g. "1.0.0"). Bumped by the self-improvement loop.
    #[serde(default = "default_version")]
    pub version: String,
    /// Target platforms (agentskills.io): windows / macos / linux / ...
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Required external commands / packages (agentskills.io).
    #[serde(default)]
    pub deps: Vec<String>,
    /// Pre-approved tool names the skill may use (kebab-case key `allowed-tools`).
    #[serde(default, rename = "allowed-tools")]
    pub allowed_tools: Vec<String>,
    /// Runtime on/off switch, managed via `skills_state.json` (not part of the
    /// YAML frontmatter contract).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool { true }
fn default_version() -> String { "1.0.0".to_string() }

/// Controls skill ranking and filtering during matching.
///
/// Skills are scored via weighted token overlap (name ×4.0, description ×2.5,
/// triggers ×2.0, body ×1.0), normalized by `sqrt(body_tokens)` to prevent
/// large documents from dominating. Only skills scoring >= `min_score` are
/// returned, up to `top_k` results.
#[derive(Debug, Clone)]
pub struct SelectionPolicy {
    pub top_k: usize,
    pub min_score: f32,
}

impl Default for SelectionPolicy {
    fn default() -> Self {
        Self {
            top_k: 3,
            min_score: 0.1,
        }
    }
}

/// Skill instruction body.
///
/// Discovery parses only the frontmatter at boot; the markdown body is read
/// lazily from the on-disk SKILL.md on first access and cached thereafter
/// (std::sync::OnceLock). `Eager` is used for in-memory-built skills (tests,
/// programmatic construction) where there is no file to read.
#[derive(Debug, Clone)]
pub enum SkillContent {
    Eager(String),
    Lazy {
        path: PathBuf,
        cell: Arc<OnceLock<String>>,
    },
}

impl SkillContent {
    /// True when this skill's body is lazily loaded from disk.
    pub fn is_lazy(&self) -> bool {
        matches!(self, SkillContent::Lazy { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepItem {
    /// A short, human-readable description of one workflow step.
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub metadata: SkillMetadata,
    /// Skill instruction body (lazy unless built in-memory).
    pub content: SkillContent,
    /// Directory path of the skill (e.g., skills/VulnerabilityPrioritization).
    /// Every skill is a directory containing SKILL.md and optional supporting files.
    pub skill_dir: String,
    /// Lazily compiled linear step contract (empty when none parseable).
    /// Computed once from the body (which is itself cached) so install/load is cheap.
    pub contract: Arc<OnceLock<Vec<StepItem>>>,
}

/// How the skill catalog is surfaced to the model on each turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillListingStrategy {
    /// Inline top-K matched skills with full body; list the remaining cold
    /// skills as name:description so the model can load them on demand via
    /// `skill_read_file`.
    #[default]
    Query,
    /// Only a compact list of skill names (no descriptions, no bodies).
    NamesOnly,
    /// Inject no skill listing at all; rely solely on the discover tool.
    DiscoverToolOnly,
    /// Inject no skill listing and expose no skill tools (sub-agents default).
    Disabled,
}

impl SkillListingStrategy {
    /// Parse from a lowercase config string (unknown -> Query).
    pub fn from_str(s: &str) -> Self {
        match s {
            "names-only" => Self::NamesOnly,
            "discover-tool-only" => Self::DiscoverToolOnly,
            "disabled" => Self::Disabled,
            _ => Self::Query,
        }
    }

    /// Canonical lowercase string for config/settings serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::NamesOnly => "names-only",
            Self::DiscoverToolOnly => "discover-tool-only",
            Self::Disabled => "disabled",
        }
    }

    /// Numeric index for storing in an atomic config (0=Query,1=NamesOnly,2=DiscoverOnly).
    pub fn index(&self) -> usize {
        match self {
            Self::Query => 0,
            Self::NamesOnly => 1,
            Self::DiscoverToolOnly => 2,
            Self::Disabled => 3,
        }
    }

    /// Rebuild from a numeric index (unknown -> Query).
    pub fn from_index(i: usize) -> Self {
        match i {
            1 => Self::NamesOnly,
            2 => Self::DiscoverToolOnly,
            3 => Self::Disabled,
            _ => Self::Query,
        }
    }
}
