//! The Omacloud window: account, devices, settings sync, secrets and
//! recovery. It works through the `omacloud` CLI (`status` to read, the
//! usual subcommands to act, all with `--json`: it reads the envelope's
//! `data`, or its `error`), so it holds no keys or state of its own and never
//! races the daemon over them. The commands that print a secret (`init`,
//! `join-code`) print it only to a terminal: the app runs those on a
//! pseudo-terminal of its own.
//!
//! `omacloud open [overview|devices|settings|secrets|recovery]` starts it on
//! that page: the CLI execs this program, installed as
//! /usr/lib/omacloud/omacloud-app. `OMACLOUD_BIN` picks the CLI (default:
//! `omacloud` on PATH; `omacloud open` hands on itself) and `OMACLOUD_CONFIG`
//! a config other than the default.
//!
//! Every word the window shows is in Omarchy's language and follows a switch
//! live. The CLI speaks English: its failures are shown in the language by
//! their `code` where the set is fixed ([`Failure::known`]), and as the CLI
//! wrote them otherwise.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    process::{Command, Stdio},
    rc::Rc,
};

use omui::gtk::{self, gio, glib, prelude::*};
use omui::i18n::{self, bind_tooltip};
use omui::{Appearance, Dialog, EmptyState, ExpanderRow, Group, Nav, Row, icons::name, tr, trn};
use serde_json::Value;

/// The GApplication id, for D-Bus: one that was already this, so a running
/// window is found and brought forward by every version.
const APP_ID: &str = "com.ferdousbhai.Omacloud";
/// Omarchy's one name: the command, the package, the `.desktop` file, the
/// icon and the Wayland app id of every window (omui sets it).
const NAME: &str = "omacloud";

fn main() -> glib::ExitCode {
    i18n::register_all(&[("en", include_str!("i18n/en.json"))]);
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let page = args.next();
    // one window: launching again brings it forward instead of opening another
    let app = omui::application(APP_ID, NAME, "Omacloud", "", move |app| {
        build(app, page.as_deref());
    });
    // the page is ours to read; GTK sees no arguments
    app.run_with_args(&[argv0])
}

/// A command that failed. What the CLI said is kept in its English, and put
/// in the user's language only as it is shown ([`Failure::shown`]), so text
/// that stays on screen is shown anew after a switch.
#[derive(Clone, Debug)]
struct Failure {
    /// The CLI's error code; one of the app's own (`app-…`) for a failure of
    /// running it at all; empty for a text-mode run or another program.
    code: String,
    /// The error's kind (`setup`, `unavailable`, …), from the CLI.
    kind: String,
    /// Whether `omacloud` said it, whose codes [`Failure::known`] knows.
    omacloud: bool,
    /// What went wrong, as the program said it, or the app's detail.
    message: String,
    /// The program's advice on fixing it.
    hint: String,
}

impl Failure {
    /// What another program, or a text-mode run, said, as it said it.
    fn said(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code: String::new(),
            kind: String::new(),
            omacloud: false,
            message: message.into(),
            hint: hint.into(),
        }
    }

    /// A failure of the app's own: `code` names it, `detail` is filled in.
    fn local(code: &str, detail: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            ..Self::said(detail, "")
        }
    }

    /// The failures whose meaning is fixed, said in the user's language
    /// whatever detail the CLI gave; None for the rest.
    fn known(&self) -> Option<String> {
        let text = match self.code.as_str() {
            "app-run" => return Some(tr!("Couldn't run {}", self.message)),
            "app-terminal" => return Some(tr!("Couldn't give the command a terminal: {}", self.message)),
            "app-crashed" => tr("The command stopped before it was done."),
            "app-silent" => return Some(tr!("{} failed without saying why.", self.message)),
            _ if !self.omacloud => return None,
            "not_set_up" => tr("This computer isn't set up with Omacloud yet."),
            "storage_gone" => tr(
                "This computer was set up on storage Omacloud no longer uses. Set it up again: your files here stay \
                 as they are.",
            ),
            "dropbox_not_allowed" => {
                tr("Omacloud wasn't allowed in Dropbox. Try again, and click Allow in the browser.")
            }
            "dropbox_code_rejected" => tr("Dropbox didn't let Omacloud in. Allow it again in the browser."),
            "not_approved" => tr(
                "This computer isn't approved yet. On a computer that syncs, approve it under Devices by its \
                 fingerprint.",
            ),
            "removed" => tr("This computer was removed from the account. Its files here stay as they are."),
            "recovery_code_rejected" => tr("That recovery code doesn't open this account. Check it and try again."),
            "join_code_rejected" => {
                tr("That join code doesn't work. Copy the whole code again from Show Join Code on your other computer.")
            }
            "no_account" => tr(
                "There is no Omacloud account in this storage. Check that it's the Dropbox or bucket the code is for.",
            ),
            "no_hard_links" => tr(
                "That folder can't keep an account: its drive has no hard links. Use a drive formatted as ext4 or \
                 btrfs, or a NAS's NFS share.",
            ),
            "folder_not_writable" => tr("That folder can't be written to. Check the drive and who may write there."),
            "key_refused" => tr(
                "Your storage refused the key. Check the key's ID and secret, and that it may read and write the \
                 bucket; on Dropbox, allow Omacloud again.",
            ),
            "settings_off" => tr("This travels with settings sync, which is off here. Turn it on under Omarchy."),
            "already_set_up" => tr("This computer is already set up."),
            "cancelled" => tr("Cancelled"),
            _ => return None,
        };
        Some(text)
    }

    /// What the program said: its message, and its fix when it gave one.
    fn english(&self) -> String {
        let mut hint = self.hint.chars();
        match hint.next() {
            Some(first) => format!("{}. {}{}", self.message, first.to_uppercase(), hint.as_str()),
            None => self.message.clone(),
        }
    }

    /// What to show, in the language of the moment where the failure is one
    /// [`Failure::known`] knows, and as the program said it otherwise.
    fn shown(&self) -> String {
        self.known().unwrap_or_else(|| self.english())
    }
}

/// `argv` as a command: `omacloud ...` means the CLI with this app's config.
fn command(argv: &[String], env: &[(String, String)]) -> Command {
    let (program, args) = argv.split_first().expect("a program to run");
    let mut cmd = if program == "omacloud" {
        let mut c = Command::new(std::env::var("OMACLOUD_BIN").unwrap_or_else(|_| program.clone()));
        if let Ok(config) = std::env::var("OMACLOUD_CONFIG") {
            c.arg("--config").arg(config);
        }
        c
    } else {
        Command::new(program)
    };
    cmd.args(args)
        .env("RUST_LOG", "warn")
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null());
    cmd
}

/// A failure from what a text-mode run left on stderr: its `error: ` line
/// (or the last line that says something) and the `hint: ` after it. A
/// program that said nothing is named.
fn text_failure(stderr: &[u8], program: &str) -> Failure {
    let err = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
    let error = lines.iter().rposition(|l| l.starts_with("error: "));
    let Some(message) = error
        .map(|i| &lines[i]["error: ".len()..])
        .or_else(|| lines.last().copied())
    else {
        return Failure::local("app-silent", program);
    };
    let hint = error
        .and_then(|i| lines[i + 1..].iter().find_map(|l| l.strip_prefix("hint: ")))
        .unwrap_or_default();
    Failure::said(message, hint)
}

/// Run another program to completion: its stdout, or the last line of
/// stderr on failure.
fn run(argv: &[String]) -> Result<String, Failure> {
    let out = command(argv, &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Failure::local("app-run", format!("{}: {e}", argv[0])))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(text_failure(&out.stderr, &argv[0]))
}

/// Run `omacloud ...` (or another program with the same contract) with
/// `--json`: the result's `data`, or its error.
fn cli(argv: &[String], env: &[(String, String)]) -> Result<Value, Failure> {
    let out = command(argv, env)
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Failure::local("app-run", format!("{}: {e}", argv[0])))?;
    let mut result = envelope(&out.stdout, &out.stderr, &argv[0]);
    if let Err(e) = &mut result {
        e.omacloud = argv[0] == "omacloud";
    }
    result
}

/// What a `--json` run printed: the result's `data`, or its error. A result
/// is on stdout, and stands even when the command then fell short of
/// something (the app has always taken the result then). A failure is the
/// error object, the last line on stderr after any warning logged before
/// it; an older omacloud printed it on stdout instead, and is still read.
fn envelope(stdout: &[u8], stderr: &[u8], program: &str) -> Result<Value, Failure> {
    let parse = |bytes: &[u8]| serde_json::from_slice::<Value>(bytes).ok();
    let error = String::from_utf8_lossy(stderr);
    let last = error.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or_default();
    let found = parse(stdout).or_else(|| parse(last.as_bytes()));
    match found {
        Some(v) if v["ok"] == Value::Bool(true) => Ok(v["data"].clone()),
        Some(v) if v["ok"] == Value::Bool(false) => Err(Failure {
            code: text(&v["error"]["code"]),
            kind: text(&v["error"]["kind"]),
            ..Failure::said(text(&v["error"]["message"]), text(&v["error"]["hint"]))
        }),
        // no envelope: it crashed, or is an omacloud from before the contract
        _ => Err(text_failure(stderr, program)),
    }
}

/// A new pseudo-terminal: its controlling side and its terminal side.
/// Neither descriptor survives an exec (`O_CLOEXEC` from the start, so no
/// process another thread starts meanwhile gets one either): only the
/// child's stdin and stdout, dup2ed onto it, are the terminal. Otherwise
/// the child, and a process it leaves running, would keep the terminal side
/// open (and the reading here would wait for that process to end), and the
/// controlling side, which reads what the child prints.
fn pty() -> std::io::Result<(File, OwnedFd)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    let fail = |r: libc::c_int| {
        if r < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(r)
        }
    };
    // SAFETY: each call gets a descriptor it owns from here on, or fails
    unsafe {
        let master = File::from_raw_fd(fail(libc::posix_openpt(flags))?);
        let fd = master.as_raw_fd();
        fail(libc::grantpt(fd))?;
        fail(libc::unlockpt(fd))?;
        let mut name = [0 as libc::c_char; 128];
        let r = libc::ptsname_r(fd, name.as_mut_ptr(), name.len());
        if r != 0 {
            return Err(std::io::Error::from_raw_os_error(r));
        }
        let slave = OwnedFd::from_raw_fd(fail(libc::open(name.as_ptr(), flags))?);
        Ok((master, slave))
    }
}

/// Run `omacloud ...` with its stdin and stdout on a pseudo-terminal of
/// this app's own: the commands that print a secret print it only to a
/// person's terminal (both of them one), and this window is the owner's.
/// What it printed there, or its error.
fn cli_tty(argv: &[String], env: &[(String, String)]) -> Result<String, Failure> {
    let no_terminal = |e: std::io::Error| Failure::local("app-terminal", e.to_string());
    let (mut master, slave) = pty().map_err(no_terminal)?;
    let mut cmd = command(argv, env);
    let stdin = slave.try_clone().map_err(no_terminal)?;
    cmd.stdin(Stdio::from(stdin))
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| Failure::local("app-run", format!("{}: {e}", argv[0])))?;
    // the terminal side closes with the child: reading then ends
    drop(cmd);
    // nobody answers on this terminal: an end of input (^D) waits there,
    // so a question the CLI would ask ends at once instead of hanging
    _ = master.write_all(b"\x04");
    let mut stderr = child.stderr.take();
    let errors = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(e) = stderr.as_mut() {
            _ = e.read_to_end(&mut buf);
        }
        buf
    });
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    // EIO once the child closed the terminal
    while let Ok(n @ 1..) = master.read(&mut buf) {
        out.extend_from_slice(&buf[..n]);
    }
    let status = child
        .wait()
        .map_err(|e| Failure::local("app-run", format!("{}: {e}", argv[0])))?;
    let errors = errors.join().unwrap_or_default();
    if status.success() {
        return Ok(String::from_utf8_lossy(&out).replace('\r', ""));
    }
    let mut failure = text_failure(&errors, &argv[0]);
    failure.omacloud = argv[0] == "omacloud";
    Err(failure)
}

/// Run `job` in the background, then hand its result to `then` on the main
/// loop: [`omui::background`], which holds the application until `then` has
/// run. Closing the last window would otherwise end the application under a
/// command that is still running, and what `then` does with its answer
/// (starting the daemon once setup is done, writing the files a restore
/// listed) would never happen; the app stays, with no window, until its work
/// is done.
fn in_background<T: Send + 'static>(
    job: impl FnOnce() -> Result<T, Failure> + Send + 'static,
    then: impl FnOnce(Result<T, Failure>) + 'static,
) {
    omui::background(job, move |result| {
        then(result.unwrap_or_else(|_| Err(Failure::local("app-crashed", ""))));
    });
}

fn owned(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|s| (*s).to_string()).collect()
}

/// [`run`] in the background.
fn run_then(argv: &[&str], then: impl FnOnce(Result<String, Failure>) + 'static) {
    let argv = owned(argv);
    in_background(move || run(&argv), then);
}

/// [`cli`] in the background.
fn cli_then(argv: &[&str], env: Vec<(String, String)>, then: impl FnOnce(Result<Value, Failure>) + 'static) {
    let argv = owned(argv);
    in_background(move || cli(&argv, &env), then);
}

/// [`cli_tty`] in the background.
fn tty_then(argv: &[&str], env: Vec<(String, String)>, then: impl FnOnce(Result<String, Failure>) + 'static) {
    let argv = owned(argv);
    in_background(move || cli_tty(&argv, &env), then);
}

struct Ui {
    window: gtk::ApplicationWindow,
    stack: gtk::Stack,
    /// Its tabs and sync button, which mean nothing until setup is done.
    header: gtk::HeaderBar,
    shell: Rc<omui::Shell>,
    overview: gtk::Box,
    devices: gtk::Box,
    settings: gtk::Box,
    secrets: gtk::Box,
    recovery: gtk::Box,
    /// Whether the overview page holds setup, which may be partway through.
    setting_up: Cell<bool>,
    /// Setup's pages while they are shown, for drawing them again in another language.
    setup_nav: RefCell<Option<omui::WeakNav>>,
    /// The last `omacloud status`: the pages are drawn from it, after each
    /// change and again after a switch of language.
    status: RefCell<Option<Result<Value, Failure>>>,
    /// Each folder's size as `du` last measured it, so that a page drawn
    /// again shows it at once.
    sizes: RefCell<HashMap<String, u64>>,
    /// The desktop's file chooser, through its portal (the scripted QA answers in the portal's place).
    picker: Rc<dyn omui::FilePicker>,
}

/// Makes `child` all that a page of the window holds.
fn put(page: &gtk::Box, child: &impl IsA<gtk::Widget>) {
    omui::clear(page);
    child.set_hexpand(true);
    page.append(child);
}

impl Ui {
    fn toast(&self, text: &str) {
        self.shell.toast(text);
    }

    /// Run an action, say how it went, and show the new state.
    fn act(self: &Rc<Self>, argv: &[&str], done: &str) {
        self.act_env(argv, Vec::new(), done);
    }

    /// [`Ui::act`], with secrets passed through the environment.
    fn act_env(self: &Rc<Self>, argv: &[&str], env: Vec<(String, String)>, done: &str) {
        let (ui, done) = (self.clone(), done.to_string());
        let then = move |r: Result<(), Failure>| {
            ui.toast(&r.map_or_else(|e| e.shown(), |()| done));
            ui.refresh();
        };
        if argv.first() == Some(&"omacloud") {
            cli_then(argv, env, move |r| then(r.map(drop)));
        } else {
            run_then(argv, move |r| then(r.map(drop)));
        }
    }

