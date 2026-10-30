//! 内置缺省技能：编译期内嵌 + 首启安装。
//!
//! `builtin_skills/`（仓库根）在构建期被嵌入为 (技能名, 相对路径, 内容) 二元组表。
//! 启动时把尚不存在的技能写入 `workspace/skills/` 并在 `skills_state.json` 里
//! 标记为启用——已存在的技能与回收站（`_deleted/`）里的技能绝不被触碰。

use std::collections::BTreeMap;
use std::path::Path;

use tracing::{info, warn};

use super::schema::{self, SkillStateStore, SourceManifest};

/// (技能名, 相对技能目录的路径, 文件内容)，由 build.rs 生成。
const BUILTIN_SKILLS: &[(&str, &str, &str)] =
    include!(concat!(env!("OUT_DIR"), "/builtin_skills_embedded.rs"));

/// 把尚不存在的内置技能安装进 `skills_dir` 并标记为启用。
///
/// 技能目录已存在（用户副本优先，绝不覆盖）或位于 `_deleted/` 回收站
/// （用户的删除意图不得被下次启动撤销）时跳过。返回本次安装的技能数；
/// 单个技能失败只记录错误，不阻塞其余安装。
pub fn ensure_builtin_skills(skills_dir: &Path) -> Result<usize, String> {
    let mut by_name: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for &(name, rel, content) in BUILTIN_SKILLS {
        by_name.entry(name).or_default().push((rel, content));
    }
    if by_name.is_empty() {
        return Ok(0);
    }
    if let Err(e) = std::fs::create_dir_all(skills_dir) {
        return Err(format!("create skills dir {}: {}", skills_dir.display(), e));
    }

    let trash = skills_dir.join("_deleted");
    let mut store = SkillStateStore::load(&skills_dir.join("skills_state.json"));
    let mut installed = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for (name, files) in &by_name {
        let target = skills_dir.join(name);
        if target.exists() {
            continue;
        }
        if trash.join(name).exists() {
            info!("Builtin skill '{}' is in the recycle bin; skipping install", name);
            continue;
        }
        match install_one(&target, name, files) {
            Ok(skill_md) => {
                if let Err(e) = schema::write_manifest(&target, &SourceManifest::local(&skill_md)) {
                    warn!("Failed to write source manifest for builtin skill '{}': {}", name, e);
                }
                if let Err(e) = store.set_enabled(name, true) {
                    // 文件已落盘且 frontmatter 默认启用；只是持久化标记失败。
                    warn!("Builtin skill '{}' installed but could not be marked enabled: {}", name, e);
                }
                info!("Installed builtin skill '{}' ({} file(s))", name, files.len());
                installed += 1;
            }
            Err(e) => errors.push(format!("builtin skill '{}': {}", name, e)),
        }
    }

    if errors.is_empty() {
        Ok(installed)
    } else {
        Err(errors.join("; "))
    }
}

/// 写入单个技能的全部文件。失败时清掉半装的目录，避免残缺树被
/// `reload()` 的 glob 发现后永久停留在损坏状态。
fn install_one(target: &Path, name: &str, files: &[(&str, &str)]) -> Result<Vec<u8>, String> {
    let mut skill_md: Option<Vec<u8>> = None;
    for (rel, content) in files {
        let dst = target.join(rel);
        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                cleanup_failed(target, name);
                return Err(format!("create {}: {}", parent.display(), e));
            }
        }
        if let Err(e) = std::fs::write(&dst, content) {
            cleanup_failed(target, name);
            return Err(format!("write {}: {}", dst.display(), e));
        }
        if *rel == "SKILL.md" {
            skill_md = Some(content.as_bytes().to_vec());
        }
    }
    match skill_md {
        Some(bytes) => Ok(bytes),
        None => {
            cleanup_failed(target, name);
            Err("no SKILL.md in the embedded set".to_string())
        }
    }
}

fn cleanup_failed(target: &Path, name: &str) {
    if let Err(e) = std::fs::remove_dir_all(target) {
        warn!("Failed to clean up half-installed builtin skill '{}': {}", name, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn expected_names() -> BTreeSet<&'static str> {
        BUILTIN_SKILLS.iter().map(|(name, _, _)| *name).collect()
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rs_builtin_skills_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn installs_all_embedded_skills_enabled() {
        let tmp = tmp_dir("all");
        let names = expected_names();
        let installed = ensure_builtin_skills(&tmp).expect("install");
        assert_eq!(installed, names.len(), "every embedded skill must install");

        let store = SkillStateStore::load(&tmp.join("skills_state.json"));
        for name in &names {
            let dir = tmp.join(name);
            assert!(dir.join("SKILL.md").is_file(), "{name}: SKILL.md must exist");
            assert!(dir.join(schema::MANIFEST_FILE).is_file(), "{name}: source manifest must exist");
            let state = store.state_of(name).unwrap_or_else(|| panic!("{name}: state entry missing"));
            assert!(state.enabled, "{name}: must be persisted as enabled");
            assert!(state.installed_at.is_some(), "{name}: install time must be recorded");
        }
        // 子目录资源随技能一起落地（references/ 等）
        assert!(
            tmp.join("phishing-analysis/references/report-template.html").is_file(),
            "nested reference files must be installed"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn second_run_is_idempotent_and_never_overwrites() {
        let tmp = tmp_dir("idem");
        let first = ensure_builtin_skills(&tmp).expect("first install");
        assert!(first > 0);

        // 用户改过的文件不得被重写
        let edited = tmp.join("calculator/SKILL.md");
        std::fs::write(&edited, "---\nname: calculator\ndescription: \"mine\"\n---\n# mine\n").unwrap();
        let second = ensure_builtin_skills(&tmp).expect("second install");
        assert_eq!(second, 0, "nothing may be installed twice");
        assert!(
            std::fs::read_to_string(&edited).unwrap().contains("# mine"),
            "an existing skill must never be overwritten"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn recycle_bin_skill_is_not_reinstalled() {
        let tmp = tmp_dir("trash");
        std::fs::create_dir_all(tmp.join("_deleted/calculator")).unwrap();
        let installed = ensure_builtin_skills(&tmp).expect("install");
        assert_eq!(installed, expected_names().len() - 1);
        assert!(!tmp.join("calculator").exists(), "a trashed skill must stay deleted");
        let store = SkillStateStore::load(&tmp.join("skills_state.json"));
        assert!(store.state_of("calculator").is_none(), "no state entry for a trashed skill");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn installed_skills_load_enabled_via_manager() {
        let tmp = tmp_dir("mgr");
        ensure_builtin_skills(&tmp).expect("install");
        let mgr = super::super::SkillManager::new(tmp.to_str().unwrap());
        let listed = mgr.list();
        let names = expected_names();
        assert_eq!(listed.len(), names.len(), "all builtin skills must be discoverable");
        for meta in &listed {
            assert!(meta.enabled, "{} must load enabled", meta.name);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
