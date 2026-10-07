//! P4 — 用户发起的安装入口（包内条目闸 + 布局闸 + 原子落位）。
//!
//! 这一层不碰网络：它只回答"这棵已解包的树能不能进 `skills/`"。隔离区在
//! `skills_dir` 之外，所以 `reload()` 的 `{skills}/**/SKILL.md` 扫不到它
//! （`src/skill/mod.rs` 的 dir_pattern）；只有过完这两道闸才允许同卷 `rename` 落位。

use std::fmt;

/// 一个待安装条目的最小描述（大小与类型，不碰内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrySpec {
    /// 包内相对路径，分隔符已归一为 `/`。
    pub rel: String,
    pub bytes: u64,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// 拒绝原因。每条都带"是哪一项、为什么"，让整包丢弃时报得出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    AbsolutePath { path: String },
    EscapesSkillDir { path: String },
    SymlinkEntry { path: String },
    ReservedPrefix { path: String, segment: String },
    ExecutablePayload { path: String, ext: String },
    TooManyEntries { count: usize, limit: usize },
    FileTooLarge { path: String, bytes: u64, limit: u64 },
    TotalTooLarge { bytes: u64, limit: u64 },
    NoSkillMd,
    NestedSkillMd { paths: Vec<String> },
    MixedRoots { roots: Vec<String> },
    /// A skill directory with that name already exists. Installing never
    /// overwrites: the caller must resolve the collision (version bump, or
    /// restore from the recycle bin) explicitly.
    NameTaken { dir: String },
    /// The staged tree itself could not be read or moved — an I/O fact, reported
    /// rather than swallowed.
    Unreadable { path: String, reason: String },
    /// P4b: an entry carries the encryption flag. FoxIR never prompts for a
    /// password, so such a package cannot be installed at all.
    Encrypted { path: String },
    /// P4b: the archive itself is not one we can read (split/spanned, truncated,
    /// or not a zip).
    UnsupportedArchive { reason: String },
    /// P4b: declared sizes imply a zip bomb, so the package is refused before
    /// anything is expanded.
    ZipBomb { path: String, expanded: u64, compressed: u64 },
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::AbsolutePath { path } => write!(f, "absolute path in package: {path}"),
            InstallError::EscapesSkillDir { path } => write!(f, "path escapes the skill dir: {path}"),
            InstallError::SymlinkEntry { path } => write!(f, "symlink entries are not installed: {path}"),
            InstallError::ReservedPrefix { path, segment } => {
                write!(f, "reserved top-level segment '{segment}' in package path: {path}")
            }
            InstallError::ExecutablePayload { path, ext } => {
                write!(f, "executable payload ({ext}) is not installed: {path}")
            }
            InstallError::TooManyEntries { count, limit } => {
                write!(f, "{count} entries exceeds the limit of {limit}")
            }
            InstallError::FileTooLarge { path, bytes, limit } => {
                write!(f, "{path} is {bytes} bytes, over the per-file limit of {limit}")
            }
            InstallError::TotalTooLarge { bytes, limit } => {
                write!(f, "expanded size {bytes} bytes exceeds the limit of {limit}")
            }
            InstallError::NoSkillMd => write!(f, "no SKILL.md in the package"),
            InstallError::NestedSkillMd { paths } => {
                write!(f, "expected exactly one SKILL.md, found {}", paths.len())
            }
            InstallError::MixedRoots { roots } => {
                write!(f, "package mixes several top-level directories: {}", roots.join(", "))
            }
            InstallError::NameTaken { dir } => {
                write!(f, "a skill directory already exists at {dir}; resolve the name first")
            }
            InstallError::Unreadable { path, reason } => {
                write!(f, "cannot handle {path}: {reason}")
            }
            InstallError::Encrypted { path } => {
                write!(f, "encrypted entries are not installed: {path}")
            }
            InstallError::UnsupportedArchive { reason } => {
                write!(f, "unsupported archive: {reason}")
            }
            InstallError::ZipBomb { path, expanded, compressed } => write!(
                f,
                "{path} expands {compressed} bytes to {expanded}, over the ratio limit"
            ),
        }
    }
}

/// 包内路径的保留区：`skills/` 下 `_` 开头是 FoxIR 自己的回收站与台账目录。
const RESERVED_PREFIX: char = '_';

