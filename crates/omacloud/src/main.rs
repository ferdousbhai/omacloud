//! `omacloud`: set up a synced folder, sync it once, or keep it synced.
//!
//! Output follows the shared product contract (`output.rs`): text on a
//! terminal, a JSON envelope when piped; errors and exit codes in `fail.rs`.
//! Commands that print a secret print it only to a terminal. `watch`, which
//! systemd runs, keeps its own exit codes (see [`UPGRADED`]).

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, mpsc},
    time::{Duration, Instant},
};

mod agent_skill;
mod alerts;
mod dropbox;
mod fail;
mod mount;
mod output;
mod power;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use log::{info, warn};
use notify::{RecursiveMode, Watcher};
use omacloud_core::{
    BucketCoordinator, Coordinator, DirCoordinator, Engine, KeyChange, Membership, RepoSpec, SettingsSetup, Setup,
    account,
    bucket::key_refused,
    bucket_key::Credentials,
    devices::{fingerprint, public_hex},
    head::{load_or_create_key, write_private},
    settings::Manifest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fail::CliError;
use crate::output::{Mode, OutputFlags, Response};

/// Appends a line to a command's text output.
macro_rules! say {
    ($out:expr) => {
        $out.push('\n')
    };
    ($out:expr, $($arg:tt)*) => {{
        use std::fmt::Write as _;
        _ = writeln!($out, $($arg)*);
    }};
}

/// What a command returns: its result, or a failure `fail::classify` maps.
type Out = Result<Response>;

/// The output mode, set once the arguments are parsed.
static MODE: OnceLock<Mode> = OnceLock::new();

fn mode() -> Mode {
    MODE.get().copied().unwrap_or(Mode::Human)
}

const AFTER_HELP: &str = "\
Output: text on a terminal, a JSON envelope {\"ok\":true,\"data\",\"summary\"} when
piped. --json forces the envelope, --styled text, --quiet prints only the data,
--ids-only one id per line (devices, versions, deleted, folders, packages,
secrets),
--count the number of results; ids-only > count > quiet > json > styled. A
failure is {\"ok\":false,\"error\":{\"code\",\"message\",\"hint\"?}} on stdout, or
error:/hint: lines on stderr in text mode.

Commands that print a secret (init when it creates an account, join-code,
invite, export, recovery split and combine) print only to a
terminal, in text mode, with stdin a terminal too (combine excepted: its shares
may come on stdin).

Exit codes: 0 ok; 1 other failure; 2 invalid usage or request (a destructive
command without --yes where nobody can be asked, a secret asked for where no
terminal shows it, ambiguous, already set up); 3 not set up, not approved, or a
key or code refused; 4 not found; 5 storage or network out of reach. `watch` (the daemon) exits 2 when this computer was
removed, the account can't be verified or its storage is gone (sync stops until
someone looks), and 75 after an upgrade, a change to settings sync or a new bucket key (the unit
restarts it into that).";

#[derive(Parser)]
#[command(
    name = "omacloud",
    version,
    about = "Omacloud: your files and settings on every computer, end to end encrypted",
    after_help = AFTER_HELP
)]
struct Cli {
    #[command(flatten)]
    out: OutputFlags,
    /// Config file [default: $XDG_CONFIG_HOME/omacloud/config.toml]
    #[arg(long, global = true, env = "OMACLOUD_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set up this device: create the account in your Dropbox or bucket
    /// (`--provider ...`), or join one with the code from `omacloud join-code` or `omacloud
    /// invite` in OMACLOUD_JOIN_CODE (joining is then approved on a computer
    /// already in), or with the recovery code in OMACLOUD_RECOVERY_CODE and
    /// `--provider ...` as when it was made
    Init {
        /// Sync this one folder, whole [default: Desktop, Documents and
        /// Pictures in home, as iCloud does; `omacloud folders` changes them]
        #[arg(long)]
        folder: Option<PathBuf>,
        #[command(flatten)]
        bucket: BucketArgs,
        /// For development, without a bucket: where the files live, a path
        /// (with --coordinator)
        #[arg(long)]
        repo: Option<String>,
        /// For development, without a bucket: where computers keep in step,
        /// a shared directory (with --repo)
        #[arg(long)]
        coordinator: Option<String>,
        /// Folders of home to sync on this computer, comma separated [default:
        /// Desktop,Documents,Pictures]. The usual folders left out (Music,
        /// Videos, Downloads, ...) are skipped here even if another computer
        /// syncs them
        #[arg(long, value_delimiter = ',')]
        folders: Option<Vec<String>>,
        /// Also sync Omarchy settings (`omacloud settings on`)
        #[arg(long)]
        settings: bool,
        /// Name for this device [default: the hostname]
        #[arg(long)]
        device: Option<String>,
    },
    /// Print a code another computer joins this account with (`omacloud
    /// init` with it in OMACLOUD_JOIN_CODE)
    JoinCode,
    /// Print a code that starts someone else's own account, in a bucket you
    /// made for them; they run `omacloud init` with it in OMACLOUD_JOIN_CODE,
    /// and get a recovery code only they hold. Their key's secret comes from
    /// OMACLOUD_SECRET_ACCESS_KEY, or is asked for
    Invite {
        #[command(flatten)]
        bucket: BucketArgs,
    },
    /// The key to your storage: change it, to cut off a lost or removed computer
    Bucket {
        #[command(subcommand)]
        action: Option<BucketCmd>,
    },
    /// Sync once and exit
    Sync,
    /// Keep the folder in sync until stopped
    Watch {
        /// Seconds between checks for other devices' changes
        #[arg(long, default_value_t = 10)]
        poll: u64,
    },
    /// Show this device's sync state (everything the Omacloud app shows, as
    /// JSON with --json or when piped)
    Status,
    /// Print this device's public key and fingerprint
    DeviceKey,
    /// List, approve or remove the account's devices
    Devices {
        #[command(subcommand)]
        action: Option<DevicesCmd>,
    },
    /// List the versions of a file, oldest first
    Versions {
        /// Path inside the synced folder
        path: PathBuf,
    },
    /// List files deleted lately and not made again, newest first
    Deleted {
        /// How many days back to look
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
    /// Show every file of the account as a read-only folder, each
    /// fetched when it's opened (folders skipped here too), until Ctrl-C
    Mount {
        /// An empty folder outside the synced ones [default: ~/Omacloud]
        dir: Option<PathBuf>,
    },
    /// Write a file as it was at a given version
    Restore {
        /// Path inside the synced folder
        path: PathBuf,
        /// Version (head number) from `omacloud versions`
        #[arg(long)]
        at: u64,
        /// Where to write it [default: <name>.v<at> in the current directory]
        #[arg(long)]
        to: Option<PathBuf>,
        /// Replace the file there if there is one
        #[arg(long)]
        yes: bool,
    },
    /// Move to a new repository key, locking out removed devices. Uploads
    /// everything again and deletes the old repository
    Rotate {
        /// Don't ask; needed where nobody can be asked
        #[arg(long)]
        yes: bool,
    },
    /// Package lists: what each device has installed, to restore elsewhere
    Packages {
        #[command(subcommand)]
        action: Option<PackagesCmd>,
    },
    /// Which folders of home sync: Desktop, Documents and Pictures to start
    Folders {
        #[command(subcommand)]
        action: Option<FoldersCmd>,
    },
    /// Secrets bundle: ssh and gpg keys and tokens, sealed so only the
    /// recovery code opens them
    Secrets {
        #[command(subcommand)]
        action: Option<SecretsCmd>,
    },
    /// Recovery with people you trust: the recovery code split among
    /// several people
    Recovery {
        #[command(subcommand)]
        action: RecoveryCmd,
    },
    /// Settings sync: the dots manifest's shared files, on this device
    Settings {
        #[command(subcommand)]
        action: Option<SettingsCmd>,
    },
    /// Set up a fresh machine from another: settings on, everything synced,
    /// the other machine's packages, the daemon started. Run `init` first
    RestoreMachine {
        /// The device to take packages from
        #[arg(long)]
        from: String,
        /// Install the packages (pacman still asks); otherwise only show
        #[arg(long)]
        yes: bool,
    },
    /// Print everything needed to restore without omacloud; with `--to`,
    /// first copy the repository somewhere of your own
    Export {
        /// Copy the repository to this path, then describe the copy
        #[arg(long)]
        to: Option<String>,
    },
    /// Print the agent skill (SKILL.md) built into omacloud, or install it
    /// for your coding agents
    Skill {
        #[command(subcommand)]
        action: Option<SkillCmd>,
    },
}

/// Where a bucket is: a provider's shorthand, or any S3 endpoint.
#[derive(clap::Args, Clone, Debug, Default)]
struct BucketArgs {
    /// Who keeps the files: `dropbox` (allowed in the browser), or a bucket:
    /// `r2` (with --account), `hetzner` (with --location) or `s3` (with
    /// --endpoint)
    #[arg(long, value_enum)]
    provider: Option<Provider>,
    /// Cloudflare account id, from the R2 overview page
    #[arg(long)]
    account: Option<String>,
    /// The R2 bucket is in the EU jurisdiction
    #[arg(long)]
    eu: bool,
    /// Hetzner location: fsn1, nbg1 or hel1
    #[arg(long)]
    location: Option<String>,
    /// S3 endpoint, such as https://s3.example.com
    #[arg(long)]
    endpoint: Option<String>,
    /// S3 region [default: auto]
    #[arg(long)]
    region: Option<String>,
    /// The bucket's name; make it private, at your provider, first
    #[arg(long)]
    bucket: Option<String>,
    /// The key's id; its secret comes from OMACLOUD_SECRET_ACCESS_KEY, or is
    /// asked for
    #[arg(long)]
    access_key_id: Option<String>,
    /// A folder in the bucket or Dropbox app folder, if not all of it
    #[arg(long)]
    root: Option<String>,
    /// Keep the records that keep computers in step in this second bucket
    /// (same endpoint and key). A bucket under a lock or object lock needs one:
    /// the records change in place
    #[arg(long)]
    coordination_bucket: Option<String>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Dropbox,
    R2,
    Hetzner,
    S3,
}

impl BucketArgs {
    fn given(&self) -> bool {
        self.provider.is_some() || self.bucket.is_some() || self.endpoint.is_some()
    }

