mod atomic;
mod ipc;
mod journal;
mod mft;
mod shutdown;
mod status;
mod task;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

struct Args {
    volumes: Vec<String>,
    output: PathBuf,
    state: PathBuf,
    max_depth: usize,
    roots: Vec<String>,
    full_volume: bool,
    watch: bool,
    serve: bool,
    poll_seconds: u64,
    background: bool,
    install_task: bool,
    start_now: bool,
}

enum Command {
    Run(Args),
    InstallTask { args: Args, start_now: bool },
    UninstallTask,
}

fn usage() -> &'static str {
    "Usage: okra-index-helper --volume C: [--volume D: ...] --output <snapshot.tsv> (--max-depth N --root C:\\Users\\you [--root C:\\Projects] | --full-volume) [--state <index.state>] [--watch] [--serve] [--poll-seconds 2] [--background]\n       Repeat --volume to merge multiple NTFS drives; multiple drives require --full-volume.\n       okra-index-helper --install-task [same options as above] [--start-now]\n       okra-index-helper --uninstall-task"
}

fn take_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("Missing value for {option}\n{}", usage()))
}

fn parse_command() -> Result<Command, String> {
    let argv: Vec<String> = env::args().skip(1).collect();
    // The one command that needs no index configuration is recognized up front, and only
    // when it is the sole argument, so a typo cannot silently change what a longer command
    // line does.
    match argv.as_slice() {
        [only] if only == "--uninstall-task" => return Ok(Command::UninstallTask),
        _ => {}
    }
    let args = parse_args()?;
    if args.install_task {
        let start_now = args.start_now;
        return Ok(Command::InstallTask { args, start_now });
    }
    if args.start_now {
        return Err(format!(
            "--start-now only applies to --install-task\n{}",
            usage()
        ));
    }
    Ok(Command::Run(args))
}

fn resolve_index_scope(
    volume: &str,
    roots: Vec<String>,
    max_depth: Option<usize>,
    full_volume: bool,
) -> Result<(Vec<String>, usize), String> {
    if full_volume {
        if !roots.is_empty() || max_depth.is_some() {
            return Err("--full-volume cannot be combined with --root or --max-depth".into());
        }
        let root = format!("{}\\", volume.trim_end_matches(['\\', '/']));
        return Ok((vec![root], usize::MAX));
    }

    if roots.is_empty() {
        return Err(format!("At least one --root is required\n{}", usage()));
    }
    let max_depth = max_depth.ok_or_else(|| usage().to_string())?;
    Ok((roots, max_depth))
}

fn parse_args() -> Result<Args, String> {
    let mut volumes = Vec::new();
    let mut output = None;
    let mut state = None;
    let mut max_depth = None;
    let mut roots = Vec::new();
    let mut full_volume = false;
    let mut watch = false;
    let mut serve = false;
    let mut poll_seconds = 2u64;
    let mut background = false;
    let mut install_task = false;
    let mut start_now = false;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--watch" => watch = true,
            "--serve" => serve = true,
            "--background" => background = true,
            "--install-task" => install_task = true,
            "--start-now" => start_now = true,
            "--volume" => volumes.push(take_value(&mut args, "--volume")?),
            "--output" => output = Some(PathBuf::from(take_value(&mut args, "--output")?)),
            "--state" => state = Some(PathBuf::from(take_value(&mut args, "--state")?)),
            "--max-depth" => {
                let value = take_value(&mut args, "--max-depth")?;
                max_depth = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| "--max-depth must be a non-negative integer".to_string())?,
                );
            }
            "--root" => roots.push(take_value(&mut args, "--root")?),
            "--full-volume" => full_volume = true,
            "--poll-seconds" => {
                let value = take_value(&mut args, "--poll-seconds")?;
                poll_seconds = value
                    .parse::<u64>()
                    .map_err(|_| "--poll-seconds must be an integer from 1 to 3600".to_string())?;
                if !(1..=3600).contains(&poll_seconds) {
                    return Err("--poll-seconds must be from 1 to 3600".into());
                }
            }
            _ => return Err(format!("Unknown argument: {arg}\n{}", usage())),
        }
    }

    if volumes.is_empty() {
        return Err(usage().to_string());
    }
    let mut normalized = std::collections::HashSet::new();
    for volume in &volumes {
        if !normalized.insert(volume.to_ascii_uppercase()) {
            return Err(format!("Duplicate volume {volume}"));
        }
    }
    if volumes.len() > 1 && !full_volume {
        return Err("Multiple volumes currently require --full-volume so each volume can be indexed independently".into());
    }
    let output = output.ok_or_else(|| usage().to_string())?;
    let state = state.unwrap_or_else(|| output.with_extension("state"));
    let (roots, max_depth) = resolve_index_scope(&volumes[0], roots, max_depth, full_volume)?;
    for volume in &volumes {
        let volume_roots = if full_volume {
            vec![format!("{}\\", volume.trim_end_matches(['\\', '/']))]
        } else {
            roots.clone()
        };
        mft::validate_volume_and_roots(volume, &volume_roots)?;
    }
    let args = Args {
        volumes,
        output,
        state,
        max_depth,
        roots,
        full_volume,
        watch: watch || serve,
        serve,
        poll_seconds,
        background,
        install_task,
        start_now,
    };
    if args.output == args.state {
        return Err("--output and --state must be different files".into());
    }
    Ok(args)
}