    fn show_tabs(&self, show: bool) {
        self.header.set_visible(show);
    }

    /// Asks for the status again, and draws the pages from it.
    fn refresh(self: &Rc<Self>) {
        // A command that outlived the window (`in_background`) has no page
        // to draw again, and asking for the status would only keep the app
        // from ending. A closed window has left its application.
        if self.window.application().is_none() {
            return;
        }
        let ui = self.clone();
        cli_then(&["omacloud", "status"], Vec::new(), move |status| {
            if let Ok(s) = &status
                && s["set_up"] == Value::Bool(false)
                && let Some(kind) = s["storage_gone"].as_str()
            {
                ui.toast(&tr!(
                    "This computer was set up on {}, which Omacloud no longer uses. Set it up again: your files \
                     here stay as they are.",
                    kind
                ));
            }
            ui.status.replace(Some(status));
            ui.render(false);
        });
    }

    /// Draws the pages from the last status. `again` is a switch of
    /// language: the same pages are drawn in it, with what was typed and
    /// where the keyboard was kept, since nothing else changed.
    fn render(self: &Rc<Self>, again: bool) {
        let Some(status) = self.status.borrow().clone() else {
            return;
        };
        match status {
            // no config yet: this computer needs setting up
            Ok(s) if s["set_up"] == Value::Bool(false) => {
                self.show_tabs(false);
                if again && self.setting_up.get() {
                    self.setup_again();
                } else if !self.setting_up.replace(true) {
                    // not while someone is partway through it
                    put(&self.overview, &setup(self, None));
                }
                self.stack.set_visible_child_name("overview");
            }
            Ok(s) => {
                self.show_tabs(true);
                // setup's pages are another thing: the keyboard has no place to keep there
                let was_setting_up = self.setting_up.replace(false);
                self.setup_nav.replace(None);
                let root = self.stack.upcast_ref::<gtk::Widget>();
                let (focus, places) = if was_setting_up {
                    (None, None)
                } else if again {
                    (None, focus_places(root))
                } else {
                    (focus_path(root), None)
                };
                put(&self.overview, &overview(self, &s));
                put(&self.devices, &devices(self, &s));
                put(&self.settings, &settings(self, &s));
                put(&self.secrets, &secrets(self, &s));
                put(&self.recovery, &recovery());
                if let Some(path) = focus {
                    focus_at(root, &path);
                }
                if let Some(places) = places {
                    focus_places_at(root, &places);
                }
            }
            Err(e) => {
                self.show_tabs(true);
                self.setting_up.set(false);
                self.setup_nav.replace(None);
                let why = match e.kind.as_str() {
                    "unavailable" if e.omacloud => tr!("Can't reach your storage: {}", e.shown()),
                    _ => e.shown(),
                };
                for page in [
                    &self.overview,
                    &self.devices,
                    &self.settings,
                    &self.secrets,
                    &self.recovery,
                ] {
                    put(page, &not_set_up(&why).widget);
                }
            }
        }
    }

    /// Setup drawn again in the language of the moment, on the page it was
    /// on, with what had been typed and chosen there and the keyboard where
    /// it was: the pages are built by the same code from the same status,
    /// so each control stands where it stood.
    fn setup_again(self: &Rc<Self>) {
        let nav = self.setup_nav.borrow().as_ref().and_then(omui::WeakNav::upgrade);
        let Some(nav) = nav else {
            put(&self.overview, &setup(self, None));
            return;
        };
        let tag = nav.visible_tag();
        let shown = nav.widget.visible_child();
        let values = shown.as_ref().map(form_values).unwrap_or_default();
        let places = focus_places(self.overview.upcast_ref());
        put(&self.overview, &setup(self, tag.as_deref()));
        let nav = self.setup_nav.borrow().as_ref().and_then(omui::WeakNav::upgrade);
        if let Some(shown) = nav.and_then(|nav| nav.widget.visible_child()) {
            set_form_values(&shown, &values);
        }
        if let Some(places) = places {
            focus_places_at(self.overview.upcast_ref(), &places);
        }
    }

    /// A question with Cancel and `responses`: `then` runs with the id of the
    /// response chosen. Escape, and any other way of putting it away, is
    /// Cancel, which loses nothing. The heading, the body and each response's
    /// label are asked for again after a switch of language, so an open
    /// question follows it.
    fn confirm(
        self: &Rc<Self>,
        heading: impl Fn() -> String + 'static,
        body: impl Fn() -> String + 'static,
        responses: &[Response],
        then: impl Fn(&str) + 'static,
    ) {
        let dialog = Dialog::new(Some(""), Some(""));
        dialog.add_response("cancel", "");
        dialog.set_response_translated("cancel", "Cancel");
        let mut ids = Vec::new();
        let mut labels = Vec::new();
        for (id, label, look) in responses {
            dialog.add_response(id, "");
            dialog.set_response_appearance(id, *look);
            ids.push((*id).to_string());
            labels.push(((*id).to_string(), *label));
        }
        dialog.bind(move |dialog| {
            dialog.set_heading(&heading());
            dialog.set_body(&body());
            for (id, label) in &labels {
                dialog.set_response_label(id, &label());
            }
        });
        dialog.set_close_response("cancel");
        dialog.set_default_response("cancel");
        dialog.connect_response(move |_, id| {
            // a response this didn't add is the dialog put away
            if ids.iter().any(|known| known == id) {
                then(id);
            }
        });
        dialog.present(&self.window);
    }
}

/// A response of [`Ui::confirm`]: its id, its label in the language of the
/// moment, and how it looks.
type Response = (&'static str, fn() -> String, Appearance);

/// The keyboard's shortcuts: an action, its keys as GTK writes them, and the
/// key as README.md's table lists it (a test holds the two together). All
/// are Ctrl with a key, never a key alone, since setup and the dialogs are
/// typed into. A dialog holds them back while it is open (omui's `Dialog`).
/// Ctrl+Q and Ctrl+W are omui's [`omui::quit_keys`]; `?` and F1 open the
/// list of them all ([`shortcut_sections`]).
const SHORTCUTS: [(&str, &[&str], &str); 12] = [
    ("win.page('overview')", &["<Control>1"], "Ctrl+1"),
    ("win.page('devices')", &["<Control>2"], "Ctrl+2"),
    ("win.page('settings')", &["<Control>3"], "Ctrl+3"),
    ("win.page('secrets')", &["<Control>4"], "Ctrl+4"),
    ("win.page('recovery')", &["<Control>5"], "Ctrl+5"),
    ("win.sync", &["<Control>r"], "Ctrl+R"),
    // a page's own: each presses the button of that name on the page in view
    ("win.press('Show…')", &["<Control>d"], "Ctrl+D"),
    ("win.press('Open on a Phone…')", &["<Control>p"], "Ctrl+P"),
    ("win.press('Show Join Code')", &["<Control>j"], "Ctrl+J"),
    ("win.press('Save…')", &["<Control>s"], "Ctrl+S"),
    ("win.press('Start')", &["<Control>b"], "Ctrl+B"),
    ("win.continue", &["<Control>Return", "<Control>KP_Enter"], "Ctrl+Enter"),
];

/// The keys omui gives the window, as README.md lists them.
#[cfg(test)]
const OMUI_KEYS: [&str; 4] = ["Ctrl+Q", "Ctrl+W", "?", "F1"];

/// What the overlay on `?` lists: every key in [`SHORTCUTS`] and
/// [`OMUI_KEYS`], in the language of the moment.
fn shortcut_sections() -> Vec<omui::ShortcutSection> {
    vec![
        omui::ShortcutSection::new(
            &tr("Pages"),
            &[
                ("Ctrl+1", &tr("Overview")),
                ("Ctrl+2", &tr("Devices")),
                ("Ctrl+3", &tr("Omarchy settings")),
                ("Ctrl+4", &tr("Keys")),
                ("Ctrl+5", &tr("Recovery")),
            ],
        ),
        omui::ShortcutSection::new(
            &tr("On the page in view"),
            &[
                ("Ctrl+R", &tr("Sync now")),
                ("Ctrl+D", &tr("Show Recently Deleted")),
                ("Ctrl+P", &tr("Open your files on a phone")),
                ("Ctrl+J", &tr("Show the join code")),
                ("Ctrl+S", &tr("Save this device's secrets")),
                ("Ctrl+B", &tr("Start background sync")),
            ],
        ),
        omui::ShortcutSection::new(&tr("Setup"), &[("Ctrl+Enter", &tr("Go on to the next step"))]),
        omui::ShortcutSection::new(
            &tr("General"),
            &[
                ("? / F1", &tr("Show the keyboard shortcuts")),
                ("Ctrl+W", &tr("Close the window")),
                ("Ctrl+Q", &tr("Quit")),
            ],
        ),
    ]
}

/// The first button under `under` that is on screen, can be pressed and
/// that `wanted` picks. A page of the stack that isn't the one in view is
/// not on screen, nor is a row of a list that is folded away.
fn button_in_view(under: &gtk::Widget, wanted: &dyn Fn(&gtk::Button) -> bool) -> Option<gtk::Button> {
    children(under).find_map(|child| match child.downcast_ref::<gtk::Button>() {
        Some(button) if button.is_mapped() && button.is_sensitive() && wanted(button) => Some(button.clone()),
        _ => button_in_view(&child, wanted),
    })
}

/// Presses `button` as a click would, and leaves the keyboard on it, so
/// that a dialog it opens gives the keyboard back there when it closes.
fn press(button: &gtk::Button) {
    button.grab_focus();
    button.emit_clicked();
}

/// The actions [`SHORTCUTS`] names, and their keys; the quit keys and the
/// overlay that lists them all.
fn shortcuts(app: &gtk::Application, ui: &Rc<Ui>) {
    // Ctrl+Q closes every window and Ctrl+W this one, as their close buttons
    // would, never `Application::quit`, which ends the main loop under a
    // command still running: with its windows closed the app ends by itself,
    // once what `in_background` holds it for is done.
    omui::quit_keys(&ui.window);
    omui::ShortcutOverlay::new(shortcut_sections).install(&ui.window, false);

    /// A window action that runs `act` with the window's [`Ui`] and its
    /// string parameter, if any.
    fn add(ui: &Rc<Ui>, name: &str, parameter: Option<&glib::VariantTy>, act: impl Fn(&Rc<Ui>, &str) + 'static) {
        let action = gio::SimpleAction::new(name, parameter);
        // weak: the window owns the action
        let weak = Rc::downgrade(ui);
        action.connect_activate(move |_, parameter| {
            if let Some(ui) = weak.upgrade() {
                act(&ui, parameter.and_then(glib::Variant::str).unwrap_or_default());
            }
        });
        ui.window.add_action(&action);
    }
    // The tabs and Sync now are hidden through setup, and so are their keys.
    add(ui, "sync", None, |ui, _| {
        if ui.header.is_visible() {
            ui.act(&["omacloud", "sync"], &tr("Synced"));
        }
    });
    add(ui, "page", Some(glib::VariantTy::STRING), |ui, page| {
        if ui.header.is_visible() {
            ui.stack.set_visible_child_name(page);
            // the keyboard goes along, to the first control of the page
            if let Some(page) = ui.stack.visible_child() {
                page.child_focus(gtk::DirectionType::TabForward);
            }
        }
    });
    add(ui, "press", Some(glib::VariantTy::STRING), |ui, label| {
        // the button's label is in the language of the moment
        let label = i18n::translate(label);
        let page = ui.stack.visible_child();
        if let Some(button) = page.and_then(|page| button_in_view(&page, &|b| title(b.upcast_ref()) == label)) {
            press(&button);
        }
    });
    // Setup's pages each have one way on, the button that stands out.
    add(ui, "continue", None, |ui, _| {
        let on = |b: &gtk::Button| b.has_css_class("suggested-action");
        if ui.setting_up.get()
            && let Some(button) = button_in_view(ui.overview.upcast_ref(), &on)
        {
            press(&button);
        }
    });
    for (action, keys, _) in SHORTCUTS {
        app.set_accels_for_action(action, keys);
    }
}

/// A page's title in its tab, in the language of the moment.
fn page_title(tag: &str) -> String {
    match tag {
        "overview" => tr("Overview"),
        "devices" => tr("Devices"),
        // Omarchy is a name: the page of its settings
        "settings" => tr("Omarchy"),
        "secrets" => tr("Keys"),
        _ => tr("Recovery"),
    }
}

/// The main menu's items, in the language of the moment.
fn fill_menu(menu: &gio::Menu) {
    menu.remove_all();
    menu.append(Some(&tr("Move from iCloud…")), Some("win.move-from-icloud"));
}

fn build(app: &gtk::Application, page: Option<&str>) {
    let stack = gtk::Stack::new();
    // the tabs follow the pages as they are added, and their titles
    let header = omui::header(Some(omui::tabs(&stack).upcast_ref()));
    const PAGES: [(&str, &str); 5] = [
        ("overview", name::CLOUD),
        ("devices", name::COMPUTER),
        ("settings", name::SETTINGS),
        ("secrets", name::KEY),
        ("recovery", name::USERS),
    ];
    let holders: Vec<gtk::Box> = PAGES
        .iter()
        .map(|&(tag, icon)| {
            let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let spinner = omui::spinner();
            spinner.set_halign(gtk::Align::Center);
            spinner.set_valign(gtk::Align::Center);
            spinner.set_size_request(32, 32);
            spinner.set_vexpand(true);
            put(&holder, &spinner);
            stack
                .add_titled(&holder, Some(tag), &page_title(tag))
                .set_icon_name(icon);
            holder
        })
        .collect();
    {
        let stack = stack.downgrade();
        i18n::on_change(&header, move || {
            if let Some(stack) = stack.upgrade() {
                for (tag, _) in PAGES {
                    if let Some(child) = stack.child_by_name(tag) {
                        stack.page(&child).set_title(&page_title(tag));
                    }
                }
            }
        });
    }

    let sync = omui::icon_button(name::REFRESH, "");
    bind_tooltip(&sync, "Sync now");
    header.pack_end(&sync);
    // leaving iCloud is optional and rare: in this menu only, and only with
    // the iCloud apps installed
    let has_menu = on_path("icloud-session");
    if has_menu {
        let menu = gio::Menu::new();
        fill_menu(&menu);
        let button = gtk::MenuButton::builder()
            .icon_name(name::MENU)
            .has_frame(false)
            .menu_model(&menu)
            .build();
        bind_tooltip(&button, "Main menu");
        i18n::on_change(&button, move || fill_menu(&menu));
        header.pack_end(&button);
    }

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Omacloud")
        .default_width(1000)
        .default_height(640)
        .width_request(360)
        .height_request(400)
        .build();
    window.set_titlebar(Some(&header));
    let [overview, devices, settings, secrets, recovery]: [gtk::Box; 5] =
        holders.try_into().expect("a holder for each page");
    let ui = Rc::new(Ui {
        window: window.clone(),
        stack: stack.clone(),
        header,
        shell: omui::Shell::new(&window, &stack),
        overview,
        devices,
        settings,
        secrets,
        recovery,
        setting_up: Cell::new(false),
        setup_nav: RefCell::new(None),
        status: RefCell::new(None),
        sizes: RefCell::default(),
        picker: Rc::new(omui::PortalFilePicker::new()),
    });
    shortcuts(app, &ui);
    // the same action as Ctrl+R
    sync.set_action_name(Some("win.sync"));
    if has_menu {
        let action = gio::SimpleAction::new("move-from-icloud", None);
        let weak = Rc::downgrade(&ui);
        action.connect_activate(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                move_from_icloud(&ui);
            }
        });
        window.add_action(&action);
    }
    {
        // Everything the pages say is drawn again in the new language, from
        // the status they were drawn from: weak, as the window outlives this.
        let weak = Rc::downgrade(&ui);
        i18n::on_change(&window, move || {
            if let Some(ui) = weak.upgrade() {
                ui.render(true);
            }
        });
    }
    if let Some(page) = page {
        stack.set_visible_child_name(page);
    }
    ui.refresh();
    window.present();
}

