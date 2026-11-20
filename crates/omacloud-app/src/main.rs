//! The Omacloud app: account, devices, settings sync, secrets and
//! recovery. It works through the `omacloud` CLI (`status` to read, the
//! usual subcommands to act, all with `--json`: it reads the envelope's
//! `data`, or its `error`), so it holds no keys or state of its own and never
//! races the daemon over them. The commands that print a secret (`init`,
//! `join-code`) print it only to a terminal: the app runs those on a
//! pseudo-terminal of its own.
//!
//! `omacloud-app [overview|devices|settings|secrets|recovery]` opens on
//! that page. `OMACLOUD_BIN` picks the CLI (default: `omacloud` on PATH) and
//! `OMACLOUD_CONFIG` a config other than the default.

use std::{
    cell::Cell,
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    process::{Command, Stdio},
    rc::Rc,
};

use omui::gtk::{self, gio, glib, prelude::*};
use omui::{Appearance, Dialog, EmptyState, ExpanderRow, Group, Nav, Row, icons::name};
use serde_json::Value;

const APP_ID: &str = "com.ferdousbhai.Omacloud";

fn main() -> glib::ExitCode {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let page = args.next();
    // one window: launching again brings it forward instead of opening another
    let app = omui::application(APP_ID, "omacloud-app", "Omacloud", "", move |app| {
        build(app, page.as_deref());
    });
    // the page is ours to read; GTK sees no arguments
    app.run_with_args(&[argv0])
}

/// A command that failed: the CLI's error code (empty for other programs and
/// text-mode runs), what went wrong, and the CLI's advice on fixing it.
struct Failure {
    code: String,
    message: String,
    hint: String,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            code: String::new(),
            message: message.into(),
            hint: String::new(),
        }
    }

    /// What to show: the message, and the fix when the CLI gave one.
    fn shown(&self) -> String {
        let mut hint = self.hint.chars();
        match hint.next() {
            Some(first) => format!("{}. {}{}", self.message, first.to_uppercase(), hint.as_str()),
            None => self.message.clone(),
        }
    }
}

/// `argv` as a command: `omacloud ...` means the CLI with this app's config.
fn command(argv: &[String], env: &[(String, String)]) -> Result<Command, Failure> {
    let (program, args) = argv.split_first().ok_or_else(|| Failure::new("nothing to run"))?;
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
    Ok(cmd)
}

/// The last line of `stderr` that says something, without an `error: `.
fn last_line(stderr: &[u8], fallback: &str) -> String {
    text_failure(stderr, fallback).message
}

/// A failure from what a text-mode run left on stderr: its `error: ` line
/// (or the last line that says something) and the `hint: ` after it.
fn text_failure(stderr: &[u8], fallback: &str) -> Failure {
    let err = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
    let error = lines.iter().rposition(|l| l.starts_with("error: "));
    let message = error
        .map(|i| &lines[i]["error: ".len()..])
        .or_else(|| lines.last().copied())
        .unwrap_or(fallback);
    let hint = error
        .and_then(|i| lines[i + 1..].iter().find_map(|l| l.strip_prefix("hint: ")))
        .unwrap_or_default();
    Failure {
        hint: hint.to_string(),
        ..Failure::new(message)
    }
}

/// Run another program to completion: its stdout, or the last line of
/// stderr on failure.
fn run(argv: &[String]) -> Result<String, Failure> {
    let out = command(argv, &[])?
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Failure::new(format!("running {}: {e}", argv[0])))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(Failure::new(last_line(&out.stderr, "it failed")))
}

/// Run `omacloud ...` with `--json`: the result's `data`, or its error.
fn cli(argv: &[String], env: &[(String, String)]) -> Result<Value, Failure> {
    let out = command(argv, env)?
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Failure::new(format!("running omacloud: {e}")))?;
    match serde_json::from_slice::<Value>(&out.stdout) {
        Ok(v) if v["ok"] == Value::Bool(true) => Ok(v["data"].clone()),
        Ok(v) if v["ok"] == Value::Bool(false) => Err(Failure {
            code: text(&v["error"]["code"]),
            message: text(&v["error"]["message"]),
            hint: text(&v["error"]["hint"]),
        }),
        // no envelope: it crashed, or is an omacloud from before the contract
        _ => Err(Failure::new(last_line(&out.stderr, "omacloud failed"))),
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
    let (mut master, slave) = pty().map_err(|e| Failure::new(format!("no terminal: {e}")))?;
    let mut cmd = command(argv, env)?;
    let stdin = slave
        .try_clone()
        .map_err(|e| Failure::new(format!("no terminal: {e}")))?;
    cmd.stdin(Stdio::from(stdin))
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| Failure::new(format!("running omacloud: {e}")))?;
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
    let status = child.wait().map_err(|e| Failure::new(e.to_string()))?;
    let errors = errors.join().unwrap_or_default();
    if status.success() {
        return Ok(String::from_utf8_lossy(&out).replace('\r', ""));
    }
    Err(text_failure(&errors, "it failed"))
}

/// Run `job` in the background, then hand its result to `then` on the main
/// loop.
fn in_background<T: Send + 'static>(
    job: impl FnOnce() -> Result<T, Failure> + Send + 'static,
    then: impl FnOnce(Result<T, Failure>) + 'static,
) {
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(job)
            .await
            .unwrap_or_else(|_| Err(Failure::new("it crashed")));
        then(result);
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

    fn refresh(self: &Rc<Self>) {
        let ui = self.clone();
        cli_then(&["omacloud", "status"], Vec::new(), move |status| {
            match status {
                // no config yet: this computer needs setting up
                Ok(s) if s["set_up"] == Value::Bool(false) => {
                    if let Some(kind) = s["storage_gone"].as_str() {
                        ui.toast(&format!(
                            "This computer was set up on {kind}, which Omacloud no longer uses. Set it up again: \
                             your files here stay as they are."
                        ));
                    }
                    ui.show_tabs(false);
                    // not while someone is partway through it
                    if !ui.setting_up.replace(true) {
                        put(&ui.overview, &setup(&ui));
                    }
                    ui.stack.set_visible_child_name("overview");
                }
                Ok(s) => {
                    ui.show_tabs(true);
                    // setup's pages are another thing: the keyboard has no place to keep there
                    let focus = (!ui.setting_up.replace(false))
                        .then(|| focus_path(ui.stack.upcast_ref()))
                        .flatten();
                    put(&ui.overview, &overview(&ui, &s));
                    put(&ui.devices, &devices(&ui, &s));
                    put(&ui.settings, &settings(&ui, &s));
                    put(&ui.secrets, &secrets(&ui, &s));
                    put(&ui.recovery, &recovery());
                    if let Some(path) = focus {
                        focus_at(ui.stack.upcast_ref(), &path);
                    }
                }
                Err(e) => {
                    ui.show_tabs(true);
                    ui.setting_up.set(false);
                    let why = match e.code.as_str() {
                        "unavailable" => format!("Can't reach your storage: {}", e.shown()),
                        _ => e.shown(),
                    };
                    for page in [&ui.overview, &ui.devices, &ui.settings, &ui.secrets, &ui.recovery] {
                        put(page, &not_set_up(&why).widget);
                    }
                }
            }
        });
    }

    fn confirm(
        self: &Rc<Self>,
        heading: &str,
        body: &str,
        responses: &[(&str, &str, Appearance)],
        then: impl Fn(&str) + 'static,
    ) {
        let dialog = Dialog::new(Some(heading), Some(body));
        dialog.add_response("cancel", "Cancel");
        for (id, label, look) in responses {
            dialog.add_response(id, label);
            dialog.set_response_appearance(id, *look);
        }
        dialog.set_close_response("cancel");
        dialog.set_default_response("cancel");
        dialog.connect_response(move |_, id| {
            if id != "cancel" {
                then(id);
            }
        });
        dialog.present(&self.window);
    }
}

