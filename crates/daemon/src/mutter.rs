//! The clipboard on GNOME, through its compositor's remote desktop interface.
//!
//! **GNOME has no privileged selection protocol and no clipboard component on
//! the bus**, which is why the session side found nothing to own there
//! (docs/09-compatibility.md). What it does have is the interface its own
//! remote desktop server takes the clipboard through:
//! `org.gnome.Mutter.RemoteDesktop`, on the session bus. A session made there
//! and never started holds no screen and no input and shows nothing, and it can
//! still own the selection, serve it when somebody pastes, and hear when it
//! changes hands.
//!
//! **The text travels over descriptors handed across the bus**, so this
//! connection asks for them in the handshake and reads with the call that
//! carries them. The messages themselves are the ones the rest of the session
//! side writes and reads.
//!
//! **The connection is the lease here too.** The compositor ends the session
//! when the connection that made it goes, and the selection with it, which is
//! the shape a clipboard owner has to have anyway (docs/07-platforms.md 5.1).

use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::dbus;

const BUS: &str = "org.freedesktop.DBus";
const BUS_OBJECT: &str = "/org/freedesktop/DBus";

const SERVICE: &str = "org.gnome.Mutter.RemoteDesktop";
const OBJECT: &str = "/org/gnome/Mutter/RemoteDesktop";
const SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";

/// What a guest's text is offered as.
///
/// **Every name a program might paste text by**, because the one asking is
/// whatever has focus: a native program asks by the first, one running under
/// the X server by one of the others. All of them are served the same UTF-8.
pub(crate) const OFFERED: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// What the desktop's clipboard is read as, the first one it offers.
pub(crate) const WANTED: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];

/// The most text read off the desktop's clipboard.
///
/// **Under what the service's channel carries**, with room for the text to be
/// escaped on its way there; a copy past it is somebody's file, not a line to
/// paste somewhere else.
const MAX_TEXT: usize = 512 * 1024;

/// How long a call waits for its answer, and a transfer for its other end.
const CALL_MS: u64 = 5_000;
const TRANSFER_MS: u64 = 2_000;

/// How long a new session listens for who already holds the clipboard.
const SETTLE_MS: u64 = 300;

/// GNOME's clipboard, owned through a remote desktop session that is never
/// started.
#[derive(Debug)]
pub(crate) struct Clip {
    bus: Bus,
    session: String,
    /// The guest's text, while this host owns the selection.
    serving: Option<String>,
    /// What was last seen or set, so a change this host caused is not sent
    /// back to the guest that caused it.
    last: String,
    /// The compositor went away and has not been reached since.
    lost: bool,
}

impl Clip {
    /// Make the session, ask to hear about it, and take the clipboard's
    /// interface, or say why not.
    pub(crate) fn connect() -> Result<Self, String> {
        let mut bus = Bus::open()?;
        let made = bus.call(SERVICE, OBJECT, SERVICE, "CreateSession", "", &[])?;
        let session = dbus::string_at(&made.body);
        if session.is_empty() {
            return Err("no session came back".to_string());
        }
        // **Asked for by name.** The bus delivers a signal only to connections
        // that said they wanted it, and these are sent to nobody in particular.
        let mut rule = Vec::new();
        dbus::put_string(
            &mut rule,
            &format!("type='signal',sender='{SERVICE}',path='{session}'"),
        );
        bus.call(BUS, BUS_OBJECT, BUS, "AddMatch", "s", &rule)?;
        bus.call(
            SERVICE,
            &session,
            SESSION,
            "EnableClipboard",
            "a{sv}",
            &offering(&[]),
        )?;
        let mut clip = Self {
            bus,
            session,
            serving: None,
            last: String::new(),
            lost: false,
        };
        clip.settle(Instant::now() + Duration::from_millis(SETTLE_MS));
        Ok(clip)
    }

    /// Take in who holds the clipboard already, without reporting it.
    ///
    /// **The compositor says so as soon as the clipboard is enabled**, and
    /// what somebody copied before this session existed is not a change to
    /// hand the guests. Read as the starting point instead, the way the other
    /// desktop's clipboard is read when it is first reached.
    fn settle(&mut self, until: Instant) {
        while let Ok(Some(message)) = self.bus.signal(until) {
            match message.member.as_deref() {
                Some("SelectionOwnerChanged") => {
                    let _ = self.taken(&message.body);
                }
                Some("SelectionTransfer") => self.serve(&message.body),
                _ => {}
            }
        }
    }

