---
title: WinRM tool HTTP 500 fix — workgroup host (Basic + AllowUnencrypted)
category: runbook
tags: ["winrm", "incident-response", "remote-management", "workgroup", "authentication", "troubleshooting", "Windows"]
source: ingest
date: 2026-09-13
confidence: medium
---

## WinRM tool HTTP 500 fix — workgroup host (Basic + AllowUnencrypted)

# WinRM tool fails with HTTP 500/401 on a workgroup host — fix runbook

## Symptom
- The `winrm` tool (or any WinRM client) to a Windows Server 2019 **workgroup** host (no domain/Kerberos) returns **HTTP 500** (NTLM handshake) or **HTTP 401** (Basic).
- Port 5985 is OPEN (`Test-NetConnection -Port 5985` → TcpTestSucceeded=True); 5986 (HTTPS) is NOT listening.
- Native PowerShell remoting (`New-PSSession` / `Invoke-Command`) WORKS, but the dedicated `winrm` tool FAILS.
- This split (native PS works, tool fails) is the signature: server-side WinRM service auth config is the problem, NOT connectivity or credentials.

## Root cause
The remote WinRM **service** has:
- `Auth/Basic = false`  (only Kerberos + Negotiate enabled)
- `AllowUnencrypted = false`  (rejects plain-HTTP auth)

On a WORKGROUP machine there is no Kerberos/domain, so Negotiate/NTLM over unencrypted HTTP gets rejected → HTTP 500. Clients that default to Basic or mismatched auth fail; native PS succeeds because it negotiates correctly.

## Diagnosis (confirm before fixing)
1. `Test-NetConnection 192.168.52.137 -Port 5985` → confirm 5985 open, 5986 closed.
2. Query server WinRM config via the WORKING channel (native PS remoting):
   ```powershell
   $s = New-PSSession -ComputerName 192.168.52.137 -Credential (Get-Credential)
   Invoke-Command -Session $s -ScriptBlock {
     winrm get winrm/config/service/auth
     winrm get winrm/config/service
   }
   ```
   Expect `Basic = false`, `AllowUnencrypted = false`.

## Fix (apply on the server via the working channel)

### Preferred: WSMan provider (Set-Item) — NO `@{...}` quoting issues
The `Set-Item` WSMan provider is native PowerShell and avoids the `@{...}` hashtable
entirely, so it is safe through ANY nesting level (PSSession, winrm tool, nested shells):
```powershell
$s = New-PSSession -ComputerName 192.168.52.137 -Credential (Get-Credential)
Invoke-Command -Session $s -ScriptBlock {
  Set-Item WSMan:\localhost\Service\Auth\Basic -Value $true
  Set-Item WSMan:\localhost\Service\AllowUnencrypted -Value $true
}
```

### Alternative: winrm set (only if NOT nested)
If you run `winrm set` directly on the server console (no outer shell), the `@{...}`
form is fine. But when it must pass through a remote/nested channel, wrap the WHOLE
value in SINGLE quotes so the outer shell does not touch it:
```powershell
winrm set winrm/config/service/auth '@{Basic="true"}'
winrm set winrm/config/service '@{AllowUnencrypted="true"}'
```

### ⚠️ Quoting pitfall (the bug this runbook previously caused)
The `@{...}` hashtable is mangled when passed as a RAW STRING through an outer shell
layer (e.g. the winrm tool / nested PowerShell) — PowerShell parses the `{`, `:`, `"`,
`;` before winrm.exe ever sees them, producing `Invalid use of command line`.
- Root cause of the mangling: `@{...}` must live INSIDE a remote script block (quoted
  properly), NOT be passed as a bare string across a channel.
- **Rule of thumb: if the command crosses ANY shell boundary, use Set-Item (WSMan
  provider) instead of `winrm set`.** This is the reliable fix.

## Verify
- Re-run `winrm get winrm/config/service/auth` → `Basic = true`, `Kerberos = true`, `Negotiate = true`.
- `winrm get winrm/config/service` → `AllowUnencrypted = true`.
- Now the `winrm` tool works with BOTH `auth=ntlm` and `auth=basic` over HTTP 5985.

## Security caveat
Enabling `Basic=true` + `AllowUnencrypted=true` sends credentials **in cleartext over HTTP**. Acceptable ONLY on an isolated/lab/DMZ network. For production, prefer an **HTTPS listener (5986) with a cert** and keep Basic disabled; or use Negotiate over TLS.

## Key facts from this case
- Target: Windows Server 2019, dual NIC — Ethernet0=192.168.52.137, Ethernet1=192.168.52.139, admin account administrator/Solarsec521.
- Hostname: WIN-S69JLUDHENG.
- "Native PS works but tool fails" is the reliable tell for a service-side auth config issue.