fn build(app: &gtk::Application, page: Option<&str>) {
    let stack = gtk::Stack::new();
    // the tabs follow the pages as they are added
    let header = omui::header(Some(omui::tabs(&stack).upcast_ref()));
    let page_of = |tag: &str, title: &str, icon: &str| {
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let spinner = omui::spinner();
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        spinner.set_size_request(32, 32);
        spinner.set_vexpand(true);
        put(&holder, &spinner);
        stack.add_titled(&holder, Some(tag), title).set_icon_name(icon);
        holder
    };
    let overview = page_of("overview", "Overview", name::CLOUD);
    let devices = page_of("devices", "Devices", name::COMPUTER);
    let settings = page_of("settings", "Omarchy", name::SETTINGS);
    let secrets = page_of("secrets", "Keys", name::KEY);
    let recovery = page_of("recovery", "Recovery", name::USERS);

    let sync = omui::icon_button(name::REFRESH, "Sync now");
    header.pack_end(&sync);
    // leaving iCloud is optional and rare: in this menu only, and only with
    // the iCloud apps installed
    let has_menu = on_path("icloud-session");
    if has_menu {
        let menu = gio::Menu::new();
        menu.append(Some("Move from iCloud…"), Some("win.move-from-icloud"));
        header.pack_end(
            &gtk::MenuButton::builder()
                .icon_name(name::MENU)
                .tooltip_text("Main menu")
                .has_frame(false)
                .menu_model(&menu)
                .build(),
        );
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
    });
    {
        let ui = ui.clone();
        sync.connect_clicked(move |_| ui.act(&["omacloud", "sync"], "Synced"));
    }
    if has_menu {
        let action = gio::SimpleAction::new("move-from-icloud", None);
        let ui = ui.clone();
        action.connect_activate(move |_, _| move_from_icloud(&ui));
        window.add_action(&action);
    }
    if let Some(page) = page {
        stack.set_visible_child_name(page);
    }
    ui.refresh();
    window.present();
}

/// Where the keyboard is under `root`: the place of each widget among its siblings, from `root` down to the focused
/// one. `None` when the keyboard is elsewhere (the title bar, a dialog).
fn focus_path(root: &gtk::Widget) -> Option<Vec<usize>> {
    let mut at = root.root()?.focus()?;
    let mut path = Vec::new();
    while at != *root {
        path.push(std::iter::successors(at.prev_sibling(), gtk::Widget::prev_sibling).count());
        at = at.parent()?;
    }
    path.reverse();
    Some(path)
}

/// Gives the keyboard to the widget at `path` ([`focus_path`]) under `root`. The pages are built again from each
/// new status, and the keyboard would otherwise be left on a row near the control someone used, or nowhere. A page
/// built again has that control in the same place unless the status changed around it: then the row now nearest
/// that place has the keyboard, or the first stop for Tab around it.
fn focus_at(root: &gtk::Widget, path: &[usize]) {
    let mut at = root.clone();
    for &place in path {
        let Some(last) = at.last_child() else { break };
        at = std::iter::successors(at.first_child(), gtk::Widget::next_sibling)
            .nth(place)
            .unwrap_or(last);
    }
    // as Tab arrives: at a control itself, and in a list at a row, never at the list, where nothing shows it
    while !at.child_focus(gtk::DirectionType::TabForward) && at != *root {
        // from inside a row to the row, not to the text in it that Tab stops at first
        let row = at.ancestor(gtk::ListBoxRow::static_type()).filter(|row| *row != at);
        let Some(around) = row.or_else(|| at.parent()) else {
            break;
        };
        at = around;
    }
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
    EmptyState::new(Some(name::CLOUD), "Nothing to show yet", Some(why))
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
/// nothing happening. Closed by the caller when it's done.
fn waiting(ui: &Rc<Ui>, text: &str) -> Dialog {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
    let spinner = omui::spinner();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    body.append(&spinner);
    body.append(
        &gtk::Label::builder()
            .label(text)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .max_width_chars(40)
            .build(),
    );
    let dialog = Dialog::new(Some("One Moment"), None);
    show_only(&dialog, &body);
    dialog.set_can_close(false);
    dialog.present(&ui.window);
    dialog
}

/// Run `omacloud init` with `args` (and secrets in `env`, never on the
/// command line), then show what came of it.
fn run_init(ui: &Rc<Ui>, args: Vec<String>, env: Vec<(String, String)>, wait: &str) {
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
                // shown once: nobody can show it again
                let dialog = Dialog::new(
                    Some("Your recovery code"),
                    Some(
                        "Write it down and keep it offline. With it you can add a computer when \
                         no other is at hand and recover the account. It is shown only now: \
                         without it and without a computer, your files can't be recovered.",
                    ),
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
                let (check_row, check) = omui::entry_row("Type its last four characters", "");
                let typed_back = group("", "");
                typed_back.add(&check_row.widget);
                let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
                body.append(&label);
                body.append(&typed_back.widget);
                dialog.set_extra_child(Some(body.upcast_ref()));
                dialog.add_response("copy", "Copy");
                dialog.add_response("done", "I Wrote It Down");
                dialog.set_response_appearance("done", Appearance::Suggested);
                dialog.set_response_enabled("done", false);
                // copying leaves it open, the code still to be written down, and
                // Escape does nothing: only the code typed back closes it
                dialog.set_response_closes("copy", false);
                dialog.set_can_close(false);
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
                let (window, ui3) = (ui2.window.clone(), ui2.clone());
                dialog.connect_response(move |_, id| {
                    if id == "copy" {
                        window.clipboard().set_text(&code);
                    } else {
                        ui3.refresh();
                    }
                });
                dialog.present(&ui2.window);
            } else if let Some(fp) = fp {
                show_text(
                    &ui2,
                    "Approve this computer",
                    "On a computer that already syncs, open omacloud's Devices and approve the one \
                     showing this fingerprint.",
                    &fp,
                );
                ui2.refresh();
            } else {
                ui2.toast("This computer is set up");
                ui2.refresh();
            }
        }
    });
}

