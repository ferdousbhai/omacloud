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
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    process::{Command, Stdio},
    rc::Rc,
};

use adw::prelude::*;
use gtk::{gio, glib};
use serde_json::Value;

mod theme;

const APP_ID: &str = "com.ferdousbhai.Omacloud";

fn main() -> glib::ExitCode {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let page = args.next();
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| theme::install(include_str!("desktop.css")));
    // one window: launching again brings it forward instead of opening another
    app.connect_activate(move |app| match app.active_window() {
        Some(window) => window.present(),
        None => build(app, page.as_deref()),
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
    window: adw::ApplicationWindow,
    stack: adw::ViewStack,
    /// Its tabs and sync button, which mean nothing until setup is done.
    view: adw::ToolbarView,
    toasts: adw::ToastOverlay,
    overview: adw::Bin,
    devices: adw::Bin,
    settings: adw::Bin,
    secrets: adw::Bin,
    recovery: adw::Bin,
}

impl Ui {
    fn toast(&self, text: &str) {
        let toast = adw::Toast::new(text);
        toast.set_use_markup(false);
        self.toasts.add_toast(toast);
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
        self.view.set_reveal_top_bars(show);
        self.view.set_reveal_bottom_bars(show);
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
                    if !ui.overview.child().is_some_and(|c| c.is::<adw::NavigationView>()) {
                        ui.overview.set_child(Some(&setup(&ui)));
                    }
                    ui.stack.set_visible_child_name("overview");
                }
                Ok(s) => {
                    ui.show_tabs(true);
                    ui.overview.set_child(Some(&overview(&ui, &s)));
                    ui.devices.set_child(Some(&devices(&ui, &s)));
                    ui.settings.set_child(Some(&settings(&ui, &s)));
                    ui.secrets.set_child(Some(&secrets(&ui, &s)));
                    ui.recovery.set_child(Some(&recovery()));
                }
                Err(e) => {
                    ui.show_tabs(true);
                    let why = match e.code.as_str() {
                        "unavailable" => format!("Can't reach your storage: {}", e.shown()),
                        _ => e.shown(),
                    };
                    for bin in [&ui.overview, &ui.devices, &ui.settings, &ui.secrets, &ui.recovery] {
                        bin.set_child(Some(&not_set_up(&why)));
                    }
                }
            }
        });
    }

    fn confirm(
        self: &Rc<Self>,
        heading: &str,
        body: &str,
        responses: &[(&str, &str, adw::ResponseAppearance)],
        then: impl Fn(&str) + 'static,
    ) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_response("cancel", "Cancel");
        for (id, label, look) in responses {
            dialog.add_response(id, label);
            dialog.set_response_appearance(id, *look);
        }
        dialog.set_close_response("cancel");
        dialog.set_default_response(Some("cancel"));
        dialog.connect_response(None, move |_, id| {
            if id != "cancel" {
                then(id);
            }
        });
        dialog.present(Some(&self.window));
    }
}