    /// Put a guest's text on the desktop's clipboard.
    ///
    /// **Claimed, not stored.** The compositor is told what this offers, and
    /// the bytes go when somebody pastes; until then this connection is what
    /// holds them.
    pub(crate) fn write(&mut self, text: &str) -> Result<(), String> {
        self.bus.call(
            SERVICE,
            &self.session,
            SESSION,
            "SetSelection",
            "a{sv}",
            &offering(&OFFERED),
        )?;
        // **What changed hands before this claim is over.** A change kept
        // while the call waited describes the clipboard as it was, and read
        // afterwards it would hand back the claim it was overtaken by.
        self.bus
            .signals
            .retain(|kept| kept.member.as_deref() != Some("SelectionOwnerChanged"));
        self.serving = Some(text.to_string());
        self.last = text.to_string();
        Ok(())
    }

    /// Wait for the desktop's clipboard to change hands, and answer with what
    /// it changed to; serve anybody pasting a guest's text meanwhile.
    ///
    /// **Answers `None` when nothing arrived before the deadline**, which is
    /// how the caller gets a turn to do anything else.
    pub(crate) fn changed(&mut self, within: Duration) -> Option<String> {
        let until = Instant::now() + within;
        if self.lost {
            self.reconnect(until);
            return None;
        }
        loop {
            let message = match self.bus.signal(until) {
                Ok(Some(message)) => message,
                Ok(None) => return None,
                Err(error) => {
                    lowlat_common::log_warn!("session: GNOME's clipboard went away, {error}");
                    self.lost = true;
                    return None;
                }
            };
            match message.member.as_deref() {
                Some("SelectionTransfer") => self.serve(&message.body),
                Some("SelectionOwnerChanged") => {
                    if let Some(text) = self.taken(&message.body) {
                        return Some(text);
                    }
                }
                Some("Closed") => {
                    lowlat_common::log_warn!("session: GNOME closed the clipboard's session");
                    self.lost = true;
                    return None;
                }
                _ => {}
            }
        }
    }

    /// Hand a paste the guest's text.
    ///
    /// **Answered either way.** The compositor waits for this side to say the
    /// transfer is over, so one that cannot be served is said to have failed
    /// rather than left open.
    fn serve(&mut self, body: &[u8]) {
        let serial = transfer_of(body);
        let served = match self.serving.as_deref() {
            Some(text) => {
                let bytes = text.as_bytes().to_vec();
                self.bus
                    .call(
                        SERVICE,
                        &self.session,
                        SESSION,
                        "SelectionWrite",
                        "u",
                        &serial.to_le_bytes(),
                    )
                    .and_then(|reply| handed(reply, 0))
                    .and_then(|fd| {
                        fill(
                            fd,
                            &bytes,
                            Instant::now() + Duration::from_millis(TRANSFER_MS),
                        )
                    })
            }
            None => Err("nothing of ours to paste".to_string()),
        };
        if let Err(error) = &served {
            lowlat_common::log_warn!("session: a paste was not served, {error}");
        }
        let mut done = Vec::new();
        done.extend_from_slice(&serial.to_le_bytes());
        done.extend_from_slice(&u32::from(served.is_ok()).to_le_bytes());
        if let Err(error) = self.bus.call(
            SERVICE,
            &self.session,
            SESSION,
            "SelectionWriteDone",
            "ub",
            &done,
        ) {
            lowlat_common::log_warn!("session: a paste was not closed, {error}");
        }
    }

