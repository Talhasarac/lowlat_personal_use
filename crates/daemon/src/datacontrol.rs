//! The clipboard on wlroots desktops -- labwc on the Raspberry Pi, sway and
//! the rest -- through the Wayland data-control protocol.
//!
//! **A clipboard manager's protocol, which is what this is.** A Wayland client
//! normally owns the selection only while one of its windows has focus; a
//! data-control client has no window and may own it, serve it and hear it
//! change at any time. The compositor offers it as `ext_data_control_manager_v1`
//! (the standard one) or `zwlr_data_control_manager_v1` (the wlroots one it
//! grew from); the two carry the same messages in the same order, so either is
//! bound and spoken the same way.
//!
//! **Spoken directly rather than through a library.** Five objects and a dozen
//! messages, each a header and a few words, with the text itself on a pipe
//! handed across the socket -- the same shape as the bus the other two
//! clipboards here are reached over.
//!
//! **The connection is the lease.** A source this client made is dropped when
//! the connection goes, and the selection with it, which is the shape a
//! clipboard owner has to have anyway (docs/07-platforms.md 5.1).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::mutter::{OFFERED, WANTED, drain, fill, receive};

/// The managers, standard first.
const MANAGERS: [&str; 2] = [
    "ext_data_control_manager_v1",
    "zwlr_data_control_manager_v1",
];

/// A type no program asks for, offered beside the text, so a selection this
/// host made is known for its own when the compositor announces it.
///
/// **Not a convenience.** Reading a selection means asking its owner for the
/// bytes and waiting on the pipe; asked of this connection's own source, the
/// request that would answer it sits behind the wait, and the read times out.
const OWN: &str = "application/x-lowlat-own";

/// How long a transfer waits for its other end.
const TRANSFER_MS: u64 = 2_000;

/// How long a new connection waits for the compositor to describe itself.
const ROUNDTRIP_MS: u64 = 1_000;

// The ids this client gives the objects it makes once, in the order it makes
// them. Ids it makes later count up from `FIRST_FREE` and are never reused.
//
// **In order, and that is the compositor's rule, not tidiness.** A client's
// new ids have to be the next unused number each time; one that skips ahead is
// refused as invalid and the connection is ended.
const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;
const ROLL_CALL: u32 = 3;
const MANAGER: u32 = 4;
const SEAT: u32 = 5;
const DEVICE: u32 = 6;
const FIRST_FREE: u32 = 7;

/// The desktop's clipboard, owned through the data-control protocol.
#[derive(Debug)]
pub(crate) struct Clip {
    wire: Wire,
    next_id: u32,
    /// Every offer the compositor has made and not been told to drop, with
    /// the types it named.
    offers: HashMap<u32, Vec<String>>,
    /// Every source this client made and has not destroyed. **Kept apart
    /// from the callbacks**, which count up from the same ids: an event's
    /// meaning is its object's kind, and a callback's one event has the same
    /// number as a source's request for the bytes.
    sources: HashSet<u32>,
    /// The source holding a guest's text, and the text, while this host owns
    /// the selection.
    serving: Option<(u32, String)>,
    /// What was last seen or set, so a change this host caused is not sent
    /// back to the guest that caused it.
    last: String,
    /// The compositor went away and has not been reached since.
    lost: bool,
}

/// What one event came to.
enum Outcome {
    Nothing,
    /// The selection changed to an offer, or to nothing.
    Selection(u32),
    /// A roundtrip finished.
    Done(u32),
}