fn build(app: &adw::Application, page: Option<&str>) {
    let stack = adw::ViewStack::new();
    let bin = || {
        let b = adw::Bin::new();
        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .width_request(32)
            .height_request(32)
            .build();
        b.set_child(Some(&spinner));
        b
    };
    let (overview_bin, devices_bin, settings_bin, secrets_bin, recovery_bin) = (bin(), bin(), bin(), bin(), bin());
    stack.add_titled_with_icon(&overview_bin, Some("overview"), "Overview", "folder-remote-symbolic");
    stack.add_titled_with_icon(&devices_bin, Some("devices"), "Devices", "computer-symbolic");
    stack.add_titled_with_icon(
        &settings_bin,
        Some("settings"),
        "Omarchy",
        "preferences-desktop-symbolic",
    );
    stack.add_titled_with_icon(&secrets_bin, Some("secrets"), "Keys", "dialog-password-symbolic");

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    let header = adw::HeaderBar::new();
    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    header.set_title_widget(Some(&switcher));
    // too narrow for the pages in the header (beside another window, say):
    // they move to a bar at the bottom, and the header shows the name
    let bottom = adw::ViewSwitcherBar::builder().stack(&stack).build();
    let sync = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Sync now")
        .build();
    header.pack_end(&sync);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.add_bottom_bar(&bottom);
    view.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Omacloud")
        .default_width(1000)
        .default_height(640)
        .content(&view)
        .width_request(360)
        .height_request(400)
        .build();
    window.add_css_class("cloud-desktop");
    let narrow =
        adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 900sp").expect("a breakpoint condition"));
    narrow.add_setter(
        &header,
        "title-widget",
        Some(&adw::WindowTitle::new("Omacloud", "").to_value()),
    );
    narrow.add_setter(&bottom, "reveal", Some(&true.to_value()));
    window.add_breakpoint(narrow);
    let ui = Rc::new(Ui {
        window: window.clone(),
        stack: stack.clone(),
        view: view.clone(),
        toasts,
        overview: overview_bin,
        devices: devices_bin,
        settings: settings_bin,
        secrets: secrets_bin,
        recovery: recovery_bin.clone(),
    });
    stack.add_titled_with_icon(&recovery_bin, Some("recovery"), "Recovery", "system-users-symbolic");
    {
        let ui = ui.clone();
        sync.connect_clicked(move |_| ui.act(&["omacloud", "sync"], "Synced"));
    }
    if let Some(page) = page {
        stack.set_visible_child_name(page);
    }
    ui.refresh();
    window.present();
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .use_markup(false)
        .subtitle_selectable(true)
        .build()
}

fn group(title: &str, description: &str) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder().title(title).build();
    if !description.is_empty() {
        g.set_description(Some(description));
    }
    g
}

fn button(label: &str, class: Option<&str>) -> gtk::Button {
    let b = gtk::Button::builder().label(label).valign(gtk::Align::Center).build();
    if let Some(c) = class {
        b.add_css_class(c);
    }
    b
}

fn not_set_up(why: &str) -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("folder-remote-symbolic")
        .title("Nothing to show yet")
        .description(glib::markup_escape_text(why))
        .build()
}

/// A dialog that stays up through a long command, so the wait doesn't look like
/// nothing happening. Closed by the caller when it's done.
fn waiting(ui: &Rc<Ui>, text: &str) -> adw::Dialog {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
    body.set_margin_top(12);
    body.set_margin_bottom(24);
    body.set_margin_start(24);
    body.set_margin_end(24);
    body.append(
        &gtk::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .build(),
    );
    body.append(
        &gtk::Label::builder()
            .label(text)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .max_width_chars(40)
            .build(),
    );
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::builder().show_end_title_buttons(false).build());
    view.set_content(Some(&body));
    let dialog = adw::Dialog::builder()
        .title("One Moment")
        .can_close(false)
        .child(&view)
        .build();
    dialog.present(Some(&ui.window));
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
                let dialog = adw::AlertDialog::new(
                    Some("Your recovery code"),
                    Some(
                        "Write it down and keep it offline. With it you can add a computer when \
                         no other is at hand and recover the account. It is shown only now: \
                         without it and without a computer, your files can't be recovered.",
                    ),
                );
                let label = gtk::Label::builder()
                    .label(&code)
                    .selectable(true)
                    .css_classes(["title-2", "monospace"])
                    .build();
                // typed back, so nobody clicks past the one chance to keep it
                let check = adw::EntryRow::builder().title("Type its last four characters").build();
                let list = gtk::ListBox::new();
                list.add_css_class("boxed-list");
                list.set_selection_mode(gtk::SelectionMode::None);
                list.append(&check);
                let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
                body.append(&label);
                body.append(&list);
                dialog.set_extra_child(Some(&body));
                dialog.add_response("copy", "Copy");
                dialog.add_response("done", "I Wrote It Down");
                dialog.set_response_appearance("done", adw::ResponseAppearance::Suggested);
                dialog.set_response_enabled("done", false);
                dialog.set_close_response("copy");
                let tail: String = {
                    let chars: Vec<char> = code.chars().filter(char::is_ascii_alphanumeric).collect();
                    chars[chars.len().saturating_sub(4)..]
                        .iter()
                        .collect::<String>()
                        .to_ascii_lowercase()
                };
                {
                    let dialog = dialog.clone();
                    check.connect_changed(move |e| {
                        let typed: String = e
                            .text()
                            .chars()
                            .filter(char::is_ascii_alphanumeric)
                            .collect::<String>()
                            .to_ascii_lowercase();
                        dialog.set_response_enabled("done", typed == tail);
                    });
                }
                let (window, ui3) = (ui2.window.clone(), ui2.clone());
                dialog.connect_response(None, move |d, id| {
                    if id == "copy" {
                        window.clipboard().set_text(&code);
                        // copying doesn't close it: the code is still to be written down
                        d.present(Some(&window));
                    } else {
                        ui3.refresh();
                    }
                });
                dialog.present(Some(&ui2.window));
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
fn setup(ui: &Rc<Ui>) -> adw::NavigationView {
    let nav = adw::NavigationView::new();
    let first = gtk::Box::new(gtk::Orientation::Vertical, 12);
    first.set_halign(gtk::Align::Center);
    let choice = |label: &str, class: Option<&str>, page: fn(&Rc<Ui>) -> adw::NavigationPage| {
        let b = button(label, class);
        b.add_css_class("pill");
        let (ui, nav) = (ui.clone(), nav.clone());
        b.connect_clicked(move |_| nav.push(&page(&ui)));
        first.append(&b);
    };
    choice("Set Up With Dropbox", Some("suggested-action"), dropbox_step);
    choice("I Already Use Omacloud", None, join_step);
    choice("Use My Own Storage", Some("flat"), bucket_step);
    let welcome = adw::StatusPage::builder()
        .icon_name("folder-remote-symbolic")
        .title("Your files on all your computers")
        .description(
            "Omacloud keeps your files and Omarchy settings the same on every computer. They're \
             encrypted here before they leave, so only your computers can read them.",
        )
        .child(&first)
        .build();
    nav.add(&step("Omacloud", &welcome, None));
    nav
}

