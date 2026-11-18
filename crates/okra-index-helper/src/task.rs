//! Task Scheduler integration: start the elevated helper at the user's logon.
//!
//! Why a scheduled task and not a Windows service: the pipe ACL is bound to the interactive
//! logon session, and a service lives in Session 0. A "run at logon, highest privileges,
//! interactive token" task starts the helper elevated, without a UAC prompt, under the same
//! account. Whether the task's token carries the *same logon-session SID* as the launcher must
//! be confirmed on Windows (see docs/ntfs-fast-mode.md, "Verifying the logon session").
//!
//! Security rules implemented here:
//! * The task runs a *copy* of the helper installed under `Program Files` (admin-write only),
//!   never the build output in a user-writable folder. Otherwise any process running as the
//!   user could replace that executable and obtain elevation without a prompt.
//! * Every external tool is invoked by absolute path under the Windows directory (no PATH
//!   lookup) and without a shell.
//! * The task XML is staged in `%SystemRoot%\Temp` with `create_new`; ordinary users cannot
//!   modify files created there by an administrator.
//! * No passwords are stored (`InteractiveToken`).

pub const TASK_NAME: &str = r"Okra Brown Search\NTFS Helper";
pub const INSTALL_DIR_NAME: &str = "Okra Brown Search";
pub const INSTALLED_EXE_NAME: &str = "okra-index-helper.exe";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSpec {
    /// Absolute path of the helper executable the task will run.
    pub exe: String,
    pub arguments: Vec<String>,
    /// `DOMAIN\user` the task runs as (and the only account whose logon triggers it).
    pub user: String,
}

#[derive(Clone, Debug)]
pub struct InstallRequest {
    pub volume: String,
    pub output: String,
    pub state: String,
    pub max_depth: usize,
    pub roots: Vec<String>,
    pub poll_seconds: u64,
}

pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Quote one argument for a Windows command line (`CommandLineToArgvW` rules).
/// Arguments containing a double quote or control character are rejected rather than escaped:
/// no legitimate path needs them, and refusing is safer than clever escaping.
pub fn quote_arg(arg: &str) -> Result<String, String> {
    if arg.chars().any(|c| c == '"' || c.is_control()) {
        return Err(format!("Argument contains a quote or control character: {arg:?}"));
    }
    if !arg.is_empty() && !arg.contains([' ', '\t']) {
        return Ok(arg.to_string());
    }
    // Backslashes directly before the closing quote must be doubled.
    let trailing = arg.chars().rev().take_while(|c| *c == '\\').count();
    let mut out = String::with_capacity(arg.len() + trailing + 2);
    out.push('"');
    out.push_str(arg);
    for _ in 0..trailing {
        out.push('\\');
    }
    out.push('"');
    Ok(out)
}

pub fn join_arguments(arguments: &[String]) -> Result<String, String> {
    let quoted: Result<Vec<String>, String> = arguments.iter().map(|a| quote_arg(a)).collect();
    Ok(quoted?.join(" "))
}

/// Arguments the scheduled helper runs with: the same index configuration the user tested
/// manually, plus `--serve` (pipe + watch) and `--background` (no console, log file).
pub fn helper_arguments(request: &InstallRequest) -> Vec<String> {
    let mut args = vec![
        "--volume".to_string(),
        request.volume.clone(),
        "--output".to_string(),
        request.output.clone(),
        "--state".to_string(),
        request.state.clone(),
        "--max-depth".to_string(),
        request.max_depth.to_string(),
    ];
    for root in &request.roots {
        args.push("--root".to_string());
        args.push(root.clone());
    }
    args.push("--serve".to_string());
    args.push("--poll-seconds".to_string());
    args.push(request.poll_seconds.to_string());
    args.push("--background".to_string());
    args
}

pub fn build_task_xml(spec: &TaskSpec) -> Result<String, String> {
    if spec.user.is_empty() || !spec.user.contains('\\') {
        return Err("The task user must be in DOMAIN\\user form".into());
    }
    if spec.exe.is_empty() || spec.exe.contains('"') {
        return Err("Invalid helper executable path".into());
    }
    let user = xml_escape(&spec.user);
    let command = xml_escape(&spec.exe);
    let arguments = xml_escape(&join_arguments(&spec.arguments)?);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Okra Brown Search: elevated NTFS index helper (USN journal + search pipe). Runs only for the installing user.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    ))
}

/// Task Scheduler accepts UTF-16 LE with a BOM for `/XML` files.
pub fn to_utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

#[cfg(windows)]
pub use win::{install, uninstall};

