//! logind over the system bus, by hand: one `AF_UNIX` stream in drv-seatd's own poll loop,
//! `EXTERNAL` auth, the three calls it makes (`Hello`, `AddMatch`, `Inhibit`) and the one
//! signal it hears (`PrepareForSleep`). Little-endian messages only, as the bus and logind
//! send on this hardware. No zbus: that brings an executor thread and a dependency tree into
//! a daemon sealed with a syscall allowlist, for four messages.

use std::collections::VecDeque;
use std::io::{self, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};

pub const SOCKET: &str = "/run/dbus/system_bus_socket";
const LOGIN1: &str = "org.freedesktop.login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";

const METHOD_CALL: u8 = 1;
const METHOD_RETURN: u8 = 2;
const ERROR: u8 = 3;
const SIGNAL: u8 = 4;

pub struct Login1 {
    sock: UnixStream,
    buf: Vec<u8>,
    fds: VecDeque<OwnedFd>,
    serial: u32,
    /// `PrepareForSleep` arguments heard and not yet taken.
    sleep: VecDeque<bool>,
}

/// What a parsed message says, beyond its body.
#[derive(Default, Debug)]
struct Header {
    kind: u8,
    reply_serial: Option<u32>,
    interface: Option<String>,
    member: Option<String>,
    error: Option<String>,
    signature: Option<String>,
    unix_fds: u32,
}

impl Login1 {
    /// Connects, authenticates as this uid, says `Hello` and subscribes to `PrepareForSleep`.
    pub fn connect() -> io::Result<Self> {
        let mut sock = UnixStream::connect(SOCKET)?;
        let uid = rustix::process::geteuid().as_raw().to_string();
        let hex: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
        sock.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())?;
        expect_line(&mut sock, "OK ")?;
        sock.write_all(b"NEGOTIATE_UNIX_FD\r\n")?;
        expect_line(&mut sock, "AGREE_UNIX_FD")?;
        sock.write_all(b"BEGIN\r\n")?;
        let mut this = Login1 {
            sock,
            buf: Vec::new(),
            fds: VecDeque::new(),
            serial: 0,
            sleep: VecDeque::new(),
        };
        this.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
            &[],
        )?;
        let rule = format!(
            "type='signal',sender='{LOGIN1}',interface='{MANAGER}',member='PrepareForSleep',path='{LOGIN1_PATH}'"
        );
        this.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            &[&rule],
        )?;
        Ok(this)
    }

    /// A delay inhibitor on sleep: logind announces `PrepareForSleep(true)` and waits for the
    /// returned fd to close (or `InhibitDelayMaxSec`) before sleeping. One per sleep.
    pub fn inhibit(&mut self) -> io::Result<OwnedFd> {
        let (header, body, mut fds) = self.call(
            LOGIN1,
            LOGIN1_PATH,
            MANAGER,
            "Inhibit",
            &["sleep", "drv-seatd", "locking the session", "delay"],
        )?;
        if header.signature.as_deref() != Some("h") || body.len() < 4 {
            return Err(invalid("Inhibit returned no fd"));
        }
        let index = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
        if index >= fds.len() {
            return Err(invalid("Inhibit's fd is missing"));
        }
        Ok(fds.swap_remove(index))
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.sock.as_fd()
    }

    /// Reads what the bus has (call when its fd is readable) and queues the sleep signals.
    pub fn pump(&mut self) -> io::Result<()> {
        self.fill()?;
        while let Some((header, _body, _fds)) = self.next_message()? {
            self.note_signal(&header, &_body);
        }
        Ok(())
    }

    /// The `PrepareForSleep` arguments heard so far, oldest first.
    pub fn sleep_events(&mut self) -> Vec<bool> {
        self.sleep.drain(..).collect()
    }

    fn note_signal(&mut self, header: &Header, body: &[u8]) {
        if header.kind == SIGNAL
            && header.interface.as_deref() == Some(MANAGER)
            && header.member.as_deref() == Some("PrepareForSleep")
            && body.len() >= 4
        {
            self.sleep.push_back(u32::from_le_bytes(body[..4].try_into().unwrap()) != 0);
        }
    }

    /// A method call with string arguments, waiting for its reply; signals that arrive
    /// meanwhile are queued.
    fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        args: &[&str],
    ) -> io::Result<(Header, Vec<u8>, Vec<OwnedFd>)> {
        self.serial += 1;
        let serial = self.serial;
        let msg = method_call(serial, destination, path, interface, member, args);
        self.sock.write_all(&msg)?;
        loop {
            if let Some((header, body, fds)) = self.next_message()? {
                if header.reply_serial == Some(serial) {
                    return match header.kind {
                        METHOD_RETURN => Ok((header, body, fds)),
                        ERROR => Err(io::Error::other(format!(
                            "{member}: {}: {}",
                            header.error.as_deref().unwrap_or("error"),
                            body_string(&body).unwrap_or_default()
                        ))),
                        other => Err(invalid(&format!("reply of type {other}"))),
                    };
                }
                self.note_signal(&header, &body);
                continue;
            }
            self.fill()?;
        }
    }

    /// One `recvmsg` into the buffer, fds included.
    fn fill(&mut self) -> io::Result<()> {
        let mut chunk = [0u8; 4096];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let msg = rustix::net::recvmsg(
            &self.sock,
            &mut [IoSliceMut::new(&mut chunk)],
            &mut control,
            RecvFlags::CMSG_CLOEXEC,
        )?;
        for m in control.drain() {
            if let RecvAncillaryMessage::ScmRights(received) = m {
                self.fds.extend(received);
            }
        }
        if msg.bytes == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the bus hung up"));
        }
        self.buf.extend_from_slice(&chunk[..msg.bytes]);
        Ok(())
    }

    /// The next complete message in the buffer, with the fds that came with it.
    fn next_message(&mut self) -> io::Result<Option<(Header, Vec<u8>, Vec<OwnedFd>)>> {
        let Some((header, body, len)) = parse(&self.buf)? else {
            return Ok(None);
        };
        self.buf.drain(..len);
        let n = header.unix_fds as usize;
        if self.fds.len() < n {
            return Err(invalid("fewer fds than the message says"));
        }
        let fds = self.fds.drain(..n).collect();
        Ok(Some((header, body, fds)))
    }
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("bus: {what}"))
}