/// A page of setup, with its own way back, and its action always in view.
fn step(title: &str, content: &impl IsA<gtk::Widget>, action: Option<&gtk::Button>) -> adw::NavigationPage {
    let view = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    if let Some(action) = action {
        header.pack_end(action);
    }
    view.add_top_bar(&header);
    view.set_content(Some(content));
    adw::NavigationPage::new(&view, title)
}

/// For someone who lost every computer: out of the way of everyone else.
fn recovery_group() -> (adw::PreferencesGroup, adw::PasswordEntryRow) {
    let g = group("", "");
    let more = adw::ExpanderRow::builder().title("Lost every computer?").build();
    let code = adw::PasswordEntryRow::builder().title("Recovery code").build();
    more.add_row(&code);
    g.add(&more);
    (g, code)
}

/// The recovery code typed in, if any; `None` if it can't be one.
fn recovery_env(ui: &Rc<Ui>, row: &adw::PasswordEntryRow) -> Option<Vec<(String, String)>> {
    let typed = row.text().to_string();
    match typed.chars().filter(char::is_ascii_alphanumeric).count() {
        0 => Some(Vec::new()),
        28 => Some(vec![("OMACLOUD_RECOVERY_CODE".to_string(), typed)]),
        _ => {
            ui.toast("That's not a recovery code (28 characters)");
            None
        }
    }
}

fn dropbox_step(ui: &Rc<Ui>) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    let (what, sync_args) = choose_what_syncs();
    page.add(&what);
    let (rescue_group, rescue) = recovery_group();
    page.add(&rescue_group);
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
    step("What to Sync", &page, Some(&connect))
}

fn bucket_step(ui: &Rc<Ui>) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    let (what, sync_args) = choose_what_syncs();
    let create = group(
        "Your bucket",
        "Make a private bucket and a key that reaches it at your provider, then give them here.",
    );
    let (rescue_group, rescue) = recovery_group();
    let provider = adw::ComboRow::builder()
        .title("Provider")
        .model(&gtk::StringList::new(&PROVIDERS.map(|p| p.0)))
        .build();
    create.add(&provider);
    let place = adw::EntryRow::builder().title(PROVIDERS[0].1).build();
    {
        let place = place.clone();
        provider.connect_selected_notify(move |p| {
            place.set_title(provider_at(p.selected()).1);
            place.set_text("");
        });
    }
    create.add(&place);
    let bucket = adw::EntryRow::builder().title("Bucket name").build();
    create.add(&bucket);
    let key_id = adw::EntryRow::builder().title("Access key ID").build();
    create.add(&key_id);
    let secret = adw::PasswordEntryRow::builder().title("Secret access key").build();
    create.add(&secret);
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
    page.add(&create);
    page.add(&what);
    page.add(&rescue_group);
    step("Your Own Storage", &page, Some(&create_button))
}