fn volume_specific_path(
    base: &std::path::Path,
    volume: &str,
    needs_suffix: bool,
) -> Result<PathBuf, String> {
    let bytes = volume.as_bytes();
    if bytes.len() != 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return Err(format!("Invalid volume name {volume:?}"));
    }
    if !needs_suffix {
        // The first configured volume keeps the legacy single-volume paths. Adding a second
        // drive therefore reuses the first drive's existing MFT state instead of forcing a
        // needless full-volume baseline rebuild.
        return Ok(base.to_path_buf());
    }
    let drive = (bytes[0] as char).to_ascii_uppercase();
    let stem = base
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("Invalid index file name {}", base.display()))?;
    let file_name = match base.extension().and_then(|value| value.to_str()) {
        Some(extension) => format!("{stem}-{drive}.{extension}"),
        None => format!("{stem}-{drive}"),
    };
    Ok(base.with_file_name(file_name))
}

fn run_index(args: Args) -> Result<(), String> {
    shutdown::install_handler()?;
    if args.background {
        background::detach()?;
    }
    let mut volume_configs = Vec::with_capacity(args.volumes.len());
    for (position, volume) in args.volumes.iter().enumerate() {
        let roots = if args.full_volume {
            vec![format!("{}\\", volume.trim_end_matches(['\\', '/']))]
        } else {
            args.roots.clone()
        };
        mft::validate_volume_and_roots(volume, &roots)?;
        let snapshot_path = volume_specific_path(&args.output, volume, position > 0)?;
        let state_path = volume_specific_path(&args.state, volume, position > 0)?;
        if snapshot_path == state_path {
            return Err(format!(
                "Snapshot and state paths collide for volume {volume}"
            ));
        }
        volume_configs.push(journal::VolumeConfig {
            volume: volume.clone(),
            roots,
            max_depth: args.max_depth,
            snapshot_path,
            state_path,
        });
    }
    eprintln!(
        "Synchronizing NTFS index on {} ...",
        args.volumes.join(", ")
    );
    if args.serve {
        // Claim the pipe name first. It narrows the window in which another process could
        // squat on the name at logon, and it lets the launcher see "starting" (searches are
        // refused with a not-ready status) instead of a missing helper while the baseline
        // runs. If the secure pipe cannot be created the helper exits with an error; it never
        // falls back to a weaker ACL.
        ipc::start_server(
            volume_configs
                .iter()
                .map(|config| config.snapshot_path.clone())
                .collect(),
        )?;
        eprintln!(
            "USN watch mode enabled; polling every {} second(s). Press Ctrl+C to stop.",
            args.poll_seconds
        );
    } else if args.watch {
        eprintln!(
            "Watch mode enabled; polling the USN Change Journal every {} second(s). Press Ctrl+C to stop.",
            args.poll_seconds
        );
    }
    let result = journal::run_many(&volume_configs, args.watch, args.poll_seconds);
    status::global().mark_stopping();
    if result.is_ok() && shutdown::global().requested() {
        eprintln!(
            "Shutdown requested; the index was saved after the last completed synchronization."
        );
    }
    // Releases the console-handler thread, which keeps Windows from killing the process
    // before this point on close/logoff/shutdown events.
    shutdown::global().mark_stopped();
    result
}

fn run() -> Result<(), String> {
    match parse_command()? {
        Command::Run(args) => run_index(args),
        Command::InstallTask { args, start_now } => {
            let request = task::InstallRequest {
                volumes: args.volumes,
                output: path_string(&args.output)?,
                state: path_string(&args.state)?,
                max_depth: args.max_depth,
                roots: args.roots,
                full_volume: args.full_volume,
                poll_seconds: args.poll_seconds,
            };
            task::install(&request, start_now)
        }
        Command::UninstallTask => task::uninstall(),
    }
}

