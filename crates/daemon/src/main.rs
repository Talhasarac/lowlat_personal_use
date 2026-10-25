//! The host service: signaling on one side, the admission seam on the other.
//!
//! This is the only place the two meet, and deliberately so. The SDK does not
//! link a signaling client, so something above both has to translate one into
//! the other, and that translation is the whole of this program.
//!
//! Four inbound actions map onto the seam's four calls, and the seam's event
//! queue maps back onto two outbound actions. Nothing else here is protocol.
//!
//!   KESSEL_SESSION=... [KESSEL_WS_SERVER=...] lowlatd [--name NAME] [--port N]
//!   lowlatd session
//!   lowlatd display list|add|hold|remove|mode|modes|gpu|pointer|edid ...

// The service is written once; what the session's side of it is, is the
// platform's (docs/impl-plan-windows.md). The channel, the bus, the seat and
// the tray are how Linux's session side is reached.
mod app;
#[cfg(target_os = "linux")]
mod channel;
#[cfg(target_os = "linux")]
mod datacontrol;
#[cfg(target_os = "linux")]
mod dbus;
mod display;
#[cfg(target_os = "linux")]
mod mutter;
#[cfg(target_os = "linux")]
mod seat;
mod service;
#[cfg(target_os = "linux")]
#[path = "session_side/linux.rs"]
mod session_side;
#[cfg(windows)]
#[path = "session_side/windows.rs"]
mod session_side;
#[cfg(target_os = "linux")]
mod sni;

fn main() {
    service::main();
}
