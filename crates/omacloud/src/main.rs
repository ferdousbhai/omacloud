//! `omacloud`: set up a synced folder, sync it once, or keep it synced.
//!
//! Output follows the shared product contract (`output.rs`): text on a
//! terminal, a JSON envelope when piped; errors and exit codes in `fail.rs`.
//! Commands that print a secret print it only to a terminal. `watch`, which
//! systemd runs, keeps its own exit codes (see [`UPGRADED`]).

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, mpsc},
    time::{Duration, Instant},
};

mod agent_skill;
mod alerts;
mod dropbox;
mod fail;
mod hosted;
mod keyring;
mod output;
mod power;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use log::{info, warn};
use notify::{RecursiveMode, Watcher};
use omacloud_core::{
    Coordinator, DeviceError, DirCoordinator, Engine, HeadError, Membership, RepoSpec, SettingsSetup, Setup, account,
    bucket::key_refused,
    devices::{fingerprint, public_hex},
    epoch::EpochError,
    head::load_or_create_key,
    settings::Manifest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fail::{CliError, KEY_REFUSED_HINT};
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
--ids-only one id per line (devices, versions, folders, packages, secrets),
--count the number of results; ids-only > count > quiet > json > styled. A
failure is {\"ok\":false,\"error\":{\"code\",\"message\",\"hint\"?}} on stdout, or
error:/hint: lines on stderr in text mode.

Commands that print a secret (init when it creates an account or recovers with
a card, join-code, export, recovery contact, split and combine) print only to a
terminal, in text mode, with stdin a terminal too (combine excepted: its shares
may come on stdin).

Exit codes: 0 ok; 1 other failure; 2 invalid usage or request (a destructive
command without --yes where nobody can be asked, a secret asked for where no
terminal shows it, ambiguous, already set up); 3 not set up, not approved, or a
key, code or card refused; 4 not found; 5 storage, network, sign-in service or
keyring out of reach. `watch` (the daemon) exits 2 when this computer was
removed and 75 after an upgrade.";

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
    /// Set up this device: create the repository and account, or join them
    Init {
        /// Sync this one folder, whole [default: Desktop, Documents and
        /// Pictures in home, as iCloud does; `omacloud folders` changes them]
        #[arg(long)]
        folder: Option<PathBuf>,
        /// Where the files live: an opendal URL like `opendal:s3` or
        /// `opendal:dropbox` (with an app key as `--opt client_id=...`; the
        /// app secret and authorization code are asked for), or a path.
        /// For S3 use
        /// `--opt endpoint=... --opt bucket=... --opt access_key_id=...`; the
        /// secret key is asked for. Joining with a join code needs none
        #[arg(long)]
        repo: Option<String>,
        /// Backend option, repeatable: `--opt bucket=name`
        #[arg(long = "opt", value_parser = parse_kv)]
        opts: Vec<(String, String)>,
        /// Where computers keep in step [default: `bucket`, beside the files]
        #[arg(long, hide = true)]
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
        /// Join with the account's recovery code instead of asking another
        /// device to approve
        #[arg(long, env = "OMACLOUD_RECOVERY_CODE", hide_env_values = true)]
        recovery_code: Option<String>,
        /// With `--coordinator bucket`: keep coordination in this bucket
        /// (same endpoint and keys) instead of beside the data. Needed when
        /// the data bucket has object lock, as Hetzner doesn't do the
        /// create-only writes coordination needs on versioned buckets
        #[arg(long)]
        coordination_bucket: Option<String>,
        /// Join your account with the code from `omacloud join-code` (or Show
        /// Join Code in the app) on one of your computers
        #[arg(long, env = "OMACLOUD_JOIN_CODE", hide_env_values = true)]
        join_code: Option<String>,
        /// Keep your files on Omacloud storage instead of a bucket of your
        /// own: sign in with Google in the browser
        #[arg(long, conflicts_with_all = ["repo", "opts", "join_code", "coordination_bucket"])]
        hosted: bool,
        /// With `--hosted`, when you have neither the recovery code nor a
        /// computer: recover with your trusted contact's card (asked for, or
        /// from OMACLOUD_CONTACT_CARD) and your Google sign-in
        #[arg(long, requires = "hosted", conflicts_with = "recovery_code")]
        contact_card: bool,
    },
    /// Print a code another computer joins this account with (`omacloud
    /// init --join-code`). It holds the bucket's key: pass it privately
    JoinCode,
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
    /// Your bucket's key: change it, to cut off a lost or removed computer
    Bucket {
        #[command(subcommand)]
        action: Option<BucketCmd>,
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
    /// Recovery with people you trust: a trusted contact's card, or the
    /// recovery code split among several people
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
        /// Copy the repository here (a path or opendal URL), then describe
        /// the copy
        #[arg(long)]
        to: Option<String>,
        /// Backend option for `--to`, repeatable
        #[arg(long = "to-opt", value_parser = parse_kv)]
        to_opts: Vec<(String, String)>,
    },
    /// Print the agent skill (SKILL.md) built into omacloud, or install it
    /// for your coding agents
    Skill {
        #[command(subcommand)]
        action: Option<SkillCmd>,
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
    /// Make a card for someone you trust (with Omacloud storage). The card
    /// and your Google sign-in together give back the recovery code (`init
    /// --hosted --contact-card`); either alone opens nothing. Asks for the
    /// recovery code; a new card replaces the last one
    Contact {
        /// Remove the trusted contact instead: their card stops working
        #[arg(long)]
        remove: bool,
    },
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
enum BucketCmd {
    /// Show the latest key change and which computers switched to it
    Status,
    /// Switch every computer to a new key. With your own bucket, make it at
    /// your provider first, and keep the old one until `bucket status` says
    /// all switched, then delete it there; the secret comes from
    /// OMACLOUD_SECRET_ACCESS_KEY, or is asked for. With Omacloud storage,
    /// Omacloud makes the key, and retires the old ones once all switched.
    /// With Dropbox, authorize the app again for a fresh refresh token.
    SetKey {
        /// The new key's id (your own bucket)
        #[arg(long)]
        access_key_id: Option<String>,
        /// Only this computer: for one that missed the change because the old
        /// key was already deleted (with Omacloud storage, signs in again)
        #[arg(long)]
        here_only: bool,
    },
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
        /// Don't ask; needed where nobody can be asked
        #[arg(long)]
        yes: bool,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected key=value, got `{s}`"))
}

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    device: String,
    folder: PathBuf,
    /// `bucket` (coordination in the bucket, see `coordination`), or a
    /// shared directory
    coordinator: String,
    repo: RepoSpec,
    /// The account root public key (hex), pinned at setup
    root: String,
    #[serde(default)]
    limits: Limits,
    /// Sync settings (the dots manifest's shared tier) on this device
    #[serde(default)]
    settings: bool,
    /// Desktop notifications from the daemon: join requests, settings that
    /// need a choice, this device being removed
    #[serde(default = "yes")]
    notifications: bool,
    /// Sync chosen folders of home (then `folder` is home); without it, all
    /// of `folder`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folders: Option<FolderChoice>,
    /// With `coordinator = "bucket"`: where in the user's own storage the
    /// coordination lives (OpenDAL options, `scheme` among them, with the
    /// bucket's keys: this device needs them before it has any account key)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coordination: Option<BTreeMap<String, String>>,
    /// The last bucket key change this device switched to
    #[serde(default, skip_serializing_if = "is_zero")]
    bucket_key_seq: u64,
    /// Omacloud storage: where this account signed in (see `hosted`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service: Option<String>,
    /// Omacloud storage or Dropbox: the last key change whose old keys were retired
    #[serde(default, skip_serializing_if = "is_zero")]
    bucket_key_retired: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct FolderChoice {
    /// Folders this device adds to the account
    #[serde(default)]
    add: std::collections::BTreeSet<String>,
    /// Folders this device doesn't sync
    #[serde(default)]
    skip: std::collections::BTreeSet<String>,
}

/// What syncs by default, as iCloud's Desktop & Documents and Photos.
const DEFAULT_FOLDERS: &[&str] = &["Desktop", "Documents", "Pictures"];

/// The folders of home a computer is asked about.
const USUAL_FOLDERS: &[&str] = &["Desktop", "Documents", "Pictures", "Music", "Videos", "Downloads"];

/// This machine's names for the XDG user folders, where they differ from
/// the English names the account uses (`Documents` -> `Dokumente`). Only
/// folders directly in home count.
fn xdg_names(home: &Path) -> BTreeMap<String, String> {
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
    .iter()
    .filter_map(|(key, name)| {
        let local = dirs.lines().find_map(|l| {
            l.trim()
                .strip_prefix(&format!("XDG_{key}_DIR="))
                .map(|v| v.trim_matches('"').replace("$HOME/", ""))
        })?;
        let direct = !local.is_empty() && !local.contains('/') && !local.starts_with('$');
        (direct && local != *name).then(|| ((*name).to_string(), local))
    })
    .collect()
}

/// The usual folders this computer points at home itself (Omarchy's
/// `XDG_DESKTOP_DIR="$HOME/"`): there is no such folder to sync.
fn xdg_at_home(home: &Path) -> Vec<String> {
    let dirs = fs::read_to_string(
        std::env::var_os("XDG_CONFIG_HOME")
            .map_or_else(|| home.join(".config"), PathBuf::from)
            .join("user-dirs.dirs"),
    )
    .unwrap_or_default();
    [("DESKTOP", "Desktop"), ("DOWNLOAD", "Downloads")]
        .iter()
        .filter(|(key, _)| {
            dirs.lines().any(|l| {
                l.trim()
                    .strip_prefix(&format!("XDG_{key}_DIR="))
                    .is_some_and(|v| matches!(v.trim_matches('"'), "$HOME" | "$HOME/"))
            })
        })
        .map(|(_, name)| (*name).to_string())
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
    /// backend, `connections` on opendal ones. Set on any repository: an own bucket's location may arrive later,
    /// sealed with the key, and the engine carries these over to it.
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
}

impl Paths {
    fn new(config: Option<PathBuf>) -> Result<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME not set")?;
        let xdg = |var: &str, fallback: &str| std::env::var_os(var).map_or_else(|| home.join(fallback), PathBuf::from);
        let config = config.unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("omacloud/config.toml"));
        // a custom config file gets its own data directory beside it
        let data = if config.file_name().is_some_and(|n| n == "config.toml") {
            xdg("XDG_DATA_HOME", ".local/share").join("omacloud")
        } else {
            config.with_extension("d")
        };
        Ok(Self { config, data })
    }
    /// Whether this is the default configuration, the one the packaged
    /// systemd unit runs.
    fn is_default(&self) -> bool {
        self.config.file_name().is_some_and(|n| n == "config.toml")
    }
    fn device_key(&self) -> PathBuf {
        self.data.join("device.key")
    }
    fn state(&self) -> PathBuf {
        self.data.join("state.json")
    }
    /// The user's ignore rules, beside the config file.
    fn ignore(&self) -> PathBuf {
        if self.config.file_name().is_some_and(|n| n == "config.toml") {
            self.config.with_file_name("ignore")
        } else {
            self.config.with_extension("ignore")
        }
    }
}

fn load(paths: &Paths) -> Result<Config> {
    let mut config = load_without_key(paths)?;
    if config.coordinator == SELF_HOSTED
        && let Some(c) = config.coordination.as_mut()
        && !has_key(c)
    {
        c.extend(keyring::lookup(&paths.config)?);
    }
    Ok(config)
}

/// The config without its bucket key, for `bucket set-key --here-only`: the
/// way back when the keyring lost it.
fn load_without_key(paths: &Paths) -> Result<Config> {
    let text = match fs::read_to_string(&paths.config) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(fail::not_set_up().into()),
        other => other.with_context(|| format!("reading {}", paths.config.display()))?,
    };
    Ok(toml::from_str(&text)?)
}

