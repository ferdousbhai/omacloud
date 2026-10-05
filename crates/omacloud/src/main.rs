//! `omacloud`: set up a synced folder, sync it once, or keep it synced.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

mod alerts;
mod hosted;
mod keyring;
mod power;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use log::{info, warn};
use notify::{RecursiveMode, Watcher};
use omacloud_core::{
    Coordinator, DeviceError, DirCoordinator, Engine, HeadError, Membership, RepoSpec,
    SettingsSetup, Setup, account,
    bucket::key_refused,
    devices::{fingerprint, public_hex},
    epoch::EpochError,
    head::load_or_create_key,
    settings::Manifest,
};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(
    version,
    about = "Omacloud: your files and settings on every computer, end to end encrypted"
)]
struct Cli {
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
        /// Where the files live: an opendal URL like `opendal:s3` (with
        /// `--opt endpoint=... --opt bucket=... --opt access_key_id=...`; the
        /// secret key is asked for), or a path. Joining with a join code needs
        /// none
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
    /// Show this device's sync state
    Status {
        /// Everything as JSON, for the Omacloud app
        #[arg(long)]
        json: bool,
    },
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
    },
    /// Move to a new repository key, locking out removed devices. Uploads
    /// everything again
    Rotate,
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
    /// Social recovery: split the recovery code among people you trust, or
    /// rebuild it from their shares
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
}

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
    /// Switch every computer to a new key (make it at your provider first;
    /// keep the old one until `bucket status` says all switched, then delete
    /// it there). The secret comes from OMACLOUD_SECRET_ACCESS_KEY, or is
    /// asked for
    SetKey {
        /// The new key's id
        #[arg(long)]
        access_key_id: String,
        /// Only this computer: for one that missed the change because the old
        /// key was already deleted
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
const USUAL_FOLDERS: &[&str] = &[
    "Desktop",
    "Documents",
    "Pictures",
    "Music",
    "Videos",
    "Downloads",
];

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
            _ = repo
                .options
                .entry("bandwidth".into())
                .or_insert_with(|| bw.clone());
        }
        if let Some(n) = self.connections {
            _ = repo
                .options
                .entry("connections".into())
                .or_insert(n.to_string());
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
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME not set")?;
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var).map_or_else(|| home.join(fallback), PathBuf::from)
        };
        let config = config
            .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("omacloud/config.toml"));
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
    let text = fs::read_to_string(&paths.config).with_context(|| {
        format!(
            "reading {} (run `omacloud init` first)",
            paths.config.display()
        )
    })?;
    let mut config: Config = toml::from_str(&text)?;
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
    let text = fs::read_to_string(&paths.config)
        .with_context(|| format!("reading {}", paths.config.display()))?;
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
        repo.options
            .extend(omacloud_core::bucket_key::credentials_of(c));
    }
    let setup = Setup {
        device: config.device.clone(),
        folder: config.folder.clone(),
        repo,
        signing,
        root: config.root.clone(),
        state_path: Some(paths.state()),
        settings: if config.settings {
            Some(SettingsSetup {
                home: home_dir()?,
                manifest: Manifest::load()?,
                backups: paths.data.join("settings"),
                packages: paths.data.join("packages"),
                secrets: paths.data.join("secrets"),
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

fn init(paths: &Paths, mut config: Config, recovery: Option<String>) -> Result<()> {
    if paths.config.exists() {
        bail!(
            "{} exists; this device is already set up",
            paths.config.display()
        );
    }
    fs::create_dir_all(&config.folder)?;
    fs::create_dir_all(&paths.data)?;
    let signing = load_or_create_key(&paths.device_key())?;
    let me = fingerprint(&public_hex(&signing));
    let coord = coordinator(&config)?;
    let creating = coord.anchor()?.is_none();

    let message = if creating {
        if config.repo.repository.is_empty() {
            bail!("creating an account needs --repo");
        }
        let created = account::create(coord.as_ref(), &config.repo, &signing, &config.device)?;
        config.root = created.root;
        format!(
            "created repository and account {}\n\n\
             account id: {}\n\n\
             recovery code: {}\n\n\
             Write it down and keep it offline. It is shown once. With it you can add a\n\
             device when no other device is at hand, and recover the account; without\n\
             it and without a device, the data is gone.\n\n\
             repository password (for leaving omacloud, also in `omacloud export`): {}\n\n\
             Other computers join with a code from `omacloud join-code`.",
            config.repo.repository, config.root, created.recovery_code, created.password
        )
    } else if let Some(code) = recovery {
        config.root = account::join_with_recovery(coord.as_ref(), &code, &signing, &config.device)?;
        "joined the account with the recovery code".to_string()
    } else {
        config.root = account::request(coord.as_ref(), &signing, &config.device)?;
        format!(
            "asked to join {}\n\n\
             this device's fingerprint: {me}\n\n\
             On a device that already syncs, run `omacloud devices` and check that it\n\
             shows the same fingerprint, then `omacloud devices approve {me}`.",
            config.repo.repository
        )
    };
    // bucket credentials live sealed in the account's key records, not in
    // the config file
    config.repo = config.repo.without_credentials();
    save(paths, &config)?;
    println!("{message}");
    println!("\naccount root: {}", fingerprint(&config.root));
    if paths.is_default() {
        // the packaged unit starts at login for users with a config; start
        // it now for this session
        println!("\nstart syncing now: systemctl --user start omacloud");
    }
    Ok(())
}

/// Keep the folders in sync. Returns when sync must stop: this device was
/// removed, or the account can't be verified.
fn watch(paths: &Paths, config: &Config, poll: Duration) -> Result<()> {
    power::be_nice();
    let mut engine = engine(paths, config)?;
    let (files, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            _ = files.send(event.paths);
        }
    })?;
    // syncing folders of home: watch those, never all of home
    let mut watched = std::collections::BTreeSet::new();
    if config.folders.is_none() {
        watcher.watch(&config.folder, RecursiveMode::Recursive)?;
    }
    for dir in engine.settings_watch_dirs() {
        // settings live in a few directories under home; watch just those
        if dir.is_dir() {
            watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        }
    }
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
    loop {
        // `omacloud folders` edits the config: follow it without a restart
        let now = config_mtime();
        if now != config_seen {
            config_seen = now;
            if let Ok(Config {
                folders: Some(choice),
                ..
            }) = load(paths)
            {
                info!("folder choice changed");
                engine.set_folders(choice.add, choice.skip);
                folders_changed = true;
            }
        }
        // folders appear as the account adds them
        for dir in engine.synced_folders() {
            if !watched.contains(&dir)
                && dir.is_dir()
                && watcher.watch(&dir, RecursiveMode::Recursive).is_ok()
            {
                info!("watching {}", dir.display());
                _ = watched.insert(dir);
            }
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
            info!(
                "{} sync (battery)",
                if paused { "pausing" } else { "resuming" }
            );
        }
        // idle: one look at the change marker instead of a sync, with a full
        // sync now and then in case a write didn't mark itself
        let marker = if paused {
            None
        } else {
            engine.remote_marker().ok().flatten()
        };
        let idle = !engine.has_pending()
            && !folders_changed
            && marker.is_some()
            && marker == seen_marker
            && full_at.is_some_and(|t: Instant| t.elapsed() < FULL_SYNC);
        if !paused && !idle {
            folders_changed = false;
            match run_sync(&mut engine, &mut waiting, alerts.as_ref()) {
                Synced::Stop => return Ok(()),
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
            }
            if let Some(a) = alerts.as_mut() {
                a.check(&mut engine);
            }
        }
        match rx.recv_timeout(poll) {
            Ok(paths) => {
                engine.notice(paths);
                let first = Instant::now();
                loop {
                    match rx.recv_timeout(quiet) {
                        Ok(more) => {
                            engine.notice(more);
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
        Err(e) if key_refused(&e) => warn!("{KEY_REFUSED}: {e:#}"),
        Err(e) => warn!("sync failed, will retry: {e:#}"),
    }
    Synced::Failed
}

fn main() -> Result<()> {
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
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,rustic_core=warn"),
    )
    .init();
    let cli = Cli::parse();
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
        } => {
            // Omacloud storage: signing in gives the bucket and its key
            let (repo, opts) = if hosted {
                let service = std::env::var("OMACLOUD_SERVICE_URL")
                    .unwrap_or_else(|_| hosted::SERVICE.to_string());
                let s = hosted::sign_in(&service)?;
                println!("signed in as {}", s.email);
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
            let joined = join_code.as_deref().map(parse_join_code).transpose()?;
            // in the bucket, unless a shared directory is given
            let coordinator = match (coordinator, &joined, repo.as_deref()) {
                (_, Some(_), _) => SELF_HOSTED.to_string(),
                (Some(c), _, _) => c,
                (None, None, Some(r)) if r.starts_with("opendal:") => SELF_HOSTED.to_string(),
                (None, None, _) => bail!(
                    "sign in to Omacloud storage with `--hosted`, or give a bucket of your own with \
                     `--repo opendal:s3 --opt endpoint=... --opt bucket=... --opt access_key_id=...` \
                     (the secret key is asked for, or comes from OMACLOUD_SECRET_ACCESS_KEY), or join an account with `--join-code` \
                     (from `omacloud join-code` on one of its computers). The Omacloud app \
                     (omacloud-app) walks through the same."
                ),
            };
            let coordinator = if coordinator == SELF_HOSTED {
                coordinator
            } else {
                absolute(Path::new(&coordinator))?
                    .to_string_lossy()
                    .into_owned()
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
                        o
                    },
                },
            };
            // no bucket given: make a private one with a random name
            let mut repo = repo;
            if repo.repository == "opendal:s3" && !repo.options.contains_key("bucket") {
                let opt = |k: &str| repo.options.get(k).cloned().unwrap_or_default();
                let (endpoint, region) = (opt("endpoint"), opt("region"));
                anyhow::ensure!(
                    !endpoint.is_empty(),
                    "give the storage's endpoint: --opt endpoint=..."
                );
                let region = if region.is_empty() {
                    "auto".to_string()
                } else {
                    region
                };
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
                println!("created bucket {name}");
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
            };
            let mut config = config;
            if config.coordinator == SELF_HOSTED {
                config.coordination = Some(match joined {
                    Some((opts, _)) => opts,
                    None => coordination_beside(&config.repo, coordination_bucket.as_deref())?,
                });
            }
            init(&paths, config, recovery_code)
        }
        Cmd::Sync => {
            let config = load(&paths)?;
            let mut e = engine(&paths, &config)?;
            record_packages(&config, &mut e);
            let st = match e.sync() {
                Err(err) if err.downcast_ref::<Membership>() == Some(&Membership::NotApproved) => {
                    bail!(
                        "this device ({}) is not approved yet: run `omacloud devices approve {}` on a device that syncs",
                        fingerprint(&e.device_key()),
                        fingerprint(&e.device_key())
                    )
                }
                Err(err) if config.coordinator == SELF_HOSTED && key_refused(&err) => {
                    return Err(err.context(KEY_REFUSED));
                }
                other => other?,
            };
            println!(
                "pushed {}, pulled {}, conflicts {}",
                st.pushed, st.pulled, st.conflicts
            );
            if follow_bucket_key(&paths, &mut e)? {
                println!("switched to the account's new bucket key");
            }
            Ok(())
        }
        Cmd::Watch { poll } => {
            watch(&paths, &load(&paths)?, Duration::from_secs(poll))?;
            // the account stopped (this device was removed): a human looks
            std::process::exit(2);
        }
        Cmd::Status { json: true } => {
            println!("{}", status_json(&paths)?);
            Ok(())
        }
        Cmd::Status { json: false } => {
            let config = load(&paths)?;
            let mut e = engine(&paths, &config)?;
            e.refresh_devices()?;
            _ = e.load_folders();
            let st = e.state();
            println!("device   {}", config.device);
            println!("account  {}", config.root);
            if config.folders.is_some() {
                let dirs: Vec<String> = e
                    .synced_folders()
                    .iter()
                    .map(|d| format!("~/{}", d.file_name().unwrap_or_default().to_string_lossy()))
                    .collect();
                println!("folders  {}", dirs.join(", "));
            } else {
                println!("folder   {}", config.folder.display());
            }
            // an own bucket's location comes sealed with the key
            let repo = e.repository().map_or_else(|_| config.repo.clone(), |r| r.0);
            match repo.options.get("bucket") {
                Some(bucket) if repo.is_bucket() => println!(
                    "repo     {} bucket {bucket}{}",
                    repo.repository,
                    repo.options
                        .get("endpoint")
                        .map_or_else(String::new, |e| format!(" at {e}"))
                ),
                _ => println!("repo     {}", repo.repository),
            }
            match &st.base {
                Some(b) => println!("synced   head {} (snapshot {})", b.seq, &b.snapshot[..8]),
                None => println!("synced   nothing yet"),
            }
            if let Some((epoch, _)) = &st.secret {
                println!("epoch    {epoch}");
            }
            let members = st.devices.members();
            let me = e.device_key();
            let role = if members.valid.contains_key(&me) {
                "member"
            } else if members.revoked.contains_key(&me) {
                "removed"
            } else {
                "waiting for approval"
            };
            println!(
                "device   {} ({role}, {} devices)",
                fingerprint(&me),
                members.valid.len()
            );
            Ok(())
        }
        Cmd::DeviceKey => {
            let key = public_hex(&load_or_create_key(&paths.device_key())?);
            println!("{key}\nfingerprint {}", fingerprint(&key));
            Ok(())
        }
        Cmd::Devices { action } => devices(&paths, action.unwrap_or(DevicesCmd::List)),
        Cmd::Versions { path } => {
            let config = load(&paths)?;
            let rel = relative(&config, &path)?;
            for v in engine(&paths, &config)?.versions(&rel)? {
                let what = v
                    .size
                    .map_or_else(|| "deleted".to_string(), |s| format!("{s} bytes"));
                println!(
                    "{:>6}  {}  {:<12}  {what}",
                    v.seq,
                    v.time
                        .to_zoned(omacloud_core::jiff::tz::TimeZone::system())
                        .strftime("%Y-%m-%d %H:%M:%S"),
                    v.device
                );
            }
            Ok(())
        }
        Cmd::Restore { path, at, to } => {
            let config = load(&paths)?;
            let rel = relative(&config, &path)?;
            let dest = to.unwrap_or_else(|| {
                let name = rel
                    .file_name()
                    .map_or_else(Default::default, |n| n.to_string_lossy());
                PathBuf::from(format!("{name}.v{at}"))
            });
            engine(&paths, &config)?.restore_version(&rel, at, &dest)?;
            println!("wrote {}", dest.display());
            Ok(())
        }
        Cmd::Settings { action } => settings(&paths, action.unwrap_or(SettingsCmd::Status)),
        Cmd::Secrets { action } => secrets(&paths, action.unwrap_or(SecretsCmd::List)),
        Cmd::JoinCode => {
            let config = load(&paths)?;
            let coordination = config
                .coordination
                .as_ref()
                .filter(|_| config.coordinator == SELF_HOSTED)
                .context("this account uses a coordinator; other devices join with --account")?;
            let blob = serde_json::to_vec(&serde_json::json!({
                "coordination": coordination,
                "account": config.root,
            }))?;
            println!("omacloud-join-{}", hex::encode(blob));
            eprintln!(
                "It holds your bucket's keys: pass it privately. The new device runs\n  \
                 omacloud init --join-code <code>\nand then needs your approval (or your recovery code)."
            );
            Ok(())
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
            print_settings(&e.settings_status());
            record_packages(&config, &mut e);
            let st = e.sync()?;
            println!("files and settings: pulled {}", st.pulled);
            packages(
                &paths,
                PackagesCmd::Restore {
                    from: from.clone(),
                    yes,
                },
            )?;
            // secrets only ever on request, with the recovery code
            if e.secrets_devices().contains(&from) {
                println!(
                    "{from} saved a secrets bundle (ssh and gpg keys, tokens). To put it here: \
                     omacloud secrets restore --from {from}"
                );
            }
            // the packaged unit runs for users with a config; start it now
            let started = std::process::Command::new("systemctl")
                .args(["--user", "start", "omacloud"])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if started {
                println!("keeping in sync from now on (systemd user unit omacloud)");
            } else {
                println!(
                    "to keep in sync, run `omacloud watch` (or install the package for its unit)"
                );
            }
            Ok(())
        }
        Cmd::Rotate => {
            let config = load(&paths)?;
            let r = engine(&paths, &config)?.rotate()?;
            println!(
                "moved to repository epoch {} ({} snapshots copied)",
                r.epoch, r.snapshots
            );
            print_leftover(&r);
            Ok(())
        }
        Cmd::Export { to, to_opts } => {
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
            println!("# restore without omacloud: restic or rustic, this repository and password");
            println!("# (repository epoch {epoch})");
            println!("repository: {repo}");
            for (k, v) in &spec.options {
                println!("option:     {k}={v}");
            }
            println!("password:   {password}");
            println!();
            if repo.starts_with("opendal:") {
                // restic can't read opendal URLs; rustic can, from a profile
                println!(
                    "# save as ~/.config/rustic/rustic.toml (the default profile), then: rustic restore latest <dir>"
                );
                println!("[repository]\nrepository = \"{repo}\"\npassword = \"{password}\"");
                println!("\n[repository.options]");
                for (k, v) in &spec.options {
                    println!("{k} = \"{v}\"");
                }
            } else {
                println!(
                    "RESTIC_PASSWORD='{password}' restic -r {repo} restore latest --target <dir>"
                );
            }
            Ok(())
        }
    }
}

fn devices(paths: &Paths, action: DevicesCmd) -> Result<()> {
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
            0 => bail!("no device with fingerprint {fp}"),
            _ => bail!("{fp} matches several devices; give more of it"),
        }
    };
    match action {
        DevicesCmd::List => {
            println!("account root {}", fingerprint(&chain.root));
            println!("\ndevices:");
            for (k, name) in &members.valid {
                let this = if *k == me { "  (this device)" } else { "" };
                println!("  {}  {name}{this}", fingerprint(k));
            }
            if !members.revoked.is_empty() {
                println!("\nremoved:");
                for (k, name) in &members.revoked {
                    println!("  {}  {name}", fingerprint(k));
                }
            }
            let requests = e.requests()?;
            if !requests.is_empty() {
                println!("\nasking to join (compare with the fingerprint the device shows):");
                for r in &requests {
                    println!("  {}  {}", fingerprint(&r.device), r.name);
                }
            }
        }
        DevicesCmd::Approve { fingerprint: fp } => {
            let requests = e.requests()?;
            let keys = requests
                .iter()
                .map(|r| (r.device.clone(), r.name.clone()))
                .collect();
            let (key, name) = pick(&fp, keys)?;
            let req = requests
                .into_iter()
                .find(|r| r.device == key)
                .context("request vanished")?;
            e.approve(&req)?;
            println!("approved {name} ({})", fingerprint(&key));
        }
        DevicesCmd::Revoke {
            fingerprint: fp,
            rotate,
        } => {
            let (key, name) = pick(&fp, members.valid.into_iter().collect())?;
            if key == me {
                bail!("remove this device from another one");
            }
            e.revoke(&key)?;
            println!(
                "removed {name} ({}). It can't push anymore.",
                fingerprint(&key)
            );
            if rotate {
                let r = e.rotate()?;
                println!(
                    "moved to repository epoch {} ({} snapshots copied); {name} can't read it",
                    r.epoch, r.snapshots
                );
                print_leftover(&r);
            } else {
                println!(
                    "It still holds the repository key it had. If the device is lost or not \
                     trusted, run `omacloud rotate`."
                );
            }
        }
    }
    Ok(())
}

/// This machine's explicitly installed packages, repository and foreign
/// (AUR) apart; `None` without pacman.
fn package_list(device: &str) -> Option<String> {
    let run = |args: &[&str]| -> Option<Vec<String>> {
        let out = std::process::Command::new("pacman")
            .args(args)
            .output()
            .ok()?;
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

fn packages(paths: &Paths, action: PackagesCmd) -> Result<()> {
    let config = load(paths)?;
    if !config.settings {
        bail!("package lists travel with settings sync: `omacloud settings on`");
    }
    let mut e = engine(paths, &config)?;
    record_packages(&config, &mut e);
    e.sync()?;
    let dir = paths.data.join("packages");
    let read = |device: &str| -> Result<(Vec<String>, Vec<String>)> {
        let text = fs::read_to_string(dir.join(format!("{device}.txt")))
            .with_context(|| format!("no package list from {device}"))?;
        let (mut repo, mut foreign, mut section) = (Vec::new(), Vec::new(), "");
        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
        {
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
                        .filter_map(|e| {
                            e.file_name()
                                .to_string_lossy()
                                .strip_suffix(".txt")
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            devices.sort();
            for d in devices {
                let (repo, foreign) = read(&d)?;
                println!(
                    "{d:<16} {} packages, {} from the AUR",
                    repo.len(),
                    foreign.len()
                );
            }
        }
        PackagesCmd::Restore { from, yes } => {
            let (repo, foreign) = read(&from)?;
            let installed: std::collections::BTreeSet<String> =
                std::process::Command::new("pacman")
                    .arg("-Qq")
                    .output()
                    .map(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
            let missing: Vec<&String> = repo.iter().filter(|p| !installed.contains(*p)).collect();
            let missing_aur: Vec<&String> =
                foreign.iter().filter(|p| !installed.contains(*p)).collect();
            if missing.is_empty() && missing_aur.is_empty() {
                println!("everything {from} has is installed here");
                return Ok(());
            }
            let list = |v: &[&String]| v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
            if !missing.is_empty() {
                println!(
                    "from the repositories ({}): {}",
                    missing.len(),
                    list(&missing)
                );
            }
            if !missing_aur.is_empty() {
                println!(
                    "from the AUR ({}), install these yourself: {}",
                    missing_aur.len(),
                    list(&missing_aur)
                );
            }
            if yes && !missing.is_empty() {
                let status = std::process::Command::new("sudo")
                    .args(["pacman", "-S", "--needed"])
                    .args(missing.iter().map(|s| s.as_str()))
                    .status()?;
                anyhow::ensure!(status.success(), "pacman did not finish");
            } else if !missing.is_empty() {
                println!("install with: omacloud packages restore --from {from} --yes");
            }
        }
    }
    Ok(())
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "device".to_string())
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}

fn settings(paths: &Paths, action: SettingsCmd) -> Result<()> {
    let mut config = load(paths)?;
    match action {
        SettingsCmd::On | SettingsCmd::Off => {
            config.settings = matches!(action, SettingsCmd::On);
            save(paths, &config)?;
            let e = engine(paths, &config)?;
            print_settings(&e.settings_status());
            if config.settings {
                println!(
                    "they sync at the next `omacloud sync`, or right away if the daemon runs (restart it: systemctl --user restart omacloud)"
                );
            }
        }
        SettingsCmd::Status => print_settings(&engine(paths, &config)?.settings_status()),
        SettingsCmd::Resolve { path, keep } => {
            let home = home_dir()?;
            let abs = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()?.join(path)
            };
            let rel = abs
                .strip_prefix(&home)
                .with_context(|| format!("{} is not under your home", abs.display()))?;
            let mut e = engine(paths, &config)?;
            e.resolve_setting(rel, matches!(keep, Keep::Remote))?;
            e.sync()?;
            println!("resolved {}", rel.display());
        }
    }
    Ok(())
}

fn print_settings(st: &omacloud_core::SettingsStatus) {
    let source = if std::env::var_os("OMARCHY_PATH")
        .map_or_else(|| PathBuf::from("/usr/share/omarchy"), PathBuf::from)
        .join("default/dots/manifest")
        .is_file()
    {
        "Omarchy's dots manifest"
    } else {
        "the dots manifest bundled with omacloud"
    };
    match (st.on, &st.dormant) {
        (false, _) => println!("settings sync is off (`omacloud settings on`)"),
        (true, Some(why)) => println!("settings sync stands down: {why}"),
        (true, None) => println!("settings sync is on, following {source}"),
    }
    for p in &st.held {
        println!(
            "  waiting: ~/{} changed here and on another device; `omacloud settings resolve ~/{} --keep local|remote`",
            p.display(),
            p.display()
        );
    }
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
    bail!(
        "{} is not inside {}",
        path.display(),
        config.folder.display()
    )
}

fn absolute(p: &Path) -> Result<PathBuf> {
    fs::create_dir_all(p)?;
    Ok(fs::canonicalize(p)?)
}

fn print_leftover(r: &omacloud_core::Rotated) {
    if r.left == 0 {
        println!("the old repository is deleted");
    } else {
        println!(
            "the bucket's delete protection keeps {} or more files of the old repository; \
             they are deleted once their retention ends",
            r.left
        );
    }
}

fn secrets(paths: &Paths, action: SecretsCmd) -> Result<()> {
    use omacloud_core::secrets;
    let config = load(paths)?;
    if !config.settings {
        bail!("the secrets bundle travels with settings sync: `omacloud settings on`");
    }
    let mut e = engine(paths, &config)?;
    let home = home_dir()?;
    match action {
        SecretsCmd::List => {
            e.sync()?;
            let devices = e.secrets_devices();
            if devices.is_empty() {
                println!("no secrets bundles; save this device's with `omacloud secrets save`");
            }
            for d in devices {
                println!("{d}");
            }
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
                bail!("none of the secrets paths exist here");
            }
            for en in &entries {
                println!("  ~/{}", en.path.display());
            }
            e.put_secrets(&secrets::seal(&config.root, &entries)?)?;
            e.sync()?;
            println!(
                "sealed {} files for {}; only the recovery code opens them",
                entries.len(),
                config.device
            );
        }
        SecretsCmd::Restore { from, yes } => {
            e.sync()?;
            let sealed = e.secrets_of(&from)?;
            let root = omacloud_core::devices::root_key(&read_recovery_code()?)?;
            let entries = secrets::open(&root, &sealed)?;
            let changes = secrets::changes(&home, &entries);
            if changes.is_empty() {
                println!("this device already has {from}'s secrets");
                return Ok(());
            }
            for en in &changes {
                let what = if home.join(&en.path).exists() {
                    "replace"
                } else {
                    "new"
                };
                println!("  {what:<8} ~/{}", en.path.display());
            }
            if !yes {
                println!("write these with: omacloud secrets restore --from {from} --yes");
                return Ok(());
            }
            let stamp = omacloud_core::jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
            let backups = paths.data.join(format!("secrets-backup/{stamp}"));
            let written = secrets::restore(&home, &entries, &backups)?;
            println!("wrote {} files", written.len());
            if backups.exists() {
                println!("the files they replaced are in {}", backups.display());
            }
        }
    }
    Ok(())
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
    anyhow::ensure!(!secret.is_empty(), "no {what} given");
    Ok(secret)
}

fn recovery(paths: &Paths, action: RecoveryCmd) -> Result<()> {
    use omacloud_core::shamir;
    match action {
        RecoveryCmd::Split { threshold, shares } => {
            let code = read_recovery_code()?;
            let root = public_hex(&omacloud_core::devices::root_key(&code)?);
            // on a set up device, make sure it's this account's code
            if let Ok(config) = load(paths)
                && config.root != root
            {
                bail!("that recovery code belongs to another account");
            }
            for share in shamir::split(&code, threshold, shares)? {
                println!("{share}");
            }
            eprintln!(
                "any {threshold} of these {shares} shares rebuild the recovery code \
                 (`omacloud recovery combine`). Give each to a different person; keep none \
                 together with the others"
            );
        }
        RecoveryCmd::Combine => {
            eprintln!("paste shares, one per line:");
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
                        eprintln!("{e:#}; try that one again");
                        continue;
                    }
                };
                got.push(line.to_string());
                if got.len() >= usize::from(k) {
                    break;
                }
                eprintln!("{} of {k}", got.len());
            }
            println!("{}", shamir::combine(&got)?);
        }
    }
    Ok(())
}

