---
name: phishing-analysis
description: "Phishing email analysis workflow: parse EML files, extract headers and IOCs, analyze URLs and attachments, check sender reputation, evaluate spoofing indicators, penetrate cloud-document trust brokers (Tencent Docs, Google Docs, Notion, etc.), deep-dive ICS calendar attachments, and produce a structured phishing verdict report."
---

# Phishing Email Analysis Workflow

You are performing a structured phishing email analysis. Follow these phases IN ORDER. Do not skip phases.

## 🔴 Core Principle (NEW)

**Three layers of trust must be independently verified:**

1. **Email layer** — sender domain, authentication, forwarding chain
2. **Platform layer** — the URL opens on a *legitimate platform* (Tencent Docs, Google Docs, etc.)? That only proves the *platform* is trusted, NOT the *content*.
3. **Content layer** — what does the document/landing page ACTUALLY contain? **This is the decisive layer.**

> ⚠️ **CRITICAL: A legitimate platform domain (docs.qq.com, docs.google.com, notion.site...) + a legitimate-looking email = the most powerful trust broker.** Attackers use real internal email chains + real platforms to hide the real payload inside the document. **You MUST penetrate the document content, not stop at "the link opens".**

For detailed cloud-doc penetration steps → **`references/cloud-doc-verification.md`**
For ICS attachment deep-dive → **`references/ics-attachment-analysis.md`**

## Phase 1: Parse the Email

> **On-demand tools**: `ir_eml` and `malware_deep` belong to the Windows forensics cold set and may NOT appear in the current tool list. If missing, first call `load_tool_schema` to pull the schema, then invoke normally:
>
> ```json
> {"name": "load_tool_schema", "arguments": {"name": "ir_eml"}}
> ```
>
> Threat-intel tools (`TB_-_Domain_Analysis`, `VT_-_Domain_Reputation`) are external/MCP peripherals, exposed the same on-demand way — probe with `load_tool_schema`; if the tool is genuinely unavailable, fall back to `web_fetch` on public reputation pages and record the gap in the report's "Unverified Items".

Call `ir_eml` to parse the EML file and extract all structured data:

```json
{"name": "ir_eml", "arguments": {"path": "<path_to_eml_file>", "extract_urls": true}}
```

Review the output carefully. Note the `phishing_indicators` section — it already contains automated risk assessment. Also note `attachments` — **including .ics calendar files, which are commonly under-analyzed**.

## Phase 2: Sender Analysis

Examine the sender information for spoofing:

### 2a. From Address Verification

- Does the **display name** match the **email domain**? (e.g., "Microsoft Support" from `@gmail.com` = spoofed)
- Is the domain a **free email provider** pretending to be a corporate entity?
- Check for **homograph domains** (e.g., `micros0ft.com`, `paypa1.com`, `arnazon.com`)
- Is the domain **newly registered** or known for abuse?
- 🔴 **TYPO-SQUATTING within the same mailbox domain**: compare the sender domain char-by-char against the recipient's corporate domain (e.g., `cosocshippring.com` vs real `coscoshipping.com` — letter swap/omission). A domain with **no DNS resolution, no WHOIS, no ICP** registered in a threat-intel DB while being near-identical to the target company = phishing domain.

### 2b. Reply-To and Return-Path

- Does `Reply-To` differ from `From`? (common in BEC attacks)
- Does `Return-Path` differ from `From`? (indicates the real sending server)
- Is `X-Originating-IP` present? Note it for geolocation lookup.

### 2c. Authentication Check

- Check `Authentication-Results` header for:
  - `spf=pass/fail/softfail` — did the sending server authorize this domain?
  - `dkim=pass/fail` — was the message signed and verified?
  - `dmarc=pass/fail` — did domain alignment pass?
- If `Authentication-Results` is missing, note it — the email may have bypassed gateway filtering.
- 🔴 **Framing-tool fingerprints**: scan Message-ID and Received headers for framework identifiers (`@go-phish`, `GoPhish`, `Modlishka`, `Evilginx`). `@go-phish` in Message-ID is a **hard indicator** the email was sent from the GoPhish phishing framework.

