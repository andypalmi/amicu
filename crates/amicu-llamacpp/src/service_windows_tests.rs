//! Task Scheduler manager over a simulated Windows host (no real `schtasks`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::service::ExecOutput;

const SID: &str = "S-1-5-21-111-222-333-1001";

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "amicu-win-{tag}-{}-{n} with space",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How much of its pid record the simulated launcher has written by the time
/// a test looks.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Record {
    /// Its own line and the server's.
    #[default]
    Full,
    /// Its own line only: the server is started but not yet recorded.
    LauncherOnly,
    /// Nothing yet.
    Nothing,
}

/// A simulated per-user Task Scheduler plus the launcher (pid 100) and server
/// (pid 4242, the launcher's child) a task runs.
#[derive(Default)]
struct Win {
    /// name -> (state, last result)
    tasks: Mutex<BTreeMap<String, (String, i64)>>,
    launcher: Mutex<bool>,
    server: Mutex<bool>,
    /// Where `/Run` makes the launcher write its pid record.
    pid_file: Mutex<Option<PathBuf>>,
    record: Record,
    /// `taskkill` kills nothing.
    stubborn: bool,
    /// `/Change /DISABLE` shows the task Disabled even while it runs.
    disabled_while_running: bool,
    /// `schtasks <verb>` fails.
    fail: Option<&'static str>,
    calls: Mutex<Vec<Vec<String>>>,
}

fn ok(stdout: &str) -> ExecOutput {
    ExecOutput {
        success: true,
        stdout: stdout.to_owned(),
        stderr: String::new(),
        code: Some(0),
    }
}

fn failed(stderr: &str) -> ExecOutput {
    ExecOutput {
        success: false,
        stdout: String::new(),
        stderr: stderr.to_owned(),
        code: Some(1),
    }
}

impl Win {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    /// The `schtasks` / `taskkill` / `icacls` / `whoami` calls, PowerShell
    /// probes left out.
    fn actions(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c[0] != "powershell")
            .map(|c| c.join(" "))
            .collect()
    }

    /// Answer the task listing and the process identity probe.
    fn powershell(&self, script: &str) -> ExecOutput {
        if script.contains("Get-ScheduledTask") {
            let mut out = String::new();
            for (name, (state, result)) in self.tasks.lock().unwrap().iter() {
                let _ = write!(out, "TASK\t{name}\t{state}\t{result}\r\n");
            }
            out.push_str("END\r\n");
            ok(&out)
        } else if script.contains("function Ours") {
            // Identity checks of the recorded pids, and the server
            // found as the launcher's child even when unrecorded.
            assert!(script.contains("if (Ours 100 1111)"), "{script}");
            assert!(script.contains("ParentProcessId = 100"), "{script}");
            // An unreadable start time is "not ours", not an error.
            assert!(script.contains("catch { return $false }"), "{script}");
            assert!(
                script.contains("$_.Name -eq 'llama-server.exe'"),
                "{script}"
            );
            let mut out = String::new();
            if *self.launcher.lock().unwrap() {
                out.push_str("LIVE\t100\r\n");
            }
            if *self.server.lock().unwrap() {
                out.push_str("LIVE\t4242\r\n");
            }
            out.push_str("END\r\n");
            ok(&out)
        } else {
            failed("unexpected script")
        }
    }

    fn set_task(&self, name: &str, state: &str, result: i64) {
        self.tasks
            .lock()
            .unwrap()
            .insert(name.to_owned(), (state.to_owned(), result));
    }
}

