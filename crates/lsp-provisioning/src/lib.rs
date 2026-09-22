//! Generic server-provisioning contract. Nothing in this module knows about dbt, or any
//! other specific upstream server — that lives behind [`ServerProvisioner`] implementations
//! (see [`dbt_fusion`]), so a future second server (per the per-server packaging initiative)
//! adds a new module here rather than growing a pile of `if server == "dbt"` branches.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub mod dbt_fusion;

/// What discovery found, in enough detail to build an actionable diagnostic from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallStatus {
    /// No candidate binary found anywhere this provisioner knows to look.
    NotFound,
    /// Found, and it satisfies this provisioner's pinned compatibility requirement.
    Found { path: PathBuf, version: String },
    /// Found something, but it's the wrong thing or an incompatible version — distinct from
    /// `NotFound` because the fix is different ("upgrade X" vs "install X").
    FoundIncompatible { path: PathBuf, version: String, reason: String },
}

/// A discovery result the caller can show to a human as-is: a status, a human-readable
/// actionable message, and a link to current (not historical) upstream docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub status: InstallStatus,
    pub message: String,
    pub doc_url: String,
    /// Whether `acquire` is a meaningful next step for this status. False for e.g. a status
    /// this provisioner has no acquisition path for at all (caller must self-serve).
    pub can_acquire: bool,
}

/// Explicit, per-call consent to run an acquisition. There is no "default yes": every
/// provisioner must require `explicit: true`, so acquisition can never be a side effect of
/// discovery, a default flag, or a config file. This is the enforcement point for "does not
/// occur silently."
#[derive(Debug, Clone, Copy, Default)]
pub struct AcquisitionConsent {
    pub explicit: bool,
}

/// Cooperative cancellation: checked between each acquisition step (and, for a spawned
/// installer subprocess, polled while it runs so the child can be killed promptly). Cloning
/// shares the same flag, so the caller can hold one end and cancel from another thread/task.
#[derive(Debug, Clone, Default)]
pub struct CancellationFlag(Arc<AtomicBool>);

impl CancellationFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Progress events an acquisition reports as it runs, so a caller can render "not silent"
/// feedback (a progress bar, a log line, whatever the host UI is) rather than blocking with
/// no output for the duration of a multi-hundred-MB download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquisitionEvent {
    Started { server_name: &'static str },
    RunningInstaller { command: String },
    VerifyingIntegrity,
    Verified { path: PathBuf, version: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquisitionOutcome {
    Installed { path: PathBuf, version: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquisitionError {
    /// `acquire` was called without `AcquisitionConsent { explicit: true }`.
    ConsentRequired,
    /// The installer ran but produced something whose integrity we could not verify —
    /// treated as a hard failure, never a soft fallback to "trust it anyway."
    IntegrityCheckFailed(String),
    /// The installer/toolchain needed to acquire the server isn't available (e.g. no `sh`,
    /// no `curl`, no network egress) — surfaced with a suggestion for the human path instead.
    ToolchainUnavailable(String),
    Io(String),
}

impl std::fmt::Display for AcquisitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConsentRequired => write!(
                f,
                "acquisition requires explicit consent (AcquisitionConsent{{ explicit: true }}); \
                 refusing to install anything silently"
            ),
            Self::IntegrityCheckFailed(msg) => write!(f, "integrity check failed: {msg}"),
            Self::ToolchainUnavailable(msg) => write!(f, "required toolchain unavailable: {msg}"),
            Self::Io(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for AcquisitionError {}

/// One upstream server this crate knows how to discover / diagnose / (optionally) acquire.
/// Discovery and diagnostics never touch the network or the filesystem beyond read-only
/// lookups (`PATH`, running `--version`); only `acquire` may write, download, or spawn an
/// installer, and only after explicit consent.
pub trait ServerProvisioner {
    fn server_name(&self) -> &'static str;
    fn languages(&self) -> &'static [&'static str];

    /// Read-only: look for a compatible install and explain what was found.
    fn diagnose(&self) -> Diagnostic;

    /// Given a `Found` diagnostic, the concrete command to launch (matching the shape
    /// `intentumdiff-lsp-client::specs::ServerSpec::command` expects). `None` for any other
    /// status — there is nothing to launch.
    fn resolve_launch_command(&self, diagnostic: &Diagnostic) -> Option<Vec<String>>;

    /// Explicit, cancellable, integrity-checked acquisition. Implementations must:
    /// - return `Err(ConsentRequired)` immediately if `consent.explicit` is false;
    /// - emit `AcquisitionEvent`s via `progress` as work happens (never run silently);
    /// - check `cancel.is_cancelled()` between steps and return `Ok(Cancelled)` promptly;
    /// - verify the result's integrity before reporting `Installed` — never skip on failure.
    fn acquire(
        &self,
        consent: AcquisitionConsent,
        progress: &mut dyn FnMut(AcquisitionEvent),
        cancel: &CancellationFlag,
    ) -> Result<AcquisitionOutcome, AcquisitionError>;
}