### 2d. Received Hop Analysis

- Trace the `received_hops` from bottom (origin) to top (destination)
- Does the first hop's IP match the claimed sender domain?
- Are there unexpected relay servers?
- Note if the sending server (e.g., Coremail) does NOT match the claimed domain — domain/service mismatch is a red flag.

### 2e. 🔴 Internal Forwarding Chain Analysis (NEW)

For emails with `Fw:`/`转发` in subject or `In-Reply-To`/`References` headers:

- Map the forwarding chain: original sender → forwarder → recipient
- 🔴 **A real internal forwarding chain does NOT clear the email.** It provides trust *backing* for the payload. The original sender's account may be **compromised (ATO)** — the email chain is real, but the account is not legitimately operated.
- Verify: does the forwarded email's link match the original sender's legitimate business context? Cross-check the document/landing-page metadata (creator identity, creation time) against the claimed internal context.

If the sender looks suspicious, use threat intel tools to check the sender domain reputation:

```json
{"name": "TB_-_Domain_Analysis", "arguments": {"domain": "<sender_domain>"}}
```

## Phase 3: URL Analysis

Review all URLs extracted by `ir_eml` from the `urls.suspicious` section.

### 3a. URL Destination Verification

For each suspicious URL, use `web_fetch` to check the landing page (if safe to do so):

```json
{"name": "web_fetch", "arguments": {"url": "<suspicious_url>"}}
```

Look for:

- **Credential harvesting pages** — login forms mimicking Microsoft 365, Google, PayPal, banks
- **Malware download pages** — auto-download executables or archives
- **Redirect chains** — URL redirects through multiple domains before landing page
- **Brand impersonation** — logos, language, and layout mimicking legitimate services

### 3b. URL-Display Mismatch

- Does the visible link text differ from the actual URL? (HTML anchor spoofing)
- Does the URL contain `@` symbols? (e.g., `https://legitimate.com@evil.com/path`)
- Are there excessive subdomains? (e.g., `login.microsoft.evil.com`)
- Does the URL use an IP address instead of a domain name?
- 🔴 **Bare internal-IP links**: a link using an RFC1918/private IP (e.g., `http://10.12.9.59/`) instead of a corporate domain or CDN = classic phishing infrastructure (internal C2 / dropper host).

### 3c. Short URL Expansion

If URL shorteners are detected (bit.ly, tinyurl.com, etc.), resolve them:

```json
{"name": "web_fetch", "arguments": {"url": "<shortened_url>"}}
```

Note the final destination domain.

### 3d. 🔴 Password-Protected Page Penetration (CRITICAL)

**Do NOT stop at HTTP 401/403 or password-protected pages.** A password gate is often used as a trust intermediary to hide the real phishing page behind it.

**When any URL returns 401/403 or a password dialog:**

1. Check if the email body or attachments contain a password

2. If a password is found, attempt to penetrate the gate:

   - Try `web_fetch` with POST body containing the password

   - Or use `browser_cdp` to simulate real browser interaction:

     ```json
     {"name": "browser_cdp", "arguments": {"action": "navigate", "url": "<password_protected_url>"}}
     ```

     Then type the password, submit, and track the redirect chain.

3. **Follow ALL redirects** to the final destination — do not stop at the first 200 OK

4. Analyze the **final** landing page, not the gate page

**If password cannot be penetrated:**

- Mark as "UNVERIFIED PASSWORD GATE → HIGH RISK" in the report
- Explicitly list it in the "Unverified Items" section

### 3e. 🟡 Hosted Platform Subdomain Detection

Identify URLs using well-known hosting/building platforms as potential trust intermediaries:

| Platform            | Pattern                       | Risk When...                                             |
| ------------------- | ----------------------------- | -------------------------------------------------------- |
| Squarespace         | `*.squarespace.com`           | + email provides password                                |
| Wix                 | `*.wixsite.com` / `*.wix.com` | + commercial email context                               |
| Notion              | `*.notion.site`               | + no attachment, link-only / + real internal email chain |
| WordPress.com       | `*.wordpress.com`             | + password protected                                     |
| Google Sites        | `sites.google.com/*`          | + redirects to external domain                           |
| Canva               | `*.canva.com`                 | + file sharing redirect                                  |
| 🔴 **Tencent Docs** | `docs.qq.com/doc/*`           | + internal Fw chain + creator is anonymous               |