    /// Read what the desktop's clipboard became, when it is somebody else's
    /// and is text.
    fn taken(&mut self, body: &[u8]) -> Option<String> {
        let owner = owner_of(body);
        // **Our own claim is not a change.** It is the guest's text, and
        // reading it back would be asking this connection to serve itself.
        if owner.ours {
            return None;
        }
        self.serving = None;
        let wanted = WANTED
            .iter()
            .find(|wanted| owner.types.iter().any(|offered| offered == *wanted))?;
        let mut asked = Vec::new();
        dbus::put_string(&mut asked, wanted);
        let read = self
            .bus
            .call(
                SERVICE,
                &self.session,
                SESSION,
                "SelectionRead",
                "s",
                &asked,
            )
            .and_then(|reply| handed(reply, 0))
            .and_then(|fd| drain(fd, Instant::now() + Duration::from_millis(TRANSFER_MS)));
        let text = match read {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                lowlat_common::log_warn!("session: the desktop's clipboard was not read, {error}");
                return None;
            }
        };
        // **Repeats are dropped**: a copy of what a guest just sent here is
        // not news to the guests.
        if text.is_empty() || text == self.last {
            return None;
        }
        self.last.clone_from(&text);
        Some(text)
    }

    /// Make the session again, after the compositor or the bus went away.
    ///
    /// **Quiet until it works.** A compositor that is gone stays gone for a
    /// while, and the tick this is asked on would otherwise be a line a tick.
    fn reconnect(&mut self, until: Instant) {
        match Self::connect() {
            Ok(fresh) => {
                lowlat_common::log_info!("session: GNOME's clipboard is back");
                *self = fresh;
            }
            Err(_) => std::thread::sleep(until.saturating_duration_since(Instant::now())),
        }
    }
}

/// One connection to the session bus that may be handed descriptors.
///
/// **Its own reader, because the bus's framing and the descriptors arrive
/// together.** A descriptor rides on the bytes of the message that names it,
/// so the bytes are buffered until a message is whole and the descriptors are
/// queued in the order they came.
#[derive(Debug)]
struct Bus {
    stream: UnixStream,
    serial: u32,
    /// Bytes read and not yet a whole message.
    pending: Vec<u8>,
    /// Descriptors that arrived and no message has claimed yet.
    fds: VecDeque<OwnedFd>,
    /// Signals that arrived while a call waited for its answer.
    signals: VecDeque<In>,
}

/// One message in, with the descriptors it carried.
#[derive(Debug)]
struct In {
    kind: u8,
    member: Option<String>,
    reply_serial: Option<u32>,
    error_name: Option<String>,
    body: Vec<u8>,
    fds: Vec<OwnedFd>,
}

impl Bus {
    fn open() -> Result<Self, String> {
        let mut bus = Self {
            stream: dbus::open_with_fds()?,
            serial: 0,
            pending: Vec::new(),
            fds: VecDeque::new(),
            signals: VecDeque::new(),
        };
        // **Before anything else is allowed.** The bus refuses every other call
        // until it has named this connection.
        bus.call(BUS, BUS_OBJECT, BUS, "Hello", "", &[])?;
        Ok(bus)
    }

    /// One method call, and its answer.
    ///
    /// **Signals that arrive first are kept, not passed over.** A paste can be
    /// asked for while this waits on something else, and one dropped here is
    /// a paste that never finishes.
    fn call(
        &mut self,
        destination: &str,
        object: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
    ) -> Result<In, String> {
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        let message = dbus::message(
            dbus::METHOD_CALL,
            0,
            serial,
            &[
                (dbus::PATH, "o", object),
                (dbus::INTERFACE, "s", interface),
                (dbus::MEMBER, "s", member),
                (dbus::DESTINATION, "s", destination),
            ],
            None,
            signature,
            body,
        );
        self.stream
            .write_all(&message)
            .map_err(|error| format!("write: {error}"))?;
        let until = Instant::now() + Duration::from_millis(CALL_MS);
        loop {
            let Some(answer) = self.next(until)? else {
                return Err(format!("{member} was not answered"));
            };
            if answer.reply_serial == Some(serial) {
                return match answer.kind {
                    dbus::METHOD_RETURN => Ok(answer),
                    _ => Err(format!(
                        "{member} refused, {}",
                        answer.error_name.unwrap_or_default()
                    )),
                };
            }
            if answer.kind == dbus::SIGNAL {
                self.signals.push_back(answer);
            }
        }
    }

