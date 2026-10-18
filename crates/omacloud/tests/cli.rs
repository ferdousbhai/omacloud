//! The built binary against the output contract: a local repository and a
//! shared coordination directory in a temporary home, no network, and no
//! keyring or systemd of the real user. Commands that print a secret run on a
//! pseudo-terminal of the test's own.

use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    /// `omacloud --config <tmp>/<device>.toml` with everything the user's own
    /// kept out: home, config and data directories, systemd.
    fn cmd(&self, device: &str, args: &[&str]) -> Command {
        let home = self.path("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_omacloud"));
        c.arg("--config")
            .arg(self.path(&format!("{device}.toml")))
            .args(args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_RUNTIME_DIR", self.path("run"))
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
            .env_remove("OMACLOUD_CONFIG")
            .env_remove("OMACLOUD_RECOVERY_CODE")
            .env_remove("OMACLOUD_JOIN_CODE")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, device: &str, args: &[&str]) -> Output {
        self.cmd(device, args).output().unwrap()
    }

    /// A device's init arguments on the shared repository and directory.
    fn init_args(&self, device: &str) -> Vec<String> {
        let p = |s: &str| self.path(s).to_string_lossy().into_owned();
        vec![
            "init".into(),
            "--folder".into(),
            p(&device.to_uppercase()),
            "--repo".into(),
            p("repo"),
            "--coordinator".into(),
            p("coord"),
            "--device".into(),
            device.into(),
        ]
    }

    /// An account made by device `a`, on a terminal (it prints the recovery
    /// code); returns the code.
    fn create(&self) -> String {
        let args = self.init_args("a");
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (out, code) = on_tty(self.cmd("a", &args));
        assert_eq!(code, 0, "{out}");
        out.lines()
            .find_map(|l| l.strip_prefix("recovery code: "))
            .unwrap_or_else(|| panic!("no recovery code in {out}"))
            .trim()
            .to_string()
    }
}

/// A new pseudo-terminal: its controlling side and its terminal side, both
/// close-on-exec from the start, so only the descriptors a child gets dup2ed
/// onto it (stdin, stdout) hold it open, and reading ends with the child.
fn pty() -> (File, OwnedFd) {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: each call gets a descriptor owned from here on, or fails
    unsafe {
        let m = libc::posix_openpt(flags);
        assert!(m >= 0, "{}", std::io::Error::last_os_error());
        let master = File::from_raw_fd(m);
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(libc::ptsname_r(m, name.as_mut_ptr(), name.len()), 0);
        let s = libc::open(name.as_ptr(), flags);
        assert!(s >= 0, "{}", std::io::Error::last_os_error());
        (master, OwnedFd::from_raw_fd(s))
    }
}

/// Runs `cmd` with stdin and stdout on a new pseudo-terminal, as a person's
/// terminal; what it printed there (stderr too, as on a terminal) and the
/// exit code.
fn on_tty(cmd: Command) -> (String, i32) {
    let (out, _, code) = tty(cmd, true, true);
    (out, code)
}

/// `cmd` with stdout on a new pseudo-terminal, and stdin and stderr there
/// too if asked (else stdin as `cmd` has it, and stderr apart).
fn tty(mut cmd: Command, stdin_there: bool, stderr_there: bool) -> (String, String, i32) {
    let (mut master, slave) = pty();
    if stdin_there {
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()));
    }
    if stderr_there {
        cmd.stderr(Stdio::from(slave.try_clone().unwrap()));
    } else {
        cmd.stderr(Stdio::piped());
    }
    cmd.stdout(Stdio::from(slave));
    let mut child = cmd.spawn().unwrap();
    // the parent's copies of the terminal side go, so reading ends with the child
    drop(cmd);
    if stdin_there {
        // An unexpected prompt ends at once (EOF) instead of hanging the test, as in the app.
        std::io::Write::write_all(&mut master, b"\x04").unwrap();
    }
    let mut errors = child.stderr.take();
    let errors = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(e) = errors.as_mut() {
            e.read_to_string(&mut buf).unwrap();
        }
        buf
    });
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    // EIO: the child closed the terminal
    while let Ok(n @ 1..) = master.read(&mut buf) {
        out.extend_from_slice(&buf[..n]);
    }
    let status = child.wait().unwrap();
    (
        String::from_utf8_lossy(&out).replace('\r', ""),
        errors.join().unwrap(),
        status.code().unwrap(),
    )
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// stdout as one JSON document.
fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}{}", stdout(out), stderr(out)))
}

