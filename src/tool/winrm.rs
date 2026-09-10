//! WinRM client module for Windows remote execution.
//! Uses winrm-rs (pure Rust WS-Management client) for secure remote command
//! execution, aligned with linux_ssh for Linux hosts.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::time::timeout;

use winrm_rs::{AuthMethod, CommandOutput as WinrmCommandOutput, WinrmClient, WinrmConfig, WinrmCredentials};

use crate::context::ToolContext;
use crate::error::AgentResult;
use crate::policy::{IntentPolicy, IntentVerdict};

use super::{TimeoutStage, Tool};

/// WinRM execute tool: run commands / PowerShell scripts on remote Windows hosts.
pub struct WinrmExecTool;

#[async_trait]
impl Tool for WinrmExecTool {
    fn name(&self) -> &str {
        "winrm"
    }

    fn description(&self) -> &str {
        "Execute commands / PowerShell scripts on remote Windows hosts via WinRM (WS-Management). Returns stdout, stderr, and exit code.\n\n\
         IMPORTANT: Use this tool for remote Windows commands. Do NOT use shell_exec with Invoke-Command / winrs (TrustedHosts + local-admin gating makes them fail).\n\n\
         Use this tool to:\n\
         - Run arbitrary PowerShell or cmd.exe commands on remote Windows hosts\n\
         - Check system status, services, processes, logs, registry, network\n\
         - Investigate incidents and perform incident response on remote Windows machines\n\
         - Perform ad-hoc operations on remote Windows servers\n\n\
         Parameters:\n\
         - target: WinRM host name or IP, optionally host:port (e.g. '192.168.1.100' or '10.0.0.5:5985')\n\
         - username: Windows account name (default: administrator)\n\
         - password: Windows account password\n\
         - domain: Optional NetBIOS domain (default: auto-detect from NTLM challenge)\n\
         - command: Command or PowerShell script to execute on the remote host\n\
         - powershell: true to run as PowerShell script (default), false to run via cmd.exe\n\
         - auth: 'ntlm' (default) or 'basic' (use 'basic' only over TLS)\n\
         - use_tls: true for HTTPS/WinRM 5986 (default false = HTTP 5985)\n\
         - port: WinRM port override (default 5985 HTTP / 5986 HTTPS)\n\
         - accept_invalid_certs: accept self-signed TLS certs (test only; default false)\n\
         - timeout_secs: operation timeout in seconds (default: 60)"
    }

