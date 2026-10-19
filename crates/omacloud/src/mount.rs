//! `omacloud mount`: the account's files as a read-only folder, each
//! fetched when it's read. Folders skipped on this computer show too, so
//! a small disk can keep a big library online only.

use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use log::warn;
use omacloud_core::browse::{Browse, Entry, Kind, Opened};

/// How long the kernel may keep what it was told. The view changes only
/// when it's refreshed, which is rare next to this.
const TTL: Duration = Duration::from_secs(10);

pub struct View {
    browse: Arc<Mutex<Arc<Browse>>>,
    inodes: Mutex<Inodes>,
    open: Mutex<HashMap<u64, Arc<Opened>>>,
    next_fh: AtomicU64,
    uid: u32,
    gid: u32,
}

/// Inode numbers by path, for as long as the view is mounted: a path keeps
/// its number across refreshes.
#[derive(Default)]
struct Inodes {
    paths: Vec<PathBuf>,
    numbers: HashMap<PathBuf, u64>,
}

impl Inodes {
    fn number(&mut self, path: &Path) -> u64 {
        if let Some(n) = self.numbers.get(path) {
            return *n;
        }
        self.paths.push(path.to_path_buf());
        let n = self.paths.len() as u64; // the top, "", is 1
        _ = self.numbers.insert(path.to_path_buf(), n);
        n
    }
}

impl View {
    pub fn new(browse: Browse) -> Self {
        let mut inodes = Inodes::default();
        inodes.number(Path::new(""));
        // SAFETY: getuid and getgid can't fail
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            browse: Arc::new(Mutex::new(Arc::new(browse))),
            inodes: Mutex::new(inodes),
            open: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            uid,
            gid,
        }
    }

    fn browse(&self) -> Arc<Browse> {
        self.browse.lock().unwrap().clone()
    }

    fn path(&self, ino: INodeNo) -> Option<PathBuf> {
        let i = usize::try_from(u64::from(ino)).ok()?.checked_sub(1)?;
        self.inodes.lock().unwrap().paths.get(i).cloned()
    }

    fn attr(&self, ino: u64, e: &Entry) -> FileAttr {
        let mtime = e
            .mtime
            .and_then(|t| u64::try_from(t.as_second()).ok())
            .map_or(UNIX_EPOCH, |s| UNIX_EPOCH + Duration::from_secs(s));
        let perm = match e.kind {
            Kind::Dir => 0o555,
            Kind::Symlink => 0o777,
            // read-only, whatever the file's mode, keeping execute bits
            Kind::File => e.mode.map_or(0o444, |m| (m & 0o555) | 0o444),
        };
        let kind = file_type(e.kind);
        FileAttr {
            ino: INodeNo(ino),
            size: e.size,
            blocks: e.size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            #[allow(clippy::cast_possible_truncation)]
            perm: perm as u16,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

fn file_type(kind: Kind) -> FileType {
    match kind {
        Kind::Dir => FileType::Directory,
        Kind::File => FileType::RegularFile,
        Kind::Symlink => FileType::Symlink,
    }
}

fn failed(what: &str, e: &anyhow::Error) -> Errno {
    warn!("{what}: {e:#}");
    Errno::EIO
}

impl Filesystem for View {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(dir) = self.path(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let path = dir.join(name);
        match self.browse().entry(&path) {
            Ok(Some(e)) => {
                let ino = self.inodes.lock().unwrap().number(&path);
                reply.entry(&TTL, &self.attr(ino, &e), Generation(0));
            }
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(failed("lookup", &e)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.browse().entry(&path) {
            Ok(Some(e)) => reply.attr(&TTL, &self.attr(ino.into(), &e)),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(failed("getattr", &e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.browse().link(&path) {
            Ok(target) => reply.data(target.as_os_str().as_encoded_bytes()),
            Err(_) => reply.error(Errno::EINVAL),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.browse().open(&path) {
            Ok(f) => {
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                _ = self.open.lock().unwrap().insert(fh, Arc::new(f));
                // the contents never change under a handle: the kernel may cache them
                reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
            }
            Err(e) => reply.error(failed("open", &e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(f) = self.open.lock().unwrap().get(&fh.0).cloned() else {
            return reply.error(Errno::EBADF);
        };
        match self.browse().read(&f, offset, size as usize) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(failed("read", &e)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        _ = self.open.lock().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let entries = match self.browse().list(&path) {
            Ok(e) => e,
            Err(e) => return reply.error(failed("readdir", &e)),
        };
        let parent = path
            .parent()
            .map_or(INodeNo::ROOT, |p| INodeNo(self.inodes.lock().unwrap().number(p)));
        let mut all = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent, FileType::Directory, "..".to_string()),
        ];
        let mut inodes = self.inodes.lock().unwrap();
        for e in entries {
            all.push((INodeNo(inodes.number(&path.join(&e.name))), file_type(e.kind), e.name));
        }
        drop(inodes);
        for (i, (ino, kind, name)) in all
            .into_iter()
            .enumerate()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        {
            if reply.add(ino, i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

/// Mount `view` at `dir` until a signal or an outside unmount. Every
/// `every`, `refresh` may give a newer head to show; files already open
/// keep reading the one they were opened on.
///
/// # Errors
///
/// If it can't be mounted.
pub fn run(view: View, dir: &Path, every: Duration, refresh: &mut dyn FnMut() -> Option<Browse>) -> anyhow::Result<()> {
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::FSName("omacloud".into()),
        fuser::MountOption::Subtype("omacloud".into()),
        fuser::MountOption::DefaultPermissions,
    ];
    let current = view.browse.clone();
    let session = fuser::spawn_mount(view, dir, &config)?;
    let mut last = SystemTime::now();
    loop {
        if session.guard.is_finished() {
            // unmounted from outside (`fusermount3 -u`)
            return Ok(session.join()?);
        }
        if signalled(Duration::from_secs(1)) {
            return Ok(session.umount_and_join()?);
        }
        if last.elapsed().unwrap_or_default() >= every {
            if let Some(b) = refresh() {
                *current.lock().unwrap() = Arc::new(b);
            }
            last = SystemTime::now();
        }
    }
}

/// Block SIGINT and SIGTERM in this thread and every thread started after,
/// so [`signalled`] receives them. Call before starting any thread.
pub fn block_signals() {
    // SAFETY: plain signal mask calls on a set built here
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, libc::SIGINT);
        libc::sigaddset(&raw mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut());
    }
}

/// Whether SIGINT or SIGTERM came within `wait`.
fn signalled(wait: Duration) -> bool {
    // SAFETY: as in block_signals; sigtimedwait only reads the set and timeout
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, libc::SIGINT);
        libc::sigaddset(&raw mut set, libc::SIGTERM);
        let timeout = libc::timespec {
            tv_sec: i64::try_from(wait.as_secs()).unwrap_or(1),
            tv_nsec: 0,
        };
        libc::sigtimedwait(&raw const set, std::ptr::null_mut(), &raw const timeout) > 0
    }
}