fn has_key(options: &BTreeMap<String, String>) -> bool {
    options
        .keys()
        .any(|k| omacloud_core::repo::CREDENTIAL_OPTIONS.contains(&k.as_str()))
}

fn save(paths: &Paths, config: &Config) -> Result<()> {
    // a self-hosted computer's bucket key goes to the keyring when there is
    // one, and the file keeps the rest
    let mut config = config.clone();
    if config.coordinator == SELF_HOSTED
        && let Some(c) = config.coordination.as_mut()
        && has_key(c)
        && keyring::store(&paths.config, &omacloud_core::bucket_key::credentials_of(c))?
    {
        c.retain(|k, _| !omacloud_core::repo::CREDENTIAL_OPTIONS.contains(&k.as_str()));
    }
    let config = &config;
    if let Some(dir) = paths.config.parent() {
        fs::create_dir_all(dir)?;
    }
    // private: without a keyring, it holds the bucket's keys
    let tmp = paths.config.with_extension("toml.tmp");
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?
        .write_all(toml::to_string_pretty(config)?.as_bytes())?;
    fs::rename(&tmp, &paths.config)?;
    Ok(())
}

/// `coordinator = "bucket"`: coordination lives in the account's bucket,
/// beside the files (a bucket of the user's own, or Omacloud storage).
const SELF_HOSTED: &str = "bucket";

/// Where `config`'s computers keep in step: its bucket, or a shared
/// directory.
fn coordinator(config: &Config) -> Result<Arc<dyn Coordinator>> {
    Ok(if config.coordinator == SELF_HOSTED {
        let mut opts = config
            .coordination
            .clone()
            .context("no coordination location in the config")?;
        let scheme = opts.remove("scheme").unwrap_or_else(|| "s3".into());
        Arc::new(omacloud_core::BucketCoordinator::new(&scheme, &opts)?)
    } else {
        Arc::new(DirCoordinator::new(&config.coordinator)?)
    })
}