    /// The repository these name, with all of its key but the secret: what's
    /// missing is refused before anything is asked for.
    fn repo(&self) -> Result<RepoSpec, CliError> {
        let missing = |what: &str, flag: &str| fail::usage(format!("this provider needs {what}: {flag}"));
        let provider = self.provider.ok_or_else(|| {
            fail::usage("say where the files live").hint(
                "give your bucket: `--provider r2 --account <id> --bucket <name> --access-key-id <id>` (or \
                 `hetzner --location fsn1`, or `s3 --endpoint <url>`), or `--provider dropbox`; to set up with a code, put it in \
                 OMACLOUD_JOIN_CODE. The Omacloud app walks through the same.",
            )
        })?;
        let (endpoint, region) = match provider {
            Provider::Dropbox => {
                if self.bucket.is_some() || self.access_key_id.is_some() || self.coordination_bucket.is_some() {
                    return Err(fail::usage(
                        "Dropbox takes no bucket or key: it asks you to allow it in the browser",
                    ));
                }
                let root = self.root.clone().unwrap_or_else(|| "/Omacloud".to_string());
                return Ok(RepoSpec {
                    repository: "opendal:dropbox".into(),
                    options: [("root", root), ("client_id", dropbox::APP_KEY.to_string())]
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect(),
                });
            }
            Provider::R2 => {
                let account = self
                    .account
                    .as_ref()
                    .ok_or_else(|| missing("your account id", "--account"))?;
                let eu = if self.eu { ".eu" } else { "" };
                (
                    format!("https://{account}{eu}.r2.cloudflarestorage.com"),
                    "auto".to_string(),
                )
            }
            Provider::Hetzner => {
                let location = self
                    .location
                    .as_ref()
                    .ok_or_else(|| missing("a location", "--location"))?;
                (format!("https://{location}.your-objectstorage.com"), location.clone())
            }
            Provider::S3 => {
                let endpoint = self
                    .endpoint
                    .as_ref()
                    .ok_or_else(|| missing("an endpoint", "--endpoint"))?;
                let region = self.region.clone().unwrap_or_else(|| "auto".to_string());
                (endpoint.trim_end_matches('/').to_string(), region)
            }
        };
        let bucket = self
            .bucket
            .clone()
            .ok_or_else(|| fail::usage("give the bucket's name: --bucket").hint("make it private, at your provider"))?;
        let key_id = self
            .access_key_id
            .clone()
            .ok_or_else(|| fail::usage("give the key's id: --access-key-id"))?;
        let mut options: BTreeMap<String, String> = [
            ("endpoint", endpoint),
            ("region", region),
            ("bucket", bucket),
            ("access_key_id", key_id),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        if let Some(root) = &self.root {
            _ = options.insert("root".into(), root.clone());
        }
        Ok(RepoSpec {
            repository: "opendal:s3".into(),
            options,
        })
    }
}

/// A key's secret half, from the environment or a prompt.
fn read_key_secret() -> Result<String> {
    read_secret("OMACLOUD_SECRET_ACCESS_KEY", "secret access key")
}

#[derive(Subcommand)]
enum BucketCmd {
    /// Show the latest key change and which computers switched to it
    Status,
    /// Switch every computer to a new key. Make it at your provider first,
    /// and keep the old one until `bucket status` says every computer
    /// switched. The secret comes from OMACLOUD_SECRET_ACCESS_KEY, or is
    /// asked for. On Dropbox it asks you to allow Omacloud again instead, and
    /// the old token is revoked by itself
    SetKey {
        /// The new key's id; Dropbox asks you to allow it again instead
        #[arg(long)]
        access_key_id: Option<String>,
        /// This computer only, when the old key no longer works here
        #[arg(long)]
        here_only: bool,
    },
}

#[derive(Subcommand)]
enum SkillCmd {
    /// Copy SKILL.md to ~/.agents/skills/omacloud/ and link it into
    /// ~/.claude/skills/ when Claude Code is installed
    Install,
    /// Remove the skill and link that `skill install` created; anything
    /// else is left alone
    Remove,
}

const SKILL: agent_skill::Skill = agent_skill::Skill {
    name: "omacloud",
    body: include_str!("../../../skills/omacloud/SKILL.md"),
    cli: "omacloud",
    version: env!("CARGO_PKG_VERSION"),
    adopts_legacy: false,
};

#[derive(Subcommand)]
enum PackagesCmd {
    /// Show the devices whose package lists are known
    List,
    /// Install what another device has and this one doesn't
    Restore {
        /// The device to copy from
        #[arg(long)]
        from: String,
        /// Run pacman (it still asks before installing); otherwise only show
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum SecretsCmd {
    /// Show the devices that saved a secrets bundle
    List,
    /// Seal this device's secrets (~/.ssh, ~/.gnupg, tokens; or what
    /// ~/.config/omacloud/secrets lists) and sync them, replacing the last
    /// bundle saved here
    Save,
    /// Put another device's secrets on this one. Asks for the recovery code;
    /// files it replaces are backed up first
    Restore {
        /// The device whose bundle to open
        #[arg(long)]
        from: String,
        /// Write the files; otherwise only show what would change
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum RecoveryCmd {
    /// Split the recovery code into shares; any `threshold` of them rebuild
    /// it, fewer reveal nothing. Give each person one share
    Split {
        /// Shares needed to rebuild the code
        #[arg(long)]
        threshold: u8,
        /// Shares to make
        #[arg(long)]
        shares: u8,
    },
    /// Rebuild the recovery code from shares, one per line on stdin
    Combine,
}

#[derive(Subcommand)]
enum SettingsCmd {
    /// Show whether settings sync is on, and anything waiting to be resolved
    Status,
    /// Sync settings on this device
    On,
    /// Stop syncing settings on this device (files stay as they are)
    Off,
    /// Leave a group of settings out on this device: shell, agents, apps,
    /// chromium or wifi
    Skip { group: String },
    /// Sync a skipped group of settings here again
    Unskip { group: String },
    /// Settle a setting that changed here and on another device
    Resolve {
        /// The setting, like ~/.config/hypr/bindings.lua
        path: PathBuf,
        /// Which version to keep
        #[arg(long, value_enum)]
        keep: Keep,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Keep {
    /// This device's version
    Local,
    /// The other device's version
    Remote,
}

#[derive(Subcommand)]
enum FoldersCmd {
    /// Show the folders syncing here, and the ones this device skips
    List,
    /// Sync a folder of home on every device (`Music`, `Videos`, ...)
    Add { name: String },
    /// Stop syncing a folder on this device; its files stay, and other
    /// devices keep syncing it
    Skip { name: String },
    /// Sync a skipped folder here again
    Unskip { name: String },
    /// Leave a subfolder out on this device, like `Documents/Notes`; its files
    /// stay, and other devices keep syncing it
    Exclude { path: String },
    /// Sync an excluded subfolder here again
    Include { path: String },
    /// Take a folder out of the cloud: no device deletes its files, each stops
    /// syncing it, and its earlier versions go within 30 days. `folders add`
    /// brings it back
    Remove {
        name: String,
        /// Don't ask; needed where nobody can be asked
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum DevicesCmd {
    /// Show devices and join requests
    List,
    /// Let a device join, after checking the fingerprint it shows
    Approve {
        /// Fingerprint (or its start) of the requesting device
        fingerprint: String,
    },
    /// Remove a device from the account
    Revoke {
        /// Fingerprint (or its start) of the device
        fingerprint: String,
        /// Also rotate the repository key, so the device can't read new data
        #[arg(long)]
        rotate: bool,
        /// It's lost or stolen: rotate too, and on Dropbox change the token (it
        /// asks you to allow Omacloud again) so it can't reach the storage either
        #[arg(long)]
        lost: bool,
        /// Don't ask; needed where nobody can be asked
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    device: String,
    folder: PathBuf,
    /// `bucket` (coordination beside the files, in `repo`'s bucket), or the
    /// shared directory computers keep in step through, for development
    coordinator: String,
    /// Where the files live; a bucket's key among its options, so this file
    /// is readable by its owner only
    repo: RepoSpec,
    /// The account root public key (hex), pinned at setup
    root: String,
    #[serde(default)]
    limits: Limits,
    /// Sync settings (the dots manifest's shared tier) on this device
    #[serde(default)]
    settings: bool,
    /// Groups of settings this device leaves out
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    settings_skip: std::collections::BTreeSet<String>,
    /// Desktop notifications from the daemon: join requests, settings that
    /// need a choice, this device being removed
    #[serde(default = "yes")]
    notifications: bool,
    /// Sync chosen folders of home (then `folder` is home); without it, all
    /// of `folder`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folders: Option<FolderChoice>,
    /// With `coordinator = "bucket"`: a second bucket for the coordination,
    /// at the same endpoint and with the same key
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coordination_bucket: Option<String>,
    /// The last bucket key change this device switched to
    #[serde(default)]
    bucket_key_seq: u64,
}

/// `coordinator = "bucket"`: coordination lives in the account's bucket,
/// beside the files.
const BUCKET: &str = "bucket";

/// `coordinator` of a computer set up on the Omacloud network, which is gone.
const OLD_NETWORK: &str = "network";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct FolderChoice {
    /// Folders this device adds to the account
    #[serde(default)]
    add: std::collections::BTreeSet<String>,
    /// Folders this device doesn't sync
    #[serde(default)]
    skip: std::collections::BTreeSet<String>,
    /// Subfolders this device leaves out, by path in the account
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    exclude: std::collections::BTreeSet<String>,
}

/// What syncs by default, as iCloud's Desktop & Documents and Photos.
const DEFAULT_FOLDERS: &[&str] = &["Desktop", "Documents", "Pictures"];

/// The folders of home a computer is asked about.
const USUAL_FOLDERS: &[&str] = &["Desktop", "Documents", "Pictures", "Music", "Videos", "Downloads"];

/// The XDG user folders this machine sets, by the English names the
/// account uses, with what `user-dirs.dirs` gives each (unquoted).
fn user_dirs(home: &Path) -> Vec<(&'static str, String)> {
    let dirs = fs::read_to_string(
        std::env::var_os("XDG_CONFIG_HOME")
            .map_or_else(|| home.join(".config"), PathBuf::from)
            .join("user-dirs.dirs"),
    )
    .unwrap_or_default();
    [
        ("DESKTOP", "Desktop"),
        ("DOCUMENTS", "Documents"),
        ("PICTURES", "Pictures"),
        ("MUSIC", "Music"),
        ("VIDEOS", "Videos"),
        ("DOWNLOAD", "Downloads"),
    ]
    .into_iter()
    .filter_map(|(key, name)| {
        let value = dirs.lines().find_map(|l| {
            l.trim()
                .strip_prefix(&format!("XDG_{key}_DIR="))
                .map(|v| v.trim_matches('"').to_string())
        })?;
        Some((name, value))
    })
    .collect()
}

/// The account's name for a folder of home given by its name here
/// (`Bilder` is `Pictures` when that's where Pictures lives here), or a
/// usage error when it isn't a folder directly in home.
fn folder_name(home: &Path, name: &str) -> Result<String> {
    // by its path too, as a file manager or a shell's completion gives it
    let name = name.trim().trim_end_matches('/');
    let name = name.strip_prefix("~/").unwrap_or(name);
    let name = Path::new(name)
        .strip_prefix(home)
        .ok()
        .and_then(Path::to_str)
        .unwrap_or(name);
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return Err(fail::usage(format!("{name:?} isn't a folder directly in home"))
            .hint("give one like Music")
            .into());
    }
    Ok(xdg_names(home)
        .into_iter()
        .find(|(_, local)| local == name)
        .map_or_else(|| name.to_string(), |(account, _)| account))
}

/// A folder of home by its account name, and where it lives here (`local`:
/// [`xdg_names`]).
fn folder_here(home: &Path, local: &BTreeMap<String, String>, name: &str) -> Value {
    json!({ "name": name, "path": home.join(local.get(name).map_or(name, String::as_str)) })
}

/// This machine's names for the XDG user folders, where they differ from
/// the English names the account uses (`Documents` -> `Dokumente`). Only
/// folders directly in home count.
fn xdg_names(home: &Path) -> BTreeMap<String, String> {
    user_dirs(home)
        .into_iter()
        .filter_map(|(name, value)| {
            let local = value.replace("$HOME/", "");
            let direct = !local.is_empty() && !local.contains('/') && !local.starts_with('$');
            (direct && local != name).then(|| (name.to_string(), local))
        })
        .collect()
}

/// The usual folders this computer points at home itself (Omarchy's
/// `XDG_DESKTOP_DIR="$HOME/"`): there is no such folder to sync.
fn xdg_at_home(home: &Path) -> Vec<String> {
    user_dirs(home)
        .into_iter()
        .filter(|(name, value)| {
            matches!(*name, "Desktop" | "Downloads") && matches!(value.as_str(), "$HOME" | "$HOME/")
        })
        .map(|(name, _)| name.to_string())
        .collect()
}

fn yes() -> bool {
    true
}

/// How much of the machine sync may use.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
struct Limits {
    /// Transfer rate cap for remote repositories, such as "5MiB" (per second)
    bandwidth: Option<String>,
    /// Parallel connections to a remote repository
    connections: Option<u32>,
    /// Pause while on battery below this percent; 0 never pauses
    pause_on_battery_below: u8,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bandwidth: None,
            connections: None,
            pause_on_battery_below: 20,
        }
    }
}

impl Limits {
    /// Backend options that enforce these limits: `bandwidth` on any
    /// backend, `connections` on bucket ones.
    fn apply(&self, repo: &mut RepoSpec) {
        if let Some(bw) = &self.bandwidth {
            // a token bucket around the backend (throttle.rs), which also
            // holds for small transfers
            _ = repo.options.entry("bandwidth".into()).or_insert_with(|| bw.clone());
        }
        if let Some(n) = self.connections {
            _ = repo.options.entry("connections".into()).or_insert(n.to_string());
        }
    }
}

/// Paths for one configuration: secrets and state live next to each other in
/// the data directory, readable only by the owner.
struct Paths {
    config: PathBuf,
    data: PathBuf,
    /// The default configuration, the one the packaged systemd unit runs.
    default: bool,
}

impl Paths {
    fn new(config: Option<PathBuf>) -> Result<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME not set")?;
        let xdg = |var: &str, fallback: &str| std::env::var_os(var).map_or_else(|| home.join(fallback), PathBuf::from);
        let standard = xdg("XDG_CONFIG_HOME", ".config").join("omacloud/config.toml");
        // the default by where it is, not its name: a test's config.toml
        // elsewhere must never reach the real account's data
        let config = config
            .map(std::path::absolute)
            .transpose()?
            .unwrap_or_else(|| standard.clone());
        let default = config == standard;
        Ok(Self {
            // a custom config file gets its own data directory beside it
            data: if default {
                xdg("XDG_DATA_HOME", ".local/share").join("omacloud")
            } else {
                config.with_extension("d")
            },
            config,
            default,
        })
    }
    /// Whether this is the default configuration, the one the packaged
    /// systemd unit runs.
    fn is_default(&self) -> bool {
        self.default
    }
    fn device_key(&self) -> PathBuf {
        self.data.join("device.key")
    }
    fn state(&self) -> PathBuf {
        self.data.join("state.json")
    }
    /// The user's ignore rules, beside the config file.
    fn ignore(&self) -> PathBuf {
        if self.is_default() {
            self.config.with_file_name("ignore")
        } else {
            self.config.with_extension("ignore")
        }
    }
}

fn load(paths: &Paths) -> Result<Config> {
    let config = read_config(paths)?;
    if let Some(kind) = storage_gone(&config) {
        return Err(fail::storage_gone(kind).into());
    }
    Ok(config)
}

fn read_config(paths: &Paths) -> Result<Config> {
    let text = match fs::read_to_string(&paths.config) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(fail::not_set_up().into()),
        other => other.with_context(|| format!("reading {}", paths.config.display()))?,
    };
    Ok(toml::from_str(&text)?)
}

/// What a config kept its files on, if that's storage Omacloud dropped: the
/// network, the earlier Dropbox (its token in the keyring, for an app that's gone)
/// and Omacloud storage. The computer needs setting up again.
fn storage_gone(config: &Config) -> Option<&'static str> {
    if config.coordinator == OLD_NETWORK {
        return Some("the Omacloud network");
    }
    if config.repo.is_dropbox() && !config.repo.options.contains_key("refresh_token") {
        return Some("the earlier Dropbox app");
    }
    if config
        .repo
        .options
        .get("endpoint")
        .is_some_and(|e| e.contains("omacloud.computer"))
    {
        return Some("Omacloud storage");
    }
    // before 0.0.18 a bucket's key was kept elsewhere
    (config.coordinator == BUCKET && !config.repo.is_dropbox() && !config.repo.options.contains_key("access_key_id"))
        .then_some("a bucket set up by an older version")
}

/// What a computer on the network kept in its data directory: others' blobs
/// and its own keys and records for a network that's gone.
const OLD_NETWORK_FILES: &[&str] = &[
    "node",
    "node.key",
    "account.seed",
    "wallet.json",
    "mirror.db",
    "mirror.db-wal",
    "mirror.db-shm",
    "shares.json",
    "holders.json",
    "suspects.json",
    "space.json",
    "renewed",
];

/// Before setting up again over a config on storage that's gone: keep it and
/// its sync state aside, under `.before-bucket`, and drop what the network left
/// here. True if there was one.
fn set_aside_old_setup(paths: &Paths) -> Result<bool> {
    let Some(old) = read_config(paths).ok().filter(|c| storage_gone(c).is_some()) else {
        return Ok(false);
    };
    if old.coordinator == OLD_NETWORK {
        for name in OLD_NETWORK_FILES {
            let path = paths.data.join(name);
            let gone = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            if let Err(e) = gone
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(e).with_context(|| format!("removing {}", path.display()));
            }
        }
    }
    for path in [&paths.config, &paths.state()] {
        if path.exists() {
            let mut aside = path.clone().into_os_string();
            aside.push(".before-bucket");
            fs::rename(path, aside)?;
        }
    }
    Ok(true)
}

fn save(paths: &Paths, config: &Config) -> Result<()> {
    if let Some(dir) = paths.config.parent() {
        fs::create_dir_all(dir)?;
    }
    write_private(&paths.config, toml::to_string_pretty(config)?.as_bytes())
}

/// Where `config`'s computers keep in step: its bucket, or a shared
/// directory.
fn coordinator(config: &Config) -> Result<Arc<dyn Coordinator>> {
    Ok(if config.coordinator == BUCKET {
        Arc::new(BucketCoordinator::beside(
            &config.repo,
            config.coordination_bucket.as_deref(),
        )?)
    } else {
        Arc::new(DirCoordinator::new(&config.coordinator)?)
    })
}

/// Whether a key just given reaches the storage, and it can hold an account.
fn check_bucket(coord: &dyn Coordinator) -> Result<()> {
    coord.check().map_err(given_key_refused)
}

/// The account at a key just given.
fn anchor_of(coord: &dyn Coordinator) -> Result<Option<omacloud_core::Anchor>> {
    coord.anchor().map_err(given_key_refused)
}

/// The storage's refusal said in terms of the key just given; anything else
/// as it is.
fn given_key_refused(e: anyhow::Error) -> anyhow::Error {
    if key_refused(&e) {
        fail::key_refused(format!("the storage refused this key: {e:#}")).into()
    } else {
        e
    }
}

/// What a join code carries: where the account is, and the key to reach it.
/// With no `account` it's an invite: the first to use it creates the account.
#[derive(Serialize, Deserialize)]
struct JoinCode {
    repo: RepoSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coordination_bucket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

impl JoinCode {
    const PREFIX: &str = "omacloud-join-";

    fn encode(&self) -> Result<String> {
        Ok(format!("{}{}", Self::PREFIX, hex::encode(serde_json::to_vec(self)?)))
    }

    fn parse(code: &str) -> Option<Self> {
        let bytes = hex::decode(code.trim().strip_prefix(Self::PREFIX)?).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// Switch this device to a newer bucket key, if one was published. A daemon
/// sees the saved config and restarts into it.
fn follow_bucket_key(paths: &Paths, e: &mut Engine) -> Result<()> {
    let mut config = load(paths)?;
    let seq = match e.key_change_since(config.bucket_key_seq)? {
        KeyChange::None => return Ok(()),
        KeyChange::CaughtUp(seq) => seq,
        KeyChange::Switch(seq, key) => {
            config.repo.set_key(&key);
            info!("switched to bucket key {seq}");
            seq
        }
    };
    // said first: said and not saved, it's only said again
    e.ack_bucket_key(seq)?;
    config.bucket_key_seq = seq;
    save(paths, &config)
}

fn engine(paths: &Paths, config: &Config) -> Result<Engine> {
    let signing = load_or_create_key(&paths.device_key())?;
    let coord = coordinator(config)?;
    let mut repo = config.repo.clone();
    config.limits.apply(&mut repo);
    let setup = Setup {
        device: config.device.clone(),
        folder: config.folder.clone(),
        repo,
        signing,
        root: config.root.clone(),
        state_path: Some(paths.state()),
        settings: if config.settings {
            let home = home_dir()?;
            let merged = paths.data.join("merged");
            // Chromium's profiles and the saved Wi-Fi networks merge too
            let mut groups: Vec<Arc<dyn omacloud_core::merged::Group>> =
                omacloud_core::chromium::Chromium::for_home(&home, &merged.join("scratch"))
                    .into_iter()
                    .map(|c| Arc::new(c) as Arc<dyn omacloud_core::merged::Group>)
                    .collect();
            let manifest = Manifest::load()?.skipping(&config.settings_skip);
            groups.push(Arc::new(
                omacloud_core::agents::Agents::new(&home).with_manifest(manifest.clone()),
            ));
            let omarchy =
                std::env::var_os("OMARCHY_PATH").map_or_else(|| PathBuf::from("/usr/share/omarchy"), PathBuf::from);
            groups.push(Arc::new(omacloud_core::theme::Theme::for_home(&home, &omarchy)));
            let user = std::env::var("USER").unwrap_or_default();
            groups.push(Arc::new(omacloud_core::wifi::Wifi::new(Box::new(
                omacloud_core::wifi::Nmcli::new(&user),
            ))));
            groups.retain(|g| !config.settings_skip.contains(g.name()));
            Some(SettingsSetup {
                home,
                manifest,
                backups: paths.data.join("settings"),
                packages: paths.data.join("packages"),
                secrets: paths.data.join("secrets"),
                merged,
                groups,
            })
        } else {
            None
        },
        folders: config.folders.as_ref().map(|f| omacloud_core::Folders {
            add: f.add.clone(),
            skip: f.skip.clone(),
            local_names: xdg_names(&config.folder),
            exclude: f.exclude.clone(),
        }),
    };
    let mut e = Engine::new(setup, coord)?;
    match fs::read_to_string(paths.ignore()) {
        Ok(text) => e.set_ignore_rules(text.lines().map(str::to_string).collect()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("reading ignore rules"),
    }
    Ok(e)
}

fn already_set_up(paths: &Paths) -> CliError {
    CliError::new(
        "already_set_up",
        output::EXIT_USAGE,
        format!("{} exists; this device is already set up", paths.config.display()),
    )
    .hint("give another --config to set up a second account here")
}

/// Refuse where a new account's recovery code wouldn't be shown to anyone.
fn need_terminal_for_recovery_code() -> Result<(), CliError> {
    if output::to_terminal(mode()) {
        Ok(())
    } else {
        Err(fail::terminal_required("the new account's recovery code"))
    }
}

/// Where `init` was told to set up.
struct InitTarget {
    coordinator: String,
    repo: RepoSpec,
    coordination_bucket: Option<String>,
    /// The account a join code named, which the bucket must hold.
    account: Option<String>,
}

/// A bucket, unless a shared directory is given (development).
fn init_target(
    coordinator: Option<String>,
    repo: Option<String>,
    bucket: &BucketArgs,
    join_code: Option<&str>,
    recovering: bool,
) -> Result<InitTarget> {
    if let Some(dir) = coordinator {
        if join_code.is_some() {
            return Err(fail::usage("a join code is for a bucket, not a shared directory").into());
        }
        if bucket.given() {
            return Err(fail::usage("a bucket needs no --coordinator").into());
        }
        return Ok(InitTarget {
            coordinator: absolute(Path::new(&dir))?.to_string_lossy().into_owned(),
            repo: RepoSpec {
                repository: repo.unwrap_or_default(),
                options: BTreeMap::new(),
            },
            coordination_bucket: None,
            account: None,
        });
    }
    if repo.is_some() {
        return Err(fail::usage("--repo goes with --coordinator; a bucket needs neither").into());
    }
    if let Some(code) = join_code {
        if bucket.given() {
            return Err(fail::usage("a join code says where the bucket is already").into());
        }
        let joined = JoinCode::parse(code).ok_or_else(|| {
            fail::setup("join_code_rejected", "that join code is damaged")
                .hint("copy the whole code again from `omacloud join-code` (or Show Join Code in the app)")
        })?;
        return Ok(InitTarget {
            coordinator: BUCKET.to_string(),
            repo: joined.repo,
            coordination_bucket: joined.coordination_bucket,
            account: joined.account,
        });
    }
    let mut repo = bucket.repo()?;
    if !recovering {
        need_terminal_for_recovery_code()?;
    }
    if repo.is_dropbox() {
        let refresh = dropbox::authorize()?;
        _ = repo.options.insert("refresh_token".into(), refresh);
    } else {
        _ = repo.options.insert("secret_access_key".into(), read_key_secret()?);
    }
    Ok(InitTarget {
        coordinator: BUCKET.to_string(),
        repo,
        coordination_bucket: bucket.coordination_bucket.clone(),
        account: None,
    })
}

/// `account`: the account a join code named, which the bucket must hold.
fn init(paths: &Paths, mut config: Config, recovery: Option<String>, account: Option<String>) -> Out {
    // the refusals that need no account come before anything is made here:
    // no folder, device key or coordination skeleton is left behind. A
    // shared directory is only looked at, as opening it makes its skeleton
    if config.coordinator != BUCKET && !Path::new(&config.coordinator).join("account.json").exists() {
        if config.repo.repository.is_empty() {
            return Err(fail::usage("creating an account needs --repo")
                .hint("run `omacloud init --help`")
                .into());
        }
        need_terminal_for_recovery_code()?;
    }
    let coord = coordinator(&config)?;
    let anchor = anchor_of(coord.as_ref())?;
    let creating = anchor.is_none();
    if creating && (recovery.is_some() || account.is_some()) {
        return Err(fail::setup("no_account", "this storage has no account in it")
            .hint("check that it's the bucket or Dropbox the code is for")
            .into());
    }
    if let (Some(anchor), Some(code)) = (&anchor, &recovery) {
        let root = omacloud_core::devices::root_key(code).ok().map(|k| public_hex(&k));
        if root.as_deref() != Some(anchor.root.as_str()) {
            return Err(fail::recovery_code_rejected("that recovery code doesn't belong to this account").into());
        }
    }
    if let (Some(anchor), Some(account)) = (&anchor, &account)
        && anchor.root != *account
    {
        return Err(fail::setup(
            "join_code_rejected",
            "this bucket holds another account than the join code's",
        )
        .into());
    }
    // creating an account shows its recovery code and password once: only
    // to a terminal
    if creating {
        need_terminal_for_recovery_code()?;
        check_bucket(coord.as_ref())?;
    }
    fs::create_dir_all(&paths.data)?;
    let signing = load_or_create_key(&paths.device_key())?;
    let me = fingerprint(&public_hex(&signing));
    fs::create_dir_all(&config.folder)?;

    let mut out = String::new();
    let how = if creating {
        let created = account::create(coord.as_ref(), &config.repo, &signing, &config.device)?;
        config.root = created.root;
        let place = if config.repo.is_dropbox() {
            "in your Dropbox".to_string()
        } else {
            config
                .repo
                .options
                .get("bucket")
                .map_or_else(|| config.repo.repository.clone(), |b| format!("in bucket {b}"))
        };
        say!(
            out,
            "created repository and account {}\n\n\
             account id: {}\n\n\
             recovery code: {}\n\n\
             Write it down and keep it offline. It is shown once. With it you can add a\n\
             device when no other device is at hand, and recover the account; without\n\
             it and without a device, the data is gone.\n\n\
             repository password (for leaving omacloud, also in `omacloud export`): {}\n\n\
             Other computers join with a code from `omacloud join-code`.",
            place,
            config.root,
            created.recovery_code,
            created.password
        );
        "created"
    } else if let Some(code) = recovery {
        config.root = account::join_with_recovery(coord.as_ref(), &code, &signing, &config.device)?;
        say!(out, "joined the account with the recovery code");
        "joined_with_recovery_code"
    } else {
        config.root = account::request(coord.as_ref(), &signing, &config.device)?;
        say!(
            out,
            "asked to join the account\n\n\
             this device's fingerprint: {me}\n\n\
             On a device that already syncs, run `omacloud devices` and check that it\n\
             shows the same fingerprint, then `omacloud devices approve {me}`."
        );
        "asked_to_join"
    };
    save(paths, &config)?;
    say!(out, "\naccount root: {}", fingerprint(&config.root));
    if paths.is_default() {
        // the packaged unit starts at login for users with a config; start
        // it now for this session
        say!(out, "\nstart syncing now: systemctl --user start omacloud");
    }
    let summary = match how {
        "created" => "created the account".to_string(),
        "asked_to_join" => format!("asked to join; approve fingerprint {me} on a computer that syncs"),
        _ => "joined the account".to_string(),
    };
    let data = json!({
        "result": how,
        "device": config.device,
        "fingerprint": me,
        "account": config.root,
        "account_fingerprint": fingerprint(&config.root),
        "approved": how != "asked_to_join",
    });
    Ok(Response::new(data, summary).human(out.trim_end()))
}

/// How long a computer whose key is refused waits before asking again: it may
/// come back, but it isn't worth a request every poll.
const REFUSED_WAIT: Duration = Duration::from_secs(300);

/// Why the daemon stopped.
enum Ended {
    /// Removed from the account, or the account can't be verified.
    Stopped,
    /// The package replaced the binary, or settings sync or the bucket's key
    /// changed: restart into the new one.
    Upgraded,
}

/// Exit status to be restarted (an upgrade, a changed config): a failure to
/// the unit, so `Restart=` starts it again (2 is the status it doesn't
/// restart).
const UPGRADED: i32 = 75;

/// Keep the folders in sync, until sync must stop or an upgrade replaced
/// this binary.
fn watch(paths: &Paths, config: &Config, poll: Duration) -> Result<Ended> {
    power::be_nice();
    let mut engine = engine(paths, config)?;
    let exe = Exe::running();
    let mut refused_at: Option<Instant> = None;
    // a path changed, or `None`: events were lost, look at everything
    let (files, rx) = mpsc::channel();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        _ = files.send(match res {
            Ok(event) if !event.need_rescan() => Some(event.paths),
            Ok(_) => None,
            Err(e) => {
                warn!("watching files: {e}");
                None
            }
        });
    })?;
    let mut watches = Watches::new(watcher);
    info!("watching {}", config.folder.display());

    // wait for a second of quiet before pushing, but no longer than ten
    let (quiet, longest) = (Duration::from_secs(1), Duration::from_secs(10));
    let (mut paused, mut waiting) = (false, false);
    let mut packages_at: Option<Instant> = None;
    let mut tidied_at: Option<Instant> = None;
    let mut alerts = config
        .notifications
        .then(|| alerts::Alerts::new((!paths.is_default()).then_some(paths.config.as_path())));
    let config_mtime = || fs::metadata(&paths.config).and_then(|m| m.modified()).ok();
    let mut config_seen = config_mtime();
    let (mut seen_marker, mut full_at, mut folders_changed) = (None, None, false);
    // the change marker, looked at no more than once a poll: a browser's
    // files wake this loop every second or two, and need no storage request
    let (mut marker_at, mut last_marker): (Option<Instant>, Option<String>) = (None, None);
    // when everything was last looked over, whether or not that sync went
    // through: a sync that keeps failing mustn't walk every file each poll
    let mut rescan_at: Option<Instant> = None;
    loop {
        // the config was edited: folders are followed as they are; settings
        // sync turned on or off, or a new bucket key, restarts into it (exit
        // 75, as for an upgrade, which the unit restarts)
        let now = config_mtime();
        if now != config_seen {
            config_seen = now;
            if let Ok(c) = load(paths) {
                if c.settings != config.settings || c.settings_skip != config.settings_skip || c.repo != config.repo {
                    info!("settings sync or the bucket's key changed; restarting into it");
                    return Ok(Ended::Upgraded);
                }
                if let Some(choice) = c.folders {
                    info!("folder choice changed");
                    engine.set_folders(choice.add, choice.skip, choice.exclude);
                    folders_changed = true;
                }
            }
        }
        // between syncs: an upgrade restarts into the new binary
        if exe.as_ref().is_some_and(Exe::replaced) {
            info!("omacloud was upgraded; restarting into the new version");
            return Ok(Ended::Upgraded);
        }
        // syncing folders of home: watch those, never all of home. Folders
        // appear as the account adds them, and one deleted and made again
        // is watched again
        let mut wanted: Vec<(PathBuf, RecursiveMode)> = if config.folders.is_none() {
            vec![(config.folder.clone(), RecursiveMode::Recursive)]
        } else {
            engine
                .synced_folders()
                .into_iter()
                .map(|d| (d, RecursiveMode::Recursive))
                .collect()
        };
        // settings live in a few directories under home; watch just those
        wanted.extend(engine.settings_watch_dirs().into_iter().map(|(d, deep)| {
            let mode = if deep {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            (d, mode)
        }));
        if watches.follow(&wanted) {
            // what changed before the watch began went unseen
            engine.request_rescan();
        }
        // the package list, at start and hourly
        if packages_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600)) {
            record_packages(config, &mut engine);
            packages_at = Some(Instant::now());
        }
        // changes noticed while paused stay pending and go out on resume
        let pause = power::should_pause(power::battery(), config.limits.pause_on_battery_below);
        if pause != paused {
            paused = pause;
            info!("{} sync (battery)", if paused { "pausing" } else { "resuming" });
        }
        // idle: one look at the change marker instead of a sync, with a full
        // sync now and then in case a write didn't mark itself
        let refused = refused_at.is_some_and(|t: Instant| t.elapsed() < REFUSED_WAIT);
        let marker = if paused || refused {
            None
        } else if marker_at.is_some_and(|t| t.elapsed() < poll) {
            last_marker.clone()
        } else {
            marker_at = Some(Instant::now());
            last_marker = engine.remote_marker().ok().flatten();
            last_marker.clone()
        };
        // apps that wrote files: a cheap local look, at most once a minute
        engine.look_at_apps();
        let idle = !engine.has_pending()
            && !folders_changed
            && marker.is_some()
            && marker == seen_marker
            && full_at.is_some_and(|t: Instant| t.elapsed() < FULL_SYNC);
        if !paused && !refused && !idle {
            folders_changed = false;
            // now and then, look at everything: a watcher can miss changes
            if rescan_at.is_none_or(|t| t.elapsed() >= FULL_SYNC) {
                engine.request_rescan();
                rescan_at = Some(Instant::now());
            }
            match run_sync(&mut engine, &mut waiting, alerts.as_mut()) {
                Synced::Stop => return Ok(Ended::Stopped),
                Synced::Ok => {
                    // the marker read before syncing: anything written
                    // since moves it again
                    seen_marker = marker;
                    full_at = Some(Instant::now());
                    // daily: versions past the window go (one computer of
                    // the account does it)
                    if tidied_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(24 * 3600)) {
                        tidied_at = Some(Instant::now());
                        if let Err(e) = engine.tidy(omacloud_core::VERSIONS_KEPT) {
                            warn!("letting old versions go: {e:#}");
                        }
                    }
                }
                Synced::Failed => seen_marker = None,
                Synced::Refused => {
                    seen_marker = None;
                    refused_at = Some(Instant::now());
                }
            }
            // the account's new bucket key: saved, it restarts this into it. A
            // key just refused can't read one
            if refused_at.is_none_or(|t: Instant| t.elapsed() >= REFUSED_WAIT)
                && let Err(e) =
                    follow_bucket_key(paths, &mut engine).and_then(|()| retire_old_token(paths, &mut engine).map(drop))
            {
                warn!("checking for a new bucket key: {e:#}");
            }
            if let Some(a) = alerts.as_mut() {
                a.check(&mut engine);
            }
        }
        // a folder made in home: offer to sync it, once
        if let (Some(a), true) = (alerts.as_mut(), config.folders.is_some()) {
            let synced: Vec<String> = engine.folder_list().into_iter().map(|(name, _)| name).collect();
            a.new_folders(&config.folder, &paths.data, &home_folders(&config.folder), &synced);
        }
        let mut notice = |changed: Option<Vec<PathBuf>>| match changed {
            // what matters is pending now; the rest needs nothing
            Some(paths) => _ = engine.notice(paths),
            None => engine.request_rescan(),
        };
        match rx.recv_timeout(poll) {
            Ok(changed) => {
                notice(changed);
                let first = Instant::now();
                loop {
                    match rx.recv_timeout(quiet) {
                        Ok(more) => {
                            notice(more);
                            if first.elapsed() >= longest {
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// The directories being watched, each with its identity when the watch
/// began: a directory deleted and made again is a new one, which a watch on
/// the old one doesn't see. Its birth time tells them apart where the inode
/// number is reused.
struct Watches<W> {
    watcher: W,
    dirs: BTreeMap<PathBuf, (u64, u64, Option<std::time::SystemTime>)>,
    /// directories a watch failed on, said once rather than every poll
    failing: std::collections::BTreeSet<PathBuf>,
}

impl<W: Watcher> Watches<W> {
    fn new(watcher: W) -> Self {
        Self {
            watcher,
            dirs: BTreeMap::new(),
            failing: std::collections::BTreeSet::new(),
        }
    }

    /// Watch each of `wanted` that exists, again where it was made anew.
    /// Returns whether a watch began.
    fn follow(&mut self, wanted: &[(PathBuf, RecursiveMode)]) -> bool {
        use std::os::unix::fs::MetadataExt;
        let mut began = false;
        for (dir, mode) in wanted {
            let now = fs::metadata(dir)
                .ok()
                .filter(fs::Metadata::is_dir)
                .map(|m| (m.dev(), m.ino(), m.created().ok()));
            let was = self.dirs.get(dir).copied();
            if now == was {
                continue;
            }
            if was.is_some() {
                _ = self.watcher.unwatch(dir);
                _ = self.dirs.remove(dir);
            }
            let Some(id) = now else { continue };
            match self.watcher.watch(dir, *mode) {
                Ok(()) => {
                    info!("watching {}", dir.display());
                    _ = self.dirs.insert(dir.clone(), id);
                    _ = self.failing.remove(dir);
                    began = true;
                }
                Err(e) => {
                    if self.failing.insert(dir.clone()) {
                        warn!(
                            "watching {}: {e} (the ten-minute look over everything still finds its changes)",
                            dir.display()
                        );
                    }
                }
            }
        }
        began
    }
}

/// The binary this process runs, to notice a package upgrade replacing it.
struct Exe {
    path: PathBuf,
    id: (u64, u64),
}

impl Exe {
    fn running() -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let path = fs::read_link("/proc/self/exe").ok()?;
        // the running file itself, even once unlinked
        let m = fs::metadata("/proc/self/exe").ok()?;
        Some(Self {
            path,
            id: (m.dev(), m.ino()),
        })
    }

    /// Whether another file is at the binary's path now: an upgrade. Only
    /// removed is not replaced; there'd be nothing to restart into.
    fn replaced(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) != self.id)
    }
}

/// How long an idle device goes without a full sync, change marker or not.
const FULL_SYNC: Duration = Duration::from_secs(600);

/// What came of a sync.
enum Synced {
    Ok,
    /// Try again at the next check.
    Failed,
    /// The storage refuses this computer's key: only a new one helps, and
    /// saving it restarts the daemon.
    Refused,
    /// Sync must stop until a human looks.
    Stop,
}

/// One sync.
fn run_sync(engine: &mut Engine, waiting: &mut bool, alerts: Option<&mut alerts::Alerts>) -> Synced {
    match engine.sync() {
        Ok(st) => {
            *waiting = false;
            if st != omacloud_core::Stats::default() {
                info!("synced: {st:?}");
            }
            return Synced::Ok;
        }
        Err(e) if e.downcast_ref::<Membership>() == Some(&Membership::NotApproved) => {
            if !*waiting {
                info!(
                    "waiting for approval: run `omacloud devices approve {}` on another device",
                    fingerprint(&engine.device_key())
                );
                *waiting = true;
            }
        }
        // a key change this computer missed: it may yet come back, but only
        // a person can give it the new key
        Err(e) if key_refused(&e) => {
            warn!("the storage refused this computer's key, will retry: {e:#}");
            if let Some(a) = alerts {
                a.key_refused();
            }
            return Synced::Refused;
        }
        // a rotation in progress, or a grant not written yet: wait
        Err(e) if fail::is_waiting(&e) => {
            info!("waiting: {e}");
        }
        // removed from the account, or a coordinator we can't verify: stop
        // until a human looks
        Err(e) if e.downcast_ref::<Membership>().is_some() || fail::is_unverified(&e) => {
            log::error!("refusing to sync: {e}");
            if e.downcast_ref::<Membership>() == Some(&Membership::Revoked)
                && let Some(a) = alerts
            {
                a.removed();
            }
            return Synced::Stop;
        }
        Err(e) => warn!("sync failed, will retry: {e:#}"),
    }
    Synced::Failed
}

fn main() -> std::process::ExitCode {
    // printing into a closed pipe (`omacloud devices | head`) ends quietly.
    // Not through SIGPIPE: a network connection the provider closed is a
    // broken pipe too, and the default disposition would kill the daemon on
    // its next request instead of letting it retry.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or("");
        if message.contains("failed printing to stdout") && message.contains("Broken pipe") {
            std::process::exit(0);
        }
        default_hook(info);
    }));
    let cli = match output::parse::<Cli>() {
        Ok(cli) => cli,
        Err(code) => return code.into(),
    };
    let daemon = matches!(cli.cmd, Cmd::Watch { .. });
    // the daemon's stdout and stderr are its journal: text, unless asked
    let mode = if daemon { cli.out.asked_mode() } else { cli.out.mode() };
    _ = MODE.set(mode);
    // a program reading the output gets warnings, not progress; the
    // daemon's journal gets both. OpenDAL's retries are left out: one that
    // finally fails is an error
    let filter = if mode.is_machine() && !daemon {
        "warn,opendal::layers::retry=error,tracing=warn"
    } else {
        "info,rustic_core=warn,opendal::layers::retry=error,tracing=warn"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(filter)).init();
    if let Some(home) = agent_skill::home() {
        agent_skill::refresh(&home, &SKILL);
    }
    if let Cmd::Watch { poll } = cli.cmd {
        // the daemon: systemd acts on these exit codes (contrib/omacloud.service)
        let ended = Paths::new(cli.config).and_then(|paths| watch(&paths, &load(&paths)?, Duration::from_secs(poll)));
        match ended {
            Ok(Ended::Upgraded) => std::process::exit(UPGRADED),
            // the account stopped (this device was removed): a human looks
            Ok(Ended::Stopped) => std::process::exit(2),
            Err(e) => {
                let e = fail::classify(&e);
                // set up on storage that's gone: restarting won't help
                let stop = e.code == "storage_gone";
                e.report(mode);
                std::process::exit(if stop { 2 } else { 1 })
            }
        }
    }
    let code = match run(cli) {
        Ok(response) => response.print(mode),
        Err(e) => fail::classify(&e).report(mode),
    };
    code.into()
}

/// Asks before something that can't be undone, where someone can answer;
/// elsewhere `--yes` is needed.
fn confirm(yes: bool, what: &str, question: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !output::interactive(mode()) {
        return Err(fail::confirmation_required(what).into());
    }
    eprint!("{question} [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if matches!(line.trim(), "y" | "Y" | "yes") {
        Ok(())
    } else {
        Err(CliError::new("cancelled", output::EXIT_ERROR, "cancelled").into())
    }
}

fn skill(action: Option<SkillCmd>) -> Out {
    let Some(action) = action else {
        return Ok(Response::new(
            json!({ "name": SKILL.name, "body": SKILL.body }),
            "the omacloud agent skill",
        )
        .human(SKILL.body.trim_end()));
    };
    let home = agent_skill::home().context("no home directory to install the skill into")?;
    let (out, done) = match action {
        SkillCmd::Install => (agent_skill::install(&home, &SKILL)?, "Installed"),
        SkillCmd::Remove => (agent_skill::remove(&home, &SKILL)?, "Removed"),
    };
    let mut text = String::new();
    for p in &out.paths {
        say!(text, "{done} {}", p.display());
    }
    for n in &out.notices {
        say!(text, "{n}");
    }
    let summary = if out.paths.is_empty() && out.notices.is_empty() {
        "Nothing to do.".to_string()
    } else {
        format!("{done} {} paths", out.paths.len())
    };
    let data = json!({ "paths": out.paths, "notices": out.notices });
    Ok(Response::new(data, summary.clone()).human(if text.is_empty() { summary } else { text }))
}

fn run(cli: Cli) -> Out {
    if let Cmd::Skill { action } = cli.cmd {
        return skill(action);
    }
    let paths = Paths::new(cli.config)?;
    match cli.cmd {
        Cmd::Init {
            folder,
            bucket,
            repo,
            coordinator,
            folders: chosen,
            settings,
            device,
        } => {
            // secrets never go on a command line
            let code = |var| std::env::var(var).ok().filter(|c: &String| !c.trim().is_empty());
            let (recovery_code, join_code) = (code("OMACLOUD_RECOVERY_CODE"), code("OMACLOUD_JOIN_CODE"));
            if set_aside_old_setup(&paths)? {
                eprintln!(
                    "The old setup ({}) is kept aside as .before-bucket; setting up in your storage.",
                    paths.config.display()
                );
            }
            if paths.config.exists() {
                return Err(already_set_up(&paths).into());
            }
            if repo.as_deref().is_some_and(|r| r.starts_with("opendal:")) {
                return Err(fail::usage("--repo takes a path; a bucket is --provider and --bucket").into());
            }
            // folder names are checked before anything is made here
            let chosen = chosen
                .map(|c| -> Result<Vec<String>> {
                    let home = home_dir()?;
                    // none chosen (`--folders ""`): settings only
                    c.iter()
                        .filter(|n| !n.trim().is_empty())
                        .map(|n| folder_name(&home, n))
                        .collect()
                })
                .transpose()?;
            let target = init_target(
                coordinator,
                repo,
                &bucket,
                join_code.as_deref(),
                recovery_code.is_some(),
            )?;
            let (folder, folders) = match folder {
                Some(f) => (absolute(&f)?, None),
                None => {
                    // folders this computer points at home itself don't exist
                    let home = home_dir()?;
                    let at_home = xdg_at_home(&home);
                    // chosen: those sync here, and the other usual ones are
                    // skipped here even if the account has them
                    let skip = chosen.as_ref().map_or_else(Default::default, |c| {
                        USUAL_FOLDERS
                            .iter()
                            .filter(|u| !c.iter().any(|n| n == *u))
                            .filter(|u| !at_home.iter().any(|h| h == *u))
                            .map(|u| (*u).to_string())
                            .collect()
                    });
                    let add = chosen
                        .map_or_else(
                            || DEFAULT_FOLDERS.iter().map(|s| (*s).to_string()).collect(),
                            |c| c.into_iter().collect::<std::collections::BTreeSet<_>>(),
                        )
                        .into_iter()
                        .filter(|n| !at_home.contains(n))
                        .collect();
                    (
                        home,
                        Some(FolderChoice {
                            add,
                            skip,
                            exclude: std::collections::BTreeSet::new(),
                        }),
                    )
                }
            };
            let device = device.unwrap_or_else(hostname);
            let config = Config {
                device,
                folder,
                coordinator: target.coordinator,
                repo: target.repo,
                root: String::new(),
                limits: Limits::default(),
                settings,
                settings_skip: std::collections::BTreeSet::new(),
                notifications: true,
                folders,
                coordination_bucket: target.coordination_bucket,
                bucket_key_seq: 0,
            };
            init(&paths, config, recovery_code, target.account)
        }
        Cmd::Sync => {
            let config = load(&paths)?;
            let mut e = engine(&paths, &config)?;
            record_packages(&config, &mut e);
            let st = match e.sync() {
                Err(err) if err.downcast_ref::<Membership>() == Some(&Membership::NotApproved) => {
                    let me = fingerprint(&e.device_key());
                    return Err(
                        fail::setup("not_approved", format!("this device ({me}) is not approved yet"))
                            .hint(format!("run `omacloud devices approve {me}` on a device that syncs"))
                            .into(),
                    );
                }
                other => other?,
            };
            let summary = format!("pushed {}, pulled {}, conflicts {}", st.pushed, st.pulled, st.conflicts);
            follow_bucket_key(&paths, &mut e)?;
            let data = json!({
                "pushed": st.pushed,
                "pulled": st.pulled,
                "conflicts": st.conflicts,
            });
            Ok(Response::new(data, summary))
        }
        Cmd::Watch { .. } => unreachable!("handled before the other commands"),
        Cmd::Skill { .. } => unreachable!("handled before loading the config"),
        Cmd::Status => status(&paths),
        Cmd::DeviceKey => {
            let key = public_hex(&load_or_create_key(&paths.device_key())?);
            let fp = fingerprint(&key);
            Ok(Response::new(
                json!({ "public_key": key, "fingerprint": fp }),
                format!("fingerprint {fp}"),
            )
            .human(format!("{key}\nfingerprint {fp}"))
            .ids(vec![fp]))
        }
        Cmd::Devices { action } => devices(&paths, action.unwrap_or(DevicesCmd::List)),
        Cmd::Versions { path } => {
            let config = load(&paths)?;
            let mut e = engine(&paths, &config)?;
            let rel = relative(&config, &mut e, &path)?;
            let versions = e.versions(&rel)?;
            if versions.is_empty() {
                return Err(fail::not_found(format!("no versions of {}", rel.display()))
                    .hint("give a path inside the synced folder, as it was synced")
                    .into());
            }
            let mut out = String::new();
            let mut data = Vec::new();
            for v in &versions {
                let what = v.size.map_or_else(|| "deleted".to_string(), |s| format!("{s} bytes"));
                say!(
                    out,
                    "{:>6}  {}  {:<12}  {what}",
                    v.seq,
                    local_time(v.time, "%Y-%m-%d %H:%M:%S"),
                    v.device
                );
                data.push(json!({
                    "seq": v.seq,
                    "time": v.time.to_string(),
                    "device": v.device,
                    "size": v.size,
                    "deleted": v.size.is_none(),
                }));
            }
            let ids = versions.iter().map(|v| v.seq.to_string()).collect();
            Ok(Response::new(
                data,
                format!("{} of {}", counted(versions.len(), "version"), rel.display()),
            )
            .human(out.trim_end())
            .ids(ids))
        }
        Cmd::Deleted { days } => {
            let config = load(&paths)?;
            let now = omacloud_core::jiff::Timestamp::now();
            let since = omacloud_core::jiff::Timestamp::from_second(now.as_second() - i64::from(days) * 86_400)?;
            let mut e = engine(&paths, &config)?;
            e.load_folders()?;
            let mut gone = e.deleted(since)?;
            // only what syncs here: a folder skipped here has none to put back
            gone.retain(|d| e.syncs(&d.path));
            let mut out = String::new();
            let mut data = Vec::new();
            for d in &gone {
                let path = e.on_disk(&d.path);
                say!(
                    out,
                    "{}  {:<12}  {}",
                    local_time(d.time, "%Y-%m-%d %H:%M"),
                    d.device,
                    d.path.display()
                );
                data.push(json!({
                    "path": path,
                    "relative": d.path,
                    "deleted_at": d.time.to_string(),
                    "device": d.device,
                    "seq": d.seq,
                    "version": d.last,
                    "size": d.size,
                }));
            }
            if let Some(d) = gone.first() {
                let path = e.on_disk(&d.path);
                say!(
                    out,
                    "\nput one back: omacloud restore {p} --at {} --to {p}",
                    d.last,
                    p = path.display()
                );
            }
            let ids = gone.iter().map(|d| d.path.display().to_string()).collect();
            Ok(Response::new(
                data,
                format!("{} deleted in the last {days} days", counted(gone.len(), "file")),
            )
            .human(if gone.is_empty() {
                format!("nothing deleted in the last {days} days")
            } else {
                out.trim_end().to_string()
            })
            .ids(ids))
        }
        Cmd::Mount { dir } => {
            // before any thread starts, so Ctrl-C reaches the unmount
            mount::block_signals();
            let config = load(&paths)?;
            let dir = match dir {
                Some(d) => std::path::absolute(d)?,
                None => home_dir()?.join("Omacloud"),
            };
            let mut e = engine(&paths, &config)?;
            e.load_folders()?;
            let inside = if config.folders.is_some() {
                e.synced_folders().iter().any(|f| dir.starts_with(f))
            } else {
                dir.starts_with(&config.folder)
            };
            if inside {
                return Err(fail::usage(format!("{} is inside a synced folder", dir.display()))
                    .hint("mount outside the folders that sync, such as ~/Omacloud")
                    .into());
            }
            fs::create_dir_all(&dir)?;
            if fs::read_dir(&dir)?.next().is_some() {
                return Err(fail::usage(format!("{} isn't empty", dir.display()))
                    .hint("give an empty folder, or a new one")
                    .into());
            }
            let browse = e.browse()?;
            let mut seq = browse.seq;
            eprintln!(
                "{} shows every file as of version {seq}, read-only; each is fetched when opened.\n\
                 Ctrl-C (or `fusermount3 -u {}`) unmounts it.",
                dir.display(),
                dir.display()
            );
            // the marker is one small read; the history is read again only
            // when it changed
            let mut marker = e.remote_marker().ok().flatten();
            mount::run(
                mount::View::new(browse),
                &dir,
                std::time::Duration::from_secs(60),
                &mut || {
                    let now = e.remote_marker().ok().flatten();
                    if now.is_some() && now == marker {
                        return None;
                    }
                    match e.browse() {
                        Ok(b) => {
                            marker = now;
                            (b.seq != seq).then(|| {
                                seq = b.seq;
                                b
                            })
                        }
                        Err(err) => {
                            warn!("couldn't refresh the view: {err:#}");
                            None
                        }
                    }
                },
            )?;
            Ok(Response::new(
                json!({ "dir": dir }),
                format!("unmounted {}", dir.display()),
            ))
        }
        Cmd::Restore { path, at, to, yes } => {
            let config = load(&paths)?;
            let mut e = engine(&paths, &config)?;
            let rel = relative(&config, &mut e, &path)?;
            let dest = to.unwrap_or_else(|| {
                let name = rel.file_name().map_or_else(Default::default, |n| n.to_string_lossy());
                PathBuf::from(format!("{name}.v{at}"))
            });
            if dest.exists() && !yes {
                if !output::interactive(mode()) {
                    return Err(CliError::new(
                        "exists",
                        output::EXIT_USAGE,
                        format!("{} already exists", dest.display()),
                    )
                    .hint("pass --yes to replace it, or --to another path")
                    .into());
                }
                confirm(false, "replacing it", &format!("replace {}?", dest.display()))?;
            }
            e.restore_version(&rel, at, &dest)
                .map_err(|e| missing_version(e, &rel, at))?;
            Ok(Response::new(
                json!({ "path": rel, "at": at, "written": dest }),
                format!("wrote {}", dest.display()),
            ))
        }
        Cmd::Settings { action } => settings(&paths, action.unwrap_or(SettingsCmd::Status)),
        Cmd::Secrets { action } => secrets(&paths, action.unwrap_or(SecretsCmd::List)),
        Cmd::JoinCode => {
            if !output::to_terminal(mode()) {
                return Err(fail::terminal_required("a join code (it holds the key to your storage)").into());
            }
            let config = load(&paths)?;
            if config.coordinator != BUCKET {
                return Err(
                    fail::unsupported("this account keeps in step through a shared directory, not a bucket")
                        .hint("other computers join with `omacloud init --repo <path> --coordinator <directory>`")
                        .into(),
                );
            }
            let code = JoinCode {
                repo: config.repo.clone(),
                coordination_bucket: config.coordination_bucket.clone(),
                account: Some(config.root.clone()),
            }
            .encode()?;
            eprintln!(
                "Pass it privately: it holds the key to your storage (it can't open your files). The new\n\
                 computer runs `omacloud init` with it in OMACLOUD_JOIN_CODE (or Add This Computer\n\
                 in the app), and then needs your approval (or your recovery code)."
            );
            Ok(Response::new(Value::Null, "join code").human(code))
        }
        Cmd::Invite { bucket } => {
            let mut repo = bucket.repo()?;
            if repo.is_dropbox() {
                return Err(fail::usage("Dropbox needs no invite")
                    .hint("each person runs `omacloud init --provider dropbox` and signs in to their own Dropbox")
                    .into());
            }
            if !output::to_terminal(mode()) {
                return Err(fail::terminal_required("an invite (it holds the bucket's key)").into());
            }
            _ = repo.options.insert("secret_access_key".into(), read_key_secret()?);
            let coord = BucketCoordinator::beside(&repo, bucket.coordination_bucket.as_deref())?;
            check_bucket(&coord)?;
            if anchor_of(&coord)?.is_some() {
                return Err(CliError::new(
                    "exists",
                    output::EXIT_USAGE,
                    "this bucket already holds an account",
                )
                .hint("each person gets a bucket and key of their own; another computer of theirs uses `omacloud join-code`")
                .into());
            }
            let code = JoinCode {
                repo,
                coordination_bucket: bucket.coordination_bucket.clone(),
                account: None,
            }
            .encode()?;
            eprintln!(
                "Pass it privately: it holds this bucket's key. They run `omacloud init` with it in\n\
                 OMACLOUD_JOIN_CODE (or paste it under Or Use a Code in the app) and get a recovery code only they hold:\n\
                 you can't read their files, nor they yours."
            );
            Ok(Response::new(Value::Null, "invite").human(code))
        }
        Cmd::Folders { action } => folders(&paths, action.unwrap_or(FoldersCmd::List)),
        Cmd::Bucket { action } => bucket(&paths, action.unwrap_or(BucketCmd::Status)),
        Cmd::Recovery { action } => recovery(&paths, action),
        Cmd::Packages { action } => packages(&paths, action.unwrap_or(PackagesCmd::List)),
        Cmd::RestoreMachine { from, yes } => {
            let mut config = load(&paths)?;
            if !config.settings {
                config.settings = true;
                save(&paths, &config)?;
            }
            let mut e = engine(&paths, &config)?;
            let mut out = settings_text(&e.settings_status());
            record_packages(&config, &mut e);
            let st = e.sync()?;
            say!(out, "files and settings: pulled {}", st.pulled);
            let packages = packages(
                &paths,
                PackagesCmd::Restore {
                    from: from.clone(),
                    yes,
                },
            )?;
            say!(out, "{}", packages.text());
            // secrets only ever on request, with the recovery code
            let has_secrets = e.secrets_devices().contains(&from);
            if has_secrets {
                say!(
                    out,
                    "{from} saved a secrets bundle (ssh and gpg keys, tokens). To put it here: \
                     omacloud secrets restore --from {from}"
                );
            }
            // the packaged unit runs for users with a config; start it now
            let started = std::process::Command::new("systemctl")
                .args(["--user", "start", "omacloud"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if started {
                say!(out, "keeping in sync from now on (systemd user unit omacloud)");
            } else {
                say!(
                    out,
                    "to keep in sync, run `omacloud watch` (or install the package for its unit)"
                );
            }
            let data = json!({
                "from": from,
                "pulled": st.pulled,
                "packages": packages.data(),
                "secrets_bundle": has_secrets,
                "daemon_started": started,
            });
            let response = Response::new(data, format!("set up from {from}")).human(out.trim_end());
            // everything but the daemon is done: say so, and exit 5
            Ok(if started {
                response
            } else {
                response.exit(output::EXIT_UNAVAILABLE)
            })
        }
        Cmd::Rotate { yes } => {
            let config = load(&paths)?;
            confirm(
                yes,
                "rotating the repository key",
                "move to a new repository key? This uploads everything again and deletes the old repository",
            )?;
            let r = engine(&paths, &config)?.rotate()?;
            let summary = format!(
                "moved to repository epoch {} ({} snapshots copied)",
                r.epoch, r.snapshots
            );
            let data = json!({ "epoch": r.epoch, "snapshots": r.snapshots, "left": r.left });
            Ok(Response::new(data, summary.clone()).human(format!("{summary}\n{}", leftover_text(&r))))
        }
        Cmd::Export { to } => {
            if !output::to_terminal(mode()) {
                return Err(fail::terminal_required("the repository password").into());
            }
            let config = load(&paths)?;
            let mut e = engine(&paths, &config)?;
            e.sync()?; // pick up any key rotation first
            let (mut spec, secret, epoch) = e.repository()?;
            let password = secret.password;
            if let Some(to) = to {
                let dest = RepoSpec {
                    repository: to,
                    options: BTreeMap::new(),
                };
                let n = spec.copy_files_to(&dest)?;
                eprintln!("copied {n} files to {}", dest.repository);
                spec = dest;
            }
            let repo = &spec.repository;
            let mut out = String::new();
            if spec.is_dropbox() {
                say!(
                    out,
                    "# restore without omacloud: restic reads Dropbox through rclone (a remote you\n\
                     # set up, here called dropbox), or copy it first with `omacloud export --to <folder>`\n\
                     # (repository epoch {epoch})\n\
                     RESTIC_PASSWORD='{password}' \\\n  \
                     restic -r rclone:dropbox:Apps/Omacloud{} restore latest --target <dir>",
                    spec.options.get("root").map_or("", |r| r.trim_end_matches('/'))
                );
                return Ok(Response::new(Value::Null, "export").human(out.trim_end()));
            }
            if spec.is_remote() {
                let opt = |k: &str| spec.options.get(k).cloned().unwrap_or_default();
                say!(
                    out,
                    "# restore without omacloud: restic or rustic, this bucket and password\n\
                     # (repository epoch {epoch})\n\
                     AWS_ACCESS_KEY_ID='{}' AWS_SECRET_ACCESS_KEY='{}' AWS_DEFAULT_REGION='{}' \\\n  \
                     RESTIC_PASSWORD='{password}' \\\n  \
                     restic -r s3:{}/{}{} restore latest --target <dir>",
                    opt("access_key_id"),
                    opt("secret_access_key"),
                    opt("region"),
                    opt("endpoint"),
                    opt("bucket"),
                    opt("root").trim_end_matches('/')
                );
                return Ok(Response::new(Value::Null, "export").human(out.trim_end()));
            }
            say!(
                out,
                "# restore without omacloud: restic or rustic, this repository and password"
            );
            say!(out, "# (repository epoch {epoch})");
            say!(out, "repository: {repo}");
            say!(out, "password:   {password}");
            say!(out);
            say!(
                out,
                "RESTIC_PASSWORD='{password}' restic -r {repo} restore latest --target <dir>"
            );
            Ok(Response::new(Value::Null, "export").human(out.trim_end()))
        }
    }
}

/// `t` in this computer's time zone, as `fmt` lays it out.
fn local_time(t: omacloud_core::jiff::Timestamp, fmt: &str) -> String {
    t.to_zoned(omacloud_core::jiff::tz::TimeZone::system())
        .strftime(fmt)
        .to_string()
}

/// `n` things, as "1 device" or "2 devices".
fn counted(n: usize, thing: &str) -> String {
    if n == 1 {
        format!("1 {thing}")
    } else {
        format!("{n} {thing}s")
    }
}

/// A restore of a version or path that isn't in the history: not found.
fn missing_version(e: anyhow::Error, rel: &Path, at: u64) -> anyhow::Error {
    // the engine says so in its own words, with no underlying error
    let message = e.to_string();
    if e.chain().count() == 1 && (message == format!("no head {at}") || message.contains(" is not a file at head ")) {
        return fail::not_found(format!("{} has no version {at}", rel.display()))
            .hint(format!("list them with `omacloud versions {}`", rel.display()))
            .into();
    }
    e
}

/// Devices by key, as `devices` and `status` give them: `me` marks this one.
fn device_list(m: &BTreeMap<String, String>, me: &str) -> Vec<Value> {
    m.iter()
        .map(|(k, name)| json!({ "fingerprint": fingerprint(k), "name": name, "this": k == me }))
        .collect()
}

fn request_list(requests: &[omacloud_core::JoinRequest]) -> Vec<Value> {
    requests
        .iter()
        .map(|r| json!({ "fingerprint": fingerprint(&r.device), "name": r.name }))
        .collect()
}

fn devices(paths: &Paths, action: DevicesCmd) -> Out {
    let config = load(paths)?;
    let mut e = engine(paths, &config)?;
    let me = e.device_key();
    let chain = e.refresh_devices()?.clone();
    let members = chain.members();
    // a fingerprint prefix, dashes optional, matched against `keys`
    let pick = |fp: &str, keys: Vec<(String, String)>| -> Result<(String, String)> {
        let want: String = fp.chars().filter(|c| *c != '-').collect();
        let mut hits: Vec<_> = keys
            .into_iter()
            .filter(|(k, _)| fingerprint(k).replace('-', "").starts_with(&want))
            .collect();
        match hits.len() {
            1 => Ok(hits.remove(0)),
            0 => Err(fail::not_found(format!("no device with fingerprint {fp}"))
                .hint("run `omacloud devices list`")
                .into()),
            _ => Err(
                CliError::new("ambiguous", output::EXIT_USAGE, format!("{fp} matches several devices"))
                    .hint("give more of the fingerprint")
                    .into(),
            ),
        }
    };
    match action {
        DevicesCmd::List => {
            let requests = e.requests()?;
            let mut out = String::new();
            say!(out, "account root {}", fingerprint(&chain.root));
            say!(out, "\ndevices:");
            for (k, name) in &members.valid {
                let this = if *k == me { "  (this device)" } else { "" };
                say!(out, "  {}  {name}{this}", fingerprint(k));
            }
            if !members.revoked.is_empty() {
                say!(out, "\nremoved:");
                for (k, name) in &members.revoked {
                    say!(out, "  {}  {name}", fingerprint(k));
                }
            }
            if !requests.is_empty() {
                say!(out, "\nasking to join (compare with the fingerprint the device shows):");
                for r in &requests {
                    say!(out, "  {}  {}", fingerprint(&r.device), r.name);
                }
            }
            let data = json!({
                "account_fingerprint": fingerprint(&chain.root),
                "devices": device_list(&members.valid, &me),
                "removed": device_list(&members.revoked, &me),
                "requests": request_list(&requests),
            });
            let ids: Vec<String> = members.valid.keys().map(|k| fingerprint(k)).collect();
            let summary = format!(
                "{}, {} asking to join",
                counted(members.valid.len(), "device"),
                requests.len()
            );
            Ok(Response::new(data, summary).human(out.trim_end()).ids(ids))
        }
        DevicesCmd::Approve { fingerprint: fp } => {
            let requests = e.requests()?;
            let keys = requests.iter().map(|r| (r.device.clone(), r.name.clone())).collect();
            let (key, name) = pick(&fp, keys)?;
            let req = requests
                .into_iter()
                .find(|r| r.device == key)
                .context("request vanished")?;
            e.approve(&req)?;
            let fp = fingerprint(&key);
            Ok(Response::new(
                json!({ "approved": { "fingerprint": fp, "name": name } }),
                format!("approved {name} ({fp})"),
            )
            .ids(vec![fp]))
        }
        DevicesCmd::Revoke {
            fingerprint: fp,
            rotate,
            lost,
            yes,
        } => {
            let (key, name) = pick(&fp, members.valid.into_iter().collect())?;
            if key == me {
                return Err(fail::usage("this is the device being removed")
                    .hint("remove this device from another one")
                    .into());
            }
            confirm(
                yes,
                "removing a device",
                &format!("remove {name} ({}) from the account?", fingerprint(&key)),
            )?;
            e.revoke(&key)?;
            let mut out = String::new();
            say!(out, "removed {name} ({}). It can't push anymore.", fingerprint(&key));
            let mut rotated = Value::Null;
            if rotate || lost {
                let r = e.rotate()?;
                say!(
                    out,
                    "moved to repository epoch {} ({} snapshots copied); {name} can't read it",
                    r.epoch,
                    r.snapshots
                );
                say!(out, "{}", leftover_text(&r));
                rotated = json!({ "epoch": r.epoch, "snapshots": r.snapshots, "left": r.left });
            } else {
                say!(
                    out,
                    "It still holds the repository key it had, and the key to your storage. If the device is \
                     lost or not trusted, run `omacloud rotate` and `omacloud bucket set-key`."
                );
            }
            if lost && config.repo.is_dropbox() {
                drop(e);
                bucket(
                    paths,
                    BucketCmd::SetKey {
                        access_key_id: None,
                        here_only: false,
                    },
                )
                .map_err(|err| err.context(format!("{name} is removed; finish with `omacloud bucket set-key`")))?;
                say!(
                    out,
                    "The old Dropbox token is revoked once your other computers have switched."
                );
            } else if lost {
                say!(
                    out,
                    "It still holds the bucket's key: make a new one at your provider, and run `omacloud bucket set-key`."
                );
            }
            let fp = fingerprint(&key);
            let data = json!({ "removed": { "fingerprint": fp, "name": name }, "rotated": rotated });
            Ok(Response::new(data, format!("removed {name} ({fp})"))
                .human(out.trim_end())
                .ids(vec![fp]))
        }
    }
}

/// This machine's explicitly installed packages, repository and foreign
/// (AUR) apart; `None` without pacman.
fn package_list(device: &str) -> Option<String> {
    let run = |args: &[&str]| -> Option<Vec<String>> {
        let out = std::process::Command::new("pacman").args(args).output().ok()?;
        out.status.success().then(|| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
    };
    let all = run(&["-Qqe"])?;
    let foreign = run(&["-Qqem"]).unwrap_or_default();
    let repo: Vec<&String> = all.iter().filter(|p| !foreign.contains(p)).collect();
    let mut text = format!("# explicitly installed packages on {device}\n[repo]\n");
    for p in repo {
        text.push_str(p);
        text.push('\n');
    }
    text.push_str("[foreign]\n");
    for p in &foreign {
        text.push_str(p);
        text.push('\n');
    }
    Some(text)
}

/// Record this device's package list when settings sync is on.
fn record_packages(config: &Config, e: &mut Engine) {
    if !config.settings {
        return;
    }
    if let Some(list) = package_list(&config.device)
        && let Err(err) = e.update_package_list(&list)
    {
        warn!("recording the package list: {err:#}");
    }
}

fn packages(paths: &Paths, action: PackagesCmd) -> Out {
    let config = load(paths)?;
    if !config.settings {
        return Err(fail::settings_off("package lists").into());
    }
    let mut e = engine(paths, &config)?;
    record_packages(&config, &mut e);
    e.sync()?;
    let dir = paths.data.join("packages");
    let read = |device: &str| -> Result<(Vec<String>, Vec<String>)> {
        let text = match fs::read_to_string(dir.join(format!("{device}.txt"))) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(fail::not_found(format!("no package list from {device}"))
                    .hint("run `omacloud packages list` for the devices that have one")
                    .into());
            }
            other => other.with_context(|| format!("reading the package list from {device}"))?,
        };
        let (mut repo, mut foreign, mut section) = (Vec::new(), Vec::new(), "");
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            match line {
                "[repo]" | "[foreign]" => section = line,
                p if section == "[repo]" => repo.push(p.to_string()),
                p => foreign.push(p.to_string()),
            }
        }
        Ok((repo, foreign))
    };
    match action {
        PackagesCmd::List => {
            let mut devices: Vec<String> = fs::read_dir(&dir)
                .map(|it| {
                    it.flatten()
                        .filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".txt").map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            devices.sort();
            let (mut out, mut data) = (String::new(), Vec::new());
            for d in &devices {
                let (repo, foreign) = read(d)?;
                say!(out, "{d:<16} {} packages, {} from the AUR", repo.len(), foreign.len());
                data.push(json!({ "device": d, "packages": repo.len(), "aur": foreign.len() }));
            }
            Ok(
                Response::new(data, format!("{} with package lists", counted(devices.len(), "device")))
                    .human(if out.is_empty() {
                        "no package lists yet".to_string()
                    } else {
                        out.trim_end().to_string()
                    })
                    .ids(devices),
            )
        }
        PackagesCmd::Restore { from, yes } => {
            let (repo, foreign) = read(&from)?;
            let installed: std::collections::BTreeSet<String> = std::process::Command::new("pacman")
                .arg("-Qq")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect())
                .unwrap_or_default();
            let missing: Vec<&String> = repo.iter().filter(|p| !installed.contains(*p)).collect();
            let missing_aur: Vec<&String> = foreign.iter().filter(|p| !installed.contains(*p)).collect();
            let mut data = json!({ "from": from, "missing": missing, "missing_aur": missing_aur, "installed": false });
            if missing.is_empty() && missing_aur.is_empty() {
                return Ok(Response::new(data, format!("everything {from} has is installed here")));
            }
            let mut out = String::new();
            let list = |v: &[&String]| v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
            if !missing.is_empty() {
                say!(out, "from the repositories ({}): {}", missing.len(), list(&missing));
            }
            if !missing_aur.is_empty() {
                say!(
                    out,
                    "from the AUR ({}), install these yourself: {}",
                    missing_aur.len(),
                    list(&missing_aur)
                );
            }
            if yes && !missing.is_empty() {
                let mut pacman = std::process::Command::new("sudo");
                pacman.args(["pacman", "-S", "--needed"]);
                // --yes said it already where nobody can answer pacman
                if !output::interactive(mode()) {
                    pacman.arg("--noconfirm");
                }
                // stdout is the result's; pacman talks on stderr
                if mode().is_machine() {
                    pacman.stdout(std::io::stderr());
                } else {
                    // what it found goes out before pacman asks
                    eprint!("{out}");
                    out.clear();
                }
                let status = pacman.args(missing.iter().map(|s| s.as_str())).status().map_err(|e| {
                    CliError::new(
                        "unavailable",
                        output::EXIT_UNAVAILABLE,
                        format!("running sudo pacman: {e}"),
                    )
                })?;
                if !status.success() {
                    return Err(CliError::new("command_failed", output::EXIT_ERROR, "pacman did not finish").into());
                }
                data["installed"] = json!(true);
                say!(out, "installed {} packages from the repositories", missing.len());
            } else if !missing.is_empty() {
                say!(out, "install with: omacloud packages restore --from {from} --yes");
            }
            let summary = format!(
                "{} packages and {} from the AUR missing here",
                missing.len(),
                missing_aur.len()
            );
            Ok(Response::new(data, summary)
                .human(out.trim_end())
                .count(missing.len() + missing_aur.len()))
        }
    }
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "device".to_string())
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME not set")
}

/// The groups of settings a computer can leave out.
const SETTINGS_GROUPS: &[&str] = &["shell", "agents", "apps", "theme", "chromium", "wifi"];

fn settings(paths: &Paths, action: SettingsCmd) -> Out {
    let mut config = load(paths)?;
    let skipping = matches!(action, SettingsCmd::Skip { .. });
    match action {
        SettingsCmd::On | SettingsCmd::Off => {
            config.settings = matches!(action, SettingsCmd::On);
            save(paths, &config)?;
            let e = engine(paths, &config)?;
            let st = e.settings_status();
            let mut out = settings_text(&st);
            if config.settings {
                say!(
                    out,
                    "they sync at the next `omacloud sync`, or right away if the daemon runs"
                );
            }
            let summary = if config.settings {
                "settings sync is on"
            } else {
                "settings sync is off"
            };
            Ok(Response::new(settings_json(&st, &config), summary).human(out.trim_end()))
        }
        SettingsCmd::Skip { group } | SettingsCmd::Unskip { group } if !SETTINGS_GROUPS.contains(&group.as_str()) => {
            Err(fail::usage(format!("{group} isn't a group of settings"))
                .hint(format!("the groups are {}", SETTINGS_GROUPS.join(", ")))
                .into())
        }
        SettingsCmd::Skip { group } | SettingsCmd::Unskip { group } => {
            let summary = if skipping {
                _ = config.settings_skip.insert(group.clone());
                format!("{group} stays on this computer")
            } else {
                _ = config.settings_skip.remove(&group);
                format!("{group} syncs again")
            };
            save(paths, &config)?;
            let st = engine(paths, &config)?.settings_status();
            Ok(Response::new(settings_json(&st, &config), summary).human(settings_text(&st).trim_end()))
        }
        SettingsCmd::Status => {
            let st = engine(paths, &config)?.settings_status();
            let summary = match (st.on, &st.dormant) {
                (false, _) => "settings sync is off".to_string(),
                (true, Some(_)) => "settings sync stands down".to_string(),
                (true, None) => format!("settings sync is on, {} waiting to be resolved", st.held.len()),
            };
            Ok(Response::new(settings_json(&st, &config), summary).human(settings_text(&st).trim_end()))
        }
        SettingsCmd::Resolve { path, keep } => {
            let home = home_dir()?;
            let abs = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()?.join(path)
            };
            let rel = abs.strip_prefix(&home).map_err(|_| {
                fail::usage(format!("{} is not under your home", abs.display()))
                    .hint("give a setting like ~/.config/hypr/bindings.lua")
            })?;
            let mut e = engine(paths, &config)?;
            if !e.settings_status().held.iter().any(|p| p.as_path() == rel) {
                return Err(
                    fail::not_found(format!("~/{} is not waiting to be resolved", rel.display()))
                        .hint("`omacloud settings status` lists what is")
                        .into(),
                );
            }
            e.resolve_setting(rel, matches!(keep, Keep::Remote))?;
            e.sync()?;
            let keep = if matches!(keep, Keep::Remote) {
                "remote"
            } else {
                "local"
            };
            Ok(Response::new(
                json!({ "resolved": format!("~/{}", rel.display()), "kept": keep }),
                format!("resolved {}", rel.display()),
            ))
        }
    }
}

fn settings_json(st: &omacloud_core::SettingsStatus, config: &Config) -> Value {
    json!({
        "on": st.on,
        "skipped": config.settings_skip,
        "dormant": st.dormant,
        "held": st.held,
        "groups": st.groups,
        "source": settings_source(),
    })
}

/// Where the list of settings to sync comes from.
fn settings_source() -> &'static str {
    if std::env::var_os("OMARCHY_PATH")
        .map_or_else(|| PathBuf::from("/usr/share/omarchy"), PathBuf::from)
        .join("default/dots/manifest")
        .is_file()
    {
        "Omarchy's dots manifest"
    } else {
        "the dots manifest bundled with omacloud"
    }
}

fn settings_text(st: &omacloud_core::SettingsStatus) -> String {
    let mut out = String::new();
    match (st.on, &st.dormant) {
        (false, _) => say!(out, "settings sync is off (`omacloud settings on`)"),
        (true, Some(why)) => say!(out, "settings sync stands down: {why}"),
        (true, None) => say!(out, "settings sync is on, following {}", settings_source()),
    }
    out.push_str(&groups_text(&st.groups, "  "));
    for p in &st.held {
        say!(
            out,
            "  waiting: ~/{} changed here and on another device; `omacloud settings resolve ~/{} --keep local|remote`",
            p.display(),
            p.display()
        );
    }
    out
}

/// Each settings group and how it stands, one line each, then what to do.
fn groups_text(groups: &[omacloud_core::merged::Item], indent: &str) -> String {
    let mut out = String::new();
    for g in groups {
        let state = match (g.group.as_str(), g.state.as_str()) {
            (_, "synced") => "in sync",
            (_, "new") => "not synced yet",
            ("chromium", "waiting") => "waiting for Chromium to close",
            (_, "waiting") => "waiting to apply",
            (_, "install") => "to install",
            (_, "settled") => "edits settled",
            (_, "refused") => "not applied",
            (_, "failed") => "can't write",
            (_, "conflict") => "to resolve",
            (_, "unavailable") => "not available",
            (_, other) => other,
        };
        say!(out, "{indent}{:<22} {state}: {}", g.title, g.detail);
        for i in &g.install {
            say!(out, "{indent}  install {}: {}", i.name, i.command.join(" "));
        }
        for n in &g.not_synced {
            say!(out, "{indent}  not synced: {n}");
        }
    }
    out
}

/// A path given on the command line, relative to the synced folder, or
/// (for a setting under home) under the settings prefix.
fn relative(config: &Config, e: &mut Engine, path: &Path) -> Result<PathBuf> {
    // the account's folders, those other computers added among them
    e.load_folders()?;
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if let Ok(rel) = abs.strip_prefix(&config.folder) {
        // with folders the folder is home: what's outside them is settings
        if let Some(path) = e.account_path(rel) {
            // a folder skipped here is still the account's folder
            let skipped = config.folders.as_ref().is_some_and(|f| {
                path.components()
                    .next()
                    .and_then(|c| c.as_os_str().to_str())
                    .is_some_and(|n| f.skip.contains(n))
            });
            if e.syncs(&path) || skipped || !config.settings {
                return Ok(path);
            }
        }
    }
    if config.settings
        && let Ok(rest) = abs.strip_prefix(home_dir()?)
    {
        return Ok(Path::new(omacloud_core::settings::PREFIX).join(rest));
    }
    if path.is_relative() {
        return Ok(path.to_path_buf()); // taken as folder relative
    }
    Err(
        fail::usage(format!("{} is not inside {}", path.display(), config.folder.display()))
            .hint("give a path inside the synced folder")
            .into(),
    )
}

/// `p` made absolute, with symlinks resolved as far as it exists. It makes
/// nothing: `init` creates the folder only once it knows it goes ahead.
fn absolute(p: &Path) -> Result<PathBuf> {
    // Resolve what exists (symlinks included); a `..` in the part that does not
    // exist yet is taken lexically, so `new/../B` stores `…/B`. Nothing is created.
    let mut out = PathBuf::new();
    for c in std::path::absolute(p)?.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => {
                out.push(other);
                if let Ok(real) = fs::canonicalize(&out) {
                    out = real;
                }
            }
        }
    }
    Ok(out)
}

fn leftover_text(r: &omacloud_core::Rotated) -> String {
    if r.left == 0 {
        "the old repository is deleted".to_string()
    } else {
        format!(
            "{} or more files of the old repository couldn't be deleted; a later rotation tries again",
            r.left
        )
    }
}

fn secrets(paths: &Paths, action: SecretsCmd) -> Out {
    use omacloud_core::secrets;
    let config = load(paths)?;
    if !config.settings {
        return Err(fail::settings_off("secrets bundles").into());
    }
    let mut e = engine(paths, &config)?;
    let home = home_dir()?;
    match action {
        SecretsCmd::List => {
            e.sync()?;
            let devices = e.secrets_devices();
            let human = if devices.is_empty() {
                "no secrets bundles; save this device's with `omacloud secrets save`".to_string()
            } else {
                devices.join("\n")
            };
            Ok(Response::new(
                &devices,
                format!("{} saved a secrets bundle", counted(devices.len(), "device")),
            )
            .human(human)
            .ids(devices.clone()))
        }
        SecretsCmd::Save => {
            let list_file = paths.config.with_file_name("secrets");
            let list = match fs::read_to_string(&list_file) {
                Ok(text) => Some(text),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => return Err(err).context("reading the secrets list"),
            };
            let entries = secrets::collect(&home, &secrets::paths(list.as_deref())?)?;
            if entries.is_empty() {
                return Err(fail::not_found("none of the secrets paths exist here")
                    .hint(format!("list the paths to save in {}", list_file.display()))
                    .into());
            }
            let mut out = String::new();
            for en in &entries {
                say!(out, "  ~/{}", en.path.display());
            }
            e.put_secrets(&secrets::seal(&config.root, &entries)?)?;
            e.sync()?;
            let summary = format!(
                "sealed {} files for {}; only the recovery code opens them",
                entries.len(),
                config.device
            );
            say!(out, "{summary}");
            let files: Vec<String> = entries.iter().map(|en| format!("~/{}", en.path.display())).collect();
            Ok(
                Response::new(json!({ "device": config.device, "files": files }), summary)
                    .human(out.trim_end())
                    .count(entries.len()),
            )
        }
        SecretsCmd::Restore { from, yes } => {
            e.sync()?;
            if !e.secrets_devices().contains(&from) {
                return Err(fail::not_found(format!("no secrets bundle from {from}"))
                    .hint("`omacloud secrets list` shows the devices that saved one")
                    .into());
            }
            let sealed = e.secrets_of(&from)?;
            let code = read_recovery_code()?;
            let root =
                omacloud_core::devices::root_key(&code).map_err(|e| fail::recovery_code_rejected(format!("{e:#}")))?;
            let entries = secrets::open(&root, &sealed).map_err(|e| fail::recovery_code_rejected(format!("{e:#}")))?;
            let changes = secrets::changes(&home, &entries);
            let list: Vec<Value> = changes
                .iter()
                .map(|en| {
                    let action = if home.join(&en.path).exists() { "replace" } else { "new" };
                    json!({ "path": format!("~/{}", en.path.display()), "action": action })
                })
                .collect();
            let mut data = json!({ "from": from, "changes": list, "written": 0, "backups": null });
            if changes.is_empty() {
                return Ok(Response::new(data, format!("this device already has {from}'s secrets")).count(0));
            }
            let mut out = String::new();
            for c in &list {
                say!(
                    out,
                    "  {:<8} {}",
                    c["action"].as_str().unwrap_or_default(),
                    c["path"].as_str().unwrap_or_default()
                );
            }
            if !yes {
                say!(out, "write these with: omacloud secrets restore --from {from} --yes");
                return Ok(
                    Response::new(data, format!("{} would change", counted(changes.len(), "file")))
                        .human(out.trim_end())
                        .count(changes.len()),
                );
            }
            let stamp = omacloud_core::jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
            let backups = paths.data.join(format!("secrets-backup/{stamp}"));
            let written = secrets::restore(&home, &entries, &backups)?;
            let summary = format!("wrote {} files", written.len());
            say!(out, "{summary}");
            data["written"] = json!(written.len());
            if backups.exists() {
                say!(out, "the files they replaced are in {}", backups.display());
                data["backups"] = json!(backups);
            }
            Ok(Response::new(data, summary).human(out.trim_end()).count(written.len()))
        }
    }
}

/// The recovery code from `OMACLOUD_RECOVERY_CODE`, or asked for on stdin.
fn read_recovery_code() -> Result<String> {
    read_secret("OMACLOUD_RECOVERY_CODE", "recovery code")
}

/// A secret from the environment variable `var`, or asked for on stdin
/// without echoing it: never from the command line, where other processes
/// can read it.
fn read_secret(var: &str, what: &str) -> Result<String> {
    if let Ok(s) = std::env::var(var) {
        return Ok(s);
    }
    // asked only where someone can answer
    if !output::interactive(mode()) {
        return Err(fail::usage(format!("the {what} is needed"))
            .hint(format!("set {var} in the environment, or run this in a terminal"))
            .into());
    }
    eprint!("{what}: ");
    // SAFETY: termios calls on stdin with a zeroed, then filled, struct
    let saved = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        (libc::tcgetattr(libc::STDIN_FILENO, &raw mut t) == 0).then(|| {
            let old = t;
            t.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const t);
            old
        })
    };
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if let Some(old) = saved {
        // SAFETY: restoring the settings read above
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const old) };
        eprintln!();
    }
    read?;
    let secret = line.trim().to_string();
    if secret.is_empty() {
        return Err(fail::usage(format!("no {what} given")).into());
    }
    Ok(secret)
}

