# NTFS fast mode: implementation stages

## Stage 1: MFT snapshot helper

`okra-index-helper.exe` is a separate Windows binary with an embedded `requireAdministrator`
manifest. It can enumerate NTFS metadata through `FSCTL_ENUM_USN_DATA`, reconstruct paths from
file-reference/parent-reference pairs, filter to configured roots and depth, and export the
existing `okra-index-v1` TSV cache format.

## Stage 2: persistent USN-journal synchronization

The helper now performs an initial MFT baseline and replays journal records that overlapped the
scan. It queries the journal before and after enumeration, then reads from the earlier `NextUsn`
to close the race between the MFT scan and live file changes. It checks the journal identifier,
`FirstUsn`, `LowestValidUsn`, and the saved cursor before resuming.

The state file stores the volume, configured roots, depth, journal ID, cursor, and MFT
file-reference graph. Its default path is the snapshot path with a `.state` extension; pass
`--state <path>` to choose another location. The TSV cache remains in the `okra-index-v1` format
so existing cache readers do not need to change.

On later runs, the helper reads only journal records after the saved cursor and updates records by
file reference. Creates and ordinary updates insert or refresh an entry; deletes remove it; rename
and move records replace the entry's parent reference/name. Because descendants retain their
parent references, renaming a directory updates descendant paths during export without rewriting
each descendant record.

Run once to initialize or synchronize:

```powershell
.\target\release\okra-index-helper.exe `
  --volume C: `
  --output "$env:LOCALAPPDATA\okra-brown-search\mft-snapshot.tsv" `
  --max-depth 8 `
  --root "$env:USERPROFILE"
```

Use `--watch` to keep the elevated helper running and poll the journal. The default polling interval
is two seconds; `--poll-seconds 5` changes it to five seconds. Press Ctrl+C to stop. The watch mode
is a development process, not an installed Windows service. Run only one helper instance for a given
volume and state file at a time.

If the journal is not active, the helper reports that condition and does not silently create one.
An administrator can inspect it with `fsutil usn queryjournal C:`. To enable it, consult the
`fsutil usn createjournal` documentation and choose suitable `m=` (maximum size) and `a=` (allocation
delta) values; both are specified in bytes. Do not copy arbitrary small example values onto a
live volume.

If the journal is recreated, its ID changes, or the saved cursor falls behind the retained journal
history, the helper discards the old in-memory graph and rebuilds from the MFT. A failed or
incompatible state file also causes a rebuild. The cache is written before the cursor state, so a
crash between those writes causes safe replay rather than silently skipping changes.

## Stage 3: logon-session-restricted local search IPC

The helper now supports `--serve`. This implies watch mode: while the elevated process maintains the
USN cursor and writes the snapshot, a named-pipe server exposes bounded file-search requests to the
ordinary launcher. The pipe name is `\\.\pipe\okra-brown-search-v1`.