/// Per-file expanded cap, total cap and entry cap (spec §5.2).
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 200;

/// A skill directory has no reason to carry executables; anything that could run
/// is refused rather than "installed and never executed".
const EXECUTABLE_EXTS: [&str; 8] = [
    "exe", "dll", "scr", "bat", "cmd", "ps1", "vbs", "lnk",
];

/// What the gate accepted: which root to strip, and the entries to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// `None` = `SKILL.md` at the package root; `Some(dir)` = one top-level folder.
    pub root: Option<String>,
    /// Entries with the root prefix removed, ready to write into the skill dir.
    pub staged: Vec<EntrySpec>,
}

fn is_absolute(rel: &str) -> bool {
    rel.starts_with('/') || rel.as_bytes().get(1) == Some(&b':')
}

fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn extension(rel: &str) -> Option<&str> {
    file_name(rel).rsplit_once('.').map(|(_, ext)| ext)
}

/// Screen a staged package: entry paths, sizes, and the "exactly one SKILL.md"
/// shape. Returns every violation at once so a rejected package can be reported
/// in one pass instead of one error per retry.
pub fn plan_entries(entries: &[EntrySpec]) -> Result<Layout, Vec<InstallError>> {
    let mut errors: Vec<InstallError> = Vec::new();

    if entries.len() > MAX_ENTRIES {
        errors.push(InstallError::TooManyEntries {
            count: entries.len(),
            limit: MAX_ENTRIES,
        });
    }
    let total: u64 = entries.iter().map(|entry| entry.bytes).sum();
    if total > MAX_TOTAL_BYTES {
        errors.push(InstallError::TotalTooLarge {
            bytes: total,
            limit: MAX_TOTAL_BYTES,
        });
    }

    let mut skill_md: Vec<String> = Vec::new();
    for entry in entries {
        let rel = entry.rel.replace('\\', "/");
        if is_absolute(&rel) {
            errors.push(InstallError::AbsolutePath { path: rel });
            continue;
        }
        let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        if segments.iter().any(|segment| *segment == "..") {
            errors.push(InstallError::EscapesSkillDir { path: rel });
            continue;
        }
        if segments.first().map(|segment| segment.starts_with(RESERVED_PREFIX)) == Some(true) {
            let segment = segments[0].to_string();
            errors.push(InstallError::ReservedPrefix { path: rel, segment });
            continue;
        }
        if entry.kind == EntryKind::Symlink {
            errors.push(InstallError::SymlinkEntry { path: rel });
            continue;
        }
        if extension(&rel).map(|ext| EXECUTABLE_EXTS.contains(&ext.to_lowercase().as_str()))
            == Some(true)
        {
            let ext = extension(&rel).unwrap_or_default().to_string();
            errors.push(InstallError::ExecutablePayload { path: rel.clone(), ext });
            continue;
        }
        if entry.bytes > MAX_FILE_BYTES {
            errors.push(InstallError::FileTooLarge {
                path: rel.clone(),
                bytes: entry.bytes,
                limit: MAX_FILE_BYTES,
            });
        }
        if file_name(&rel).eq_ignore_ascii_case("SKILL.md") {
            skill_md.push(rel);
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    match skill_md.len() {
        0 => return Err(vec![InstallError::NoSkillMd]),
        count if count > 1 => {
            return Err(vec![InstallError::NestedSkillMd { paths: skill_md }])
        }
        _ => {}
    }

    let skill_md = &skill_md[0];
    let root = match skill_md.split_once('/') {
        Some((head, _)) => Some(head.to_string()),
        None => None,
    };

    // Everything must live under that same root: a package that also carries a
    // second top-level directory is not one skill. With `SKILL.md` at the package
    // root there is no root to compare against — every other entry, file or
    // folder, is a child of that package.
    let mut roots: Vec<String> = Vec::new();
    if root.is_some() {
        for entry in entries {
            let rel = entry.rel.replace('\\', "/");
            let head = match rel.split_once('/') {
                Some((head, _)) => head,
                None => "",
            };
            if !head.is_empty() && !roots.iter().any(|known| known == head) {
                roots.push(head.to_string());
            }
        }
    }
    if let Some(root) = &root {
        roots.retain(|candidate| candidate != root);
    }
    if !roots.is_empty() {
        let mut mixed = roots;
        if let Some(root) = &root {
            mixed.insert(0, root.clone());
        }
        return Err(vec![InstallError::MixedRoots { roots: mixed }]);
    }

    let staged = entries
        .iter()
        .filter_map(|entry| {
            let rel = entry.rel.replace('\\', "/");
            let trimmed = match &root {
                Some(dir) => {
                    let prefix = format!("{dir}/");
                    if rel == *dir {
                        return None; // the root folder itself is implied
                    }
                    rel.strip_prefix(&prefix)?.to_string()
                }
                None => rel,
            };
            Some(EntrySpec {
                rel: trimmed,
                bytes: entry.bytes,
                kind: entry.kind,
            })
        })
        .collect();

    Ok(Layout { root, staged })
}

/// What landed: where, under which name, and the content it was bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    pub skill_name: String,
    pub dir: std::path::PathBuf,
    pub skill_md_hash: String,
}

/// Describe a staged tree as package entries (sizes and kinds only).
pub fn walk_staged(root: &std::path::Path) -> Result<Vec<EntrySpec>, Vec<InstallError>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = std::fs::read_dir(&dir)
            .map_err(|e| vec![InstallError::Unreadable { path: dir.display().to_string(), reason: e.to_string() }])?;
        for entry in read {
            let entry = entry.map_err(|e| {
                vec![InstallError::Unreadable { path: dir.display().to_string(), reason: e.to_string() }]
            })?;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .map_err(|_| {
                    vec![InstallError::Unreadable {
                        path: path.display().to_string(),
                        reason: "not under the staging root".to_string(),
                    }]
                })?
                .to_string_lossy()
                .replace('\\', "/");
            let meta = std::fs::symlink_metadata(&path).map_err(|e| {
                vec![InstallError::Unreadable { path: path.display().to_string(), reason: e.to_string() }]
            })?;
            let kind = if meta.file_type().is_symlink() {
                EntryKind::Symlink
            } else if meta.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            out.push(EntrySpec { rel, bytes: meta.len(), kind });
            if meta.is_dir() && !meta.file_type().is_symlink() {
                stack.push(path);
            }
        }
    }
    Ok(out)
}