fn recovery(paths: &Paths, action: RecoveryCmd) -> Out {
    use omacloud_core::shamir;
    // both print the recovery code or a part of it; combine reads its
    // shares from stdin, which may be a pipe
    let to_terminal = if matches!(action, RecoveryCmd::Combine) {
        output::stdout_to_terminal(mode())
    } else {
        output::to_terminal(mode())
    };
    if !to_terminal {
        let what = match action {
            RecoveryCmd::Split { .. } => "shares of your recovery code",
            RecoveryCmd::Combine => "your recovery code",
        };
        return Err(fail::terminal_required(what).into());
    }
    match action {
        RecoveryCmd::Split { threshold, shares } => {
            let code = read_recovery_code()?;
            let root =
                omacloud_core::devices::root_key(&code).map_err(|e| fail::recovery_code_rejected(format!("{e:#}")))?;
            // on a set up device, make sure it's this account's code
            if let Ok(config) = load(paths)
                && config.root != public_hex(&root)
            {
                return Err(fail::recovery_code_rejected("that recovery code belongs to another account").into());
            }
            let parts = shamir::split(&code, threshold, shares).map_err(|e| fail::usage(format!("{e:#}")))?;
            eprintln!(
                "any {threshold} of these {shares} shares rebuild the recovery code \
                 (`omacloud recovery combine`). Give each to a different person; keep none \
                 together with the others"
            );
            Ok(Response::new(Value::Null, "recovery code shares").human(parts.join("\n")))
        }
        RecoveryCmd::Combine => {
            use std::io::IsTerminal;
            let asking = std::io::stdin().is_terminal();
            if asking {
                eprintln!("paste shares, one per line:");
            }
            let mut got: Vec<String> = Vec::new();
            for line in std::io::stdin().lines() {
                let line = line?;
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let k = match shamir::needed(line) {
                    Ok(k) => k,
                    Err(e) => {
                        // by position: the share itself stays off stderr
                        eprintln!("share {}: {e:#}; try that one again", got.len() + 1);
                        continue;
                    }
                };
                got.push(line.to_string());
                if got.len() >= usize::from(k) {
                    break;
                }
                if asking {
                    eprintln!("{} of {k}", got.len());
                }
            }
            let code = shamir::combine(&got).map_err(|e| fail::setup("shares_rejected", format!("{e:#}")))?;
            Ok(Response::new(Value::Null, "recovery code").human(code))
        }
    }
}