    fn is_builtin(&self) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        false // Commands may modify the remote system
    }

    fn timeout_stage(&self) -> TimeoutStage {
        TimeoutStage::Long
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "WinRM host name or IP, optionally host:port (e.g. '192.168.1.100' or '10.0.0.5:5985')"
                },
                "username": {
                    "type": "string",
                    "description": "Windows account name (default: administrator)"
                },
                "password": {
                    "type": "string",
                    "description": "Windows account password"
                },
                "domain": {
                    "type": "string",
                    "description": "Optional NetBIOS domain (default: auto-detect from NTLM challenge)"
                },
                "command": {
                    "type": "string",
                    "description": "Command or PowerShell script to execute on the remote host"
                },
                "powershell": {
                    "type": "boolean",
                    "description": "true = run as PowerShell script (default); false = run via cmd.exe"
                },
                "auth": {
                    "type": "string",
                    "enum": ["ntlm", "basic"],
                    "description": "Authentication method (default: ntlm)"
                },
                "use_tls": {
                    "type": "boolean",
                    "description": "Use HTTPS (WinRM 5986) instead of HTTP (default: false)"
                },
                "port": {
                    "type": "integer",
                    "description": "WinRM port override (default: 5985 HTTP / 5986 HTTPS)"
                },
                "accept_invalid_certs": {
                    "type": "boolean",
                    "description": "Accept self-signed TLS certs (test only; default false)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Operation timeout in seconds (default: 60)"
                }
            },
            "required": ["target", "command"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        let target = args["target"]
            .as_str()
            .ok_or("Missing required parameter: target")?;
        let command = args["command"]
            .as_str()
            .ok_or("Missing required parameter: command")?;
        let username = args["username"].as_str().unwrap_or("administrator");
        let password = args["password"]
            .as_str()
            .ok_or("Missing required parameter: password")?;
        let domain = args["domain"].as_str().unwrap_or("");
        let powershell = args["powershell"].as_bool().unwrap_or(true);
        let auth_str = args["auth"].as_str().unwrap_or("ntlm");
        let use_tls = args["use_tls"].as_bool().unwrap_or(false);
        let accept_invalid_certs = args["accept_invalid_certs"].as_bool().unwrap_or(false);
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(60);

        // ── Intent Policy evaluation (aligned with shell_exec) ──
        // Independent of the Permission system:
        // - Block: catastrophic irreversible ops → hard reject regardless of permissions
        // - Audit: high-risk but legitimate → log and proceed
        // - Pass: normal → silent
        let shell_str = if powershell { "powershell" } else { "cmd" };
        let policy = IntentPolicy::new();
        match policy.evaluate(command, shell_str) {
            IntentVerdict::Block { reason } => {
                return Err(format!(
                    "BLOCKED (safety interlock): {}. \
                     This operation is irreversible and cannot be executed through FoxIR. \
                     If you truly need this, execute it manually outside the agent.",
                    reason
                ).into());
            }
            IntentVerdict::Audit { reason } => {
                tracing::warn!(
                    "[AUDIT] winrm high-risk: {} | shell={} | target={} | command={}",
                    reason, shell_str, target, command
                );
                // Proceed — user has authorized via Permission gate or accepts risk
            }
            IntentVerdict::Pass => { /* silent */ }
        }

        // Parse target: host or host:port
        let (host, port_from_target) = parse_winrm_target(target);
        let default_port: u16 = if use_tls { 5986 } else { 5985 };
        let requested_port = args["port"].as_u64().unwrap_or(port_from_target as u64);
        let port: u16 = if requested_port == 0 {
            default_port
        } else {
            requested_port.min(u16::MAX as u64) as u16
        };

        let auth_method = match auth_str {
            "basic" => AuthMethod::Basic,
            _ => AuthMethod::Ntlm,
        };

        let config = WinrmConfig {
            port,
            use_tls,
            accept_invalid_certs,
            operation_timeout_secs: timeout_secs,
            auth_method,
            ..Default::default()
        };

        let creds = WinrmCredentials::new(
            username.to_string(),
            password.to_string(),
            domain.to_string(),
        );

        let client = WinrmClient::new(config, creds)
            .map_err(|e| format!("WinRM client init failed: {}", e))?;

        tracing::info!(
            "[winrm] Executing on {}:{} ({}): {}",
            host,
            port,
            if powershell { "powershell" } else { "cmd" },
            command
        );

        ctx.report_progress(&format!("Connecting to {}:{}...", host, port));

        let effective_timeout = Duration::from_secs(timeout_secs + 15);
        let output: WinrmCommandOutput = if powershell {
            timeout(effective_timeout, client.run_powershell(&host, command))
                .await
                .map_err(|_| {
                    format!(
                        "WinRM PowerShell command timed out after {}s (target {})",
                        timeout_secs, host
                    )
                })?
                .map_err(|e| format!("WinRM PowerShell execution failed: {}", e))?
        } else {
            timeout(effective_timeout, client.run_command(&host, "cmd.exe", &["/C", command]))
                .await
                .map_err(|_| {
                    format!(
                        "WinRM command timed out after {}s (target {})",
                        timeout_secs, host
                    )
                })?
                .map_err(|e| format!("WinRM command execution failed: {}", e))?
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        tracing::info!(
            "[winrm] Command completed: exit_code={}, stdout={} bytes, stderr={} bytes",
            output.exit_code,
            stdout.len(),
            stderr.len()
        );

        Ok(json!({
            "exit_code": output.exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "target": format!("{}@{}:{}", username, host, port),
            "command": command,
        }))
    }
}

/// Parse a WinRM target into (host, optional_port). Returns 0 as port when absent.
fn parse_winrm_target(target: &str) -> (String, u16) {
    // Format: host or host:port (hostname / IPv4).
    if let Some(idx) = target.rfind(':') {
        if let Ok(p) = target[idx + 1..].parse::<u16>() {
            return (target[..idx].to_string(), p);
        }
    }
    (target.to_string(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_winrm_target() {
        assert_eq!(
            parse_winrm_target("192.168.52.137"),
            ("192.168.52.137".into(), 0)
        );
        assert_eq!(
            parse_winrm_target("10.0.0.5:5986"),
            ("10.0.0.5".into(), 5986)
        );
        assert_eq!(parse_winrm_target("server01"), ("server01".into(), 0));
    }
}