    /// The next signal: one kept while a call waited, or the next to arrive
    /// before the deadline.
    fn signal(&mut self, until: Instant) -> Result<Option<In>, String> {
        if let Some(kept) = self.signals.pop_front() {
            return Ok(Some(kept));
        }
        loop {
            match self.next(until)? {
                Some(message) if message.kind == dbus::SIGNAL => return Ok(Some(message)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    /// The next whole message off the socket, or `None` at the deadline.
    fn next(&mut self, until: Instant) -> Result<Option<In>, String> {
        loop {
            if let Some(message) = self.take()? {
                return Ok(Some(message));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some(left))
                .map_err(|error| format!("deadline: {error}"))?;
            let mut chunk = [0u8; 4096];
            match receive(&self.stream, &mut chunk, &mut self.fds) {
                Ok(0) => return Err("the bus closed".to_string()),
                Ok(read) => self
                    .pending
                    .extend_from_slice(chunk.get(..read).unwrap_or(&[])),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(format!("read: {error}")),
            }
        }
    }

    /// A whole message from what has been read, if there is one yet.
    fn take(&mut self) -> Result<Option<In>, String> {
        if self.pending.len() < dbus::HEADER {
            return Ok(None);
        }
        let body_len = dbus::le32(&self.pending, 4) as usize;
        let fields_len = dbus::le32(&self.pending, 12) as usize;
        let padded = fields_len.next_multiple_of(8);
        if body_len > dbus::MAX_MESSAGE || padded > dbus::MAX_MESSAGE {
            return Err("a message over the cap".to_string());
        }
        let whole = dbus::HEADER + padded + body_len;
        if self.pending.len() < whole {
            return Ok(None);
        }
        let message: Vec<u8> = self.pending.drain(..whole).collect();
        let fields = dbus::fields(
            message
                .get(dbus::HEADER..dbus::HEADER + fields_len)
                .unwrap_or(&[]),
        );
        let carried = fields.unix_fds.unwrap_or(0) as usize;
        let fds = (0..carried).filter_map(|_| self.fds.pop_front()).collect();
        Ok(Some(In {
            kind: message.get(1).copied().unwrap_or(0),
            member: fields.member,
            reply_serial: fields.reply_serial,
            error_name: fields.error_name,
            body: message.get(dbus::HEADER + padded..).unwrap_or(&[]).to_vec(),
            fds,
        }))
    }
}

/// The descriptor an answer names at the start of its body.
fn handed(reply: In, at: usize) -> Result<OwnedFd, String> {
    let index = dbus::le32(&reply.body, at) as usize;
    reply
        .fds
        .into_iter()
        .nth(index)
        .ok_or_else(|| "no descriptor came back".to_string())
}

/// One read off the bus, keeping any descriptors that came with it.
///
/// **A plain read would close them.** The kernel hands descriptors over only
/// to the call that asks for them and discards them for one that does not.
pub(crate) fn receive(
    stream: &UnixStream,
    into: &mut [u8],
    fds: &mut VecDeque<OwnedFd>,
) -> std::io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: into.as_mut_ptr().cast(),
        iov_len: into.len(),
    };
    // Room for a handful of descriptors, aligned for the header they follow.
    let mut control = [0u64; 16];
    // SAFETY: an all-zero message header is the empty one.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = std::mem::size_of_val(&control);
    // SAFETY: every pointer in the header is to a live local of the size it
    // is said to be.
    let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut header, libc::MSG_CMSG_CLOEXEC) };
    if read < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: walking the control data the call just wrote, by the macros that
    // bound it to the length it reported.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&header);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                let bytes = (*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize;
                for at in 0..bytes / std::mem::size_of::<RawFd>() {
                    fds.push_back(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(at))));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&header, cmsg);
        }
    }
    if header.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(std::io::Error::other("descriptors were cut off"));
    }
    Ok(usize::try_from(read).unwrap_or(0))
}

/// Read a descriptor to its end, or fail at the cap or the deadline.
pub(crate) fn drain(fd: OwnedFd, until: Instant) -> Result<Vec<u8>, String> {
    let mut file = std::fs::File::from(fd);
    nonblocking(&file)?;
    let mut got = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return Ok(got),
            Ok(read) => {
                got.extend_from_slice(chunk.get(..read).unwrap_or(&[]));
                if got.len() > MAX_TEXT {
                    return Err(format!("over {MAX_TEXT} bytes"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                ready(&file, libc::POLLIN, until)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("read: {error}")),
        }
    }
}

/// Write all of the bytes to a descriptor, or fail at the deadline.
pub(crate) fn fill(fd: OwnedFd, bytes: &[u8], until: Instant) -> Result<(), String> {
    let mut file = std::fs::File::from(fd);
    nonblocking(&file)?;
    let mut at = 0;
    while at < bytes.len() {
        match file.write(bytes.get(at..).unwrap_or(&[])) {
            Ok(0) => return Err("the paste closed".to_string()),
            Ok(written) => at += written,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                ready(&file, libc::POLLOUT, until)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("write: {error}")),
        }
    }
    Ok(())
}