impl CommandExec for Win {
    fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput, LlamaCppError> {
        let mut call = vec![program.to_owned()];
        call.extend(args.iter().map(|a| (*a).to_owned()));
        self.calls.lock().unwrap().push(call);
        let tn = |args: &[&str]| {
            let i = args.iter().position(|a| *a == "/TN").unwrap();
            args[i + 1].strip_prefix("amicu\\").unwrap().to_owned()
        };
        Ok(match program {
            "whoami" => ok(&format!("\"desktop\\me\",\"{SID}\"\r\n")),
            "icacls" => ok(""),
            "taskkill" => {
                if !self.stubborn {
                    // `/T`: the launcher's tree includes the server.
                    if args[1] == "100" {
                        *self.launcher.lock().unwrap() = false;
                    }
                    *self.server.lock().unwrap() = false;
                }
                ok("")
            }
            "powershell" => self.powershell(args[5]),
            "schtasks" => {
                if self.fail == Some(args[0]) {
                    return Ok(failed("ERROR: Access is denied."));
                }
                let name = tn(args);
                let mut tasks = self.tasks.lock().unwrap();
                match args[0] {
                    "/Create" => {
                        tasks.insert(name, ("Ready".to_owned(), 0x41303));
                    }
                    "/Run" => {
                        let Some(t) = tasks.get_mut(&name) else {
                            return Ok(failed("ERROR: not found"));
                        };
                        t.0 = "Running".to_owned();
                        *self.launcher.lock().unwrap() = true;
                        *self.server.lock().unwrap() = true;
                        let line = "L\t100\t1111\tllama-server.exe\n";
                        let text = match self.record {
                            Record::Full => format!("{line}S\t4242\t1337\n"),
                            Record::LauncherOnly => line.to_owned(),
                            Record::Nothing => String::new(),
                        };
                        if let Some(p) = self.pid_file.lock().unwrap().as_ref()
                            && !text.is_empty()
                        {
                            std::fs::write(p, text).unwrap();
                        }
                    }
                    "/Change" => {
                        if let Some(t) = tasks.get_mut(&name)
                            && (t.0 != "Running" || self.disabled_while_running)
                        {
                            t.0 = "Disabled".to_owned();
                        }
                    }
                    "/End" => {
                        // Ends the launcher; the server (its child) lives on.
                        *self.launcher.lock().unwrap() = false;
                        if let Some(t) = tasks.get_mut(&name)
                            && t.0 == "Running"
                        {
                            t.0 = "Disabled".to_owned();
                            t.1 = 0x41306;
                        }
                    }
                    "/Delete" => {
                        if tasks.remove(&name).is_none() {
                            return Ok(failed("ERROR: not found"));
                        }
                    }
                    _ => return Ok(failed("unexpected schtasks verb")),
                }
                ok("SUCCESS")
            }
            _ => failed("unexpected program"),
        })
    }
}

// `Arc<Win>` is a `CommandExec` through the crate tests' blanket impl.

const LABEL: &str = "io.amicu.llama.0a1b2c3d.chat";

fn manager(win: &Arc<Win>, dir: &Dir) -> TaskSchedulerManager<Arc<Win>> {
    let m = TaskSchedulerManager::new(
        win.clone(),
        dir.0.join("tasks"),
        dir.0.join("env"),
        dir.0.join("logs"),
    )
    .with_settle_timeout(Duration::from_millis(300));
    *win.pid_file.lock().unwrap() = Some(m.pid_path(LABEL));
    m
}