impl Clip {
    /// Reach the compositor, find the manager and the seat, and take the
    /// seat's clipboard device, or say why not.
    pub(crate) fn connect() -> Result<Self, String> {
        let stream = UnixStream::connect(socket_path()).map_err(|error| format!("{error}"))?;
        let mut clip = Self {
            wire: Wire::new(stream),
            next_id: FIRST_FREE,
            offers: HashMap::new(),
            sources: HashSet::new(),
            serving: None,
            last: String::new(),
            lost: false,
        };
        clip.wire
            .send(DISPLAY, 1, &Args::new().uint(REGISTRY), None)?;
        let globals = clip.globals()?;
        let (name, interface) = MANAGERS
            .iter()
            .find_map(|wanted| {
                globals
                    .iter()
                    .find(|(_, offered, _)| offered == wanted)
                    .map(|(name, offered, _)| (*name, offered.clone()))
            })
            .ok_or("the compositor offers no data-control protocol")?;
        let seat = globals
            .iter()
            .find(|(_, offered, _)| offered == "wl_seat")
            .map(|(name, _, _)| *name)
            .ok_or("the compositor offers no seat")?;
        clip.bind(name, &interface, MANAGER)?;
        clip.bind(seat, "wl_seat", SEAT)?;
        clip.wire
            .send(MANAGER, 1, &Args::new().uint(DEVICE).uint(SEAT), None)?;
        // **What is on the clipboard already is the starting point, not a
        // change.** The device announces it at once; somebody copied it before
        // this host was here, and it is not the guests' to be handed.
        let until = Instant::now() + Duration::from_millis(ROUNDTRIP_MS);
        let done = clip.sync()?;
        while let Some(outcome) = clip.next(until)? {
            match outcome {
                Outcome::Selection(offer) => {
                    if let Some(text) = clip.read(offer) {
                        clip.last = text;
                    }
                }
                Outcome::Done(id) if id == done => break,
                _ => {}
            }
        }
        lowlat_common::log_info!("session: the clipboard is the compositor's, through {interface}");
        Ok(clip)
    }