/// Wait for a descriptor to be ready, until the deadline.
fn ready(file: &std::fs::File, events: i16, until: Instant) -> Result<(), String> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("the other end did not answer in time".to_string());
    }
    let mut poll = libc::pollfd {
        fd: file.as_raw_fd(),
        events,
        revents: 0,
    };
    let ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1);
    // SAFETY: one live poll entry, counted as one.
    if unsafe { libc::poll(&mut poll, 1, ms) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("poll: {error}"));
        }
    }
    Ok(())
}

fn nonblocking(file: &std::fs::File) -> Result<(), String> {
    let fd = file.as_raw_fd();
    // SAFETY: reading and setting the flags of a descriptor this owns.
    let set = unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0
    };
    if set {
        Ok(())
    } else {
        Err(format!("flags: {}", std::io::Error::last_os_error()))
    }
}

/// An `a{sv}` with one entry, the types this offers, or none at all.
///
/// **The dictionary's padding is written even when it is empty**: an array
/// is padded to its elements' alignment whether or not it has any.
fn offering(types: &[&str]) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    dbus::align(&mut body, 8);
    let entries = body.len();
    if !types.is_empty() {
        dbus::put_string(&mut body, "mime-types");
        dbus::put_signature(&mut body, "as");
        dbus::align(&mut body, 4);
        let length = body.len();
        body.extend_from_slice(&[0u8; 4]);
        let first = body.len();
        for name in types {
            dbus::put_string(&mut body, name);
        }
        set_len(&mut body, length, first);
    }
    set_len(&mut body, 0, entries);
    body
}

/// Write an array's length at `at`: the bytes from `from` to the end.
fn set_len(body: &mut [u8], at: usize, from: usize) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a dictionary of a handful of names"
    )]
    let len = ((body.len() - from) as u32).to_le_bytes();
    if let Some(slot) = body.get_mut(at..at + 4) {
        slot.copy_from_slice(&len);
    }
}

/// The serial a paste is asked for under: `SelectionTransfer(s, u)`.
fn transfer_of(body: &[u8]) -> u32 {
    let mime = dbus::le32(body, 0) as usize;
    dbus::le32(body, (4 + mime + 1).next_multiple_of(4))
}

/// Who holds the clipboard now, from `SelectionOwnerChanged(a{sv})`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Owner {
    /// This session holds it: the guest's text, claimed here.
    ours: bool,
    /// What it can be read as.
    types: Vec<String>,
}

