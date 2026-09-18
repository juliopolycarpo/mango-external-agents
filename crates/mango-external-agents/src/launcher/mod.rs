//! A default [`ProcessLauncher`](crate::ProcessLauncher) for hosts without a spawner of their own.
//!
//! Optional on purpose. A host that already owns its sandbox — job objects, cgroups, a container,
//! a bubblewrap profile — implements the port itself and this module never compiles. What is here
//! is the ordinary answer: a child in its own process group on Unix, nested private Job Objects
//! on Windows, no console window where requested, and an escalation that asks before it insists.

#[cfg(all(feature = "launcher-tokio", windows))]
mod powershell;

#[cfg(feature = "launcher-tokio")]
mod tokio_launcher;

#[cfg(feature = "launcher-tokio")]
pub use tokio_launcher::TokioLauncher;