**Detection rule:** If the URL is a subdomain of a known hosting platform AND the email:

- Has no attachments (files shared via link) — OR attachment is only a .ics calendar
- Provides a password in the body
- Has a commercial/business pretext — OR internal forwarding chain
- **Document is anonymous-created / view-only**

→ **Mark as HIGH SUSPICION — potential trust chain phishing. Then PENETRATE the document content (see 3g).**

### 3f. 🔴 Trust Chain Analysis (Phase 3.5)

This analysis dimension maps the trust relationship across all domains in the attack chain.

**Input:** Collect all domains in the chain:

- Sender domain (e.g., `hartsas.gr` / real internal `coscoshipping.com`)
- URL domain(s) (e.g., `hartsas.squarespace.com` / `docs.qq.com`)
- Final destination domain (e.g., `indlesmieux.com` / `xlvgn.cn`)

**Cross-compare across these dimensions:**

| Dimension          | What to check                 | Risk Signal                                                          |
| ------------------ | ----------------------------- | -------------------------------------------------------------------- |
| Domain age         | Compare ages across the chain | Final domain much younger = 🚩                                       |
| Registrar          | Same or different registrars  | Different registrars with privacy proxy = 🚩                         |
| Registrant info    | Same or different owners      | Different owners = 🚩 (trust break)                                  |
| SSL cert dates     | Compare cert issuance dates   | Final domain cert issued same day as registration = 🚩               |
| Language/region    | Geographic mismatch           | Sender region ≠ document creator region, final domain unrelated = 🚩 |
| DNS infrastructure | Same or different providers   | Final domain uses Cloudflare/anti-bot CDN to hide origin = 🚩        |

**Weighted scoring for trust chain:**

| Pattern                                                               | Score                   |
| --------------------------------------------------------------------- | ----------------------- |
| Final domain registered < 7 days before email                         | +3                      |
| Final domain uses privacy proxy/WHOIS guard                           | +2                      |
| Final domain hosted on Cloudflare/anti-bot CDN                        | +1                      |
| Intermediate domain is a hosted platform subdomain (docs.qq.com etc.) | +2                      |
| Intermediate platform + anonymous creator (no corp binding)           | +2                      |
| Platform document created same day as email                           | +3                      |
| Document content ≠ email subject (content swap)                       | +3                      |
| Email uses REAL internal forwarding chain                             | +2 (trust-broker boost) |
| Intermediate domain + password gate                                   | +3                      |
| Domain registrant country differs from sender domain country          | +1                      |
| Sender domain age > 5 years BUT final domain < 30 days                | +3                      |

**Threshold:** Cumulative score ≥ +4 → mark as **PHISHING / LIKELY PHISHING** regardless of email authentication results.

### 3g. 🔴 Cloud Document Platform Content Penetration (NEW — CRITICAL)

**When the URL points to a cloud document platform (docs.qq.com, docs.google.com, notion.site, yuque, shimo, kdocs...):**

1. **Do NOT stop at "the page opens"** — the platform is being used as a trust broker
2. **Extract document metadata**: fetch the raw page HTML, find `basicClientVars`/`__INITIAL_STATE__`, decode base64 blobs to get: title, padId, **creator nickname**, **creator UID**, **creator corp binding** (empty = personal account), createTime, lastModifyTime, access policy
3. **Pull the actual content via the platform API** (see `references/cloud-doc-verification.md` for per-platform recipes). For Tencent Docs, call the `dop-api/opendoc` endpoint with the correct **Referer header**, then decode `initialAttributedText.text` (base64, possibly chunked) into plaintext
4. **Analyze the decoded content for**:
   - Embedded external links (`HYPERLINK`, marked URLs) → ⟶ **threat-intel the final domain** (TB/VT)
   - Lures: subsidies/refund/social-security/salary validation/verification-code/bind-bank-card
   - Form/QR-code collection prompts
   - **Word-count padding**: page shows large stats (e.g., 25 pages/16583 words) but body is short → filled with ad/spam text to look legit
