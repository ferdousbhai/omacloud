//! The built binary against the output contract: a local repository and a
//! shared coordination directory in a temporary home, no network, and no
//! systemd of the real user. Commands that print a secret run on a
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

/// A machine-mode failure: exit 1, nothing on stdout, and the error object
/// as the one line on stderr, with `code` and `kind`. Returns the error
/// object.
fn machine_error(out: &Output, want_code: &str, want_kind: &str) -> Value {
    assert_eq!(code(out), 1, "{}{}", stdout(out), stderr(out));
    assert_eq!(stdout(out), "", "stdout holds only a result");
    let err = stderr(out);
    assert_eq!(err.lines().count(), 1, "one line on stderr: {err}");
    let v: Value = serde_json::from_str(&err).unwrap_or_else(|e| panic!("{e}: {err}"));
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], want_code, "{v}");
    assert_eq!(v["error"]["kind"], want_kind, "{v}");
    assert!(v["error"]["message"].as_str().is_some_and(|m| !m.is_empty()));
    assert!(err.starts_with("{\"ok\":false,"), "ok first: {err}");
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
    // the file, and what syncs on this computer (`omacloud choose`)
    assert_eq!(v["data"]["pushed"], 2, "{v}");
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
    let e = machine_error(&h.run("a", &["sync"]), "not_set_up", "setup");
    assert!(e["hint"].as_str().unwrap().contains("omacloud init"));
    // the same in every machine mode
    machine_error(&h.run("a", &["devices", "--quiet"]), "not_set_up", "setup");
    machine_error(&h.run("a", &["devices", "--ids-only"]), "not_set_up", "setup");

    let text = h.run("a", &["sync", "--styled"]);
    assert_eq!(code(&text), 1);
    assert_eq!(stdout(&text), "");
    let err = stderr(&text);
    assert!(err.starts_with("error: this computer isn't set up"), "{err}");
    assert!(err.contains("\nhint: run `omacloud init`"), "{err}");
}

#[test]
fn usage_errors_follow_the_contract() {
    let h = Home::new();
    let e = machine_error(&h.run("a", &["no-such-command"]), "usage", "usage");
    assert_eq!(e["hint"], "run `omacloud --help`");
    machine_error(&h.run("a", &["folders", "add", "--json"]), "usage", "usage");
    machine_error(&h.run("a", &["restore", "x", "--at", "many"]), "usage", "usage");
    // a folder of home is a name directly in it, as `folders add` takes
    let (repo, coord) = (h.path("repo"), h.path("coord"));
    let mut bad = h.cmd("a", &["init", "--folders", "Documents,.ssh"]);
    _ = bad.arg("--repo").arg(&repo).arg("--coordinator").arg(&coord);
    machine_error(&bad.output().unwrap(), "usage", "usage");
    // none at all (the app's setup with every folder off) is no error
    let mut none = h.cmd("a", &["init", "--folders", ""]);
    _ = none.arg("--repo").arg(&repo).arg("--coordinator").arg(&coord);
    let (out, c) = on_tty(none);
    assert_eq!(c, 0, "{out}");
    // text mode: clap's own words on stderr
    let text = h.run("a", &["no-such-command", "--styled"]);
    assert_eq!(code(&text), 1);
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
    machine_error(&h.run("a", &args), "terminal_required", "usage");
    for left in ["a.toml", "a.d", "A", "repo", "coord"] {
        assert!(!h.path(left).exists(), "{left} made by a refused init");
    }

    let recovery = h.create();
    for args in [
        &["export"][..],
        &["web-code"],
        &["web-link"],
        &["web-link", "--keep"],
        &["agents", "connect", "Notes"],
        &["recovery", "split", "--threshold", "2", "--shares", "3"],
        &["recovery", "combine"],
    ] {
        let out = h
            .cmd("a", args)
            .env("OMACLOUD_RECOVERY_CODE", &recovery)
            .output()
            .unwrap();
        let e = machine_error(&out, "terminal_required", "usage");
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
        assert_eq!(code(&out), 1, "{args:?}");
        assert_eq!(stdout(&out), "", "{args:?}");
        assert!(stderr(&out).starts_with("error: this prints "), "{}", stderr(&out));
    }
    // stdout on a terminal but stdin not: no person's terminal either
    let (out, err, exit) = tty(h.cmd("a", &["export"]), false, false);
    assert_eq!(exit, 1, "{out}{err}");
    assert!(!out.contains("password:"), "{out}");
    assert!(err.starts_with("error: this prints "), "{err}");
    // on a terminal, export shows the password
    let (out, exit) = on_tty(h.cmd("a", &["export"]));
    assert_eq!(exit, 0, "{out}");
    assert!(out.contains("\npassword:   "), "{out}");
    let (out, exit) = on_tty(h.cmd("a", &["export", "--json"]));
    assert_eq!(exit, 1, "{out}");
    assert!(!out.contains("password:"), "{out}");
}

