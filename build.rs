use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;

/// Escape a string for embedding as a Rust string literal.
fn escape_for_rust(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn embed_rules(out_dir: &str) {
    let dest_path = Path::new(out_dir).join("rules_embedded.rs");
    let mut f = fs::File::create(&dest_path).unwrap();

    let rules_dir = Path::new("rules");

    let mut entries: Vec<(String, String)> = Vec::new();

    if rules_dir.exists() {
        for entry in fs::read_dir(rules_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "yar" || ext == "yara") {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                let content = fs::read_to_string(&path).unwrap_or_default();
                entries.push((name, content));
            }
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    write!(f, "vec![").unwrap();
    for (name, content) in &entries {
        let escaped = escape_for_rust(content);
        let name_escaped = escape_for_rust(name);
        write!(f, "(\"{}\", \"{}\"),", name_escaped, escaped).unwrap();
    }
    write!(f, "]").unwrap();

    println!("cargo:rerun-if-changed=rules");
}

fn embed_workspace_files(out_dir: &str) {
    let dest_path = Path::new(out_dir).join("embedded_files.rs");
    let mut f = fs::File::create(&dest_path).unwrap();

    let files = ["AGENTS.md", "SOUL.md", "TOOLS.md", "USER.md"];

    write!(f, "&[").unwrap();
    for name in &files {
        let path = Path::new(name);
        if path.exists() {
            let content = fs::read_to_string(path).unwrap_or_default();
            let escaped = escape_for_rust(&content);
            write!(f, "(\"{}\", \"{}\"),", name, escaped).unwrap();
        }
    }
    write!(f, "]").unwrap();

    for name in &files {
        println!("cargo:rerun-if-changed={}", name);
    }
}

/// Recursively collect (skill_name, relative_path, content) for every text
/// file under `dir`. Paths are relative to the skill directory (`skill_root`).
/// Non-UTF-8 files abort the build: builtin skills are compiled-in text
/// resources, not arbitrary binaries.
fn collect_skill_files(skill_root: &Path, dir: &Path, skill: &str, out: &mut Vec<(String, String, String)>) {
    let mut children: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read builtin skill dir {}: {}", dir.display(), e))
        .map(|entry| entry.unwrap().path())
        .collect();
    children.sort();
    for path in children {
        if path.is_dir() {
            collect_skill_files(skill_root, &path, skill, out);
        } else {
            let rel = path
                .strip_prefix(skill_root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let content = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("builtin skill resource {} must be UTF-8 text: {}", path.display(), e));
            out.push((skill.to_string(), rel, content));
        }
    }
}

fn embed_builtin_skills(out_dir: &str) {
    let dest_path = Path::new(out_dir).join("builtin_skills_embedded.rs");
    let mut f = fs::File::create(&dest_path).unwrap();

    let root = Path::new("builtin_skills");
    let mut entries: Vec<(String, String, String)> = Vec::new();

    if root.exists() {
        let mut skill_dirs: Vec<_> = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| p.is_dir())
            .collect();
        skill_dirs.sort();
        for dir in &skill_dirs {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            collect_skill_files(dir, dir, &name, &mut entries);
        }
    }

    // Stable order so the generated file is reproducible across builds.
    entries.sort();

    write!(f, "&[").unwrap();
    for (name, rel, content) in &entries {
        write!(
            f,
            "(\"{}\", \"{}\", \"{}\"),",
            escape_for_rust(name),
            escape_for_rust(rel),
            escape_for_rust(content)
        )
        .unwrap();
    }
    write!(f, "]").unwrap();

    println!("cargo:rerun-if-changed=builtin_skills");
    for (name, rel, _) in &entries {
        println!("cargo:rerun-if-changed=builtin_skills/{}/{}", name, rel);
    }
}

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    embed_rules(&out_dir);
    embed_workspace_files(&out_dir);
    embed_builtin_skills(&out_dir);

    // Embed Windows application icon
    if cfg!(target_os = "windows") {
        let icon_path = "FOXIR.ico";
        if Path::new(icon_path).exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon(icon_path);
            if let Err(e) = res.compile() {
                eprintln!("Failed to compile Windows resource: {}", e);
            }
            println!("cargo:rerun-if-changed={}", icon_path);
        }
    }
}