/// `status`: what the Omacloud app shows. Reads only; no sync. A computer
/// not set up yet is a state to show (`set_up: false`), not a failure.
fn status(paths: &Paths) -> Out {
    if !paths.config.exists() {
        return Ok(Response::new(json!({ "set_up": false }), "this computer isn't set up")
            .human("this computer isn't set up: run `omacloud init`, or set it up in the Omacloud app"));
    }
    let config = read_config(paths)?;
    if let Some(kind) = storage_gone(&config) {
        let e = fail::storage_gone(kind);
        return Ok(
            Response::new(json!({ "set_up": false, "storage_gone": kind }), e.message.clone()).human(format!(
                "{}: {}",
                e.message,
                e.hint.clone().unwrap_or_default()
            )),
        );
    }
    let mut e = engine(paths, &config)?;
    let chain = e.refresh_devices()?.clone();
    let members = chain.members();
    let me = e.device_key();
    _ = e.load_folders();
    let st = e.state();
    let repo = e.repository().map_or_else(|_| config.repo.clone(), |r| r.0);
    let bucket_name = repo.options.get("bucket");
    let kind = if repo.is_dropbox() {
        "dropbox"
    } else if repo.is_remote() {
        "bucket"
    } else {
        "path"
    };
    let role = if members.valid.contains_key(&me) {
        "member"
    } else if members.revoked.contains_key(&me) {
        "removed"
    } else {
        "waiting"
    };
    let requests = request_list(&e.requests().unwrap_or_default());
    let settings = e.settings_status();
    let daemon = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "omacloud"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let synced = e.folder_list();

    let mut out = String::new();
    say!(out, "device   {}", config.device);
    say!(out, "account  {}", config.root);
    if config.folders.is_some() {
        let dirs: Vec<String> = synced
            .iter()
            .map(|(_, d)| format!("~/{}", d.file_name().unwrap_or_default().to_string_lossy()))
            .collect();
        say!(out, "folders  {}", dirs.join(", "));
    } else {
        say!(out, "folder   {}", config.folder.display());
    }
    match bucket_name {
        Some(b) => say!(
            out,
            "storage  bucket {b} at {}",
            repo.options.get("endpoint").map_or("", String::as_str)
        ),
        None if repo.is_dropbox() => say!(out, "storage  your Dropbox, its Apps/Omacloud folder"),
        None => say!(out, "repo     {}", repo.repository),
    }
    match &st.base {
        Some(b) => say!(out, "synced   head {} (snapshot {})", b.seq, &b.snapshot[..8]),
        None => say!(out, "synced   nothing yet"),
    }
    if let Some((epoch, _)) = &st.secret {
        say!(out, "epoch    {epoch}");
    }
    say!(
        out,
        "device   {} ({}, {} devices)",
        fingerprint(&me),
        if role == "waiting" {
            "waiting for approval"
        } else {
            role
        },
        members.valid.len()
    );
    match (settings.on, &settings.dormant) {
        (false, _) => say!(out, "settings off"),
        (true, Some(why)) => say!(out, "settings standing down: {why}"),
        (true, None) => {
            say!(out, "settings");
            out.push_str(&groups_text(&settings.groups, "  "));
        }
    }
    say!(out, "daemon   {}", if daemon { "running" } else { "not running" });

    let summary = format!(
        "{} ({role}), {}, {}",
        config.device,
        counted(members.valid.len(), "device"),
        st.base.as_ref().map_or_else(
            || "nothing synced yet".to_string(),
            |b| format!("synced head {}", b.seq)
        )
    );
    let data = json!({
        "set_up": true,
        "device": config.device,
        "account": config.root,
        "account_fingerprint": fingerprint(&config.root),
        "folder": config.folder,
        "folders": config.folders.as_ref().map(|f| {
            // the usual folders this computer doesn't sync, with where they'd be
            let local = xdg_names(&config.folder);
            let here = |name: &str| folder_here(&config.folder, &local, name);
            let others: Vec<_> = other_folders(&config.folder, &synced, &f.skip)
                .into_iter()
                .map(here)
                .collect();
            let skipped: Vec<_> = f.skip.iter().map(|s| here(s)).collect();
            json!({
                "synced": synced.iter()
                    .map(|(name, path)| json!({ "name": name, "path": path }))
                    .collect::<Vec<_>>(),
                "skipped": skipped,
                "others": others,
                "more": more_folders(&config.folder, &synced, &f.skip)
                    .iter()
                    .map(|n| here(n))
                    .collect::<Vec<_>>(),
            })
        }),
        "repo": {
            "kind": kind,
            "repository": repo.repository,
            "bucket": bucket_name,
            "endpoint": repo.options.get("endpoint"),
        },
        "synced_head": st.base.as_ref().map(|b| b.seq),
        "synced_at": (st.synced_at > 0).then_some(st.synced_at),
        "epoch": st.secret.as_ref().map(|s| s.0),
        "this_device": { "fingerprint": fingerprint(&me), "role": role },
        "devices": device_list(&members.valid, &me),
        "removed": device_list(&members.revoked, &me),
        "requests": requests,
        "settings": {
            "on": settings.on,
            "dormant": settings.dormant,
            "held": settings.held,
            "groups": settings.groups,
        },
        "secrets": e.secrets_devices(),
        "daemon": daemon,
    });
    Ok(Response::new(data, summary).human(out.trim_end()))
}