/// Where a package came from, written next to it once it lands. `Default` is
/// today's folder-import shape: local provenance, nothing to hash.
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    pub source: Option<crate::skill::schema::Source>,
    pub url: Option<String>,
    pub package_sha256: Option<String>,
}

/// Move a validated, already-staged skill folder into `skills_dir`.
///
/// Nothing inside `skills_dir` is created before the package is screened, and the
/// move itself is a same-volume `rename`, so there is no moment where a partial
/// or unvalidated tree is discoverable by `reload()`'s `**/SKILL.md` glob. A
/// refused install leaves the staging tree in place for the caller to trash.
pub fn install_staged(
    skills_dir: &std::path::Path,
    staging_root: &std::path::Path,
    name: &str,
) -> Result<InstallOutcome, Vec<InstallError>> {
    install_staged_with(skills_dir, staging_root, name, Provenance::default())
}

/// The same landing step, recording where the package came from. An archive
/// install has a package to hash; a folder import does not, and says so with
/// `package_sha256: None` rather than a guessed value.
pub fn install_staged_with(
    skills_dir: &std::path::Path,
    staging_root: &std::path::Path,
    name: &str,
    provenance: Provenance,
) -> Result<InstallOutcome, Vec<InstallError>> {
    let entries = walk_staged(staging_root)?;
    let layout = plan_entries(&entries)?;

    let source = match &layout.root {
        Some(dir) => staging_root.join(dir),
        None => staging_root.to_path_buf(),
    };
    let dir_name = super::sanitize_dir_name(name);
    if dir_name.is_empty() {
        return Err(vec![InstallError::Unreadable {
            path: name.to_string(),
            reason: "skill name has no usable characters for a directory".to_string(),
        }]);
    }
    let target = skills_dir.join(&dir_name);
    if target.exists() {
        return Err(vec![InstallError::NameTaken { dir: dir_name }]);
    }

    let skill_md = std::fs::read(source.join("SKILL.md")).map_err(|e| {
        vec![InstallError::Unreadable {
            path: source.join("SKILL.md").display().to_string(),
            reason: e.to_string(),
        }]
    })?;
    let hash = crate::skill::schema::skill_md_hash(&skill_md);
    let manifest = crate::skill::schema::SourceManifest {
        source: provenance.source.unwrap_or_default(),
        url: provenance.url,
        package_sha256: provenance.package_sha256,
        ..crate::skill::schema::SourceManifest::local(&skill_md)
    };
    crate::skill::schema::write_manifest(&source, &manifest).map_err(|e| {
        vec![InstallError::Unreadable {
            path: source.display().to_string(),
            reason: e,
        }]
    })?;

    std::fs::create_dir_all(skills_dir)
        .map_err(|e| vec![InstallError::Unreadable { path: skills_dir.display().to_string(), reason: e.to_string() }])?;
    std::fs::rename(&source, &target).map_err(|e| {
        vec![InstallError::Unreadable {
            path: format!("{} -> {}", source.display(), target.display()),
            reason: e.to_string(),
        }]
    })?;

    Ok(InstallOutcome {
        skill_name: name.to_string(),
        dir: target,
        skill_md_hash: hash,
    })
}

