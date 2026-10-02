//! The OneCloud app: account, devices, settings sync, secrets and
//! social recovery. It works through the `onecloud` CLI (`status --json` to
//! read, the usual subcommands to act), so it holds no keys or state of its
//! own and never races the daemon over them.
//!
//! `onecloud-app [overview|devices|settings|secrets|recovery]` opens on
//! that page. `ONECLOUD_BIN` picks the CLI (default: `onecloud` on PATH) and
//! `ONECLOUD_CONFIG` a config other than the default.

use std::{
    io::Write,
    process::{Command, Stdio},
    rc::Rc,
};

use adw::prelude::*;
use gtk::{gio, glib};
use serde_json::Value;

const APP_ID: &str = "computer.onecloud.OneCloud";

fn main() -> glib::ExitCode {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let page = args.next();
    let app = adw::Application::builder().application_id(APP_ID).build();
    // one window: launching again brings it forward instead of opening another
    app.connect_activate(move |app| match app.active_window() {
        Some(window) => window.present(),
        None => build(app, page.as_deref()),
    });
    // the page is ours to read; GTK sees no arguments
    app.run_with_args(&[argv0])
}

/// Run a command to completion: `onecloud ...` means the CLI with this
/// app's config. Returns stdout, or the last line of stderr on failure.
fn run(argv: &[String], env: &[(String, String)], stdin: Option<&str>) -> Result<String, String> {
    let (program, args) = argv.split_first().ok_or("nothing to run")?;
    let mut cmd = if program == "onecloud" {
        let mut c = Command::new(std::env::var("ONECLOUD_BIN").unwrap_or_else(|_| program.clone()));
        if let Ok(config) = std::env::var("ONECLOUD_CONFIG") {
            c.arg("--config").arg(config);
        }
        c
    } else {
        Command::new(program)
    };
    cmd.args(args)
        .env("RUST_LOG", "warn")
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("running {program}: {e}"))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        _ = pipe.write_all(text.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(err
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("it failed")
        .trim_start_matches("Error: ")
        .to_string())
}