fn folders(paths: &Paths, action: FoldersCmd) -> Out {
    let mut config = load(paths)?;
    // folders the account or this computer has by a name: kept as given,
    // though it's also another's name here (an account folder `Bilder`
    // where Pictures is `~/Bilder`)
    let known: std::collections::BTreeSet<String> = match &action {
        FoldersCmd::Skip { .. } if config.folders.is_some() => {
            let mut e = engine(paths, &config)?;
            e.load_folders()?;
            e.folder_list().into_iter().map(|(name, _)| name).collect()
        }
        _ => Default::default(),
    };
    let Some(choice) = config.folders.as_mut() else {
        return Err(fail::unsupported(format!(
            "this device syncs all of {}; folders are for setups that sync folders of home",
            config.folder.display()
        ))
        .into());
    };
    let (mut back, mut removing) = (None, None);
    // what's weighed is for choosing: not after every change
    let weigh = matches!(action, FoldersCmd::List);
    let changed = match action {
        FoldersCmd::List => false,
        FoldersCmd::Add { name } => {
            let name = folder_name(&config.folder, &name)?;
            back = Some(name.clone());
            let here = xdg_names(&config.folder)
                .get(&name)
                .cloned()
                .unwrap_or_else(|| name.clone());
            fs::create_dir_all(config.folder.join(here))?;
            _ = choice.skip.remove(&name);
            choice.add.insert(name)
        }
        FoldersCmd::Skip { name } => {
            let given = name.trim();
            let name = if known.contains(given) || choice.add.contains(given) || choice.skip.contains(given) {
                given.to_string()
            } else {
                folder_name(&config.folder, given)?
            };
            _ = choice.add.remove(&name);
            choice.skip.insert(name)
        }
        FoldersCmd::Unskip { ref name } => {
            let name2 = folder_name(&config.folder, name)?;
            let changed = choice.skip.remove(name.trim()) || choice.skip.remove(&name2);
            back = Some(name2);
            changed
        }
        FoldersCmd::Exclude { ref path } => choice.exclude.insert(subfolder(&config.folder, path)?),
        FoldersCmd::Include { ref path } => choice.exclude.remove(&subfolder(&config.folder, path)?),
        FoldersCmd::Remove { ref name, yes } => {
            let name = folder_name(&config.folder, name)?;
            confirm(
                yes,
                "taking a folder out of the cloud",
                &format!(
                    "take {name} out of the cloud? No computer deletes its files; each stops syncing it, and its earlier versions go within 30 days."
                ),
            )?;
            removing = Some(name.clone());
            choice.add.remove(&name)
        }
    };
    if changed {
        save(paths, &config)?;
    }
    // the account's folders are the ones in its history: sync to see them
    let mut e = engine(paths, &config)?;
    if let Some(name) = &back {
        e.take_back(name);
    }
    if let Some(name) = &removing {
        e.remove_from_cloud(name)?;
    } else {
        e.sync()?;
    }
    let synced = e.folder_list();
    let mut out = String::new();
    for (_, dir) in &synced {
        say!(out, "{}", dir.display());
    }
    let skipped = &config.folders.as_ref().expect("checked above").skip;
    if !skipped.is_empty() {
        let list: Vec<&str> = skipped.iter().map(String::as_str).collect();
        say!(out, "skipped here: {}", list.join(", "));
    }

    let excluded = &config.folders.as_ref().expect("checked above").exclude;
    if !excluded.is_empty() {
        let list: Vec<&str> = excluded.iter().map(String::as_str).collect();
        say!(out, "left out here: {}", list.join(", "));
    }
    if !e.dropped_folders().is_empty() {
        let list: Vec<&str> = e.dropped_folders().iter().map(String::as_str).collect();
        say!(out, "out of the cloud, kept here: {}", list.join(", "));
    }

    // with sizes, for whoever is choosing
    let home = &config.folder;
    let size = |path: &Path| weigh.then(|| tree_size(path));
    let sized = |name: &str| {
        let path = e.on_disk(Path::new(name));
        json!({ "name": name, "path": path, "size": size(&path) })
    };
    let others = other_folders(home, &synced, skipped);
    let data = json!({
        "synced": synced.iter().map(|(name, path)| json!({ "name": name, "path": path, "size": size(path) })).collect::<Vec<_>>(),
        "skipped": skipped.iter().map(|s| sized(s)).collect::<Vec<_>>(),
        "others": others.iter().map(|n| sized(n)).collect::<Vec<_>>(),
        // not weighed: these can be most of home
        "more": more_folders(home, &synced, skipped)
            .iter()
            .map(|n| json!({ "name": n, "path": e.on_disk(Path::new(n)) }))
            .collect::<Vec<_>>(),
        "excluded": excluded.iter().map(|p| json!({ "path": p, "size": size(&e.on_disk(Path::new(p))) })).collect::<Vec<_>>(),
        "removed_from_cloud": e.dropped_folders(),
        "changed": changed,
    });
    let ids = synced.iter().map(|(name, _)| name.clone()).collect();
    Ok(
        Response::new(data, format!("{} sync here", counted(synced.len(), "folder")))
            .human(out.trim_end())
            .ids(ids)
            .count(synced.len()),
    )
}