fn spec() -> ServiceSpec {
    ServiceSpec {
        label: LABEL.to_owned(),
        argv: vec![
            r"C:\Program Files\llama.cpp\llama-server.exe".to_owned(),
            "-m".to_owned(),
            r"C:\Users\Jo O'Neil\models\tiny model.gguf".to_owned(),
            "--port".to_owned(),
            "8080".to_owned(),
        ],
        env: vec![("LLAMA_API_KEY".to_owned(), "s3cr%et\"'!^&".to_owned())],
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(f)
}

// ---- rendering --------------------------------------------------------------

#[test]
fn task_xml_golden() {
    let xml = task_xml(
        LABEL,
        SID,
        Path::new(r"C:\Users\A & B\AppData\Local\amicu\tasks\x.ps1"),
    );
    let expected = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>amicu deployment io.amicu.llama.0a1b2c3d.chat</Description>
    <URI>\amicu\io.amicu.llama.0a1b2c3d.chat</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>S-1-5-21-111-222-333-1001</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-21-111-222-333-1001</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>powershell.exe</Command>
      <Arguments>-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File "C:\Users\A &amp; B\AppData\Local\amicu\tasks\x.ps1"</Arguments>
    </Exec>
  </Actions>
</Task>
"#;
    assert_eq!(xml, expected);
    // No password, no stored credentials, nothing secret.
    assert!(!xml.contains("Password") && !xml.contains("LLAMA_API_KEY"));
}

#[test]
fn launcher_golden_quotes_every_literal() {
    let base = r"C:\Users\Jo O'Neil\AppData\Local\amicu";
    let script = launcher_script(
        &spec(),
        Path::new(&format!(r"{base}\env\x.env")),
        Path::new(&format!(r"{base}\tasks\x.pid")),
        Path::new(&format!(r"{base}\logs\x.out.log")),
        Path::new(&format!(r"{base}\logs\x.err.log")),
    );
    let expected = [
        "# amicu launcher for io.amicu.llama.0a1b2c3d.chat. Generated; rewritten on every apply.",
        "$ErrorActionPreference = 'Stop'",
        r#"[System.IO.File]::WriteAllText('C:\Users\Jo O''Neil\AppData\Local\amicu\tasks\x.pid', "L`t$PID`t$((Get-Process -Id $PID).StartTime.ToFileTimeUtc())`t" + 'llama-server.exe' + "`n")"#,
        r"foreach ($line in [System.IO.File]::ReadAllLines('C:\Users\Jo O''Neil\AppData\Local\amicu\env\x.env')) {",
        "  if ($line -eq '') { continue }",
        "  $i = $line.IndexOf('=')",
        "  $value = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($line.Substring($i + 1)))",
        "  [System.Environment]::SetEnvironmentVariable($line.Substring(0, $i), $value, 'Process')",
        "}",
        r#"$p = Start-Process -FilePath 'C:\Program Files\llama.cpp\llama-server.exe' -ArgumentList '-m "C:\Users\Jo O''Neil\models\tiny model.gguf" --port 8080' -NoNewWindow -PassThru -RedirectStandardOutput 'C:\Users\Jo O''Neil\AppData\Local\amicu\logs\x.out.log' -RedirectStandardError 'C:\Users\Jo O''Neil\AppData\Local\amicu\logs\x.err.log'"#,
        "$null = $p.Handle",
        r#"try { [System.IO.File]::AppendAllText('C:\Users\Jo O''Neil\AppData\Local\amicu\tasks\x.pid', "S`t$($p.Id)`t$($p.StartTime.ToFileTimeUtc())`n") } catch { $p.Kill(); exit 1 }"#,
        "$p.WaitForExit()",
        "exit $p.ExitCode",
        "",
    ]
    .join("\n");
    assert_eq!(script, expected);
    assert!(
        !script.contains("s3cr"),
        "the secret never enters the launcher"
    );
}

#[test]
fn launcher_without_arguments_omits_the_argument_list() {
    let mut s = spec();
    s.argv.truncate(1);
    let script = launcher_script(
        &s,
        Path::new("e"),
        Path::new("p"),
        Path::new("o"),
        Path::new("r"),
    );
    assert!(!script.contains("-ArgumentList"), "{script}");
}

#[test]
fn windows_arg_round_trips_by_command_line_to_argv_rules() {
    assert_eq!(windows_arg("plain"), "plain");
    assert_eq!(windows_arg(r"C:\a\b"), r"C:\a\b");
    assert_eq!(windows_arg(""), "\"\"");
    assert_eq!(windows_arg("a b"), "\"a b\"");
    assert_eq!(windows_arg("say \"hi\""), r#""say \"hi\"""#);
    // Backslashes before a quote, and trailing ones inside quotes, double.
    assert_eq!(windows_arg(r#"a\"b"#), r#""a\\\"b""#);
    assert_eq!(
        windows_arg(r"C:\dir with space\"),
        r#""C:\dir with space\\""#
    );
    // Backslashes elsewhere stay literal.
    assert_eq!(windows_arg(r"a b\c"), r#""a b\c""#);
}

#[test]
fn ps_quote_doubles_every_single_quote_form() {
    assert_eq!(ps_quote("plain"), "'plain'");
    assert_eq!(ps_quote("it's"), "'it''s'");
    assert_eq!(ps_quote("$env:X `n \"q\""), "'$env:X `n \"q\"'");
    assert_eq!(ps_quote("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
}

#[test]
fn env_file_is_base64_and_refuses_odd_names() {
    let body = env_file(&[
        ("LLAMA_API_KEY".to_owned(), "s3cr%et\"'!^&".to_owned()),
        ("MULTI".to_owned(), "a\nb".to_owned()),
    ])
    .unwrap();
    assert_eq!(body, "LLAMA_API_KEY=czNjciVldCInIV4m\r\nMULTI=YQpi\r\n");
    assert!(env_file(&[("BAD=NAME".to_owned(), "x".to_owned())]).is_err());
    assert!(env_file(&[(String::new(), "x".to_owned())]).is_err());
}

#[test]
fn base64_matches_rfc_4648_vectors() {
    for (input, want) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(input.as_bytes()), want, "{input}");
    }
}

#[test]
fn csv_fields_handle_quotes_and_commas() {
    assert_eq!(
        csv_fields("\"desktop\\me\",\"S-1-5-21-1\"\r\n"),
        vec!["desktop\\me", "S-1-5-21-1"]
    );
    assert_eq!(csv_fields("\"a,b\",\"c\"\"d\""), vec!["a,b", "c\"d"]);
}

#[test]
fn labels_that_are_not_task_names_are_refused() {
    for bad in ["a:b", "a*b", "a\"b", "a|b", "a'b", "x.", "a\\b", "a b"] {
        assert!(check_task_label(bad).is_err(), "{bad}");
    }
    check_task_label(LABEL).unwrap();
}

// ---- lifecycle ----------------------------------------------------------------

#[test]
fn start_writes_private_files_registers_the_task_and_runs_it() {
    let dir = Dir::new("start");
    let win = Arc::new(Win::default());
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();

    let xml = m.xml_path(LABEL).to_string_lossy().into_owned();
    let env_dir = dir.0.join("env").to_string_lossy().into_owned();
    let env = m.env_path(LABEL).to_string_lossy().into_owned();
    assert_eq!(
        win.actions(),
        vec![
            "whoami /user /fo csv /nh".to_owned(),
            format!("icacls {env_dir} /inheritance:r /grant:r *{SID}:(OI)(CI)F"),
            format!("icacls {env} /inheritance:r /grant:r *{SID}:F"),
            format!("schtasks /Create /XML {xml} /TN amicu\\{LABEL} /F"),
            format!("schtasks /Run /TN amicu\\{LABEL}"),
        ]
    );
    // The secret is only in the env file, base64-encoded.
    let env_body = std::fs::read_to_string(m.env_path(LABEL)).unwrap();
    assert_eq!(env_body, "LLAMA_API_KEY=czNjciVldCInIV4m\r\n");
    let xml_bytes = std::fs::read(m.xml_path(LABEL)).unwrap();
    assert_eq!(&xml_bytes[..2], &[0xFF, 0xFE], "UTF-16LE with BOM");
    let launcher = std::fs::read(m.launcher_path(LABEL)).unwrap();
    assert_eq!(&launcher[..3], &[0xEF, 0xBB, 0xBF], "UTF-8 with BOM");
    for file in [&xml_bytes, &launcher] {
        let text = String::from_utf8_lossy(file);
        assert!(!text.contains("s3cr") && !text.contains("czNjci"));
    }
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Running);
    assert_eq!(m.state(LABEL).unwrap(), ServiceState::Active);
}

#[test]
fn stop_disables_ends_kills_waits_then_deletes_task_and_files() {
    let dir = Dir::new("stop");
    let win = Arc::new(Win::default());
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();
    win.calls.lock().unwrap().clear();

    block_on(m.stop(LABEL)).unwrap();
    let name = format!("amicu\\{LABEL}");
    assert_eq!(
        win.actions(),
        vec![
            format!("schtasks /Change /TN {name} /DISABLE"),
            // Both trees, while the launcher still lives, then the task.
            "taskkill /PID 100 /T /F".to_owned(),
            "taskkill /PID 4242 /T /F".to_owned(),
            format!("schtasks /End /TN {name}"),
            format!("schtasks /Delete /TN {name} /F"),
        ]
    );
    for path in [
        m.env_path(LABEL),
        m.pid_path(LABEL),
        m.launcher_path(LABEL),
        m.xml_path(LABEL),
    ] {
        assert!(!path.exists(), "{} left behind", path.display());
    }
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Down);
    assert_eq!(m.state(LABEL).unwrap(), ServiceState::Inactive);
}

#[test]
fn a_stop_is_not_confirmed_while_the_server_lives_and_everything_is_kept() {
    let dir = Dir::new("stubborn");
    let win = Arc::new(Win {
        stubborn: true,
        ..Win::default()
    });
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();

    let err = block_on(m.stop(LABEL)).unwrap_err().to_string();
    assert!(err.contains("still running"), "{err}");
    assert!(
        !win.actions().iter().any(|a| a.contains("/Delete")),
        "a task whose server lives is never unregistered"
    );
    for path in [m.env_path(LABEL), m.pid_path(LABEL), m.xml_path(LABEL)] {
        assert!(
            path.exists(),
            "{} removed before a confirmed stop",
            path.display()
        );
    }
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Running);
}

/// Ignores `schtasks /End`: the task keeps running.
struct EndIgnored(Arc<Win>);
impl CommandExec for EndIgnored {
    fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput, LlamaCppError> {
        if program == "schtasks" && args[0] == "/End" {
            return Ok(ok(""));
        }
        self.0.exec(program, args)
    }
}

#[test]
fn a_task_still_running_without_a_pid_is_not_confirmed_stopped() {
    let dir = Dir::new("running-task");
    let win = Arc::new(Win::default());
    let m = manager(&win, &dir);
    // `/End` has not taken effect: the task keeps reporting Running.
    win.set_task(LABEL, "Running", 0x41301);
    let m2 = TaskSchedulerManager::new(
        EndIgnored(win.clone()),
        m.task_dir.clone(),
        m.env_dir.clone(),
        m.log_dir.clone(),
    )
    .with_settle_timeout(Duration::from_millis(200));
    assert!(block_on(m2.stop(LABEL)).is_err());
    assert!(win.tasks.lock().unwrap().contains_key(LABEL));
}

#[test]
fn an_unknown_task_state_is_unknown_presence_and_never_stopped() {
    let dir = Dir::new("odd-state");
    let win = Arc::new(Win::default());
    let m = manager(&win, &dir);
    win.set_task(LABEL, "SomethingNew", 0);
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Unknown);
    assert_eq!(m.state(LABEL).unwrap(), ServiceState::Active);
    assert!(block_on(m.stop(LABEL)).is_err());
    assert!(win.tasks.lock().unwrap().contains_key(LABEL));
}

#[test]
fn presence_is_down_without_a_task_and_failed_after_a_bad_exit() {
    let dir = Dir::new("presence");
    let win = Arc::new(Win::default());
    let m = manager(&win, &dir);
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Down);
    assert_eq!(m.state(LABEL).unwrap(), ServiceState::Inactive);
    // Registered, never run: nothing can be running.
    win.set_task(LABEL, "Ready", 0x41303);
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Down);
    // It ran and failed, but there is no pid record: cannot be confirmed.
    win.set_task(LABEL, "Ready", 1);
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Unknown);
    assert_eq!(m.state(LABEL).unwrap(), ServiceState::Failed);
    // With a record whose processes are gone it is confirmed down.
    std::fs::create_dir_all(&m.task_dir).unwrap();
    std::fs::write(m.pid_path(LABEL), "L\t100\t1111\tllama-server.exe\n").unwrap();
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Down);
    win.set_task(LABEL, "Queued", 0);
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Running);
}