/// A machine-mode failure: one JSON document on stdout, nothing on stderr,
/// with `code` and `exit`. Returns the error object.
fn machine_error(out: &Output, want_code: &str, exit: i32) -> Value {
    assert_eq!(code(out), exit, "{}{}", stdout(out), stderr(out));
    assert_eq!(stderr(out), "", "stderr in a machine mode");
    let v = json(out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], want_code, "{v}");
    assert!(v["error"]["message"].as_str().is_some_and(|m| !m.is_empty()));
    assert!(
        stdout(out).trim_start().starts_with("{\n  \"ok\": false,"),
        "ok first: {}",
        stdout(out)
    );
    v["error"].clone()
}

#[test]
fn piped_output_is_the_envelope_and_quiet_is_its_data() {
    let h = Home::new();
    let out = h.run("a", &["status"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let v = json(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["set_up"], false);
    assert!(v["summary"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(
        stdout(&out).starts_with("{\n  \"ok\": true,"),
        "ok first: {}",
        stdout(&out)
    );

    h.create();
    let envelope = json(&h.run("a", &["status"]));
    assert_eq!(envelope["data"]["set_up"], true);
    assert_eq!(envelope["data"]["device"], "a");
    assert_eq!(envelope["data"]["this_device"]["role"], "member");
    let quiet = json(&h.run("a", &["status", "--quiet"]));
    assert_eq!(quiet, envelope["data"]);
    // --json on a terminal or off it, the same
    assert_eq!(json(&h.run("a", &["--json", "status"]))["data"], envelope["data"]);
}

#[test]
fn devices_ids_count_and_flag_combinations() {
    let h = Home::new();
    h.create();
    let data = json(&h.run("a", &["devices"]))["data"].clone();
    let devices = data["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["name"], "a");
    assert_eq!(devices[0]["this"], true);
    let fp = devices[0]["fingerprint"].as_str().unwrap();

    let ids = h.run("a", &["devices", "list", "--ids-only"]);
    assert_eq!(stdout(&ids), format!("{fp}\n"));
    let count = h.run("a", &["devices", "--count"]);
    assert_eq!(stdout(&count), "1\n");
    assert_eq!(json(&h.run("a", &["devices", "--quiet"])), data);

    // ids-only > count > quiet > json > styled
    assert_eq!(
        stdout(&h.run("a", &["devices", "--json", "--count", "--ids-only"])),
        format!("{fp}\n")
    );
    assert_eq!(stdout(&h.run("a", &["devices", "--quiet", "--json", "--count"])), "1\n");
    assert_eq!(json(&h.run("a", &["devices", "--json", "--quiet"])), data);
    assert_eq!(json(&h.run("a", &["devices", "--styled", "--json"]))["data"], data);
    // flags repeat, and go before the command too
    assert_eq!(stdout(&h.run("a", &["--count", "devices", "--count"])), "1\n");

    let text = h.run("a", &["devices", "--styled"]);
    assert_eq!(code(&text), 0);
    assert!(
        stdout(&text).contains(&format!("{fp}  a  (this device)")),
        "{}",
        stdout(&text)
    );
}

#[test]
fn sync_reports_what_it_did() {
    let h = Home::new();
    h.create();
    std::fs::write(h.path("A/hello.txt"), "hi").unwrap();
    let v = json(&h.run("a", &["sync"]));
    assert_eq!(v["data"]["pushed"], 1, "{v}");
    assert_eq!(v["data"]["conflicts"], 0);
    let text = h.run("a", &["sync", "--styled"]);
    assert!(
        stdout(&text).starts_with("pushed 0, pulled 0, conflicts 0"),
        "{}",
        stdout(&text)
    );

    let versions = h.run("a", &["versions", &h.path("A/hello.txt").to_string_lossy()]);
    let v = json(&versions);
    assert_eq!(v["data"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["data"][0]["device"], "a");
    let ids = h.run(
        "a",
        &["versions", &h.path("A/hello.txt").to_string_lossy(), "--ids-only"],
    );
    assert_eq!(stdout(&ids), format!("{}\n", v["data"][0]["seq"]));
}

#[test]
fn a_machine_error_is_one_json_document_and_text_goes_to_stderr() {
    let h = Home::new();
    let e = machine_error(&h.run("a", &["sync"]), "not_set_up", 3);
    assert!(e["hint"].as_str().unwrap().contains("omacloud init"));
    // the same in every machine mode
    machine_error(&h.run("a", &["devices", "--quiet"]), "not_set_up", 3);
    machine_error(&h.run("a", &["devices", "--ids-only"]), "not_set_up", 3);

    let text = h.run("a", &["sync", "--styled"]);
    assert_eq!(code(&text), 3);
    assert_eq!(stdout(&text), "");
    let err = stderr(&text);
    assert!(err.starts_with("error: this computer isn't set up"), "{err}");
    assert!(err.contains("\nhint: run `omacloud init`"), "{err}");
}

#[test]
fn usage_errors_follow_the_contract() {
    let h = Home::new();
    let e = machine_error(&h.run("a", &["no-such-command"]), "usage", 2);
    assert_eq!(e["hint"], "run `omacloud --help`");
    machine_error(&h.run("a", &["folders", "add", "--json"]), "usage", 2);
    machine_error(&h.run("a", &["restore", "x", "--at", "many"]), "usage", 2);
    // text mode: clap's own words on stderr
    let text = h.run("a", &["no-such-command", "--styled"]);
    assert_eq!(code(&text), 2);
    assert_eq!(stdout(&text), "");
    assert!(stderr(&text).starts_with("error: "), "{}", stderr(&text));
    // --help is no error
    let help = h.run("a", &["--help"]);
    assert_eq!(code(&help), 0);
    assert!(stdout(&help).contains("Exit codes"));
}

#[test]
fn commands_that_print_a_secret_refuse_anywhere_but_a_terminal() {
    let h = Home::new();
    // creating an account shows its recovery code: refused before anything
    // is made, no config, folder, device key, repository or coordination
    let args = h.init_args("a");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    machine_error(&h.run("a", &args), "terminal_required", 2);
    for left in ["a.toml", "a.d", "A", "repo", "coord"] {
        assert!(!h.path(left).exists(), "{left} made by a refused init");
    }

    let recovery = h.create();
    for args in [
        &["export"][..],
        &["recovery", "split", "--threshold", "2", "--shares", "3"],
        &["recovery", "combine"],
    ] {
        let out = h
            .cmd("a", args)
            .env("OMACLOUD_RECOVERY_CODE", &recovery)
            .output()
            .unwrap();
        let e = machine_error(&out, "terminal_required", 2);
        assert!(e["hint"].as_str().unwrap().contains("terminal"), "{args:?}");
        assert!(!stdout(&out).contains(&recovery), "{args:?}");
        // --styled into a pipe is no terminal either
        let mut styled = args.to_vec();
        styled.push("--styled");
        let out = h
            .cmd("a", &styled)
            .env("OMACLOUD_RECOVERY_CODE", &recovery)
            .output()
            .unwrap();
        assert_eq!(code(&out), 2, "{args:?}");
        assert_eq!(stdout(&out), "", "{args:?}");
        assert!(stderr(&out).starts_with("error: this prints "), "{}", stderr(&out));
    }
    // stdout on a terminal but stdin not: no person's terminal either
    let (out, err, exit) = tty(h.cmd("a", &["export"]), false, false);
    assert_eq!(exit, 2, "{out}{err}");
    assert!(!out.contains("password:"), "{out}");
    assert!(err.starts_with("error: this prints "), "{err}");
    // on a terminal, export shows the password
    let (out, exit) = on_tty(h.cmd("a", &["export"]));
    assert_eq!(exit, 0, "{out}");
    assert!(out.contains("\npassword:   "), "{out}");
    let (out, exit) = on_tty(h.cmd("a", &["export", "--json"]));
    assert_eq!(exit, 2, "{out}");
    assert!(!out.contains("password:"), "{out}");
}

#[test]
fn exit_codes_say_what_went_wrong() {
    let h = Home::new();
    let recovery = h.create();
    // 2: already set up; a destructive command nobody confirmed
    machine_error(
        &h.run("a", &h.init_args("a").iter().map(String::as_str).collect::<Vec<_>>()),
        "already_set_up",
        2,
    );
    let e = machine_error(&h.run("a", &["rotate"]), "confirmation_required", 2);
    assert_eq!(e["hint"], "pass --yes");
    // 3: a wrong recovery code
    let args = h.init_args("b");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let wrong = "aaaa-aaaa-aaaa-aaaa-aaaa-aaaa-aaaa";
    let out = h.cmd("b", &args).env("OMACLOUD_RECOVERY_CODE", wrong).output().unwrap();
    machine_error(&out, "recovery_code_rejected", 3);
    // the right one joins, and says so as JSON (no secret in it)
    let out = h
        .cmd("b", &args)
        .env("OMACLOUD_RECOVERY_CODE", &recovery)
        .output()
        .unwrap();
    let v = json(&out);
    assert_eq!(code(&out), 0, "{v}");
    assert_eq!(v["data"]["result"], "joined_with_recovery_code");
    assert_eq!(v["data"]["approved"], true);
    assert!(!stdout(&out).contains(&recovery));
    // 4: no such device, no such file
    let e = machine_error(&h.run("a", &["devices", "approve", "ffff"]), "not_found", 4);
    assert!(e["hint"].as_str().unwrap().contains("devices list"));
    machine_error(
        &h.run("a", &["versions", &h.path("A/nothing.txt").to_string_lossy()]),
        "not_found",
        4,
    );
    machine_error(&h.run("a", &["secrets"]), "settings_off", 3);
}

#[test]
fn skill_prints_as_data_too() {
    let h = Home::new();
    let v = json(&h.run("a", &["skill"]));
    assert_eq!(v["data"]["name"], "omacloud");
    assert!(v["data"]["body"].as_str().unwrap().starts_with("---\nname: omacloud"));
    let text = h.run("a", &["skill", "--styled"]);
    assert!(stdout(&text).starts_with("---\nname: omacloud"));
}

/// `b` joined to `a`'s account with the recovery code; its fingerprint.
fn join_b(h: &Home, recovery: &str) -> String {
    let args = h.init_args("b");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = h
        .cmd("b", &args)
        .env("OMACLOUD_RECOVERY_CODE", recovery)
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    json(&out)["data"]["fingerprint"].as_str().unwrap().to_string()
}

#[test]
fn watch_exits_2_once_this_computer_is_removed() {
    let h = Home::new();
    let recovery = h.create();
    let fp = join_b(&h, &recovery);
    // nobody confirmed it: nothing revoked
    let e = machine_error(&h.run("a", &["devices", "revoke", &fp]), "confirmation_required", 2);
    assert_eq!(e["hint"], "pass --yes");
    let out = h.run("a", &["devices", "revoke", &fp, "--yes"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));

    // the systemd unit's RestartPreventExitStatus=2 depends on this exit
    let mut watch = h
        .cmd("b", &["watch", "--poll", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = watch.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            watch.kill().unwrap();
            panic!("watch kept running after its computer was removed");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let mut log = String::new();
    watch.stderr.take().unwrap().read_to_string(&mut log).unwrap();
    assert_eq!(status.code(), Some(2), "{log}");
    assert!(log.contains("refusing to sync"), "{log}");
}

#[test]
fn restore_over_an_existing_file_needs_yes() {
    let h = Home::new();
    h.create();
    let file = h.path("A/hello.txt");
    std::fs::write(&file, "one").unwrap();
    assert_eq!(code(&h.run("a", &["sync"])), 0);
    let file_arg = file.to_string_lossy().into_owned();
    let seq = json(&h.run("a", &["versions", &file_arg]))["data"][0]["seq"].to_string();
    let to = h.path("restored.txt");
    let to_arg = to.to_string_lossy().into_owned();
    std::fs::write(&to, "mine").unwrap();
    let args = [
        "restore",
        file_arg.as_str(),
        "--at",
        seq.as_str(),
        "--to",
        to_arg.as_str(),
    ];
    let e = machine_error(&h.run("a", &args), "exists", 2);
    assert!(e["hint"].as_str().unwrap().contains("--yes"), "{e}");
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "mine");
    let mut yes = args.to_vec();
    yes.push("--yes");
    let v = json(&h.run("a", &yes));
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "one");
}