5. **Timestamp self-proof**: compare document createTime against email Date and ICS DTSTAMP — all within ~30 min = scripted attack chain

**Decision: a legitimate platform + anonymous personal-account creator + same-day creation + content swap (subject ≠ body) = PHISHING with very high confidence.**

### 3h. 🔴 Canvas-Rendered Content Handling (NEW)

When document content renders as Canvas (pixels, not DOM):

- Canvas drawing ≠ real readable content; check whether captions/word-count are **server-reported but content is empty**
- Extract the underlying data via the platform API (the text is sent to the client as base64/JSON even though DOM shows canvas)
- If `initialAttributedText.text` is EMPTY but page claims 16583 words → the page stats are fabricated/padded → strong anomaly signal

## Phase 4: Attachment Analysis

Review the `attachments` section from `ir_eml` output.

### 4a. Suspicious Extensions

Flag any attachments with:

- Executable: `.exe`, `.scr`, `.bat`, `.cmd`, `.ps1`, `.vbs`, `.js`, `.wsf`, `.hta`, `.cpl`, `.msi`, `.dll`, `.com`, `.pif`
- Document macros: `.doc`, `.docx`, `.xls`, `.xlsx`, `.ppt`, `.pptx` (may contain macros)
- Archive files: `.zip`, `.rar`, `.7z` (may contain executables)
- **Double extensions**: `invoice.pdf.exe`, `report.doc.scr`

### 4b. Deep Malware Analysis

If a suspicious attachment is present and saved locally, run deep analysis:

```json
{"name": "malware_deep", "arguments": {"path": "<extracted_attachment_path>"}}
```

### 4c. Nested Messages

If `is_nested_message` is true in any attachment, parse the nested email:

- Check if the nested message contains additional phishing indicators
- Note the nested message's From/To/Subject for the report

### 4d. 🔴 ICS Calendar Attachment Deep-Dive (NEW)

**NEVER dismiss an `.ics` file as "just a calendar file".** ICS is a **second delivery channel** for phishing links and exposes the true attack scale:

1. Extract and parse the ICS (see `references/ics-attachment-analysis.md`):
   - **DESCRIPTION / X-ALT-DESC**: search for URLs — the phishing link is often embedded here (second channel)
   - **ATTENDEE list**: count and group by domain — mass invitations = organization-wide blast; security/IT mailboxes included = scattershot
   - **ORGANIZER vs From**: account-takeover signal when combined with forwarding chains
   - **DTSTAMP / DTSTART / DTEND**: all-day placeholder (07:00–23:00) with no real meeting content = fake; DTSTAMP synchronized with document creation = scripted
   - **UID / hashes**: record as IOCs for mailbox-wide search of the invite
   - **URL / ATTACH fields**: standard invites shouldn't have them; present = high risk
2. Time-stamp cross-correlation: doc createTime → email Date → ICS DTSTAMP → forward time. All within 30–40 min = orchestrated campaign.
3. **Do not flag the .ics format itself as malicious** — it's a benign VCALENDAR carrier; the *content* it carries is malicious.

## Phase 5: Social Engineering Assessment

Evaluate the email body (from `body.text` and `body.html`) for social engineering tactics:

| Tactic        | Indicators                                                                                                             |
| ------------- | ---------------------------------------------------------------------------------------------------------------------- |
| **Urgency**   | "immediate action required", "your account will be suspended", "within 24 hours", "逾期不再受理"                             |
| **Authority** | Impersonating CEO, IT department, HR, bank, government agency, "人资通知", "第XX号文"                                         |
| **Fear**      | "unauthorized transaction", "account compromised", "legal action"                                                      |
| **Curiosity** | "see attached invoice", "your order confirmation", "payment receipt", "待办文件"                                           |
| **Greed**     | "you've won", "refund pending", "tax return", "补贴申领", "2800-5800元"                                                     |
| **Trust**     | Uses real company logos, correct branding, professional formatting, internal-style notice format with document numbers |