/// Exact media types accepted for a single-file install. A whitelist, and nothing
/// is sniffed; parameters such as `; charset=utf-8` are stripped because that is
/// HTTP semantics, not content guessing.
const SINGLE_FILE_TYPES: [&str; 3] = ["text/markdown", "text/plain", "application/octet-stream"];

/// P4b: media types accepted for an archive install (spec §4.3 step 3). A separate
/// list on purpose — `text/markdown` must never route into the unpacker, and
/// `application/zip` must never be written out as a `SKILL.md`.
const ARCHIVE_TYPES: [&str; 3] = [
    "application/zip",
    "application/x-zip-compressed",
    "application/octet-stream",
];

/// Which of the two install paths a fetched payload belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageShape {
    SingleFile,
    Archive,
}

/// The zip local file header is the only evidence that routes to `Archive`. An
/// end-of-central-directory alone (`PK\x05\x06`) carries no entries, so it is not
/// a package and goes to the single-file path, where it will be refused.
pub fn package_shape(bytes: &[u8]) -> PackageShape {
    if bytes.starts_with(b"PK\x03\x04") {
        PackageShape::Archive
    } else {
        PackageShape::SingleFile
    }
}

/// Strip parameters and lowercase: `text/markdown; charset=utf-8` -> `text/markdown`.
fn media_type(header: &str) -> String {
    header
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// Media type whitelist for a single-file install.
pub fn content_type_ok(header: &str) -> bool {
    let media = media_type(header);
    !media.is_empty() && SINGLE_FILE_TYPES.contains(&media.as_str())
}

/// Media type whitelist for an archive install.
pub fn archive_content_type_ok(header: &str) -> bool {
    let media = media_type(header);
    !media.is_empty() && ARCHIVE_TYPES.contains(&media.as_str())
}

/// True for every address a download must never reach: loopback, the RFC1918
/// ranges, link-local (including the cloud metadata address), unique-local,
/// unspecified, multicast and broadcast.
pub fn ip_is_denied(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (first & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (first & 0xff00) == 0xff00 // ff00::/8 multicast
        }
    }
}

/// Render a rejection list as one message (for callers whose error type is text).
pub fn errors_text(errors: &[InstallError]) -> String {
    errors
        .iter()
        .map(InstallError::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Recursively copy a package into the staging area. Symlinks are never followed:
/// a rejected entry is reported by the gate, not silently copied.
pub fn copy_tree(source: &std::path::Path, dest: &std::path::Path) -> Result<(), InstallError> {
    std::fs::create_dir_all(dest).map_err(|e| InstallError::Unreadable {
        path: dest.display().to_string(),
        reason: e.to_string(),
    })?;
    for entry in std::fs::read_dir(source).map_err(|e| InstallError::Unreadable {
        path: source.display().to_string(),
        reason: e.to_string(),
    })? {
        let entry = entry.map_err(|e| InstallError::Unreadable {
            path: source.display().to_string(),
            reason: e.to_string(),
        })?;
        let file_type = entry.file_type().map_err(|e| InstallError::Unreadable {
            path: entry.path().display().to_string(),
            reason: e.to_string(),
        })?;
        let target = dest.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(InstallError::SymlinkEntry {
                path: entry.path().display().to_string(),
            });
        }
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).map_err(|e| InstallError::Unreadable {
                path: entry.path().display().to_string(),
                reason: e.to_string(),
            })?;
        }
    }
    Ok(())
}