    /// Put a guest's text on the desktop's clipboard.
    ///
    /// **Claimed, not stored.** The compositor is told what this offers, and
    /// the bytes go when somebody pastes; until then this connection holds
    /// them.
    pub(crate) fn write(&mut self, text: &str) -> Result<(), String> {
        let source = self.fresh_id();
        self.wire
            .send(MANAGER, 0, &Args::new().uint(source), None)?;
        self.sources.insert(source);
        for kind in OFFERED.iter().chain([&OWN]) {
            self.wire.send(source, 0, &Args::new().string(kind), None)?;
        }
        self.wire.send(DEVICE, 0, &Args::new().uint(source), None)?;
        // The one it replaces is told it was cancelled, and dropped then.
        self.serving = Some((source, text.to_string()));
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
            match self.next(until) {
                Ok(None) => return None,
                Ok(Some(Outcome::Selection(offer))) => {
                    if let Some(text) = self.read(offer)
                        && !text.is_empty()
                        && text != self.last
                    {
                        self.last.clone_from(&text);
                        return Some(text);
                    }
                }
                Ok(Some(_)) => {}
                Err(error) => {
                    lowlat_common::log_warn!(
                        "session: the compositor's clipboard went away, {error}"
                    );
                    self.lost = true;
                    return None;
                }
            }
        }
    }

    /// The next event that matters to the caller, handling the rest on the
    /// way, or nothing by the deadline.
    fn next(&mut self, until: Instant) -> Result<Option<Outcome>, String> {
        loop {
            let Some(message) = self.wire.message(until)? else {
                return Ok(None);
            };
            match self.handle(&message)? {
                Outcome::Nothing => {}
                outcome => return Ok(Some(outcome)),
            }
        }
    }

    fn handle(&mut self, message: &Message) -> Result<Outcome, String> {
        let mut args = Reader::new(&message.body);
        match (message.object, message.opcode) {
            (DISPLAY, 0) => {
                let _object = args.uint();
                let code = args.uint();
                let said = args.string();
                Err(format!(
                    "the compositor refused a request, code {code:?}: {said:?}"
                ))
            }
            (DEVICE, 0) => {
                if let Some(offer) = args.uint() {
                    self.offers.insert(offer, Vec::new());
                }
                Ok(Outcome::Nothing)
            }
            (DEVICE, 1) => Ok(Outcome::Selection(args.uint().unwrap_or(0))),
            (DEVICE, 2) => Err("the compositor finished the clipboard device".to_string()),
            (DEVICE, 3) => {
                // The primary selection is not the clipboard; its offer is
                // given straight back unless it is the clipboard's too.
                if let Some(offer) = args.uint().filter(|offer| *offer != 0) {
                    self.drop_offer(offer);
                }
                Ok(Outcome::Nothing)
            }
            (object, 0) if self.offers.contains_key(&object) => {
                if let (Some(kind), Some(kinds)) = (args.string(), self.offers.get_mut(&object)) {
                    kinds.push(kind);
                }
                Ok(Outcome::Nothing)
            }
            (object, 0) if self.sources.contains(&object) => {
                // **Somebody is pasting**: the type asked for does not matter,
                // every one offered is the same UTF-8. A source that has since
                // been replaced closes the descriptor unanswered, which the
                // other end reads as an empty paste rather than a hang.
                let fd = self.wire.fds.pop_front();
                if let Some((_, text)) = self.serving.as_ref().filter(|(id, _)| *id == object)
                    && let Some(fd) = fd
                {
                    let until = Instant::now() + Duration::from_millis(TRANSFER_MS);
                    if let Err(error) = fill(fd, text.as_bytes(), until) {
                        lowlat_common::log_warn!("session: a paste was not served, {error}");
                    }
                }
                Ok(Outcome::Nothing)
            }
            (object, 1) if self.sources.contains(&object) => {
                // Cancelled: somebody else owns the selection now.
                self.sources.remove(&object);
                if self.serving.as_ref().is_some_and(|(id, _)| *id == object) {
                    self.serving = None;
                }
                self.wire.send(object, 1, &Args::new(), None)?;
                Ok(Outcome::Nothing)
            }
            (object, 0) if object >= FIRST_FREE => Ok(Outcome::Done(object)),
            _ => Ok(Outcome::Nothing),
        }
    }

    /// Read a selection's text, or nothing when it is not text, is this
    /// host's own, or does not arrive in time.
    fn read(&mut self, offer: u32) -> Option<String> {
        // **Every other offer is stale once a selection is announced**, and
        // each one is an object the compositor keeps until it is told.
        let stale: Vec<u32> = self
            .offers
            .keys()
            .copied()
            .filter(|id| *id != offer)
            .collect();
        for id in stale {
            self.drop_offer(id);
        }
        if offer == 0 {
            return None;
        }
        let kinds = self.offers.get(&offer)?;
        if kinds.iter().any(|kind| kind == OWN) {
            return None;
        }
        let kind = WANTED
            .iter()
            .find(|wanted| kinds.iter().any(|kind| kind == *wanted))?;
        let (theirs, ours) = pipe().ok()?;
        if let Err(error) = self
            .wire
            .send(offer, 0, &Args::new().string(kind), Some(&ours))
        {
            lowlat_common::log_warn!("session: the clipboard was not asked for, {error}");
            return None;
        }
        // Ours is closed here, so the read ends when the owner closes its own.
        drop(ours);
        let until = Instant::now() + Duration::from_millis(TRANSFER_MS);
        match drain(theirs, until) {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(error) => {
                lowlat_common::log_warn!("session: the clipboard could not be read, {error}");
                None
            }
        }
    }

    /// Tell the compositor an offer is no longer wanted.
    fn drop_offer(&mut self, offer: u32) {
        if self.offers.remove(&offer).is_some() {
            let _ = self.wire.send(offer, 1, &Args::new(), None);
        }
    }

    /// The registry's globals, by number, name and version.
    fn globals(&mut self) -> Result<Vec<(u32, String, u32)>, String> {
        let done = ROLL_CALL;
        self.wire.send(DISPLAY, 0, &Args::new().uint(done), None)?;
        let until = Instant::now() + Duration::from_millis(ROUNDTRIP_MS);
        let mut globals = Vec::new();
        loop {
            let message = self
                .wire
                .message(until)?
                .ok_or("the compositor did not describe itself in time")?;
            match (message.object, message.opcode) {
                (REGISTRY, 0) => {
                    let mut args = Reader::new(&message.body);
                    if let (Some(name), Some(interface), Some(version)) =
                        (args.uint(), args.string(), args.uint())
                    {
                        globals.push((name, interface, version));
                    }
                }
                (id, 0) if id == done => return Ok(globals),
                (DISPLAY, 0) => return Err("the compositor refused the registry".to_string()),
                _ => {}
            }
        }
    }

    /// Bind a global at the first version, which is all this asks of any.
    fn bind(&mut self, name: u32, interface: &str, id: u32) -> Result<(), String> {
        self.wire.send(
            REGISTRY,
            0,
            &Args::new().uint(name).string(interface).uint(1).uint(id),
            None,
        )
    }

    /// Ask for a callback once everything sent so far is handled.
    fn sync(&mut self) -> Result<u32, String> {
        let callback = self.fresh_id();
        self.wire
            .send(DISPLAY, 0, &Args::new().uint(callback), None)?;
        Ok(callback)
    }

    fn fresh_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Try the compositor again, or wait out the turn.
    fn reconnect(&mut self, until: Instant) {
        match Self::connect() {
            Ok(fresh) => {
                lowlat_common::log_info!("session: the compositor's clipboard is back");
                *self = fresh;
            }
            Err(_) => std::thread::sleep(until.saturating_duration_since(Instant::now())),
        }
    }
}

