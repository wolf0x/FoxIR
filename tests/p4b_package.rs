//! P4b 完整测试矩阵：只走公开 API（`unpack_zip` / `SkillManager::install_from_archive`），
//! 从包外面看进去 —— 单元用例证明"每条闸各自拦得住什么"，这里证明"真包真目录树
//! 上的行为"，并且每条拒绝都配一个能落地的正对照，防止"永远拒绝"也能全绿。

use FoxIR::skill::install::InstallError;
use FoxIR::skill::unpack::unpack_zip;
use FoxIR::skill::SkillManager;
use std::io::Write;
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

fn stored() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
}

const SKILL_MD: &[u8] = b"---\nname: Probe\ndescription: a probe skill\n---\n# p\n";

/// 造包器：stored（不压缩），所以条目大小与比值都由我写死，不靠压缩器配合。
struct Zip {
    w: ZipWriter<std::io::Cursor<Vec<u8>>>,
}

impl Zip {
    fn new() -> Self {
        Self {
            w: ZipWriter::new(std::io::Cursor::new(Vec::new())),
        }
    }
    fn file(mut self, name: &str, bytes: &[u8]) -> Self {
        self.w.start_file(name, stored()).expect("start_file");
        self.w.write_all(bytes).expect("write_all");
        self
    }
    fn dir(mut self, name: &str) -> Self {
        self.w.add_directory(name, stored()).expect("add_directory");
        self
    }
    fn symlink(mut self, name: &str, target: &str) -> Self {
        self.w
            .add_symlink(name, target, stored())
            .expect("add_symlink");
        self
    }
    /// 只有一份 SKILL.md 的合法骨架，其余测试在它上面加一项坏东西。
    fn skill(self) -> Self {
        self.file("SKILL.md", SKILL_MD)
    }
    fn finish(self) -> Vec<u8> {
        self.w.finish().expect("finish").into_inner()
    }
}

/// 一次性沙箱：`pkg.zip` + 解包目标 `staged/`，Drop 时整目录删掉。
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "rs_p4b_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("sandbox root");
        Self { root }
    }

    fn archive(&self) -> PathBuf {
        self.root.join("pkg.zip")
    }

    fn staged(&self) -> PathBuf {
        self.root.join("staged")
    }

    fn unpack(&self, bytes: &[u8]) -> Result<(), Vec<InstallError>> {
        std::fs::write(self.archive(), bytes).expect("write archive");
        unpack_zip(&self.archive(), &self.staged())
    }

    fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn has_error(errors: &[InstallError], wanted: impl Fn(&InstallError) -> bool) -> bool {
    errors.iter().any(wanted)
}

/// 只改**指定条目**在中央目录与本地头里声明的大小，其余条目保持真话。
/// 偏移是 zip 格式事实：本地头 usize@+22 / namelen@+26 / name@+30；
/// 中央目录 usize@+24 / namelen@+28 / name@+46。
fn patch_expanded_size(bytes: &mut [u8], entry: &str, expanded: u32) {
    let name = entry.as_bytes();
    let mut at = 0usize;
    while at + 30 <= bytes.len() {
        let sig = &bytes[at..at + 4];
        let (size_at, len_at, name_at) = if sig == b"PK\x03\x04" {
            (at + 22, at + 26, at + 30)
        } else if sig == b"PK\x01\x02" {
            (at + 24, at + 28, at + 46)
        } else {
            at += 1;
            continue;
        };
        let declared = u16::from_le_bytes([bytes[len_at], bytes[len_at + 1]]) as usize;
        if name_at + declared <= bytes.len() && &bytes[name_at..name_at + declared] == name {
            bytes[size_at..size_at + 4].copy_from_slice(&expanded.to_le_bytes());
        }
        at += 1;
    }
}

/// 一个 deflate 条目：表上写 `declared_expanded`，真实展开是 `actual_expanded`。
fn deflated_zip(entry: &str, actual_expanded: usize, zeros: bool) -> Vec<u8> {
    use std::io::Write;
    let mut w = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    w.start_file("SKILL.md", stored()).unwrap();
    w.write_all(SKILL_MD).unwrap();
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    w.start_file(entry, deflated).unwrap();
    // 全零可压到极小；非零数据压不动，用来造"展开后仍然很大"的合法载荷
    let payload: Vec<u8> = if zeros {
        vec![0u8; actual_expanded]
    } else {
        (0..actual_expanded).map(|i| (i % 251) as u8).collect()
    };
    w.write_all(&payload).unwrap();
    w.finish().unwrap().into_inner()
}

// ---------------------------------------------------------------- 路径形状