/// Run in the background, then hand the result to `then` on the main loop.
fn run_then(
    argv: &[&str],
    env: Vec<(String, String)>,
    stdin: Option<String>,
    then: impl FnOnce(Result<String, String>) + 'static,
) {
    let argv: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || run(&argv, &env, stdin.as_deref()))
            .await
            .unwrap_or_else(|_| Err("it crashed".into()));
        then(result);
    });
}

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    overview: adw::Bin,
    devices: adw::Bin,
    settings: adw::Bin,
    secrets: adw::Bin,
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
        run_then(argv, env, None, move |r| {
            ui.toast(&r.map_or_else(|e| e, |_| done));
            ui.refresh();
        });
    }

    fn refresh(self: &Rc<Self>) {
        let ui = self.clone();
        run_then(
            &["onecloud", "status", "--json"],
            Vec::new(),
            None,
            move |r| {
                let status = r
                    .and_then(|out| serde_json::from_str::<Value>(&out).map_err(|e| e.to_string()));
                match status {
                    // no config yet: this computer needs setting up
                    Ok(s) if s["set_up"] == Value::Bool(false) => {
                        ui.overview.set_child(Some(&setup(&ui)));
                        for bin in [&ui.devices, &ui.settings, &ui.secrets] {
                            bin.set_child(Some(&not_set_up(
                                "Set up this computer first, under Overview.",
                            )));
                        }
                    }
                    Ok(s) => {
                        ui.overview.set_child(Some(&overview(&ui, &s)));
                        ui.devices.set_child(Some(&devices(&ui, &s)));
                        ui.settings.set_child(Some(&settings(&ui, &s)));
                        ui.secrets.set_child(Some(&secrets(&ui, &s)));
                    }
                    Err(e) => {
                        for bin in [&ui.overview, &ui.devices, &ui.settings, &ui.secrets] {
                            bin.set_child(Some(&not_set_up(&e)));
                        }
                    }
                }
            },
        );
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
    let (overview_bin, devices_bin, settings_bin, secrets_bin) = (bin(), bin(), bin(), bin());
    stack.add_titled_with_icon(
        &overview_bin,
        Some("overview"),
        "Overview",
        "folder-remote-symbolic",
    );
    stack.add_titled_with_icon(
        &devices_bin,
        Some("devices"),
        "Devices",
        "computer-symbolic",
    );
    stack.add_titled_with_icon(
        &settings_bin,
        Some("settings"),
        "Omarchy",
        "preferences-desktop-symbolic",
    );
    stack.add_titled_with_icon(
        &secrets_bin,
        Some("secrets"),
        "Keys",
        "dialog-password-symbolic",
    );

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
        .title("OneCloud")
        .default_width(1000)
        .default_height(640)
        .content(&view)
        .width_request(360)
        .height_request(400)
        .build();
    let narrow = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 900sp").expect("a breakpoint condition"),
    );
    narrow.add_setter(
        &header,
        "title-widget",
        Some(&adw::WindowTitle::new("OneCloud", "").to_value()),
    );
    narrow.add_setter(&bottom, "reveal", Some(&true.to_value()));
    window.add_breakpoint(narrow);
    let ui = Rc::new(Ui {
        window: window.clone(),
        toasts,
        overview: overview_bin,
        devices: devices_bin,
        settings: settings_bin,
        secrets: secrets_bin,
    });
    stack.add_titled_with_icon(
        &recovery(&ui),
        Some("recovery"),
        "Recovery",
        "system-users-symbolic",
    );
    {
        let ui = ui.clone();
        sync.connect_clicked(move |_| ui.act(&["onecloud", "sync"], "Synced"));
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
    let b = gtk::Button::builder()
        .label(label)
        .valign(gtk::Align::Center)
        .build();
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

/// A labelled field in a boxed list.
fn entry(title: &str) -> adw::EntryRow {
    adw::EntryRow::builder().title(title).build()
}

/// Run `onecloud init` with `args` (and secrets in `env`, never on the
/// command line), then show what came of it.
fn run_init(ui: &Rc<Ui>, args: Vec<String>, env: Vec<(String, String)>) {
    let mut argv = vec!["onecloud".to_string(), "init".to_string()];
    argv.extend(args);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    let ui2 = ui.clone();
    ui.toast("Setting up…");
    run_then(&argv_ref, env, None, move |r| match r {
        Err(e) => ui2.toast(&e),
        Ok(out) => {
            // set up: keep it syncing from now on, not from the next login
            run_then(
                &["systemctl", "--user", "start", "onecloud"],
                Vec::new(),
                None,
                |_| {},
            );
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
                let check = adw::EntryRow::builder()
                    .title("Type its last four characters")
                    .build();
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
                    let chars: Vec<char> =
                        code.chars().filter(char::is_ascii_alphanumeric).collect();
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
                    "On a computer that already syncs, open onecloud's Devices and approve the one \
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

/// Where a new self-hosted account's bucket lives: what to ask for and how
/// to get it.
struct Provider {
    name: &'static str,
    steps: &'static str,
    link: &'static str,
}

const PROVIDERS: [Provider; 4] = [
    Provider {
        name: "Hetzner Object Storage",
        steps: "1. In the Hetzner Console, open Object Storage and create a bucket with any \
                name. Leave the other settings as they are (Object Lock stays disabled).\n\
                2. Under Security, S3 credentials, generate credentials and \
                copy both keys. Hetzner shows the secret key only once.",
        link: "https://console.hetzner.com/projects",
    },
    Provider {
        name: "Cloudflare R2",
        steps: "1. In the Cloudflare dashboard, open R2 and create a bucket.\n2. Under API tokens, \
                create a token with Object Read & Write on that bucket and copy its Access Key ID \
                and Secret Access Key. Your account ID is on the R2 overview page.",
        link: "https://dash.cloudflare.com/?to=/:account/r2/overview",
    },
    Provider {
        name: "Backblaze B2",
        steps: "1. In Backblaze, create a bucket (Files in Bucket: Private) and note its S3 endpoint, like \
                s3.eu-central-003.backblazeb2.com.\n2. Under Application Keys, add a key with read \
                and write access to that bucket only, and copy its keyID and applicationKey.",
        link: "https://secure.backblaze.com/b2_buckets.htm",
    },
    Provider {
        name: "Other S3 storage",
        steps: "Any S3-compatible storage works, such as MinIO. You need its endpoint, its region, \
                a bucket, and a key that can read and write that bucket.",
        link: "",
    },
];

const HETZNER_LOCATIONS: [(&str, &str); 3] = [
    ("Falkenstein", "fsn1"),
    ("Nuremberg", "nbg1"),
    ("Helsinki", "hel1"),
];

/// The endpoint and region for a provider's answers, or what's missing.
fn s3_location(
    provider: usize,
    location: usize,
    account: &str,
    eu: bool,
    endpoint: &str,
    region: &str,
) -> Result<(String, String), &'static str> {
    let with_scheme = |e: &str| {
        if e.starts_with("https://") || e.starts_with("http://") {
            e.to_string()
        } else {
            format!("https://{e}")
        }
    };
    match provider {
        0 => {
            let loc = HETZNER_LOCATIONS[location.min(2)].1;
            Ok((format!("https://{loc}.your-objectstorage.com"), loc.into()))
        }
        1 if account.is_empty() => Err("Fill in your Cloudflare account ID"),
        1 => Ok((
            format!(
                "https://{account}.{}r2.cloudflarestorage.com",
                if eu { "eu." } else { "" }
            ),
            "auto".into(),
        )),
        2 => {
            // s3.<region>.backblazeb2.com
            let host = endpoint
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            let region = host
                .strip_prefix("s3.")
                .and_then(|r| r.strip_suffix(".backblazeb2.com"))
                .ok_or("The endpoint looks like s3.eu-central-003.backblazeb2.com")?;
            Ok((with_scheme(host), region.into()))
        }
        _ if endpoint.is_empty() => Err("Fill in the endpoint"),
        _ => Ok((
            with_scheme(endpoint),
            if region.is_empty() { "auto" } else { region }.into(),
        )),
    }
}

/// First run: start an account, or add this computer to one.
fn setup(ui: &Rc<Ui>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let (what, sync_args) = choose_what_syncs();
    page.add(&what);

    // create, own bucket
    let own = group(
        "Start a new account",
        "OneCloud keeps your files in a storage bucket you rent from a provider. You pay the \
         provider for the space you use; OneCloud itself charges nothing. Everything is \
         encrypted on this computer before it leaves, so the provider can't read it.",
    );
    let names: Vec<&str> = PROVIDERS.iter().map(|p| p.name).collect();
    let provider = adw::ComboRow::builder()
        .title("Provider")
        .model(&gtk::StringList::new(&names))
        .build();
    own.add(&provider);
    let steps = adw::ActionRow::builder()
        .title("Before you start")
        .subtitle(PROVIDERS[0].steps)
        .subtitle_selectable(true)
        .build();
    let open = button("Open", Some("flat"));
    open.set_valign(gtk::Align::Center);
    steps.add_suffix(&open);
    own.add(&steps);
    let locations: Vec<&str> = HETZNER_LOCATIONS.iter().map(|l| l.0).collect();
    let location = adw::ComboRow::builder()
        .title("Location of the bucket")
        .model(&gtk::StringList::new(&locations))
        .build();
    let account = entry("Cloudflare account ID");
    let eu = adw::SwitchRow::builder()
        .title("The bucket is in the EU jurisdiction")
        .build();
    let endpoint = entry("Endpoint");
    let region = entry("Region (blank for auto)");
    let bucket = entry("Bucket name");
    let key_id = entry("Access key ID");
    let key = adw::PasswordEntryRow::builder()
        .title("Secret access key")
        .build();
    // a bucket with object lock can't take the create-only writes
    // coordination needs on some providers: it goes in a second bucket
    let advanced = adw::ExpanderRow::builder()
        .title("The bucket has object lock")
        .subtitle("Then OneCloud needs a second, plain bucket for keeping your computers in step")
        .show_enable_switch(true)
        .enable_expansion(false)
        .build();
    let coord_bucket = entry("Second bucket's name");
    advanced.add_row(&coord_bucket);
    for w in [
        &location.clone().upcast::<gtk::Widget>(),
        account.upcast_ref(),
        eu.upcast_ref(),
        endpoint.upcast_ref(),
        region.upcast_ref(),
        bucket.upcast_ref(),
        key_id.upcast_ref(),
    ] {
        own.add(w);
    }
    own.add(&key);
    own.add(&advanced);
    let show = {
        let (steps, location, account, eu, endpoint, region, open) = (
            steps.clone(),
            location.clone(),
            account.clone(),
            eu.clone(),
            endpoint.clone(),
            region.clone(),
            open.clone(),
        );
        move |i: usize| {
            let p = &PROVIDERS[i.min(PROVIDERS.len() - 1)];
            steps.set_subtitle(p.steps);
            open.set_visible(!p.link.is_empty());
            location.set_visible(i == 0);
            account.set_visible(i == 1);
            eu.set_visible(i == 1);
            endpoint.set_visible(i >= 2);
            endpoint.set_title(if i == 2 {
                "S3 endpoint, like s3.eu-central-003.backblazeb2.com"
            } else {
                "Endpoint, like https://minio.example.com"
            });
            region.set_visible(i == 3);
        }
    };
    show(0);
    provider.connect_selected_notify(move |c| show(c.selected() as usize));
    {
        let (provider, window) = (provider.clone(), ui.window.clone());
        open.connect_clicked(move |_| {
            let link = PROVIDERS[(provider.selected() as usize).min(PROVIDERS.len() - 1)].link;
            gtk::UriLauncher::new(link).launch(Some(&window), None::<&gio::Cancellable>, |_| {});
        });
    }
    let create_own = button("Create Account", Some("suggested-action"));
    create_own.set_halign(gtk::Align::End);
    create_own.set_margin_top(12);
    {
        let (ui, sync_args) = (ui.clone(), sync_args.clone());
        create_own.connect_clicked(move |_| {
            let location = s3_location(
                provider.selected() as usize,
                location.selected() as usize,
                account.text().trim(),
                eu.is_active(),
                endpoint.text().trim(),
                region.text().trim(),
            );
            let (e, r) = match location {
                Ok(l) => l,
                Err(why) => {
                    ui.toast(why);
                    return;
                }
            };
            let (b, k, sk) = (
                bucket.text().trim().to_string(),
                key_id.text().trim().to_string(),
                key.text().trim().to_string(),
            );
            if [&b, &k, &sk].iter().any(|v| v.is_empty()) {
                ui.toast("Fill in the bucket name and both keys");
                return;
            }
            let mut args = vec!["--repo".to_string(), "opendal:s3".to_string()];
            for (name, value) in [
                ("endpoint", e),
                ("bucket", b),
                ("region", r),
                ("access_key_id", k),
            ] {
                args.push("--opt".into());
                args.push(format!("{name}={value}"));
            }
            // self-hosted: coordination lives in the bucket too, or in a
            // second one beside a bucket with object lock
            args.push("--coordinator".into());
            args.push("bucket".into());
            let cb = coord_bucket.text().trim().to_string();
            if advanced.enables_expansion() {
                if cb.is_empty() {
                    ui.toast("Fill in the second bucket's name, or turn off object lock");
                    return;
                }
                args.push("--coordination-bucket".into());
                args.push(cb);
            }
            args.extend(sync_args());
            // the secret through the environment: a command line is readable
            // by other processes
            run_init(
                &ui,
                args,
                vec![("ONECLOUD_SECRET_ACCESS_KEY".to_string(), sk)],
            );
        });
    }
    own.add(&create_own);
    page.add(&own);

    // join
    let join = group(
        "Add this computer to your account",
        "Already use OneCloud on another computer? On that computer, open OneCloud, go to \
         Devices and click Show Join Code, then paste the code here. That computer approves \
         this one, or your recovery code lets it in right away.",
    );
    let jcode = adw::PasswordEntryRow::builder().title("Join code").build();
    let code = adw::PasswordEntryRow::builder()
        .title("Recovery code (optional)")
        .build();
    join.add(&jcode);
    join.add(&code);
    let join_button = button("Join", Some("suggested-action"));
    join_button.set_halign(gtk::Align::End);
    join_button.set_margin_top(12);
    {
        let ui = ui.clone();
        join_button.connect_clicked(move |_| {
            let (j, c) = (jcode.text().trim().to_string(), code.text().to_string());
            let mut env = Vec::new();
            if !j.is_empty() {
                env.push(("ONECLOUD_JOIN_CODE".to_string(), j));
            }
            if !c.is_empty() {
                env.push(("ONECLOUD_RECOVERY_CODE".to_string(), c));
            }
            if env.first().is_none_or(|(k, _)| k != "ONECLOUD_JOIN_CODE") {
                ui.toast("Paste the join code from one of your computers");
                return;
            }
            run_init(&ui, sync_args(), env);
        });
    }
    join.add(&join_button);
    page.add(&join);
    page
}

fn overview(ui: &Rc<Ui>, s: &Value) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();

    let sync = group("Sync", "");
    let open_button = |path: String| {
        let open = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("Open")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let window = ui.window.clone();
        open.connect_clicked(move |_| {
            gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(
                Some(&window),
                None::<&gio::Cancellable>,
                |_| {},
            );
        });
        open
    };
    let folders = &s["folders"];
    let mut folders_group = None;
    let mut folder_group_row = None;
    if folders.is_object() {
        folders_group = Some(what_syncs(ui, s));
    } else {
        let folder = text(&s["folder"]);
        let folder_row = row("Folder", &folder);
        folder_row.add_suffix(&open_button(folder));
        folder_group_row = Some(folder_row);
    }
    let daemon = s["daemon"].as_bool().unwrap_or(false);
    let synced = s["synced_at"].as_u64().map_or_else(
        || "Not synced yet".to_string(),
        |t| format!("Synced {}", ago(t)),
    );
    let status = row(
        &synced,
        if daemon {
            "Changes sync as they happen"
        } else {
            "Background sync is off: nothing syncs until it starts"
        },
    );
    if !daemon {
        let start = button("Start", Some("suggested-action"));
        let ui2 = ui.clone();
        start.connect_clicked(move |_| {
            ui2.act(&["systemctl", "--user", "start", "onecloud"], "Started");
        });
        status.add_suffix(&start);
    }
    sync.add(&status);
    if let Some(r) = folder_group_row {
        sync.add(&r);
    }
    page.add(&sync);
    if let Some(g) = folders_group {
        page.add(&g);
    }

    let storage = group("Storage", "");
    let repo = &s["repo"];
    let place = match repo["kind"].as_str() {
        Some("bucket") => {
            let host = text(&repo["endpoint"]);
            let host = host
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            format!("Your bucket {} at {host}", text(&repo["bucket"]))
        }
        _ => "A folder or server of your own".to_string(),
    };
    storage.add(&row("Where", place.trim()));
    page.add(&storage);

    // for support and the curious
    let details = adw::ExpanderRow::builder().title("Details").build();
    for (title, value) in [
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

/// "2 minutes ago", for a time in seconds since the epoch.
fn ago(t: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let s = now.saturating_sub(t);
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
    run_then(&["du", "-sb", path], Vec::new(), None, move |r| {
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
    let switch = gtk::Switch::builder()
        .active(on)
        .valign(gtk::Align::Center)
        .build();
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
    let home = std::env::var("HOME").unwrap_or_default();
    let mut rows: Vec<(String, String, &'static str)> = Vec::new();
    for f in folders["synced"].as_array().unwrap_or(&empty) {
        rows.push((text(&f["name"]), text(&f["path"]), "synced"));
    }
    for f in folders["skipped"].as_array().unwrap_or(&empty) {
        let name = text(f);
        let path = format!("{home}/{name}");
        rows.push((name, path, "skipped"));
    }
    for f in folders["others"].as_array().unwrap_or(&empty) {
        rows.push((text(&f["name"]), text(&f["path"]), "off"));
    }
    // the usual order, then the account's own additions
    let order = |n: &str| {
        [
            "Desktop",
            "Documents",
            "Pictures",
            "Music",
            "Videos",
            "Downloads",
        ]
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
                ui2.act(&["onecloud", "folders", verb, &name], "Syncing");
            } else {
                let ui3 = ui2.clone();
                let ui4 = ui2.clone();
                let dialog = adw::AlertDialog::new(
                    Some(&format!("Stop syncing {name} here?")),
                    Some(
                        "Its files stay on this computer, and your other computers keep syncing it.",
                    ),
                );
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("skip", "Stop Syncing");
                dialog.set_response_appearance("skip", adw::ResponseAppearance::Destructive);
                dialog.set_close_response("cancel");
                dialog.connect_response(None, move |_, id| {
                    if id == "skip" {
                        ui3.act(
                            &["onecloud", "folders", "skip", &name],
                            "Stopped syncing here",
                        );
                    } else {
                        ui4.refresh();
                    }
                });
                dialog.present(Some(&ui2.window));
            }
        });
        subtitle_with_size(&r, &path);
        if state == "synced" {
            let open = gtk::Button::builder()
                .icon_name("folder-open-symbolic")
                .tooltip_text("Open")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let (window, path) = (ui.window.clone(), path.clone());
            open.connect_clicked(move |_| {
                gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(
                    Some(&window),
                    None::<&gio::Cancellable>,
                    |_| {},
                );
            });
            r.add_prefix(&open);
        }
        g.add(&r);
    }
    // Omarchy settings, beside the folders, as iCloud lists more than files
    let settings_on = s["settings"]["on"].as_bool().unwrap_or(false);
    let ui2 = ui.clone();
    let settings = switch_row("Omarchy settings", settings_on, move |want| {
        let verb = if want { "on" } else { "off" };
        ui2.act(
            &["onecloud", "settings", verb],
            if want {
                "Omarchy settings sync"
            } else {
                "Omarchy settings stay here"
            },
        );
    });
    settings.set_subtitle("Bindings, themes, terminal and shell setup");
    g.add(&settings);

    let add = button("Add Folder…", Some("flat"));
    let ui2 = ui.clone();
    add.connect_clicked(move |_| {
        let dialog = gtk::FileDialog::builder()
            .title("Choose a folder in your home")
            .modal(true)
            .build();
        let home = std::env::var("HOME").unwrap_or_default();
        dialog.set_initial_folder(Some(&gio::File::for_path(&home)));
        let ui3 = ui2.clone();
        dialog.select_folder(Some(&ui2.window), None::<&gio::Cancellable>, move |r| {
            let Some(path) = r.ok().and_then(|f| f.path()) else {
                return;
            };
            match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) if parent == std::path::Path::new(&home) => {
                    let name = name.to_string_lossy().to_string();
                    ui3.act(&["onecloud", "folders", "add", &name], "Syncing");
                }
                _ => ui3.toast("Choose a folder directly in your home, like ~/Projects"),
            }
        });
    });
    g.set_header_suffix(Some(&add));
    g
}

/// The setup page's choice of what syncs, as iCloud asks on a new Mac:
/// switches for the usual folders and Omarchy settings, with their sizes.
/// Returns the group and what to add to `onecloud init`.
fn choose_what_syncs() -> (adw::PreferencesGroup, Rc<dyn Fn() -> Vec<String>>) {
    let g = group(
        "What to sync",
        "Change any of these later on Overview. Anything named like name.nosync stays on \
         this computer only.",
    );
    let home = std::env::var("HOME").unwrap_or_default();
    // where this computer keeps each (a German one's Documents is Dokumente)
    // None when this computer points the folder at home itself, as Omarchy
    // does with Desktop: there's nothing to sync
    let local = |name: &str| -> Option<String> {
        let key = match name {
            "Downloads" => "DOWNLOAD".to_string(),
            other => other.to_uppercase(),
        };
        let dirs =
            std::fs::read_to_string(format!("{home}/.config/user-dirs.dirs")).unwrap_or_default();
        let set = dirs
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix(&format!("XDG_{key}_DIR="))
                    .map(str::to_string)
            })
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
        .subtitle("Bindings, themes, terminal and shell setup")
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
    (g, Rc::new(args))
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
                    move |_| ui3.act(&["onecloud", "devices", "approve", &fp2], "Approved"),
                );
            });
            rw.add_suffix(&approve);
            g.add(&rw);
        }
        page.add(&g);
    }

    let self_hosted = s["self_hosted"].as_bool().unwrap_or(false);
    let me = &s["this_device"];
    let g = group(
        "Devices",
        if me["role"] == "waiting" {
            "This device is waiting for approval."
        } else {
            ""
        },
    );
    if self_hosted && me["role"] == "member" {
        let add = button("Show Join Code", Some("flat"));
        let ui2 = ui.clone();
        add.connect_clicked(move |_| show_join_code(&ui2));
        g.set_header_suffix(Some(&add));
    }
    for d in list("devices") {
        let (name, fp) = (text(&d["name"]), text(&d["fingerprint"]));
        let rw = row(&name, &fp);
        if d["this"].as_bool() == Some(true) {
            let tag = gtk::Label::new(Some("This device"));
            tag.add_css_class("dim-label");
            rw.add_suffix(&tag);
        } else if self_hosted {
            let remove = button("Remove…", Some("flat"));
            let ui2 = ui.clone();
            remove.connect_clicked(move |_| {
                let ui3 = ui2.clone();
                let fp2 = fp.clone();
                ui2.confirm(
                    &format!("Remove {name}?"),
                    "It can't sync anymore, but it still holds your bucket's key. To lock it out, \
                     make a new key at your provider and switch every computer to it: that's the \
                     next step.",
                    &[(
                        "remove",
                        "Remove and Change Key…",
                        adw::ResponseAppearance::Destructive,
                    )],
                    move |_| {
                        let ui4 = ui3.clone();
                        run_then(
                            &["onecloud", "devices", "revoke", &fp2],
                            Vec::new(),
                            None,
                            move |r| match r {
                                Err(e) => ui4.toast(&e),
                                Ok(_) => {
                                    ui4.refresh();
                                    change_bucket_key(&ui4);
                                }
                            },
                        );
                    },
                );
            });
            rw.add_suffix(&remove);
        } else {
            let remove = button("Remove…", Some("flat"));
            let ui2 = ui.clone();
            remove.connect_clicked(move |_| {
                let ui3 = ui2.clone();
                let fp2 = fp.clone();
                ui2.confirm(
                    &format!("Remove {name}?"),
                    "It can't sync anymore. It still holds the key to what it has already seen; \
                     changing the key locks it out of that too, but uploads everything again.",
                    &[
                        ("remove", "Remove", adw::ResponseAppearance::Destructive),
                        (
                            "rotate",
                            "Remove and Change Key",
                            adw::ResponseAppearance::Destructive,
                        ),
                    ],
                    move |id| {
                        let mut argv = vec!["onecloud", "devices", "revoke", fp2.as_str()];
                        if id == "rotate" {
                            argv.push("--rotate");
                        }
                        ui3.act(&argv, "Removed");
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
    if self_hosted {
        page.add(&bucket_key(ui, &s["bucket_key"]));
    }
    page
}

/// A join code for adding another computer to this account.
fn show_join_code(ui: &Rc<Ui>) {
    let ui2 = ui.clone();
    run_then(
        &["onecloud", "join-code"],
        Vec::new(),
        None,
        move |r| match r {
            Err(e) => ui2.toast(&e),
            Ok(out) => show_text(
                &ui2,
                "Join code",
                "On the new computer, install OneCloud, open it, and paste this under Add this \
             computer to your account. Then approve it here. The code holds your bucket's key: \
             pass it only to your own computers, and not through chat or email.",
                out.trim(),
            ),
        },
    );
}

/// A self-hosted account's bucket key: every computer holds it, so a lost
/// one is locked out only by a new key, which the others switch to.
fn bucket_key(ui: &Rc<Ui>, status: &Value) -> adw::PreferencesGroup {
    let empty = Vec::new();
    let computers = status["computers"].as_array().unwrap_or(&empty);
    let waiting = computers
        .iter()
        .filter(|c| c["switched"].as_bool() != Some(true))
        .count();
    let description = if status.is_null() {
        "Every computer holds your bucket's key. To lock out a lost one, make a new key at \
         your provider and switch to it here."
            .to_string()
    } else if waiting == 0 {
        "Every computer is on the latest key. Delete the old one at your provider if you \
         haven't yet: until then, a removed computer can still reach the bucket."
            .to_string()
    } else {
        format!(
            "{waiting} of {} computers still to switch; each does on its next sync. Keep the \
             old key at your provider until then.",
            computers.len()
        )
    };
    let g = group("Bucket key", &description);
    // who has switched matters only while some haven't
    for c in computers.iter().filter(|_| waiting > 0) {
        let done = c["switched"].as_bool() == Some(true);
        g.add(&row(
            &text(&c["name"]),
            if done { "On the new key" } else { "Not yet" },
        ));
    }
    let change = button("Change Key…", Some("flat"));
    let ui2 = ui.clone();
    change.connect_clicked(move |_| change_bucket_key(&ui2));
    g.set_header_suffix(Some(&change));
    g
}

fn change_bucket_key(ui: &Rc<Ui>) {
    let id = adw::EntryRow::builder().title("New access key").build();
    let secret = adw::PasswordEntryRow::builder()
        .title("New secret key")
        .build();
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list.append(&id);
    list.append(&secret);
    let dialog = adw::AlertDialog::new(
        Some("Change the bucket key"),
        Some(
            "Make a new key at your provider for this bucket, and enter it here. Every computer \
             still on the account switches to it; removed ones get nothing. Keep the old key \
             until all have switched.",
        ),
    );
    dialog.set_extra_child(Some(&list));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("change", "Change Key");
    dialog.set_response_appearance("change", adw::ResponseAppearance::Suggested);
    dialog.set_close_response("cancel");
    let ui2 = ui.clone();
    dialog.connect_response(Some("change"), move |_, _| {
        let (id, secret) = (id.text().trim().to_string(), secret.text().to_string());
        if id.is_empty() || secret.is_empty() {
            ui2.toast("Enter the new key's id and secret");
            return;
        }
        ui2.act_env(
            &["onecloud", "bucket", "set-key", "--access-key-id", &id],
            vec![("ONECLOUD_SECRET_ACCESS_KEY".to_string(), secret)],
            "Key changed: the other computers switch on their next sync",
        );
    });
    dialog.present(Some(&ui.window));
}

fn settings(ui: &Rc<Ui>, s: &Value) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let st = &s["settings"];
    let on = st["on"].as_bool().unwrap_or(false);
    let g = group(
        "Omarchy settings",
        "The shared files in Omarchy's dots manifest: bindings, look and feel, terminals, \
         shell. Machine specific ones, like monitors, stay on each machine.",
    );
    let switch = adw::SwitchRow::builder()
        .title("Sync settings on this device")
        .active(on)
        .build();
    {
        let ui = ui.clone();
        switch.connect_active_notify(move |sw| {
            if sw.is_active() {
                ui.act(&["onecloud", "settings", "on"], "Settings sync is on");
            } else {
                ui.act(&["onecloud", "settings", "off"], "Settings sync is off");
            }
        });
    }
    g.add(&switch);
    if let Some(why) = st["dormant"].as_str().filter(|_| on) {
        g.add(&row("Standing down", why));
    }
    page.add(&g);

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
                    ui.act(
                        &["onecloud", "settings", "resolve", &path, "--keep", keep],
                        "Resolved",
                    );
                });
                rw.add_suffix(&b);
            }
            g.add(&rw);
        }
        page.add(&g);
    }
    page
}