#[test]
fn failures_exit_1_and_their_kind_says_what_went_wrong() {
    let h = Home::new();
    let recovery = h.create();
    // usage: already set up; a destructive command nobody confirmed
    machine_error(
        &h.run("a", &h.init_args("a").iter().map(String::as_str).collect::<Vec<_>>()),
        "already_set_up",
        "usage",
    );
    let e = machine_error(&h.run("a", &["rotate"]), "confirmation_required", "usage");
    assert_eq!(e["hint"], "pass --yes");
    // setup: a wrong recovery code
    let args = h.init_args("b");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let wrong = "aaaa-aaaa-aaaa-aaaa-aaaa-aaaa-aaaa";
    let out = h.cmd("b", &args).env("OMACLOUD_RECOVERY_CODE", wrong).output().unwrap();
    machine_error(&out, "recovery_code_rejected", "setup");
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
    // not_found: no such device, no such file
    let e = machine_error(&h.run("a", &["devices", "approve", "ffff"]), "not_found", "not_found");
    assert!(e["hint"].as_str().unwrap().contains("devices list"));
    machine_error(
        &h.run("a", &["versions", &h.path("A/nothing.txt").to_string_lossy()]),
        "not_found",
        "not_found",
    );
    machine_error(&h.run("a", &["secrets"]), "settings_off", "setup");
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
fn only_an_account_in_a_bucket_has_browsers_that_stay_signed_in() {
    let h = Home::new();
    h.create();
    let e = machine_error(&h.run("a", &["browsers"]), "unsupported", "usage");
    assert!(e["hint"].as_str().is_some(), "{e}");
    // and status says none, without asking anything
    let status = json(&h.run("a", &["status"]));
    assert_eq!(status["data"]["browsers"], serde_json::json!([]));
}

#[test]
fn no_connector_for_agents_until_one_is_set_up() {
    let h = Home::new();
    h.create();
    let listed = json(&h.run("a", &["agents"]));
    assert_eq!(listed["data"]["agents"], serde_json::json!([]));
    _ = machine_error(&h.run("a", &["agents", "disconnect"]), "not_found", "not_found");
    // a connector reads a bucket or a Dropbox, which this account isn't in
    let (out, exit) = on_tty(h.cmd("a", &["agents", "connect", "A"]));
    assert_eq!(exit, 1, "{out}");
    assert!(out.contains("reads a bucket or a Dropbox"), "{out}");
    assert!(!h.path("a.d/agents.json").exists());
}

#[test]
fn a_computers_name_is_one_line() {
    let h = Home::new();
    for name in ["a\nb", "", "tab\tbed"] {
        let mut args = h.init_args("a");
        *args.last_mut().unwrap() = name.to_string();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let e = machine_error(&h.run("a", &args), "usage", "usage");
        assert!(e["message"].as_str().unwrap().contains("name"), "{e}");
    }
    assert!(!h.path("a.toml").exists());
}

#[test]
fn an_account_in_a_folder_waits_for_its_drive() {
    let h = Home::new();
    let drive = h.path("drive");
    std::fs::create_dir_all(&drive).unwrap();
    let init = |device: &str| {
        let folder = h.path(&device.to_uppercase());
        h.cmd(
            device,
            &[
                "init",
                "--folder",
                folder.to_str().unwrap(),
                "--provider",
                "folder",
                "--path",
                drive.to_str().unwrap(),
                "--device",
                device,
            ],
        )
    };
    // a folder is named, there, and the only thing given
    let e = machine_error(
        &h.run("a", &["init", "--provider", "folder", "--device", "a"]),
        "usage",
        "usage",
    );
    assert!(e["message"].as_str().unwrap().contains("--path"), "{e}");
    let nowhere = h.path("nowhere");
    let out = h.run(
        "a",
        &["init", "--provider", "folder", "--path", nowhere.to_str().unwrap()],
    );
    _ = machine_error(&out, "unavailable", "unavailable");
    assert!(!nowhere.exists(), "nothing is made where no drive is");
    let out = h.run("a", &["init", "--provider", "r2", "--path", drive.to_str().unwrap()]);
    _ = machine_error(&out, "usage", "usage");

    let (out, code_a) = on_tty(init("a"));
    assert_eq!(code_a, 0, "{out}");
    assert!(drive.join("Omacloud/coordination/account.json").exists());
    assert!(drive.join("Omacloud/repo/config").exists());
    // another computer joins by naming the folder, and is approved
    let out = init("b").output().unwrap();
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let fp = json(&out)["data"]["fingerprint"].as_str().unwrap().to_string();
    assert_eq!(code(&h.run("a", &["devices", "approve", &fp])), 0);
    std::fs::write(h.path("A/note.txt"), "hello").unwrap();
    assert_eq!(code(&h.run("a", &["sync"])), 0);
    assert_eq!(code(&h.run("b", &["sync"])), 0);
    assert_eq!(std::fs::read_to_string(h.path("B/note.txt")).unwrap(), "hello");

    // the drive unplugged: where it mounts is an empty folder, and stays one
    let away = h.path("drive.away");
    std::fs::rename(&drive, &away).unwrap();
    std::fs::create_dir(&drive).unwrap();
    for command in ["sync", "status"] {
        let e = machine_error(&h.run("b", &[command]), "unavailable", "unavailable");
        assert!(e["hint"].as_str().unwrap().contains("drive"), "{e}");
    }
    assert_eq!(std::fs::read_dir(&drive).unwrap().count(), 0);
    // and back
    std::fs::remove_dir(&drive).unwrap();
    std::fs::rename(&away, &drive).unwrap();
    assert_eq!(code(&h.run("b", &["sync"])), 0);
}

#[test]
fn watch_exits_2_once_this_computer_is_removed() {
    let h = Home::new();
    let recovery = h.create();
    let fp = join_b(&h, &recovery);
    // nobody confirmed it: nothing revoked
    let e = machine_error(
        &h.run("a", &["devices", "revoke", &fp]),
        "confirmation_required",
        "usage",
    );
    assert_eq!(e["hint"], "pass --yes");
    let out = h.run("a", &["devices", "revoke", &fp, "--yes"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));

    // a computer on low battery pauses sync and would never find out
    let config = h.path("b.toml");
    let toml = std::fs::read_to_string(&config).unwrap();
    assert!(toml.contains("pause_on_battery_below = 20"), "{toml}");
    std::fs::write(
        &config,
        toml.replace("pause_on_battery_below = 20", "pause_on_battery_below = 0"),
    )
    .unwrap();

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
fn a_deleted_file_is_listed_and_put_back() {
    let h = Home::new();
    h.create();
    let file = h.path("A/gone.txt");
    std::fs::write(&file, "keep me").unwrap();
    assert_eq!(code(&h.run("a", &["sync"])), 0);
    std::fs::remove_file(&file).unwrap();
    assert_eq!(code(&h.run("a", &["sync"])), 0);

    let v = json(&h.run("a", &["deleted"]));
    assert_eq!(v["data"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["data"][0]["relative"], "gone.txt");
    assert_eq!(v["data"][0]["path"], file.to_string_lossy().as_ref());
    assert_eq!(stdout(&h.run("a", &["deleted", "--ids-only"])), "gone.txt\n");
    let path = file.to_string_lossy().into_owned();
    let at = v["data"][0]["version"].to_string();
    let back = json(&h.run("a", &["restore", &path, "--at", &at, "--to", &path]));
    assert_eq!(back["ok"], true, "{back}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me");
    assert_eq!(code(&h.run("a", &["sync"])), 0);
    assert_eq!(json(&h.run("a", &["deleted", "--count"])), 0);
}

/// With home's folders and settings synced, a folder another computer
/// added, or one skipped here (named in another language), is still a
/// folder, not settings.
#[test]
fn versions_find_a_folder_another_computer_added() {
    let h = Home::new();
    let (repo, coord) = (h.path("repo"), h.path("coord"));
    // Pictures named in another language here
    std::fs::create_dir_all(h.path("home/.config")).unwrap();
    std::fs::write(
        h.path("home/.config/user-dirs.dirs"),
        "XDG_PICTURES_DIR=\"$HOME/Bilder\"\n",
    )
    .unwrap();
    let init = |device: &str, folders: &str| {
        let mut c = h.cmd(
            device,
            &["init", "--folders", folders, "--settings", "--device", device],
        );
        _ = c.arg("--repo").arg(&repo).arg("--coordinator").arg(&coord);
        c
    };
    let (out, c) = on_tty(init("a", "Documents,Pictures"));
    assert_eq!(c, 0, "{out}");
    let recovery = out
        .lines()
        .find_map(|l| l.strip_prefix("recovery code: "))
        .unwrap()
        .trim()
        .to_string();
    let song = h.path("home/Notes/song.txt");
    std::fs::create_dir_all(song.parent().unwrap()).unwrap();
    std::fs::write(&song, "la").unwrap();
    // and one that b skips
    let cat = h.path("home/Bilder/cat.txt");
    std::fs::create_dir_all(cat.parent().unwrap()).unwrap();
    std::fs::write(&cat, "meow").unwrap();
    // any other folder of home is offered, and added by its path too
    let list = json(&h.run("a", &["folders", "list"]));
    let more = |list: &Value| -> Vec<String> {
        list["data"]["more"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(more(&list), ["Notes"], "{list}");
    let notes = format!("{}/", h.path("home/Notes").display());
    let list = json(&h.run("a", &["folders", "add", &notes]));
    assert!(more(&list).is_empty(), "{list}");
    assert!(
        list["data"]["synced"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "Notes"),
        "{list}"
    );
    assert_eq!(code(&h.run("a", &["sync"])), 0);

    let out = init("b", "Documents")
        .env("OMACLOUD_RECOVERY_CODE", &recovery)
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert_eq!(code(&h.run("b", &["sync"])), 0);
    for f in [&song, &cat] {
        let v = json(&h.run("b", &["versions", &f.to_string_lossy()]));
        assert_eq!(v["ok"], true, "{v}");
    }
    // skipped, with where it lives here
    let st = json(&h.run("b", &["status"]));
    let skipped = st["data"]["folders"]["skipped"].as_array().unwrap();
    let pictures = skipped
        .iter()
        .find(|f| f["name"] == "Pictures")
        .expect("Pictures skipped");
    assert_eq!(pictures["path"], h.path("home/Bilder").to_string_lossy().as_ref());
    // added by its name here, it's the account's Pictures
    let ids = stdout(&h.run("b", &["folders", "add", "Bilder", "--ids-only"]));
    assert!(ids.lines().any(|l| l == "Pictures"), "{ids}");
    assert!(!ids.lines().any(|l| l == "Bilder"), "{ids}");

    // a subfolder, by the account's name for its folder, with sizes to choose by
    let list = json(&h.run("b", &["folders", "exclude", "Bilder/trip"]));
    assert_eq!(list["data"]["excluded"][0]["path"], "Pictures/trip", "{list}");
    machine_error(&h.run("b", &["folders", "exclude", "Pictures"]), "usage", "usage");
    machine_error(
        &h.run("b", &["folders", "exclude", "Pictures/../secret"]),
        "usage",
        "usage",
    );
    let list = json(&h.run("b", &["folders", "include", "Pictures/trip"]));
    assert_eq!(list["data"]["excluded"], serde_json::json!([]), "{list}");
    let list = json(&h.run("b", &["folders", "list"]));
    assert!(list["data"]["synced"][0]["size"].is_u64(), "{list}");

    // removing asks first, which a script can't answer
    assert_ne!(code(&h.run("b", &["folders", "remove", "Pictures"])), 0);
    let list = json(&h.run("b", &["folders", "remove", "Pictures", "--yes"]));
    assert_eq!(
        list["data"]["removed_from_cloud"],
        serde_json::json!(["Pictures"]),
        "{list}"
    );
    assert!(cat.exists());

    // groups of settings
    let s = json(&h.run("b", &["settings", "skip", "agents"]));
    assert_eq!(s["data"]["skipped"], serde_json::json!(["agents"]), "{s}");
    machine_error(&h.run("b", &["settings", "skip", "nonsense"]), "usage", "usage");
    let s = json(&h.run("b", &["settings", "unskip", "agents"]));
    assert_eq!(s["data"]["skipped"], serde_json::json!([]), "{s}");

    // choosing, one thing at a time
    let claude = h.path("home/.claude/CLAUDE.md");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(&claude, "only here\n").unwrap();
    let claude = claude.to_string_lossy();
    let s = json(&h.run("b", &["choose", &claude, "backup"]));
    assert_eq!(s["data"]["choice"], "backup", "{s}");
    let s = json(&h.run("b", &["settings", "status"]));
    assert_eq!(s["data"]["own"], serde_json::json!([".claude/CLAUDE.md"]), "{s}");
    let monitors = h.path("home/.config/hypr/monitors.lua");
    let monitors = monitors.to_string_lossy();
    // about the computer: backed up or off, never synced
    machine_error(&h.run("b", &["choose", &monitors, "sync"]), "usage", "usage");
    assert_eq!(
        json(&h.run("b", &["choose", &monitors, "off"]))["data"]["choice"],
        "off"
    );
    machine_error(&h.run("b", &["choose", "/etc/hosts", "off"]), "usage", "usage");
    machine_error(&h.run("b", &["choose", "agents", "backup"]), "usage", "usage");
    machine_error(&h.run("b", &["choose", "Documents", "backup"]), "usage", "usage");
    machine_error(&h.run("b", &["choose", &claude]), "usage", "usage");
    // a link would be neither synced nor backed up from here
    let real = h.path("home/dots-bashrc");
    std::fs::write(&real, "x\n").unwrap();
    let bashrc = h.path("home/.bashrc");
    std::os::unix::fs::symlink(&real, &bashrc).unwrap();
    machine_error(
        &h.run("b", &["choose", &bashrc.to_string_lossy(), "backup"]),
        "usage",
        "usage",
    );
    std::fs::remove_file(&bashrc).unwrap();
    // a file in a folder, by this computer's ignore rules
    let note = h.path("home/Documents/draft.md");
    std::fs::create_dir_all(note.parent().unwrap()).unwrap();
    std::fs::write(&note, "x\n").unwrap();
    json(&h.run("b", &["choose", &note.to_string_lossy(), "off"]));
    let rules = std::fs::read_to_string(h.path("b.ignore")).unwrap_or_default();
    assert!(rules.lines().any(|l| l == "/Documents/draft.md"), "{rules}");
    json(&h.run("b", &["choose", &note.to_string_lossy(), "sync"]));
    let rules = std::fs::read_to_string(h.path("b.ignore")).unwrap_or_default();
    assert!(!rules.contains("draft.md"), "{rules}");

    // a folder of one's own, by name alone, and a usual one never synced
    std::fs::create_dir_all(h.path("home/Work")).unwrap();
    let s = json(&h.run("b", &["choose", "Work", "sync"]));
    assert_eq!(s["data"]["what"], "Work", "{s}");
    let s = json(&h.run("b", &["choose", "Music", "sync"]));
    assert_eq!(s["data"]["what"], "Music", "{s}");

    // every computer's choices, side by side
    json(&h.run("a", &["sync"]));
    json(&h.run("b", &["sync"]));
    // what b chose since its last sync reaches a with the next ones
    json(&h.run("a", &["sync"]));
    let all = json(&h.run("a", &["choose"]));
    let items = all["data"]["items"].as_array().unwrap();
    let find = |name: &str| {
        items
            .iter()
            .find(|i| i["name"] == name)
            .unwrap_or_else(|| panic!("{name}: {all}"))
    };
    assert_eq!(find("~/.claude/CLAUDE.md")["choices"]["b"], "backup", "{all}");
    assert_eq!(find("~/.config/hypr/monitors.lua")["choices"]["b"], "off", "{all}");
    assert_eq!(find("agents")["choices"]["a"], "sync", "{all}");
    assert_eq!(find("Work")["choices"]["b"], "sync", "{all}");
    assert_eq!(find("Music")["choices"]["b"], "sync", "{all}");
    // back to synced
    assert_eq!(
        json(&h.run("b", &["choose", &claude, "sync"]))["data"]["choice"],
        "sync"
    );
    let s = json(&h.run("b", &["settings", "status"]));
    assert_eq!(s["data"]["own"], serde_json::json!([]), "{s}");
}

#[test]
fn mount_shows_every_file_read_only() {
    let fuse = std::path::Path::new("/dev/fuse").exists()
        && std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("fusermount3").exists()));
    if !fuse {
        eprintln!("skipped: no FUSE here");
        return;
    }
    let h = Home::new();
    h.create();
    std::fs::create_dir_all(h.path("A/trip")).unwrap();
    std::fs::write(h.path("A/trip/photo.jpg"), "jpeg bytes").unwrap();
    assert_eq!(code(&h.run("a", &["sync"])), 0);

    let dir = h.path("view");
    let mut child = h
        .cmd("a", &["mount", &dir.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let photo = dir.join("trip/photo.jpg");
    let start = std::time::Instant::now();
    while !photo.exists() {
        assert!(start.elapsed().as_secs() < 30, "not mounted");
        assert!(child.try_wait().unwrap().is_none(), "mount exited");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(std::fs::read_to_string(&photo).unwrap(), "jpeg bytes");
    let e = std::fs::write(&photo, "changed").unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EROFS), "{e}");
    assert!(!dir.join(".omacloud").exists());

    // SAFETY: a signal to the child started here
    unsafe { libc::kill(i32::try_from(child.id()).unwrap(), libc::SIGTERM) };
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "still mounted");
}

#[test]
fn a_computer_set_up_on_storage_thats_gone_is_told_and_set_up_again() {
    let h = Home::new();
    let config = h.path("a.toml");
    std::fs::write(
        &config,
        format!(
            "device = \"a\"\nfolder = \"{}\"\ncoordinator = \"bucket\"\nroot = \"{}\"\n\
             [repo]\nrepository = \"opendal:dropbox\"\n[repo.options]\nroot = \"/omacloud\"\n",
            h.path("A").display(),
            "ab".repeat(32)
        ),
    )
    .unwrap();
    let v = json(&h.run("a", &["status"]));
    assert_eq!(v["data"]["set_up"], false, "{v}");
    assert_eq!(v["data"]["storage_gone"], "the earlier Dropbox app");
    let e = machine_error(&h.run("a", &["sync"]), "storage_gone", "setup");
    assert!(e["hint"].as_str().unwrap().contains("omacloud init"), "{e}");
    // the daemon stops for good rather than restarting into it
    assert_eq!(code(&h.run("a", &["watch"])), 2);

    // init keeps the old setup aside and sets up again
    h.create();
    assert!(h.path("a.toml.before-bucket").exists());
    assert_eq!(json(&h.run("a", &["status"]))["data"]["repo"]["kind"], "path");
}

#[test]
fn a_computer_on_the_old_network_is_told_too() {
    let h = Home::new();
    std::fs::write(
        h.path("a.toml"),
        format!(
            "device = \"a\"\nfolder = \"{}\"\ncoordinator = \"network\"\nroot = \"{}\"\n\
             [repo]\nrepository = \"network\"\n[network]\nurl = \"https://omacloud.computer\"\n",
            h.path("A").display(),
            "ab".repeat(32)
        ),
    )
    .unwrap();
    let v = json(&h.run("a", &["status"]));
    assert_eq!(v["data"]["storage_gone"], "the Omacloud network", "{v}");
    machine_error(&h.run("a", &["sync"]), "storage_gone", "setup");
    assert_eq!(code(&h.run("a", &["watch"])), 2);

    // what the network left goes when it's set up again, and nothing else
    let data = h.path("a.d");
    std::fs::create_dir_all(data.join("node/blobs")).unwrap();
    std::fs::write(data.join("node/blobs/others"), "ciphertext").unwrap();
    std::fs::write(data.join("account.seed"), "seed").unwrap();
    std::fs::write(data.join("keep.me"), "mine").unwrap();
    h.create();
    assert!(!data.join("node").exists() && !data.join("account.seed").exists());
    assert!(data.join("keep.me").exists());
    assert!(h.path("a.toml.before-bucket").exists());
}

#[test]
fn init_says_where_the_files_should_live() {
    let h = Home::new();
    let e = machine_error(&h.run("a", &["init"]), "usage", "usage");
    assert!(e["hint"].as_str().unwrap().contains("--provider"), "{e}");
    // a provider's shorthand needs what it's made from
    machine_error(
        &h.run("a", &["init", "--provider", "r2", "--bucket", "b"]),
        "usage",
        "usage",
    );
    assert!(!h.path("a.toml").exists());
}

#[test]
fn a_damaged_join_code_is_refused_and_an_invite_needs_a_terminal() {
    let h = Home::new();
    let e = machine_error(
        &h.cmd("a", &["init"])
            .env("OMACLOUD_JOIN_CODE", "omacloud-join-00")
            .output()
            .unwrap(),
        "join_code_rejected",
        "setup",
    );
    assert!(e["hint"].as_str().unwrap().contains("join-code"), "{e}");
    machine_error(
        &h.run(
            "a",
            &[
                "invite",
                "--provider",
                "s3",
                "--endpoint",
                "http://localhost:1",
                "--bucket",
                "b",
                "--access-key-id",
                "k",
            ],
        ),
        "terminal_required",
        "usage",
    );
}

#[test]
fn a_config_named_config_toml_elsewhere_keeps_its_own_data() {
    let h = Home::new();
    std::fs::create_dir_all(h.path("elsewhere")).unwrap();
    let args = h.init_args("a");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (out, code) = on_tty(h.cmd("elsewhere/config", &args));
    assert_eq!(code, 0, "{out}");
    assert!(h.path("elsewhere/config.d/device.key").exists());
    // never the data directory of the account the default config is for
    assert!(!h.path("home/.local/share/omacloud").exists());
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
    let e = machine_error(&h.run("a", &args), "exists", "usage");
    assert!(e["hint"].as_str().unwrap().contains("--yes"), "{e}");
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "mine");
    let mut yes = args.to_vec();
    yes.push("--yes");
    let v = json(&h.run("a", &yes));
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "one");
}

/// `omacloud open` becomes the window's program (here a script that says
/// what it was given), on the page asked for, with this CLI and the config
/// handed on; the launcher entry and the bar run it.
#[test]
fn open_starts_the_window_with_this_cli_and_config() {
    use std::os::unix::fs::PermissionsExt;
    let h = Home::new();
    let app = h.path("omacloud-app");
    let said = h.path("said");
    std::fs::write(
        &app,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" \"$OMACLOUD_BIN\" \"$OMACLOUD_CONFIG\" > '{}'\n",
            said.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = h
        .cmd("a", &["open", "devices"])
        .env("OMACLOUD_APP", &app)
        .env_remove("OMACLOUD_BIN")
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let said = std::fs::read_to_string(&said).unwrap();
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines[0], "devices");
    assert_eq!(
        std::path::Path::new(lines[1]),
        std::path::Path::new(env!("CARGO_BIN_EXE_omacloud"))
    );
    assert_eq!(std::path::Path::new(lines[2]), h.path("a.toml"));

    // a page the window doesn't have is a usage error, and no window opens
    let out = h
        .cmd("a", &["open", "nowhere"])
        .env("OMACLOUD_APP", &app)
        .output()
        .unwrap();
    machine_error(&out, "usage", "usage");

    // no window installed: said, not crashed
    let out = h
        .cmd("a", &["open"])
        .env("OMACLOUD_APP", h.path("missing"))
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
}

/// The command line never links GTK, so `--help`, `--version` and every
/// command run where there is no display; the window is a program of its own.
#[test]
fn the_cli_links_no_gtk_and_needs_no_display() {
    let binary = std::fs::read(env!("CARGO_BIN_EXE_omacloud")).unwrap();
    for library in [&b"libgtk-4.so"[..], b"libgdk", b"libwayland-client.so"] {
        assert!(
            !binary.windows(library.len()).any(|w| w == library),
            "{}",
            String::from_utf8_lossy(library)
        );
    }
    for args in [&["--help"][..], &["--version"], &["open", "--help"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_omacloud"))
            .args(args)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .output()
            .unwrap();
        assert_eq!(code(&out), 0, "{args:?}: {}", stderr(&out));
        assert!(!stdout(&out).is_empty());
    }
}