/// Cap for a single fetched file.
pub const MAX_DOWNLOAD_BYTES: u64 = 8 * 1024 * 1024;

/// Split `(host, port)` out of an `https` URL, refusing anything else.
pub fn host_and_port(url: &str) -> Result<(String, u16), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("only https urls are fetched, got: {url}"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((head, tail))
            if !tail.is_empty() && (head.contains('.') || head.contains(':')) =>
        {
            let head = head.trim_matches(['[', ']']);
            (head.to_string(), tail.parse::<u16>().unwrap_or(443))
        }
        _ => (authority.trim_matches(['[', ']']).to_string(), 443),
    };
    if host.is_empty() {
        return Err(format!("url has no host: {url}"));
    }
    Ok((host, port))
}

/// The SSRF guard: loopback, RFC1918, link-local (the cloud metadata address is
/// `169.254.169.254`), unique-local and friends are never fetched.
fn deny_private_target(host: &str, port: u16) -> Result<(), String> {
    if host.eq_ignore_ascii_case("localhost") {
        return Err(format!("refusing to fetch from the private address {host}"));
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip_is_denied(&ip) {
            return Err(format!("refusing to fetch from the private address {ip}"));
        }
        return Ok(());
    }
    use std::net::ToSocketAddrs;
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?;
    for addr in addrs {
        let ip = addr.ip();
        if ip_is_denied(&ip) {
            return Err(format!(
                "refusing to fetch from the private address {ip} (host {host})"
            ));
        }
    }
    Ok(())
}

/// Fetch a package and report which install path it belongs to. The declared
/// media type decides whether the response is accepted at all; the bytes decide
/// the shape, because `application/octet-stream` is on both whitelists.
///
/// Residual risk, stated rather than hidden: resolution is checked before the
/// request, so a DNS name that answers differently on the second lookup
/// (rebinding) can still move between check and connect. Callers that need that
/// closed should pin the resolved address on the client.
pub async fn download_package(url: &str) -> Result<(Vec<u8>, PackageShape), String> {
    let bytes = fetch_capped(url, "package").await?;
    let shape = package_shape(&bytes);
    Ok((bytes, shape))
}

async fn fetch_capped(url: &str, what: &str) -> Result<Vec<u8>, String> {
    let (host, port) = host_and_port(url)?;
    deny_private_target(&host, port)?;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let next = attempt.url().as_str();
            match host_and_port(next).and_then(|(h, p)| deny_private_target(&h, p).map(|_| (h, p)))
            {
                Ok(_) => attempt.follow(),
                Err(reason) => {
                    attempt.error(std::io::Error::other(reason))
                }
            }
        }))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("cannot build the http client: {e}"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    let final_url = response.url().as_str().to_string();
    let (final_host, final_port) = host_and_port(&final_url)?;
    deny_private_target(&final_host, final_port)?;

    let media = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(media_type)
        .unwrap_or_default();
    // The union of the two whitelists, derived from them rather than a third
    // copy: `octet-stream` sits on both, which is why the shape is decided by
    // the bytes below rather than by the header.
    if !(content_type_ok(&media) || archive_content_type_ok(&media)) {
        return Err(format!("unexpected content type '{media}'; refusing to install the {what}"));
    }
    if let Some(len) = response.headers().get(reqwest::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
        if len > MAX_DOWNLOAD_BYTES {
            return Err(format!("response is {len} bytes, over the {} byte cap", MAX_DOWNLOAD_BYTES));
        }
    }

    let mut collected: Vec<u8> = Vec::new();
    let stream = response.bytes_stream();
    use futures::StreamExt;
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download interrupted: {e}"))?;
        if collected.len() + chunk.len() > MAX_DOWNLOAD_BYTES as usize {
            return Err(format!(
                "response exceeded the {} byte cap",
                MAX_DOWNLOAD_BYTES
            ));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected)
}

#[cfg(test)]
mod tests {
    use super::{EntryKind::*, *};

