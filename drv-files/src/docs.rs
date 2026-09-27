//! The documents mount: `<docs>/<id>/<name>` is one file the person picked for one app.
//! The kernel tells us the caller's UID on every request; a grant belongs to one UID and
//! nobody else sees it, not even in the listing. The file stays where it is, open in our
//! hands; the app only ever gets reads and writes through us.
//!
//! Next to a writable document the app may make files of its own (a download's
//! `.crdownload`, an editor's temporary copy): scratch, unnamed files in the document's
//! directory on disk, gone with the grant. Renaming one onto the document makes its
//! contents the document's, in place: the file on disk keeps its inode and its name (the
//! kernel then knows the document by the scratch file's inode too, as renames go).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
    WriteFlags,
};

/// One file the person handed to one app.
pub struct Grant {
    pub uid: u32,
    pub name: String,
    pub file: File,
    pub write: bool,
    /// The file's directory on disk: where its scratch files live.
    pub dir: File,
}

enum Scratch {
    /// A file the app made next to its document.
    File { grant: u64, name: String, file: File },
    /// Was renamed onto the document: the kernel knows the document by this inode now,
    /// until it forgets it.
    Onto { grant: u64 },
}

#[derive(Default)]
pub struct Grants {
    next: u64,
    by_id: HashMap<u64, Grant>,
    /// By inode.
    scratch: HashMap<u64, Scratch>,
    next_scratch: u64,
}

impl Grants {
    /// Files the grant; the id is the directory the app finds it in.
    pub fn add(&mut self, grant: Grant) -> u64 {
        self.next += 1;
        self.by_id.insert(self.next, grant);
        self.next
    }

}

/// The inode of grant `grant`'s scratch file called `name`.
fn scratch_named(scratch: &HashMap<u64, Scratch>, grant: u64, name: &OsStr) -> Option<u64> {
    scratch
        .iter()
        .find(|(_, s)| matches!(s, Scratch::File { grant: g, name: n, .. } if *g == grant && OsStr::new(n) == name))
        .map(|(ino, _)| *ino)
}

pub type Shared = Arc<Mutex<Grants>>;

pub struct Docs {
    grants: Shared,
    /// Shown as the root directory's owner.
    uid: u32,
}

const TTL: Duration = Duration::from_secs(1);

// Inodes: 1 is the root, `2 * id` a grant's directory, `2 * id + 1` its file, and from
// `SCRATCH` up the scratch files.
const SCRATCH: u64 = 1 << 40;

fn dir_ino(id: u64) -> INodeNo {
    INodeNo(id * 2)
}

fn file_ino(id: u64) -> INodeNo {
    INodeNo(id * 2 + 1)
}

enum Node {
    Root,
    Dir(u64),
    Doc(u64),
    Scratch(u64),
}

