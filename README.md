# Okra Brown Search

A minimal desktop search launcher written in Rust, focused on Windows. It searches installed programs, files, Windows Settings, and currently open windows. It has no plugin system or plugin section.

![Okra Brown Search logo](assets/okra-brown-search.png)

## Search features

- **Programs:** Start Menu shortcuts and registered Windows applications, including packaged/UWP apps discovered through `Get-StartApps`.
- **Files and folders:** voidtools Everything is the primary backend. The app tries the Everything SDK DLL, then `es.exe`, then its built-in filename index.
- **Windows Settings:** direct shortcuts to common `ms-settings:` pages plus useful administrative tools such as Task Manager, Device Manager, Disk Management, Services, and Event Viewer.
- **Open windows:** enumerate visible top-level windows and bring the selected window to the foreground.
- **Ranking:** fuzzy matching, prefix/word-boundary weighting, and a small local usage-history boost.
- **Resident launcher:** Ctrl + Alt + Space opens the search window and pressing it again closes it; Escape or a click elsewhere also hides it. The app remains in the notification area until Exit is selected from the tray menu. It can optionally launch hidden when you sign in to Windows (the window is never shown in that case, so there is no flash), and a single-instance guard prevents duplicate copies.
- **Quick open:** the first nine results show `Ctrl+1` ... `Ctrl+9`; pressing one opens that result without the arrow keys.
- **Preview pane:** the window is split like macOS Spotlight: results on the left, details of the selected result on the right. Files and folders show their icon, type, size (or item count), modified and created dates and location; PNG, JPEG, GIF and BMP images show a thumbnail and their pixel size; text files show their first lines; apps show where they live and how often you opened them; settings show what they open; calculator answers are shown large. File details are read on a worker thread, so moving through results never waits on the disk.
- **Starts empty:** the launcher opens as just the search bar, centred on the screen where the mouse is, with no results until you type. The window itself keeps a fixed size so the bar never jumps while you type.
- **Appearance:** warm-light and dark themes, a choice of the bundled Inter font or the system Segoe UI, plus a compact settings screen.
- **Heads-up messages:** if the Ctrl + Alt + Space shortcut is already taken by another program, or Everything is missing so file search is slower, a short note shows in the bar for the first few opens.

## Search syntax

Unprefixed text searches across programs, file names, Windows Settings, and open windows. Prefixes narrow the search:

| Prefix | Search scope | Example |
|---|---|---|
| `a ` | Applications | `a firefox` |
| `f ` | Files and folders | `f report.pdf` |
| `s ` | Windows Settings and system tools | `s bluetooth` |
| `win ` | Currently open windows | `win browser` |

The file query is passed to Everything unchanged, so Everything operators are available, for example:

- `f ext:pdf report`
- `f size:>10mb`
- `f dm:today`
- `f content:invoice` (requires content indexing to be enabled in Everything)

## Performance design

- **Instant start.** The installed-app list is cached in `%LOCALAPPDATA%\okra-brown-search\apps.json` and loaded immediately; the slow part (Start Menu walk plus `Get-StartApps` via PowerShell) runs on a background thread, rewrites the cache, and re-runs the query on screen when it finishes. It repeats every 30 minutes so newly installed apps appear without a restart.
- **Nothing slow on the keystroke path.** Typing is coalesced on a worker thread; only the newest query is ever searched.
- **Allocation-free fuzzy matcher.** The query is prepared once per search, non-matches are rejected without allocating, and scoring is linear in name length. A randomized test checks it against the original reference implementation (identical scores and matched positions).
- **Fast built-in index.** Each indexed file carries a 64-bit character signature in a contiguous array, so most files are rejected with one AND before any text is read; only the best results are selected rather than sorting every hit.
- **Cheap ranking.** Usage history is resolved once per search (not once per result), so scoring hundreds of results allocates nothing. History is saved by a background thread with an atomic write-then-rename, so opening a result never waits on the disk.
- **Draw once, not every frame.** Highlight ranges and shortened paths are computed on the search thread when results arrive; the UI only draws them.
- **Real program icons, off the UI thread.** Results show each program's own icon (Start-menu shortcuts, Store apps, and per-type icons for files and folders), fetched from the shell by a worker thread, rows on screen first and cached for the session. Rows appear instantly with a coloured letter tile that is swapped for the icon as it arrives, so typing never waits on icons. Settings, open windows and calculator results keep the tile.
- **Open windows are listed once per moment, not once per key.** The list of open windows is reused for under a second while you type.
- **Closes when you click away.** On Windows the launcher asks the OS which window is in the foreground and is woken by a foreground-change hook, so there is no polling while it is open. If something misbehaves, set `OKRA_DEBUG=1` to log show/hide/focus events to `%TEMP%\okra-debug.log`.
- **Everything is still the fast path.** When Everything is running, file queries are answered by its own index; the built-in index is only a fallback.