/// `status --json`: what the Omacloud app shows. Reads only; no sync.
fn status_json(paths: &Paths) -> Result<serde_json::Value> {
    use serde_json::json;
    // a computer to set up, not an error
    if !paths.config.exists() {
        return Ok(json!({ "set_up": false }));
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
                .map(|(k, name)| {
                    json!({ "name": name, "fingerprint": fingerprint(k), "switched": acked.contains(k) })
                })
                .collect();
            json!({ "seq": seq, "computers": computers })
        })
    } else {
        None
    };
    let st = e.state();
    let repo = e.repository().map_or_else(|_| config.repo.clone(), |r| r.0);
    let kind = if repo.is_bucket() { "bucket" } else { "path" };
    let role = if members.valid.contains_key(&me) {
        "member"
    } else if members.revoked.contains_key(&me) {
        "removed"
    } else {
        "waiting"
    };
    let list = |m: &std::collections::BTreeMap<String, String>| -> Vec<serde_json::Value> {
        m.iter()
            .map(|(k, name)| {
                json!({ "fingerprint": fingerprint(k), "name": name, "this": *k == me })
            })
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
        .status()
        .is_ok_and(|s| s.success());
    Ok(json!({
        "device": config.device,
        "account": config.root,
        "account_fingerprint": fingerprint(&config.root),
        "folder": config.folder,
        "folders": config.folders.as_ref().map(|f| {
            let synced = e.folder_list();
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
                "synced": synced.into_iter()
                    .map(|(name, path)| json!({ "name": name, "path": path }))
                    .collect::<Vec<_>>(),
                "skipped": f.skip,
                "others": others,
            })
        }),
        "repo": {
            "kind": kind,
            "bucket": repo.options.get("bucket"),
            "endpoint": repo.options.get("endpoint"),
        },
        "synced_head": st.base.as_ref().map(|b| b.seq),
        "synced_at": (st.synced_at > 0).then_some(st.synced_at),
        "self_hosted": config.coordinator == SELF_HOSTED,
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
        },
        "secrets": e.secrets_devices(),
        "daemon": daemon,
    }))
}