/// The usual folders this computer doesn't sync and could.
fn other_folders(
    home: &Path,
    synced: &[(String, PathBuf)],
    skip: &std::collections::BTreeSet<String>,
) -> Vec<&'static str> {
    let at_home = xdg_at_home(home);
    USUAL_FOLDERS
        .iter()
        .filter(|u| !synced.iter().any(|(n, _)| n == *u) && !skip.contains(**u))
        .filter(|u| !at_home.iter().any(|h| h == *u))
        .copied()
        .collect()
}

/// The folders directly in home, hidden ones aside, by their names here.
fn home_folders(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Every other folder of home this computer could sync, past the usual
/// ones: `~/Projects`, by the account's names.
fn more_folders(home: &Path, synced: &[(String, PathBuf)], skip: &std::collections::BTreeSet<String>) -> Vec<String> {
    home_folders(home)
        .into_iter()
        .filter_map(|n| folder_name(home, &n).ok())
        .filter(|n| !USUAL_FOLDERS.contains(&n.as_str()))
        .filter(|n| !synced.iter().any(|(s, _)| s == n) && !skip.contains(n))
        .collect()
}

/// A subfolder of a folder of home, as the account names it: `Documents/Notes`.
fn subfolder(home: &Path, given: &str) -> Result<String> {
    let given = given.trim().trim_end_matches('/');
    let rel = Path::new(given).strip_prefix(home).unwrap_or(Path::new(given));
    let parts: Vec<_> = rel.components().collect();
    let plain = parts.iter().all(|c| matches!(c, std::path::Component::Normal(_)));
    let (Some(first), true, true) = (parts.first(), parts.len() >= 2, plain) else {
        return Err(fail::usage(format!("{given:?} isn't a subfolder of a folder of home"))
            .hint("give one like Documents/Notes; a whole folder is `folders skip`")
            .into());
    };
    let name = folder_name(home, &first.as_os_str().to_string_lossy())?;
    let rest = parts[1..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    Ok(format!("{name}/{}", rest.join("/")))
}

/// The bytes of the files under `path`, not following links.
fn tree_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path).map_or(0, |entries| entries.flatten().map(|e| tree_size(&e.path())).sum())
}

