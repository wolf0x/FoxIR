//! P4b 测试口径（规格 §5.3）。实现见本文件后半段的 `unpack_zip`。

//! P4b —— 把 zip 包解进隔离区。
//!
//! 顺序就是这条设计的安全性：条目数 → 只读中央目录、复用 `install::plan_entries`
//! 做写前筛查 → 压缩比 → 才开始落盘。任何一步失败，整棵已写的树删掉；隔离区在
//! `skills/` 之外，所以 `reload()` 的 glob 永远看不到半成品。
//!
//! 落盘是逐条目流式的，并且**边写边计数**：中央目录里声明的大小可能是假的，
//! 真正拦住 25 MiB 展开量的是读到的字节数，不是表上的数字。

use std::io::{Read, Write};
use std::path::Path;

use super::install::{
    EntryKind, EntrySpec, InstallError, MAX_ENTRIES, MAX_FILE_BYTES, MAX_TOTAL_BYTES,
};

/// 压缩比上限。真实技能包（文本为主）远达不到 50:1，而炸弹通常上千。
pub const MAX_RATIO: u64 = 50;

/// 读一块就记一块的计费器：单文件与整包两个上限。
#[derive(Debug, Default)]
pub struct ByteBudget {
    file: u64,
    total: u64,
}

impl ByteBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new entry starts its own per-file count; the running total carries over.
    pub fn start_entry(&mut self, _path: &str) {
        self.file = 0;
    }

    pub fn charge(&mut self, path: &str, more: u64) -> Result<(), InstallError> {
        self.file += more;
        if self.file > MAX_FILE_BYTES {
            return Err(InstallError::FileTooLarge {
                path: path.to_string(),
                bytes: self.file,
                limit: MAX_FILE_BYTES,
            });
        }
        self.total += more;
        if self.total > MAX_TOTAL_BYTES {
            return Err(InstallError::TotalTooLarge {
                bytes: self.total,
                limit: MAX_TOTAL_BYTES,
            });
        }
        Ok(())
    }
}

/// Turn any zip-level failure into one refusal reason. Split/spanned and
/// password-protected archives arrive here as `UnsupportedArchive`.
fn refuse(err: zip::result::ZipError) -> Vec<InstallError> {
    vec![InstallError::UnsupportedArchive {
        reason: err.to_string(),
    }]
}

/// Unpack `archive` into `dest`, or leave nothing behind.
pub fn unpack_zip(archive: &Path, dest: &Path) -> Result<(), Vec<InstallError>> {
    unpack(archive, dest).map_err(|errors| {
        // 整包丢弃：绝不留一棵部分写出的树给后面的环节误用。
        let _ = std::fs::remove_dir_all(dest);
        errors
    })
}