fn folders(paths: &Paths, action: FoldersCmd) -> Result<()> {
    let mut config = load(paths)?;
    let Some(choice) = config.folders.as_mut() else {
        bail!(
            "this device syncs all of {}; folders are for setups that sync folders of home",
            config.folder.display()
        );
    };
    let valid = |name: &str| -> Result<()> {
        anyhow::ensure!(
            !name.is_empty() && !name.contains('/') && !name.starts_with('.'),
            "a folder here is one directly in home, like Music"
        );
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
    for dir in e.synced_folders() {
        println!("{}", dir.display());
    }
    let skipped = &config.folders.as_ref().expect("checked above").skip;
    if !skipped.is_empty() {
        let list: Vec<&str> = skipped.iter().map(String::as_str).collect();
        println!("skipped here: {}", list.join(", "));
    }
    if changed {
        println!(
            "(a running daemon picks this up when restarted: systemctl --user restart omacloud)"
        );
    }
    Ok(())
}

/// Where coordination lives beside the data: the data's bucket (or
/// `bucket`, with the same endpoint and keys), under `omacloud-coordination`.
fn coordination_beside(repo: &RepoSpec, bucket: Option<&str>) -> Result<BTreeMap<String, String>> {
    let scheme = repo.repository.strip_prefix("opendal:").context(
        "coordinating in the bucket needs the repository to be one (`--repo opendal:s3 --opt ...`)",
    )?;
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

/// Why a self-hosted computer can't reach its bucket, and the way back.
const KEY_REFUSED: &str = "the bucket refused this computer's key. If the account changed \
     its key while this computer was away, get the new key from your provider and run \
     `omacloud bucket set-key --access-key-id <id> --here-only`";

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

fn bucket(paths: &Paths, action: BucketCmd) -> Result<()> {
    let mut config = if matches!(
        action,
        BucketCmd::SetKey {
            here_only: true,
            ..
        }
    ) {
        load_without_key(paths)?
    } else {
        load(paths)?
    };
    anyhow::ensure!(
        config.coordinator == SELF_HOSTED,
        "only self-hosted accounts keep their own bucket key"
    );
    match action {
        BucketCmd::Status => {
            let mut e = engine(paths, &config)?;
            let members = e.refresh_devices()?.members();
            match e.bucket_key_status()? {
                None => println!("the bucket key was never changed through omacloud"),
                Some((seq, acked)) => {
                    println!("key change {seq}:");
                    let mut waiting = 0;
                    for (k, name) in &members.valid {
                        let done = acked.contains(k);
                        if !done {
                            waiting += 1;
                        }
                        println!(
                            "  {}  {name:<16} {}",
                            fingerprint(k),
                            if done { "switched" } else { "not yet" }
                        );
                    }
                    if waiting == 0 {
                        println!(
                            "every computer switched: delete the old key at your provider; a removed or lost computer is then cut off"
                        );
                    } else {
                        println!("{waiting} computers still to switch; they do on their next sync");
                    }
                }
            }
        }
        BucketCmd::SetKey {
            access_key_id,
            here_only,
        } => {
            // the same key again would cut nobody off
            anyhow::ensure!(
                here_only
                    || config
                        .coordination
                        .as_ref()
                        .and_then(|c| c.get("access_key_id"))
                        != Some(&access_key_id),
                "{access_key_id} is the key in use: make a new one at your provider first"
            );
            let secret = read_secret("OMACLOUD_SECRET_ACCESS_KEY", "secret access key")?;
            let key: omacloud_core::bucket_key::Credentials = [
                ("access_key_id".to_string(), access_key_id),
                ("secret_access_key".to_string(), secret),
            ]
            .into();
            // does the new key reach the bucket?
            let mut trial = config.clone();
            if let Some(c) = trial.coordination.as_mut() {
                c.retain(|k, _| !omacloud_core::repo::CREDENTIAL_OPTIONS.contains(&k.as_str()));
                c.extend(key.clone());
            }
            coordinator(&trial)?
                .anchor()
                .context("the new key doesn't reach the bucket")?
                .context("the new key reaches a bucket with no account in it")?;
            if here_only {
                config = trial;
                save(paths, &config)?;
                println!(
                    "this computer uses the new key; it picks up the account's key change on the next sync"
                );
                return Ok(());
            }
            // published with the old key, which still works
            let mut e = engine(paths, &config)?;
            let seq = e.change_bucket_key(&key)?;
            config = trial;
            config.bucket_key_seq = seq;
            save(paths, &config)?;
            println!(
                "key change {seq} published, sealed to this account's computers. Each switches on its \
                 next sync; `omacloud bucket status` shows which have. Keep the old key until all have, \
                 then delete it at your provider."
            );
        }
    }
    Ok(())
}