/// One widget on the way from the pages down to the one with the keyboard: what it is, and where it stood.
struct Step {
    kind: glib::Type,
    /// A group's or a row's title, a button's label; empty for anything else.
    title: String,
    /// Its place among its siblings of that kind and title,
    nth: usize,
    /// and among them all.
    place: usize,
}

impl Step {
    fn of(widget: &gtk::Widget) -> Self {
        let like = |other: &gtk::Widget| other.type_() == widget.type_() && title(other) == title(widget);
        let before = || std::iter::successors(widget.prev_sibling(), gtk::Widget::prev_sibling);
        Self {
            kind: widget.type_(),
            title: title(widget),
            nth: before().filter(like).count(),
            place: before().count(),
        }
    }

    fn is(&self, widget: &gtk::Widget) -> bool {
        widget.type_() == self.kind && title(widget) == self.title
    }
}

fn children(of: &gtk::Widget) -> impl Iterator<Item = gtk::Widget> + use<> {
    std::iter::successors(of.first_child(), gtk::Widget::next_sibling)
}

/// What names `widget` on a page however the page is laid out: a button's label, and the title of one of omui's rows
/// or groups, read from the label omui gives the class `row-title` or `group-title`. Empty for any other widget.
fn title(widget: &gtk::Widget) -> String {
    fn label(under: &gtk::Widget, class: &str) -> Option<String> {
        children(under).find_map(|child| match child.downcast_ref::<gtk::Label>() {
            Some(label) if label.has_css_class(class) => Some(label.label().into()),
            _ => label(&child, class),
        })
    }
    if let Some(button) = widget.downcast_ref::<gtk::Button>() {
        button.label().unwrap_or_default().into()
    } else if widget.is::<gtk::ListBoxRow>() {
        label(widget, "row-title").unwrap_or_default()
    } else if widget.has_css_class("group") {
        label(widget, "group-title").unwrap_or_default()
    } else {
        String::new()
    }
}

/// Where the keyboard is under `root`: each widget from `root` down to the focused one. `None` when the keyboard is
/// elsewhere (the title bar, a dialog).
fn focus_path(root: &gtk::Widget) -> Option<Vec<Step>> {
    let mut at = root.root()?.focus()?;
    let mut path = Vec::new();
    while at != *root {
        path.push(Step::of(&at));
        at = at.parent()?;
    }
    path.reverse();
    Some(path)
}

/// Gives the keyboard to the widget at `path` ([`focus_path`]) under `root`. The pages are built again from each
/// new status, and the keyboard would otherwise be left on a row near the control someone used, or nowhere. Each
/// step is found by what it is, not by where it stood: a group that came or went above it moves every place after.
/// When the status changed the control itself, the keyboard goes to its row; when the row left with its group for
/// another (a computer approved), to the row there; and when the row is gone, to the row now nearest its place, or
/// the first stop for Tab around it. Never to a control that only stands where the old one did: Enter there would
/// do another thing.
fn focus_at(root: &gtk::Widget, path: &[Step]) {
    fn find(under: &gtk::Widget, step: &Step) -> Option<gtk::Widget> {
        children(under).find_map(|child| {
            if step.is(&child) {
                Some(child)
            } else {
                find(&child, step)
            }
        })
    }
    let row = gtk::ListBoxRow::static_type();
    let mut at = root.clone();
    let mut left = path;
    while let Some((step, rest)) = left.split_first() {
        let Some(same) = children(&at).filter(|child| step.is(child)).nth(step.nth) else {
            break;
        };
        (at, left) = (same, rest);
    }
    if let Some(gone) = left.first() {
        if let Some(around) = at.ancestor(row) {
            at = around;
        } else if let Some(i) = left.iter().position(|step| step.kind == row && !step.title.is_empty())
            && let Some(moved) = find(&at, &left[i])
        {
            return focus_at(&moved, &left[i + 1..]);
        } else if let Some(nearest) = children(&at).nth(gone.place).or_else(|| at.last_child()) {
            at = nearest;
        }
    }
    tab_into(root, at);
}

/// Gives the keyboard to `at` as Tab arrives: at a control itself, and in a list at a row, never at the list, where
/// nothing shows it.
fn tab_into(root: &gtk::Widget, mut at: gtk::Widget) {
    let row = gtk::ListBoxRow::static_type();
    while !at.child_focus(gtk::DirectionType::TabForward) && at != *root {
        // from inside a row to the row, not to the text in it that Tab stops at first
        let Some(around) = at.ancestor(row).filter(|row| *row != at).or_else(|| at.parent()) else {
            break;
        };
        at = around;
    }
}

/// Where the keyboard is under `root`, by place alone: the index of each widget among its siblings, from `root`
/// down. For drawing the same pages again in another language, where every title changed and nothing else did.
fn focus_places(root: &gtk::Widget) -> Option<Vec<usize>> {
    let mut at = root.root()?.focus()?;
    let mut places = Vec::new();
    while at != *root {
        places.push(std::iter::successors(at.prev_sibling(), gtk::Widget::prev_sibling).count());
        at = at.parent()?;
    }
    places.reverse();
    Some(places)
}

/// Gives the keyboard back at `places` ([`focus_places`]) under `root`, or as near it as the new pages go.
fn focus_places_at(root: &gtk::Widget, places: &[usize]) {
    let mut at = root.clone();
    for place in places {
        let Some(child) = children(&at).nth(*place) else {
            break;
        };
        at = child;
    }
    // in a field, the cursor comes back as it was left, not with the whole text selected
    let back = match at.downcast_ref::<gtk::Text>() {
        Some(text) => text.grab_focus_without_selecting(),
        None => at.is_focusable() && at.grab_focus(),
    };
    if !back {
        tab_into(root, at);
    }
}

/// One thing typed or chosen on a page of setup, in the order the page holds them.
#[derive(Clone, Debug, PartialEq)]
enum FormValue {
    Text(String),
    On(bool),
    Choice(u32),
    Expanded(bool),
}

/// The text of an entry or a password entry; None for any other widget.
fn entry_text(widget: &gtk::Widget) -> Option<String> {
    if let Some(entry) = widget.downcast_ref::<gtk::Entry>() {
        Some(entry.text().into())
    } else {
        widget
            .downcast_ref::<gtk::PasswordEntry>()
            .map(|entry| entry.text().into())
    }
}

/// What was typed and chosen under `under`, in the order its controls stand.
fn form_values(under: &gtk::Widget) -> Vec<FormValue> {
    let mut values = Vec::new();
    for child in children(under) {
        if let Some(text) = entry_text(&child) {
            values.push(FormValue::Text(text));
            continue;
        }
        if let Some(switch) = child.downcast_ref::<gtk::Switch>() {
            values.push(FormValue::On(switch.is_active()));
        } else if let Some(pick) = child.downcast_ref::<gtk::DropDown>() {
            values.push(FormValue::Choice(pick.selected()));
        } else if child.has_css_class("expander-row") {
            values.push(FormValue::Expanded(child.has_css_class("expanded")));
        }
        values.extend(form_values(&child));
    }
    values
}

/// Puts `values` ([`form_values`]) back under `under`, a page built by the same code: control by control, in order,
/// stopping where the two part.
fn set_form_values(under: &gtk::Widget, values: &[FormValue]) {
    fn walk(under: &gtk::Widget, values: &mut std::slice::Iter<FormValue>) -> bool {
        for child in children(under) {
            let is_entry = entry_text(&child).is_some();
            let is_control = is_entry
                || child.is::<gtk::Switch>()
                || child.is::<gtk::DropDown>()
                || child.has_css_class("expander-row");
            if is_control {
                let Some(value) = values.next() else {
                    return false;
                };
                match value {
                    FormValue::Text(text) if is_entry => {
                        if let Some(entry) = child.downcast_ref::<gtk::Entry>() {
                            entry.set_text(text);
                        } else if let Some(entry) = child.downcast_ref::<gtk::PasswordEntry>() {
                            entry.set_text(text);
                        }
                        continue;
                    }
                    FormValue::On(on) if child.is::<gtk::Switch>() => {
                        if let Some(switch) = child.downcast_ref::<gtk::Switch>() {
                            switch.set_active(*on);
                        }
                    }
                    FormValue::Choice(i) if child.is::<gtk::DropDown>() => {
                        if let Some(pick) = child.downcast_ref::<gtk::DropDown>() {
                            pick.set_selected(*i);
                        }
                    }
                    FormValue::Expanded(open) if child.has_css_class("expander-row") => {
                        // opened as a click on its header opens it: the row holds a list of one, the header
                        let header = children(&child)
                            .find_map(|content| children(&content).next())
                            .and_then(|list| list.first_child())
                            .and_downcast::<gtk::ListBoxRow>();
                        if *open
                            && !child.has_css_class("expanded")
                            && let Some(header) = header
                        {
                            header.emit_activate();
                        }
                    }
                    _ => return false,
                }
            }
            if !walk(&child, values) {
                return false;
            }
        }
        true
    }
    walk(under, &mut values.iter());
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn row(title: &str, subtitle: &str) -> Row {
    let row = Row::new(title, subtitle);
    row.set_subtitle_selectable(true);
    row
}

fn group(title: &str, description: &str) -> Group {
    Group::new(title, description)
}

/// A page of groups, as wide as a column of settings reads well.
fn page() -> (gtk::ScrolledWindow, gtk::Box) {
    omui::page(600)
}

fn button(label: &str, class: Option<&str>) -> gtk::Button {
    let b = gtk::Button::builder().label(label).valign(gtk::Align::Center).build();
    if let Some(c) = class {
        b.add_css_class(c);
    }
    b
}

fn not_set_up(why: &str) -> EmptyState {
    EmptyState::new(Some(name::CLOUD), &tr("Nothing to show yet"), Some(why))
}

/// Makes `child` the dialog's own widget, for one that only shows something:
/// with nothing in it to type in or choose, the dialog would give the keyboard
/// to the scroller it puts around the widget, and outline that, in place of a
/// response.
fn show_only(dialog: &Dialog, child: &impl IsA<gtk::Widget>) {
    dialog.set_extra_child(Some(child.as_ref()));
    if let Some(scroller) = child.ancestor(gtk::ScrolledWindow::static_type()) {
        // not `focusable`: a scrolled window that only isn't focusable still
        // claims the keyboard, and then no key reaches the dialog
        scroller.set_can_focus(false);
    }
}

/// A dialog that stays up through a long command, so the wait doesn't look like
/// nothing happening. Closed by the caller when it's done. `text` says what
/// it waits for, and is asked for again after a switch of language.
fn waiting(ui: &Rc<Ui>, text: impl Fn() -> String + 'static) -> Dialog {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
    let spinner = omui::spinner();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    body.append(&spinner);
    let label = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .max_width_chars(40)
        .build();
    i18n::bind(&label, move |label| label.set_label(&text()));
    body.append(&label);
    let dialog = Dialog::new(Some(""), None);
    dialog.set_heading_translated("One Moment");
    show_only(&dialog, &body);
    dialog.set_can_close(false);
    dialog.present(&ui.window);
    dialog
}

/// Run `omacloud init` with `args` (and secrets in `env`, never on the
/// command line), then show what came of it.
fn run_init(ui: &Rc<Ui>, args: Vec<String>, env: Vec<(String, String)>, wait: impl Fn() -> String + 'static) {
    let mut argv = vec!["omacloud".to_string(), "init".to_string()];
    argv.extend(args);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    let ui2 = ui.clone();
    let busy = waiting(ui, wait);
    // creating an account prints its recovery code: only to a terminal
    tty_then(&argv_ref, env, move |r| match r {
        Err(e) => {
            busy.force_close();
            ui2.toast(&e.shown());
        }
        Ok(out) => {
            busy.force_close();
            // set up: keep it syncing from now on, not from the next login
            run_then(&["systemctl", "--user", "start", "omacloud"], |_| {});
            let code = out
                .lines()
                .find_map(|l| l.strip_prefix("recovery code: "))
                .map(str::to_string);
            let fp = out
                .lines()
                .find_map(|l| l.strip_prefix("this device's fingerprint: "))
                .map(str::to_string);
            if let Some(code) = code {
                show_recovery_code(&ui2, code);
            } else if let Some(fp) = fp {
                show_text(
                    &ui2,
                    || tr("Approve this computer"),
                    || {
                        tr(
                            "On a computer that already syncs, open Omacloud's Devices and approve the one showing \
                             this fingerprint.",
                        )
                    },
                    &fp,
                );
                ui2.refresh();
            } else {
                ui2.toast(&tr("This computer is set up"));
                ui2.refresh();
            }
        }
    });
}

/// A new account's recovery code, shown once: nobody can show it again.
fn show_recovery_code(ui: &Rc<Ui>, code: String) {
    let dialog = Dialog::new(Some(""), Some(""));
    dialog.set_heading_translated("Your recovery code");
    dialog.set_body_translated(
        "Write it down and keep it offline. With it you can add a computer when no other is at hand and recover the \
         account. It is shown only now: without it and without a computer, your files can't be recovered.",
    );
    // selected with the pointer, and no stop for the keyboard: the
    // dialog opens with the cursor where the code is typed back
    let label = gtk::Label::builder()
        .label(&code)
        .selectable(true)
        .can_focus(false)
        .css_classes(["title-2", "monospace"])
        .build();
    // typed back, so nobody clicks past the one chance to keep it
    let (check_row, check) = omui::entry_row("", "");
    check_row.set_title_translated("Type its last four characters");
    let typed_back = group("", "");
    typed_back.add(&check_row.widget);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.append(&label);
    body.append(&typed_back.widget);
    dialog.set_extra_child(Some(body.upcast_ref()));
    dialog.add_response("copy", "");
    dialog.set_response_translated("copy", "Copy");
    dialog.add_response("done", "");
    dialog.set_response_translated("done", "I Wrote It Down");
    dialog.set_response_appearance("done", Appearance::Suggested);
    dialog.set_response_enabled("done", false);
    // Enter after the four characters is the button, once it is on
    dialog.set_default_response("done");
    check.set_activates_default(true);
    // copying leaves it open, the code still to be written down, and
    // Escape does nothing: only the code typed back closes it
    dialog.set_response_closes("copy", false);
    dialog.set_can_close(false);
    // were it put away all the same (its window closing), Done loses nothing more than any other answer would
    dialog.set_close_response("done");
    let tail: String = {
        let chars: Vec<char> = code.chars().filter(char::is_ascii_alphanumeric).collect();
        chars[chars.len().saturating_sub(4)..]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase()
    };
    {
        // weak: the dialog owns the entry
        let dialog = dialog.downgrade();
        check.connect_changed(move |e| {
            let typed: String = e
                .text()
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase();
            if let Some(dialog) = dialog.upgrade() {
                dialog.set_response_enabled("done", typed == tail);
            }
        });
    }
    let (window, ui2) = (ui.window.clone(), ui.clone());
    dialog.connect_response(move |_, id| match id {
        "copy" => window.clipboard().set_text(&code),
        _ => ui2.refresh(),
    });
    dialog.present(&ui.window);
}

/// The storage a new account can be on, with its `init` flags.
const PROVIDERS: [(&str, &str); 4] = [
    ("r2", "--account"),
    ("hetzner", "--location"),
    ("s3", "--endpoint"),
    ("folder", "--path"),
];

/// A folder on a drive or NAS, which takes no bucket and no key.
const FOLDER: usize = 3;

/// What a provider is called here, and the field that says where its bucket is, in the language of the moment.
fn provider_words(i: usize) -> (String, String) {
    match i {
        // names, as they are
        0 => ("Cloudflare R2".to_string(), tr("Cloudflare account ID")),
        1 => ("Hetzner".to_string(), tr("Location (fsn1, nbg1 or hel1)")),
        2 => (tr("Other S3"), tr("Endpoint (https://...)")),
        _ => (tr("A Drive or NAS"), tr("Folder on it (/run/media/...)")),
    }
}

fn provider_at(i: u32) -> usize {
    usize::try_from(i).unwrap_or(0).min(PROVIDERS.len() - 1)
}

fn setting_up_text() -> String {
    tr("Omacloud is setting up this computer, which can take a minute.")
}

/// A page of setup after the first, built on the pages under it.
type SetupPage = fn(&Rc<Ui>, &Nav) -> gtk::Box;

/// First run, one question at a time: what most people need comes first, and
/// the details of buckets and codes only for those who choose them. `at` is
/// the page to open on: a switch of language draws setup again where it was.
fn setup(ui: &Rc<Ui>, at: Option<&str>) -> gtk::Stack {
    let first = gtk::Box::new(gtk::Orientation::Vertical, 12);
    first.set_halign(gtk::Align::Center);
    let welcome = EmptyState::new(
        Some(name::CLOUD),
        &tr("Your files on all your computers"),
        Some(&tr(
            "Omacloud keeps your files and Omarchy settings the same on every computer. They're encrypted here \
                  before they leave, so only your computers can read them.",
        )),
    );
    welcome.set_child(Some(first.upcast_ref()));
    let nav = Nav::new(&step(None, "Omacloud", &welcome.widget, None));
    let pages: [(&str, SetupPage); 3] = [("dropbox", dropbox_step), ("join", join_step), ("bucket", bucket_step)];
    let open = {
        // weak: the pages hold the buttons that open them
        let (ui, weak) = (ui.clone(), nav.downgrade());
        move |tag: &str| {
            let Some(nav) = weak.upgrade() else { return };
            if let Some((_, page)) = pages.iter().find(|(t, _)| *t == tag) {
                let page = page(&ui, &nav);
                nav.push(tag, &page);
                // The keyboard starts in the form (the page's last child), not on Back; setup drawn again for a
                // switch of language is not in the window yet, and puts the keyboard back where it was itself.
                if let Some(form) = page.last_child().filter(|form| form.root().is_some()) {
                    form.child_focus(gtk::DirectionType::TabForward);
                }
            }
        }
    };
    let open = Rc::new(open);
    let choice = |label: &str, class: Option<&str>, tag: &'static str| {
        let b = button(label, class);
        b.add_css_class("pill");
        let open = open.clone();
        b.connect_clicked(move |_| open(tag));
        first.append(&b);
    };
    choice(&tr("Set Up With Dropbox"), Some("suggested-action"), "dropbox");
    choice(&tr("I Already Use Omacloud"), None, "join");
    choice(&tr("Use My Own Storage"), Some("flat"), "bucket");
    if let Some(tag) = at {
        open(tag);
    }
    ui.setup_nav.replace(Some(nav.downgrade()));
    nav.widget
}

