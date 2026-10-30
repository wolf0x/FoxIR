# Cloud Document Platform Content Penetration & Verification

> Trigger: the email links to a **cloud document platform** (Tencent Docs docs.qq.com, Google Docs, Kingsoft Docs, Shimo, Notion, Yuque, OneDrive/SharePoint shared docs, etc.) and you must NOT conclude "pass" merely because "the page opens and the domain is a legitimate platform".
> Core principle: **A trusted platform domain ≠ trusted document content.** Attackers use legitimate platforms as trust brokers, hiding the real malicious content inside the document.

---

## 1. Decide Whether Penetration Is Needed (Trigger)

Email contains a platform-document link AND any of the following:

- The document is accessed via a **share link** (not a public directory), e.g. `docs.qq.com/doc/<padId>`
- Document title relates to the email subject but **body cannot be directly confirmed**
- Document creator identity is unknown
- Email is an internal share/forward ("Fw:" / "转发"), leveraging an internal trust chain as backing

## 2. Penetration Path Overview (Shallow → Deep)

| Layer                        | Method                                       | What You Obtain                                               |
| ---------------------------- | -------------------------------------------- | ------------------------------------------------------------- |
| L1 Browser render            | browser_cdp navigate → get_text / screenshot | Page shell, title, page/word count stats, screenshot evidence |
| L2 Raw HTML fetch            | web_fetch page URL                           | `basicClientVars` (title/padId/creator info, base64-encoded)  |
| L3 **Platform internal API** | Per-platform recipes below                   | **Full document body text** (decisive)                        |
| L4 Metadata endpoint         | `clientVars` in the same L3 response         | createTime, creator nickname/UID/corp binding, access policy  |

## 3. Tencent Docs (docs.qq.com) — Step-by-Step

### 3.1 Fetch Raw Page HTML for Basic Metadata

```
web_fetch: https://docs.qq.com/doc/<padId>
```

Search the HTML `script` blocks for `basicClientVars`; decode the base64 fragments to obtain:

- `padId`, `title`, `createInfo.createTime`, `creatorNick`, `ownerNick`
- `createInfo.creatorCorpId / creatorCorpName` (empty = created by a personal account)

### 3.2 Pull the Body via the Core API (Decisive Step)

Build the opendoc endpoint — **the Referer header MUST carry the doc page URL**, otherwise you get 403/empty data:

```http
GET https://docs.qq.com/dop-api/opendoc?u=&id=<padId>&normal=1&outformat=1&noEscape=1&commandsFormat=1&doc_chunk_version=3&preview_token=&doc_chunk_flag=1&callback=clientVarsCallback&xsrf=&t=<randhex>
Referer: https://docs.qq.com/doc/<padId>
User-Agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64)
```

PowerShell reference (save the raw response to avoid encoding loss):

```powershell
$u="https://docs.qq.com/dop-api/opendoc?u=&id=<padId>&normal=1&outformat=1&noEscape=1&commandsFormat=1&doc_chunk_version=3&preview_token=&doc_chunk_flag=1&callback=clientVarsCallback&xsrf=&t=$(Get-Random -Maximum 0x7fffffff)"
$r = Invoke-WebRequest -Uri $u -UseBasicParsing -TimeoutSec 30 -UserAgent "Mozilla/5.0" -Headers @{Referer="https://docs.qq.com/doc/<padId>"}
$r.Content | Out-File "$env:TEMP\qqdoc.jsonp" -Encoding utf8
```

### 3.3 Extract & Decode the Body

The `initialAttributedText.text` field in the response holds the **base64-encoded full text** (may be a chunked `text` array — concatenate and decode each chunk):

```javascript
// Node script: parse JSONP → extract text → base64-decode → write file
const fs = require('fs');
const raw = fs.readFileSync(process.env.TEMP + '\\qqdoc.jsonp', 'utf8');
const json = JSON.parse(raw.replace(/^clientVarsCallback\(/, '').replace(/\)\s*;?\s*$/, ''));
const body = json.clientVars.collab_client_vars;
let text = '';
for (const chunk of body.initialAttributedText.text || []) {
  text += Buffer.from(chunk, 'base64').toString('utf8');
}
fs.writeFileSync(process.env.TEMP + '\\qqdoc_text.txt', text, 'utf8');
console.log('DECODED_LEN=' + text.length);
```

> ⚠️ Printing Chinese text in a PowerShell console often shows mojibake (GBK codepage) — **write decode results to a file and read them with file_read; never trust the console display.**

