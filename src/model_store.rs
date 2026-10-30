//! Model config persistence — save/load Vec<ModelConfig> to/from JSON.
//! Follows the same pattern as MCP server config persistence (mcp_client.rs).
//! API keys are encrypted at rest using AES-256-GCM.

use std::path::Path;
use tracing::{info, warn, error};

use crate::config::ModelConfig;

/// Save model configs to a JSON file, encrypting api_keys.
pub fn save_configs(configs: &[ModelConfig], path: &Path) {
    let mut configs: Vec<ModelConfig> = configs.to_vec();
    for cfg in &mut configs {
        if let Some(ref key) = cfg.api_key {
            if !key.is_empty() && !crate::crypto::is_encrypted(key) {
                cfg.api_key = Some(crate::crypto::encrypt(key));
            }
        }
    }
    match serde_json::to_string_pretty(&configs) {
        Ok(json_str) => {
            if let Err(e) = std::fs::write(path, json_str) {
                error!("Failed to save model configs: {}", e);
            } else {
                info!("Model configs saved to {} (api keys encrypted)", path.display());
            }
        }
        Err(e) => error!("Failed to serialize model configs: {}", e),
    }
}

/// Load model configs from a JSON file, decrypting api_keys.
pub fn load_configs(path: &Path) -> Vec<ModelConfig> {
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<Vec<ModelConfig>>(&content) {
            Ok(mut configs) => {
                for cfg in &mut configs {
                    if let Some(ref key) = cfg.api_key {
                        if !key.is_empty() {
                            cfg.api_key = Some(crate::crypto::decrypt(key));
                        }
                    }
                }
                // Backfill: older models.json entries without a title default to their name.
                for cfg in &mut configs {
                    if cfg.title.trim().is_empty() {
                        cfg.title = cfg.name.clone();
                    }
                }
                info!(
                    "Loaded {} persisted model config(s) from {} (api keys decrypted)",
                    configs.len(),
                    path.display()
                );
                configs
            }
            Err(e) => {
                warn!("Failed to parse model configs: {}", e);
                Vec::new()
            }
        },
        Err(e) => {
            warn!("Failed to read model configs: {}", e);
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(vision: bool) -> ModelConfig {
        ModelConfig {
            title: "m".into(), name: "m".into(), api_base: "http://x".into(),
            api_key: None, api_key_env: None,
            context_window: 128000, max_tokens: 16384, temperature: 0.7,
            supports_vision: vision,
        }
    }

    /// 视觉开关必须原样往返持久化：关掉的存量模型不能因为默认值改动而被打开。
    #[test]
    fn vision_flag_survives_save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");

        save_configs(&[sample(false), sample(true)], &path);
        let loaded = load_configs(&path);

        assert_eq!(loaded.len(), 2, "both models must persist");
        assert!(!loaded[0].supports_vision, "an off model stays off across reload");
        assert!(loaded[1].supports_vision, "an on model stays on across reload");
    }
}