    fn file(rel: &str, bytes: u64) -> EntrySpec {
        EntrySpec {
            rel: rel.to_string(),
            bytes,
            kind: File,
        }
    }

    fn temp_install_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rs_install_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn package_entry_paths_are_screened_segment_by_segment() {
        // 每一条都要被拒；"包内相对路径"这个限定很重要 —— 它管的是包里的段，
        // 不是最终落位的目录名（`_foo` 作为技能名是 Warn，不是这里的事）。
        for bad in [
            "/etc/passwd",
            "C:\\Windows\\win.ini",
            "../outside.txt",
            "a/../../outside.txt",
            "_deleted/KeepMe/SKILL.md",
            "_audit/grants.jsonl",
        ] {
            let errors = plan_entries(&[file("SKILL.md", 10), file(bad, 10)])
                .expect_err(&format!("{bad} must be rejected"));
            assert!(!errors.is_empty(), "{bad} produced no error");
            assert!(
                errors.iter().any(|e| e.to_string().contains(bad)
                    || matches!(
                        e,
                        InstallError::AbsolutePath { .. }
                            | InstallError::EscapesSkillDir { .. }
                            | InstallError::ReservedPrefix { .. }
                    )),
                "{bad}: unexpected rejection {errors:?}"
            );
        }

        // 正对照：常规形状必须过
        assert!(
            plan_entries(&[file("SKILL.md", 10), file("reference.md", 20)]).is_ok(),
            "a plain skill folder must be accepted"
        );
    }