/// The compositor's socket: the session's display name, in its runtime
/// directory unless it is a path already.
fn socket_path() -> std::path::PathBuf {
    let display = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
    let display = std::path::PathBuf::from(display);
    if display.is_absolute() {
        return display;
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
        // SAFETY: getuid cannot fail.
        || std::path::PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })),
        std::path::PathBuf::from,
    );
    runtime.join(display)
}

/// A pipe, read end first, both closed on exec.
fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0; 2];
    // SAFETY: two slots for the two descriptors the call writes.
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both were just made and are owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) })
}

/// One message from the compositor.
#[derive(Debug)]
struct Message {
    object: u32,
    opcode: u16,
    body: Vec<u8>,
}

/// The socket, framed: bytes buffered until a message is whole, descriptors
/// queued in the order they came.
#[derive(Debug)]
struct Wire {
    stream: UnixStream,
    pending: Vec<u8>,
    fds: VecDeque<OwnedFd>,
}

impl Wire {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            pending: Vec::new(),
            fds: VecDeque::new(),
        }
    }

    /// Send one request, with a descriptor when it carries one.
    fn send(
        &mut self,
        object: u32,
        opcode: u16,
        args: &Args,
        fd: Option<&OwnedFd>,
    ) -> Result<(), String> {
        let size = u32::try_from(8 + args.0.len()).map_err(|_| "a request too large")?;
        let mut bytes = Vec::with_capacity(8 + args.0.len());
        bytes.extend_from_slice(&object.to_ne_bytes());
        bytes.extend_from_slice(&((size << 16) | u32::from(opcode)).to_ne_bytes());
        bytes.extend_from_slice(&args.0);
        match fd {
            None => self
                .stream
                .write_all(&bytes)
                .map_err(|error| format!("{error}")),
            Some(fd) => send_with_fd(&self.stream, &bytes, fd).map_err(|error| format!("{error}")),
        }
    }

    /// The next whole message, or nothing by the deadline.
    fn message(&mut self, until: Instant) -> Result<Option<Message>, String> {
        loop {
            if self.pending.len() >= 8 {
                let object = u32::from_ne_bytes(self.pending[0..4].try_into().unwrap_or_default());
                let word = u32::from_ne_bytes(self.pending[4..8].try_into().unwrap_or_default());
                let size = (word >> 16) as usize;
                if size < 8 {
                    return Err("a message shorter than its header".to_string());
                }
                if self.pending.len() >= size {
                    let body = self.pending[8..size].to_vec();
                    self.pending.drain(..size);
                    return Ok(Some(Message {
                        object,
                        opcode: (word & 0xffff) as u16,
                        body,
                    }));
                }
            }
            let left = until.saturating_duration_since(Instant::now());
            let mut poll = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX);
            // SAFETY: one live poll entry, counted as one.
            let woke = unsafe { libc::poll(&mut poll, 1, ms) };
            if woke == 0 {
                return Ok(None);
            }
            if woke < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("poll: {error}"));
            }
            let mut chunk = [0u8; 4096];
            let read = receive(&self.stream, &mut chunk, &mut self.fds)
                .map_err(|error| format!("{error}"))?;
            if read == 0 {
                return Err("the compositor closed the connection".to_string());
            }
            self.pending
                .extend_from_slice(chunk.get(..read).unwrap_or(&[]));
        }
    }
}