fn secrets(ui: &Rc<Ui>, s: &Value) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    if s["settings"]["on"].as_bool() != Some(true) {
        let g = group(
            "Secrets",
            "ssh and gpg keys and tokens travel with settings sync. Turn it on under Settings.",
        );
        page.add(&g);
        return page;
    }
    let g = group(
        "This device",
        "Seal ~/.ssh, ~/.gnupg and token files (or what ~/.config/onecloud/secrets lists) \
         so they travel with your files. No device can open them: only your recovery code.",
    );
    let rw = row(
        "Save this device's secrets",
        "Replaces what was saved before",
    );
    let save = button("Save…", Some("suggested-action"));
    {
        let ui = ui.clone();
        save.connect_clicked(move |_| {
            let ui2 = ui.clone();
            ui.confirm(
                "Save secrets?",
                "They are sealed so that only your recovery code opens them.",
                &[("save", "Save", adw::ResponseAppearance::Suggested)],
                move |_| ui2.act(&["onecloud", "secrets", "save"], "Secrets saved"),
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
    let entry = adw::PasswordEntryRow::builder()
        .title("Recovery code")
        .build();
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
        let env = vec![("ONECLOUD_RECOVERY_CODE".to_string(), code.clone())];
        let (ui3, device2) = (ui2.clone(), device.clone());
        run_then(
            &["onecloud", "secrets", "restore", "--from", &device],
            env,
            None,
            move |r| match r {
                Err(e) => ui3.toast(&e),
                Ok(out) => {
                    let changes: Vec<&str> = out
                        .lines()
                        .filter(|l| l.starts_with("  "))
                        .map(str::trim)
                        .collect();
                    if changes.is_empty() {
                        ui3.toast(out.trim());
                        return;
                    }
                    let ui4 = ui3.clone();
                    ui3.confirm(
                        "Write these files?",
                        &format!(
                            "{}\n\nFiles this replaces are backed up first.",
                            changes.join("\n")
                        ),
                        &[("write", "Write", adw::ResponseAppearance::Destructive)],
                        move |_| {
                            let env = vec![("ONECLOUD_RECOVERY_CODE".to_string(), code.clone())];
                            let ui5 = ui4.clone();
                            run_then(
                                &[
                                    "onecloud", "secrets", "restore", "--from", &device2, "--yes",
                                ],
                                env,
                                None,
                                move |r| {
                                    ui5.toast(&r.map_or_else(
                                        |e| e,
                                        |out| {
                                            out.lines()
                                                .rev()
                                                .find(|l| l.starts_with("wrote"))
                                                .unwrap_or("Restored")
                                                .to_string()
                                        },
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

fn recovery(ui: &Rc<Ui>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();

    let split = group(
        "Share your recovery code",
        "Split it among people you trust. Any few of them together can rebuild it; \
         fewer learn nothing about it. Give each person one share.",
    );
    let threshold = adw::SpinRow::with_range(2.0, 10.0, 1.0);
    threshold.set_title("Shares needed");
    threshold.set_value(3.0);
    let shares = adw::SpinRow::with_range(2.0, 10.0, 1.0);
    shares.set_title("Shares");
    shares.set_value(5.0);
    let code = adw::PasswordEntryRow::builder()
        .title("Recovery code")
        .build();
    split.add(&threshold);
    split.add(&shares);
    split.add(&code);
    let make = button("Make Shares", Some("suggested-action"));
    make.set_halign(gtk::Align::End);
    make.set_margin_top(12);
    {
        let ui = ui.clone();
        make.connect_clicked(move |_| {
            let (k, n) = (threshold.value().to_string(), shares.value().to_string());
            let env = vec![(
                "ONECLOUD_RECOVERY_CODE".to_string(),
                code.text().to_string(),
            )];
            let ui2 = ui.clone();
            run_then(
                &[
                    "onecloud",
                    "recovery",
                    "split",
                    "--threshold",
                    &k,
                    "--shares",
                    &n,
                ],
                env,
                None,
                move |r| match r {
                    Err(e) => ui2.toast(&e),
                    Ok(out) => {
                        show_shares(&ui2, out.lines().filter(|l| !l.trim().is_empty()).collect())
                    }
                },
            );
        });
    }
    split.add(&make);
    page.add(&split);

    let combine = group(
        "Rebuild the recovery code",
        "Paste the shares you collected, one per line.",
    );
    let input = gtk::TextView::builder()
        .monospace(true)
        .wrap_mode(gtk::WrapMode::Char)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .height_request(120)
        .build();
    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&input));
    combine.add(&frame);
    let rebuild = button("Rebuild", None);
    rebuild.set_halign(gtk::Align::End);
    rebuild.set_margin_top(12);
    {
        let ui = ui.clone();
        rebuild.connect_clicked(move |_| {
            let buf = input.buffer();
            let pasted = buf
                .text(&buf.start_iter(), &buf.end_iter(), false)
                .to_string();
            let ui2 = ui.clone();
            run_then(
                &["onecloud", "recovery", "combine"],
                Vec::new(),
                Some(format!("{pasted}\n")),
                move |r| match r {
                    Err(e) => ui2.toast(&e),
                    Ok(out) => show_text(
                        &ui2,
                        "Your recovery code",
                        "Write it down and keep it offline.",
                        out.trim(),
                    ),
                },
            );
        });
    }
    combine.add(&rebuild);
    page.add(&combine);
    page
}

/// One row per share, each with its own copy button: each goes to a
/// different person.
fn show_shares(ui: &Rc<Ui>, shares: Vec<&str>) {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for (i, share) in shares.iter().enumerate() {
        let r = row(&format!("Share {}", i + 1), share);
        r.add_css_class("monospace");
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text("Copy")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let (window, share) = (ui.window.clone(), (*share).to_string());
        copy.connect_clicked(move |_| window.clipboard().set_text(&share));
        r.add_suffix(&copy);
        list.append(&r);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .propagate_natural_height(true)
        .max_content_height(420)
        .width_request(560)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let dialog = adw::AlertDialog::new(
        Some("Your shares"),
        Some("Give each to a different person. None of them can open your account alone."),
    );
    dialog.set_extra_child(Some(&scroll));
    dialog.add_response("close", "Done");
    dialog.present(Some(&ui.window));
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
    use super::s3_location;

    fn loc(
        p: usize,
        l: usize,
        account: &str,
        eu: bool,
        endpoint: &str,
        region: &str,
    ) -> (String, String) {
        s3_location(p, l, account, eu, endpoint, region).expect("a location")
    }

    #[test]
    fn hetzner_follows_the_location() {
        assert_eq!(
            loc(0, 0, "", false, "", ""),
            ("https://fsn1.your-objectstorage.com".into(), "fsn1".into())
        );
        assert_eq!(
            loc(0, 2, "", false, "", ""),
            ("https://hel1.your-objectstorage.com".into(), "hel1".into())
        );
    }

    #[test]
    fn r2_needs_the_account_and_knows_the_eu() {
        assert!(s3_location(1, 0, "", false, "", "").is_err());
        assert_eq!(
            loc(1, 0, "abc123", false, "", ""),
            (
                "https://abc123.r2.cloudflarestorage.com".into(),
                "auto".into()
            )
        );
        assert_eq!(
            loc(1, 0, "abc123", true, "", "").0,
            "https://abc123.eu.r2.cloudflarestorage.com"
        );
    }

    #[test]
    fn b2_takes_the_region_from_the_endpoint() {
        for given in [
            "s3.eu-central-003.backblazeb2.com",
            "https://s3.eu-central-003.backblazeb2.com",
        ] {
            assert_eq!(
                loc(2, 0, "", false, given, ""),
                (
                    "https://s3.eu-central-003.backblazeb2.com".into(),
                    "eu-central-003".into()
                )
            );
        }
        assert!(s3_location(2, 0, "", false, "backblaze.com", "").is_err());
    }

    #[test]
    fn other_s3_keeps_what_it_is_given() {
        assert!(s3_location(3, 0, "", false, "", "").is_err());
        assert_eq!(
            loc(3, 0, "", false, "http://127.0.0.1:9000", ""),
            ("http://127.0.0.1:9000".into(), "auto".into())
        );
        assert_eq!(
            loc(3, 0, "", false, "minio.example.com", "us-east-1"),
            ("https://minio.example.com".into(), "us-east-1".into())
        );
    }
}