/// The storage a new account can be on: its name here, what the field that
/// says where is called, and its `init` flags.
const PROVIDERS: [(&str, &str, &str, &str); 3] = [
    ("Cloudflare R2", "Cloudflare account ID", "r2", "--account"),
    ("Hetzner", "Location (fsn1, nbg1 or hel1)", "hetzner", "--location"),
    ("Other S3", "Endpoint (https://...)", "s3", "--endpoint"),
];

fn provider_at(i: u32) -> (&'static str, &'static str, &'static str, &'static str) {
    PROVIDERS[usize::try_from(i).unwrap_or(0).min(PROVIDERS.len() - 1)]
}

const SETTING_UP: &str = "Omacloud is setting up this computer, which can take a minute.";

/// First run, one question at a time: what most people need comes first, and
/// the details of buckets and codes only for those who choose them.
fn setup(ui: &Rc<Ui>) -> gtk::Stack {
    let first = gtk::Box::new(gtk::Orientation::Vertical, 12);
    first.set_halign(gtk::Align::Center);
    let welcome = EmptyState::new(
        Some(name::CLOUD),
        "Your files on all your computers",
        Some(
            "Omacloud keeps your files and Omarchy settings the same on every computer. They're \
             encrypted here before they leave, so only your computers can read them.",
        ),
    );
    welcome.set_child(Some(first.upcast_ref()));
    let nav = Nav::new(&step(None, "Omacloud", &welcome.widget, None));
    let choice = |label: &str, class: Option<&str>, tag: &'static str, page: fn(&Rc<Ui>, &Nav) -> gtk::Box| {
        let b = button(label, class);
        b.add_css_class("pill");
        // weak: the pages hold the button
        let (ui, nav) = (ui.clone(), nav.downgrade());
        b.connect_clicked(move |_| {
            if let Some(nav) = nav.upgrade() {
                let page = page(&ui, &nav);
                nav.push(tag, &page);
                // the keyboard starts in the form (the page's last child), not on Back
                if let Some(form) = page.last_child() {
                    form.child_focus(gtk::DirectionType::TabForward);
                }
            }
        });
        first.append(&b);
    };
    choice("Set Up With Dropbox", Some("suggested-action"), "dropbox", dropbox_step);
    choice("I Already Use Omacloud", None, "join", join_step);
    choice("Use My Own Storage", Some("flat"), "bucket", bucket_step);
    nav.widget
}

/// A page of setup, with its own way back (to the page under it in `nav`), and
/// its action always in view.
fn step(nav: Option<&Nav>, title: &str, content: &impl IsA<gtk::Widget>, action: Option<&gtk::Button>) -> gtk::Box {
    let header = omui::header(Some(&omui::Title::new(title, "").widget.upcast()));
    if let Some(nav) = nav {
        let back = omui::icon_button(name::BACK, "Back");
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
    let more = ExpanderRow::new("Lost every computer?", "");
    let (row, code) = omui::password_row("Recovery code");
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
            ui.toast("That's not a recovery code (28 characters)");
            None
        }
    }
}

fn dropbox_step(ui: &Rc<Ui>, nav: &Nav) -> gtk::Box {
    let (page, column) = page();
    let (what, sync_args) = choose_what_syncs();
    column.append(&what.widget);
    let (rescue_group, rescue) = recovery_group();
    column.append(&rescue_group.widget);
    let connect = button("Continue", Some("suggested-action"));
    let ui = ui.clone();
    connect.connect_clicked(move |_| {
        let Some(env) = recovery_env(&ui, &rescue) else {
            return;
        };
        let mut args = vec!["--provider".to_string(), "dropbox".to_string()];
        args.extend(sync_args());
        run_init(
            &ui,
            args,
            env,
            "Allow Omacloud in Dropbox in your browser. Setting up can then take a minute.",
        );
    });
    step(Some(nav), "What to Sync", &page, Some(&connect))
}

fn bucket_step(ui: &Rc<Ui>, nav: &Nav) -> gtk::Box {
    let (page, column) = page();
    let (what, sync_args) = choose_what_syncs();
    let create = group(
        "Your bucket",
        "Make a private bucket and a key that reaches it at your provider, then give them here.",
    );
    let (rescue_group, rescue) = recovery_group();
    let (provider_row, provider) = omui::combo_row("Provider", "", &gtk::StringList::new(&PROVIDERS.map(|p| p.0)), 0);
    create.add(&provider_row.widget);
    let (place_row, place) = omui::entry_row(PROVIDERS[0].1, "");
    {
        let (place_row, place) = (place_row.clone(), place.clone());
        provider.connect_selected_notify(move |p| {
            place_row.set_title(provider_at(p.selected()).1);
            place.set_text("");
        });
    }
    create.add(&place_row.widget);
    let (bucket_row, bucket) = omui::entry_row("Bucket name", "");
    create.add(&bucket_row.widget);
    let (key_id_row, key_id) = omui::entry_row("Access key ID", "");
    create.add(&key_id_row.widget);
    let (secret_row, secret) = omui::password_row("Secret access key");
    create.add(&secret_row.widget);
    let create_button = button("Create Account", Some("suggested-action"));
    {
        let ui = ui.clone();
        create_button.connect_clicked(move |_| {
            let (place, bucket, key_id) = (
                place.text().trim().to_string(),
                bucket.text().trim().to_string(),
                key_id.text().trim().to_string(),
            );
            if place.is_empty() || bucket.is_empty() || key_id.is_empty() || secret.text().is_empty() {
                ui.toast("Fill in where your bucket is, and its key");
                return;
            }
            let (_, _, name, flag) = provider_at(provider.selected());
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
            run_init(&ui, args, env, SETTING_UP);
        });
    }
    column.append(&create.widget);
    column.append(&what.widget);
    column.append(&rescue_group.widget);
    step(Some(nav), "Your Own Storage", &page, Some(&create_button))
}

