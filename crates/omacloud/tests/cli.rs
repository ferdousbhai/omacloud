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
    /// kept out: home, config and data directories, keyring, systemd.
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
            .env("OMACLOUD_KEYRING", "0")
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

/// Runs `cmd` with stdout on a new pseudo-terminal; what it printed there
/// (stderr too, as on a terminal) and the exit code.
fn on_tty(mut cmd: Command) -> (String, i32) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty fills in two new descriptors, owned from here on
    let (master, slave) = unsafe {
        assert_eq!(
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null()
            ),
            0
        );
        (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
    };
    cmd.stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut child = cmd.spawn().unwrap();
    // the parent's copies of the terminal side go, so reading ends with the child
    drop(cmd);
    let mut out = Vec::new();
    let mut master = master;
    let mut buf = [0u8; 4096];
    loop {
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            // EIO: the child closed the terminal
            Err(_) => break,
        }
    }
    let status = child.wait().unwrap();
    (String::from_utf8_lossy(&out).replace('\r', ""), status.code().unwrap())
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
    // creating an account shows its recovery code: refused before anything is made
    let args = h.init_args("a");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    machine_error(&h.run("a", &args), "terminal_required", 2);
    assert!(!h.path("a.toml").exists());
    assert!(!h.path("repo").exists() || std::fs::read_dir(h.path("repo")).unwrap().next().is_none());

    let recovery = h.create();
    for args in [
        &["join-code"][..],
        &["export"],
        &["recovery", "contact"],
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
fn unreachable_storage_is_exit_5() {
    let h = Home::new();
    let folder = h.path("N");
    std::fs::write(
        h.path("n.toml"),
        format!(
            "device = \"n\"\nfolder = {folder:?}\ncoordinator = \"bucket\"\nroot = \"00\"\n\
             [repo]\nrepository = \"\"\n[repo.options]\n\
             [coordination]\nscheme = \"s3\"\nendpoint = \"http://127.0.0.1:1\"\nbucket = \"b\"\n\
             region = \"us-east-1\"\naccess_key_id = \"k\"\nsecret_access_key = \"s\"\nroot = \"/c\"\n"
        ),
    )
    .unwrap();
    let e = machine_error(&h.run("n", &["sync"]), "unavailable", 5);
    assert!(e["hint"].as_str().is_some_and(|h| h.contains("network")));
    let text = h.run("n", &["status", "--styled"]);
    assert_eq!(code(&text), 5);
    assert!(stderr(&text).contains("hint: "));
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

#[test]
fn a_bucket_key_kept_in_a_keyring_that_is_off_is_exit_3() {
    let h = Home::new();
    let folder = h.path("K");
    std::fs::write(
        h.path("k.toml"),
        format!(
            "device = \"k\"\nfolder = {folder:?}\ncoordinator = \"bucket\"\nroot = \"00\"\n\
             [repo]\nrepository = \"\"\n[repo.options]\n\
             [coordination]\nscheme = \"s3\"\nendpoint = \"http://127.0.0.1:1\"\nbucket = \"b\"\n\
             region = \"us-east-1\"\nroot = \"/c\"\n"
        ),
    )
    .unwrap();
    let e = machine_error(&h.run("k", &["sync"]), "keyring_off", 3);
    assert_eq!(e["hint"], "unset OMACLOUD_KEYRING");
}