fn join_step(ui: &Rc<Ui>) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    let (what, sync_args) = choose_what_syncs();
    let join = group(
        "Your code",
        "On your other computer, open Omacloud, go to Devices and click Show Join Code, then \
         paste it here. That computer then approves this one. If someone gave you an invite, \
         paste that instead, and you get an account of your own.",
    );
    let jcode = adw::PasswordEntryRow::builder().title("Join code").build();
    let (rescue_group, code) = recovery_group();
    join.add(&jcode);
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
    page.add(&join);
    page.add(&what);
    page.add(&rescue_group);
    step("Add This Computer", &page, Some(&join_button))
}

/// A button that opens `path` in the file manager.
fn open_button(window: &adw::ApplicationWindow, path: String) -> gtk::Button {
    let open = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text("Open")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
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

fn overview(ui: &Rc<Ui>, s: &Value) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();

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
    let mut folders_group = None;
    let mut folder_group_row = None;
    if folders.is_object() {
        folders_group = Some(what_syncs(ui, s));
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
        ("Not syncing", Some("dialog-warning-symbolic"))
    } else if fresh {
        ("Up to date", Some("object-select-symbolic"))
    } else if synced_at.is_some() {
        ("Not synced lately", Some("dialog-warning-symbolic"))
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
        status.add_prefix(&gtk::Image::from_icon_name(icon));
    }
    if !daemon {
        let start = button("Start", Some("suggested-action"));
        let ui2 = ui.clone();
        start.connect_clicked(move |_| {
            ui2.act(&["systemctl", "--user", "start", "omacloud"], "Started");
        });
        status.add_suffix(&start);
    }
    sync.add(&status);
    if let Some(r) = folder_group_row {
        sync.add(&r);
    }
    let online = row(
        "Every File",
        "All your folders in ~/Omacloud, those not kept here too, each downloaded when opened",
    );
    let browse = button("Browse…", None);
    let ui2 = ui.clone();
    browse.connect_clicked(move |_| browse_online(&ui2));
    online.add_suffix(&browse);
    let deleted = row("Recently Deleted", "Files deleted in the last 30 days, to put back");
    let show = button("Show…", None);
    let ui2 = ui.clone();
    show.connect_clicked(move |_| show_deleted(&ui2));
    deleted.add_suffix(&show);
    page.add(&sync);
    if let Some(g) = folders_group {
        page.add(&g);
    }
    let files = group("Your files", "");
    files.add(&online);
    files.add(&deleted);
    files.add(&search);
    page.add(&files);

    // for support and the curious
    let details = adw::ExpanderRow::builder().title("Details").build();
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
        let r = row(title, &value);
        r.set_subtitle_selectable(true);
        details.add_row(&r);
    }
    let more = group("", "");
    more.add(&details);
    page.add(&more);
    page
}

