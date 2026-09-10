# USER.md — User Profile & Preferences

> This file records **who the user is, their language, and professional preferences**.
> AI communication style (tone/format/content/length) is defined in `SOUL.md`; task
> execution red-lines are in `AGENTS.md`.

## Basic Information
- Address the user as **Master** by default.
- The user works in cybersecurity, focused on desktop administration, security
  operations (SOC), threat detection, incident response, and digital forensics.
- The user is building a local AI agent system tailored for cybersecurity
  scenarios: alert triage, analysis, and remediation.

## Language & Region (User Environment)
- Default language: English.
- Date format YYYY-MM-DD, currency CNY, timezone CST (UTC+8, Shanghai).
- Security terminology may stay in English (e.g. APT, C2, Lateral Movement), with
  a Chinese gloss in parentheses when helpful.
- Default reply language: English unless the user explicitly requests otherwise
  (applies to main and CRON sessions).

## Professional Preferences (Cybersecurity)
- **Core domains**: Threat Detection & Response (TDR), Security Operations (SOC),
  alert triage & remediation, and AI security (prompt-injection defense).
- **Tools & platforms**: Snort/Suricata (IDS/IPS), YARA (malware detection),
  Sigma (generic detection rules), ELK/Splunk (log analysis).
- **Agent scenarios**: alert classification (TP/FP), multi-agent collaboration,
  MCP tool invocation, and memory-management optimization under high-concurrency
  alert flood.
- **Architecture focus**: concurrency, multi-agent scheduling, modular Skills
  design, dynamic tool loading, and large-scale GPU inference optimization.
- **Compliance & open source**: sensitive to licensing when using open-source
  security tools commercially; cite license provenance when referencing code.
- **Threat intel**: IOC management, ATT&CK mapping, and threat-hunting methodology.
