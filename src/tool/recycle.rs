//! Local safe-delete interceptor for shell_exec.
//!
//! `shell_exec` runs arbitrary PowerShell/CMD strings. We cannot safely rewrite
//! arbitrary shell, but a *direct, literal-path delete* (single statement, no
//! pipeline / chaining / wildcard / variable / encoding) is unambiguous enough to
//! redirect to the OS Recycle Bin via the `trash` crate instead of hard-deleting.
//!
//! `parse_delete_paths` returns `Some(paths)` only for those safe, literal cases;
//! anything ambiguous returns `None` so the caller falls back to normal execution
//! (which still goes through the delete permission gate + audit). To keep parity
//! with `file_delete`, path resolution happens against the tool working dir.

use crate::context::ToolContext;
use crate::error::AgentResult;
use serde_json::{json, Value};
use std::path::PathBuf;

/// PowerShell verbs that alias `Remove-Item`.
const PS_DELETE_VERBS: &[&str] = &["remove-item", "remove", "rm", "del", "erase", "ri"];
/// cmd file-delete verbs.
const CMD_FILE_VERBS: &[&str] = &["del", "erase"];
/// cmd directory-delete verbs.
const CMD_DIR_VERBS: &[&str] = &["rmdir", "rd"];

/// Analyze `command` and, when it is a direct, literal-path delete, return the
/// explicit path arguments (still quoted as in the original). Returns `None` for
/// anything that cannot be safely parsed or is not a delete.
pub fn parse_delete_paths(command: &str, shell: &str) -> Option<Vec<String>> {
    let cmd = command.trim();
    if cmd.is_empty() {
        return None;
    }

    // Reject any construct that makes path extraction unreliable:
    // pipeline, command chaining, redirects, sub-expressions, variables,
    // backticks, wildcards, and cmd `%var%` expansion.
    for c in [';', '|', '&', '>', '<', '`', '$', '*', '?', '(', ')'] {
        if cmd.contains(c) {
            return None;
        }
    }
    if cmd.contains('%') {
        return None;
    }

    let tokens = tokenize(cmd);
    if tokens.is_empty() {
        return None;
    }

    let is_ps = shell.eq_ignore_ascii_case("powershell");
    let verb = tokens[0].to_ascii_lowercase();
    let found = if is_ps {
        PS_DELETE_VERBS.contains(&verb.as_str())
    } else {
        CMD_FILE_VERBS.contains(&verb.as_str()) || CMD_DIR_VERBS.contains(&verb.as_str())
    };
    if !found {
        return None;
    }

    let flag_char = if is_ps { '-' } else { '/' };
    let mut paths: Vec<String> = Vec::new();
    // PowerShell `-LiteralPath <p>` / `-Path <p>` consumes the next token as ONE path.
    let mut expect_literal_next = false;
    let mut i = 1;
    while i < tokens.len() {
        let raw = tokens[i].trim();
        if raw.starts_with(flag_char) {
            let lower = raw.to_ascii_lowercase();
            if is_ps {
                match lower.as_str() {
                    "-literalpath" | "-path" => {
                        // next token is a single literal path
                        if i + 1 >= tokens.len() {
                            return None;
                        }
                        let p = unquote(tokens[i + 1].trim());
                        if p.is_empty() {
                            return None;
                        }
                        paths.push(p);
                        i += 2;
                        continue;
                    }
                    "-force" | "-recurse" | "-confirm" | "-whatif" | "-erroraction" | "-ea" => {
                        // harmless flags that do not change the path set semantics
                        i += 1;
                        continue;
                    }
                    _ => return None, // unknown flag -> bail to normal execution
                }
            } else {
                // cmd flags: /F /Q /S /A /P  (switches are single-token, no value)
                if matches!(lower.as_str(), "/f" | "/q" | "/s" | "/a" | "/p") {
                    i += 1;
                    continue;
                }
                return None;
            }
        }
        // a positional path token
        let p = unquote(raw);
        if p.is_empty() {
            return None;
        }
        paths.push(p);
        i += 1;
    }

    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// Move a list of already-resolved literal paths to the OS Recycle Bin and return
/// a result value. Never hard-deletes; if the recycle fails it errors out so the
/// raw (hard) shell command is NOT run.
pub fn recycle_paths(
    ctx: &ToolContext,
    shell: &str,
    command: &str,
    paths: &[String],
) -> AgentResult<Value> {
    let mut recycled: Vec<String> = Vec::new();
    for p in paths {
        let raw = PathBuf::from(p);
        let resolved = if raw.is_absolute() {
            raw
        } else {
            PathBuf::from(&ctx.working_dir).join(raw)
        };
        if !resolved.exists() {
            return Err(format!(
                "Cannot recycle '{}' (resolved to {}): path does not exist. Refusing to run the raw shell delete.",
                p,
                resolved.display()
            )
            .into());
        }
        trash::delete(&resolved).map_err(|e| {
            format!(
                "Failed to move '{}' to Recycle Bin: {}. Refusing to hard-delete recoverable data.",
                resolved.display(),
                e
            )
        })?;
        tracing::warn!(
            "[AUDIT] shell_exec delete redirected to Recycle Bin: {} | shell={} | command={}",
            resolved.display(),
            shell,
            command
        );
        recycled.push(resolved.to_string_lossy().replace('\\', "/"));
    }
    Ok(json!({
        "redirected_to_recycle_bin": true,
        "recycled": recycled,
        "note": "Shell delete was redirected to the OS Recycle Bin; no hard delete was performed."
    }))
}

/// Split whitespace while keeping single/double-quoted segments intact.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in s.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                    cur.push(ch);
                } else {
                    cur.push(ch);
                }
            }
            None => {
                if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                    cur.push(ch);
                } else if ch.is_whitespace() {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                } else {
                    cur.push(ch);
                }
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Strip one layer of matching surrounding quotes (single or double).
fn unquote(tok: &str) -> String {
    let t = tok.trim();
    if t.len() >= 2 {
        let bytes = t.as_bytes();
        let first = bytes[0] as char;
        let last = bytes[bytes.len() - 1] as char;
        if (first == '\'' && last == '\'') || (first == '"' && last == '"') {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(cmd: &str) -> Option<Vec<String>> {
        parse_delete_paths(cmd, "powershell")
    }
    fn cmd(cmd: &str) -> Option<Vec<String>> {
        parse_delete_paths(cmd, "cmd")
    }

    #[test]
    fn ps_basic() {
        assert_eq!(ps("Remove-Item C:\\Temp\\a.txt"), Some(vec!["C:\\Temp\\a.txt".into()]));
        assert_eq!(ps("Remove-Item C:\\Temp\\a.txt -Force"), Some(vec!["C:\\Temp\\a.txt".into()]));
        assert_eq!(ps("Remove-Item -LiteralPath 'C:\\My File.txt' -Force"), Some(vec!["C:\\My File.txt".into()]));
        assert_eq!(ps("Remove-Item -Path C:\\Temp\\a.txt -Recurse"), Some(vec!["C:\\Temp\\a.txt".into()]));
        assert_eq!(
            ps("rm C:\\a.txt C:\\b.log"),
            Some(vec!["C:\\a.txt".into(), "C:\\b.log".into()])
        );
    }

    #[test]
    fn cmd_basic() {
        assert_eq!(cmd("del C:\\Temp\\a.txt"), Some(vec!["C:\\Temp\\a.txt".into()]));
        assert_eq!(cmd("del /f /q C:\\Temp\\a.txt"), Some(vec!["C:\\Temp\\a.txt".into()]));
        assert_eq!(cmd("rd /s /q C:\\Old"), Some(vec!["C:\\Old".into()]));
        assert_eq!(cmd("rmdir C:\\Old /s"), Some(vec!["C:\\Old".into()]));
    }

    #[test]
    fn reject_non_delete_or_complex() {
        assert_eq!(ps("Get-Process"), None);
        assert_eq!(ps("Remove-Item $env:TEMP\\a"), None);
        assert_eq!(ps("Remove-Item C:\\*.tmp"), None);
        assert_eq!(ps("Get-ChildItem C:\\x | Remove-Item"), None);
        assert_eq!(ps("Remove-Item C:\\a; Write-Host x"), None);
        assert_eq!(ps("Remove-Item -Include *.txt C:\\x"), None);
        assert_eq!(cmd("Copy-Item a b"), None);
        assert_eq!(cmd("del /?"), None);
        assert_eq!(cmd("ipconfig"), None);
        assert_eq!(ps("Remove-Item"), None); // no path
        assert_eq!(ps("Write-Host hi"), None);
    }
}