/// Mount every file at ~/Omacloud (`omacloud mount`, kept running as a
/// user unit after the app closes), then open it.
fn browse_online(ui: &Rc<Ui>) {
    let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Omacloud");
    let mut argv = owned(&["systemd-run", "--user", "--unit=omacloud-mount", "--collect", "--quiet"]);
    argv.push(std::env::var("OMACLOUD_BIN").unwrap_or_else(|_| "omacloud".into()));
    if let Ok(config) = std::env::var("OMACLOUD_CONFIG") {
        argv.extend(["--config".to_string(), config]);
    }
    argv.extend(["mount".to_string(), dir.to_string_lossy().into_owned()]);
    let (ui, dir2) = (ui.clone(), dir.clone());
    ui.toast("Opening every file…");
    in_background(
        move || {
            if !mounted(&dir2) {
                run(&argv)?;
                let start = std::time::Instant::now();
                while !mounted(&dir2) {
                    if start.elapsed().as_secs() > 60 {
                        return Err(Failure::new(
                            "couldn't show every file: `journalctl --user -u omacloud-mount` says why",
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
            Ok(())
        },
        move |r| match r {
            Ok(()) => gtk::FileLauncher::new(Some(&gio::File::for_path(&dir))).launch(
                Some(&ui.window),
                None::<&gio::Cancellable>,
                |_| {},
            ),
            Err(e) => ui.toast(&e.shown()),
        },
    );
}

/// Whether a filesystem is mounted at `dir`.
fn mounted(dir: &std::path::Path) -> bool {
    std::fs::read_to_string("/proc/self/mountinfo").is_ok_and(|m| {
        m.lines()
            .filter_map(|l| l.split(' ').nth(4))
            .any(|p| std::path::Path::new(p) == dir)
    })
}

/// Files deleted in the last 30 days, each with a button that writes its
/// last version back where it was.
fn show_deleted(ui: &Rc<Ui>) {
    let page = adw::PreferencesPage::new();
    let list = group("", "Looking through your history…");
    page.add(&list);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title("Recently Deleted")
        .content_width(560)
        .content_height(520)
        .child(&view)
        .build();
    dialog.present(Some(&ui.window));
    let ui = ui.clone();
    cli_then(&["omacloud", "deleted"], Vec::new(), move |r| {
        let files = match r {
            Ok(Value::Array(files)) => files,
            Ok(_) => Vec::new(),
            Err(e) => {
                list.set_description(Some(&e.shown()));
                return;
            }
        };
        list.set_description(Some(if files.is_empty() {
            "Nothing was deleted in the last 30 days."
        } else {
            "Put Back writes the last version where it was. Older versions stay under `omacloud versions`."
        }));
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
                            list.remove(&rw);
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
            list.add(&rw);
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
fn subtitle_with_size(row: &adw::ActionRow, path: &str) {
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
fn switch_row(title: &str, on: bool, flip: impl Fn(bool) + 'static) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let switch = gtk::Switch::builder().active(on).valign(gtk::Align::Center).build();
    switch.connect_state_set(move |_, want| {
        flip(want);
        // the state follows once the change is made and the page refreshed
        glib::Propagation::Stop
    });
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));
    row
}

/// What syncs on this computer: the usual folders of home, the ones the
/// account added, and Omarchy settings, each with a switch, as iCloud lists
/// what it keeps in step.
fn what_syncs(ui: &Rc<Ui>, s: &Value) -> adw::PreferencesGroup {
    let g = group(
        "On this computer",
        "Switch a folder off to keep its files here and stop syncing it on this computer; \
         your other computers keep it. Anything named like name.nosync stays on this \
         computer only.",
    );
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
    // the usual order, then the account's own additions
    let order = |n: &str| {
        ["Desktop", "Documents", "Pictures", "Music", "Videos", "Downloads"]
            .iter()
            .position(|u| *u == n)
            .unwrap_or(99)
    };
    rows.sort_by(|a, b| order(&a.0).cmp(&order(&b.0)).then(a.0.cmp(&b.0)));
    for (name, path, state) in rows {
        let (ui2, name2) = (ui.clone(), name.clone());
        let r = switch_row(&name, state == "synced", move |want| {
            let name = name2.clone();
            if want {
                let verb = if state == "skipped" { "unskip" } else { "add" };
                ui2.act(&["omacloud", "folders", verb, &name], "Syncing");
            } else {
                let ui3 = ui2.clone();
                let ui4 = ui2.clone();
                let dialog = adw::AlertDialog::new(
                    Some(&format!("Stop syncing {name} here?")),
                    Some("Its files stay on this computer, and your other computers keep syncing it."),
                );
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("skip", "Stop Syncing");
                dialog.set_response_appearance("skip", adw::ResponseAppearance::Destructive);
                dialog.set_close_response("cancel");
                dialog.connect_response(None, move |_, id| {
                    if id == "skip" {
                        ui3.act(&["omacloud", "folders", "skip", &name], "Stopped syncing here");
                    } else {
                        ui4.refresh();
                    }
                });
                dialog.present(Some(&ui2.window));
            }
        });
        subtitle_with_size(&r, &path);
        g.add(&r);
    }
    // every other folder of home, a switch each
    let more = folders["more"].as_array().unwrap_or(&empty);
    if !more.is_empty() {
        let x = adw::ExpanderRow::builder()
            .title("Other folders in your home")
            .subtitle("Switch one on to sync it on all your computers")
            .build();
        for f in more {
            let (name, ui2) = (text(&f["name"]), ui.clone());
            let name2 = name.clone();
            let r = switch_row(&name, false, move |_| {
                ui2.act(&["omacloud", "folders", "add", &name2], "Syncing");
            });
            r.set_subtitle(&short_path(&text(&f["path"])));
            x.add_row(&r);
        }
        g.add(&x);
    }
    // Omarchy settings, beside the folders, as iCloud lists more than files
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
    g.add(&settings);

    g
}

/// The setup page's choice of what syncs, as iCloud asks on a new Mac:
/// switches for the usual folders and Omarchy settings, with their sizes.
/// Returns the group and what to add to `omacloud init`.
fn choose_what_syncs() -> (adw::PreferencesGroup, impl Fn() -> Vec<String>) {
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
        let row = adw::SwitchRow::builder().title(name).active(on).build();
        subtitle_with_size(row.upcast_ref(), &path);
        g.add(&row);
        switches.push((name, row));
    }
    let settings = adw::SwitchRow::builder()
        .title("Omarchy settings")
        .subtitle(SETTINGS_SYNCED)
        .active(true)
        .build();
    g.add(&settings);
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

fn devices(ui: &Rc<Ui>, s: &Value) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
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
                    &[("approve", "Approve", adw::ResponseAppearance::Suggested)],
                    move |_| ui3.act(&["omacloud", "devices", "approve", &fp2], "Approved"),
                );
            });
            rw.add_suffix(&approve);
            g.add(&rw);
        }
        page.add(&g);
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
        g.set_header_suffix(Some(&add));
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
                        ("remove", "Remove", adw::ResponseAppearance::Default),
                        ("lost", "It's Lost or Stolen", adw::ResponseAppearance::Destructive),
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
        g.add(&rw);
    }
    page.add(&g);

    let removed = list("removed");
    if !removed.is_empty() {
        let g = group("Removed", "");
        for d in removed {
            g.add(&row(&text(&d["name"]), &text(&d["fingerprint"])));
        }
        page.add(&g);
    }
    page
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

fn settings(ui: &Rc<Ui>, s: &Value) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let st = &s["settings"];
    let on = st["on"].as_bool().unwrap_or(false);
    let g = group(
        "Omarchy settings",
        "The shared files in Omarchy's dots manifest (bindings, look and feel, terminals, \
         shell), AI agents' settings and skills, Chromium's bookmarks and settings, and \
         saved Wi-Fi networks. Machine specific ones, like monitors, and sign-in files stay \
         on each machine; API keys written into settings files sync with them, encrypted.",
    );
    let switch = adw::SwitchRow::builder()
        .title("Sync settings on this device")
        .active(on)
        .build();
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
    g.add(&switch);
    if let Some(why) = st["dormant"].as_str().filter(|_| on) {
        g.add(&row("Standing down", why));
    }
    page.add(&g);

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
        page.add(&g);
    }

    let empty = Vec::new();
    let held = st["held"].as_array().unwrap_or(&empty);
    if !held.is_empty() {
        let home = std::env::var("HOME").unwrap_or_default();
        let g = group(
            "Changed on two machines",
            "These changed here and on another device in ways that don't merge. \
             This machine keeps its version until you choose.",
        );
        for p in held {
            let rel = text(p);
            let rw = row(&format!("~/{rel}"), "");
            let path = format!("{home}/{rel}");
            for (label, keep) in [("Keep Mine", "local"), ("Keep Theirs", "remote")] {
                let b = button(label, Some("flat"));
                let (ui, path) = (ui.clone(), path.clone());
                b.connect_clicked(move |_| {
                    ui.act(&["omacloud", "settings", "resolve", &path, "--keep", keep], "Resolved");
                });
                rw.add_suffix(&b);
            }
            g.add(&rw);
        }
        page.add(&g);
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
fn setting_group(ui: &Rc<Ui>, item: &Value) -> gtk::Widget {
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
        return r.upcast();
    }
    let x = adw::ExpanderRow::builder()
        .title(text(&item["title"]))
        .subtitle(text(&item["detail"]))
        .title_selectable(false)
        .build();
    x.set_use_markup(false);
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
        x.add_row(&r);
    }
    for n in not_synced {
        x.add_row(&row("Not synced", &text(n)));
    }
    x.upcast()
}

