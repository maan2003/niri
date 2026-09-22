//! The documents mount: `<docs>/<id>/<name>` is one file the person picked for one app.
//! The kernel tells us the caller's UID on every request; a grant belongs to one UID and
//! nobody else sees it, not even in the listing. The file stays where it is, open in our
//! hands; the app only ever gets reads and writes through us.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

/// One file the person handed to one app.
pub struct Grant {
    pub uid: u32,
    pub name: String,
    pub file: File,
    pub write: bool,
}

#[derive(Default)]
pub struct Grants {
    next: u64,
    by_id: HashMap<u64, Grant>,
}

impl Grants {
    /// Files the grant; the id is the directory the app finds it in.
    pub fn add(&mut self, grant: Grant) -> u64 {
        self.next += 1;
        self.by_id.insert(self.next, grant);
        self.next
    }
}

pub type Shared = Arc<Mutex<Grants>>;

pub struct Docs {
    grants: Shared,
    /// Shown as the root directory's owner.
    uid: u32,
}

const TTL: Duration = Duration::from_secs(1);

// Inodes: 1 is the root, `2 * id` a grant's directory, `2 * id + 1` its file.
fn dir_ino(id: u64) -> INodeNo {
    INodeNo(id * 2)
}

fn file_ino(id: u64) -> INodeNo {
    INodeNo(id * 2 + 1)
}

/// `(grant id, is the file)` for anything below the root.
fn split(ino: INodeNo) -> Option<(u64, bool)> {
    (ino.0 >= 2).then(|| (ino.0 / 2, ino.0 % 2 == 1))
}

fn attr(ino: INodeNo, kind: FileType, perm: u16, uid: u32, size: u64, mtime: SystemTime) -> FileAttr {
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind,
        perm,
        nlink: if kind == FileType::Directory { 2 } else { 1 },
        uid,
        gid: uid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn read_at_most(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read_at(&mut buf[n..], offset + n as u64) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

impl Docs {
    pub fn new(grants: Shared, uid: u32) -> Self {
        Self { grants, uid }
    }

    /// The grant `id`, if it is the caller's. A stranger gets `ENOENT`, never `EACCES`: the
    /// listing does not admit the file exists.
    fn with_grant<R>(&self, req: &Request, id: u64, f: impl FnOnce(&Grant) -> R) -> Result<R, Errno> {
        let grants = self.grants.lock().unwrap();
        match grants.by_id.get(&id) {
            Some(g) if g.uid == req.uid() => Ok(f(g)),
            _ => Err(Errno::ENOENT),
        }
    }

    fn dir_attr(&self, req: &Request, id: u64) -> Result<FileAttr, Errno> {
        self.with_grant(req, id, |g| {
            attr(dir_ino(id), FileType::Directory, 0o500, g.uid, 0, UNIX_EPOCH)
        })
    }

    fn file_attr(&self, req: &Request, id: u64) -> Result<FileAttr, Errno> {
        self.with_grant(req, id, |g| {
            let (size, mtime) = g
                .file
                .metadata()
                .map(|m| (m.size(), m.modified().unwrap_or(UNIX_EPOCH)))
                .unwrap_or((0, UNIX_EPOCH));
            let perm = if g.write { 0o600 } else { 0o400 };
            attr(file_ino(id), FileType::RegularFile, perm, g.uid, size, mtime)
        })
    }

    fn attr_of(&self, req: &Request, ino: INodeNo) -> Result<FileAttr, Errno> {
        match split(ino) {
            None if ino == INodeNo::ROOT => {
                Ok(attr(INodeNo::ROOT, FileType::Directory, 0o555, self.uid, 0, UNIX_EPOCH))
            }
            None => Err(Errno::ENOENT),
            Some((id, false)) => self.dir_attr(req, id),
            Some((id, true)) => self.file_attr(req, id),
        }
    }

    /// Runs `f` on the caller's grant behind a file inode, for reads and writes.
    fn on_file<R>(
        &self,
        req: &Request,
        ino: INodeNo,
        f: impl FnOnce(&Grant) -> Result<R, Errno>,
    ) -> Result<R, Errno> {
        match split(ino) {
            Some((id, true)) => self.with_grant(req, id, f)?,
            _ => Err(Errno::EISDIR),
        }
    }
}

impl Filesystem for Docs {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = if parent == INodeNo::ROOT {
            name.to_str()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or(Errno::ENOENT)
                .and_then(|id| self.dir_attr(req, id))
        } else {
            match split(parent) {
                Some((id, false)) => self
                    .with_grant(req, id, |g| OsStr::new(&g.name) == name)
                    .and_then(|found| if found { self.file_attr(req, id) } else { Err(Errno::ENOENT) }),
                _ => Err(Errno::ENOENT),
            }
        };
        match result {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr_of(req, ino) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Truncation is the one change an app makes here (O_TRUNC, ftruncate). Ownership and
        // mode are ours to report and theirs to wish for.
        if let Some(size) = size {
            let done = self.on_file(req, ino, |g| {
                if !g.write {
                    return Err(Errno::EACCES);
                }
                g.file.set_len(size).map_err(|_| Errno::EIO)
            });
            if let Err(e) = done {
                return reply.error(e);
            }
        }
        match self.attr_of(req, ino) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(&self, req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let dot = |ino| (ino, FileType::Directory, ".".to_owned());
        let dotdot = (INodeNo::ROOT, FileType::Directory, "..".to_owned());
        let entries: Vec<(INodeNo, FileType, String)> = if ino == INodeNo::ROOT {
            let grants = self.grants.lock().unwrap();
            let mut ids: Vec<u64> = grants
                .by_id
                .iter()
                .filter(|(_, g)| g.uid == req.uid())
                .map(|(id, _)| *id)
                .collect();
            ids.sort_unstable();
            let mut v = vec![dot(INodeNo::ROOT), dotdot];
            v.extend(ids.into_iter().map(|id| (dir_ino(id), FileType::Directory, id.to_string())));
            v
        } else {
            match split(ino) {
                Some((id, false)) => match self.with_grant(req, id, |g| g.name.clone()) {
                    Ok(name) => vec![dot(dir_ino(id)), dotdot, (file_ino(id), FileType::RegularFile, name)],
                    Err(e) => return reply.error(e),
                },
                _ => return reply.error(Errno::ENOTDIR),
            }
        };
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*ino, i as u64 + 1, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let wants_write = !matches!(flags.acc_mode(), OpenAccMode::O_RDONLY);
        match self.on_file(req, ino, |g| if g.write || !wants_write { Ok(()) } else { Err(Errno::EACCES) }) {
            Ok(()) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let mut buf = vec![0u8; size as usize];
        match self.on_file(req, ino, |g| read_at_most(&g.file, &mut buf, offset).map_err(|_| Errno::EIO)) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => reply.error(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let done = self.on_file(req, ino, |g| {
            if !g.write {
                return Err(Errno::EACCES);
            }
            g.file.write_all_at(data, offset).map_err(|_| Errno::EIO)
        });
        match done {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(e),
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn fsync(&self, req: &Request, ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        match self.on_file(req, ino, |g| g.file.sync_all().map_err(|_| Errno::EIO)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
}