#[test]
fn pid_records_parse_and_garbage_is_refused() {
    assert_eq!(
        parse_pid_record("L\t100\t1111\tllama-server.exe\nS\t4242\t1337\n"),
        Some(PidRecord {
            launcher: (100, 1111),
            image: "llama-server.exe".to_owned(),
            server: Some((4242, 1337)),
        })
    );
    assert_eq!(
        parse_pid_record("L\t100\t1111\tllama-server.exe\r\n").map(|r| r.server),
        Some(None)
    );
    for bad in [
        "",
        "garbage",
        "S\t4242\t1337\n",
        "L\tx\t1\tl.exe",
        "L\t1\t1\t",
    ] {
        assert_eq!(parse_pid_record(bad), None, "{bad:?}");
    }
    let dir = Dir::new("pid-garbage");
    let win = Arc::new(Win::default());
    let m = manager(&win, &dir);
    std::fs::create_dir_all(&m.task_dir).unwrap();
    std::fs::write(m.pid_path(LABEL), "garbage").unwrap();
    assert!(
        m.presence(LABEL, None).is_err(),
        "an unreadable record is not 'gone'"
    );
}

#[test]
fn a_server_started_but_not_yet_recorded_is_found_as_the_launchers_child() {
    let dir = Dir::new("unrecorded");
    let win = Arc::new(Win {
        record: Record::LauncherOnly,
        ..Win::default()
    });
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Running);
    block_on(m.stop(LABEL)).unwrap();
    assert!(
        !*win.server.lock().unwrap(),
        "the unrecorded server was killed"
    );

    // Same, but it does not die: never confirmed, everything kept.
    let dir = Dir::new("unrecorded-stubborn");
    let win = Arc::new(Win {
        record: Record::LauncherOnly,
        stubborn: true,
        ..Win::default()
    });
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();
    assert!(block_on(m.stop(LABEL)).is_err());
    assert!(win.tasks.lock().unwrap().contains_key(LABEL));
    assert!(m.env_path(LABEL).exists());
}