Count the urgency indicators detected by `ir_eml` in `phishing_indicators`. If 3+ are triggered, flag as high social engineering risk.

### 5a. 🔴 "No Attachment, Link Only" Signal (NEW)

A critical behavioral signal that is often missed:

- Legitimate business emails with file sharing typically use **attachments** OR **known enterprise platforms** (SharePoint, OneDrive, Dropbox, Box)
- If the email is a commercial/business communication AND:
  - Has **no attachments** (or only a .ics invite)
  - Links to an **unusual hosting platform** (not enterprise-grade)
  - Provides a **password** in plain text — or the platform document is anonymous/view-only
  - Uses urgency language

→ **Combine this signal with Phase 3.5 trust chain analysis for cumulative scoring**

### 5b. 🔴 "Benefit/Subsidy Bait" Pattern (NEW)

Chinese-government/social-security themed phishing (high prevalence in CN orgs):

- Fake official notices: 《关于2026年五险一金补贴申领通知》with fake 文号 (e.g., "人资（2026）第16号")
- Amount bait: "2800-5800元" — high but plausible
- Action lures: "点此进行办理", "绑定银行卡", "验证信息", "短信验证码"
- Often delivered via **real internal email chains + cloud-doc links** for maximum trust
- Associated crime groups: GoldenCicada (金蝉) and similar subsidy-themed phishing rings

## Phase 6: IOCs and Threat Intel

### 6a. Compile IOCs

From the `ir_eml` output, compile all IOCs into a structured list:

- **Sender email** and domain
- **Reply-To** address (if different)
- **Return-Path** domain
- **X-Originating-IP** (if present)
- **All URLs** (especially suspicious ones)
- **Attachment hashes** (if analyzed with malware_deep) — **including .ics hashes**
- **Sender domain** and any redirect domains
- **🔴 Final destination domain** (after penetrating password gates AND cloud-doc content)
- 🔴 **ICS artifacts**: UID, ORGANIZER, ATTENDEE list (de-identified)
- 🔴 **Cloud-doc artifacts**: padId, creator UID, creator nickname, createTime

### 6b. Threat Intel Lookup

For high-priority IOCs, search threat intelligence:

```json
{"name": "TB_-_Domain_Analysis", "arguments": {"domain": "<suspicious_domain>"}}
{"name": "VT_-_Domain_Reputation", "arguments": {"domain": "<suspicious_domain>"}}
```

- Check if any IOCs match known campaigns / crime groups (GoldenCicada 金蝉, etc.)
- Note: a domain could be registered long ago (e.g., 2025-10) yet currently phishing — check **current resolution** (`cur_ips` empty = infrastructure taken down / rotating = phishing trait)

### 6c. 🔴 Trust Chain IOC Mapping (NEW)

Present the full attack chain as a structured IOC map:

```
Real Sender Account (possibly ATO) → Cloud-Doc Platform (anonymous creator) → Final Phishing Domain
```

For each node in the chain, record:

- Domain name and registration date
- SSL certificate issuance date
- Hosting provider / CDN (docs.qq.com = Tencent; platform node role = trust broker)
- Registrant info (if not privacy-protected)
- Role in the attack chain

## Phase 7: Verdict and Report

### 🔴 REVISED Verdict Matrix — Cumulative Weighted Scoring

**CRITICAL RULE:** Authentication passing (SPF/DKIM/DMARC all pass) does NOT automatically clear an email. It only proves the email originated from the claimed domain. **The content and link chain must be independently evaluated. A real internal forwarding chain + real platform domain is a trust-broker tactic, not clearance.**

**Weighted scoring system:**