fn secrets(ui: &Rc<Ui>, s: &Value) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    if s["settings"]["on"].as_bool() != Some(true) {
        let g = group(
            "Secrets",
            "ssh and gpg keys and tokens travel with settings sync. Turn it on under Omarchy.",
        );
        page.add(&g);
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
                &[("save", "Save", adw::ResponseAppearance::Suggested)],
                move |_| ui2.act(&["omacloud", "secrets", "save"], "Secrets saved"),
            );
        });
    }
    rw.add_suffix(&save);
    g.add(&rw);
    page.add(&g);

    let empty = Vec::new();
    let saved = s["secrets"].as_array().unwrap_or(&empty);
    let g = group(
        "Saved",
        if saved.is_empty() {
            "No device has saved its secrets yet."
        } else {
            "Restoring asks for your recovery code and shows what it will write first."
        },
    );
    for d in saved {
        let device = text(d);
        let rw = row(&device, "");
        let restore = button("Restore…", Some("flat"));
        let ui = ui.clone();
        restore.connect_clicked(move |_| restore_secrets(&ui, &device));
        rw.add_suffix(&restore);
        g.add(&rw);
    }
    page.add(&g);
    page
}

/// A recovery code entry in a boxed list, for a dialog's extra child.
fn code_entry() -> (gtk::ListBox, adw::PasswordEntryRow) {
    let entry = adw::PasswordEntryRow::builder().title("Recovery code").build();
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list.append(&entry);
    (list, entry)
}