/// A page of setup, with its own way back (to the page under it in `nav`), and
/// its action always in view.
fn step(nav: Option<&Nav>, title: &str, content: &impl IsA<gtk::Widget>, action: Option<&gtk::Button>) -> gtk::Box {
    let header = omui::header(Some(&omui::Title::new(title, "").widget.upcast()));
    if let Some(nav) = nav {
        let back = omui::icon_button(name::BACK, &tr("Back"));
        let nav = nav.downgrade();
        back.connect_clicked(move |_| {
            if let Some(nav) = nav.upgrade() {
                nav.pop();
            }
        });
        header.pack_start(&back);
    }
    if let Some(action) = action {
        header.pack_end(action);
    }
    content.set_vexpand(true);
    let view = gtk::Box::new(gtk::Orientation::Vertical, 0);
    view.append(&header);
    view.append(content);
    view
}

/// For someone who lost every computer: out of the way of everyone else.
fn recovery_group() -> (Group, gtk::PasswordEntry) {
    let g = group("", "");
    let more = ExpanderRow::new(&tr("Lost every computer?"), "");
    let (row, code) = omui::password_row(&tr("Recovery code"));
    more.add_row(&row.widget);
    g.add(&more.widget);
    (g, code)
}

/// The recovery code typed in, if any; `None` if it can't be one.
fn recovery_env(ui: &Rc<Ui>, entry: &gtk::PasswordEntry) -> Option<Vec<(String, String)>> {
    let typed = entry.text().to_string();
    match typed.chars().filter(char::is_ascii_alphanumeric).count() {
        0 => Some(Vec::new()),
        28 => Some(vec![("OMACLOUD_RECOVERY_CODE".to_string(), typed)]),
        _ => {
            ui.toast(&tr("That's not a recovery code (28 characters)"));
            None
        }
    }
}

fn dropbox_step(ui: &Rc<Ui>, nav: &Nav) -> gtk::Box {
    let (page, column) = page();
    let (what, sync_args) = choose_what_syncs(ui);
    column.append(&what.widget);
    let (rescue_group, rescue) = recovery_group();
    column.append(&rescue_group.widget);
    let connect = button(&tr("Continue"), Some("suggested-action"));
    let ui = ui.clone();
    connect.connect_clicked(move |_| {
        let Some(env) = recovery_env(&ui, &rescue) else {
            return;
        };
        let mut args = vec!["--provider".to_string(), "dropbox".to_string()];
        args.extend(sync_args());
        run_init(&ui, args, env, || {
            tr("Allow Omacloud in Dropbox in your browser. Setting up can then take a minute.")
        });
    });
    step(Some(nav), &tr("What to Sync"), &page, Some(&connect))
}

fn bucket_step(ui: &Rc<Ui>, nav: &Nav) -> gtk::Box {
    let (page, column) = page();
    let (what, sync_args) = choose_what_syncs(ui);
    let create = group(
        &tr("Your bucket"),
        &tr("Make a private bucket and a key that reaches it at your provider, then give them here."),
    );
    let (rescue_group, rescue) = recovery_group();
    let names: Vec<String> = (0..PROVIDERS.len()).map(|i| provider_words(i).0).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let (provider_row, provider) = omui::combo_row(&tr("Provider"), "", &gtk::StringList::new(&names), 0);
    create.add(&provider_row.widget);
    let (place_row, place) = omui::entry_row(&provider_words(0).1, "");
    {
        let (place_row, place) = (place_row.clone(), place.clone());
        provider.connect_selected_notify(move |p| {
            place_row.set_title(&provider_words(provider_at(p.selected())).1);
            place.set_text("");
        });
    }
    create.add(&place_row.widget);
    let (bucket_row, bucket) = omui::entry_row(&tr("Bucket name"), "");
    create.add(&bucket_row.widget);
    let (key_id_row, key_id) = omui::entry_row(&tr("Access key ID"), "");
    create.add(&key_id_row.widget);
    let (secret_row, secret) = omui::password_row(&tr("Secret access key"));
    create.add(&secret_row.widget);
    {
        // a folder has no bucket and no key
        let rows = [
            bucket_row.widget.clone(),
            key_id_row.widget.clone(),
            secret_row.widget.clone(),
        ];
        provider.connect_selected_notify(move |p| {
            let bucket = provider_at(p.selected()) != FOLDER;
            for row in &rows {
                row.set_visible(bucket);
            }
        });
    }
    let create_button = button(&tr("Create Account"), Some("suggested-action"));
    {
        let ui = ui.clone();
        create_button.connect_clicked(move |_| {
            let (place, bucket, key_id) = (
                place.text().trim().to_string(),
                bucket.text().trim().to_string(),
                key_id.text().trim().to_string(),
            );
            let (name, flag) = PROVIDERS[provider_at(provider.selected())];
            if provider_at(provider.selected()) == FOLDER {
                if place.is_empty() {
                    ui.toast(&tr("Say which folder, on the drive or NAS"));
                    return;
                }
                let mut args: Vec<String> = ["--provider", name, flag, &place].map(str::to_string).to_vec();
                args.extend(sync_args());
                let Some(env) = recovery_env(&ui, &rescue) else {
                    return;
                };
                run_init(&ui, args, env, setting_up_text);
                return;
            }
            if place.is_empty() || bucket.is_empty() || key_id.is_empty() || secret.text().is_empty() {
                ui.toast(&tr("Fill in where your bucket is, and its key"));
                return;
            }
            let mut args: Vec<String> = [
                "--provider",
                name,
                flag,
                &place,
                "--bucket",
                &bucket,
                "--access-key-id",
                &key_id,
            ]
            .map(str::to_string)
            .to_vec();
            args.extend(sync_args());
            let mut env = vec![("OMACLOUD_SECRET_ACCESS_KEY".to_string(), secret.text().to_string())];
            let Some(rescue) = recovery_env(&ui, &rescue) else {
                return;
            };
            env.extend(rescue);
            run_init(&ui, args, env, setting_up_text);
        });
    }
    column.append(&create.widget);
    column.append(&what.widget);
    column.append(&rescue_group.widget);
    step(Some(nav), &tr("Your Own Storage"), &page, Some(&create_button))
}

fn join_step(ui: &Rc<Ui>, nav: &Nav) -> gtk::Box {
    let (page, column) = page();
    let (what, sync_args) = choose_what_syncs(ui);
    let join = group(
        &tr("Your code"),
        &tr(
            "On your other computer, open Omacloud, go to Devices and click Show Join Code, then paste it here. \
             That computer then approves this one. If someone gave you an invite, paste that instead, and you get \
             an account of your own.",
        ),
    );
    let (jcode_row, jcode) = omui::password_row(&tr("Join code"));
    let (rescue_group, code) = recovery_group();
    join.add(&jcode_row.widget);
    let join_button = button(&tr("Add This Computer"), Some("suggested-action"));
    {
        let ui = ui.clone();
        join_button.connect_clicked(move |_| {
            let j = jcode.text().trim().to_string();
            if j.is_empty() {
                ui.toast(&tr("Paste the join code or invite"));
                return;
            }
            let Some(mut env) = recovery_env(&ui, &code) else {
                return;
            };
            env.push(("OMACLOUD_JOIN_CODE".to_string(), j));
            run_init(&ui, sync_args(), env, setting_up_text);
        });
    }
    column.append(&join.widget);
    column.append(&what.widget);
    column.append(&rescue_group.widget);
    step(Some(nav), &tr("Add This Computer"), &page, Some(&join_button))
}

/// A button that opens `path` in the file manager.
fn open_button(window: &gtk::ApplicationWindow, path: String) -> gtk::Button {
    let open = omui::icon_button(name::FOLDER_OPEN, &tr("Open"));
    open.set_valign(gtk::Align::Center);
    let window = window.clone();
    open.connect_clicked(move |_| {
        gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(
            Some(&window),
            None::<&gio::Cancellable>,
            |_| {},
        );
    });
    open
}

/// When this computer last synced, `t` in seconds since the epoch: a while
/// ago in minutes, hours or days as the language counts them, and as a date
/// once it was more than a week ago.
fn synced_when(t: u64) -> String {
    let s = secs_since(t);
    match s {
        0..60 => tr("Synced just now"),
        60..3600 => trn!("Synced {n} minutes ago", s / 60),
        3600..86400 => trn!("Synced {n} hours ago", s / 3600),
        86400..604800 => trn!("Synced {n} days ago", s / 86400),
        _ => glib::DateTime::from_unix_local(i64::try_from(t).unwrap_or(0)).map_or_else(
            |_| tr("Synced long ago"),
            |when| tr!("Synced on {}", i18n::format_date(&when)),
        ),
    }
}

fn overview(ui: &Rc<Ui>, s: &Value) -> gtk::ScrolledWindow {
    let (page, column) = page();

    let sync = group("", "");
    let search = row(
        &tr("Find files by meaning"),
        &tr("Optional Recall · searches folders selected in Recall settings"),
    );
    let launch = button(&tr("Open Recall"), None);
    let ui2 = ui.clone();
    launch.connect_clicked(move |_| {
        let ui = ui2.clone();
        omui::background(
            || Command::new("recall").arg("toggle").status(),
            move |result| {
                if !matches!(result, Ok(Ok(status)) if status.success()) {
                    ui.toast(&tr("Recall is unavailable. Use Open on a sync folder to browse files."));
                }
            },
        );
    });
    search.add_suffix(&launch);
    let folders = &s["folders"];
    let mut folder_groups = Vec::new();
    let mut folder_group_row = None;
    if folders.is_object() {
        folder_groups = what_syncs(ui, s);
    } else {
        let folder = text(&s["folder"]);
        let folder_row = row(&tr("Folder"), &folder);
        folder_row.add_suffix(&open_button(&ui.window, folder));
        folder_group_row = Some(folder_row);
    }
    let daemon = s["daemon"].as_bool().unwrap_or(false);
    let synced_at = s["synced_at"].as_u64();
    let synced = synced_at.map_or_else(|| tr("Not synced yet"), synced_when);
    let repo = &s["repo"];
    let place = match repo["kind"].as_str() {
        Some("bucket") => tr!("{} at {}", text(&repo["bucket"]), text(&repo["endpoint"])),
        Some("dropbox") => tr("your Dropbox"),
        Some("path") => short_path(&text(&repo["repository"])),
        _ => text(&repo["repository"]),
    };
    // a daemon syncs at least every ten minutes: much longer, and something's wrong
    let fresh = synced_at.is_some_and(|t| secs_since(t) < 3600);
    let (state, icon) = if !daemon {
        (tr("Not syncing"), Some(name::WARNING))
    } else if fresh {
        (tr("Up to date"), Some(name::CHECK))
    } else if synced_at.is_some() {
        (tr("Not synced lately"), Some(name::WARNING))
    } else {
        (tr("Syncing…"), None)
    };
    let status = row(
        &state,
        &if daemon {
            tr!("{} · in {}", synced, place.trim())
        } else {
            tr("Nothing syncs until background sync starts")
        },
    );
    if let Some(icon) = icon {
        status.add_prefix(&omui::icon(icon));
    }
    if !daemon {
        let start = button(&tr("Start"), Some("suggested-action"));
        let ui2 = ui.clone();
        start.connect_clicked(move |_| {
            ui2.act(&["systemctl", "--user", "start", "omacloud"], &tr("Started"));
        });
        status.add_suffix(&start);
    }
    sync.add(&status.widget);
    if let Some(r) = folder_group_row {
        sync.add(&r.widget);
    }
    let deleted = row(
        &tr("Recently Deleted"),
        &tr("Files deleted in the last 30 days, to put back"),
    );
    let show = button(&tr("Show…"), None);
    let ui2 = ui.clone();
    show.connect_clicked(move |_| show_deleted(&ui2));
    deleted.add_suffix(&show);
    column.append(&sync.widget);
    let whole_folder = folder_groups.is_empty();
    for g in &folder_groups {
        column.append(&g.widget);
    }
    let files = group(
        &tr("Your files"),
        &tr("Anything named like name.nosync stays on this computer only."),
    );
    // syncing one whole folder, there's no Folders group to open them from
    if whole_folder {
        let online = row(
            &tr("On another device"),
            &tr("Your files in a browser, at omacloud.computer/files"),
        );
        online.add_suffix(&phone_button(ui));
        files.add(&online.widget);
    }
    files.add(&deleted.widget);
    files.add(&search.widget);
    column.append(&files.widget);

    // for support and the curious
    let details = ExpanderRow::new(&tr("Details"), "");
    for (title, value) in [
        (tr("Stored in"), place.trim().to_string()),
        (tr("Account"), text(&s["account_fingerprint"])),
        (tr("Account ID"), text(&s["account"])),
        (
            tr("Version"),
            s["synced_head"]
                .as_i64()
                .map_or_else(|| tr("None yet"), i18n::format_int),
        ),
        (
            tr("Key epoch"),
            s["epoch"].as_i64().map_or_else(|| tr("No key yet"), i18n::format_int),
        ),
    ] {
        details.add_row(&row(&title, &value).widget);
    }
    let more = group("", "");
    more.add(&details.widget);
    column.append(&more.widget);
    page
}