The helper builds a pipe DACL from its process token's logon-session SID and grants access only to
that interactive logon session and Local System; remote pipe clients are rejected. The server claims
the name with `FILE_FLAG_FIRST_PIPE_INSTANCE` and stops if another process already owns it, rather
than attaching to a potentially spoofed endpoint. The elevated helper and unelevated launcher must
run in the same Windows logon session (elevating with a different administrator account will not
match the launcher's logon-session SID). This prevents a different terminal-services session using
the same account from opening the pipe. The pipe has an explicit medium-integrity mandatory label,
so the standard-integrity desktop app can write requests without granting write access to lower-
integrity clients. The helper enables `SeSecurityPrivilege` only during pipe creation and restores
its previous state immediately afterward. The protocol is versioned and length-prefixed,
validates query and result bounds, and only returns paths from the configured-root snapshot. A
missing or unavailable helper is treated as a normal backend miss, so the app falls through to the
existing built-in walker index. The app keeps its priority order: Everything SDK, `es.exe`, the Okra
NTFS helper, then the local walker fallback. The explicit “Everything only” and “Built-in index only”
settings remain unchanged.

Start the elevated helper in an Administrator PowerShell window and keep it open:

```powershell
$indexDir = "$env:LOCALAPPDATA\okra-brown-search"
& ".\target\release\okra-index-helper.exe" `
  --volume C: `
  --output "$indexDir\mft-snapshot.tsv" `
  --state "$indexDir\mft-snapshot.state" `
  --max-depth 8 `
  --root "$env:USERPROFILE" `
  --serve `
  --poll-seconds 1
```

Then start Okra normally (without elevation). In Automatic file-backend mode, file queries use
Everything first when it is available, then the named-pipe helper, then the existing unprivileged
walker index. Stage 4 below installs it as a logon task so you do not have to start it by hand; for
manual testing, close it with Ctrl+C.

## Stage 4: automatic startup, health status, and graceful shutdown

### Install (Administrator PowerShell, once)

`--install-task` registers a Task Scheduler task named `Okra Brown Search\NTFS Helper` that runs
the helper at *your* logon, elevated ("highest privileges", interactive token, no stored
password), so there is no UAC prompt at startup and the launcher stays unprivileged. It takes
the same index options you used manually; the task adds `--serve --background` itself.

```powershell
$indexDir = "$env:LOCALAPPDATA\okra-brown-search"
& ".\target\release\okra-index-helper.exe" `
  --install-task --start-now `
  --volume C: `
  --output "$indexDir\mft-snapshot.tsv" `
  --state "$indexDir\mft-snapshot.state" `
  --max-depth 8 `
  --root "$env:USERPROFILE" `
  --poll-seconds 1
```

Remove it again with `okra-index-helper.exe --uninstall-task` (the index files in
`%LOCALAPPDATA%\okra-brown-search` are kept).

**The task runs a copy, not your build output.** The installer copies the helper to
`C:\Program Files\Okra Brown Search\` (administrator-write only) and registers that copy. If it
pointed at `target\release\`, any process running as you could replace that executable and
obtain elevation silently at the next logon. Re-run `--install-task` after rebuilding to update
the installed copy. `schtasks.exe` and the Windows directory are located through the OS, not
through PATH or environment variables.

### What the task gives you

| Requirement | Mechanism |
| --- | --- |
| Start at logon, elevated, same account | `LogonTrigger` for your account + `HighestAvailable` + `InteractiveToken` |
| Resume the persistent index | unchanged Stage 2 resume (journal ID + cursor checks, MFT rebuild if invalid) |
| Pipe + continuous USN polling | `--serve` (pipe is created first, then the sync loop runs) |
| One instance only | `MultipleInstancesPolicy=IgnoreNew`, plus `FILE_FLAG_FIRST_PIPE_INSTANCE` |
| Crash / failure recovery | `RestartOnFailure`: every 1 minute, up to 999 times, after a non-zero exit |
| No runtime limit | `ExecutionTimeLimit=PT0S` |
| Quiet background run | `--background`: console detached, output goes to `helper.log` next to the installed exe (rotated at 1 MiB) |

A helper that cannot claim the secure pipe, or whose first synchronization fails, exits non-zero
(so the scheduler retries). It never starts with a weaker pipe ACL.

### Health reporting

The pipe now answers a second request, `OKS1`, with a fixed 28-byte `OKT1` reply: state
(starting / ready / stopping), indexed path count, helper pid, time of the last successful
synchronization, and consecutive failed synchronizations. `okra_core::providers::files::
ntfs_helper_status()` (also `FilesProvider::helper_status()`) turns that into:

`NotRunning`, `Busy`, `Starting`, `Ready { entries, last_sync_age_secs, pid, failed_syncs }`,
`Stale`, `Stopping`, `Incompatible` (an older helper that must be restarted after an upgrade).

Staleness is judged on the launcher's own clock. The helper also enforces it server-side: while it
is starting or stopping, or if its sync loop has not completed a pass for 60 s, it refuses
searches with a non-zero response status. The launcher already treats any non-zero status as a
backend miss, so results fall through to the walker index instead of being served out of date.
Because the pipe is now claimed before the first baseline finishes, a rebuild shows up as
`Starting` instead of as a missing helper.

### Graceful shutdown

Ctrl+C, Ctrl+Break, console close, logoff, and system shutdown set a flag; the sync loop exits
within about 100 ms, marks the helper `Stopping`, and exits with status 0 (no scheduler restart).
`schtasks /End` and a crash are hard kills. They are safe because the snapshot is written before
the cursor and both are replaced atomically, so the next start replays from the last saved cursor.

### Verifying it on Windows

1. Git Bash (normal, not elevated), from the repository: `cargo run -p okra-core --example
   helper_status`. This runs unprivileged exactly like the launcher. Expect `ready` and exit code 0
   after the helper has started. If the helper is running but this prints `not running`, the task's
   token is **not** in your logon session (the pipe ACL is keyed to the logon-session SID);
   please report that, because it changes the Stage 4 design.
2. Sign out and back in, wait a few seconds, repeat step 1.
3. Kill the helper from Task Manager (or `taskkill /F /IM okra-index-helper.exe` in Administrator
   PowerShell); within about a minute the scheduler restarts it. Then run step 1 again.
4. Read `C:\Program Files\Okra Brown Search\helper.log`.

Task Scheduler's restart-on-failure behaviour for a hard-killed process, the logon-session match,
and the `--background` log redirection have been type-checked for Windows but not executed; they
need step 1 to 4 above before being treated as verified.

## Stage 5: launcher health display

The Settings page now displays the optional helper's current health using the existing `OKS1` / `OKT1` status protocol. It shows the state, indexed path count, last successful synchronization age, process ID, and consecutive synchronization failures when the helper is ready. A manual Refresh action is available; while Settings remains open, the UI also refreshes every five seconds.

Status requests run on one dedicated background worker, not on the egui update or search path. Requests use a one-slot queue so repeated refreshes coalesce. This is important because the current named-pipe server does not yet enforce I/O timeouts: a stalled health request can block its worker, but it cannot freeze typing, rendering, or the launcher itself. Checking health never starts the helper and never prompts for elevation.

On non-Windows builds the existing status API reports `NotRunning`, as expected for a Windows-only helper.

## Current constraints

- This stage reads `USN_RECORD_V2`, appropriate for the current NTFS implementation. ReFS
  128-bit file identifiers and USN record versions 3/4 are not implemented. Unknown record
  versions fail closed: the helper reports the unsupported version and does not save a later
  cursor, rather than silently skipping a change.
- Hard links are not a complete multi-name model. The current graph stores one entry per file
  reference; hard-link changes need dedicated handling before claiming full Everything parity.
- The persistent state retains the full MFT graph, not just configured-root entries. Memory and
  disk use will need measurement and optimization on very large volumes.
- The helper is a per-user logon task (Stage 4), not a Windows service. The launcher never starts it
  or requests elevation; when it is not running, the app uses its normal fallback chain.
- The pipe server has no I/O timeouts and serves one client at a time, so a same-session process that
  connects and then stalls can block searches (they fall back) until it disconnects. Overlapped I/O
  with timeouts is a separate hardening item.
- The index files live in `%LOCALAPPDATA%`, which is writable by your account, while the helper
  that reads and writes them is elevated. Moving the elevated helper's state to an
  administrator-only location is a separate hardening item.
- The pipe server is a single-request-at-a-time development bridge. It reloads the TSV only when
  its file metadata changes.
- The IPC endpoint returns at most 200 filename-fuzzy matches per request and accepts paths up to
  128 KiB in UTF-8. It is intended for the current configured roots and depth, not as a
  general-purpose filesystem broker. It handles one request at a time; if busy, the app falls
  through to the built-in index for that query.
- Non-NTFS volumes, unsupported record versions, inaccessible records, and helper failures still
  require the normal file-search fallback.

## Next stages

1. Add hard-link and unsupported-record handling, exclusions, and robust recovery tests.
2. Harden named-pipe IPC with bounded I/O timeouts and move elevated-helper state away from the
   user-writable `%LOCALAPPDATA%` location before treating the helper as production-hardened.
3. Benchmark cold start, incremental update latency, query latency, CPU, disk activity, and memory
   at 100k / 1M / 5M entries before changing defaults.