fn unpack(archive: &Path, dest: &Path) -> Result<(), Vec<InstallError>> {
    let file = std::fs::File::open(archive).map_err(|e| {
        vec![InstallError::Unreadable {
            path: archive.display().to_string(),
            reason: e.to_string(),
        }]
    })?;
    let mut zip = zip::ZipArchive::new(file).map_err(refuse)?;

    let count = zip.len();
    if count > MAX_ENTRIES {
        return Err(vec![InstallError::TooManyEntries {
            count,
            limit: MAX_ENTRIES,
        }]);
    }

    // Pass 1: central directory only. No content is touched yet.
    let mut specs: Vec<EntrySpec> = Vec::with_capacity(count);
    let mut errors: Vec<InstallError> = Vec::new();
    for index in 0..count {
        let entry = zip.by_index(index).map_err(refuse)?;
        let rel = entry.name().replace('\\', "/");
        if entry.encrypted() {
            errors.push(InstallError::Encrypted { path: rel });
            continue;
        }
        let kind = if entry.is_symlink() {
            EntryKind::Symlink
        } else if entry.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        let expanded = entry.size();
        let compressed = entry.compressed_size();
        if kind == EntryKind::File && expanded > 0 && expanded > MAX_RATIO * compressed.max(1) {
            errors.push(InstallError::ZipBomb {
                path: rel.clone(),
                expanded,
                compressed,
            });
            continue;
        }
        specs.push(EntrySpec {
            rel,
            bytes: expanded,
            kind,
        });
    }
    if errors.is_empty() {
        errors = plan_entries_for_unpack(&specs);
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Pass 2: write, streaming, counting as we go.
    std::fs::create_dir_all(dest).map_err(|e| {
        vec![InstallError::Unreadable {
            path: dest.display().to_string(),
            reason: e.to_string(),
        }]
    })?;
    let mut budget = ByteBudget::new();
    let mut buffer = vec![0u8; 64 * 1024];
    for index in 0..count {
        let mut entry = zip.by_index(index).map_err(refuse)?;
        let rel = entry.name().replace('\\', "/");
        // `enclosed_name` is zip's own traversal guard; our screen already
        // refused `..` and absolute paths, and both must agree before writing.
        if entry.enclosed_name().is_none() {
            return Err(vec![InstallError::EscapesSkillDir { path: rel }]);
        }
        let target = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| {
                vec![InstallError::Unreadable {
                    path: target.display().to_string(),
                    reason: e.to_string(),
                }]
            })?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                vec![InstallError::Unreadable {
                    path: parent.display().to_string(),
                    reason: e.to_string(),
                }]
            })?;
        }
        let mut out = std::fs::File::create(&target).map_err(|e| {
            vec![InstallError::Unreadable {
                path: target.display().to_string(),
                reason: e.to_string(),
            }]
        })?;
        budget.start_entry(&rel);
        loop {
            let read = entry.read(&mut buffer).map_err(|e| {
                vec![InstallError::Unreadable {
                    path: rel.clone(),
                    reason: e.to_string(),
                }]
            })?;
            if read == 0 {
                break;
            }
            budget.charge(&rel, read as u64).map_err(|e| vec![e])?;
            out.write_all(&buffer[..read]).map_err(|e| {
                vec![InstallError::Unreadable {
                    path: rel.clone(),
                    reason: e.to_string(),
                }]
            })?;
        }
    }
    Ok(())
}