/// A request's arguments, as the wire lays them out.
struct Args(Vec<u8>);

impl Args {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn uint(mut self, value: u32) -> Self {
        self.0.extend_from_slice(&value.to_ne_bytes());
        self
    }

    /// A string: its length counting the terminator, the bytes, the
    /// terminator, padded to a word.
    fn string(mut self, value: &str) -> Self {
        let len = u32::try_from(value.len() + 1).unwrap_or(u32::MAX);
        self.0.extend_from_slice(&len.to_ne_bytes());
        self.0.extend_from_slice(value.as_bytes());
        self.0.push(0);
        while self.0.len() % 4 != 0 {
            self.0.push(0);
        }
        self
    }
}

/// An event's arguments, read in order.
struct Reader<'a> {
    body: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self { body, at: 0 }
    }

    fn uint(&mut self) -> Option<u32> {
        let word = self.body.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(u32::from_ne_bytes(word.try_into().ok()?))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.uint()? as usize;
        if len == 0 {
            return Some(String::new());
        }
        let bytes = self.body.get(self.at..self.at + len - 1)?;
        self.at += len.next_multiple_of(4);
        Some(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// The bytes one descriptor takes in a control message.
const FD_BYTES: u32 = 4;

/// Write a request with one descriptor riding on it.
fn send_with_fd(stream: &UnixStream, bytes: &[u8], fd: &OwnedFd) -> std::io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    // Room for one descriptor's header, aligned for it.
    let mut control = [0u64; 4];
    // SAFETY: an all-zero message header is the empty one.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    // SAFETY: the macro computes a size from a constant.
    header.msg_controllen = unsafe { libc::CMSG_SPACE(FD_BYTES) } as usize;
    // SAFETY: the control buffer is large enough for one descriptor's header,
    // and every pointer is to a live local.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(FD_BYTES) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>(), fd.as_raw_fd());
    }
    // SAFETY: the header describes live locals of the sizes it states.
    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &header, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if usize::try_from(sent).unwrap_or(0) != bytes.len() {
        return Err(std::io::Error::other("a request was cut short"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_counts_its_terminator_and_pads_to_a_word() {
        let args = Args::new().string("wl_seat").uint(7);
        // Seven bytes and the terminator fill two words exactly.
        assert_eq!(&args.0[..4], &8u32.to_ne_bytes());
        assert_eq!(&args.0[4..12], b"wl_seat\0");
        assert_eq!(&args.0[12..], &7u32.to_ne_bytes());
        let padded = Args::new().string("text/plain");
        assert_eq!(padded.0.len(), 4 + 12);
    }

    #[test]
    fn an_event_is_read_back_in_the_order_it_was_written() {
        let body = Args::new()
            .uint(33)
            .string("ext_data_control_manager_v1")
            .uint(1)
            .0;
        let mut reader = Reader::new(&body);
        assert_eq!(reader.uint(), Some(33));
        assert_eq!(
            reader.string().as_deref(),
            Some("ext_data_control_manager_v1")
        );
        assert_eq!(reader.uint(), Some(1));
        assert_eq!(reader.uint(), None);
    }
}