#[cfg(not(windows))]
pub fn install(_request: &InstallRequest, _start_now: bool) -> Result<(), String> {
    Err("Task installation is available only on Windows".into())
}

#[cfg(not(windows))]
pub fn uninstall() -> Result<(), String> {
    Err("Task removal is available only on Windows".into())
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemWindowsDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }

    /// `C:\Windows`, from the OS rather than from environment variables that an unelevated
    /// parent process could have altered before this elevated process was started.
    fn windows_dir() -> Result<PathBuf, String> {
        let mut buffer = [0u16; 260];
        let len = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        if len == 0 || len >= buffer.len() {
            return Err("Could not determine the Windows directory".into());
        }
        Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..len])))
    }

    fn schtasks_exe() -> Result<PathBuf, String> {
        Ok(windows_dir()?.join("System32").join("schtasks.exe"))
    }

    fn install_dir() -> Result<PathBuf, String> {
        let windows = windows_dir()?;
        let drive = windows
            .to_str()
            .and_then(|path| path.get(..2))
            .filter(|prefix| prefix.ends_with(':'))
            .ok_or_else(|| "Unexpected Windows directory format".to_string())?;
        Ok(PathBuf::from(format!(r"{drive}\Program Files")).join(INSTALL_DIR_NAME))
    }

    fn run_schtasks(args: &[&str]) -> Result<std::process::Output, String> {
        Command::new(schtasks_exe()?)
            .args(args)
            .output()
            .map_err(|error| format!("Could not run schtasks.exe: {error}"))
    }

    fn describe(output: &std::process::Output) -> String {
        let mut text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !err.is_empty() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&err);
        }
        text
    }

    fn copy_with_retries(from: &Path, to: &Path) -> Result<(), String> {
        let mut last = String::new();
        for _ in 0..10 {
            match fs::copy(from, to) {
                Ok(_) => return Ok(()),
                Err(error) => {
                    last = error.to_string();
                    // The previous helper may still be exiting after `schtasks /End`.
                    thread::sleep(Duration::from_millis(300));
                }
            }
        }
        Err(format!("Could not copy the helper to {} ({last}). Run this from an elevated prompt.", to.display()))
    }

    pub fn install(request: &InstallRequest, start_now: bool) -> Result<(), String> {
        let domain = std::env::var("USERDOMAIN").map_err(|_| "USERDOMAIN is not set".to_string())?;
        let user_name = std::env::var("USERNAME").map_err(|_| "USERNAME is not set".to_string())?;
        let user = format!("{domain}\\{user_name}");

        // Stop a running instance first so its executable can be replaced.
        let _ = run_schtasks(&["/End", "/TN", TASK_NAME]);

        let dir = install_dir()?;
        fs::create_dir_all(&dir)
            .map_err(|error| format!("Could not create {} ({error}). Run this from an elevated prompt.", dir.display()))?;
        let installed = dir.join(INSTALLED_EXE_NAME);
        let current = std::env::current_exe().map_err(|error| format!("Could not locate this executable: {error}"))?;
        if current != installed {
            copy_with_retries(&current, &installed)?;
        }

        let spec = TaskSpec {
            exe: installed.to_string_lossy().into_owned(),
            arguments: helper_arguments(request),
            user: user.clone(),
        };
        let xml = to_utf16le_with_bom(&build_task_xml(&spec)?);

        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let xml_path = windows_dir()?
            .join("Temp")
            .join(format!("okra-task-{}-{stamp}.xml", std::process::id()));
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&xml_path)
                .map_err(|error| format!("Could not stage the task definition: {error}"))?;
            file.write_all(&xml).map_err(|error| format!("Could not write the task definition: {error}"))?;
        }
        let xml_arg = xml_path.to_string_lossy().into_owned();
        let created = run_schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &xml_arg, "/F"]);
        let _ = fs::remove_file(&xml_path);
        let created = created?;
        if !created.status.success() {
            return Err(format!("schtasks /Create failed: {}", describe(&created)));
        }

        println!("Installed scheduled task \"{TASK_NAME}\" for {user}.");
        println!("It runs {} at logon with highest privileges (no UAC prompt).", installed.display());
        println!("Log file: {}", dir.join("helper.log").display());
        if start_now {
            let started = run_schtasks(&["/Run", "/TN", TASK_NAME])?;
            if started.status.success() {
                println!("Started the task now.");
            } else {
                return Err(format!("Installed, but starting failed: {}", describe(&started)));
            }
        } else {
            println!("It starts at your next sign-in; to start it now run: schtasks /Run /TN \"{TASK_NAME}\"");
        }
        Ok(())
    }

    pub fn uninstall() -> Result<(), String> {
        let _ = run_schtasks(&["/End", "/TN", TASK_NAME]);
        let deleted = run_schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])?;
        if !deleted.status.success() {
            return Err(format!("schtasks /Delete failed: {}", describe(&deleted)));
        }
        println!("Removed scheduled task \"{TASK_NAME}\".");
        let dir = install_dir()?;
        // Remove only what the installer created; leave anything else in the folder alone.
        for name in [INSTALLED_EXE_NAME, "helper.log", "helper.log.1"] {
            let _ = fs::remove_file(dir.join(name));
        }
        let _ = fs::remove_dir(&dir);
        println!("The index files in your local app data folder were left in place.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TaskSpec {
        TaskSpec {
            exe: r"C:\Program Files\Okra Brown Search\okra-index-helper.exe".into(),
            arguments: vec!["--root".into(), r"C:\Users\Jane & Co\Docs".into(), "--serve".into()],
            user: r"SILK\barda".into(),
        }
    }

    #[test]
    fn quoting_follows_windows_command_line_rules() {
        assert_eq!(quote_arg("C:").unwrap(), "C:");
        assert_eq!(quote_arg(r"C:\Users\me").unwrap(), r"C:\Users\me");
        assert_eq!(quote_arg(r"C:\Program Files\x").unwrap(), r#""C:\Program Files\x""#);
        assert_eq!(quote_arg(r"C:\My Dir\").unwrap(), r#""C:\My Dir\\""#);
        assert_eq!(quote_arg("").unwrap(), r#""""#);
        assert!(quote_arg("bad\"quote").is_err());
        assert!(quote_arg("bad\nline").is_err());
    }

    #[test]
    fn xml_escaping_covers_markup_characters() {
        assert_eq!(xml_escape(r#"a&b<c>"d"'e'"#), "a&amp;b&lt;c&gt;&quot;d&quot;&apos;e&apos;");
    }

    #[test]
    fn task_xml_requests_logon_start_highest_privileges_and_restart_on_failure() {
        let xml = build_task_xml(&spec()).unwrap();
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains(r"<UserId>SILK\barda</UserId>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(xml.contains(r"<Command>C:\Program Files\Okra Brown Search\okra-index-helper.exe</Command>"));
        // Markup characters are escaped in the XML text...
        assert!(xml.contains("Jane &amp; Co"));
        assert!(!xml.contains("Jane & Co"));
        // ...and unescaping must give back exactly the intended, correctly quoted command line.
        let start = xml.find("<Arguments>").unwrap() + "<Arguments>".len();
        let end = xml.find("</Arguments>").unwrap();
        let unescaped = xml[start..end]
            .replace("&quot;", "\"")
            .replace("&amp;", "&");
        assert_eq!(unescaped, r#"--root "C:\Users\Jane & Co\Docs" --serve"#);
        // No stored credentials.
        assert!(!xml.contains("<Password>"));
        assert!(!xml.contains("S4U"));
    }

    #[test]
    fn task_xml_rejects_bad_user_and_executable() {
        let mut bad = spec();
        bad.user = "barda".into();
        assert!(build_task_xml(&bad).is_err());
        let mut bad = spec();
        bad.exe = "C:\\x\"y.exe".into();
        assert!(build_task_xml(&bad).is_err());
        let mut bad = spec();
        bad.arguments.push("evil\" --root \"C:\\".into());
        assert!(build_task_xml(&bad).is_err());
    }

    #[test]
    fn xml_file_is_utf16_le_with_bom() {
        let bytes = to_utf16le_with_bom("<a>é</a>");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let units: Vec<u16> = bytes[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "<a>é</a>");
    }

    #[test]
    fn scheduled_arguments_repeat_the_manual_configuration_and_add_serve_and_background() {
        let request = InstallRequest {
            volume: "C:".into(),
            output: r"C:\Users\barda\AppData\Local\okra-brown-search\mft-snapshot.tsv".into(),
            state: r"C:\Users\barda\AppData\Local\okra-brown-search\mft-snapshot.state".into(),
            max_depth: 8,
            roots: vec![r"C:\Users\barda".into(), r"C:\Projects".into()],
            poll_seconds: 1,
        };
        let args = helper_arguments(&request);
        assert_eq!(args.iter().filter(|a| *a == "--root").count(), 2);
        for expected in ["--serve", "--background", "--poll-seconds", "--state"] {
            assert!(args.iter().any(|a| a == expected), "missing {expected}");
        }
        assert!(join_arguments(&args).unwrap().starts_with("--volume C: --output "));
    }
}