fn join_step(ui: &Rc<Ui>, nav: &Nav) -> gtk::Box {
    let (page, column) = page();
    let (what, sync_args) = choose_what_syncs();
    let join = group(
        "Your code",
        "On your other computer, open Omacloud, go to Devices and click Show Join Code, then \
         paste it here. That computer then approves this one. If someone gave you an invite, \
         paste that instead, and you get an account of your own.",
    );
    let (jcode_row, jcode) = omui::password_row("Join code");
    let (rescue_group, code) = recovery_group();
    join.add(&jcode_row.widget);
    let join_button = button("Add This Computer", Some("suggested-action"));
    {
        let ui = ui.clone();
        join_button.connect_clicked(move |_| {
            let j = jcode.text().trim().to_string();
            if j.is_empty() {
                ui.toast("Paste the join code or invite");
                return;
            }
            let Some(mut env) = recovery_env(&ui, &code) else {
                return;
            };
            env.push(("OMACLOUD_JOIN_CODE".to_string(), j));
            run_init(&ui, sync_args(), env, SETTING_UP);
        });
    }
    column.append(&join.widget);
    column.append(&what.widget);
    column.append(&rescue_group.widget);
    step(Some(nav), "Add This Computer", &page, Some(&join_button))
}

/// A button that opens `path` in the file manager.
fn open_button(window: &gtk::ApplicationWindow, path: String) -> gtk::Button {
    let open = omui::icon_button(name::FOLDER_OPEN, "Open");
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

fn overview(ui: &Rc<Ui>, s: &Value) -> gtk::ScrolledWindow {
    let (page, column) = page();

    let sync = group("", "");
    let search = row(
        "Find files by meaning",
        "Optional Recall · searches folders selected in Recall settings",
    );
    let launch = button("Open Recall", None);
    let ui2 = ui.clone();
    launch.connect_clicked(move |_| {
        let ui = ui2.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(|| std::process::Command::new("recall").arg("toggle").status()).await;
            if !matches!(result, Ok(Ok(status)) if status.success()) {
                ui.toast("Recall is unavailable. Use Open on a sync folder to browse files.");
            }
        });
    });
    search.add_suffix(&launch);
    let folders = &s["folders"];
    let mut folder_groups = Vec::new();
    let mut folder_group_row = None;
    if folders.is_object() {
        folder_groups = what_syncs(ui, s);
    } else {
        let folder = text(&s["folder"]);
        let folder_row = row("Folder", &folder);
        folder_row.add_suffix(&open_button(&ui.window, folder));
        folder_group_row = Some(folder_row);
    }
    let daemon = s["daemon"].as_bool().unwrap_or(false);
    let synced_at = s["synced_at"].as_u64();
    let synced = synced_at.map_or_else(|| "Not synced yet".to_string(), |t| format!("Synced {}", ago(t)));
    let repo = &s["repo"];
    let place = match repo["kind"].as_str() {
        Some("bucket") => format!("{} at {}", text(&repo["bucket"]), text(&repo["endpoint"])),
        Some("dropbox") => "your Dropbox".to_string(),
        Some("path") => short_path(&text(&repo["repository"])),
        _ => text(&repo["repository"]),
    };
    // a daemon syncs at least every ten minutes: much longer, and something's wrong
    let fresh = synced_at.is_some_and(|t| secs_since(t) < 3600);
    let (state, icon) = if !daemon {
        ("Not syncing", Some(name::WARNING))
    } else if fresh {
        ("Up to date", Some(name::CHECK))
    } else if synced_at.is_some() {
        ("Not synced lately", Some(name::WARNING))
    } else {
        ("Syncing…", None)
    };
    let status = row(
        state,
        &if daemon {
            format!("{synced} · in {}", place.trim())
        } else {
            "Nothing syncs until background sync starts".to_string()
        },
    );
    if let Some(icon) = icon {
        status.add_prefix(&omui::icon(icon));
    }
    if !daemon {
        let start = button("Start", Some("suggested-action"));
        let ui2 = ui.clone();
        start.connect_clicked(move |_| {
            ui2.act(&["systemctl", "--user", "start", "omacloud"], "Started");
        });
        status.add_suffix(&start);
    }
    sync.add(&status.widget);
    if let Some(r) = folder_group_row {
        sync.add(&r.widget);
    }
    let deleted = row("Recently Deleted", "Files deleted in the last 30 days, to put back");
    let show = button("Show…", None);
    let ui2 = ui.clone();
    show.connect_clicked(move |_| show_deleted(&ui2));
    deleted.add_suffix(&show);
    column.append(&sync.widget);
    let whole_folder = folder_groups.is_empty();
    for g in &folder_groups {
        column.append(&g.widget);
    }
    let files = group(
        "Your files",
        "Anything named like name.nosync stays on this computer only.",
    );
    // syncing one whole folder, there's no Folders group to open them from
    if whole_folder {
        let online = row(
            "On another device",
            "Your files in a browser, at omacloud.computer/drive",
        );
        online.add_suffix(&phone_button(ui));
        files.add(&online.widget);
    }
    files.add(&deleted.widget);
    files.add(&search.widget);
    column.append(&files.widget);

    // for support and the curious
    let details = ExpanderRow::new("Details", "");
    for (title, value) in [
        ("Stored in", place.trim().to_string()),
        ("Account", text(&s["account_fingerprint"])),
        ("Account id", text(&s["account"])),
        (
            "Version",
            s["synced_head"]
                .as_u64()
                .map_or_else(|| "None yet".to_string(), |h| h.to_string()),
        ),
        (
            "Key epoch",
            s["epoch"]
                .as_u64()
                .map_or_else(|| "No key yet".to_string(), |e| e.to_string()),
        ),
    ] {
        details.add_row(&row(title, &value).widget);
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
    let list = Rc::new(group("", "Looking through your history…"));
    column.append(&list.widget);
    let dialog = Dialog::new(Some("Recently Deleted"), None);
    dialog.set_extra_child(Some(page.upcast_ref()));
    // The list's own height; the heading and Close add 126 px, for the 520 the dialog had with libadwaita.
    dialog.set_content_size(560, 394);
    dialog.add_response("close", "Close");
    dialog.set_close_response("close");
    dialog.present(&ui.window);
    let ui = ui.clone();
    cli_then(&["omacloud", "deleted"], Vec::new(), move |r| {
        let files = match r {
            Ok(Value::Array(files)) => files,
            Ok(_) => Vec::new(),
            Err(e) => {
                list.set_description(&e.shown());
                return;
            }
        };
        list.set_description(if files.is_empty() {
            "Nothing was deleted in the last 30 days."
        } else {
            "Put Back writes the last version where it was. Older versions stay under `omacloud versions`."
        });
        for f in files {
            let size = f["size"].as_u64().unwrap_or(0);
            let rw = row(
                &text(&f["relative"]),
                &format!("Deleted on {} · {}", text(&f["device"]), human(size)),
            );
            let put = button("Put Back", Some("flat"));
            let (ui, group, rw2) = (ui.clone(), list.clone(), rw.clone());
            let (path, version) = (text(&f["path"]), f["version"].to_string());
            put.connect_clicked(move |b| {
                b.set_sensitive(false);
                let (ui, list, rw, b) = (ui.clone(), group.clone(), rw2.clone(), b.clone());
                cli_then(
                    &["omacloud", "restore", &path, "--at", &version, "--to", &path],
                    Vec::new(),
                    move |r| match r {
                        Ok(_) => {
                            list.remove(&rw.widget);
                            ui.toast("Put back");
                        }
                        Err(e) => {
                            b.set_sensitive(true);
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

fn secs_since(t: u64) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .saturating_sub(t)
}

/// "2 minutes ago", for a time in seconds since the epoch.
fn ago(t: u64) -> String {
    let s = secs_since(t);
    let n = |v: u64, unit: &str| format!("{v} {unit}{} ago", if v == 1 { "" } else { "s" });
    match s {
        0..60 => "just now".to_string(),
        60..3600 => n(s / 60, "minute"),
        3600..86400 => n(s / 3600, "hour"),
        _ => n(s / 86400, "day"),
    }
}

/// `~/Documents` for a path in home.
fn short_path(path: &str) -> String {
    std::env::var("HOME")
        .ok()
        .and_then(|h| path.strip_prefix(&h).map(|rest| format!("~{rest}")))
        .unwrap_or_else(|| path.to_string())
}

/// "2.4 GB", for a size in bytes.
fn human(bytes: u64) -> String {
    let units = ["bytes", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < units.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{size:.1} {}", units[unit])
    }
}

/// Show `path` and, once measured, its size as `row`'s subtitle.
fn subtitle_with_size(row: &Row, path: &str) {
    let shown = short_path(path);
    row.set_subtitle(&shown);
    if !std::path::Path::new(path).is_dir() {
        return;
    }
    let row = row.clone();
    run_then(&["du", "-sb", path], move |r| {
        if let Some(bytes) = r
            .ok()
            .and_then(|out| out.split_whitespace().next()?.parse::<u64>().ok())
        {
            row.set_subtitle(&format!("{shown} · {}", human(bytes)));
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
    let phone = button("Open on a Phone…", Some("flat"));
    phone.set_tooltip_text(Some(
        "Your files in a browser on another device, at omacloud.computer/drive",
    ));
    let ui2 = ui.clone();
    phone.connect_clicked(move |_| show_drive_link(&ui2));
    phone
}

/// A drive link as the code a phone's camera reads: Omacloud Drive opens in
/// its browser with nothing typed. The link works for five minutes, and
/// closes with this dialog.
fn show_drive_link(ui: &Rc<Ui>) {
    let ui2 = ui.clone();
    // it opens the account's files: printed only to a terminal
    tty_then(&["omacloud", "drive-link", "--keep"], Vec::new(), move |r| {
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
            return ui2.toast("Couldn't draw the link");
        };
        let picture = gtk::DrawingArea::builder()
            .content_width(300)
            .content_height(300)
            .halign(gtk::Align::Center)
            .build();
        picture.set_draw_func(move |_, cr, width, height| {
            // black on white whatever the theme: that's what a camera reads
            const MARGIN: usize = 3;
            let side = code.width() + 2 * MARGIN;
            let cell = (f64::from(width.min(height)) / side as f64).floor().max(1.0);
            let left = (f64::from(width) - cell * side as f64) / 2.0;
            let top = (f64::from(height) - cell * side as f64) / 2.0;
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.rectangle(left, top, cell * side as f64, cell * side as f64);
            _ = cr.fill();
            cr.set_source_rgb(0.0, 0.0, 0.0);
            for (i, color) in code.to_colors().into_iter().enumerate() {
                if color == qrcode::Color::Dark {
                    let (x, y) = (i % code.width() + MARGIN, i / code.width() + MARGIN);
                    cr.rectangle(left + cell * x as f64, top + cell * y as f64, cell, cell);
                }
            }
            _ = cr.fill();
        });
        let dialog = Dialog::new(
            Some("Open on a phone"),
            Some(
                "Point your phone's camera at this. Your files open in its browser, read straight \
                 from your storage. It works for five minutes, or until you close this.",
            ),
        );
        show_only(&dialog, &picture);
        dialog.add_response("copy", "Copy Link");
        dialog.add_response("close", "Done");
        dialog.set_response_appearance("close", Appearance::Suggested);
        dialog.set_close_response("close");
        let ui3 = ui2.clone();
        dialog.connect_response(move |_, id| {
            if id == "copy" {
                // it stays good for its five minutes, to be pasted elsewhere
                ui3.window.clipboard().set_text(&link);
                ui3.toast("Link copied: it works for five minutes");
            } else {
                cli_then(&["omacloud", "drive-link", "--close"], Vec::new(), |_| {});
            }
        });
        dialog.present(&ui2.window);
    });
}

/// The folders every Omarchy computer has, in their usual order.
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
                ui2.act(&["omacloud", "folders", verb, &name], "Syncing");
            } else {
                let ui3 = ui2.clone();
                let ui4 = ui2.clone();
                let dialog = Dialog::new(
                    Some(&format!("Stop syncing {name} here?")),
                    Some("Its files stay on this computer, and your other computers keep syncing it."),
                );
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("skip", "Stop Syncing");
                dialog.set_response_appearance("skip", Appearance::Destructive);
                dialog.set_close_response("cancel");
                dialog.connect_response(move |_, id| {
                    if id == "skip" {
                        ui3.act(&["omacloud", "folders", "skip", &name], "Stopped syncing here");
                    } else {
                        ui4.refresh();
                    }
                });
                dialog.present(&ui2.window);
            }
        });
        subtitle_with_size(&r, &path);
        r
    };
    let folders_group = group("Folders", "Each one is the same on all your computers that sync it.");
    folders_group.set_header_suffix(Some(phone_button(ui).upcast_ref()));
    for r in rows {
        folders_group.add(&folder_row(r).widget);
    }
    // every other folder of home, a switch each
    let more = folders["more"].as_array().unwrap_or(&empty);
    if !more.is_empty() {
        let x = ExpanderRow::new("Add a folder from your home", "");
        for f in more {
            let (name, ui2) = (text(&f["name"]), ui.clone());
            let name2 = name.clone();
            let r = switch_row(&name, false, move |_| {
                ui2.act(&["omacloud", "folders", "add", &name2], "Syncing");
            });
            r.set_subtitle(&short_path(&text(&f["path"])));
            x.add_row(&r.widget);
        }
        folders_group.add(&x.widget);
    }

    // Omarchy settings, beside the folders, as iCloud lists more than files
    let settings_group = group("Settings", "");
    let settings_on = s["settings"]["on"].as_bool().unwrap_or(false);
    let ui2 = ui.clone();
    let settings = switch_row("Omarchy settings", settings_on, move |want| {
        let verb = if want { "on" } else { "off" };
        ui2.act(
            &["omacloud", "settings", verb],
            if want {
                "Omarchy settings sync"
            } else {
                "Omarchy settings stay here"
            },
        );
    });
    settings.set_subtitle(SETTINGS_SYNCED);
    settings_group.add(&settings.widget);

    vec![folders_group, settings_group]
}

/// The setup page's choice of what syncs, as iCloud asks on a new Mac:
/// switches for the usual folders and Omarchy settings, with their sizes.
/// Returns the group and what to add to `omacloud init`.
fn choose_what_syncs() -> (Group, impl Fn() -> Vec<String>) {
    let g = group("What to sync", "You can change these any time.");
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
        subtitle_with_size(&row, &path);
        g.add(&row.widget);
        switches.push((name, switch));
    }
    let (settings_row, settings) = omui::switch_row("Omarchy settings", SETTINGS_SYNCED, true);
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
            "Asking to join",
            "Approve a device only if it shows the same fingerprint.",
        );
        for r in requests {
            let (name, fp) = (text(&r["name"]), text(&r["fingerprint"]));
            let rw = row(&name, &fp);
            let approve = button("Approve…", Some("suggested-action"));
            let ui2 = ui.clone();
            approve.connect_clicked(move |_| {
                let ui3 = ui2.clone();
                let fp2 = fp.clone();
                ui2.confirm(
                    &format!("Approve {name}?"),
                    &format!(
                        "Only if {name} shows this fingerprint:\n\n{fp}\n\nIt will get the key to everything in this account."
                    ),
                    &[("approve", "Approve", Appearance::Suggested)],
                    move |_| ui3.act(&["omacloud", "devices", "approve", &fp2], "Approved"),
                );
            });
            rw.add_suffix(&approve);
            g.add(&rw.widget);
        }
        column.append(&g.widget);
    }

    let me = &s["this_device"];
    let g = group(
        "Devices",
        if me["role"] == "waiting" {
            "This device is waiting for approval."
        } else {
            ""
        },
    );
    if me["role"] == "member" {
        let add = button("Show Join Code", Some("flat"));
        let ui2 = ui.clone();
        add.connect_clicked(move |_| show_join_code(&ui2));
        g.set_header_suffix(Some(add.upcast_ref()));
    }
    let on_dropbox = s["repo"]["kind"] == "dropbox";
    for d in list("devices") {
        let (name, fp) = (text(&d["name"]), text(&d["fingerprint"]));
        let rw = row(&name, &fp);
        if d["this"].as_bool() == Some(true) {
            let tag = gtk::Label::new(Some("This device"));
            tag.add_css_class("dim-label");
            rw.add_suffix(&tag);
        } else {
            let remove = button("Remove…", Some("flat"));
            let ui2 = ui.clone();
            remove.connect_clicked(move |_| {
                let ui3 = ui2.clone();
                let fp2 = fp.clone();
                ui2.confirm(
                    &format!("Remove {name}?"),
                    "It stops syncing, and its files stay on it. If it's lost or stolen, or someone \
                     else has it, Omacloud also changes your keys so it can't reach anything new. \
                     That uploads everything again, so it can take a while.",
                    &[
                        ("remove", "Remove", Appearance::Default),
                        ("lost", "It's Lost or Stolen", Appearance::Destructive),
                    ],
                    move |id| {
                        if id == "lost" {
                            cut_off(&ui3, &fp2, on_dropbox);
                        } else {
                            ui3.act(&["omacloud", "devices", "revoke", fp2.as_str(), "--yes"], "Removed");
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
        let g = group("Removed", "");
        for d in removed {
            g.add(&row(&text(&d["name"]), &text(&d["fingerprint"])).widget);
        }
        column.append(&g.widget);
    }
    page
}

/// What each computer keeps, one expandable row per computer: its folders,
/// settings and settings files, each marked synced, backed up only or off.
/// This computer's settings files are chosen here; folders and settings by
/// their switches.
fn each_computer(ui: &Rc<Ui>, choices: &Value) -> Group {
    const WORDS: [&str; 3] = ["sync", "backup", "off"];
    let label = |w: &str| match w {
        "sync" => "Synced",
        "backup" => "Backed up only",
        "off" => "Off",
        _ => "Not known",
    };
    let marker = |w: &str| match w {
        "sync" => name::REFRESH,
        "backup" => name::DRIVE,
        "off" => name::UNAVAILABLE,
        _ => name::QUESTION,
    };
    let g = group(
        "What each computer keeps",
        "Synced: the same on every computer. Backed up only: kept from that computer, not synced. \
         Off: neither.",
    );
    let me = text(&choices["this_device"]);
    let empty = Vec::new();
    let items = choices["items"].as_array().unwrap_or(&empty);
    for device in choices["devices"].as_array().unwrap_or(&empty) {
        let device = text(device);
        let here = device == me;
        let x = ExpanderRow::new(&device, if here { "This computer" } else { "" });
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
                "Folders"
            } else {
                "Settings"
            };
            // with settings off, its groups and files say nothing more
            if section == "Settings" && all_settings != "sync" && name != "settings" {
                continue;
            }
            if section != heading {
                heading = section;
                let h = gtk::Label::builder()
                    .label(section)
                    .xalign(0.0)
                    .margin_top(12)
                    .margin_bottom(6)
                    .margin_start(12)
                    .build();
                h.add_css_class("heading");
                x.add_row(&h);
            }
            let kind = match item["kind"].as_str().unwrap_or_default() {
                "folders" => "",
                "settings" if name == "settings" => "All of them",
                "settings" => "A group",
                _ => "A file",
            };
            let title = if name == "settings" {
                "Omarchy settings".to_string()
            } else {
                name.clone()
            };
            let r = Row::new(&title, kind);
            let icon = omui::icon(marker(word));
            icon.set_tooltip_text(Some(label(word)));
            if word != "sync" {
                icon.add_css_class("dim-label");
            }
            r.add_prefix(&icon);
            if here && item["kind"] == "files" {
                let labels: Vec<&str> = WORDS.iter().map(|w| label(w)).collect();
                let pick = gtk::DropDown::from_strings(&labels);
                pick.set_valign(gtk::Align::Center);
                if let Some(i) = WORDS.iter().position(|w| *w == word) {
                    pick.set_selected(i as u32);
                }
                let ui = ui.clone();
                pick.connect_selected_notify(move |p| {
                    if let Some(w) = WORDS.get(p.selected() as usize) {
                        ui.act(&["omacloud", "choose", &name, w], "Changed here");
                    }
                });
                r.add_suffix(&pick);
            } else {
                let l = gtk::Label::new(Some(label(word)));
                l.add_css_class("dim-label");
                r.add_suffix(&l);
            }
            x.add_row(&r.widget);
        }
        if here && all_settings == "sync" {
            // a settings file not listed yet: backed up only, to start
            let add = row("Back up a settings file only", "like ~/.claude/CLAUDE.md, not synced");
            let b = button("Choose…", Some("flat"));
            let ui = ui.clone();
            b.connect_clicked(move |_| {
                let home = std::env::var("HOME").unwrap_or_default();
                let dialog = gtk::FileDialog::builder()
                    .title("Back up only")
                    .initial_folder(&gtk::gio::File::for_path(&home))
                    .build();
                let ui = ui.clone();
                dialog.open(Some(&ui.window.clone()), gtk::gio::Cancellable::NONE, move |file| {
                    if let Some(path) = file.ok().and_then(|f| f.path()) {
                        ui.act(
                            &["omacloud", "choose", &path.to_string_lossy(), "backup"],
                            "Backed up only",
                        );
                    }
                });
            });
            add.add_suffix(&b);
            x.add_row(&add.widget);
        }
        g.add(&x.widget);
    }
    g
}

/// A computer lost or stolen: removed, and cut off as far as the storage allows.
fn cut_off(ui: &Rc<Ui>, fingerprint: &str, on_dropbox: bool) {
    let (wait, done) = if on_dropbox {
        (
            "Changing your keys, which uploads everything again. Dropbox then asks you in your browser to \
             allow Omacloud once more.",
            "Removed. It's cut off once your other computers have switched",
        )
    } else {
        (
            "Changing your keys, which uploads everything again.",
            "Removed. Now make a new key at your provider, and run omacloud bucket set-key",
        )
    };
    let busy = waiting(ui, wait);
    let ui = ui.clone();
    cli_then(
        &["omacloud", "devices", "revoke", fingerprint, "--yes", "--lost"],
        Vec::new(),
        move |removed| {
            busy.force_close();
            ui.toast(&removed.map_or_else(|e| e.shown(), |_| done.to_string()));
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
            "Join code",
            "On the new computer, install Omacloud, open it, and paste this under Add this \
             computer to your account. Then approve it here. Pass the code only to your own computers, \
             and not through chat or email.",
            out.lines()
                .find(|l| l.starts_with("omacloud-join-"))
                .unwrap_or_default()
                .trim(),
        ),
    });
}

/// What the settings switch sends: passwords and keys are named, so nobody
/// turns it on thinking it's only themes.
const SETTINGS_SYNCED: &str =
    "Shell, desktop and apps, Chromium, AI agents with any API keys in their settings, and Wi-Fi passwords";

fn settings(ui: &Rc<Ui>, s: &Value) -> gtk::ScrolledWindow {
    let (page, column) = page();
    let st = &s["settings"];
    let on = st["on"].as_bool().unwrap_or(false);
    let g = group(
        "Omarchy settings",
        "The shared files in Omarchy's dots manifest (bindings, look and feel, terminals, \
         shell), AI agents' settings and skills, Chromium's bookmarks and settings, and \
         saved Wi-Fi networks. Ones about the computer, like monitors, are backed up only; \
         sign-in files are off. API keys written into settings files sync with them, encrypted.",
    );
    let (switch_row, switch) = omui::switch_row("Sync settings on this device", "", on);
    {
        let ui = ui.clone();
        switch.connect_active_notify(move |sw| {
            if sw.is_active() {
                ui.act(&["omacloud", "settings", "on"], "Settings sync is on");
            } else {
                ui.act(&["omacloud", "settings", "off"], "Settings sync is off");
            }
        });
    }
    g.add(&switch_row.widget);
    if let Some(why) = st["dormant"].as_str().filter(|_| on) {
        g.add(&row("Standing down", why).widget);
    }
    column.append(&g.widget);

    let empty_groups = Vec::new();
    let groups = st["groups"].as_array().unwrap_or(&empty_groups);
    if on && !groups.is_empty() {
        let g = group(
            "What syncs",
            "Edits on two computers merge. Chromium's are written in while it's closed; \
             history, passwords, cookies and open tabs stay on each computer.",
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
            "Changed on two computers",
            "These changed here and on another computer in ways that don't merge. \
             This computer keeps its version until you choose. Keep Each backs up \
             this computer's version only, and the others' stays synced.",
        );
        for p in held {
            let rel = text(p);
            let rw = row(&format!("~/{rel}"), "");
            let path = format!("{home}/{rel}");
            for (label, keep) in [("Keep Mine", "local"), ("Keep Theirs", "remote"), ("Keep Each", "each")] {
                let b = button(label, Some("flat"));
                let (ui, path) = (ui.clone(), path.clone());
                b.connect_clicked(move |_| {
                    ui.act(&["omacloud", "settings", "resolve", &path, "--keep", keep], "Resolved");
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
/// says. Only Chromium waits for an app to close.
fn state_label(group: &str, state: &str) -> String {
    match (group, state) {
        (_, "synced") => "In sync",
        (_, "new") => "Not synced yet",
        ("chromium", "waiting") => "Waiting for Chromium to close",
        (_, "waiting") => "Waiting to apply",
        (_, "install") => "Extensions to install",
        (_, "settled") => "Edits settled",
        (_, "refused") => "Not applied",
        (_, "failed") => "Can't write",
        (_, "conflict") => "Choose below",
        (_, "unavailable") => "Not available",
        (_, other) => other,
    }
    .to_string()
}

/// One settings group: its state, extensions to install, and what stays
/// on this computer. The same as `omacloud settings` lists.
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
        let b = button("Install", Some("flat"));
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
                Err(e) => ui.toast(&format!("running {program}: {e}")),
            }
        });
        r.add_suffix(&b);
        x.add_row(&r.widget);
    }
    for n in not_synced {
        x.add_row(&row("Not synced", &text(n)).widget);
    }
    x.widget
}

fn secrets(ui: &Rc<Ui>, s: &Value) -> gtk::ScrolledWindow {
    let (page, column) = page();
    if s["settings"]["on"].as_bool() != Some(true) {
        let g = group(
            "Secrets",
            "ssh and gpg keys and tokens travel with settings sync. Turn it on under Omarchy.",
        );
        column.append(&g.widget);
        return page;
    }
    let g = group(
        "This device",
        "Seal ~/.ssh, ~/.gnupg and token files (or what ~/.config/omacloud/secrets lists) \
         so they travel with your files. No device can open them: only your recovery code.",
    );
    let rw = row("Save this device's secrets", "Replaces what was saved before");
    let save = button("Save…", Some("suggested-action"));
    {
        let ui = ui.clone();
        save.connect_clicked(move |_| {
            let ui2 = ui.clone();
            ui.confirm(
                "Save secrets?",
                "They are sealed so that only your recovery code opens them.",
                &[("save", "Save", Appearance::Suggested)],
                move |_| ui2.act(&["omacloud", "secrets", "save"], "Secrets saved"),
            );
        });
    }
    rw.add_suffix(&save);
    g.add(&rw.widget);
    column.append(&g.widget);

    let empty = Vec::new();
    let saved = s["secrets"].as_array().unwrap_or(&empty);
    let g = group(
        "Saved",
        if saved.is_empty() {
            "No device has saved its secrets yet."
        } else {
            ""
        },
    );
    for d in saved {
        let device = text(d);
        let rw = row(&device, "");
        let restore = button("Restore…", Some("flat"));
        let ui = ui.clone();
        restore.connect_clicked(move |_| restore_secrets(&ui, &device));
        rw.add_suffix(&restore);
        g.add(&rw.widget);
    }
    column.append(&g.widget);
    page
}

/// A recovery code entry in a boxed list, for a dialog's extra child.
fn code_entry() -> (Group, gtk::PasswordEntry) {
    let (row, entry) = omui::password_row("Recovery code");
    let list = group("", "");
    list.add(&row.widget);
    (list, entry)
}

fn restore_secrets(ui: &Rc<Ui>, device: &str) {
    let (list, entry) = code_entry();
    let dialog = Dialog::new(
        Some(&format!("Restore {device}'s secrets?")),
        Some("Enter your recovery code."),
    );
    dialog.set_extra_child(Some(list.widget.upcast_ref()));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("next", "Continue");
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
                    let changes: Vec<String> = data["changes"]
                        .as_array()
                        .unwrap_or(&empty)
                        .iter()
                        .map(|c| format!("{:<8} {}", text(&c["action"]), text(&c["path"])))
                        .collect();
                    if changes.is_empty() {
                        ui3.toast("This computer already has those keys");
                        return;
                    }
                    let ui4 = ui3.clone();
                    ui3.confirm(
                        "Write these files?",
                        &format!("{}\n\nFiles this replaces are backed up first.", changes.join("\n")),
                        &[("write", "Write", Appearance::Destructive)],
                        move |_| {
                            let env = vec![("OMACLOUD_RECOVERY_CODE".to_string(), code.clone())];
                            let ui5 = ui4.clone();
                            cli_then(
                                &["omacloud", "secrets", "restore", "--from", &device2, "--yes"],
                                env,
                                move |r| {
                                    ui5.toast(&r.map_or_else(
                                        |e| e.shown(),
                                        |data| format!("Wrote {} files", data["written"].as_u64().unwrap_or(0)),
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
            "If you lose everything",
            "If you lose your recovery code and every computer, your files can't be recovered. \
             Keep a copy of the recovery code with someone you trust, or split it among several \
             people with `omacloud recovery split`.",
        )
        .widget,
    );
    page
}

/// Show text to copy.
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
    let intro = group(
        "",
        "Optional. Each app you choose syncs with iCloud a last time; then Omacloud syncs its folder \
         and the app stops using iCloud on this computer, for good. iCloud keeps its copy as it is: \
         nothing there is deleted. Do this on each of your computers.",
    );
    column.append(&intro.widget);
    let apps = group("Apps", "");
    let dialog = Dialog::new(Some("Move from iCloud"), None);
    dialog.set_extra_child(Some(page.upcast_ref()));
    // As tall as what it says, which is all known now; a lower window scrolls it.
    page.set_propagate_natural_height(true);
    dialog.set_content_size(520, -1);
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("leave", "Leave iCloud");
    dialog.set_response_appearance("leave", Appearance::Destructive);
    dialog.set_response_enabled("leave", false);
    dialog.set_close_response("cancel");
    let chosen: Rc<std::cell::RefCell<Vec<String>>> = Rc::default();
    let (confirm_row, confirm) = omui::entry_row("Type leave to confirm", "");
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
        let subtitle = match state.as_str() {
            "ready" => format!("{shown} goes to Omacloud"),
            "left" => "Left iCloud: Omacloud syncs it".to_string(),
            "not_used" => "Not used on this computer".to_string(),
            "not_installed" => continue,
            _ => text(&app["message"]),
        };
        let r = row(&name, &subtitle);
        if state == "ready" {
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
            let b = button(&format!("Sync ~/{top}"), Some("flat"));
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
        if word_is(app, "reminders") && state == "ready" {
            r.set_subtitle(&format!(
                "{subtitle}. Alerts, priorities, flags, list colours, repeats, sections and shared lists stay in iCloud only"
            ));
        }
        if word_is(app, "photos") && state == "ready" {
            r.set_subtitle(&format!(
                "{subtitle}. Every original is downloaded, then uploaded to your storage; albums become lists. \
                 An iPhone keeps sending new photos to iCloud"
            ));
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
            let busy = waiting(
                &ui,
                "Syncing with iCloud a last time, then handing the folders to Omacloud.",
            );
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

fn word_is(app: &Value, word: &str) -> bool {
    app["app"].as_str() == Some(word)
}

/// What `icloud-session leave` did, in a sentence.
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
    let mut out = if left.is_empty() {
        "Nothing left iCloud".to_string()
    } else {
        let them = if left.len() == 1 { "it" } else { "them" };
        format!("{} left iCloud; Omacloud syncs {them} now", left.join(", "))
    };
    if d["failed"].is_object() {
        out.push_str(&format!(
            ". {} didn't: {}",
            names(&text(&d["failed"]["app"])),
            text(&d["failed"]["message"])
        ));
    }
    out
}

fn show_text(ui: &Rc<Ui>, heading: &str, body: &str, content: &str) {
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
    let dialog = Dialog::new(Some(heading), Some(body));
    show_only(&dialog, &frame);
    dialog.add_response("close", "Close");
    dialog.add_response("copy", "Copy");
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

    #[test]
    fn leave_outcome_names_what_left_and_what_failed() {
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
