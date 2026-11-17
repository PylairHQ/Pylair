# W0 Windows manual checklist - about 15 min total

For Alby. The automatable rows are already verified (see README.md,
"Windows (win64) runs"): Job Object tree-kill, hung-server probe, IPC
rejection from the remote page, server-kill recovery, graceful and hard
app exit with zero survivors, launch-to-ready times.

What remains is the human half of the kill criterion: upload and
download must work in the webview without invasive hacks, plus the
usual page-level checks. Fill the results table at the bottom.

## Setup (once, about 1 min)

Run from the worktree spike folder in PowerShell:

```powershell
cd C:\Users\alber\CascadeProjects\Pylair-w0b\spikes\w0-wrap
cargo build --manifest-path src-tauri\Cargo.toml   # only if exe is stale
$env:W0_PID_FILE = "$env:TEMP\w0-pids.txt"
```

## fastapi (~5 min)

```powershell
Remove-Item $env:TEMP\w0-pids.txt -ErrorAction Ignore
$env:W0_APP = 'fastapi'
.\src-tauri\target\debug\w0-wrap.exe
```

A window opens, shows "starting", then navigates to the app page.

1. **Page load.** Page shows the heading `W0-WRAP-MARKER`; SSE ticks
   (`tick0 tick1 ...`) appear in the page log.
2. **Upload (picker).** In the upload form click the file input, pick a
   file in the native picker, submit. Page reports the file name + size.
3. **Upload (drag-drop).** Drag a file from Explorer onto the file
   input, submit. Same result expected.
4. **Download.** Click "download file". Does a save dialog appear?
   Where does `w0-download.txt` land (check `$env:USERPROFILE\Downloads`
   and the page itself for a nav-to-content quirk)? Contents should be
   `W0-WRAP-MARKER`.
5. **External link.** Click "external link". Expected on plain wry:
   silently swallowed (no handler is registered in the spike). Note if
   it instead opens the system browser or navigates in-window.
6. **Clipboard.** Select text in the page, copy, paste into Notepad.
   Paste text into the page if there is an input.
7. **Kill the server.** In a SECOND PowerShell window:

   ```powershell
   Get-Content $env:TEMP\w0-pids.txt        # note the "server <pid>" line
   taskkill /F /PID <server-pid>
   ```

   Watch the app window: it should briefly show a dead page, then the
   supervisor restarts the launcher on a NEW port and the window
   re-navigates and reloads. Note whether you see an error page in
   between.
8. **Close the window.** Then verify nothing is left, checked by the
   worktree path (not by process name):

   ```powershell
   Get-CimInstance Win32_Process -Filter "Name like 'python%'" |
     Where-Object { $_.ExecutablePath -like '*Pylair-w0b*' }
   ```

   Expected: empty output.

## gradio (~5 min)

```powershell
$env:W0_APP = 'gradio'
.\src-tauri\target\debug\w0-wrap.exe
```

Startup is slow (about 30s to READY on this machine) - the window sits
on "starting" that long. That is gradio import time, not a hang.

1. **Page load.** Gradio UI renders.
2. **Chat.** Send a message, echo appears.
3. **File up/down.** Upload a file via the file widget; trigger the
   file output, download it. Dialog? Landing folder?
4. **External link** (if present) - same expectation as fastapi.
5. **Kill + close** - repeat steps 7-8 from fastapi.

## streamlit (~4 min)

```powershell
$env:W0_APP = 'streamlit'
.\src-tauri\target\debug\w0-wrap.exe
```

1. **Page load.** Streamlit renders, widgets interact.
2. **Upload.** Use the file uploader widget.
3. **Download.** `st.download_button` - dialog? landing folder? On
   macOS it silently dropped into ~/Downloads with zero feedback; note
   the Windows behaviour.
4. **Session loss on restart.** Type into a widget, then kill the
   server PID (step 7 above). The page reloads on a new port. Expected:
   your input is gone (Streamlit sessions are in-memory). Confirm you
   see the reload, not a stranded dead page.
5. **Close the window**, verify no `Pylair-w0b` python processes remain
   (step 8 above).

## Results

| row | fastapi | gradio | streamlit |
|---|---|---|---|
| page load | | | |
| upload via file picker | | | |
| upload via drag-drop | | | |
| download: dialog? where does it land? | | | |
| external link opens system browser | | | |
| clipboard copy/paste | | | |
| kill server PID: window recovers | | | |
| close window: no venv python left | | | |

Copy filled cells into the "win" columns of the matrix in README.md.