fn path_string(path: &std::path::Path) -> Result<String, String> {
    // Task Scheduler arguments must be absolute: the task does not run in this directory.
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|error| format!("Could not resolve the current directory: {error}"))?
            .join(path)
    };
    absolute
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "Paths for the scheduled task must be valid Unicode".to_string())
}

/// `--background`: drop the console window and write progress to a log file next to the
/// executable. When installed by `--install-task` that folder is under Program Files, so
/// only an administrator can write the log. Rotated at 1 MiB (one previous file kept).
#[cfg(windows)]
mod background {
    use std::fs::OpenOptions;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    extern "system" {
        fn FreeConsole() -> i32;
        fn SetStdHandle(which: u32, handle: *mut std::ffi::c_void) -> i32;
    }
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // (DWORD)-12
    const MAX_LOG_BYTES: u64 = 1024 * 1024;

    pub fn detach() -> Result<(), String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("Could not locate this executable: {error}"))?;
        let log_path = exe.with_file_name("helper.log");
        if std::fs::metadata(&log_path)
            .map(|m| m.len() > MAX_LOG_BYTES)
            .unwrap_or(false)
        {
            let _ = std::fs::rename(&log_path, exe.with_file_name("helper.log.1"));
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| {
                format!(
                    "Could not open the helper log {}: {error}",
                    log_path.display()
                )
            })?;
        let handle = log.as_raw_handle();
        unsafe {
            FreeConsole();
            if SetStdHandle(STD_OUTPUT_HANDLE, handle) == 0
                || SetStdHandle(STD_ERROR_HANDLE, handle) == 0
            {
                return Err("Could not redirect helper output to its log file".into());
            }
        }
        // The process-wide standard handles now own this file for the life of the process.
        std::mem::forget(log);
        eprintln!(
            "---- okra-index-helper started (pid {}) ----",
            std::process::id()
        );
        Ok(())
    }
}

#[cfg(not(windows))]
mod background {
    pub fn detach() -> Result<(), String> {
        Ok(())
    }
}

fn main() -> ExitCode {
    #[cfg(not(windows))]
    {
        eprintln!("The NTFS index helper can only run on Windows.");
        ExitCode::FAILURE
    }
    #[cfg(windows)]
    {
        match run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(test)]
mod full_volume_tests {
    use super::{resolve_index_scope, volume_specific_path};
    use std::path::{Path, PathBuf};

    #[test]
    fn full_volume_uses_the_volume_root_and_unlimited_depth() {
        let (roots, depth) = resolve_index_scope("C:", Vec::new(), None, true).unwrap();
        assert_eq!(roots, vec![r"C:\"]);
        assert_eq!(depth, usize::MAX);
    }

    #[test]
    fn full_volume_rejects_mixed_root_and_depth_options() {
        assert!(resolve_index_scope("C:", vec![r"C:\Users".into()], None, true).is_err());
        assert!(resolve_index_scope("C:", Vec::new(), Some(8), true).is_err());
    }

    #[test]
    fn scoped_mode_still_requires_a_root_and_depth() {
        assert!(resolve_index_scope("C:", Vec::new(), Some(8), false).is_err());
        assert!(resolve_index_scope("C:", vec![r"C:\Users".into()], None, false).is_err());
        assert_eq!(
            resolve_index_scope("C:", vec![r"C:\Users".into()], Some(8), false).unwrap(),
            (vec![r"C:\Users".to_string()], 8)
        );
    }
    #[test]
    fn multiple_volumes_get_distinct_snapshot_and_state_names() {
        let base = Path::new("index-dir").join("mft-snapshot.tsv");
        assert_eq!(
            volume_specific_path(&base, "C:", false).unwrap(),
            PathBuf::from("index-dir").join("mft-snapshot.tsv")
        );
        assert_eq!(
            volume_specific_path(&base, "D:", true).unwrap(),
            PathBuf::from("index-dir").join("mft-snapshot-D.tsv")
        );
        assert_eq!(
            volume_specific_path(
                Path::new("index-dir").join("mft-snapshot.state").as_path(),
                "D:",
                true
            )
            .unwrap(),
            PathBuf::from("index-dir").join("mft-snapshot-D.state")
        );
        assert_eq!(volume_specific_path(&base, "C:", false).unwrap(), base);
    }

    #[test]
    fn multiple_volume_path_generation_rejects_non_drive_identifiers() {
        assert!(volume_specific_path(Path::new("snapshot.tsv"), "Volume{123}", true).is_err());
    }
}