#[test]
fn a_task_that_ran_without_any_pid_record_is_never_confirmed_stopped() {
    let dir = Dir::new("no-record");
    let win = Arc::new(Win {
        record: Record::Nothing,
        ..Win::default()
    });
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();
    // `/End` leaves the task Disabled; the server may live on unrecorded.
    let err = block_on(m.stop(LABEL)).unwrap_err().to_string();
    assert!(err.contains("cannot confirm"), "{err}");
    let hint = format!(
        "If no llama-server.exe is running (check Task Manager), run \
         `schtasks /Delete /TN 'amicu\\{LABEL}' /F` and retry"
    );
    assert!(err.contains(&hint), "{err}");
    // A new start points at the same way out, not at `/End`.
    let err = m.start(&spec()).unwrap_err().to_string();
    assert!(err.contains(&hint) && !err.contains("/End"), "{err}");
    assert!(!win.actions().iter().any(|a| a.contains("/Delete")));
    assert!(m.env_path(LABEL).exists());
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Unknown);
}

#[test]
fn a_task_shown_disabled_while_its_server_runs_is_not_stopped() {
    let dir = Dir::new("disabled-running");
    let win = Arc::new(Win {
        disabled_while_running: true,
        stubborn: true,
        ..Win::default()
    });
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();
    assert!(block_on(m.stop(LABEL)).is_err());
    assert_eq!(win.tasks.lock().unwrap()[LABEL].0, "Disabled");
    assert!(m.pid_path(LABEL).exists() && m.env_path(LABEL).exists());
}

