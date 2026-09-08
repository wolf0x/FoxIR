//! 记忆迁移：MEMORY.md 存量 → 浅层记忆（shallow），深层留待以后再提炼。
//!
//! - `import_memory_md_to_shallow`：一次性把 MEMORY.md 的 curated 内容按小节切分，
//!   写入浅层记忆（explicit_save=true，受 GC 保护、importance 4.5），并给 MEMORY.md
//!   打归档标记（幂等）。此后 MEMORY.md 仅作存档，不再作为 prompt 输入源。
//! - 深层（deep_facts）暂不写入——由未来的提炼流程（Curator 等）另行沉淀。

use crate::memory::MemoryStore;
use crate::shallow_memory::{ShallowEntry, make_hash};

/// MEMORY.md 顶部归档标记：出现后不再重复迁移。
const ARCHIVED_MARK: &str = "<!-- rustagent: archived-to-shallow-memory -->";

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 一次性迁移 MEMORY.md → 浅层记忆。幂等：已打归档标记则跳过。
pub fn import_memory_md_to_shallow(
    workspace_dir: &str,
    store: &MemoryStore,
) -> Result<usize, String> {
    let md_path = std::path::Path::new(workspace_dir).join("MEMORY.md");
    if !md_path.exists() {
        return Ok(0);
    }
    let raw = std::fs::read_to_string(&md_path).map_err(|e| format!("read MEMORY.md: {e}"))?;
    if raw.contains(ARCHIVED_MARK) {
        return Ok(0);
    }

    // 按 "## " 标题切分小节；整文件无标题则作为一条。
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut title = String::new();
    let mut body = String::new();
    for line in raw.lines() {
        if line.trim_start().starts_with("## ") {
            if !title.is_empty() || !body.trim().is_empty() {
                sections.push((title.clone(), std::mem::take(&mut body)));
            }
            title = line.trim_start().trim_start_matches("## ").trim().to_string();
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    if !title.is_empty() || !body.trim().is_empty() {
        sections.push((title, body));
    }
    if sections.is_empty() {
        sections.push(("MEMORY.md".to_string(), raw.clone()));
    }

    let now = now_secs();
    let mut count = 0usize;
    for (t, b) in sections {
        let content = if b.trim().is_empty() {
            t.clone()
        } else {
            b.trim().to_string()
        };
        if content.is_empty() {
            continue;
        }
        let title_text = if t.trim().is_empty() {
            content.chars().take(160).collect()
        } else {
            t.clone()
        };
        let essence_text: String =
            content.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
        let hash = make_hash("MEMORY-import", &content);
        let entry = ShallowEntry::new(
            hash,
            content,
            title_text,
            essence_text,
            Vec::new(),
            4.5,
            true, // explicit：受 GC 保护
            "MEMORY-import".to_string(),
            now,
        );
        store.shallow_store(&entry).map_err(|e| format!("shallow_store: {e}"))?;
        count += 1;
    }

    // 归档标记（前置到原文，保留归案）
    let header = format!(
        "{ARCHIVED_MARK}\n<!-- migrated {} chunk(s) into shallow memory; MEMORY.md is now ARCHIVE ONLY (not injected). -->\n\n",
        count
    );
    let _ = std::fs::write(&md_path, format!("{header}{raw}"));
    Ok(count)
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("memmig_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn import_persists_to_shallow_and_marks_archive() {
        let d = tmpdir("t1");
        std::fs::write(
            d.join("MEMORY.md"),
            "# 归档\n\n## 偏好\n- 用中文报告\n\n## 约束\n- 不删除证据\n\n## 身份\n- 我是 Wolf\n",
        )
        .unwrap();
        let db = d.join("mem.sqlite").to_string_lossy().into_owned();
        let store = MemoryStore::new(&db).unwrap();

        let n = import_memory_md_to_shallow(d.to_str().unwrap(), &store).unwrap();
        assert!(n >= 3, "expected >=3 chunks, got {n}");

        // 浅层确有 explicit 条目
        let cands = store.shallow_query_candidates(50).unwrap();
        assert!(!cands.is_empty(), "shallow empty after import");
        let explicit = cands.iter().filter(|e| e.explicit_save).count();
        assert!(explicit >= 3, "explicit entries {explicit} < 3");

        // 文件被打归档标记
        let text = std::fs::read_to_string(d.join("MEMORY.md")).unwrap();
        assert!(text.contains(ARCHIVED_MARK));

        // 幂等：二次调用返回 0
        let n2 = import_memory_md_to_shallow(d.to_str().unwrap(), &store).unwrap();
        assert_eq!(n2, 0);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn import_missing_file_is_noop() {
        let d = tmpdir("t2");
        let db = d.join("mem.sqlite").to_string_lossy().into_owned();
        let store = MemoryStore::new(&db).unwrap();
        let n = import_memory_md_to_shallow(d.to_str().unwrap(), &store).unwrap();
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&d);
    }
}