fn engine(paths: &Paths, config: &Config) -> Result<Engine> {
    let signing = load_or_create_key(&paths.device_key())?;
    let coord = coordinator(config)?;
    let mut repo = config.repo.clone();
    config.limits.apply(&mut repo);
    // self-hosted: the bucket key lives in this config, and key changes
    // update it here; the data uses it too
    if config.coordinator == SELF_HOSTED
        && let Some(c) = &config.coordination
    {
        repo.options.extend(omacloud_core::bucket_key::credentials_of(c));
    }
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
            groups.push(Arc::new(omacloud_core::agents::Agents::new(&home)));
            let user = std::env::var("USER").unwrap_or_default();
            groups.push(Arc::new(omacloud_core::wifi::Wifi::new(Box::new(
                omacloud_core::wifi::Nmcli::new(&user),
            ))));
            Some(SettingsSetup {
                home,
                manifest: Manifest::load()?,
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

/// How a recovering computer got the recovery code.
#[derive(PartialEq, Eq)]
enum Recovery {
    Code,
    /// From a trusted contact's card: shown again, as nobody has it now
    Card,
}

fn already_set_up(paths: &Paths) -> CliError {
    CliError::new(
        "already_set_up",
        output::EXIT_USAGE,
        format!("{} exists; this device is already set up", paths.config.display()),
    )
    .hint("give another --config to set up a second account here")
}

/// What `init` did before setting up the account: signed in, made a bucket.
#[derive(Default)]
struct InitNotes {
    signed_in_as: Option<String>,
    created_bucket: Option<String>,
}

fn init(paths: &Paths, mut config: Config, recovery: Option<(String, Recovery)>, notes: InitNotes) -> Out {
    if paths.config.exists() {
        return Err(already_set_up(paths).into());
    }
    // the refusals that need no account come before anything is made here:
    // no folder, device key or coordination skeleton is left behind. A
    // shared directory is only looked at, as opening it makes its skeleton
    let bucket = if config.coordinator == SELF_HOSTED {
        let c = coordinator(&config)?;
        let anchor = c.anchor()?;
        Some((c, anchor))
    } else {
        None
    };
    let creating = bucket.as_ref().map_or_else(
        || !Path::new(&config.coordinator).join("account.json").exists(),
        |(_, anchor)| anchor.is_none(),
    );
    if matches!(recovery, Some((_, Recovery::Card))) && !output::to_terminal(mode()) {
        return Err(fail::terminal_required("your recovery code").into());
    }
    if creating && !output::to_terminal(mode()) {
        if config.repo.repository.is_empty() {
            return Err(fail::usage("creating an account needs --repo")
                .hint("run `omacloud init --help`")
                .into());
        }
        let refused = fail::terminal_required("the new account's recovery code");
        // Omacloud storage: only the sign-in told there's no account yet
        return Err(if notes.signed_in_as.is_some() {
            refused.hint("there's no account for this Google account yet; create it in a terminal of your own")
        } else {
            refused
        }
        .into());
    }
    fs::create_dir_all(&config.folder)?;
    fs::create_dir_all(&paths.data)?;
    let signing = load_or_create_key(&paths.device_key())?;
    let me = fingerprint(&public_hex(&signing));
    let (coord, anchor) = match bucket {
        Some(opened) => opened,
        None => {
            let c = coordinator(&config)?;
            let anchor = c.anchor()?;
            (c, anchor)
        }
    };
    let creating = anchor.is_none();
    if creating && matches!(recovery, Some((_, Recovery::Card))) {
        return Err(fail::not_found("there's no account here to recover")
            .hint("sign in with the Google account you used")
            .into());
    }
    if let (Some(anchor), Some((code, how))) = (&anchor, &recovery) {
        let root = omacloud_core::devices::root_key(code).ok().map(|k| public_hex(&k));
        if root.as_deref() != Some(anchor.root.as_str()) {
            return Err(match how {
                Recovery::Card => fail::setup(
                    "card_rejected",
                    "that card doesn't go with this account: it may be an earlier card, replaced by \
                     a newer one, or another account's",
                )
                .hint("ask your trusted contact for their latest card"),
                Recovery::Code => fail::recovery_code_rejected("that recovery code doesn't belong to this account"),
            }
            .into());
        }
    }
    // creating an account shows its recovery code and password once, and
    // recovering with a card shows the code again: only to a terminal.
    // Checked again here, as the account may have appeared or gone meanwhile
    if creating && config.repo.repository.is_empty() {
        return Err(fail::usage("creating an account needs --repo")
            .hint("run `omacloud init --help`")
            .into());
    }
    if creating && !output::to_terminal(mode()) {
        return Err(fail::terminal_required("the new account's recovery code").into());
    }
    if matches!(recovery, Some((_, Recovery::Card))) && !output::to_terminal(mode()) {
        return Err(fail::terminal_required("your recovery code").into());
    }

    // the sign-in and a new bucket were told as they happened (on stderr)
    let mut out = String::new();
    let how = if creating {
        let created = account::create(coord.as_ref(), &config.repo, &signing, &config.device)?;
        config.root = created.root;
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
            config.repo.repository,
            config.root,
            created.recovery_code,
            created.password
        );
        "created"
    } else if let Some((code, how)) = recovery {
        config.root = account::join_with_recovery(coord.as_ref(), &code, &signing, &config.device)?;
        if how == Recovery::Card {
            say!(
                out,
                "joined the account with your trusted contact's card\n\n\
                 recovery code: {code}\n\n\
                 Write it down again and keep it offline. Your contact's card keeps working."
            );
            "recovered_with_card"
        } else {
            say!(out, "joined the account with the recovery code");
            "joined_with_recovery_code"
        }
    } else {
        config.root = account::request(coord.as_ref(), &signing, &config.device)?;
        say!(
            out,
            "asked to join {}\n\n\
             this device's fingerprint: {me}\n\n\
             On a device that already syncs, run `omacloud devices` and check that it\n\
             shows the same fingerprint, then `omacloud devices approve {me}`.",
            config.repo.repository
        );
        "asked_to_join"
    };
    // bucket credentials live sealed in the account's key records, not in
    // the config file
    config.repo = config.repo.without_credentials();
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
        "signed_in_as": notes.signed_in_as,
        "created_bucket": notes.created_bucket,
    });
    Ok(Response::new(data, summary).human(out.trim_end()))
}

/// Why the daemon stopped.
enum Ended {
    /// Removed from the account, or the account can't be verified.
    Stopped,
    /// The package replaced the binary: restart into the new one.
    Upgraded,
}

/// Exit status after an upgrade: a failure to the unit, so `Restart=` starts
/// the new binary (2 is the status it doesn't restart).
const UPGRADED: i32 = 75;