/// Files deleted in the last 30 days, each with a button that writes its
/// last version back where it was.
fn show_deleted(ui: &Rc<Ui>) {
    let (page, column) = page();
    let list = Rc::new(group("", ""));
    column.append(&list.widget);
    let dialog = Dialog::new(Some(""), None);
    dialog.set_heading_translated("Recently Deleted");
    dialog.set_extra_child(Some(page.upcast_ref()));
    // The list's own height; the heading and Close add 126 px, for the 520 the dialog had with libadwaita.
    dialog.set_content_size(560, 394);
    dialog.add_response("close", "");
    dialog.set_response_translated("close", "Close");
    dialog.set_close_response("close");
    // What the list says above its rows changes as the history is read, and is said again after a switch of
    // language: weak, as the list's own widget is what keeps this.
    let note: Rc<RefCell<Box<dyn Fn() -> String>>> =
        Rc::new(RefCell::new(Box::new(|| tr("Looking through your history…"))));
    let say = {
        let (weak, note) = (Rc::downgrade(&list), note.clone());
        move || {
            if let Some(list) = weak.upgrade() {
                list.set_description(&note.borrow()());
            }
        }
    };
    {
        let say = say.clone();
        i18n::on_change(&list.widget, say);
    }
    say();
    dialog.present(&ui.window);
    let ui = ui.clone();
    let weak_dialog = dialog.downgrade();
    cli_then(&["omacloud", "deleted"], Vec::new(), move |r| {
        let files = match r {
            Ok(Value::Array(files)) => files,
            Ok(_) => Vec::new(),
            Err(e) => {
                *note.borrow_mut() = Box::new(move || e.shown());
                say();
                return;
            }
        };
        let words: fn() -> String = if files.is_empty() {
            || tr("Nothing was deleted in the last 30 days.")
        } else {
            || tr("Put Back writes the last version where it was. Older versions stay under `omacloud versions`.")
        };
        *note.borrow_mut() = Box::new(words);
        say();
        let Some(dialog) = weak_dialog.upgrade() else { return };
        for f in files {
            let size = f["size"].as_u64().unwrap_or(0);
            let rw = row(&text(&f["relative"]), "");
            {
                let (rw, device) = (rw.clone(), text(&f["device"]));
                dialog.bind(move |_| rw.set_subtitle(&tr!("Deleted on {} · {}", device, human(size))));
            }
            let put = button("", Some("flat"));
            i18n::bind_label(&put, "Put Back");
            let (ui, group, rw2) = (ui.clone(), list.clone(), rw.clone());
            let (path, version) = (text(&f["path"]), f["version"].to_string());
            // Not made insensitive while it runs: GTK takes the keyboard from a button that is, and from one that
            // is removed, to nowhere. So a press while one runs does nothing, and the keyboard is moved on to the
            // next row before this one goes, where someone at the keyboard goes on.
            let busy = Rc::new(Cell::new(false));
            put.connect_clicked(move |_| {
                if busy.replace(true) {
                    return;
                }
                let (ui, list, rw, busy) = (ui.clone(), group.clone(), rw2.clone(), busy.clone());
                cli_then(
                    &["omacloud", "restore", &path, "--at", &version, "--to", &path],
                    Vec::new(),
                    move |r| match r {
                        Ok(_) => {
                            hand_on_focus(&rw.widget);
                            list.remove(&rw.widget);
                            ui.toast(&tr("Put back"));
                        }
                        Err(e) => {
                            busy.set(false);
                            ui.toast(&e.shown());
                        }
                    },
                );
            });
            rw.add_suffix(&put);
            list.add(&rw.widget);
            // The dialog gave the keyboard out before the rows were here, to the list itself, where nothing shows it.
            if page.is_focus() {
                put.grab_focus();
            }
        }
    });
}

/// Before `row` goes, moves the keyboard from inside it to the button of the row after it (or before it), or else
/// to the buttons of the dialog it is in, so that it is never left on nothing.
fn hand_on_focus(row: &gtk::ListBoxRow) {
    let Some(focus) = row.root().and_then(|root| root.focus()) else {
        return;
    };
    if !focus.is_ancestor(row) && focus != *row.upcast_ref::<gtk::Widget>() {
        return;
    }
    let any = |_: &gtk::Button| true;
    let next = row
        .next_sibling()
        .or_else(|| row.prev_sibling())
        .and_then(|other| button_in_view(&other, &any));
    if let Some(next) = next {
        next.grab_focus();
        return;
    }
    // the last one: the dialog's own buttons, below the list
    let mut at = row.parent();
    while let Some(widget) = at {
        if widget.has_css_class("dialog") {
            if let Some(close) = button_in_view(&widget, &|b| !b.is_ancestor(row)) {
                close.grab_focus();
            }
            return;
        }
        at = widget.parent();
    }
}

fn secs_since(t: u64) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .saturating_sub(t)
}

/// `~/Documents` for a path in home.
fn short_path(path: &str) -> String {
    std::env::var("HOME")
        .ok()
        .and_then(|h| path.strip_prefix(&h).map(|rest| format!("~{rest}")))
        .unwrap_or_else(|| path.to_string())
}

/// "2.4 GB", for a size in bytes, written as the language writes numbers.
fn human(bytes: u64) -> String {
    // units are as they are in every language
    let units = ["", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < units.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        trn!("{n} bytes", bytes)
    } else {
        format!("{} {}", i18n::format_number(size, 1), units[unit])
    }
}

/// Show `path` and, once measured, its size as `row`'s subtitle.
fn subtitle_with_size(ui: &Rc<Ui>, row: &Row, path: &str) {
    let shown = short_path(path);
    let with_size = move |bytes: u64| tr!("{} · {}", shown, human(bytes));
    match ui.sizes.borrow().get(path) {
        Some(bytes) => row.set_subtitle(&with_size(*bytes)),
        None => row.set_subtitle(&short_path(path)),
    }
    if !std::path::Path::new(path).is_dir() {
        return;
    }
    let (row, ui, key) = (row.clone(), ui.clone(), path.to_string());
    run_then(&["du", "-sb", path], move |r| {
        if let Some(bytes) = r
            .ok()
            .and_then(|out| out.split_whitespace().next()?.parse::<u64>().ok())
        {
            ui.sizes.borrow_mut().insert(key, bytes);
            row.set_subtitle(&with_size(bytes));
        }
    });
}

/// An iCloud-style switch row: `on` says what it shows; flipping it calls
/// `flip` with the wanted state, and the page is rebuilt from the result.
fn switch_row(title: &str, on: bool, flip: impl Fn(bool) + 'static) -> Row {
    let (row, switch) = omui::switch_row(title, "", on);
    switch.connect_state_set(move |_, want| {
        flip(want);
        // the state follows once the change is made and the page refreshed
        glib::Propagation::Stop
    });
    row
}

/// Your files in a browser on another device.
fn phone_button(ui: &Rc<Ui>) -> gtk::Button {
    let phone = button(&tr("Open on a Phone…"), Some("flat"));
    phone.set_tooltip_text(Some(&tr(
        "Your files in a browser on another device, at omacloud.computer/files",
    )));
    let ui2 = ui.clone();
    phone.connect_clicked(move |_| show_web_link(&ui2));
    phone
}

/// A web link as the code a phone's camera reads: Omacloud in a browser opens in
/// its browser with nothing typed. The link works for five minutes, and
/// closes with this dialog.
fn show_web_link(ui: &Rc<Ui>) {
    let ui2 = ui.clone();
    // it opens the account's files: printed only to a terminal
    tty_then(&["omacloud", "web-link", "--keep"], Vec::new(), move |r| {
        let link = match r {
            Err(e) => return ui2.toast(&e.shown()),
            Ok(out) => out
                .lines()
                .find(|l| l.contains("#link="))
                .unwrap_or_default()
                .trim()
                .to_string(),
        };
        let Ok(code) = qrcode::QrCode::with_error_correction_level(&link, qrcode::EcLevel::L) else {
            return ui2.toast(&tr("Couldn't draw the link"));
        };
        let picture = gtk::DrawingArea::builder()
            .content_width(300)
            .content_height(300)
            .halign(gtk::Align::Center)
            .build();
        picture.set_draw_func(move |_, cr, width, height| {
            // Black on white whatever the theme, and square-cornered: that's what a camera reads. The only colours
            // here that are not the theme's.
            const MARGIN: usize = 3;
            let side = code.width() + 2 * MARGIN;
            let cell = (f64::from(width.min(height)) / side as f64).floor().max(1.0);
            let start = (f64::from(width) - cell * side as f64) / 2.0;
            let top = (f64::from(height) - cell * side as f64) / 2.0;
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.rectangle(start, top, cell * side as f64, cell * side as f64);
            _ = cr.fill();
            cr.set_source_rgb(0.0, 0.0, 0.0);
            for (i, color) in code.to_colors().into_iter().enumerate() {
                if color == qrcode::Color::Dark {
                    let (x, y) = (i % code.width() + MARGIN, i / code.width() + MARGIN);
                    cr.rectangle(start + cell * x as f64, top + cell * y as f64, cell, cell);
                }
            }
            _ = cr.fill();
        });
        let dialog = Dialog::new(Some(""), Some(""));
        dialog.set_heading_translated("Open on a phone");
        dialog.set_body_translated(
            "Point your phone's camera at this. Your files open in its browser, read straight from your storage. It \
             works for five minutes, or until you close this.",
        );
        show_only(&dialog, &picture);
        dialog.add_response("copy", "");
        dialog.set_response_translated("copy", "Copy Link");
        dialog.add_response("close", "");
        dialog.set_response_translated("close", "Done");
        dialog.set_response_appearance("close", Appearance::Suggested);
        dialog.set_close_response("close");
        let ui3 = ui2.clone();
        dialog.connect_response(move |_, id| {
            if id == "copy" {
                // it stays good for its five minutes, to be pasted elsewhere
                ui3.window.clipboard().set_text(&link);
                ui3.toast(&tr("Link copied: it works for five minutes"));
            } else {
                // Done, or the dialog put away: the link closes with it, as it said
                cli_then(&["omacloud", "web-link", "--close"], Vec::new(), |_| {});
            }
        });
        dialog.present(&ui2.window);
    });
}

/// The folders every Omarchy computer has, in their usual order. They are
/// the folders' names in home, so they are shown as they are.
const OMARCHY_FOLDERS: [&str; 6] = ["Desktop", "Documents", "Pictures", "Music", "Videos", "Downloads"];

