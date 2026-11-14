# Okra Brown Search

A minimal desktop search launcher written in Rust, focused on Windows. It searches installed programs, files, Windows Settings, and currently open windows. It has no plugin system or plugin section.

![Okra Brown Search logo](assets/okra-brown-search.png)

## Search features

- **Programs:** Start Menu shortcuts and registered Windows applications, including packaged/UWP apps discovered through `Get-StartApps`.
- **Files and folders:** voidtools Everything is the primary backend. The app tries the Everything SDK DLL, then `es.exe`, then its built-in filename index.
- **Windows Settings:** direct shortcuts to common `ms-settings:` pages plus useful administrative tools such as Task Manager, Device Manager, Disk Management, Services, and Event Viewer.
- **Open windows:** enumerate visible top-level windows and bring the selected window to the foreground.
- **Ranking:** fuzzy matching, prefix/word-boundary weighting, and a small local usage-history boost.
- **Resident launcher:** Ctrl + Alt + Space opens the search window; Escape hides it. The app remains in the notification area until Exit is selected from the tray menu. It can optionally launch hidden when you sign in to Windows, and a single-instance guard prevents duplicate copies.
- **Appearance:** warm-light and dark themes, plus a compact settings screen.

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
2. `es.exe` on `PATH`, under the Everything installation folder, or the configured `es_exe` path.
3. The built-in filename index over the configured `file_roots`.

Configuration is stored in `%APPDATA%\okra-brown-search\config.json` (or the platform config directory for non-Windows builds). The Settings page also lets you select the backend, theme, maximum result count, and optional Windows sign-in startup. Enabling sign-in startup adds an `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` entry; disabling it removes that entry. Backend and result-count changes are applied on the next launch.

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
  "start_with_windows": false
}
```

## Current boundaries

This build includes the resident launcher and Windows top-level window switching. It does **not** yet index browser tabs, keep a clipboard-history database, or extract/search text inside documents itself. `content:` queries are delegated to Everything and therefore depend on Everything's own content-index settings. The built-in filename index is a fallback snapshot; Everything is the live-updating file index. No plugin loader is included by design.

## Project layout

- `crates/okra-core` — providers, Everything integration, fuzzy matching, settings catalog, activation, and usage history.
- `crates/okra-app` — egui interface, resident global hotkey, tray menu, single-instance guard, startup integration, and settings UI.
- `assets` — Okra Brown Search logo and Windows application icon.