fn node(ino: INodeNo) -> Option<Node> {
    match ino.0 {
        1 => Some(Node::Root),
        n if n >= SCRATCH => Some(Node::Scratch(n)),
        n if n >= 2 && n % 2 == 0 => Some(Node::Dir(n / 2)),
        n if n >= 2 => Some(Node::Doc(n / 2)),
        _ => None,
    }
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

fn file_attr_of(ino: INodeNo, file: &File, write: bool, uid: u32) -> FileAttr {
    let (size, mtime) = file
        .metadata()
        .map(|m| (m.size(), m.modified().unwrap_or(UNIX_EPOCH)))
        .unwrap_or((0, UNIX_EPOCH));
    attr(ino, FileType::RegularFile, if write { 0o600 } else { 0o400 }, uid, size, mtime)
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

/// `to` becomes a copy of `from`.
fn copy_into(from: &File, to: &File) -> io::Result<()> {
    to.set_len(0)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut offset = 0;
    loop {
        let n = read_at_most(from, &mut buf, offset)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all_at(&buf[..n], offset)?;
        offset += n as u64;
    }
}

fn plain_name(name: &OsStr) -> Option<&str> {
    let s = name.to_str()?;
    (!s.is_empty() && s != "." && s != ".." && !s.contains('/')).then_some(s)
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

    /// The caller's writable grant `id` and the scratch files, for making and moving them.
    fn writable<R>(
        &self,
        req: &Request,
        id: u64,
        f: impl FnOnce(&Grant, &mut HashMap<u64, Scratch>, &mut u64) -> Result<R, Errno>,
    ) -> Result<R, Errno> {
        let mut grants = self.grants.lock().unwrap();
        let Grants { by_id, scratch, next_scratch, .. } = &mut *grants;
        match by_id.get(&id) {
            Some(g) if g.uid != req.uid() => Err(Errno::ENOENT),
            Some(g) if !g.write => Err(Errno::EACCES),
            Some(g) => f(g, scratch, next_scratch),
            None => Err(Errno::ENOENT),
        }
    }

    fn attr_of(&self, req: &Request, ino: INodeNo) -> Result<FileAttr, Errno> {
        match node(ino) {
            Some(Node::Root) => Ok(attr(INodeNo::ROOT, FileType::Directory, 0o555, self.uid, 0, UNIX_EPOCH)),
            Some(Node::Dir(id)) => self.with_grant(req, id, |g| {
                attr(dir_ino(id), FileType::Directory, if g.write { 0o700 } else { 0o500 }, g.uid, 0, UNIX_EPOCH)
            }),
            Some(Node::Doc(id)) => self.with_grant(req, id, |g| file_attr_of(file_ino(id), &g.file, g.write, g.uid)),
            Some(Node::Scratch(_)) => self.on_file(req, ino, |file, write, uid| Ok(file_attr_of(ino, file, write, uid))),
            None => Err(Errno::ENOENT),
        }
    }

    /// Runs `f` on the file behind `ino` (a document or a scratch file of the caller's):
    /// the file, whether it may be written, its owner.
    fn on_file<R>(
        &self,
        req: &Request,
        ino: INodeNo,
        f: impl FnOnce(&File, bool, u32) -> Result<R, Errno>,
    ) -> Result<R, Errno> {
        match node(ino) {
            Some(Node::Doc(id)) => self.with_grant(req, id, |g| f(&g.file, g.write, g.uid))?,
            Some(Node::Scratch(n)) => {
                let grants = self.grants.lock().unwrap();
                let (grant, file) = match grants.scratch.get(&n).ok_or(Errno::ENOENT)? {
                    Scratch::File { grant, file, .. } => (*grant, Some(file)),
                    Scratch::Onto { grant } => (*grant, None),
                };
                match grants.by_id.get(&grant) {
                    Some(g) if g.uid == req.uid() => match file {
                        Some(file) => f(file, true, g.uid),
                        None => f(&g.file, g.write, g.uid),
                    },
                    _ => Err(Errno::ENOENT),
                }
            }
            Some(Node::Root | Node::Dir(_)) => Err(Errno::EISDIR),
            None => Err(Errno::ENOENT),
        }
    }
}

impl Filesystem for Docs {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = match node(parent) {
            Some(Node::Root) => name
                .to_str()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or(Errno::ENOENT)
                .and_then(|id| self.attr_of(req, dir_ino(id))),
            Some(Node::Dir(id)) => {
                let ino = self.with_grant(req, id, |g| OsStr::new(&g.name) == name).and_then(|is_doc| {
                    if is_doc {
                        return Ok(file_ino(id));
                    }
                    scratch_named(&self.grants.lock().unwrap().scratch, id, name).map(INodeNo).ok_or(Errno::ENOENT)
                });
                ino.and_then(|ino| self.attr_of(req, ino))
            }
            _ => Err(Errno::ENOENT),
        };
        match result {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, _nlookup: u64) {
        if let Some(Node::Scratch(n)) = node(ino) {
            let mut grants = self.grants.lock().unwrap();
            if matches!(grants.scratch.get(&n), Some(Scratch::Onto { .. })) {
                grants.scratch.remove(&n);
            }
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
        // Truncation is the one change an app makes here (O_TRUNC, ftruncate). Ownership,
        // mode and times are ours to report and theirs to wish for.
        if let Some(size) = size {
            let done = self.on_file(req, ino, |file, write, _| {
                if !write {
                    return Err(Errno::EACCES);
                }
                file.set_len(size).map_err(|_| Errno::EIO)
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
        let entries: Vec<(INodeNo, FileType, String)> = match node(ino) {
            Some(Node::Root) => {
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
            }
            Some(Node::Dir(id)) => match self.with_grant(req, id, |g| g.name.clone()) {
                Ok(name) => {
                    let mut v = vec![dot(dir_ino(id)), dotdot, (file_ino(id), FileType::RegularFile, name)];
                    let grants = self.grants.lock().unwrap();
                    let mut scratch: Vec<(u64, &str)> = grants
                        .scratch
                        .iter()
                        .filter_map(|(ino, s)| match s {
                            Scratch::File { grant, name, .. } if *grant == id => Some((*ino, name.as_str())),
                            _ => None,
                        })
                        .collect();
                    scratch.sort_unstable();
                    v.extend(scratch.into_iter().map(|(ino, name)| (INodeNo(ino), FileType::RegularFile, name.to_owned())));
                    v
                }
                Err(e) => return reply.error(e),
            },
            _ => return reply.error(Errno::ENOTDIR),
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
        match self.on_file(req, ino, |_, write, _| if write || !wants_write { Ok(()) } else { Err(Errno::EACCES) }) {
            Ok(()) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    /// A scratch file next to a writable document, under any other name.
    #[allow(clippy::too_many_arguments)]
    fn create(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let Some(Node::Dir(id)) = node(parent) else {
            return reply.error(Errno::EACCES);
        };
        let Some(plain) = plain_name(name) else {
            return reply.error(Errno::EINVAL);
        };
        let made = self.writable(req, id, |g, scratch, next| {
            if g.name == plain || scratch_named(scratch, id, name).is_some() {
                return Err(Errno::EEXIST);
            }
            let fd = rustix::fs::openat(
                &g.dir,
                ".",
                rustix::fs::OFlags::TMPFILE | rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o600),
            )
            .map_err(|_| Errno::EIO)?;
            let file = File::from(fd);
            let ino = SCRATCH + *next;
            *next += 1;
            let a = file_attr_of(INodeNo(ino), &file, true, g.uid);
            scratch.insert(ino, Scratch::File { grant: id, name: plain.to_owned(), file });
            Ok(a)
        });
        match made {
            Ok(a) => reply.created(&TTL, &a, Generation(0), FileHandle(0), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    /// Scratch files go; the document stays (it is the person's, on disk).
    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(Node::Dir(id)) = node(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let done = self.writable(req, id, |g, scratch, _| {
            if OsStr::new(&g.name) == name {
                return Err(Errno::EPERM);
            }
            let ino = scratch_named(scratch, id, name).ok_or(Errno::ENOENT)?;
            scratch.remove(&ino);
            Ok(())
        });
        match done {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    /// Within a grant's directory: a scratch file onto the document (its contents, in
    /// place) or under another scratch name. The document itself is not moved.
    #[allow(clippy::too_many_arguments)]
    fn rename(&self, req: &Request, parent: INodeNo, name: &OsStr, newparent: INodeNo, newname: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        let Some(Node::Dir(id)) = node(parent) else {
            return reply.error(Errno::ENOENT);
        };
        if newparent != parent {
            return reply.error(Errno::EXDEV);
        }
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(Errno::EINVAL);
        }
        let Some(new) = plain_name(newname) else {
            return reply.error(Errno::EINVAL);
        };
        let done = self.writable(req, id, |g, scratch, _| {
            if OsStr::new(&g.name) == name {
                return Err(Errno::EPERM);
            }
            let ino = scratch_named(scratch, id, name).ok_or(Errno::ENOENT)?;
            if g.name == new {
                let Some(Scratch::File { file, .. }) = scratch.get(&ino) else {
                    return Err(Errno::ENOENT);
                };
                copy_into(file, &g.file).map_err(|_| Errno::EIO)?;
                scratch.insert(ino, Scratch::Onto { grant: id });
                return Ok(());
            }
            if let Some(other) = scratch_named(scratch, id, newname) {
                if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                    return Err(Errno::EEXIST);
                }
                scratch.remove(&other);
            }
            if let Some(Scratch::File { name, .. }) = scratch.get_mut(&ino) {
                *name = new.to_owned();
            }
            Ok(())
        });
        match done {
            Ok(()) => reply.ok(),
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
        match self.on_file(req, ino, |file, _, _| read_at_most(file, &mut buf, offset).map_err(|_| Errno::EIO)) {
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
        let done = self.on_file(req, ino, |file, write, _| {
            if !write {
                return Err(Errno::EACCES);
            }
            file.write_all_at(data, offset).map_err(|_| Errno::EIO)
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
        match self.on_file(req, ino, |file, _, _| file.sync_all().map_err(|_| Errno::EIO)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
}