    #[test]
    fn executable_and_symlink_entries_are_never_installed() {
        for ext in ["exe", "dll", "scr", "bat", "cmd", "ps1", "vbs", "lnk"] {
            let errors = plan_entries(&[
                file("SKILL.md", 10),
                file(&format!("tools/helper.{ext}"), 10),
            ])
            .expect_err(&format!(".{ext} must be rejected"));
            assert!(
                errors
                    .iter()
                    .any(|e| matches!(e, InstallError::ExecutablePayload { .. })),
                ".{ext}: {errors:?}"
            );
        }

        let errors = plan_entries(&[
            file("SKILL.md", 10),
            EntrySpec {
                rel: "linked".to_string(),
                bytes: 0,
                kind: Symlink,
            },
        ])
        .expect_err("symlinks must be rejected");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, InstallError::SymlinkEntry { .. })),
            "{errors:?}"
        );
    }

    #[test]
    fn exactly_one_skill_md_at_root_or_under_a_single_top_folder() {
        // 根上有一份：接受，且不带子目录前缀
        let layout = plan_entries(&[file("SKILL.md", 10), file("reference.md", 5)])
            .expect("root layout accepted");
        assert_eq!(layout.root, None, "{layout:?}");

        // 一层子目录下有一份：接受，root 记下来
        let layout = plan_entries(&[file("MySkill/SKILL.md", 10), file("MySkill/x.md", 5)])
            .expect("single-folder layout accepted");
        assert_eq!(layout.root.as_deref(), Some("MySkill"));

        // 两份 SKILL.md（含"一包多技能"）：整包拒
        let errors = plan_entries(&[
            file("SKILL.md", 10),
            file("Nested/SKILL.md", 10),
        ])
        .expect_err("a second SKILL.md must be rejected");
        assert!(
            matches!(&errors[..], [InstallError::NestedSkillMd { paths }] if paths.len() == 2),
            "{errors:?}"
        );

        // 没有 SKILL.md
        let errors = plan_entries(&[file("readme.md", 10)]).expect_err("needs a SKILL.md");
        assert!(matches!(&errors[..], [InstallError::NoSkillMd]), "{errors:?}");

        // 混了两个顶层目录：不是一个技能
        let errors = plan_entries(&[file("A/SKILL.md", 10), file("B/x.md", 10)])
            .expect_err("mixed roots must be rejected");
        assert!(
            matches!(&errors[..], [InstallError::MixedRoots { roots }] if roots.len() == 2),
            "{errors:?}"
        );
    }

    /// `SKILL.md` 在包根时，同级的 `assets/` 是这个技能的子目录，不是"第二个顶层根"。
    /// P4b 的正对照夹具第一次暴露这条不对称：同级**文件**放行、同级**目录**被拒。
    /// P4b 的白名单是另一张表：zip 的类型不能拿去当单文件装，反之亦然。
    #[test]
    fn archive_and_single_file_content_types_stay_on_separate_lists() {
        assert!(archive_content_type_ok("application/zip"));
        assert!(archive_content_type_ok("application/x-zip-compressed"));
        assert!(
            archive_content_type_ok("application/octet-stream; charset=binary"),
            "parameters are HTTP semantics, not content guessing"
        );
        assert!(!archive_content_type_ok("text/markdown"), "a markdown type is not an archive");
        assert!(!archive_content_type_ok(""));
        assert!(
            !content_type_ok("application/zip"),
            "and the single-file list must not accept an archive type"
        );
    }

    /// 走哪条安装路径由本地头签名决定，不由服务器声明决定：octet-stream 两种形状
    /// 都可能，签名才是"这堆字节里有没有内容"的证据。
    #[test]
    fn only_a_zip_local_header_routes_to_the_archive_shape() {
        assert_eq!(package_shape(b"PK\x03\x04\x14\x00"), PackageShape::Archive);
        assert_eq!(package_shape(b"---\nname: x\n---\n"), PackageShape::SingleFile);
        assert_eq!(
            package_shape(b"PK\x05\x06\x00\x00\x00\x00"),
            PackageShape::SingleFile,
            "an end-of-central-directory alone carries no entries; it is not an installable package"
        );
    }

    #[test]
    fn a_root_skill_md_with_sibling_directories_is_one_package() {
        let layout = plan_entries(&[
            file("SKILL.md", 10),
            file("assets/notes.txt", 20),
            file("reference.md", 30),
        ])
        .expect("a root SKILL.md package that also carries a folder is a normal shape");
        assert_eq!(layout.root, None, "nothing to strip from a root package");
        assert_eq!(layout.staged.len(), 3, "{:?}", layout.staged);
    }

    /// 门在网络上碰之前就拒掉私网目标（端口 1 是开不着的：如果守卫没生效，
    /// 报错会是"连不上"而不是"拒绝访问私网地址"）。
    #[tokio::test]
    async fn downloading_refuses_a_private_target_before_touching_the_network() {
        let err = download_package("http://127.0.0.1:1/SKILL.md")
            .await
            .expect_err("plaintext must be refused");
        assert!(err.contains("https"), "{err}");

        let err = download_package("https://127.0.0.1:1/SKILL.md")
            .await
            .expect_err("loopback must be refused");
        assert!(
            err.contains("private") && !err.to_lowercase().contains("refused to connect"),
            "must be a policy refusal, not a connect failure: {err}"
        );
    }

    #[test]
    fn a_staged_folder_lands_atomically_and_never_clobbers() {
        let root = temp_install_dir("land");
        let skills = root.join("skills");
        let staging = root.join("staging/Good");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(staging.join("SKILL.md"), "---\nname: Good\ndescription: d\n---\n# G\n").unwrap();
        std::fs::write(staging.join("reference.md"), "ref").unwrap();

        let outcome = install_staged(&skills, &staging, "Good").expect("install accepted");
        assert!(skills.join("Good/SKILL.md").is_file());
        assert!(skills.join("Good/reference.md").is_file(), "companion files move with it");
        assert_eq!(outcome.dir, skills.join("Good"));
        let manifest = crate::skill::schema::read_manifest(&skills.join("Good"))
            .expect("manifest readable")
            .expect("manifest written");
        assert_eq!(manifest.source, crate::skill::schema::Source::Local);
        assert!(
            manifest.package_sha256.is_none(),
            "a folder import has no package to hash: {manifest:?}"
        );
        assert!(manifest.skill_md_hash.is_some());
        assert!(
            !root.join("staging/Good").exists(),
            "the staged tree must be consumed, not left behind"
        );

        // 同名不覆盖：拒绝，并且原有内容一个字节都不动
        std::fs::create_dir_all(root.join("staging2/Good")).unwrap();
        std::fs::write(root.join("staging2/Good/SKILL.md"), "# different\n").unwrap();
        let err = install_staged(&skills, &root.join("staging2/Good"), "Good")
            .expect_err("an existing skill dir must not be replaced silently");
        assert!(err.iter().any(|e| matches!(e, InstallError::NameTaken { .. })), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(skills.join("Good/SKILL.md")).unwrap(),
            "---\nname: Good\ndescription: d\n---\n# G\n",
            "the refused install must not touch the installed skill"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rejected_package_leaves_nothing_inside_the_skills_dir() {
        let root = temp_install_dir("reject");
        let skills = root.join("skills");
        let staging = root.join("staging/Bad");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("SKILL.md"), "---\nname: Bad\ndescription: d\n---\n# B\n").unwrap();
        // 包内想写进保留区（回收站/台账），整包拒
        std::fs::create_dir_all(staging.join("_audit")).unwrap();
        std::fs::write(staging.join("_audit/grants.jsonl"), "tamper").unwrap();

        let err = install_staged(&skills, &staging, "Bad").expect_err("reserved prefix rejected");
        assert!(
            err.iter().any(|e| matches!(e, InstallError::ReservedPrefix { .. })),
            "{err:?}"
        );
        assert_eq!(
            std::fs::read_dir(&skills).map(|d| d.count()).unwrap_or(0),
            0,
            "skills/ must be untouched by a rejected package"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn private_and_link_local_targets_are_denied() {
        // https 只挡住明文，挡不住内网。用户发起的下载同样要拒私网与元数据地址，
        // 否则一个"装技能"的动作就是 SSRF 探针。
        for denied in [
            "10.0.0.1",
            "172.16.5.4",
            "192.168.1.10",
            "127.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::",
        ] {
            let ip: std::net::IpAddr = denied.parse().unwrap();
            assert!(ip_is_denied(&ip), "{denied} must be denied");
        }
        for allowed in ["93.184.216.34", "8.8.8.8", "2606:4700:4700::1111"] {
            let ip: std::net::IpAddr = allowed.parse().unwrap();
            assert!(!ip_is_denied(&ip), "{allowed} is a public address");
        }
    }

    #[test]
    fn only_https_and_the_single_file_content_types_pass() {
        // 同一个判定只有一处实现：host_and_port 就是 https 闸（重定向每一跳也走它）
        assert!(host_and_port("https://example.com/SKILL.md").is_ok());
        assert!(host_and_port("http://example.com/SKILL.md").is_err(), "no plaintext");
        assert!(host_and_port("file:///C:/etc/passwd").is_err());
        assert!(host_and_port("https://").is_err(), "no host");

        for ok in [
            "text/markdown",
            "text/plain",
            "application/octet-stream",
            "TEXT/MARKDOWN; charset=utf-8", // 参数按 HTTP 语义剥掉，不是 sniffing
        ] {
            assert!(content_type_ok(&ok), "{ok}");
        }
        for bad in [
            "application/x-msdownload",
            "text/html",
            "application/zip",
            "",
            "text/markdownx", // 近似名字不许混进来
            "; charset=utf-8",
        ] {
            assert!(
                !content_type_ok(bad),
                "{bad} must be refused without sniffing"
            );
        }
    }

    #[test]
    fn size_and_count_caps_are_enforced() {
        let errors = plan_entries(&[file("SKILL.md", MAX_FILE_BYTES + 1)])
            .expect_err("one oversized file is refused");
        assert!(
            matches!(&errors[..], [InstallError::FileTooLarge { .. }]),
            "{errors:?}"
        );

        let many: Vec<EntrySpec> = (0..=MAX_ENTRIES)
            .map(|i| file(&format!("f{i}.md"), 1))
            .chain(std::iter::once(file("SKILL.md", 1)))
            .collect();
        let errors = plan_entries(&many).expect_err("entry count is capped");
        assert!(
            matches!(&errors[..], [InstallError::TooManyEntries { .. }]),
            "{errors:?}"
        );

        // 三份各自不超单文件上限、加起来超总上限：只能报总上限
        let errors = plan_entries(&[
            file("SKILL.md", 10),
            file("a.bin", 9 * 1024 * 1024),
            file("b.bin", 9 * 1024 * 1024),
            file("c.bin", 9 * 1024 * 1024),
        ])
        .expect_err("total expanded size is capped");
        assert!(
            matches!(&errors[..], [InstallError::TotalTooLarge { .. }]),
            "{errors:?}"
        );
    }
}