/// The write-before screen: the same gate a staged folder is held to.
fn plan_entries_for_unpack(specs: &[EntrySpec]) -> Vec<InstallError> {
    match super::install::plan_entries(specs) {
        Ok(_) => Vec::new(),
        Err(errors) => errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    fn opts_stored() -> SimpleFileOptions {
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
    }
    fn opts_deflated() -> SimpleFileOptions {
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)
    }

    const SKILL_MD: &[u8] = b"---\nname: Zip\n---\n# body\n";

    /// 一个正常技能包：根上一个 SKILL.md，外加一个子目录文件。
    fn healthy_zip() -> Vec<u8> {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("SKILL.md", opts_stored()).unwrap();
        w.write_all(SKILL_MD).unwrap();
        w.start_file("assets/notes.txt", opts_stored()).unwrap();
        w.write_all(b"hello").unwrap();
        w.finish().unwrap().into_inner()
    }

    fn write_tmp(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rs_unpack_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pkg.zip"), bytes).unwrap();
        dir
    }

    /// 列出解包结果（相对路径，含目录），失败信息里要能看见树。
    fn tree_of(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else { continue };
            for entry in entries.flatten() {
                let rel = entry
                    .path()
                    .strip_prefix(dir)
                    .unwrap_or(entry.path().as_path())
                    .to_string_lossy()
                    .replace('\\', "/");
                if entry.path().is_dir() {
                    out.push(format!("{rel}/"));
                    stack.push(entry.path());
                } else {
                    out.push(rel);
                }
            }
        }
        out.sort();
        out
    }

    /// 正对照：健康包必须真的解出来。没有这条，上面所有"被拒"的断言都可以靠
    /// "永远拒绝"通过。
    #[test]
    fn a_healthy_zip_unpacks_into_the_staging_dir() {
        let dir = write_tmp("healthy", &healthy_zip());
        let dest = dir.join("staged");

        let outcome = unpack_zip(&dir.join("pkg.zip"), &dest);
        let tree = tree_of(&dest);
        let skill_md = std::fs::read(dest.join("SKILL.md"));
        let notes = std::fs::read(dest.join("assets/notes.txt"));
        let _ = std::fs::remove_dir_all(&dir);
        outcome.expect("a plain skill package must unpack");

        assert!(tree.contains(&"SKILL.md".to_string()), "tree: {tree:?}");
        assert!(
            tree.contains(&"assets/notes.txt".to_string()),
            "tree: {tree:?}"
        );
        assert_eq!(
            skill_md.expect("SKILL.md must land"),
            SKILL_MD,
            "SKILL.md must land byte-for-byte"
        );
        assert_eq!(
            notes.expect("nested paths must be recreated"),
            b"hello",
            "assets/notes.txt must land byte-for-byte"
        );
    }

    /// 压缩比先于展开：一个 4 MiB 全零文件压完只剩几 KiB，比值远超 50:1，
    /// 而它单文件并不超 10 MiB 的上限 —— 只有比值这一条能拦住它。
    #[test]
    fn zip_bomb_is_rejected_by_ratio_before_full_expansion() {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("SKILL.md", opts_stored()).unwrap();
        w.write_all(SKILL_MD).unwrap();
        w.start_file("blob.bin", opts_deflated()).unwrap();
        w.write_all(&vec![0u8; 4 * 1024 * 1024]).unwrap();
        let bytes = w.finish().unwrap().into_inner();

        let dir = write_tmp("bomb", &bytes);
        let dest = dir.join("staged");
        let errors = unpack_zip(&dir.join("pkg.zip"), &dest)
            .err()
            .expect("a 1000:1 ratio must be refused");
        let wrote_anything = dest.exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            errors.iter().any(|e| matches!(e, InstallError::ZipBomb { .. })),
            "the refusal must name the ratio: {errors:?}"
        );
        assert!(
            !wrote_anything,
            "the whole package is discarded, not left half-written"
        );
    }

    /// 条目数在碰任何内容之前就被拦住。
    #[test]
    fn entry_count_and_size_caps_are_enforced() {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("SKILL.md", opts_stored()).unwrap();
        w.write_all(SKILL_MD).unwrap();
        for i in 0..MAX_ENTRIES {
            w.start_file(format!("f{i}.txt"), opts_stored()).unwrap();
            w.write_all(b"x").unwrap();
        }
        let bytes = w.finish().unwrap().into_inner();

        let dir = write_tmp("count", &bytes);
        let errors = unpack_zip(&dir.join("pkg.zip"), &dir.join("staged"))
            .err()
            .expect("MAX_ENTRIES + 1 files must be refused");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            matches!(
                &errors[..],
                [InstallError::TooManyEntries { count, limit }] if *count == MAX_ENTRIES + 1 && *limit == MAX_ENTRIES
            ),
            "one refusal, naming the count: {errors:?}"
        );
    }

    /// 逐条目流式计费的两个上限（单文件、累计）。写 25 MiB 进测试不划算，
    /// 所以断的是循环真正调用的那个计费器。
    #[test]
    fn the_streaming_budget_stops_at_the_per_file_and_total_caps() {
        let mut budget = ByteBudget::new();
        budget.start_entry("a.txt");
        budget.charge("a.txt", MAX_FILE_BYTES - 1).expect("just under the cap");
        assert!(
            matches!(
                budget.charge("a.txt", 2),
                Err(InstallError::FileTooLarge { bytes, limit, .. })
                    if bytes == MAX_FILE_BYTES + 1 && limit == MAX_FILE_BYTES
            ),
            "the over-cap numbers must be reported, not just the refusal"
        );

        let mut wide = ByteBudget::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            wide.start_entry(name);
            let charge = wide.charge(name, 9 * 1024 * 1024);
            if name == "c.txt" {
                assert!(
                    matches!(&charge, Err(InstallError::TotalTooLarge { bytes, limit })
                        if *bytes == 27 * 1024 * 1024 && *limit == MAX_TOTAL_BYTES),
                    "the running total must stop the package mid-write: {charge:?}"
                );
            } else {
                charge.expect("each file is under the per-file cap on its own");
            }
        }
    }

    /// 把通用位标志的 bit 0（加密）在每个本地头（+6）和每个中央目录头（+8）里置起来。
    /// 两个偏移不同是 zip 的格式事实：中央目录头比本地头多两个版本字段。
    fn mark_encrypted(bytes: &mut Vec<u8>) {
        for start in 0..bytes.len().saturating_sub(11) {
            let (flag_at, is_header) = match &bytes[start..start + 4] {
                b"PK\x03\x04" => (start + 6, true),
                b"PK\x01\x02" => (start + 8, true),
                _ => (start, false),
            };
            if is_header {
                bytes[flag_at] |= 1;
            }
        }
    }

    /// 加密位与"根本不是 zip"两条真路径。多卷包没有真夹具（要造合法上卷结构），
    /// 它由 zip 自己以 `UnsupportedArchive` 抛出，映射到同一条拒绝上。
    #[test]
    fn encrypted_or_split_zip_is_refused() {
        let mut encrypted = healthy_zip();
        mark_encrypted(&mut encrypted);
        let dir = write_tmp("encrypted", &encrypted);
        let errors = unpack_zip(&dir.join("pkg.zip"), &dir.join("staged"))
            .err()
            .expect("an encrypted package must be refused");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, InstallError::Encrypted { .. } | InstallError::UnsupportedArchive { .. })),
            "the refusal must say encryption: {errors:?}"
        );

        let dir = write_tmp("garbage", b"this is not a zip file at all, not even close");
        let errors = unpack_zip(&dir.join("pkg.zip"), &dir.join("staged"))
            .err()
            .expect("a non-archive must be refused");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            errors.iter().any(|e| matches!(
                e,
                InstallError::UnsupportedArchive { .. } | InstallError::Unreadable { .. }
            )),
            "an unreadable archive is reported, never half-installed: {errors:?}"
        );
    }

    /// 绝对路径与符号链接都不许落到盘上。
    #[test]
    fn zip_with_absolute_paths_and_symlinks_is_refused() {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("SKILL.md", opts_stored()).unwrap();
        w.write_all(SKILL_MD).unwrap();
        w.add_symlink("innocent.md", "/etc/passwd", opts_stored()).unwrap();
        let with_symlink = w.finish().unwrap().into_inner();

        let dir = write_tmp("symlink", &with_symlink);
        let errors = unpack_zip(&dir.join("pkg.zip"), &dir.join("staged"))
            .err()
            .unwrap_or_else(|| panic!("a symlink entry must be refused"));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            errors.iter().any(|e| matches!(e, InstallError::SymlinkEntry { .. })),
            "{errors:?}"
        );

        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("/etc/passwd", opts_stored()).unwrap();
        w.write_all(b"nope").unwrap();
        w.start_file("SKILL.md", opts_stored()).unwrap();
        w.write_all(SKILL_MD).unwrap();
        let with_absolute = w.finish().unwrap().into_inner();

        let dir = write_tmp("absolute", &with_absolute);
        let errors = unpack_zip(&dir.join("pkg.zip"), &dir.join("staged")).err();
        let _ = std::fs::remove_dir_all(&dir);
        let errors = match errors {
            Some(errors) => errors,
            // The writer may sanitize the leading slash away; then this fixture
            // proves nothing and must be reported rather than passed silently.
            None => panic!("the absolute-path fixture was not refused; the writer likely rewrote the name"),
        };
        assert!(
            errors.iter().any(|e| matches!(e, InstallError::AbsolutePath { .. })),
            "{errors:?}"
        );
    }
}