| Evidence                                                                 | Direction    | Weight |
| ------------------------------------------------------------------------ | ------------ | ------ |
| SPF/DKIM/DMARC all pass                                                  | → CLEAN      | -3     |
| Sender domain age > 5 years                                              | → CLEAN      | -2     |
| Verified company entity with real registration                           | → CLEAN      | -1     |
| No attachments, file shared via link                                     | → SUSPICIOUS | +1     |
| Attachment is only an .ics calendar invite                               | → SUSPICIOUS | +1     |
| Hosted platform subdomain (Squarespace/Wix/Notion/TencentDocs)           | → SUSPICIOUS | +2     |
| Platform document creator = anonymous/personal account (no corp binding) | → MALICIOUS  | +3     |
| Platform document created on the SAME DAY as email                       | → MALICIOUS  | +3     |
| Platform doc content ≠ email subject (content swap)                      | → MALICIOUS  | +3     |
| Platform doc embeds external link to unsigned domain                     | → MALICIOUS  | +3     |
| Platform doc huge word-count but empty/blank body (padded)               | → SUSPICIOUS | +2     |
| ICS DESCRIPTION embeds the same phishing URL (dual channel)              | → MALICIOUS  | +3     |
| ICS ATTENDEE count > 20 or cross-entity domains (mass blast)             | → MALICIOUS  | +2     |
| Email uses REAL internal forwarding chain as trust backing               | → SUSPICIOUS | +2     |
| Password-protected page + password in email body                         | → SUSPICIOUS | +3     |
| Sender has no prior email history with recipient                         | → SUSPICIOUS | +1     |
| Commercial email using link instead of attachment                        | → SUSPICIOUS | +1     |
| Internal email with phishing-framework Message-ID (@go-phish etc.)       | → MALICIOUS  | +4     |
| Final domain registered < 7 days before email                            | → MALICIOUS  | +3     |
| Final domain uses privacy proxy                                          | → SUSPICIOUS | +2     |
| Final domain hosted on Cloudflare (hiding origin)                        | → SUSPICIOUS | +1     |
| Trust chain broken (intermediate ≠ final domain owner)                   | → MALICIOUS  | +3     |
| Display name spoofing detected                                           | → MALICIOUS  | +3     |
| Credential harvesting page confirmed                                     | → MALICIOUS  | +5     |
| SPF/DKIM/DMARC all fail                                                  | → MALICIOUS  | +3     |
| URL contains @ symbol or IP address                                      | → MALICIOUS  | +2     |

**Verdict by cumulative score:**

| Total Score | Verdict                             |
| ----------- | ----------------------------------- |
| ≤ -3        | CLEAN                               |
| -2 to +2    | SUSPICIOUS (Low confidence)         |
| +3 to +5    | LIKELY PHISHING (Medium confidence) |
| +6 to +8    | PHISHING (High confidence)          |
| ≥ +9        | PHISHING (Very High confidence)     |

**Important:** If the final destination domain is confirmed as a credential harvesting page (fake login form) OR the cloud-doc content embeds a link to a domain confirmed malicious, **override everything** and mark as PHISHING regardless of score.

### 🔴 Unverified Items Section

BEFORE presenting the final verdict, the report MUST include a section titled **"Unverified Items"** that explicitly lists anything the analysis could not confirm:

| Unverified Item                           | Why Not Verified               | Risk Impact                                     |
| ----------------------------------------- | ------------------------------ | ----------------------------------------------- |
| Content behind password gate              | Password not provided in email | 🔴 High — could contain phishing page           |
| Final redirect destination                | Did not follow redirect chain  | 🔴 High — could lead to credential harvester    |
| Sender's past email history               | No historical data available   | 🟡 Medium — new sender is higher risk           |
| Domain registrant identity                | Privacy proxy / WHOIS guard    | 🟡 Medium — identity hidden                     |
| Cloud-doc full body (remaining pages)     | Canvas-rendered / login-gated  | 🟡 Medium — remaining pages may hold more links |
| Whether internal sender's account was ATO | No access to login logs        | 🔴 High — determines false sense of trust       |

**This section serves as a forcing function — if it has entries, the verdict should be more conservative.**

### Output Format

Present your final assessment as:

1. **Verdict**: PHISHING / MALWARE DELIVERY / LIKELY PHISHING / SUSPICIOUS / CLEAN

2. **Confidence**: Very High / High / Medium / Low

