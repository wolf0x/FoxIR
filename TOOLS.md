# TOOLS.md — Local Environment Configuration

## Windows Execution Rules

### Core Rules

1. **Always append the `.exe` extension explicitly for executable invocations**  
   ✅ Correct: `python.exe script.py`, `git.exe status`  
   ❌ Wrong: `python script.py` (may trigger file association or ambiguous matching)

2. **Use the full absolute path when a program is not on the system `%PATH%`**  
   Query the location with `where.exe <command>` (e.g. `where.exe python`)

3. **Invoke PowerShell scripts uniformly with `powershell.exe -File <script path>`**  
   Or use `pwsh.exe` (if PowerShell Core is installed)

4. **CMD built-ins (e.g. `dir`, `copy`, `del`) need no suffix; external tools follow the rules**

5. **Quote paths that contain spaces**  
   Example: `"C:\Program Files\Git\bin\git.exe"`

### Local Paths (adjust to the actual environment)

- Git: `"C:\Program Files\Git\bin\git.exe"`
- Python: `"%USERPROFILE%\AppData\Local\Programs\Python\Python312\python.exe"`
- Node: `"C:\Program Files\nodejs\node.exe"`
- Workspace: `%USERPROFILE%\.RustAgent\workspace`
- Output: `%USERPROFILE%\.RustAgent\workspace\output`

### Output Artifacts

**All produced files must be written to the `output/` directory** with descriptive names:

- Report: `output/ir_report_2026-08-07_suspicious_process.html`
- Screenshot: `output/screenshot_desktop_2026-08-07_143022.png`
- Analysis result: `output/yara_scan_results_2026-08-07.json`

**When referencing artifacts, use the full path returned by the tool** (already
normalized to forward slashes). Do not shorten to the bare filename.

## Local Environment Notes

_Add your environment-specific configuration here:_
- SSH hosts: (to be configured)
- Camera name: (to be configured)
- Voice preference: (to be configured)
- Other: (to be configured)