#[test]
fn start_refuses_while_an_earlier_instance_runs_and_keeps_its_pid_record() {
    let dir = Dir::new("start-twice");
    let win = Arc::new(Win::default());
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();
    let record = std::fs::read_to_string(m.pid_path(LABEL)).unwrap();
    let err = m.start(&spec()).unwrap_err().to_string();
    assert!(err.contains("may still be running"), "{err}");
    assert_eq!(std::fs::read_to_string(m.pid_path(LABEL)).unwrap(), record);
}

#[test]
fn release_fails_when_the_old_instance_does_not_stop() {
    let dir = Dir::new("release-stubborn");
    let win = Arc::new(Win {
        stubborn: true,
        ..Win::default()
    });
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();
    assert!(block_on(m.release(LABEL)).is_err());
    assert!(m.pid_path(LABEL).exists());
}

#[test]
fn a_failed_disable_stops_nothing() {
    let dir = Dir::new("disable-fails");
    let win = Arc::new(Win {
        fail: Some("/Change"),
        ..Win::default()
    });
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();
    let err = block_on(m.stop(LABEL)).unwrap_err().to_string();
    assert!(err.contains("/DISABLE failed"), "{err}");
    assert!(m.env_path(LABEL).exists());
}

#[test]
fn the_listing_treats_a_missing_folder_as_no_tasks() {
    assert!(LIST_TASKS_SCRIPT.contains("-TaskPath '\\amicu\\'"));
    assert!(LIST_TASKS_SCRIPT.contains("'ObjectNotFound') { $tasks = @() } else { throw }"));
}