3. **Cumulative Score**: [numeric score] (from weighted system)

4. **Attack Type**: Credential Harvesting / Malware Delivery / BEC (Business Email Compromise) / Invoice Fraud / Romance Scam / Multi-Stage Trust Chain Phishing / **Cloud-Doc Trust Broker Phishing** / Other

5. **MITRE ATT&CK**:

   - T1566.001 — Phishing: Spearphishing Attachment
   - T1566.002 — Phishing: Spearphishing Link
   - T1566.003 — Phishing: Spearphishing via Service
   - T1598 — Phishing for Information
   - T1078 — Valid Accounts (ATO of internal account)
   - T1585.001 — Establish Accounts: Social Media / Cloud Platform
   - T1204.001 — User Execution: Malicious Link

6. **Executive Summary** (2-3 sentences)

7. **Key Indicators** (bullet list of the strongest evidence)

8. **Attack Chain Diagram**: show the full node-to-node flow:

   ```
   Real/ATO Sender Account → Cloud-Doc Platform (anonymous doc, link embedded) → Final Phishing Page
   ```

9. **Trust Chain Analysis** (cross-domain comparison table)

10. **Cloud-Doc Content Evidence** (NEW): decoded document text excerpt, embedded links, creator metadata, timestamp correlation

11. **ICS Attachment Evidence** (NEW, if applicable): ORGANIZER, ATTENDEE count/domains, embedded URL, UID, DTSTAMP correlation

12. **Unverified Items** (what could not be checked)

13. **IOCs** (structured list for threat intel sharing)

    - Sender: email, domain, IP
    - URLs: all URLs with roles (initial / intermediate / final)
    - Attachments: filenames, hashes, content-types (**incl. .ics hash + UID**)
    - Infrastructure: redirect domains, CDN providers, C2 IPs (**incl. platform doc IDs, creator UIDs**)
    - Registrant: name, email, phone, address (if exposed)

14. **Recommended Actions**:

    - Block sender domain at email gateway
    - Block suspicious URLs/IPs at web proxy/firewall
    - Search mailbox for other emails from same sender across the organization
    - 🔴 **Search mailbox/calendar for the .ics UID and the cloud-doc link** to determine the full blast radius (all ATTENDEEs)
    - If attachment was opened: isolate host and run malware scan
    - If credentials were entered: force password reset immediately
    - If internal account suspected of ATO: **verify login logs, force password reset + session revocation, audit sent items**
    - Report to relevant authorities / anti-phishing working groups
    - 🔴 **If a legitimate company's email was compromised** → notify them via non-email channel (phone, website contact form)

## HTML Report Template (Mandatory)

When generating a full HTML phishing report, you **MUST** start from the skill's built-in template `references/report-template.html`(derived from the 2026-08-27 COSCO cloud-doc trust-broker case report). Hand-writing a report or reusing an old one is **FORBIDDEN**.

### How to Use

1. `skill_read_file` from `references/report-template.html` (skill="phishing-analysis", path="references/report-template.html") — use the skill's actual registered name
2. **Copy the template** and replace all `{{PLACEHOLDER}}` (**26 total** — grep `{{` must return 0 hits before writing)
3. Output to `output/phishing_report_<email-abbrev>_<date>.html`

### Placeholder List (26)