/// Reads one SASL line and checks its start.
fn expect_line(sock: &mut UnixStream, start: &str) -> io::Result<()> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        if sock.read(&mut byte)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "bus auth"));
        }
        line.push(byte[0]);
        if line.len() > 4096 {
            return Err(invalid("auth line too long"));
        }
    }
    if !line.starts_with(start.as_bytes()) {
        return Err(invalid(&format!(
            "auth: expected {start:?}, got {:?}",
            String::from_utf8_lossy(&line).trim_end()
        )));
    }
    Ok(())
}

fn pad(out: &mut Vec<u8>, align: usize) {
    while out.len() % align != 0 {
        out.push(0);
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    pad(out, 4);
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// A header field holding a string-like variant (`s`, `o` or `g`).
fn put_field(out: &mut Vec<u8>, code: u8, sig: u8, value: &str) {
    pad(out, 8);
    out.push(code);
    out.extend_from_slice(&[1, sig, 0]);
    if sig == b'g' {
        out.push(value.len() as u8);
        out.extend_from_slice(value.as_bytes());
        out.push(0);
    } else {
        put_str(out, value);
    }
}

/// A method call whose arguments are all strings.
fn method_call(
    serial: u32,
    destination: &str,
    path: &str,
    interface: &str,
    member: &str,
    args: &[&str],
) -> Vec<u8> {
    let mut body = Vec::new();
    for arg in args {
        put_str(&mut body, arg);
    }
    let mut fields = Vec::new();
    put_field(&mut fields, 1, b'o', path);
    put_field(&mut fields, 2, b's', interface);
    put_field(&mut fields, 3, b's', member);
    put_field(&mut fields, 6, b's', destination);
    if !args.is_empty() {
        put_field(&mut fields, 8, b'g', &"s".repeat(args.len()));
    }
    let mut out = vec![b'l', METHOD_CALL, 0, 1];
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    out.extend_from_slice(&fields);
    pad(&mut out, 8);
    out.extend_from_slice(&body);
    out
}

fn u32_at(buf: &[u8], at: usize) -> io::Result<u32> {
    buf.get(at..at + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| invalid("short message"))
}

/// A string (`s`/`o`) at `at`; returns it and the offset after its NUL.
fn str_at(buf: &[u8], at: usize) -> io::Result<(String, usize)> {
    let len = u32_at(buf, at)? as usize;
    let bytes = buf
        .get(at + 4..at + 4 + len)
        .ok_or_else(|| invalid("short string"))?;
    let s = String::from_utf8(bytes.to_vec()).map_err(|_| invalid("string not UTF-8"))?;
    Ok((s, at + 4 + len + 1))
}

/// Parses the first message in `buf` if it is all there: header, body, bytes consumed.
fn parse(buf: &[u8]) -> io::Result<Option<(Header, Vec<u8>, usize)>> {
    if buf.len() < 16 {
        return Ok(None);
    }
    if buf[0] != b'l' {
        return Err(invalid("not little-endian"));
    }
    let body_len = u32_at(buf, 4)? as usize;
    let fields_len = u32_at(buf, 12)? as usize;
    let body_start = (16 + fields_len).div_ceil(8) * 8;
    let total = body_start + body_len;
    if buf.len() < total {
        return Ok(None);
    }
    let mut header = Header {
        kind: buf[1],
        ..Header::default()
    };
    let mut at = 16;
    let end = 16 + fields_len;
    while at < end {
        at = at.div_ceil(8) * 8;
        if at >= end {
            break;
        }
        let code = buf[at];
        // The variant's signature: a one-character type for every field there is.
        let sig = *buf.get(at + 2).ok_or_else(|| invalid("short field"))?;
        at += 4;
        match sig {
            b's' | b'o' => {
                at = at.div_ceil(4) * 4;
                let (s, next) = str_at(buf, at)?;
                match code {
                    2 => header.interface = Some(s),
                    3 => header.member = Some(s),
                    4 => header.error = Some(s),
                    _ => (),
                }
                at = next;
            }
            b'g' => {
                let len = buf[at] as usize;
                let bytes = buf.get(at + 1..at + 1 + len).ok_or_else(|| invalid("short signature"))?;
                if code == 8 {
                    header.signature = Some(String::from_utf8_lossy(bytes).into_owned());
                }
                at += 1 + len + 1;
            }
            b'u' => {
                at = at.div_ceil(4) * 4;
                let v = u32_at(buf, at)?;
                match code {
                    5 => header.reply_serial = Some(v),
                    9 => header.unix_fds = v,
                    _ => (),
                }
                at += 4;
            }
            other => return Err(invalid(&format!("header field of type {other:?}"))),
        }
    }
    Ok(Some((header, buf[body_start..total].to_vec(), total)))
}

/// An error's message: its body's first string.
fn body_string(body: &[u8]) -> Option<String> {
    str_at(body, 0).ok().map(|(s, _)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_parses_back() {
        let msg = method_call(7, LOGIN1, LOGIN1_PATH, MANAGER, "Inhibit", &["sleep", "drv-seatd", "locking the session", "delay"]);
        let (header, body, len) = parse(&msg).unwrap().unwrap();
        assert_eq!(len, msg.len());
        assert_eq!(header.kind, METHOD_CALL);
        assert_eq!(u32_at(&msg, 8).unwrap(), 7);
        assert_eq!(header.interface.as_deref(), Some(MANAGER));
        assert_eq!(header.member.as_deref(), Some("Inhibit"));
        assert_eq!(header.signature.as_deref(), Some("ssss"));
        let (first, next) = str_at(&body, 0).unwrap();
        assert_eq!(first, "sleep");
        let (second, _) = str_at(&body, next.div_ceil(4) * 4).unwrap();
        assert_eq!(second, "drv-seatd");
        // Half a message is not a message yet.
        assert!(parse(&msg[..msg.len() - 1]).unwrap().is_none());
    }

    #[test]
    fn a_signal_and_a_reply_with_an_fd() {
        // PrepareForSleep(true) as logind sends it: fields PATH, INTERFACE, MEMBER, SIGNATURE,
        // SENDER; body one boolean.
        let mut fields = Vec::new();
        put_field(&mut fields, 1, b'o', LOGIN1_PATH);
        put_field(&mut fields, 2, b's', MANAGER);
        put_field(&mut fields, 3, b's', "PrepareForSleep");
        put_field(&mut fields, 8, b'g', "b");
        put_field(&mut fields, 7, b's', ":1.3");
        let mut msg = vec![b'l', SIGNAL, 1, 1];
        msg.extend_from_slice(&4u32.to_le_bytes());
        msg.extend_from_slice(&99u32.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad(&mut msg, 8);
        msg.extend_from_slice(&1u32.to_le_bytes());
        // Followed in the same read by a METHOD_RETURN carrying one fd (REPLY_SERIAL 7,
        // UNIX_FDS 1, body the fd index 0).
        let mut fields = Vec::new();
        pad(&mut fields, 8);
        fields.extend_from_slice(&[5, 1, b'u', 0]);
        fields.extend_from_slice(&7u32.to_le_bytes());
        pad(&mut fields, 8);
        fields.extend_from_slice(&[9, 1, b'u', 0]);
        fields.extend_from_slice(&1u32.to_le_bytes());
        put_field(&mut fields, 8, b'g', "h");
        let mut reply = vec![b'l', METHOD_RETURN, 1, 1];
        reply.extend_from_slice(&4u32.to_le_bytes());
        reply.extend_from_slice(&5u32.to_le_bytes());
        reply.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        reply.extend_from_slice(&fields);
        pad(&mut reply, 8);
        reply.extend_from_slice(&0u32.to_le_bytes());
        let both = [msg.clone(), reply.clone()].concat();

        let (header, body, len) = parse(&both).unwrap().unwrap();
        assert_eq!(len, msg.len());
        assert_eq!(header.kind, SIGNAL);
        assert_eq!(header.member.as_deref(), Some("PrepareForSleep"));
        assert_eq!(body, 1u32.to_le_bytes());
        let (header, body, len) = parse(&both[msg.len()..]).unwrap().unwrap();
        assert_eq!(len, reply.len());
        assert_eq!(header.reply_serial, Some(7));
        assert_eq!(header.unix_fds, 1);
        assert_eq!(header.signature.as_deref(), Some("h"));
        assert_eq!(body, 0u32.to_le_bytes());
    }
}