/// The owner a change names, read from the entries this knows and stopped at
/// a value of a type it does not, rather than guessing that value's width.
fn owner_of(body: &[u8]) -> Owner {
    let mut owner = Owner::default();
    let end = 8 + dbus::le32(body, 0) as usize;
    let text = |at: usize, len: usize| {
        body.get(at..at + len)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    };
    let mut at = 8;
    while at < end.min(body.len()) {
        at = at.next_multiple_of(8);
        if at >= end {
            break;
        }
        let key_len = dbus::le32(body, at) as usize;
        let Some(key) = text(at + 4, key_len) else {
            break;
        };
        at += 4 + key_len + 1;
        let Some(&signature_len) = body.get(at) else {
            break;
        };
        let signature_len = usize::from(signature_len);
        let Some(signature) = body.get(at + 1..at + 1 + signature_len) else {
            break;
        };
        at += 1 + signature_len + 1;
        match signature {
            b"b" => {
                at = at.next_multiple_of(4);
                if key == "session-is-owner" {
                    owner.ours = dbus::le32(body, at) != 0;
                }
                at += 4;
            }
            // **The compositor sends the types inside a structure**, `(as)`,
            // though it takes them as a bare `as`; a structure is aligned to
            // eight and holds the same array.
            b"as" | b"(as)" => {
                if signature == b"(as)" {
                    at = at.next_multiple_of(8);
                }
                at = at.next_multiple_of(4);
                let stop = at + 4 + dbus::le32(body, at) as usize;
                let mut item = at + 4;
                while item < stop.min(body.len()) {
                    item = item.next_multiple_of(4);
                    let len = dbus::le32(body, item) as usize;
                    let Some(name) = text(item + 4, len) else {
                        break;
                    };
                    if key == "mime-types" {
                        owner.types.push(name);
                    }
                    item += 4 + len + 1;
                }
                at = stop;
            }
            b"s" | b"o" => {
                at = at.next_multiple_of(4);
                at += 4 + dbus::le32(body, at) as usize + 1;
            }
            b"u" | b"i" => at = at.next_multiple_of(4) + 4,
            _ => break,
        }
    }
    owner
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A change of owner as the compositor sends it -- the types in a
    /// structure, which is what GNOME 50 was seen to send -- with a value of a
    /// type this does not read placed last so it ends the walk and nothing
    /// else.
    fn changed_to(types: &[&str], ours: bool) -> Vec<u8> {
        let mut body = vec![0u8; 8];
        let entries = 8;
        dbus::put_string(&mut body, "mime-types");
        dbus::put_signature(&mut body, "(as)");
        dbus::align(&mut body, 8);
        let length = body.len();
        body.extend_from_slice(&[0u8; 4]);
        let first = body.len();
        for name in types {
            dbus::put_string(&mut body, name);
        }
        set_len(&mut body, length, first);
        dbus::align(&mut body, 8);
        dbus::put_string(&mut body, "session-is-owner");
        dbus::put_signature(&mut body, "b");
        dbus::align(&mut body, 4);
        body.extend_from_slice(&u32::from(ours).to_le_bytes());
        dbus::align(&mut body, 8);
        dbus::put_string(&mut body, "unknown");
        dbus::put_signature(&mut body, "ay");
        dbus::align(&mut body, 4);
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(b"xyz");
        set_len(&mut body, 0, entries);
        body
    }

    #[test]
    fn an_owner_change_is_read_for_who_and_what() {
        let read = owner_of(&changed_to(
            &["text/plain;charset=utf-8", "UTF8_STRING"],
            false,
        ));
        assert_eq!(
            read,
            Owner {
                ours: false,
                types: vec![
                    "text/plain;charset=utf-8".to_string(),
                    "UTF8_STRING".to_string()
                ],
            }
        );
        assert!(owner_of(&changed_to(&OFFERED, true)).ours);
        // A clipboard emptied by its owner leaving names nothing to read.
        assert_eq!(
            owner_of(&changed_to(&[], false)).types,
            Vec::<String>::new()
        );
    }

    /// **The empty options are eight bytes, not four.** The array's length is
    /// followed by its elements' padding whether or not there are any, and the
    /// compositor refuses a body that stops short of it.
    #[test]
    fn no_options_still_pad_the_dictionary() {
        assert_eq!(offering(&[]), vec![0u8; 8]);
        let offered = offering(&["text/plain"]);
        assert_eq!(dbus::le32(&offered, 0) as usize, offered.len() - 8);
        // And what this offers reads back, in the bare form it is sent in.
        assert_eq!(owner_of(&offered).types, vec!["text/plain".to_string()]);
    }

    #[test]
    fn a_paste_is_asked_for_by_its_serial() {
        let mut body = Vec::new();
        dbus::put_string(&mut body, "UTF8_STRING");
        dbus::align(&mut body, 4);
        body.extend_from_slice(&77u32.to_le_bytes());
        assert_eq!(transfer_of(&body), 77);
    }

    /// **Off by default: it needs a GNOME session's own bus.** Run it as the
    /// person who is logged in, with `--ignored`. It leaves the clipboard
    /// empty once both sessions have gone.
    ///
    /// Two sessions stand in for the two ends: one claims a guest's text and
    /// serves it, the other is the desktop pasting it, through the same
    /// interface a program's paste reaches it by.
    #[test]
    #[ignore = "needs a GNOME session"]
    fn the_gnome_clipboard_can_be_owned_and_read() {
        let mut ours = Clip::connect().expect("GNOME's clipboard");
        let mut desktop = Clip::connect().expect("a second session");
        let written = format!("lowlat probe {} - héllo ✓", std::process::id());
        ours.write(&written).expect("claimed");

        // **The owner has to keep answering while the paste is read**, so it
        // runs on its own, and what it reports is any echo of its own write.
        let serving = std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(3);
            let mut echoed = None;
            while Instant::now() < until {
                if let Some(text) = ours.changed(Duration::from_millis(100)) {
                    echoed = Some(text);
                }
            }
            echoed
        });
        assert_eq!(
            desktop.changed(Duration::from_secs(2)).as_deref(),
            Some(written.as_str()),
            "the claimed text did not read back"
        );
        assert_eq!(
            serving.join().expect("the owner ran"),
            None,
            "our own write came back as a change"
        );
    }
}