| Placeholder                                                                                              | Meaning                                                                                   |
| -------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `{{EMAIL_SUBJECT}}` / `{{REPORT_TITLE}}`                                                                 | Email subject / report title                                                              |
| `{{EMAIL_FILE_PATH}}` / `{{REPORT_DATE}}`                                                                | Analyzed file path / report date                                                          |
| `{{EMAIL_SENT_AT}}` / `{{EMAIL_FROM}}` / `{{EMAIL_TO}}` / `{{EMAIL_ORIGINAL_FROM}}` / `{{EMAIL_SERVER}}` | Email header metadata (6-cell meta-grid)                                                  |
| `{{VERDICT}}` / `{{VERDICT_DESC}}` / `{{SCORE}}` / `{{CONFIDENCE}}` / `{{ATTACK_TYPE}}`                  | Verdict banner                                                                            |
| `{{SUMMARY_P1}}` / `{{SUMMARY_P2}}` / `{{SUMMARY_WARNING}}`                                              | Executive summary (3 paragraphs)                                                          |
| `{{KEY_METRICS_CARDS}}`                                                                                  | 6-8 metric cards (.card template; colors --ok/--warn/--accent)                            |
| `{{EVIDENCE_OVERVIEW_ROWS}}` / `{{EVIDENCE_SECTIONS}}`                                                   | Evidence overview table rows + detailed evidence subsections (3.2+ .docquote/.shot/table) |
| `{{CHAIN_NODES}}` / `{{TRUST_BREAK_ROWS}}`                                                               | Attack-chain nodes (.node/.hot/.mid + .arrow) + trust-break table                         |
| `{{MITRE_ROWS}}`                                                                                         | MITRE ATT&CK table rows                                                                   |
| `{{IOC_ITEMS}}`                                                                                          | IOC entries (.ioc template; badges b-red/b-yellow/b-cyan)                                 |
| `{{UNVERIFIED_ROWS}}`                                                                                    | Unverified-items table rows (🔴/🟡/🟢 + description)                                      |
| `{{SOCIAL_ENG_ROWS}}`                                                                                    | Social-engineering tactic table rows                                                      |
| `{{ACTION_ITEMS}}`                                                                                       | Recommended-action ordered-list items (numbered badge auto-count)                         |
| `{{ANALYSIS_METHODS}}`                                                                                   | Footer analysis-methods note                                                              |

### Hard Requirements (violation = invalid, discard and rebuild)

- **Fixed section order**: Header → Verdict banner → 1 Executive Summary → 2 Key Metrics → 3 Key Evidence Chain (3.1 overview → 3.x decisive evidence) → 4 Attack Chain Reconstruction → 5 MITRE → 6 IOCs → 7 Unverified Items → 8 Social Engineering → 9 Recommended Actions → Footer. No reordering / merging / skipping sections.
- **Verdict banner colors**: PHISHING red (default) / LIKELY PHISHING yellow (--warn) / SUSPICIOUS orange / CLEAN green (--ok) — adjust the `.verdict` border/background accordingly.
- **Severity badges**: CRITICAL=t-red / HIGH=t-yellow / MEDIUM=t-cyan / CLEAN=t-green.
- **Screenshot evidence** (incl. browser_cdp captures): copy into the report directory and reference with relative paths `<img src="xx.png">`, each with a `.cap` caption (figure # + content + time); decisive page screenshots go into evidence section 3.x.
- **Placeholder self-check**: grep `{{` must return 0 hits before writing.
- Mini-templates inside `{{xxx}}` HTML comments are format references only — remove them from the final output.

## Rules

- NEVER open or execute email attachments directly — analysis is static only
- NEVER click phishing links from your own machine — use `web_fetch` (isolated) or `browser_cdp` (sandboxed) if needed
- ALWAYS preserve the original EML file as evidence
- If the email contains sensitive PII/credentials, note it but do NOT repeat the sensitive data in your response
- Cross-reference findings across all phases — a single indicator may be a false positive, but multiple correlated indicators confirm phishing
- Document everything — this report may be used for security awareness training
- 🔴 **CRITICAL: Authentication passes ≠ email is safe.** Always independently evaluate the link chain, especially when password-protected pages or hosted platform subdomains are involved.
- 🔴 **CRITICAL: A legitimate platform + real internal email chain is the strongest trust broker.** Always penetrate the cloud-doc content via platform APIs — do NOT stop at "the page opens" or "the cover title looks right".
- 🔴 **CRITICAL: Canvas-rendered content must be extracted via the platform data API, not assumed from page statistics.** Empty body + huge page stats = fabricated.
- 🔴 **CRITICAL: Never dismiss .ics attachments.** They are dual-channel delivery vehicles and expose the true blast radius.
- 🔴 **CRITICAL: If you cannot verify something, say so.** The "Unverified Items" section is mandatory. False certainty is more dangerous than admitting uncertainty.