Micro-benchmarks: `cargo run --release -p okra-core --example bench`, and for the real built-in index path `cargo test --release -p okra-core -- --ignored --nocapture bench_builtin`; for per-keystroke ranking overhead `... bench_engine_overhead`.

## Requirements

- Windows 10 or Windows 11 (the global hotkey, tray, Start app discovery, Settings shortcuts, and window switching are Windows features).
- Rust 1.80 or newer.
- [voidtools Everything](https://www.voidtools.com/) installed and running for fast indexed file search. Install the Everything SDK DLL or `es.exe` if you want the app to use those transports; the built-in filename index remains a fallback.

## Build and run

This archive contains the complete Rust source and assets, not a precompiled `.exe`. From the repository root on Windows:

```powershell
cargo run --release -p okra-brown-search
```

Create a release binary:

```powershell
cargo build --release -p okra-brown-search
```

The executable is under `target\release\okra-brown-search.exe`. The included GitHub Actions workflow builds and tests the project on a Windows runner when the repository is pushed to GitHub.

## Everything configuration

The default `auto` mode tries:

1. `Everything64.dll` / `Everything32.dll` from the executable folder, normal DLL search locations, or the configured `everything_dll` path.
2. `es.exe` on `PATH`, under the Everything installation folder, or the configured `es_exe` path. This is only probed when the DLL is not found, because both talk to the same running Everything instance.
3. The built-in filename index over the configured `file_roots`.

Configuration is stored in `%APPDATA%\okra-brown-search\config.json` (or the platform config directory for non-Windows builds). The Settings page also lets you select the backend, theme, font, maximum result count, and optional Windows sign-in startup. Enabling sign-in startup adds an `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` entry; disabling it removes that entry. Backend and result-count changes are applied on the next launch.

Example configuration:

```json
{
  "file_backend": "auto",
  "everything_dll": null,
  "es_exe": null,
  "file_roots": ["C:\\Users\\YourName"],
  "file_max_depth": 8,
  "max_results": 40,
  "theme": "warm_light",
  "font": "inter",
  "start_with_windows": false
}
```

## Current boundaries

This build includes the resident launcher and Windows top-level window switching. It does **not** yet index browser tabs, keep a clipboard-history database, or extract/search text inside documents itself. `content:` queries are delegated to Everything and therefore depend on Everything's own content-index settings. The built-in filename index is a fallback snapshot; Everything is the live-updating file index. No plugin loader is included by design.

## Project layout

- `crates/okra-core` — providers, Everything integration, fuzzy matching, settings catalog, activation, and usage history.
- `crates/okra-app` — egui interface (`preview.rs` is the preview pane), resident global hotkey (toggle), tray menu, single-instance guard, startup integration, and settings UI. The window is found by process and title and cached, not looked up by title alone.
- `assets` — logo and icons (window/app icon, `.ico`, in-app mark, tray icon).
- `tools/make_icons.py` — regenerates every file in `assets` from one definition (`pip install pillow numpy`, then `python tools/make_icons.py`).