fn restore_secrets(ui: &Rc<Ui>, device: &str) {
    let (list, entry) = code_entry();
    let dialog = adw::AlertDialog::new(
        Some(&format!("Restore {device}'s secrets?")),
        Some("Enter your recovery code. Nothing is written until you confirm."),
    );
    dialog.set_extra_child(Some(&list));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("next", "Continue");
    dialog.set_response_appearance("next", adw::ResponseAppearance::Suggested);
    dialog.set_close_response("cancel");
    let (ui2, device) = (ui.clone(), device.to_string());
    dialog.connect_response(Some("next"), move |_, _| {
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
                        &[("write", "Write", adw::ResponseAppearance::Destructive)],
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
    dialog.present(Some(&ui.window));
}

fn recovery() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.add(&group(
        "If you lose everything",
        "If you lose your recovery code and every computer, your files can't be recovered. \
         Keep a copy of the recovery code with someone you trust, or split it among several \
         people with `omacloud recovery split`.",
    ));
    page
}

/// Show text to copy.
fn show_text(ui: &Rc<Ui>, heading: &str, body: &str, content: &str) {
    let view = gtk::TextView::builder()
        .monospace(true)
        .editable(false)
        .wrap_mode(gtk::WrapMode::Char)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    view.buffer().set_text(content);
    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&view));
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.set_extra_child(Some(&frame));
    dialog.add_response("close", "Close");
    dialog.add_response("copy", "Copy");
    dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
    let (window, content) = (ui.window.clone(), content.to_string());
    dialog.connect_response(Some("copy"), move |_, _| {
        window.clipboard().set_text(&content);
    });
    dialog.present(Some(&ui.window));
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
    fn only_chromium_waits_for_chromium() {
        assert_eq!(
            super::state_label("chromium", "waiting"),
            "Waiting for Chromium to close"
        );
        assert_eq!(super::state_label("wifi", "waiting"), "Waiting to apply");
        assert_eq!(super::state_label("wifi", "settled"), "Edits settled");
    }
}