#[test]
fn parent_traversal_is_refused_and_nothing_is_written() {
    let sand = Sandbox::new("traversal");
    let bytes = Zip::new()
        .skill()
        .file("../evil.txt", b"nope")
        .finish();

    let errors = sand
        .unpack(&bytes)
        .err()
        .expect("a .. entry must be refused");
    assert!(
        has_error(&errors, |e| matches!(e, InstallError::EscapesSkillDir { .. })),
        "{errors:?}"
    );
    assert!(!sand.staged().exists(), "refused before anything landed");
    assert!(
        !sand.root.join("..").join("evil.txt").exists(),
        "the escape target must not exist either"
    );
}

#[test]
fn drive_absolute_and_backslash_escaped_paths_are_refused() {
    let sand = Sandbox::new("absolute");
    let bytes = Zip::new()
        .skill()
        .file("C:\\Windows\\evil.txt", b"nope")
        .finish();
    let errors = sand.unpack(&bytes).err().expect("a drive-absolute entry");
    assert!(
        has_error(&errors, |e| matches!(e, InstallError::AbsolutePath { .. })),
        "{errors:?}"
    );

    let sand = Sandbox::new("backslash");
    let bytes = Zip::new()
        .skill()
        .file("assets\\..\\..\\escape.txt", b"nope")
        .finish();
    let errors = sand
        .unpack(&bytes)
        .err()
        .expect("backslashes must be normalized before screening");
    assert!(
        has_error(&errors, |e| matches!(e, InstallError::EscapesSkillDir { .. })),
        "{errors:?}"
    );
}

#[test]
fn reserved_underscore_and_executable_entries_are_refused() {
    let sand = Sandbox::new("reserved");
    let bytes = Zip::new()
        .skill()
        .file("_deleted/old.md", b"nope")
        .finish();
    let errors = sand.unpack(&bytes).err().expect("a reserved segment");
    assert!(
        has_error(&errors, |e| matches!(e, InstallError::ReservedPrefix { .. })),
        "{errors:?}"
    );

    let sand = Sandbox::new("executable");
    let bytes = Zip::new().skill().file("tool.exe", b"MZ").finish();
    let errors = sand.unpack(&bytes).err().expect("an executable payload");
    assert!(
        has_error(&errors, |e| matches!(e, InstallError::ExecutablePayload { .. })),
        "{errors:?}"
    );
    assert!(!sand.staged().exists(), "nothing lands for a refused package");
}

#[test]
fn symlink_entries_are_refused_and_regular_children_still_land() {
    let sand = Sandbox::new("symlink");
    let bytes = Zip::new()
        .skill()
        .symlink("innocent.md", "/etc/passwd")
        .finish();
    let errors = sand.unpack(&bytes).err().expect("a symlink entry");
    assert!(
        has_error(&errors, |e| matches!(e, InstallError::SymlinkEntry { .. })),
        "{errors:?}"
    );

    // 正对照：同一形状，只是把符号链接换成普通文件，就必须解出来
    let sand = Sandbox::new("symlink_control");
    let bytes = Zip::new()
        .skill()
        .file("innocent.md", b"harmless")
        .finish();
    sand.unpack(&bytes).expect("a plain sibling file is fine");
    assert!(sand.staged().join("innocent.md").is_file());
}

// ---------------------------------------------------------------- 包形状

#[test]
fn package_shape_rules_are_enforced() {
    let cases: Vec<(&str, Vec<u8>, fn(&InstallError) -> bool)> = vec![
        (
            "no_skill_md",
            Zip::new().file("readme.md", b"x").finish(),
            |e| matches!(e, InstallError::NoSkillMd),
        ),
        (
            "two_skill_md",
            Zip::new()
                .skill()
                .file("inner/SKILL.md", SKILL_MD)
                .finish(),
            |e| matches!(e, InstallError::NestedSkillMd { .. }),
        ),
        (
            "two_roots",
            Zip::new()
                .file("A/SKILL.md", SKILL_MD)
                .file("B/x.md", b"x")
                .finish(),
            |e| matches!(e, InstallError::MixedRoots { .. }),
        ),
    ];
    for (tag, bytes, matches) in cases {
        let sand = Sandbox::new(tag);
        let errors = sand
            .unpack(&bytes)
            .err()
            .unwrap_or_else(|| panic!("{tag}: expected a refusal"));
        assert!(
            has_error(&errors, |e| matches(e)),
            "{tag}: {errors:?}"
        );
    }
}