#[test]
fn a_failed_run_unregisters_the_task_and_removes_the_secret() {
    let dir = Dir::new("run-fails");
    let win = Arc::new(Win {
        fail: Some("/Run"),
        ..Win::default()
    });
    let m = manager(&win, &dir);
    let err = m.start(&spec()).unwrap_err().to_string();
    assert!(err.contains("schtasks /Run failed"), "{err}");
    assert!(win.tasks.lock().unwrap().is_empty());
    assert!(!m.env_path(LABEL).exists());
    assert!(!m.xml_path(LABEL).exists());
}

#[test]
fn a_failed_create_leaves_no_secret_behind() {
    let dir = Dir::new("create-fails");
    let win = Arc::new(Win {
        fail: Some("/Create"),
        ..Win::default()
    });
    let m = manager(&win, &dir);
    assert!(m.start(&spec()).is_err());
    assert!(!m.env_path(LABEL).exists());
}

#[test]
fn start_refuses_relative_dirs_before_writing_anything() {
    let win = Arc::new(Win::default());
    let m = TaskSchedulerManager::new(win.clone(), "tasks", "env", "logs");
    assert!(m.start(&spec()).is_err());
    assert_eq!(win.calls().len(), 0);
}

#[test]
fn release_waits_for_the_old_instance_and_keeps_the_files() {
    let dir = Dir::new("release");
    let win = Arc::new(Win::default());
    let m = manager(&win, &dir);
    m.start(&spec()).unwrap();
    block_on(m.release(LABEL)).unwrap();
    assert!(!*win.server.lock().unwrap());
    assert!(m.xml_path(LABEL).exists());
    // A restart re-registers and runs again.
    m.start(&spec()).unwrap();
    assert_eq!(m.presence(LABEL, None).unwrap(), Presence::Running);
}

#[test]
fn context_paths_and_listing() {
    let dir = Dir::new("ctx");
    let win = Arc::new(Win::default());
    let m = manager(&win, &dir);
    let ctx = m.context().unwrap();
    assert_eq!(ctx.backend, HostBackend::TaskScheduler);
    assert_eq!(ctx.uid, None);
    assert_eq!(ctx.sid.as_deref(), Some(SID));
    let paths = m.paths(LABEL).unwrap();
    assert!(paths.definition.ends_with(format!("tasks/{LABEL}.xml")));
    assert!(paths.env.unwrap().ends_with(format!("env/{LABEL}.env")));
    assert_eq!(paths.logs.len(), 2);

    win.set_task(LABEL, "Running", 0x41301);
    let tasks = list_tasks(win.as_ref()).unwrap();
    assert_eq!(
        tasks,
        vec![TaskInfo {
            name: LABEL.to_owned(),
            state: "Running".to_owned(),
            last_result: Some(0x41301),
        }]
    );
}

#[test]
fn an_incomplete_listing_is_an_error_not_an_empty_folder() {
    struct Truncated;
    impl CommandExec for Truncated {
        fn exec(&self, _: &str, _: &[&str]) -> Result<ExecOutput, LlamaCppError> {
            Ok(ok(""))
        }
    }
    assert!(list_tasks(&Truncated).is_err());
}

#[test]
fn the_server_image_gets_exe_when_argv0_has_no_extension() {
    assert_eq!(server_image(r"C:\llama\llama-server"), "llama-server.exe");
    assert_eq!(server_image("llama-server"), "llama-server.exe");
    assert_eq!(
        server_image(r"C:\llama\llama-server.exe"),
        "llama-server.exe"
    );
    assert_eq!(server_image("/opt/x/LLAMA-SERVER.EXE"), "LLAMA-SERVER.EXE");
    let mut s = spec();
    s.argv[0] = r"C:\llama\llama-server".to_owned();
    let script = launcher_script(
        &s,
        Path::new("e"),
        Path::new("p"),
        Path::new("o"),
        Path::new("r"),
    );
    assert!(
        script.contains("`t\" + 'llama-server.exe' + \"`n"),
        "{script}"
    );
}