### 3.4 Threat Analysis of the Body (After You Have the Full Text)

In the decoded body you MUST check for:

- `HYPERLINK` / `http(s)://` **external links** (especially those after lures like "点此办理 / 点此进入 / 立即领取" / "click to proceed") → run threat intel on the external link's domain
- Lure copy: "补贴 / 退税 / 社保 / 薪资核对 / 验证码 / 绑定银行卡 / 额度验证" (subsidy/refund/social-security/salary-validation/verification-code/bind-bank-card)
- Form lures: "填写工号 / 身份证号 / 银行卡号" (fill in employee-ID / ID-card / bank-card number)
- Attachment / QR-code image lures
- Word-count authenticity: if the page shows a large word count but the body is actually short → check for **padding text** (ad/spam text stuffed to look legit; in the COSCO case the body contained repeated Youdao-Notes ad paragraphs to fake "25 pages / 16583 words")

## 4. Document Metadata Analysis (Key Triage Signal)

| Field                                      | Legitimate corporate doc               | Malicious doc traits                              |
| ------------------------------------------ | -------------------------------------- | ------------------------------------------------- |
| Creator nickname                           | Real name / corporate name             | Random string (e.g. `24-26`)                      |
| Creator account type                       | Corp-bound (corpId/corpName non-empty) | **Personal account** (corpId empty)               |
| Create time                                | Matches the document content           | **Same day/hour as the phishing email**           |
| Create → modify span                       | Manual editing, minutes+               | **"Done" in seconds** (batch-template generation) |
| Page/word count vs body                    | Consistent                             | Word-padded (inflated stats)                      |
| Access policy                              | Members / internal                     | guestPolicy=deny, share-link-only readable        |
| Body initial value `initialAttributedText` | Real content                           | Empty, or only recoverable after base64 decoding  |

> **Timestamp self-proof method**: compare document createTime with the email send time and the ICS DTSTAMP. If all three happen within ~30 minutes of each other → highly suspicious (orchestrated attacker landing flow).

## 5. Other Platforms (General Guide)

| Platform                                      | Penetration approach                                                                                                                                |
| --------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| Google Docs `docs.google.com/document/d/<id>` | `https://docs.google.com/document/d/<id>/export?format=txt` (public links yield plaintext directly); ServerAction=openView page + check filePreview |
| Yuque `yuque.com`                             | Public docs: try `export/markdown` endpoint; page `__INITIAL_STATE__`                                                                               |
| Kingsoft Docs `kdocs.cn` / `ksyun`            | Check `?preview` parameter and share API                                                                                                            |
| Notion `notion.site`                          | Embedded JSON in page: search `signedUrl` / `block` structure; body stored as JSON in `__INITIAL_STATE__`                                           |
| Shimo `shimo.im`                              | `/docs/<id>` apis endpoint, readable with login state                                                                                               |
| OneDrive/SharePoint shared                    | `/download.aspx` / embed endpoint                                                                                                                   |

> General tricks: check page HTML for globals like `__INITIAL_STATE__` / `basicClientVars` / `window.__DATA__`; look for `export`/`download`/`print` endpoints to bypass the render layer; Canvas-rendered body is usually delivered to the client in the **API response** as base64/JSON, **not in the DOM**.

## 6. Verdict Integration

- Doc embeds an external link to an **unknown domain** → run TB/VT intel on it; if the domain is malicious → **PHISHING, hard evidence**
- Anonymous/personal-account creator + doc created the same day as the email → strongly suspicious
- Doc body contradicts the email subject (subject "待办文件 to-do file", body "补贴申领 subsidy claim") → **content swap**, escalate immediately

## 7. Real Case (2026-08-27 CC Phishing)

- Email: internal forwarding chain (real domain) + Tencent Docs link, subject "8月19号待办文件"
- Surface: domain ICP/whitelisted, link opens, "25 pages / 16583 words"
- After penetration: opendoc API base64-decode revealed 《关于2026年五险一金补贴申领通知》("2026 Five-Insurance-One-Fund Subsidy Claim Notice") with embedded `HYPERLINK https://xlvgn.cn` ("点此进行办理 click to proceed")
- Intel: xlvgn.cn → ThreatBook verdict **Phishing + GoldenCicada (金蝉) group**, personal registration, no ICP
- Lesson: **platform-doc link + real internal email chain = the most powerful trust broker**; only by penetrating the document content can you reach a verdict