#[test]
fn empty_directories_are_recreated_and_a_root_package_may_hold_folders() {
    // 只有目录条目的包：目录要建出来，别静默丢掉结构
    let sand = Sandbox::new("empty_dir");
    let bytes = Zip::new().skill().dir("assets/").finish();
    sand.unpack(&bytes).expect("a directory entry is not a violation");
    assert!(sand.staged().join("assets").is_dir(), "the folder must exist");

    // P4a 缺陷的回归位：根上有 SKILL.md + 同级目录，是一个正常形状
    let sand = Sandbox::new("root_with_folder");
    let bytes = Zip::new()
        .skill()
        .file("assets/notes.txt", b"hi")
        .finish();
    sand.unpack(&bytes).expect("a root package with a sibling folder is one skill");
    assert!(sand.staged().join("assets/notes.txt").is_file());
}

// ---------------------------------------------------------------- 大小与比值

#[test]
fn entry_count_cap_refuses_before_writing() {
    let mut zip = Zip::new().skill();
    for index in 0..200 {
        zip = zip.file(&format!("f{index}.txt"), b"x");
    }
    let bytes = zip.finish();

    let sand = Sandbox::new("count");
    let errors = sand.unpack(&bytes).err().expect("201 entries must be refused");
    assert!(
        has_error(&errors, |e| matches!(
            e,
            InstallError::TooManyEntries { count, limit } if *count == 201 && *limit == 200
        )),
        "{errors:?}"
    );
    assert!(!sand.staged().exists(), "the cap fires before any write");
}

#[test]
fn a_lying_size_header_cannot_smuggle_more_than_the_per_file_cap() {
    // 11 MiB 真内容（压不动的图案数据），表上把展开大小写成 1 KiB：写前筛查用的
    // 就是表上的数，会放过它 —— 只有"边写边计数"拦得住。
    let mut bytes = deflated_zip("big.bin", 11 * 1024 * 1024, false);
    patch_expanded_size(&mut bytes, "big.bin", 1024);

    let sand = Sandbox::new("lying_sizes");
    let errors = sand
        .unpack(&bytes)
        .err()
        .expect("actual bytes must beat the declared sizes");
    assert!(
        has_error(&errors, |e| matches!(
            e,
            InstallError::FileTooLarge { bytes, limit, .. }
                if *bytes > *limit && *limit == 10 * 1024 * 1024
        )),
        "{errors:?}"
    );
    assert!(
        !sand.staged().exists(),
        "a package stopped mid-write leaves nothing behind"
    );
}

#[test]
fn a_small_file_under_a_lying_header_still_unpacks() {
    // 正对照：上面那条不是"表上数字不对就一律拒"。1 KiB 的真内容、表上写 1 字节，
    // 照样要能解出来并逐字节一致。
    let mut bytes = deflated_zip("small.bin", 1024, true);
    patch_expanded_size(&mut bytes, "small.bin", 1);

    let sand = Sandbox::new("lying_sizes_control");
    sand.unpack(&bytes)
        .expect("the streaming caps are absolute sizes, not a trust check on the table");
    assert_eq!(
        std::fs::read(sand.staged().join("small.bin")).expect("file must land"),
        vec![0u8; 1024],
        "content must survive byte-for-byte"
    );
}

#[test]
fn a_deflated_bomb_is_refused_by_ratio_before_expansion() {
    let mut w = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    w.start_file("SKILL.md", stored()).unwrap();
    w.write_all(SKILL_MD).unwrap();
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    w.start_file("blob.bin", deflated).unwrap();
    w.write_all(&vec![0u8; 4 * 1024 * 1024]).unwrap();
    let raw = w.finish().unwrap().into_inner();

    let sand = Sandbox::new("ratio");
    let errors = sand
        .unpack(&raw)
        .err()
        .expect("a 4 MiB-of-zeros deflate entry is a bomb shape");
    assert!(
        has_error(&errors, |e| matches!(e, InstallError::ZipBomb { .. })),
        "{errors:?}"
    );
    assert!(!sand.staged().exists());
}

// ---------------------------------------------------------------- 落位