/// Revoke the Dropbox token a key change here replaced, once every computer has
/// switched. Only the computer that made the change holds it. True if it did.
fn retire_old_token(paths: &Paths, e: &mut Engine) -> Result<bool> {
    let file = paths.data.join(dropbox::OLD_TOKEN_FILE);
    let old = match fs::read_to_string(&file) {
        Ok(token) => token,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    // saved by a change that never got published: it's still the one in use
    if load(paths)?.repo.options.get("refresh_token") == Some(&old) {
        return Ok(false);
    }
    let Some((seq, acked)) = e.bucket_key_status()? else {
        return Ok(false);
    };
    if !e.refresh_devices()?.members().valid.keys().all(|k| acked.contains(k)) {
        return Ok(false);
    }
    dropbox::revoke(&old)?;
    fs::remove_file(file)?;
    info!("revoked the old Dropbox token after key change {seq}");
    Ok(true)
}

/// Show or change the bucket's key.
fn bucket(paths: &Paths, action: BucketCmd) -> Out {
    let config = load(paths)?;
    if config.coordinator != BUCKET {
        return Err(fail::unsupported("only accounts that keep in step through their bucket have a bucket key").into());
    }
    match action {
        BucketCmd::Status => {
            let mut e = engine(paths, &config)?;
            let members = e.refresh_devices()?.members();
            let Some((seq, acked)) = e.bucket_key_status()? else {
                return Ok(Response::new(
                    json!({ "seq": null, "computers": [], "waiting": 0 }),
                    "the bucket key was never changed through omacloud",
                ));
            };
            let (mut computers, mut lines, mut waiting) = (Vec::new(), String::new(), 0);
            for (key, name) in &members.valid {
                let switched = acked.contains(key);
                let fp = fingerprint(key);
                say!(lines, "{name}  {fp}  {}", if switched { "switched" } else { "waiting" });
                computers.push(json!({ "device": name, "fingerprint": fp, "switched": switched }));
                waiting += usize::from(!switched);
            }
            let revoked = waiting == 0 && retire_old_token(paths, &mut e)?;
            let summary = if revoked {
                format!("every computer switched to key change {seq}; the old Dropbox token is revoked")
            } else if waiting == 0 && config.repo.is_dropbox() {
                format!("every computer switched to key change {seq}; the computer that made it revokes the old token")
            } else if waiting == 0 {
                format!("every computer switched to key change {seq}; delete the old key at your provider")
            } else {
                format!("key change {seq}: {} still to switch", counted(waiting, "computer"))
            };
            let text = format!("{summary}\n{}", lines.trim_end());
            Ok(Response::new(
                json!({ "seq": seq, "computers": computers, "waiting": waiting }),
                summary,
            )
            .human(text))
        }
        BucketCmd::SetKey {
            access_key_id,
            here_only,
        } => {
            let old_token = paths.data.join(dropbox::OLD_TOKEN_FILE);
            let key = if config.repo.is_dropbox() {
                // not after sending anyone to the browser; the same token again
                // was saved by a change that never got published
                if !here_only
                    && fs::read_to_string(&old_token)
                        .is_ok_and(|saved| Some(&saved) != config.repo.options.get("refresh_token"))
                {
                    return Err(fail::usage("an earlier Dropbox token is still waiting to be revoked")
                        .hint("wait until every computer switched (`omacloud bucket status`), then try again")
                        .into());
                }
                let refresh_token = dropbox::authorize()?;
                if !here_only && config.repo.options.get("refresh_token") == Some(&refresh_token) {
                    return Err(CliError::new(
                        "same_key",
                        output::EXIT_USAGE,
                        "Dropbox returned the token in use, which would cut nobody off",
                    )
                    .hint("allow Omacloud again for a new one")
                    .into());
                }
                Credentials::Dropbox { refresh_token }
            } else {
                let access_key_id = access_key_id.ok_or_else(|| {
                    fail::usage("give the new key's id: --access-key-id").hint("make it at your provider")
                })?;
                // the same key again would cut nobody off
                if !here_only && config.repo.options.get("access_key_id") == Some(&access_key_id) {
                    return Err(CliError::new(
                        "same_key",
                        output::EXIT_USAGE,
                        format!("{access_key_id} is the key in use"),
                    )
                    .hint("make a new key at your provider first")
                    .into());
                }
                Credentials::S3 {
                    access_key_id,
                    secret_access_key: read_key_secret()?,
                }
            };
            // does the new key reach the bucket, and may it write there?
            let mut trial = config.clone();
            trial.repo.set_key(&key);
            let coord = coordinator(&trial)?;
            check_bucket(coord.as_ref())?;
            if anchor_of(coord.as_ref())?.is_none() {
                return Err(fail::setup("no_account", "the new key reaches storage with no account in it").into());
            }
            if here_only {
                save(paths, &trial)?;
                return Ok(Response::new(
                    json!({ "seq": null, "here_only": true }),
                    "this computer uses the new key; it picks up the account's key change on the next sync",
                ));
            }
            // published with the old key, which still works
            let mut e = engine(paths, &config)?;
            if config.repo.is_dropbox() {
                let old = config.repo.options.get("refresh_token").context("no Dropbox token")?;
                write_private(&old_token, old.as_bytes())?;
            }
            let seq = e.change_bucket_key(&key)?;
            trial.bucket_key_seq = seq;
            save(paths, &trial)?;
            let after = if trial.repo.is_dropbox() {
                "Once all have, the old token is revoked by itself, which cuts off a removed computer."
            } else {
                "Keep the old key until all have, then delete it at your provider."
            };
            Ok(Response::new(
                json!({ "seq": seq, "here_only": false }),
                format!("key change {seq} published"),
            )
            .human(format!(
                "key change {seq} published, sealed to this account's computers. Each switches on its \
                 next sync; `omacloud bucket status` shows which have. {after}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_resolves_dotdot_in_parts_that_do_not_exist_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(absolute(&root.join("new/../B")).unwrap(), root.join("B"));
        assert_eq!(absolute(&root.join("a/./b/../c")).unwrap(), root.join("a/c"));
        assert!(!root.join("new").exists() && !root.join("B").exists() && !root.join("a").exists());
        // What exists is resolved for real, symlinks included.
        fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert_eq!(absolute(&root.join("link/x")).unwrap(), root.join("real/x"));
    }

    #[test]
    fn an_upgrade_is_noticed() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("omacloud");
        fs::write(&path, "old")?;
        let m = fs::metadata(&path)?;
        let exe = Exe {
            path: path.clone(),
            id: (m.dev(), m.ino()),
        };
        assert!(!exe.replaced());
        // as pacman installs: the new file renamed over the old
        let new = tmp.path().join("omacloud.new");
        fs::write(&new, "new")?;
        let old = fs::File::open(&path)?; // keep the old inode alive
        fs::rename(&new, &path)?;
        assert!(exe.replaced());
        drop(old);
        fs::remove_file(&path)?;
        assert!(!exe.replaced());
        Ok(())
    }

    #[test]
    fn a_folder_made_again_is_watched_again() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("Documents");
        let (tx, rx) = mpsc::channel();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(e) = res {
                _ = tx.send(e.paths);
            }
        })?;
        let mut watches = Watches::new(watcher);
        let wanted = [(dir.clone(), RecursiveMode::Recursive)];
        assert!(!watches.follow(&wanted)); // not there yet
        fs::create_dir(&dir)?;
        assert!(watches.follow(&wanted));
        assert!(!watches.follow(&wanted));
        fs::remove_dir(&dir)?;
        fs::create_dir(&dir)?;
        assert!(watches.follow(&wanted));
        while rx.try_recv().is_ok() {}
        fs::write(dir.join("new.txt"), "hi")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while !seen && Instant::now() < deadline {
            if let Ok(paths) = rx.recv_timeout(Duration::from_millis(100)) {
                seen = paths.contains(&dir.join("new.txt"));
            }
        }
        assert!(seen, "no event from the folder made again");
        Ok(())
    }
}