/// Keep the folders in sync, until sync must stop or an upgrade replaced
/// this binary.
fn watch(paths: &Paths, config: &Config, poll: Duration) -> Result<Ended> {
    power::be_nice();
    let mut engine = engine(paths, config)?;
    let exe = Exe::running();
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
    let mut alerts = config
        .notifications
        .then(|| alerts::Alerts::new((!paths.is_default()).then_some(paths.config.as_path())));
    let mut key_at: Option<Instant> = None;
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
        // `omacloud folders` edits the config: follow it without a restart
        let now = config_mtime();
        if now != config_seen {
            config_seen = now;
            if let Ok(Config {
                folders: Some(choice), ..
            }) = load(paths)
            {
                info!("folder choice changed");
                engine.set_folders(choice.add, choice.skip);
                folders_changed = true;
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
        let marker = if paused {
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
        if !paused && !idle {
            folders_changed = false;
            // now and then, look at everything: a watcher can miss changes
            if rescan_at.is_none_or(|t| t.elapsed() >= FULL_SYNC) {
                engine.request_rescan();
                rescan_at = Some(Instant::now());
            }
            match run_sync(&mut engine, &mut waiting, alerts.as_ref()) {
                Synced::Stop => return Ok(Ended::Stopped),
                Synced::Ok => {
                    // the marker read before syncing: anything written
                    // since moves it again
                    seen_marker = marker;
                    full_at = Some(Instant::now());
                }
                Synced::Failed => seen_marker = None,
            }
            // a self-hosted account's new bucket key: switch, reconnect
            if key_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                key_at = Some(Instant::now());
                match follow_bucket_key(paths, &mut engine) {
                    Ok(true) => engine = crate::engine(paths, &load(paths)?)?,
                    Ok(false) => {}
                    Err(e) => warn!("checking for a new bucket key: {e:#}"),
                }
                if let Err(e) = retire_old_keys(paths, &mut engine) {
                    warn!("retiring old storage keys: {e:#}");
                }
            }
            if let Some(a) = alerts.as_mut() {
                a.check(&mut engine);
            }
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
    /// Sync must stop until a human looks.
    Stop,
}

/// One sync.
fn run_sync(engine: &mut Engine, waiting: &mut bool, alerts: Option<&alerts::Alerts>) -> Synced {
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
        // a rotation in progress, or a grant not written yet: wait
        Err(e)
            if matches!(
                e.downcast_ref::<EpochError>(),
                Some(EpochError::Pending { .. } | EpochError::NoKey { .. })
            ) =>
        {
            info!("waiting: {e}");
        }
        // removed from the account, or a coordinator we can't verify: stop
        // until a human looks
        Err(e)
            if e.downcast_ref::<Membership>().is_some()
                || e.downcast_ref::<HeadError>().is_some()
                || e.downcast_ref::<DeviceError>().is_some()
                || e.downcast_ref::<EpochError>().is_some() =>
        {
            log::error!("refusing to sync: {e}");
            if e.downcast_ref::<Membership>() == Some(&Membership::Revoked)
                && let Some(a) = alerts
            {
                a.removed();
            }
            return Synced::Stop;
        }
        Err(e) if key_refused(&e) => warn!("the storage refused this computer's key: {e:#}. {KEY_REFUSED_HINT}"),
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
    // daemon's journal gets both
    let filter = if mode.is_machine() && !daemon {
        "warn"
    } else {
        "info,rustic_core=warn"
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
                fail::classify(&e).report(mode);
                std::process::exit(1)
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
            repo,
            opts,
            coordinator,
            folders: chosen,
            settings,
            device,
            recovery_code,
            coordination_bucket,
            join_code,
            hosted,
            contact_card,
        } => {
            let mut notes = InitNotes::default();
            if paths.config.exists() {
                return Err(already_set_up(&paths).into());
            }
            // a card gives back the recovery code, which is shown again
            if contact_card && !output::to_terminal(mode()) {
                return Err(fail::terminal_required("your recovery code").into());
            }
            let mut recovery = recovery_code.map(|c| (c, Recovery::Code));
            // Omacloud storage: signing in gives the bucket and its key
            let service =
                hosted.then(|| std::env::var("OMACLOUD_SERVICE_URL").unwrap_or_else(|_| hosted::SERVICE.to_string()));
            let (repo, opts) = if let Some(service) = &service {
                let s = if contact_card {
                    // checked before the browser opens: a typo is caught here
                    let card = read_secret("OMACLOUD_CONTACT_CARD", "your contact's card")?;
                    omacloud_core::contact::checked(&card).map_err(|e| {
                        fail::setup("card_rejected", format!("{e:#}")).hint("check the card with your trusted contact")
                    })?;
                    let s = hosted::sign_in_with_pad(service)?;
                    let pad = s.contact_pad.as_deref().ok_or_else(|| {
                        fail::setup(
                            "card_rejected",
                            format!("{} has no trusted contact: no card opens it", s.email),
                        )
                        .hint("sign in with the Google account that made the card")
                    })?;
                    let code = omacloud_core::contact::open(&card, pad)
                        .map_err(|e| fail::setup("card_rejected", format!("{e:#}")))?;
                    recovery = Some((code, Recovery::Card));
                    s
                } else {
                    hosted::sign_in(service)?
                };
                // told now: a later failure would hide it
                eprintln!("signed in as {}", s.email);
                notes.signed_in_as = Some(s.email.clone());
                let opts = [
                    ("endpoint", s.endpoint),
                    ("region", s.region),
                    ("bucket", s.bucket),
                    ("access_key_id", s.access_key_id),
                    ("secret_access_key", s.secret_access_key),
                ];
                (
                    Some("opendal:s3".to_string()),
                    opts.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                )
            } else {
                (repo, opts)
            };
            // a join code says where the account coordinates
            let joined = join_code
                .as_deref()
                .map(|code| {
                    // a fixed message: the parser's would quote parts of the code
                    parse_join_code(code).map_err(|_| {
                        fail::setup("join_code_rejected", "that join code is damaged")
                            .hint("copy the whole code again from `omacloud join-code` (or Show Join Code in the app)")
                    })
                })
                .transpose()?;
            // in the bucket, unless a shared directory is given
            let coordinator = match (coordinator, &joined, repo.as_deref()) {
                (_, Some(_), _) => SELF_HOSTED.to_string(),
                (Some(c), _, _) => c,
                (None, None, Some(r)) if r.starts_with("opendal:") => SELF_HOSTED.to_string(),
                (None, None, _) => {
                    return Err(fail::usage("say where the files live")
                        .hint(
                            "sign in to Omacloud storage with `--hosted`, or give a bucket of your own with \
                         `--repo opendal:s3 --opt endpoint=... --opt bucket=... --opt access_key_id=...` \
                         (the secret key is asked for, or comes from OMACLOUD_SECRET_ACCESS_KEY), or join an \
                         account with `--join-code` (from `omacloud join-code` on one of its computers). The \
                         Omacloud app (omacloud-app) walks through the same.",
                        )
                        .into());
                }
            };
            let coordinator = if coordinator == SELF_HOSTED {
                coordinator
            } else {
                absolute(Path::new(&coordinator))?.to_string_lossy().into_owned()
            };
            let repo = match repo.as_deref() {
                // joining an account whose bucket location comes sealed with
                // its key
                None => RepoSpec {
                    repository: String::new(),
                    options: opts.into_iter().collect::<BTreeMap<_, _>>(),
                },
                Some(repo) => RepoSpec {
                    repository: repo.to_string(),
                    options: {
                        let mut o = opts.into_iter().collect::<BTreeMap<_, _>>();
                        // an S3 key's secret half: never on the command line
                        if o.contains_key("access_key_id") && !o.contains_key("secret_access_key") {
                            o.insert(
                                "secret_access_key".into(),
                                read_secret("OMACLOUD_SECRET_ACCESS_KEY", "secret access key")?,
                            );
                        }
                        if repo == "opendal:dropbox" {
                            let id = o.get("client_id").cloned().unwrap_or_default();
                            let url = dropbox::authorize_url(&id)?;
                            o.entry("root".into()).or_insert("/Omacloud".into());
                            let secret = match o.get("client_secret") {
                                Some(secret) => secret.clone(),
                                None => read_secret("OMACLOUD_DROPBOX_CLIENT_SECRET", "Dropbox app secret")?,
                            };
                            o.insert("client_secret".into(), secret.clone());
                            if !o.contains_key("refresh_token") {
                                if std::env::var_os("OMACLOUD_DROPBOX_AUTH_CODE").is_none() {
                                    eprintln!("Open this Dropbox authorization URL and paste its code:\n\n  {url}\n");
                                }
                                let code = read_secret("OMACLOUD_DROPBOX_AUTH_CODE", "Dropbox authorization code")?;
                                let refresh = dropbox::exchange(&id, &secret, &code)?;
                                o.insert("refresh_token".into(), refresh);
                            }
                        }
                        o
                    },
                },
            };
            // no bucket given: make a private one with a random name, which
            // creates an account and shows its recovery code
            let mut repo = repo;
            if repo.repository == "opendal:s3" && !repo.options.contains_key("bucket") {
                if !output::to_terminal(mode()) {
                    return Err(fail::terminal_required("the new account's recovery code").into());
                }
                let opt = |k: &str| repo.options.get(k).cloned().unwrap_or_default();
                let (endpoint, region) = (opt("endpoint"), opt("region"));
                if endpoint.is_empty() {
                    return Err(fail::usage("give the storage's endpoint: --opt endpoint=...").into());
                }
                let region = if region.is_empty() { "auto".to_string() } else { region };
                let mut made = None;
                for _ in 0..3 {
                    let name = omacloud_core::bucket::new_bucket_name();
                    if omacloud_core::bucket::create_bucket(
                        &endpoint,
                        &region,
                        &opt("access_key_id"),
                        &opt("secret_access_key"),
                        &name,
                    )? == omacloud_core::bucket::Created::Ours
                    {
                        made = Some(name);
                        break;
                    }
                }
                let name = made.context("couldn't find a free bucket name; try again")?;
                // told now: if setting up fails from here, the bucket still exists
                eprintln!("created bucket {name}");
                notes.created_bucket = Some(name.clone());
                repo.options.insert("bucket".into(), name);
                repo.options.entry("region".into()).or_insert(region);
            }
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
                            |c| {
                                c.into_iter()
                                    .filter(|n| !n.trim().is_empty())
                                    .collect::<std::collections::BTreeSet<_>>()
                            },
                        )
                        .into_iter()
                        .filter(|n| !at_home.contains(n))
                        .collect();
                    (home, Some(FolderChoice { add, skip }))
                }
            };
            let device = device.unwrap_or_else(hostname);
            let config = Config {
                device,
                folder,
                coordinator,
                repo,
                root: String::new(),
                limits: Limits::default(),
                settings,
                notifications: true,
                folders,
                coordination: None,
                bucket_key_seq: 0,
                service,
                bucket_key_retired: 0,
            };
            let mut config = config;
            if config.coordinator == SELF_HOSTED {
                config.coordination = Some(match joined {
                    Some((opts, _)) => opts,
                    None => coordination_beside(&config.repo, coordination_bucket.as_deref())?,
                });
            }
            init(&paths, config, recovery, notes)
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
                Err(err) if config.coordinator == SELF_HOSTED && key_refused(&err) => {
                    return Err(fail::setup(
                        "key_refused",
                        format!("the storage refused this computer's key: {err:#}"),
                    )
                    .hint(KEY_REFUSED_HINT)
                    .into());
                }
                other => other?,
            };
            let summary = format!("pushed {}, pulled {}, conflicts {}", st.pushed, st.pulled, st.conflicts);
            let mut out = summary.clone();
            let switched = follow_bucket_key(&paths, &mut e)?;
            if switched {
                say!(out);
                out.push_str("switched to the account's new bucket key");
            }
            let data = json!({
                "pushed": st.pushed,
                "pulled": st.pulled,
                "conflicts": st.conflicts,
                "switched_bucket_key": switched,
            });
            Ok(Response::new(data, summary).human(out))
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
            let rel = relative(&config, &path)?;
            let versions = engine(&paths, &config)?.versions(&rel)?;
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
                    v.time
                        .to_zoned(omacloud_core::jiff::tz::TimeZone::system())
                        .strftime("%Y-%m-%d %H:%M:%S"),
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
        Cmd::Restore { path, at, to, yes } => {
            let config = load(&paths)?;
            let rel = relative(&config, &path)?;
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
            engine(&paths, &config)?
                .restore_version(&rel, at, &dest)
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
                return Err(fail::terminal_required("a join code (it holds your bucket's keys)").into());
            }
            let config = load(&paths)?;
            let coordination = config
                .coordination
                .as_ref()
                .filter(|_| config.coordinator == SELF_HOSTED)
                .ok_or_else(|| {
                    fail::unsupported("this account keeps in step through a shared directory, not its bucket")
                        .hint("other computers join with `omacloud init --coordinator <that directory>`")
                })?;
            let blob = serde_json::to_vec(&json!({
                "coordination": coordination,
                "account": config.root,
            }))?;
            eprintln!(
                "It holds your bucket's keys: pass it privately. The new device runs\n  \
                 omacloud init --join-code <code>\nand then needs your approval (or your recovery code)."
            );
            Ok(Response::new(Value::Null, "join code").human(format!("omacloud-join-{}", hex::encode(blob))))
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
        Cmd::Export { to, to_opts } => {
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
                    options: to_opts.into_iter().collect(),
                };
                let n = spec.copy_files_to(&dest)?;
                eprintln!("copied {n} files to {}", dest.repository);
                spec = dest;
            }
            let repo = &spec.repository;
            let mut out = String::new();
            say!(
                out,
                "# restore without omacloud: restic or rustic, this repository and password"
            );
            say!(out, "# (repository epoch {epoch})");
            say!(out, "repository: {repo}");
            for (k, v) in &spec.options {
                say!(out, "option:     {k}={v}");
            }
            say!(out, "password:   {password}");
            say!(out);
            if repo.starts_with("opendal:") {
                // restic can't read opendal URLs; rustic can, from a profile
                say!(
                    out,
                    "# save as ~/.config/rustic/rustic.toml (the default profile), then: rustic restore latest <dir>"
                );
                say!(out, "[repository]\nrepository = \"{repo}\"\npassword = \"{password}\"");
                say!(out, "\n[repository.options]");
                for (k, v) in &spec.options {
                    say!(out, "{k} = \"{v}\"");
                }
            } else {
                say!(
                    out,
                    "RESTIC_PASSWORD='{password}' restic -r {repo} restore latest --target <dir>"
                );
            }
            Ok(Response::new(Value::Null, "export").human(out.trim_end()))
        }
    }
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
            let list = |m: &BTreeMap<String, String>| -> Vec<Value> {
                m.iter()
                    .map(|(k, name)| json!({ "fingerprint": fingerprint(k), "name": name, "this": *k == me }))
                    .collect()
            };
            let data = json!({
                "account_fingerprint": fingerprint(&chain.root),
                "devices": list(&members.valid),
                "removed": list(&members.revoked),
                "requests": requests
                    .iter()
                    .map(|r| json!({ "fingerprint": fingerprint(&r.device), "name": r.name }))
                    .collect::<Vec<_>>(),
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
            if rotate {
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
                    "It still holds the repository key it had. If the device is lost or not \
                     trusted, run `omacloud rotate`."
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

fn settings(paths: &Paths, action: SettingsCmd) -> Out {
    let mut config = load(paths)?;
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
                    "they sync at the next `omacloud sync`, or right away if the daemon runs (restart it: systemctl --user restart omacloud)"
                );
            }
            let summary = if config.settings {
                "settings sync is on"
            } else {
                "settings sync is off"
            };
            Ok(Response::new(settings_json(&st), summary).human(out.trim_end()))
        }
        SettingsCmd::Status => {
            let st = engine(paths, &config)?.settings_status();
            let summary = match (st.on, &st.dormant) {
                (false, _) => "settings sync is off".to_string(),
                (true, Some(_)) => "settings sync stands down".to_string(),
                (true, None) => format!("settings sync is on, {} waiting to be resolved", st.held.len()),
            };
            Ok(Response::new(settings_json(&st), summary).human(settings_text(&st).trim_end()))
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

fn settings_json(st: &omacloud_core::SettingsStatus) -> Value {
    json!({
        "on": st.on,
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
fn relative(config: &Config, path: &Path) -> Result<PathBuf> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    // the file may be deleted, so canonicalize only the folder part
    if let Ok(rel) = abs.strip_prefix(&config.folder) {
        return Ok(rel.to_path_buf());
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
            "the bucket's delete protection keeps {} or more files of the old repository; \
             they are deleted once their retention ends",
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

/// Where a computer on Omacloud storage signs in, and its account (the
/// bucket).
fn hosted_account(config: &Config) -> Option<(String, String)> {
    let service = hosted_service(config)?;
    Some((service, config.repo.options.get("bucket")?.clone()))
}

fn recovery(paths: &Paths, action: RecoveryCmd) -> Out {
    use omacloud_core::{contact, shamir};
    // all but removing the contact print the recovery code or a part of it;
    // combine reads its shares from stdin, which may be a pipe
    let to_terminal = if matches!(action, RecoveryCmd::Combine) {
        output::stdout_to_terminal(mode())
    } else {
        output::to_terminal(mode())
    };
    if !matches!(action, RecoveryCmd::Contact { remove: true }) && !to_terminal {
        let what = match action {
            RecoveryCmd::Contact { .. } => "a trusted contact's card",
            RecoveryCmd::Split { .. } => "shares of your recovery code",
            RecoveryCmd::Combine => "your recovery code",
        };
        return Err(fail::terminal_required(what).into());
    }
    match action {
        RecoveryCmd::Contact { remove } => {
            let config = load_without_key(paths)?;
            let (service, account) = hosted_account(&config).ok_or_else(|| {
                fail::unsupported("a trusted contact needs Omacloud storage").hint(
                    "with storage of your own, give someone you trust a copy of your recovery code, or split \
                     it among several (`omacloud recovery split`)",
                )
            })?;
            if remove {
                let email = hosted::set_contact(&service, &account, None)?;
                return Ok(Response::new(
                    json!({ "account": email, "contact": false }),
                    format!("{email} has no trusted contact now: their card opens nothing"),
                ));
            }
            let code = read_recovery_code()?;
            let root =
                omacloud_core::devices::root_key(&code).map_err(|e| fail::recovery_code_rejected(format!("{e:#}")))?;
            if public_hex(&root) != config.root {
                return Err(fail::recovery_code_rejected("that recovery code belongs to another account").into());
            }
            let pad = contact::new_pad();
            let card = contact::card(&code, &pad)?;
            let email = hosted::set_contact(&service, &account, Some(&pad))?;
            eprintln!(
                "Give this card to the person you trust. If you lose your recovery code and \
                 every computer, they read it to you, and `omacloud init --hosted \
                 --contact-card` with a sign-in as {email} brings your account back. On its \
                 own the card opens nothing. Earlier cards stop working."
            );
            Ok(Response::new(Value::Null, "trusted contact's card").human(card))
        }
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
    let config = load(paths)?;
    let mut e = engine(paths, &config)?;
    let chain = e.refresh_devices()?.clone();
    let members = chain.members();
    let me = e.device_key();
    _ = e.load_folders();
    // a self-hosted account's latest bucket key change, and who switched
    let bucket_key = if config.coordinator == SELF_HOSTED {
        e.bucket_key_status().ok().flatten().map(|(seq, acked)| {
            let computers: Vec<_> = members
                .valid
                .iter()
                .map(|(k, name)| json!({ "name": name, "fingerprint": fingerprint(k), "switched": acked.contains(k) }))
                .collect();
            json!({ "seq": seq, "computers": computers, "retired": config.bucket_key_retired >= seq })
        })
    } else {
        None
    };
    let st = e.state();
    // an own bucket's location comes sealed with the key
    let repo = e.repository().map_or_else(|_| config.repo.clone(), |r| r.0);
    let kind = if repo.repository == "opendal:dropbox" || is_dropbox(&config) {
        "dropbox"
    } else if repo.is_bucket() {
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
    let list = |m: &BTreeMap<String, String>| -> Vec<Value> {
        m.iter()
            .map(|(k, name)| json!({ "fingerprint": fingerprint(k), "name": name, "this": *k == me }))
            .collect()
    };
    let requests: Vec<_> = e
        .requests()
        .unwrap_or_default()
        .iter()
        .map(|r| json!({ "fingerprint": fingerprint(&r.device), "name": r.name }))
        .collect();
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
    match repo.options.get("bucket") {
        Some(bucket) if repo.is_bucket() => say!(
            out,
            "repo     {} bucket {bucket}{}",
            repo.repository,
            repo.options
                .get("endpoint")
                .map_or_else(String::new, |e| format!(" at {e}"))
        ),
        _ => say!(out, "repo     {}", repo.repository),
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
            let at_home = xdg_at_home(&config.folder);
            let others: Vec<_> = USUAL_FOLDERS
                .iter()
                .filter(|u| !synced.iter().any(|(n, _)| n == *u) && !f.skip.contains(**u))
                .filter(|u| !at_home.iter().any(|h| h == *u))
                .map(|u| {
                    let path = config.folder.join(local.get(*u).map_or(*u, String::as_str));
                    json!({ "name": u, "path": path })
                })
                .collect();
            json!({
                "synced": synced.iter()
                    .map(|(name, path)| json!({ "name": name, "path": path }))
                    .collect::<Vec<_>>(),
                "skipped": f.skip,
                "others": others,
            })
        }),
        "repo": {
            "kind": kind,
            "repository": repo.repository,
            "bucket": repo.options.get("bucket"),
            "endpoint": repo.options.get("endpoint"),
            "root": repo.options.get("root"),
            "client_id": repo.options.get("client_id").or_else(|| config.coordination.as_ref().and_then(|c| c.get("client_id"))),
        },
        "synced_head": st.base.as_ref().map(|b| b.seq),
        "synced_at": (st.synced_at > 0).then_some(st.synced_at),
        "self_hosted": config.coordinator == SELF_HOSTED,
        "hosted": hosted_service(&config).is_some(),
        "bucket_key": bucket_key,
        "epoch": st.secret.as_ref().map(|s| s.0),
        "this_device": { "fingerprint": fingerprint(&me), "role": role },
        "devices": list(&members.valid),
        "removed": list(&members.revoked),
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
    let Some(choice) = config.folders.as_mut() else {
        return Err(fail::unsupported(format!(
            "this device syncs all of {}; folders are for setups that sync folders of home",
            config.folder.display()
        ))
        .into());
    };
    let valid = |name: &str| -> Result<()> {
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return Err(fail::usage(format!("{name:?} isn't a folder directly in home"))
                .hint("give one like Music")
                .into());
        }
        Ok(())
    };
    let changed = match action {
        FoldersCmd::List => false,
        FoldersCmd::Add { name } => {
            valid(&name)?;
            let here = xdg_names(&config.folder)
                .get(&name)
                .cloned()
                .unwrap_or_else(|| name.clone());
            fs::create_dir_all(config.folder.join(here))?;
            _ = choice.skip.remove(&name);
            choice.add.insert(name)
        }
        FoldersCmd::Skip { name } => {
            valid(&name)?;
            _ = choice.add.remove(&name);
            choice.skip.insert(name)
        }
        FoldersCmd::Unskip { name } => choice.skip.remove(&name),
    };
    if changed {
        save(paths, &config)?;
    }
    // the account's folders are the ones in its history: sync to see them
    let mut e = engine(paths, &config)?;
    e.sync()?;
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
    if changed {
        say!(
            out,
            "(a running daemon picks this up when restarted: systemctl --user restart omacloud)"
        );
    }
    let data = json!({
        "synced": synced.iter().map(|(name, path)| json!({ "name": name, "path": path })).collect::<Vec<_>>(),
        "skipped": skipped,
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

/// Where coordination lives beside the data: the data's bucket (or
/// `bucket`, with the same endpoint and keys), under `omacloud-coordination`.
fn coordination_beside(repo: &RepoSpec, bucket: Option<&str>) -> Result<BTreeMap<String, String>> {
    let scheme = repo
        .repository
        .strip_prefix("opendal:")
        .context("coordinating in the bucket needs the repository to be one (`--repo opendal:s3 --opt ...`)")?;
    let mut opts: BTreeMap<String, String> = repo
        .options
        .iter()
        .filter(|(k, _)| !omacloud_core::repo::LOCAL_OPTIONS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let root = match bucket {
        Some(b) => {
            _ = opts.insert("bucket".into(), b.into());
            "/omacloud-coordination".to_string()
        }
        None => format!(
            "{}/omacloud-coordination",
            opts.get("root").map_or("", |r| r.trim_end_matches('/'))
        ),
    };
    _ = opts.insert("root".into(), root);
    _ = opts.insert("scheme".into(), scheme.to_string());
    Ok(opts)
}

/// A code from `omacloud join-code`: where the account coordinates, and
/// its id.
fn parse_join_code(code: &str) -> Result<(BTreeMap<String, String>, String)> {
    let bytes = code
        .trim()
        .strip_prefix("omacloud-join-")
        .and_then(|h| hex::decode(h).ok())
        .context("that isn't a join code from `omacloud join-code`")?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)?;
    let opts: BTreeMap<String, String> = serde_json::from_value(v["coordination"].clone())?;
    let account = v["account"]
        .as_str()
        .context("the join code names no account")?
        .to_string();
    Ok((opts, account))
}

/// Switch this device to a newer bucket key, if one was published. True
/// when it did: the caller reconnects with the new key.
fn follow_bucket_key(paths: &Paths, e: &mut Engine) -> Result<bool> {
    let mut config = load(paths)?;
    if config.coordinator != SELF_HOSTED {
        return Ok(false);
    }
    let Some((seq, key)) = e.latest_bucket_key()? else {
        return Ok(false);
    };
    if seq <= config.bucket_key_seq {
        return Ok(false);
    }
    let Some(key) = key else {
        // joined after the change, with a key at least as new: nothing to
        // switch to
        config.bucket_key_seq = seq;
        save(paths, &config)?;
        e.ack_bucket_key(seq)?;
        return Ok(false);
    };
    let coordination = config
        .coordination
        .as_mut()
        .context("no coordination location in the config")?;
    coordination.retain(|k, _| !omacloud_core::repo::CREDENTIAL_OPTIONS.contains(&k.as_str()));
    coordination.extend(key);
    config.bucket_key_seq = seq;
    save(paths, &config)?;
    e.ack_bucket_key(seq)?;
    info!("switched to bucket key {seq}");
    Ok(true)
}

/// Where an Omacloud storage account signs in and asks for keys; None for a
/// bucket of the user's own. Accounts set up before 0.0.10 didn't record it,
/// but their storage is at the service's.
fn hosted_service(config: &Config) -> Option<String> {
    if config.coordinator != SELF_HOSTED {
        return None;
    }
    config.service.clone().or_else(|| {
        let endpoint = config.coordination.as_ref()?.get("endpoint")?;
        (endpoint.trim_end_matches('/') == "https://storage.omacloud.computer").then(|| hosted::SERVICE.to_string())
    })
}

fn is_dropbox(config: &Config) -> bool {
    config
        .coordination
        .as_ref()
        .and_then(|c| c.get("scheme"))
        .is_some_and(|scheme| scheme == "dropbox")
}

/// The key a coordination location uses.
fn key_of(config: &Config) -> Result<(String, String, String)> {
    let c = config
        .coordination
        .as_ref()
        .context("no coordination location in the config")?;
    let get = |k: &str| c.get(k).cloned().unwrap_or_default();
    Ok((get("region"), get("access_key_id"), get("secret_access_key")))
}

/// A new Omacloud storage key: asked for with this computer's key, or, when
/// that key was retired (this computer missed a key change, or a removed one
/// retired the rest), by signing in with Google again.
fn hosted_key(config: &Config, service: &str, here_only: bool) -> Result<omacloud_core::bucket_key::Credentials> {
    let (region, id, secret) = key_of(config)?;
    let asked = if here_only || id.is_empty() {
        hosted::Asked::Refused
    } else {
        hosted::new_key(service, &region, (&id, &secret))?
    };
    let (id, secret) = match asked {
        hosted::Asked::Done(key) => key,
        hosted::Asked::Refused => {
            eprintln!("this computer's key isn't in use anymore: sign in again");
            let s = hosted::sign_in(service)?;
            if config.coordination.as_ref().and_then(|c| c.get("bucket")) != Some(&s.bucket) {
                return Err(fail::setup(
                    "wrong_account",
                    format!("{} is a different Omacloud account from this computer's", s.email),
                )
                .hint("sign in with the Google account this computer was set up with")
                .into());
            }
            (s.access_key_id, s.secret_access_key)
        }
    };
    Ok([
        ("access_key_id".to_string(), id),
        ("secret_access_key".to_string(), secret),
    ]
    .into())
}

/// Retire the previous hosted key or Dropbox token after every remaining
/// computer acknowledges the change. True when this computer did it now.
fn retire_old_keys(paths: &Paths, e: &mut Engine) -> Result<bool> {
    let mut config = load(paths)?;
    let dropbox = is_dropbox(&config);
    let service = hosted_service(&config);
    if !dropbox && service.is_none() {
        return Ok(false);
    }
    let Some((seq, acked)) = e.bucket_key_status()? else {
        return Ok(false);
    };
    let pending = paths.data.join(dropbox::OLD_TOKEN_FILE);
    if config.bucket_key_retired >= seq {
        if dropbox && pending.exists() {
            fs::remove_file(pending)?;
        }
        return Ok(false);
    }
    if config.bucket_key_seq < seq {
        return Ok(false);
    }
    let members = e.refresh_devices()?.members();
    if !members.valid.keys().all(|k| acked.contains(k)) {
        return Ok(false);
    }
    if dropbox {
        let old = match fs::read_to_string(&pending) {
            Ok(token) => token,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        let current = config.coordination.as_ref().context("no Dropbox location")?;
        let get = |key: &str| {
            current
                .get(key)
                .map(String::as_str)
                .with_context(|| format!("Dropbox location has no {key}"))
        };
        dropbox::revoke(get("client_id")?, get("client_secret")?, &old)?;
        config.bucket_key_retired = seq;
        save(paths, &config)?;
        fs::remove_file(pending)?;
        info!("revoked old Dropbox token after key change {seq}");
        return Ok(true);
    }
    // only a computer on the key the change shared keeps it: one that
    // joined after has a key of its own
    if !matches!(e.latest_bucket_key()?, Some((s, Some(_))) if s == seq) {
        return Ok(false);
    }
    let (region, id, secret) = key_of(&config)?;
    match hosted::retire_others(service.as_deref().expect("hosted storage"), &region, (&id, &secret))? {
        hosted::Asked::Done(n) => {
            info!("retired {n} old storage keys after key change {seq}");
            config.bucket_key_retired = seq;
            save(paths, &config)?;
            Ok(true)
        }
        hosted::Asked::Refused => Err(fail::setup("key_refused", "the storage refused this computer's key")
            .hint(KEY_REFUSED_HINT)
            .into()),
    }
}

fn bucket(paths: &Paths, action: BucketCmd) -> Out {
    let mut config = if matches!(action, BucketCmd::SetKey { here_only: true, .. }) {
        load_without_key(paths)?
    } else {
        load(paths)?
    };
    if config.coordinator != SELF_HOSTED {
        return Err(fail::unsupported("only accounts that keep in step through their bucket have a bucket key").into());
    }
    match action {
        BucketCmd::Status => {
            let mut e = engine(paths, &config)?;
            let members = e.refresh_devices()?.members();
            let Some((seq, acked)) = e.bucket_key_status()? else {
                return Ok(Response::new(
                    json!({ "seq": null, "computers": [], "waiting": 0, "retired": false }),
                    "the bucket key was never changed through omacloud",
                ));
            };
            let mut out = String::new();
            say!(out, "key change {seq}:");
            let mut computers = Vec::new();
            for (k, name) in &members.valid {
                let done = acked.contains(k);
                say!(
                    out,
                    "  {}  {name:<16} {}",
                    fingerprint(k),
                    if done { "switched" } else { "not yet" }
                );
                computers.push(json!({ "fingerprint": fingerprint(k), "name": name, "switched": done }));
            }
            let waiting = members.valid.keys().filter(|k| !acked.contains(*k)).count();
            let mut retired = false;
            let summary = if waiting == 0 && (hosted_service(&config).is_some() || is_dropbox(&config)) {
                let config_now = load(paths)?;
                retired = config_now.bucket_key_retired >= seq || retire_old_keys(paths, &mut e)?;
                if retired {
                    "every computer switched, and the old keys are retired: a removed or lost computer is cut off"
                } else if is_dropbox(&config) {
                    "every computer switched; check the computer that changed the Dropbox token to confirm the old token was revoked"
                } else {
                    "every computer switched; the old keys are retired by a computer on the new key"
                }
                .to_string()
            } else if waiting == 0 {
                "every computer switched: delete the old key at your provider; a removed or lost computer is then cut off"
                    .to_string()
            } else {
                format!("{waiting} computers still to switch; they do on their next sync")
            };
            say!(out, "{summary}");
            let data = json!({ "seq": seq, "computers": computers, "waiting": waiting, "retired": retired });
            Ok(Response::new(data, summary).human(out.trim_end()))
        }
        BucketCmd::SetKey {
            access_key_id,
            here_only,
        } => {
            let key: omacloud_core::bucket_key::Credentials = match (access_key_id, hosted_service(&config)) {
                (Some(access_key_id), _) => {
                    // the same key again would cut nobody off
                    if !here_only
                        && config.coordination.as_ref().and_then(|c| c.get("access_key_id")) == Some(&access_key_id)
                    {
                        return Err(CliError::new(
                            "same_key",
                            output::EXIT_USAGE,
                            format!("{access_key_id} is the key in use"),
                        )
                        .hint("make a new key at your provider first")
                        .into());
                    }
                    let secret = read_secret("OMACLOUD_SECRET_ACCESS_KEY", "secret access key")?;
                    [
                        ("access_key_id".to_string(), access_key_id),
                        ("secret_access_key".to_string(), secret),
                    ]
                    .into()
                }
                (None, Some(service)) => hosted_key(&config, &service, here_only)?,
                (None, None) if is_dropbox(&config) => {
                    let c = config.coordination.as_ref().context("no Dropbox location")?;
                    let id = c.get("client_id").context("no Dropbox app key")?;
                    let secret = if here_only || !c.contains_key("client_secret") {
                        read_secret("OMACLOUD_DROPBOX_CLIENT_SECRET", "Dropbox app secret")?
                    } else {
                        c["client_secret"].clone()
                    };
                    let url = dropbox::authorize_url(id)?;
                    if std::env::var_os("OMACLOUD_DROPBOX_AUTH_CODE").is_none() {
                        eprintln!("Open this Dropbox authorization URL and paste its code:\n\n  {url}\n");
                    }
                    let code = read_secret("OMACLOUD_DROPBOX_AUTH_CODE", "Dropbox authorization code")?;
                    let refresh = dropbox::exchange(id, &secret, &code)?;
                    if !here_only && c.get("refresh_token") == Some(&refresh) {
                        return Err(CliError::new(
                            "same_key",
                            output::EXIT_USAGE,
                            "Dropbox returned the token already in use; it would not lock out the removed computer",
                        )
                        .hint("authorize the app again for a new code")
                        .into());
                    }
                    [
                        ("client_secret".to_string(), secret.clone()),
                        ("refresh_token".to_string(), refresh),
                    ]
                    .into()
                }
                (None, None) => {
                    return Err(fail::usage("give the new key")
                        .hint("make it at your provider, then `--access-key-id <id>`")
                        .into());
                }
            };
            // does the new key reach the bucket?
            let mut trial = config.clone();
            if let Some(c) = trial.coordination.as_mut() {
                c.retain(|k, _| !omacloud_core::repo::CREDENTIAL_OPTIONS.contains(&k.as_str()));
                c.extend(key.clone());
            }
            match coordinator(&trial)?.anchor() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(CliError::new(
                        "no_account",
                        output::EXIT_NOT_FOUND,
                        "the new key reaches a bucket with no account in it",
                    )
                    .into());
                }
                Err(e) if key_refused(&e) => {
                    return Err(
                        fail::setup("key_refused", format!("the new key doesn't reach the bucket: {e:#}"))
                            .hint("check the key's id and secret, and that it may read and write this bucket")
                            .into(),
                    );
                }
                Err(e) => return Err(e.context("the new key doesn't reach the bucket")),
            }
            if here_only {
                config = trial;
                save(paths, &config)?;
                return Ok(Response::new(
                    json!({ "seq": null, "here_only": true }),
                    "this computer uses the new key; it picks up the account's key change on the next sync",
                ));
            }
            // published with the old key, which still works
            let mut e = engine(paths, &config)?;
            let pending = paths.data.join(dropbox::OLD_TOKEN_FILE);
            if is_dropbox(&config) {
                // A failed publish can leave the locally saved old token.
                // Discard it only when the account has no newer key record.
                let existing = match fs::read_to_string(&pending) {
                    Ok(token) => Some(token),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                    Err(err) => return Err(err.into()),
                };
                if let Some(old) = existing {
                    let current = config.coordination.as_ref().and_then(|c| c.get("refresh_token"));
                    let latest = e.bucket_key_status()?.map_or(0, |(seq, _)| seq);
                    if current == Some(&old) && latest <= config.bucket_key_seq {
                        fs::remove_file(&pending)?;
                    }
                }
                if pending.exists() {
                    return Err(fail::usage("an earlier Dropbox token still awaits revocation")
                        .hint("wait until every computer switched (`omacloud bucket status`), then try again")
                        .into());
                }
                let old = config
                    .coordination
                    .as_ref()
                    .and_then(|c| c.get("refresh_token"))
                    .context("no old Dropbox token")?;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&pending)?;
                file.write_all(old.as_bytes())?;
                file.sync_all()?;
            }
            let seq = e.change_bucket_key(&key)?;
            config = trial;
            config.bucket_key_seq = seq;
            save(paths, &config)?;
            let text = if hosted_service(&config).is_some() || is_dropbox(&config) {
                format!(
                    "key change {seq} published, sealed to this account's computers. Each switches on \
                     its next sync, and once all have, the old keys are retired: a removed computer is \
                     then cut off. `omacloud bucket status` shows which have."
                )
            } else {
                format!(
                    "key change {seq} published, sealed to this account's computers. Each switches on its \
                     next sync; `omacloud bucket status` shows which have. Keep the old key until all have, \
                     then delete it at your provider."
                )
            };
            Ok(Response::new(
                json!({ "seq": seq, "here_only": false }),
                format!("key change {seq} published"),
            )
            .human(text))
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
