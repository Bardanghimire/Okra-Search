# Okra Brown Search

A minimal desktop search launcher written in Rust, focused on Windows. It searches installed programs, files, Windows Settings, and currently open windows. It has no plugin system or plugin section.

![Okra Brown Search logo](assets/okra-brown-search.png)

## Search features

- **Programs:** Start Menu shortcuts and registered Windows applications, including packaged/UWP apps discovered through `Get-StartApps`.
- **Files and folders:** Okra's own NTFS MFT/USN index is the primary fast backend. A built-in unprivileged filesystem index provides fallback search when the helper is unavailable or a path is outside its indexed scope. No external search application is required or used.
- **Windows Settings:** direct shortcuts to common `ms-settings:` pages plus useful administrative tools such as Task Manager, Device Manager, Disk Management, Services, and Event Viewer.
- **Open windows:** enumerate visible top-level windows and bring the selected window to the foreground.
- **Ranking:** fuzzy matching, prefix/word-boundary weighting, and a small local usage-history boost.
- **Resident launcher:** Ctrl + Alt + Space opens the search window and pressing it again closes it; Escape or a click elsewhere also hides it. The app remains in the notification area until Exit is selected from the tray menu. It can optionally launch hidden when you sign in to Windows (the window is never shown in that case, so there is no flash), and a single-instance guard prevents duplicate copies.
- **Quick open:** the first nine results show `Ctrl+1` ... `Ctrl+9`; pressing one opens that result without the arrow keys.
- **Preview pane:** the window is split like macOS Spotlight: results on the left, details of the selected result on the right. Files and folders show their icon, type, size (or item count), modified and created dates and location; PNG, JPEG, GIF and BMP images show a thumbnail and their pixel size; text files show their first lines; apps show where they live and how often you opened them; settings show what they open; calculator answers are shown large. File details are read on a worker thread, so moving through results never waits on the disk.
- **Starts empty:** the launcher opens as just the search bar, centred on the screen where the mouse is, with no results until you type. The window itself keeps a fixed size so the bar never jumps while you type.
- **Appearance:** warm-light and dark themes, a choice of the bundled Inter font or the system Segoe UI, plus a compact settings screen.
- **NTFS helper diagnostics:** Settings shows whether the optional elevated helper is ready, starting, stale, stopped, or incompatible, together with index size and last-sync age when available. Health checks run on a background worker and never start the helper or request elevation.
- **Heads-up messages:** if the Ctrl + Alt + Space shortcut is already taken by another program, a short note shows in the bar for the first few opens.

## Search syntax

Unprefixed text searches across programs, file names, Windows Settings, and open windows. Prefixes narrow the search:

| Prefix | Search scope | Example |
|---|---|---|
| `a ` | Applications | `a firefox` |
| `f ` | Files and folders | `f report.pdf` |
| `s ` | Windows Settings and system tools | `s bluetooth` |
| `win ` | Currently open windows | `win browser` |

File queries use Okra's native fuzzy filename matcher. Search by part of a filename or folder name, for example `f report`, `f Cargo.toml`, or `f notes`. Filter operators and document-content indexing are not implemented yet; they are planned as native Okra features rather than delegated to another search product.

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
- **Native index first.** On Windows, file queries go to Okra's elevated NTFS helper first and then to Okra's built-in walker index. The fallback is warmed in the background so helper outages do not trigger a cold scan on the first query.

Micro-benchmarks: `cargo run --release -p okra-core --example bench`, and for the real built-in index path `cargo test --release -p okra-core -- --ignored --nocapture bench_builtin`; for per-keystroke ranking overhead `... bench_engine_overhead`.

## Requirements

- Windows 10 or Windows 11 (the global hotkey, tray, Start app discovery, Settings shortcuts, and window switching are Windows features).
- Rust 1.80 or newer.
- No external file-search application is required. For the fastest indexed search on NTFS, install Okra's own elevated NTFS helper using the instructions in `docs/ntfs-fast-mode.md`; filename search still has a built-in fallback when the helper is unavailable.

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

## Native index configuration

Configuration is stored in `%APPDATA%\okra-brown-search\config.json` (or the platform config directory for non-Windows builds). The Settings page lets you choose the theme, font, maximum result count, and optional Windows sign-in startup. Index roots and maximum depth are currently configured in `config.json`; the default root is the current user's home folder and the default maximum depth is 8.

Example configuration:

```json
{
  "file_roots": ["C:\\Users\\YourName"],
  "file_max_depth": 8,
  "max_results": 40,
  "theme": "warm_light",
  "font": "inter",
  "start_with_windows": false
}
```

Legacy `file_backend`, `everything_dll`, and `es_exe` fields from older config files are ignored. Okra no longer loads the Everything SDK, probes for `es.exe`, or sends queries to an external search process.

## Current boundaries

This build includes the resident launcher and Windows top-level window switching. It does **not** yet index browser tabs, keep a clipboard-history database, or extract/search text inside documents. Native filename search is implemented; native `ext:`, size/date filters, regex, and document-content indexing are future work. The NTFS helper incrementally follows the USN journal for its configured roots; the built-in filename index walks the configured roots as a fallback. No plugin loader is included by design. The current default index scope is the user's home folder at depth 8, not every path on every volume.

## Project layout

- `crates/okra-core` — native file-index integration, providers, fuzzy matching, settings catalog, activation, and usage history.
- `crates/okra-app` — egui interface (`preview.rs` is the preview pane), resident global hotkey (toggle), tray menu, single-instance guard, startup integration, and settings UI. The window is found by process and title and cached, not looked up by title alone.
- `assets` — logo and icons (window/app icon, `.ico`, in-app mark, tray icon).
- `tools/make_icons.py` — regenerates every file in `assets` from one definition (`pip install pillow numpy`, then `python tools/make_icons.py`).