/// What syncs on this computer, each with a switch, as iCloud lists what it
/// keeps in step: every folder, Omarchy's usual ones first and a person's
/// own after, all alike; and Omarchy settings.
fn what_syncs(ui: &Rc<Ui>, s: &Value) -> Vec<Group> {
    let empty = Vec::new();
    let folders = &s["folders"];
    let mut rows: Vec<(String, String, &'static str)> = Vec::new();
    for f in folders["synced"].as_array().unwrap_or(&empty) {
        rows.push((text(&f["name"]), text(&f["path"]), "synced"));
    }
    for f in folders["skipped"].as_array().unwrap_or(&empty) {
        rows.push((text(&f["name"]), text(&f["path"]), "skipped"));
    }
    for f in folders["others"].as_array().unwrap_or(&empty) {
        rows.push((text(&f["name"]), text(&f["path"]), "off"));
    }
    let order = |n: &str| OMARCHY_FOLDERS.iter().position(|u| *u == n);
    // the usual ones first, in their order, then a person's own by name
    rows.sort_by_key(|r| (order(&r.0).is_none(), order(&r.0), r.0.clone()));
    let folder_row = |(name, path, state): (String, String, &'static str)| {
        let (ui2, name2) = (ui.clone(), name.clone());
        let r = switch_row(&name, state == "synced", move |want| {
            let name = name2.clone();
            if want {
                let verb = if state == "skipped" { "unskip" } else { "add" };
                ui2.act(&["omacloud", "folders", verb, &name], &tr("Syncing"));
            } else {
                let ui3 = ui2.clone();
                let ui4 = ui2.clone();
                let dialog = Dialog::new(Some(""), Some(""));
                {
                    let name = name.clone();
                    dialog.bind(move |dialog| dialog.set_heading(&tr!("Stop syncing {} here?", name)));
                }
                dialog
                    .set_body_translated("Its files stay on this computer, and your other computers keep syncing it.");
                dialog.add_response("cancel", "");
                dialog.set_response_translated("cancel", "Cancel");
                dialog.add_response("skip", "");
                dialog.set_response_translated("skip", "Stop Syncing");
                dialog.set_response_appearance("skip", Appearance::Destructive);
                dialog.set_close_response("cancel");
                dialog.connect_response(move |_, id| {
                    if id == "skip" {
                        ui3.act(&["omacloud", "folders", "skip", &name], &tr("Stopped syncing here"));
                    } else {
                        ui4.refresh();
                    }
                });
                dialog.present(&ui2.window);
            }
        });
        subtitle_with_size(ui, &r, &path);
        r
    };
    let folders_group = group(
        &tr("Folders"),
        &tr("Each one is the same on all your computers that sync it."),
    );
    folders_group.set_header_suffix(Some(phone_button(ui).upcast_ref()));
    for r in rows {
        folders_group.add(&folder_row(r).widget);
    }
    // every other folder of home, a switch each
    let more = folders["more"].as_array().unwrap_or(&empty);
    if !more.is_empty() {
        let x = ExpanderRow::new(&tr("Add a folder from your home"), "");
        for f in more {
            let (name, ui2) = (text(&f["name"]), ui.clone());
            let name2 = name.clone();
            let r = switch_row(&name, false, move |_| {
                ui2.act(&["omacloud", "folders", "add", &name2], &tr("Syncing"));
            });
            r.set_subtitle(&short_path(&text(&f["path"])));
            x.add_row(&r.widget);
        }
        folders_group.add(&x.widget);
    }

    // Omarchy settings, beside the folders, as iCloud lists more than files
    let settings_group = group(&tr("Settings"), "");
    let settings_on = s["settings"]["on"].as_bool().unwrap_or(false);
    let ui2 = ui.clone();
    let settings = switch_row(&tr("Omarchy settings"), settings_on, move |want| {
        let verb = if want { "on" } else { "off" };
        ui2.act(
            &["omacloud", "settings", verb],
            &if want {
                tr("Omarchy settings sync")
            } else {
                tr("Omarchy settings stay here")
            },
        );
    });
    settings.set_subtitle(&settings_synced());
    settings_group.add(&settings.widget);

    vec![folders_group, settings_group]
}

/// The setup page's choice of what syncs, as iCloud asks on a new Mac:
/// switches for the usual folders and Omarchy settings, with their sizes.
/// Returns the group and what to add to `omacloud init`.
fn choose_what_syncs(ui: &Rc<Ui>) -> (Group, impl Fn() -> Vec<String> + use<>) {
    let g = group(&tr("What to sync"), &tr("You can change these any time."));
    let home = std::env::var("HOME").unwrap_or_default();
    // where this computer keeps each (a German one's Documents is Dokumente)
    // None when this computer points the folder at home itself, as Omarchy
    // does with Desktop: there's nothing to sync
    let local = |name: &str| -> Option<String> {
        let key = match name {
            "Downloads" => "DOWNLOAD".to_string(),
            other => other.to_uppercase(),
        };
        let dirs = std::fs::read_to_string(format!("{home}/.config/user-dirs.dirs")).unwrap_or_default();
        let set = dirs
            .lines()
            .find_map(|l| l.trim().strip_prefix(&format!("XDG_{key}_DIR=")).map(str::to_string))
            .map(|v| v.trim_matches('"').replace("$HOME", &home));
        match set {
            Some(v) if v.trim_end_matches('/') == home => None,
            Some(v) if !v.is_empty() => Some(v),
            _ => Some(format!("{home}/{name}")),
        }
    };
    let mut switches = Vec::new();
    for (name, on) in [
        ("Desktop", true),
        ("Documents", true),
        ("Pictures", true),
        ("Music", false),
        ("Videos", false),
        ("Downloads", false),
    ] {
        let Some(path) = local(name) else {
            continue;
        };
        let (row, switch) = omui::switch_row(name, "", on);
        subtitle_with_size(ui, &row, &path);
        g.add(&row.widget);
        switches.push((name, switch));
    }
    let (settings_row, settings) = omui::switch_row(&tr("Omarchy settings"), &settings_synced(), true);
    g.add(&settings_row.widget);
    let args = move || {
        let chosen: Vec<&str> = switches
            .iter()
            .filter(|(_, r)| r.is_active())
            .map(|(n, _)| *n)
            .collect();
        let mut args = vec!["--folders".to_string(), chosen.join(",")];
        if settings.is_active() {
            args.push("--settings".into());
        }
        args
    };
    (g, args)
}

fn devices(ui: &Rc<Ui>, s: &Value) -> gtk::ScrolledWindow {
    let (page, column) = page();
    let empty = Vec::new();
    let list = |k: &str| s[k].as_array().unwrap_or(&empty).clone();

    let requests = list("requests");
    if !requests.is_empty() {
        let g = group(
            &tr("Asking to join"),
            &tr("Approve a device only if it shows the same fingerprint."),
        );
        for r in requests {
            let (name, fp) = (text(&r["name"]), text(&r["fingerprint"]));
            let rw = row(&name, &fp);
            let approve = button(&tr("Approve…"), Some("suggested-action"));
            let ui2 = ui.clone();
            approve.connect_clicked(move |_| {
                let ui3 = ui2.clone();
                let fp2 = fp.clone();
                let (name, name2, fp) = (name.clone(), name.clone(), fp.clone());
                ui2.confirm(
                    move || tr!("Approve {}?", name),
                    move || {
                        tr!(
                            "Only if {0} shows this fingerprint:\n\n{1}\n\nIt will get the key to everything in this \
                             account.",
                            name2,
                            fp
                        )
                    },
                    &[("approve", || tr("Approve"), Appearance::Suggested)],
                    move |_| ui3.act(&["omacloud", "devices", "approve", &fp2], &tr("Approved")),
                );
            });
            rw.add_suffix(&approve);
            g.add(&rw.widget);
        }
        column.append(&g.widget);
    }

    let me = &s["this_device"];
    let g = group(
        &tr("Devices"),
        &if me["role"] == "waiting" {
            tr("This device is waiting for approval.")
        } else {
            String::new()
        },
    );
    if me["role"] == "member" {
        let add = button(&tr("Show Join Code"), Some("flat"));
        let ui2 = ui.clone();
        add.connect_clicked(move |_| show_join_code(&ui2));
        g.set_header_suffix(Some(add.upcast_ref()));
    }
    let on_dropbox = s["repo"]["kind"] == "dropbox";
    for d in list("devices") {
        let (name, fp) = (text(&d["name"]), text(&d["fingerprint"]));
        let rw = row(&name, &fp);
        if d["this"].as_bool() == Some(true) {
            let tag = gtk::Label::new(Some(&tr("This device")));
            tag.add_css_class("dim-label");
            rw.add_suffix(&tag);
        } else {
            let remove = button(&tr("Remove…"), Some("flat"));
            let ui2 = ui.clone();
            remove.connect_clicked(move |_| {
                let ui3 = ui2.clone();
                let fp2 = fp.clone();
                let name = name.clone();
                ui2.confirm(
                    move || tr!("Remove {}?", name),
                    || {
                        tr(
                            "It stops syncing, and its files stay on it. If it's lost or stolen, or someone else has \
                             it, Omacloud also changes your keys so it can't reach anything new. That uploads \
                             everything again, so it can take a while.",
                        )
                    },
                    &[
                        ("remove", || tr("Remove"), Appearance::Default),
                        ("lost", || tr("It's Lost or Stolen"), Appearance::Destructive),
                    ],
                    move |id| {
                        if id == "lost" {
                            cut_off(&ui3, &fp2, on_dropbox);
                        } else {
                            ui3.act(
                                &["omacloud", "devices", "revoke", fp2.as_str(), "--yes"],
                                &tr("Removed"),
                            );
                        }
                    },
                );
            });
            rw.add_suffix(&remove);
        }
        g.add(&rw.widget);
    }
    column.append(&g.widget);

    column.append(&each_computer(ui, &s["choices"]).widget);

    let removed = list("removed");
    if !removed.is_empty() {
        let g = group(&tr("Removed"), "");
        for d in removed {
            g.add(&row(&text(&d["name"]), &text(&d["fingerprint"])).widget);
        }
        column.append(&g.widget);
    }
    page
}

/// What a computer's choice for something is called.
fn choice_label(word: &str) -> String {
    match word {
        "sync" => tr("Synced"),
        "backup" => tr("Backed up only"),
        "off" => tr("Off"),
        _ => tr("Not known"),
    }
}

/// What each computer keeps, one expandable row per computer: its folders,
/// settings and settings files, each marked synced, backed up only or off.
/// This computer's settings files are chosen here; folders and settings by
/// their switches.
fn each_computer(ui: &Rc<Ui>, choices: &Value) -> Group {
    const WORDS: [&str; 3] = ["sync", "backup", "off"];
    let marker = |w: &str| match w {
        "sync" => name::REFRESH,
        "backup" => name::DRIVE,
        "off" => name::UNAVAILABLE,
        _ => name::QUESTION,
    };
    let g = group(
        &tr("What each computer keeps"),
        &tr("Synced: the same on every computer. Backed up only: kept from that computer, not synced. Off: neither."),
    );
    let me = text(&choices["this_device"]);
    let empty = Vec::new();
    let items = choices["items"].as_array().unwrap_or(&empty);
    for device in choices["devices"].as_array().unwrap_or(&empty) {
        let device = text(device);
        let here = device == me;
        let x = ExpanderRow::new(&device, &if here { tr("This computer") } else { String::new() });
        x.set_expanded(here);
        x.add_prefix(&omui::icon(name::COMPUTER));
        let all_settings = items
            .iter()
            .find(|i| i["name"] == "settings")
            .and_then(|i| i["choices"][&device].as_str())
            .unwrap_or("-");
        let mut heading = "";
        for item in items {
            let word = item["choices"][&device].as_str().unwrap_or("-");
            let name = text(&item["name"]);
            let section = if item["kind"] == "folders" {
                "folders"
            } else {
                "settings"
            };
            // with settings off, its groups and files say nothing more
            if section == "settings" && all_settings != "sync" && name != "settings" {
                continue;
            }
            if section != heading {
                heading = section;
                let h = gtk::Label::builder()
                    .label(if section == "folders" {
                        tr("Folders")
                    } else {
                        tr("Settings")
                    })
                    .xalign(0.0)
                    .margin_top(12)
                    .margin_bottom(6)
                    .margin_start(12)
                    .build();
                h.add_css_class("heading");
                x.add_row(&h);
            }
            let kind = match item["kind"].as_str().unwrap_or_default() {
                "folders" => String::new(),
                "settings" if name == "settings" => tr("All of them"),
                "settings" => tr("A group"),
                _ => tr("A file"),
            };
            let title = if name == "settings" {
                tr("Omarchy settings")
            } else {
                name.clone()
            };
            let r = Row::new(&title, &kind);
            let icon = omui::icon(marker(word));
            icon.set_tooltip_text(Some(&choice_label(word)));
            if word != "sync" {
                icon.add_css_class("dim-label");
            }
            r.add_prefix(&icon);
            if here && item["kind"] == "files" {
                let labels: Vec<String> = WORDS.iter().map(|w| choice_label(w)).collect();
                let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
                let pick = gtk::DropDown::from_strings(&labels);
                pick.set_valign(gtk::Align::Center);
                if let Some(i) = WORDS.iter().position(|w| *w == word) {
                    pick.set_selected(i as u32);
                }
                let ui = ui.clone();
                pick.connect_selected_notify(move |p| {
                    if let Some(w) = WORDS.get(p.selected() as usize) {
                        ui.act(&["omacloud", "choose", &name, w], &tr("Changed here"));
                    }
                });
                r.add_suffix(&pick);
            } else {
                let l = gtk::Label::new(Some(&choice_label(word)));
                l.add_css_class("dim-label");
                r.add_suffix(&l);
            }
            x.add_row(&r.widget);
        }
        if here && all_settings == "sync" {
            // a settings file not listed yet: backed up only, to start
            let add = row(
                &tr("Back up a settings file only"),
                &tr("like ~/.claude/CLAUDE.md, not synced"),
            );
            let b = button(&tr("Choose…"), Some("flat"));
            let ui = ui.clone();
            b.connect_clicked(move |_| back_up_only(&ui));
            add.add_suffix(&b);
            x.add_row(&add.widget);
        }
        g.add(&x.widget);
    }
    g
}

/// Asks the desktop's file chooser for a settings file to back up only. A
/// chooser that can't be shown says why, in the window.
fn back_up_only(ui: &Rc<Ui>) {
    let options = omui::PickOptions {
        folder: std::env::var_os("HOME").map(Into::into),
        accept_label: Some(tr("Back Up")),
        ..omui::PickOptions::new(&tr("Back up only"))
    };
    let ui2 = ui.clone();
    ui.picker.open(
        options,
        Box::new(move |picked| match picked {
            Ok(files) => {
                if let Some(path) = files.first() {
                    ui2.act(
                        &["omacloud", "choose", &path.to_string_lossy(), "backup"],
                        &tr("Backed up only"),
                    );
                }
            }
            Err(omui::PickError::Cancelled) => {}
            Err(failed) => ui2.toast(&failed.to_string()),
        }),
    );
}

/// A computer lost or stolen: removed, and cut off as far as the storage allows.
fn cut_off(ui: &Rc<Ui>, fingerprint: &str, on_dropbox: bool) {
    let wait = move || {
        if on_dropbox {
            tr(
                "Changing your keys, which uploads everything again. Dropbox then asks you in your browser to allow \
                Omacloud once more.",
            )
        } else {
            tr("Changing your keys, which uploads everything again.")
        }
    };
    let done = if on_dropbox {
        tr("Removed. It's cut off once your other computers have switched")
    } else {
        tr("Removed. Now make a new key at your provider, and run omacloud bucket set-key")
    };
    let busy = waiting(ui, wait);
    let ui = ui.clone();
    cli_then(
        &["omacloud", "devices", "revoke", fingerprint, "--yes", "--lost"],
        Vec::new(),
        move |removed| {
            busy.force_close();
            ui.toast(&removed.map_or_else(|e| e.shown(), |_| done));
            ui.refresh();
        },
    );
}

/// A join code for adding another computer to this account.
fn show_join_code(ui: &Rc<Ui>) {
    let ui2 = ui.clone();
    // it lets a computer into the account: printed only to a terminal
    tty_then(&["omacloud", "join-code"], Vec::new(), move |r| match r {
        Err(e) => ui2.toast(&e.shown()),
        Ok(out) => show_text(
            &ui2,
            || tr("Join code"),
            || {
                tr(
                    "On the new computer, install Omacloud, open it, and paste this under Add this computer to your \
                     account. Then approve it here. Pass the code only to your own computers, and not through chat \
                     or email.",
                )
            },
            out.lines()
                .find(|l| l.starts_with("omacloud-join-"))
                .unwrap_or_default()
                .trim(),
        ),
    });
}

/// What the settings switch sends: passwords and keys are named, so nobody
/// turns it on thinking it's only themes.
fn settings_synced() -> String {
    tr("Shell, desktop and apps, Chromium, AI agents with any API keys in their settings, and Wi-Fi passwords")
}

fn settings(ui: &Rc<Ui>, s: &Value) -> gtk::ScrolledWindow {
    let (page, column) = page();
    let st = &s["settings"];
    let on = st["on"].as_bool().unwrap_or(false);
    let g = group(
        &tr("Omarchy settings"),
        &tr(
            "The shared files in Omarchy's dots manifest (bindings, look and feel, terminals, shell), AI agents' \
             settings and skills, Chromium's bookmarks and settings, and saved Wi-Fi networks. Ones about the \
             computer, like monitors, are backed up only; sign-in files are off. API keys written into settings \
             files sync with them, encrypted.",
        ),
    );
    let (switch_row, switch) = omui::switch_row(&tr("Sync settings on this device"), "", on);
    {
        let ui = ui.clone();
        switch.connect_active_notify(move |sw| {
            if sw.is_active() {
                ui.act(&["omacloud", "settings", "on"], &tr("Settings sync is on"));
            } else {
                ui.act(&["omacloud", "settings", "off"], &tr("Settings sync is off"));
            }
        });
    }
    g.add(&switch_row.widget);
    // why it stands down is the CLI's, in its English
    if let Some(why) = st["dormant"].as_str().filter(|_| on) {
        g.add(&row(&tr("Standing down"), why).widget);
    }
    column.append(&g.widget);

    let empty_groups = Vec::new();
    let groups = st["groups"].as_array().unwrap_or(&empty_groups);
    if on && !groups.is_empty() {
        let g = group(
            &tr("What syncs"),
            &tr(
                "Edits on two computers merge. Chromium's are written in while it's closed; history, passwords, \
                 cookies and open tabs stay on each computer.",
            ),
        );
        for item in groups {
            g.add(&setting_group(ui, item));
        }
        column.append(&g.widget);
    }

    let empty = Vec::new();
    let held = st["held"].as_array().unwrap_or(&empty);
    if !held.is_empty() {
        let home = std::env::var("HOME").unwrap_or_default();
        let g = group(
            &tr("Changed on two computers"),
            &tr(
                "These changed here and on another computer in ways that don't merge. This computer keeps its \
                 version until you choose. Keep Each backs up this computer's version only, and the others' stays \
                 synced.",
            ),
        );
        for p in held {
            let rel = text(p);
            let rw = row(&format!("~/{rel}"), "");
            let path = format!("{home}/{rel}");
            for (label, keep) in [
                (tr("Keep Mine"), "local"),
                (tr("Keep Theirs"), "remote"),
                (tr("Keep Each"), "each"),
            ] {
                let b = button(&label, Some("flat"));
                let (ui, path) = (ui.clone(), path.clone());
                b.connect_clicked(move |_| {
                    ui.act(
                        &["omacloud", "settings", "resolve", &path, "--keep", keep],
                        &tr("Resolved"),
                    );
                });
                rw.add_suffix(&b);
            }
            g.add(&rw.widget);
        }
        column.append(&g.widget);
    }
    page
}

/// What a settings group's state reads as; the same as `omacloud settings`
/// says. Only Chromium waits for an app to close. A state this doesn't know
/// is shown as the CLI says it.
fn state_label(group: &str, state: &str) -> String {
    match (group, state) {
        (_, "synced") => tr("In sync"),
        (_, "new") => tr("Not synced yet"),
        ("chromium", "waiting") => tr("Waiting for Chromium to close"),
        (_, "waiting") => tr("Waiting to apply"),
        (_, "install") => tr("Extensions to install"),
        (_, "settled") => tr("Edits settled"),
        (_, "refused") => tr("Not applied"),
        (_, "failed") => tr("Can't write"),
        (_, "conflict") => tr("Choose below"),
        (_, "unavailable") => tr("Not available"),
        (_, other) => other.to_string(),
    }
}

/// One settings group: its state, extensions to install, and what stays
/// on this computer. The same as `omacloud settings` lists; its title and
/// detail are the CLI's, in English.
fn setting_group(ui: &Rc<Ui>, item: &Value) -> gtk::ListBoxRow {
    let state = state_label(
        item["group"].as_str().unwrap_or_default(),
        item["state"].as_str().unwrap_or_default(),
    );
    let empty = Vec::new();
    let install = item["install"].as_array().unwrap_or(&empty);
    let not_synced = item["not_synced"].as_array().unwrap_or(&empty);
    let tag = gtk::Label::new(Some(&state));
    tag.add_css_class("dim-label");
    if install.is_empty() && not_synced.is_empty() {
        let r = row(&text(&item["title"]), &text(&item["detail"]));
        r.add_suffix(&tag);
        return r.widget;
    }
    let x = ExpanderRow::new(&text(&item["title"]), &text(&item["detail"]));
    x.add_suffix(&tag);
    for i in install {
        let r = row(&text(&i["name"]), &text(&i["url"]));
        let b = button(&tr("Install"), Some("flat"));
        let argv: Vec<String> = i["command"].as_array().unwrap_or(&empty).iter().map(text).collect();
        let ui = ui.clone();
        b.connect_clicked(move |_| {
            // Chromium opens the extension's page in that profile, where
            // Add to Chromium installs it; the browser stays open after
            let Some((program, args)) = argv.split_first() else {
                return;
            };
            let started = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match started {
                // reaped when it exits, however long the browser stays open
                Ok(mut child) => _ = std::thread::spawn(move || child.wait()),
                Err(e) => ui.toast(&Failure::local("app-run", format!("{program}: {e}")).shown()),
            }
        });
        r.add_suffix(&b);
        x.add_row(&r.widget);
    }
    for n in not_synced {
        x.add_row(&row(&tr("Not synced"), &text(n)).widget);
    }
    x.widget
}

fn secrets(ui: &Rc<Ui>, s: &Value) -> gtk::ScrolledWindow {
    let (page, column) = page();
    if s["settings"]["on"].as_bool() != Some(true) {
        let g = group(
            &tr("Secrets"),
            &tr("ssh and gpg keys and tokens travel with settings sync. Turn it on under Omarchy."),
        );
        column.append(&g.widget);
        return page;
    }
    let g = group(
        &tr("This device"),
        &tr(
            "Seal ~/.ssh, ~/.gnupg and token files (or what ~/.config/omacloud/secrets lists) so they travel with \
             your files. No device can open them: only your recovery code.",
        ),
    );
    let rw = row(&tr("Save this device's secrets"), &tr("Replaces what was saved before"));
    let save = button(&tr("Save…"), Some("suggested-action"));
    {
        let ui = ui.clone();
        save.connect_clicked(move |_| {
            let ui2 = ui.clone();
            ui.confirm(
                || tr("Save secrets?"),
                || tr("They are sealed so that only your recovery code opens them."),
                &[("save", || tr("Save"), Appearance::Suggested)],
                move |_| ui2.act(&["omacloud", "secrets", "save"], &tr("Secrets saved")),
            );
        });
    }
    rw.add_suffix(&save);
    g.add(&rw.widget);
    column.append(&g.widget);

    let empty = Vec::new();
    let saved = s["secrets"].as_array().unwrap_or(&empty);
    let g = group(
        &tr("Saved"),
        &if saved.is_empty() {
            tr("No device has saved its secrets yet.")
        } else {
            String::new()
        },
    );
    for d in saved {
        let device = text(d);
        let rw = row(&device, "");
        let restore = button(&tr("Restore…"), Some("flat"));
        let ui = ui.clone();
        restore.connect_clicked(move |_| restore_secrets(&ui, &device));
        rw.add_suffix(&restore);
        g.add(&rw.widget);
    }
    column.append(&g.widget);
    page
}

/// A recovery code entry in a boxed list, for a dialog's extra child: its
/// title follows a switch of language while the dialog is open.
fn code_entry() -> (Group, gtk::PasswordEntry) {
    let (row, entry) = omui::password_row("");
    row.set_title_translated("Recovery code");
    let list = group("", "");
    list.add(&row.widget);
    (list, entry)
}

/// What `secrets restore` would do to a file, as a word.
fn change_word(action: &str) -> String {
    match action {
        "replace" => tr("Replace"),
        "write" => tr("Write"),
        other => other.to_string(),
    }
}

fn restore_secrets(ui: &Rc<Ui>, device: &str) {
    let (list, entry) = code_entry();
    let dialog = Dialog::new(Some(""), Some(""));
    {
        let device = device.to_string();
        dialog.bind(move |dialog| dialog.set_heading(&tr!("Restore {}'s secrets?", device)));
    }
    dialog.set_body_translated("Enter your recovery code.");
    dialog.set_extra_child(Some(list.widget.upcast_ref()));
    dialog.add_response("cancel", "");
    dialog.set_response_translated("cancel", "Cancel");
    dialog.add_response("next", "");
    dialog.set_response_translated("next", "Continue");
    dialog.set_response_appearance("next", Appearance::Suggested);
    dialog.set_close_response("cancel");
    // Enter after the code goes on, as Continue does
    dialog.set_default_response("next");
    entry.set_activates_default(true);
    let (ui2, device) = (ui.clone(), device.to_string());
    dialog.connect_response(move |_, id| {
        if id != "next" {
            return;
        }
        let code = entry.text().to_string();
        let env = vec![("OMACLOUD_RECOVERY_CODE".to_string(), code.clone())];
        let (ui3, device2) = (ui2.clone(), device.clone());
        cli_then(
            &["omacloud", "secrets", "restore", "--from", &device],
            env,
            move |r| match r {
                Err(e) => ui3.toast(&e.shown()),
                Ok(data) => {
                    let empty = Vec::new();
                    let changes: Vec<(String, String)> = data["changes"]
                        .as_array()
                        .unwrap_or(&empty)
                        .iter()
                        .map(|c| (text(&c["action"]), text(&c["path"])))
                        .collect();
                    if changes.is_empty() {
                        ui3.toast(&tr("This computer already has those keys"));
                        return;
                    }
                    let ui4 = ui3.clone();
                    ui3.confirm(
                        || tr("Write these files?"),
                        // each change's word too is in the language of the moment
                        move || {
                            let lines: Vec<String> = changes
                                .iter()
                                .map(|(action, path)| format!("{:<8} {}", change_word(action), path))
                                .collect();
                            tr!("{}\n\nFiles this replaces are backed up first.", lines.join("\n"))
                        },
                        &[("write", || tr("Write"), Appearance::Destructive)],
                        move |_| {
                            let env = vec![("OMACLOUD_RECOVERY_CODE".to_string(), code.clone())];
                            let ui5 = ui4.clone();
                            cli_then(
                                &["omacloud", "secrets", "restore", "--from", &device2, "--yes"],
                                env,
                                move |r| {
                                    ui5.toast(&r.map_or_else(
                                        |e| e.shown(),
                                        |data| trn!("Wrote {n} files", data["written"].as_u64().unwrap_or(0)),
                                    ));
                                },
                            );
                        },
                    );
                }
            },
        );
    });
    dialog.present(&ui.window);
}

fn recovery() -> gtk::ScrolledWindow {
    let (page, column) = page();
    column.append(
        &group(
            &tr("If you lose everything"),
            &tr(
                "If you lose your recovery code and every computer, your files can't be recovered. Keep a copy of \
                 the recovery code with someone you trust, or split it among several people with `omacloud \
                 recovery split`.",
            ),
        )
        .widget,
    );
    page
}

/// Whether `bin` is a program on PATH.
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// Move from iCloud: iCloud for Omarchy's Notes, Reminders and iCloud Drive
/// handed to Omacloud (`icloud-session leave`). Optional and for good, so
/// nothing is chosen at the start, and leaving waits for the word `leave`.
fn move_from_icloud(ui: &Rc<Ui>) {
    let ui = ui.clone();
    cli_then(
        &["icloud-session", "leave", "--dry-run"],
        Vec::new(),
        move |r| match r {
            Ok(data) => leave_dialog(&ui, &data),
            Err(e) => ui.toast(&e.shown()),
        },
    );
}

fn leave_dialog(ui: &Rc<Ui>, data: &Value) {
    let home = std::env::var("HOME").unwrap_or_default();
    let (page, column) = page();
    // Everything it says follows a switch of language while it is open.
    let intro = group("", "");
    intro.set_description_translated(
        "Optional. Each app you choose syncs with iCloud a last time; then Omacloud syncs its folder and the app \
         stops using iCloud on this computer, for good. iCloud keeps its copy as it is: nothing there is deleted. \
         Do this on each of your computers.",
    );
    column.append(&intro.widget);
    let apps = group("", "");
    apps.set_title_translated("Apps");
    let dialog = Dialog::new(Some(""), None);
    dialog.set_heading_translated("Move from iCloud");
    dialog.set_extra_child(Some(page.upcast_ref()));
    // As tall as what it says, which is all known now; a lower window scrolls it.
    page.set_propagate_natural_height(true);
    dialog.set_content_size(520, -1);
    dialog.add_response("cancel", "");
    dialog.set_response_translated("cancel", "Cancel");
    dialog.add_response("leave", "");
    dialog.set_response_translated("leave", "Leave iCloud");
    dialog.set_response_appearance("leave", Appearance::Destructive);
    dialog.set_response_enabled("leave", false);
    dialog.set_close_response("cancel");
    let chosen: Rc<RefCell<Vec<String>>> = Rc::default();
    // the word typed is `leave` in every language, as the label says
    let (confirm_row, confirm) = omui::entry_row("", "");
    confirm_row.set_title_translated("Type leave to confirm");
    let update = {
        // weak: the dialog owns what calls this
        let (chosen, dialog, confirm) = (chosen.clone(), dialog.downgrade(), confirm.clone());
        Rc::new(move || {
            if let Some(dialog) = dialog.upgrade() {
                dialog.set_response_enabled("leave", !chosen.borrow().is_empty() && confirm.text().trim() == "leave");
            }
        })
    };
    let empty = Vec::new();
    for app in data["apps"].as_array().unwrap_or(&empty) {
        let (word, name, state) = (text(&app["app"]), text(&app["name"]), text(&app["state"]));
        let folder = text(&app["folder"]);
        let shown = folder
            .strip_prefix(&home)
            .map_or_else(|| folder.clone(), |rest| format!("~{rest}"));
        let ready = state == "ready";
        if state == "not_installed" {
            continue;
        }
        let (message, word2) = (text(&app["message"]), word.clone());
        let subtitle = move || match state.as_str() {
            "ready" if word2 == "reminders" => tr!(
                "{} goes to Omacloud. Alerts, priorities, flags, list colours, repeats, sections and shared lists \
                 stay in iCloud only",
                shown
            ),
            "ready" if word2 == "photos" => tr!(
                "{} goes to Omacloud. Every original is downloaded, then uploaded to your storage; albums become \
                 lists. An iPhone keeps sending new photos to iCloud",
                shown
            ),
            "ready" => tr!("{} goes to Omacloud", shown),
            "left" => tr("Left iCloud: Omacloud syncs it"),
            "not_used" => tr("Not used on this computer"),
            // what iCloud for Omarchy says, in its English
            _ => message.clone(),
        };
        let r = row(&name, "");
        {
            let r = r.clone();
            dialog.bind(move |_| r.set_subtitle(&subtitle()));
        }
        if ready {
            let check = gtk::CheckButton::builder().valign(gtk::Align::Center).build();
            let (chosen, update) = (chosen.clone(), update.clone());
            check.connect_toggled(move |c| {
                let mut list = chosen.borrow_mut();
                list.retain(|w| w != &word);
                if c.is_active() {
                    list.push(word.clone());
                }
                drop(list);
                update();
            });
            r.add_prefix(&check);
            r.set_activatable_widget(&check);
        } else if text(&app["code"]) == "omacloud_folder_off" {
            // its folder of home isn't synced here: turning it on is
            // Omacloud's own choice, made here in so many words
            let top = folder
                .strip_prefix(&format!("{home}/"))
                .and_then(|rest| rest.split('/').next())
                .unwrap_or_default()
                .to_string();
            let b = button("", Some("flat"));
            {
                let top = top.clone();
                i18n::bind(&b, move |b| b.set_label(&tr!("Sync ~/{}", top)));
            }
            let (ui2, dialog2) = (ui.clone(), dialog.downgrade());
            b.connect_clicked(move |_| {
                let (ui3, dialog3) = (ui2.clone(), dialog2.clone());
                let path = format!("{}/{top}", std::env::var("HOME").unwrap_or_default());
                cli_then(&["omacloud", "folders", "add", &path], Vec::new(), move |r| match r {
                    Ok(_) => {
                        if let Some(dialog) = dialog3.upgrade() {
                            dialog.force_close();
                        }
                        ui3.refresh();
                        move_from_icloud(&ui3);
                    }
                    Err(e) => ui3.toast(&e.shown()),
                });
            });
            r.add_suffix(&b);
            r.widget.set_sensitive(true);
        } else {
            r.widget.set_sensitive(false);
        }
        apps.add(&r.widget);
    }
    column.append(&apps.widget);
    let last = group("", "");
    {
        let update = update.clone();
        confirm.connect_changed(move |_| update());
    }
    last.add(&confirm_row.widget);
    column.append(&last.widget);

    {
        let ui = ui.clone();
        dialog.connect_response(move |_, id| {
            if id != "leave" {
                return;
            }
            let mut argv = vec!["icloud-session".to_string(), "leave".to_string()];
            argv.extend(chosen.borrow().iter().cloned());
            argv.push("--yes".to_string());
            let busy = waiting(&ui, || {
                tr("Syncing with iCloud a last time, then handing the folders to Omacloud.")
            });
            let ui = ui.clone();
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            cli_then(&argv, Vec::new(), move |r| {
                busy.force_close();
                ui.toast(&match r {
                    Ok(d) => leave_outcome(&d),
                    Err(e) => e.shown(),
                });
                ui.refresh();
            });
        });
    }
    dialog.present(&ui.window);
}

/// What `icloud-session leave` did, in a sentence. The apps are Apple's
/// names, as they are; why one didn't leave is iCloud for Omarchy's English.
fn leave_outcome(d: &Value) -> String {
    let empty = Vec::new();
    let names = |app: &str| {
        match app {
            "notes" => "Notes",
            "reminders" => "Reminders",
            "drive" => "iCloud Drive",
            "photos" => "Photos",
            other => other,
        }
        .to_string()
    };
    let left: Vec<String> = d["left"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|l| names(&text(&l["app"])))
        .collect();
    let failed = d["failed"]
        .is_object()
        .then(|| (names(&text(&d["failed"]["app"])), text(&d["failed"]["message"])));
    let apps = left.join(", ");
    match (left.len(), failed) {
        (0, None) => tr("Nothing left iCloud"),
        (0, Some((app, why))) => tr!("Nothing left iCloud. {} didn't: {}", app, why),
        (n, None) => trn!("{} left iCloud; Omacloud syncs them now", n, apps),
        (n, Some((app, why))) => trn!(
            "{0} left iCloud; Omacloud syncs them now. {1} didn't: {2}",
            n,
            apps,
            app,
            why
        ),
    }
}

/// `content` in a dialog that copies it. The heading and the body are asked
/// for again after a switch of language.
fn show_text(ui: &Rc<Ui>, heading: impl Fn() -> String + 'static, body: impl Fn() -> String + 'static, content: &str) {
    // selected with the pointer, and no stop for the keyboard: Copy is
    let text = gtk::Label::builder()
        .label(content)
        .selectable(true)
        .can_focus(false)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::Char)
        .max_width_chars(40)
        .xalign(0.0)
        .css_classes(["monospace"])
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&text));
    let dialog = Dialog::new(Some(""), Some(""));
    dialog.bind(move |dialog| {
        dialog.set_heading(&heading());
        dialog.set_body(&body());
    });
    show_only(&dialog, &frame);
    dialog.add_response("close", "");
    dialog.set_response_translated("close", "Close");
    dialog.add_response("copy", "");
    dialog.set_response_translated("copy", "Copy");
    dialog.set_response_appearance("copy", Appearance::Suggested);
    dialog.set_close_response("close");
    let (window, content) = (ui.window.clone(), content.to_string());
    dialog.connect_response(move |_, id| {
        if id == "copy" {
            window.clipboard().set_text(&content);
        }
    });
    dialog.present(&ui.window);
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    /// The English catalog, which holds the counted strings' forms, as `main` registers it.
    fn english() {
        omui::i18n::register_all(&[("en", include_str!("i18n/en.json"))]);
    }

    /// The catalogs list every string the window shows, and only those.
    #[test]
    fn the_catalogs_match_the_sources() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        omui::i18n::CatalogCheck::new(root.join("src"), root.join("src/i18n")).assert_ok();
    }

    /// The CLI's failure is the error object on the last line of stderr,
    /// after any warning, and stdout is empty; its code reaches the pages.
    #[test]
    fn a_failure_is_read_from_stderr() {
        let stderr = b"[2026-11-21T10:00:00Z WARN omacloud] the bucket was slow\n\
            {\"ok\":false,\"error\":{\"code\":\"not_set_up\",\"kind\":\"setup\",\"message\":\"not set up\",\"hint\":\"run init\"}}\n";
        let failure = super::envelope(b"", stderr, "omacloud").unwrap_err();
        assert_eq!(
            (
                failure.code.as_str(),
                failure.kind.as_str(),
                failure.message.as_str(),
                failure.hint.as_str()
            ),
            ("not_set_up", "setup", "not set up", "run init")
        );
    }

    /// A failure whose code has a fixed meaning is said in the window's own
    /// words (and so in its language); any other is the CLI's English, with
    /// its hint. Another program's codes are its own business.
    #[test]
    fn known_failures_are_said_in_the_windows_words() {
        let failure = |code: &str| super::Failure {
            code: code.into(),
            kind: "setup".into(),
            omacloud: true,
            ..super::Failure::said("raw message", "raw hint")
        };
        assert_eq!(
            failure("recovery_code_rejected").shown(),
            "That recovery code doesn't open this account. Check it and try again."
        );
        assert_eq!(failure("something_new").shown(), "raw message. Raw hint");
        let elsewhere = super::Failure {
            omacloud: false,
            ..failure("not_set_up")
        };
        assert_eq!(elsewhere.shown(), "raw message. Raw hint");
        assert_eq!(
            super::Failure::local("app-silent", "du").shown(),
            "du failed without saying why."
        );
    }

    /// A result on stdout stands, even beside a failure on stderr (a command
    /// that did its work and fell short of one thing, exit 1).
    #[test]
    fn a_result_is_read_from_stdout() {
        let stdout = b"{\n  \"ok\": true,\n  \"data\": {\"from\": \"pier\"},\n  \"summary\": \"set up\"\n}\n";
        let stderr =
            b"{\"ok\":false,\"error\":{\"code\":\"unavailable\",\"kind\":\"unavailable\",\"message\":\"no unit\"}}\n";
        assert_eq!(
            super::envelope(stdout, stderr, "omacloud").ok().unwrap()["from"],
            "pier"
        );
        assert_eq!(super::envelope(stdout, b"", "omacloud").ok().unwrap()["from"], "pier");
    }

    /// An older CLI's error on stdout, and a CLI that crashed with no
    /// envelope at all, still come back as a failure that says something.
    #[test]
    fn an_older_or_crashed_cli_still_fails_readably() {
        let old = b"{\"ok\": false, \"error\": {\"code\": \"usage\", \"message\": \"bad\"}}";
        assert_eq!(super::envelope(old, b"", "omacloud").unwrap_err().code, "usage");
        let crashed = super::envelope(b"", b"thread 'main' panicked\n", "omacloud").unwrap_err();
        assert_eq!(
            (crashed.code.as_str(), crashed.message.as_str()),
            ("", "thread 'main' panicked")
        );
        let silent = super::envelope(b"", b"", "omacloud").unwrap_err();
        assert_eq!(silent.shown(), "omacloud failed without saying why.");
    }

    /// Sizes and counts are counted strings and the language's numbers.
    #[test]
    fn sizes_and_times_are_counted_and_written_as_the_language_writes_them() {
        english();
        assert_eq!(super::human(1), "1 byte");
        assert_eq!(super::human(999), "999 bytes");
        assert_eq!(super::human(482_133), "482.1 KB");
        assert_eq!(super::human(3_204_551_000), "3.2 GB");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(super::synced_when(now), "Synced just now");
        assert_eq!(super::synced_when(now - 60), "Synced 1 minute ago");
        assert_eq!(super::synced_when(now - 7200), "Synced 2 hours ago");
        assert_eq!(super::synced_when(now - 86400), "Synced 1 day ago");
        assert!(super::synced_when(now - 30 * 86400).starts_with("Synced on "));
    }

    /// `cli_tty` against a stand-in CLI that starts a child holding none of
    /// its stdio (a process it leaves running), checks it has a
    /// terminal, and asks a question nobody answers. It must come back as
    /// soon as the CLI is done, not when that child is.
    #[test]
    fn cli_tty_returns_with_the_cli_not_with_what_it_started() {
        let dir = std::env::temp_dir().join(format!("omacloud-app-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("omacloud");
        std::fs::write(
            &bin,
            "#!/bin/sh\n\
             sleep 5 </dev/null >/dev/null 2>&1 &\n\
             [ -t 0 ] && [ -t 1 ] && echo 'on a terminal'\n\
             read answer\n\
             echo \"answer: [$answer]\"\n\
             case \"$1\" in fail) echo 'error: it broke' >&2; echo 'hint: do this' >&2; exit 3;; esac\n\
             echo the-secret\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: the only test here that reads the environment or spawns
        unsafe {
            std::env::set_var("OMACLOUD_BIN", &bin);
            std::env::remove_var("OMACLOUD_CONFIG");
        }

        let start = Instant::now();
        let out =
            super::cli_tty(&super::owned(&["omacloud", "recovery"]), &[]).unwrap_or_else(|e| panic!("{}", e.shown()));
        assert!(start.elapsed() < Duration::from_secs(3), "waited {:?}", start.elapsed());
        assert_eq!(out, "on a terminal\nanswer: []\nthe-secret\n");

        let e = super::cli_tty(&super::owned(&["omacloud", "fail"]), &[]).err().unwrap();
        assert_eq!(e.shown(), "it broke. Do this");
        assert!(start.elapsed() < Duration::from_secs(3));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A command still running when the last window closes: the application
    /// has nothing else to stay for (here it never had a window), and must
    /// stay until the command's answer has been handled. The hold is
    /// omui's, through `in_background`.
    #[test]
    fn the_app_stays_until_a_running_command_is_handled() {
        use omui::gtk::{gio, gio::prelude::*};
        use std::{cell::Cell, rc::Rc};

        // Its own id, on no bus: nothing of a running Omacloud is reached. The first application of a process is
        // its default one, which is where omui's hold finds it.
        let app = gio::Application::new(
            Some("com.ferdousbhai.Omacloud.HoldTest"),
            gio::ApplicationFlags::NON_UNIQUE,
        );
        let handled = Rc::new(Cell::new(None));
        let seen = handled.clone();
        app.connect_activate(move |_| {
            let seen = seen.clone();
            super::in_background(
                || {
                    std::thread::sleep(Duration::from_millis(300));
                    Ok("done")
                },
                move |answer| seen.set(answer.ok()),
            );
        });
        let start = Instant::now();
        app.run_with_args(&["omacloud-app"]);
        assert_eq!(handled.get(), Some("done"), "the application ended under its command");
        assert!(start.elapsed() >= Duration::from_millis(300));
    }

    /// README.md's table of shortcuts lists every key the app takes, and so
    /// does the overlay `?` opens, where keys that do the same share a row.
    #[test]
    fn the_readme_and_the_overlay_list_every_shortcut() {
        let readme = include_str!("../../../README.md");
        let listed: Vec<String> = super::shortcut_sections()
            .into_iter()
            .flat_map(|section| section.keys.into_iter().map(|(key, _)| key))
            .flat_map(|keys| keys.split(" / ").map(str::to_string).collect::<Vec<_>>())
            .collect();
        let keys = super::SHORTCUTS.iter().map(|(_, _, key)| *key).chain(super::OMUI_KEYS);
        for key in keys {
            assert!(
                readme.contains(&format!("| `{key}` |")),
                "README.md has no row for {key}"
            );
            assert!(listed.iter().any(|k| k == key), "the overlay doesn't list {key}");
        }
    }

    /// No key is given to two actions, and each is Ctrl with a key: a key alone would be taken from a text field.
    #[test]
    fn each_shortcut_is_ctrl_and_a_key_of_its_own() {
        let keys: Vec<&str> = super::SHORTCUTS
            .iter()
            .flat_map(|(_, keys, _)| keys.iter().copied())
            .chain(["<Control>q", "<Control>w"])
            .collect();
        for (i, key) in keys.iter().enumerate() {
            assert!(key.starts_with("<Control>"), "{key}");
            assert!(!keys[..i].contains(key), "{key} is given twice");
        }
    }

    /// Omarchy's one name, `omacloud`, is the command, the `.desktop` file,
    /// its icon, the class Hyprland matches the window by, and what both
    /// packages install; the window's own program is out of `bin`, and the
    /// old `omacloud-app` command still opens it.
    #[test]
    fn one_name_everywhere() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let desktop = std::fs::read_to_string(root.join(format!("contrib/{}.desktop", super::NAME))).unwrap();
        for line in [
            "Exec=omacloud open",
            "Icon=omacloud",
            "StartupWMClass=omacloud",
            "TryExec=omacloud",
        ] {
            assert!(desktop.lines().any(|l| l == line), "the desktop entry has no {line}");
        }
        assert!(root.join("contrib/omacloud.svg").is_file());
        for pkgbuild in ["pkgbuild/PKGBUILD", "packaging/omarchy-pkgs/omacloud/PKGBUILD"] {
            let text = std::fs::read_to_string(root.join(pkgbuild)).unwrap();
            for installed in [
                "/usr/share/applications/omacloud.desktop",
                "/usr/share/icons/hicolor/scalable/apps/omacloud.svg",
                "/usr/lib/omacloud/omacloud-app",
                "/usr/bin/omacloud-app",
            ] {
                assert!(text.contains(installed), "{pkgbuild} doesn't install {installed}");
            }
            // The old command is the window's own program, which opens the window as `omacloud open` does, so key
            // bindings and notification actions that still run it keep working.
            assert!(
                text.contains(r#"ln -s ../lib/omacloud/omacloud-app "$pkgdir/usr/bin/omacloud-app""#),
                "{pkgbuild} doesn't link the old omacloud-app command to the window"
            );
            assert!(
                !text.contains("com.ferdousbhai.Omacloud"),
                "{pkgbuild} still uses the old name"
            );
        }
        let widget = std::fs::read_to_string(root.join("contrib/omarchy-plugin/Widget.qml")).unwrap();
        assert!(widget.contains("run(\"omacloud open\")"));
    }

    #[test]
    fn leave_outcome_names_what_left_and_what_failed() {
        english();
        let d = serde_json::json!({"left": [{"app": "notes"}, {"app": "drive"}], "skipped": [], "failed": null});
        assert_eq!(
            super::leave_outcome(&d),
            "Notes, iCloud Drive left iCloud; Omacloud syncs them now"
        );
        let d = serde_json::json!({"left": [{"app": "notes"}], "failed": null});
        assert_eq!(super::leave_outcome(&d), "Notes left iCloud; Omacloud syncs it now");
        let d = serde_json::json!({"left": [], "failed": {"app": "reminders", "message": "the folder is not in step"}});
        assert_eq!(
            super::leave_outcome(&d),
            "Nothing left iCloud. Reminders didn't: the folder is not in step"
        );
    }

    #[test]
    fn only_chromium_waits_for_chromium() {
        assert_eq!(
            super::state_label("chromium", "waiting"),
            "Waiting for Chromium to close"
        );
        assert_eq!(super::state_label("wifi", "waiting"), "Waiting to apply");
        assert_eq!(super::state_label("wifi", "settled"), "Edits settled");
    }
}