#[test]
fn install_names_the_skill_and_refuses_to_overwrite_or_resurrect() {
    let sand = Sandbox::new("install_naming");
    let skills = sand.skills_dir();
    std::fs::create_dir_all(&skills).unwrap();
    let mgr = SkillManager::new(skills.to_str().unwrap());

    let top_folder = Zip::new()
        .file("Triage/SKILL.md", b"---\nname: Triage\ndescription: d\n---\n# t\n")
        .file("Triage/assets/n.txt", b"hi")
        .finish();
    let outcome = mgr
        .install_from_archive(&top_folder, None, Some("https://example.test/t.zip"))
        .expect("a top-folder package installs under that folder's name");
    assert_eq!(outcome.dir, skills.join("Triage"), "{outcome:?}");
    assert!(skills.join("Triage/assets/n.txt").is_file());

    // 同名再装：明确拒绝，不覆盖用户已有的技能
    let err = mgr
        .install_from_archive(&top_folder, None, Some("https://example.test/t.zip"))
        .expect_err("installing over an existing skill must be refused");
    assert!(err.contains("already exists"), "{err}");

    // 没有顶层目录也没有显式名字时，用 frontmatter 的 name 落位
    let flat = Zip::new()
        .file("SKILL.md", b"---\nname: TriageCopy\ndescription: d\n---\n# c\n")
        .finish();
    let renamed = mgr
        .install_from_archive(&flat, None, Some("https://example.test/c.zip"))
        .expect("a root package takes its name from its frontmatter");
    assert_eq!(renamed.dir, skills.join("TriageCopy"));

    // 回收站里有同名时不许静默复活（按技能名删，再按同一个名字装）
    mgr.delete_skill("TriageCopy").expect("moved to recycle bin");
    let err = mgr
        .install_from_archive(&flat, Some("TriageCopy"), None)
        .expect_err("a trashed name must not be silently resurrected");
    assert!(err.contains("_deleted"), "{err}");
}

#[test]
fn a_single_file_install_takes_its_name_from_fenced_frontmatter() {
    // 同一类缺陷的另一半：P4a 的 `install_single_file` 在没给名字时也要读 frontmatter
    // 拿名字，而它拿到的是**整份文件**（带 `---` 围栏），不是剥出来的那段。
    let sand = Sandbox::new("single_file_name");
    let skills = sand.skills_dir();
    std::fs::create_dir_all(&skills).unwrap();
    let mgr = SkillManager::new(skills.to_str().unwrap());

    let md = b"---\nname: Fenced\ndescription: d\n---\n# body\n";
    let outcome = mgr
        .install_single_file(md, None)
        .expect("a fenced SKILL.md must yield its name");
    assert_eq!(outcome.dir, skills.join("Fenced"), "{outcome:?}");
    assert!(skills.join("Fenced/SKILL.md").is_file());
}

#[test]
fn an_invalid_skill_md_lands_but_is_reported_not_registered() {
    // P4b 与 P1 的接缝：安装器判路径与大小，frontmatter 由 reload() 判。装完必须
    // "看得见被拒"，而不是静默少一个技能。缺 `description` 在 P1 里是明确裁决过的
    // Warn（会注册），缺 `name` 才是 Blocking，所以这里造的是缺 name。
    let sand = Sandbox::new("invalid_frontmatter");
    let skills = sand.skills_dir();
    std::fs::create_dir_all(&skills).unwrap();
    let mgr = SkillManager::new(skills.to_str().unwrap());

    let bytes = Zip::new()
        .file("Bad/SKILL.md", b"---\ndescription: no name here\n---\n# b\n")
        .finish();
    mgr.install_from_archive(&bytes, Some("Bad"), Some("https://example.test/bad.zip"))
        .expect("the package gate is about paths and sizes, not frontmatter");

    assert!(skills.join("Bad/SKILL.md").is_file(), "the tree landed");
    let names: Vec<String> = mgr.list().into_iter().map(|m| m.name).collect();
    assert!(
        !names.contains(&"Bad".to_string()),
        "a SKILL.md without a name must not be registered: {names:?}"
    );
    let report = FoxIR::skill::catalog_report(&mgr);
    let rejected = report["rejected"].as_array().expect("rejected is a list");
    assert_eq!(rejected.len(), 1, "the refusal must be counted: {report}");
    let findings = rejected[0]["findings"].to_string();
    assert!(
        findings.contains("MissingField") && findings.contains("\"name\""),
        "the report must say which field is missing: {findings}"
    );
}

#[test]
fn a_refused_archive_leaves_no_scratch_anywhere() {
    let sand = Sandbox::new("no_scratch");
    let skills = sand.skills_dir();
    std::fs::create_dir_all(&skills).unwrap();
    let mgr = SkillManager::new(skills.to_str().unwrap());

    let bytes = Zip::new().skill().symlink("evil.md", "/etc/passwd").finish();
    let err = mgr
        .install_from_archive(&bytes, None, None)
        .expect_err("a symlink entry refuses the package");
    assert!(err.contains("symlink"), "{err}");

    assert_eq!(names_in(&skills), Vec::<String>::new(), "skills/ must be empty");
    assert!(
        !sand.root.join(".skill-quarantine").exists(),
        "our own quarantine scratch must be cleaned up"
    );
    assert!(!sand.staged().exists());
}
