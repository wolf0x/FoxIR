use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Represents a discovered external tool (executable file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalTool {
    /// Tool name (filename without extension)
    pub name: String,
    /// Full path to the executable
    pub path: PathBuf,
    /// Human-readable description
    pub description: String,
    /// Whether this tool is enabled
    pub enabled: bool,
    /// File extension (.exe, .bat, .ps1, .cmd)
    pub extension: String,
    /// true = 描述是用户在 GUI 里改的（该写进 config.toml）；
    /// false = 来同目录 sidecar 或文件名推出来的（不写死，保持 Tools 目录可拷走即用）。
    pub desc_pinned: bool,
}

/// Manages discovery and state of external tools in the Tools directory.
pub struct ExternalToolsManager {
    tools_dir: PathBuf,
    tools: Vec<ExternalTool>,
    /// 配置归属workspace（config.toml 所在）：External Tools 的启用状态现在跟其它
    /// GUI 设置一起存在那里，不再单独一个 json。
    workspace_dir: String,
    /// 旧格式文件，只用来读（迁移用），不再写。
    legacy_state_path: PathBuf,
}

/// 旧版 `tools/tools_state.json` 的形状，仅用于一次性导入。
#[derive(Debug, Clone, Deserialize, Default)]
struct LegacyToolsState {
    #[serde(default)]
    tools: HashMap<String, LegacyToolState>,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyToolState {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    description: Option<String>,
}

fn default_true() -> bool { true }

impl ExternalToolsManager {
    /// `workspace_dir` 是 config.toml 所在目录（Tools 状态就存在那里）。
    pub fn new(tools_dir: PathBuf, workspace_dir: String) -> Self {
        let legacy_state_path = tools_dir.join("tools_state.json");
        let mut mgr = Self {
            tools_dir,
            tools: Vec::new(),
            workspace_dir,
            legacy_state_path,
        };
        // Ensure tools directory exists
        if !mgr.tools_dir.exists() {
            let _ = std::fs::create_dir_all(&mgr.tools_dir);
        }
        mgr.scan();
        mgr
    }

    /// Scan the Tools directory for executable files.
    pub fn scan(&mut self) {
        self.tools.clear();
        let state = self.load_state();

        if !self.tools_dir.exists() {
            return;
        }

        let entries = match std::fs::read_dir(&self.tools_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!("Failed to read tools dir: {}", e);
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            // Only include known executable types
            if !matches!(ext.as_str(), "exe" | "bat" | "ps1" | "cmd") {
                continue;
            }

            let name = path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            // Check for sidecar .json file
            let sidecar_path = path.with_extension("json");
            let description = if sidecar_path.exists() {
                match std::fs::read_to_string(&sidecar_path) {
                    Ok(content) => {
                        if let Ok(sidecar) = serde_json::from_str::<serde_json::Value>(&content) {
                            sidecar["description"].as_str()
                                .or_else(|| sidecar["name"].as_str())
                                .unwrap_or("")
                                .to_string()
                        } else {
                            Self::auto_description(&name, &ext)
                        }
                    }
                    Err(_) => Self::auto_description(&name, &ext),
                }
            } else {
                Self::auto_description(&name, &ext)
            };

            // Apply state overrides
            let (enabled, description, desc_pinned) = match state.get(&name) {
                // config 里有条目：enabled 以它为准；description 只有被显式记下来过才赢。
                Some(entry) => {
                    let desc = entry
                        .description
                        .clone()
                        .unwrap_or_else(|| description.clone());
                    (entry.enabled, desc, entry.description.is_some())
                }
                None => (true, description, false),
            };

            info!("Discovered tool: {} ({}) enabled={}", name, path.display(), enabled);
            self.tools.push(ExternalTool {
                name,
                path,
                description,
                enabled,
                extension: ext,
                desc_pinned,
            });
        }
    }

    fn auto_description(name: &str, ext: &str) -> String {
        let friendly_name = name.replace('_', " ");
        match ext {
            "exe" => format!("Execute {} tool", friendly_name),
            "bat" => format!("Run {} batch script", friendly_name),
            "ps1" => format!("Run {} PowerShell script", friendly_name),
            "cmd" => format!("Run {} command script", friendly_name),
            _ => format!("Run {} tool", friendly_name),
        }
    }

    /// List all discovered tools.
    pub fn list_tools(&self) -> Vec<serde_json::Value> {
        self.tools.iter().map(|t| {
            serde_json::json!({
                "name": t.name,
                "path": t.path.to_string_lossy(),
                "description": t.description,
                "enabled": t.enabled,
                "extension": t.extension,
            })
        }).collect()
    }

    /// Toggle a tool's enabled state.
    pub fn toggle_tool(&mut self, name: &str) -> Option<bool> {
        let tool = self.tools.iter_mut().find(|t| t.name == name)?;
        tool.enabled = !tool.enabled;
        Some(tool.enabled)
    }

    /// Update a tool's description.
    pub fn update_description(&mut self, name: &str, description: &str) -> bool {
        if let Some(tool) = self.tools.iter_mut().find(|t| t.name == name) {
            tool.description = description.to_string();
            tool.desc_pinned = true;
            true
        } else {
            false
        }
    }

    /// Get the tools directory path.
    pub fn tools_dir(&self) -> &Path {
        &self.tools_dir
    }

    /// 读持久状态：以 config.toml 为准；那里还是空时（旧部署升级上来），
    /// 一次性从 `tools/tools_state.json` 导入。导入不会删旧文件（不动用户文件），
    /// 但下次保存只写 config，旧文件从此不会再被读进配置非空的工作区。
    fn load_state(&self) -> HashMap<String, crate::config::ExternalToolState> {
        let from_config = crate::config::Config::load(&self.workspace_dir)
            .map(|c| c.agent.external_tools)
            .unwrap_or_default();
        if !from_config.is_empty() {
            return from_config;
        }
        if !self.legacy_state_path.exists() {
            return HashMap::new();
        }
        match std::fs::read_to_string(&self.legacy_state_path)
            .ok()
            .and_then(|content| serde_json::from_str::<LegacyToolsState>(&content).ok())
        {
            Some(legacy) if !legacy.tools.is_empty() => {
                info!(
                    "Importing {} external tool state entry(ies) from {} into config.toml",
                    legacy.tools.len(),
                    self.legacy_state_path.display()
                );
                legacy
                    .tools
                    .into_iter()
                    .map(|(name, e)| {
                        (
                            name,
                            crate::config::ExternalToolState {
                                enabled: e.enabled,
                                description: e.description,
                            },
                        )
                    })
                    .collect()
            }
            _ => HashMap::new(),
        }
    }

    /// 保存全量状态到 config.toml。失败只记日志（与旧行为一致）：一个工具开关存不上
    /// 不该让整次 toggle 报错返回给用户，但必须能在日志里看出来。
    pub fn save_state(&self) {
        let entries: Vec<(String, bool, Option<String>)> = self
            .tools
            .iter()
            .map(|t| {
                (
                    t.name.clone(),
                    t.enabled,
                    if t.desc_pinned { Some(t.description.clone()) } else { None },
                )
            })
            .collect();
        if let Err(e) = crate::config::Config::save_external_tools(&self.workspace_dir, entries) {
            warn!("Failed to save tools state to config.toml: {}", e);
        }
    }

    /// Get enabled tools as registration handles: (name, path, description, extension).
    pub fn get_tool_handles(&self) -> Vec<(String, PathBuf, String, String)> {
        self.tools.iter()
            .filter(|t| t.enabled)
            .map(|t| (
                format!("ext_{}", t.name),
                t.path.clone(),
                t.description.clone(),
                t.extension.clone(),
            ))
            .collect()
    }
}
