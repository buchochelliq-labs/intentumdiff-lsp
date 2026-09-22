//! [`ServerProvisioner`] for dbt's language server — languages `dbt-sql`, `dbt-yaml`.
//!
//! # Ownership-audit findings (read before touching this file)
//!
//! The original issue text is stale on three load-bearing facts, checked against current
//! upstream docs during implementation (2026-09):
//!
//! 1. **`dbt-lsp` is not an independently installable server.** It used to be a separate
//!    download; today the language server ships *inside* the single `dbt` Fusion engine
//!    binary ("the Fusion CLI and the language server ship in a single dbt Fusion engine
//!    binary, so they always share the same version"). There is no `pip install dbt-lsp`.
//!    A file literally named `dbt-lsp` does still turn up (the VS Code extension copies the
//!    Fusion binary to that name under its own global-storage dir), but that's an
//!    implementation detail of the extension, not a general-PATH install a user can rely on.
//! 2. **Licensing is not a clean "just vendor it."** dbt Core proper is Apache-2.0, but the
//!    Fusion *binary* (which contains the LSP) is additionally governed by dbt Labs' own
//!    "dbt Product Licensing Agreement" — free to use, with provisions, not a permissive OSS
//!    license we're clearly clear to redistribute under. See `THIRD_PARTY_NOTICES.md`.
//! 3. There **is** an official, versioned, checksummed distribution channel we can reuse
//!    instead of hosting anything ourselves: dbt Labs publish a standalone installer script
//!    and per-extension-version SHA-256 checksum manifests (see
//!    `docs.getdbt.com/docs/dbt-versions/fusion-version-compatibility`).
//!
//! Net design: **never bundle the binary.** Discover a compatible `dbt` on PATH; if absent
//! or incompatible, give an actionable diagnostic; only on explicit consent, shell out to
//! dbt Labs' own official installer and verify the result against their published checksum
//! manifest before ever reporting success.
//!
//! # Open item — needs verification against a real Fusion install before this ships
//!
//! The exact stdio invocation for the language server (what argv the VS Code extension
//! actually launches) is **not confirmed** — dbt Labs don't document a public `dbt lsp ...`
//! subcommand; the extension is the only documented caller. [`STDIO_COMMAND_CANDIDATE`] is a
//! best-guess placeholder, not a verified fact, and is marked as such everywhere it's used.
//! Confirm with `dbt --help` / `strings` on a real Fusion binary, or by asking dbt Labs,
//! before wiring this into `known_server_specs()`.

use std::path::PathBuf;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::{
    AcquisitionConsent, AcquisitionError, AcquisitionEvent, AcquisitionOutcome, CancellationFlag,
    Diagnostic, InstallStatus, ServerProvisioner,
};

/// Minimum Fusion version this provisioner accepts, pinned to the compatibility matrix at
/// time of writing (dbt VS Code extension 0.36.x → Fusion >= `2.0.0-preview.90`). Update this
/// alongside a real compatibility check when bumping — do not let it silently drift.
pub const MIN_FUSION_VERSION: FusionVersion = FusionVersion { major: 2, minor: 0, patch: 0, preview: Some(90) };

/// **Unverified placeholder** — see the module-level "Open item" note. Do not treat as fact.
pub const STDIO_COMMAND_CANDIDATE: &[&str] = &["dbt", "lsp-server", "--stdio"];

pub const CURRENT_DOC_URL: &str = "https://docs.getdbt.com/docs/about-dbt-lsp";
const INSTALLER_URL: &str = "https://public.cdn.getdbt.com/fs/install/install.sh";

pub struct DbtFusionProvisioner;

impl DbtFusionProvisioner {
    pub fn new() -> Self {
        Self
    }

    fn locate_dbt(&self) -> Option<PathBuf> {
        which("dbt")
    }

    /// Runs `<dbt> --version --format json` and extracts the `version` field. Returns `None`
    /// on any failure (not found, non-zero exit, unparsable output) — callers treat that as
    /// "found something, but couldn't confirm what."
    fn probe_version(&self, dbt: &PathBuf) -> Option<String> {
        let output = Command::new(dbt).args(["--version", "--format", "json"]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
        parsed.get("version")?.as_str().map(str::to_owned)
    }
}

impl Default for DbtFusionProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerProvisioner for DbtFusionProvisioner {
    fn server_name(&self) -> &'static str {
        "dbt Fusion (dbt-lsp)"
    }

    fn languages(&self) -> &'static [&'static str] {
        &["dbt-sql", "dbt-yaml"]
    }

    fn diagnose(&self) -> Diagnostic {
        let Some(path) = self.locate_dbt() else {
            return Diagnostic {
                status: InstallStatus::NotFound,
                message: format!(
                    "No `dbt` binary found on PATH. dbt-sql/dbt-yaml enrichment needs the dbt \
                     Fusion engine, which ships both the CLI and the language server in one \
                     binary. Install it (standalone script, pip, Homebrew, or winget — see the \
                     docs link), or run acquisition here with explicit consent. See {CURRENT_DOC_URL}."
                ),
                doc_url: CURRENT_DOC_URL.to_owned(),
                can_acquire: true,
            };
        };
        let Some(version) = self.probe_version(&path) else {
            return Diagnostic {
                status: InstallStatus::FoundIncompatible {
                    path: path.clone(),
                    version: "unknown".to_owned(),
                    reason: "`dbt --version --format json` did not return a parsable version — \
                              this may be a legacy dbt-core (Python) install rather than the \
                              Fusion engine; the LSP is Fusion-only."
                        .to_owned(),
                },
                message: format!(
                    "Found `dbt` at {} but couldn't confirm it's the Fusion engine (the LSP \
                     only ships in Fusion, not legacy dbt-core). Run `dbt --version` yourself \
                     to check, or reinstall via the Fusion install path. See {CURRENT_DOC_URL}.",
                    path.display()
                ),
                doc_url: CURRENT_DOC_URL.to_owned(),
                can_acquire: true,
            };
        };
        match FusionVersion::parse(&version) {
            Some(parsed) if parsed >= MIN_FUSION_VERSION => {
                Diagnostic {
                    status: InstallStatus::Found { path, version },
                    message: "Compatible dbt Fusion engine found.".to_owned(),
                    doc_url: CURRENT_DOC_URL.to_owned(),
                    can_acquire: false,
                }
            }
            Some(_) => Diagnostic {
                status: InstallStatus::FoundIncompatible {
                    path: path.clone(),
                    version: version.clone(),
                    reason: format!(
                        "installed Fusion version {version} is older than the pinned minimum \
                         {MIN_FUSION_VERSION}"
                    ),
                },
                message: format!(
                    "Found dbt Fusion {version} at {}, but this integration is pinned to \
                     {MIN_FUSION_VERSION} or newer. Run `dbt system update`, or run \
                     acquisition here with explicit consent. See {CURRENT_DOC_URL}.",
                    path.display()
                ),
                doc_url: CURRENT_DOC_URL.to_owned(),
                can_acquire: true,
            },
            None => Diagnostic {
                status: InstallStatus::FoundIncompatible {
                    path: path.clone(),
                    version: version.clone(),
                    reason: format!("version string {version:?} did not parse as a Fusion version"),
                },
                message: format!(
                    "Found `dbt` at {} reporting version {version:?}, which doesn't look like a \
                     Fusion version string. Treating as incompatible rather than guessing. \
                     See {CURRENT_DOC_URL}.",
                    path.display()
                ),
                doc_url: CURRENT_DOC_URL.to_owned(),
                can_acquire: true,
            },
        }
    }

    fn resolve_launch_command(&self, diagnostic: &Diagnostic) -> Option<Vec<String>> {
        match &diagnostic.status {
            InstallStatus::Found { path, .. } => {
                // TODO(verify): STDIO_COMMAND_CANDIDATE's *arguments* are unconfirmed (see
                // module docs). The binary path itself is real; only argv[1..] is a guess.
                let mut command = vec![path.to_string_lossy().into_owned()];
                command.extend(STDIO_COMMAND_CANDIDATE[1..].iter().map(|s| (*s).to_owned()));
                Some(command)
            }
            _ => None,
        }
    }

    fn acquire(
        &self,
        consent: AcquisitionConsent,
        progress: &mut dyn FnMut(AcquisitionEvent),
        cancel: &CancellationFlag,
    ) -> Result<AcquisitionOutcome, AcquisitionError> {
        if !consent.explicit {
            return Err(AcquisitionError::ConsentRequired);
        }
        progress(AcquisitionEvent::Started { server_name: self.server_name() });

        if cancel.is_cancelled() {
            return Ok(AcquisitionOutcome::Cancelled);
        }

        // Reuse dbt Labs' own official installer rather than fetching/hosting a binary
        // ourselves — it is the one artifact we know is both current and licensed for this
        // use (it's the public download path dbt Labs ship to every user). We do not proxy
        // or cache the binary content anywhere in this repo.
        for tool in ["sh", "curl"] {
            if which(tool).is_none() {
                return Err(AcquisitionError::ToolchainUnavailable(format!(
                    "`{tool}` not found; run the installer yourself: \
                     curl -fsSL {INSTALLER_URL} | sh -s -- --update"
                )));
            }
        }

        let command_str = format!("curl -fsSL {INSTALLER_URL} | sh -s -- --update");
        progress(AcquisitionEvent::RunningInstaller { command: command_str.clone() });

        if cancel.is_cancelled() {
            return Ok(AcquisitionOutcome::Cancelled);
        }

        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("curl -fsSL {INSTALLER_URL} | sh -s -- --update"))
            .status()
            .map_err(|e| AcquisitionError::Io(format!("failed to spawn installer: {e}")))?;
        if !status.success() {
            return Err(AcquisitionError::Io(format!(
                "installer exited with {status}; nothing was verified or reported as installed"
            )));
        }

        if cancel.is_cancelled() {
            // The installer already ran; we didn't skip a step, but we still decline to
            // report success on a path the caller asked us to abandon.
            return Ok(AcquisitionOutcome::Cancelled);
        }

        progress(AcquisitionEvent::VerifyingIntegrity);
        let path = self
            .locate_dbt()
            .ok_or_else(|| AcquisitionError::Io("installer ran but no `dbt` found on PATH afterwards".to_owned()))?;
        let version = self.probe_version(&path).ok_or_else(|| {
            AcquisitionError::IntegrityCheckFailed(
                "installed binary did not report a parsable version".to_owned(),
            )
        })?;
        verify_against_published_checksum(&path).map_err(AcquisitionError::IntegrityCheckFailed)?;

        progress(AcquisitionEvent::Verified { path: path.clone(), version: version.clone() });
        Ok(AcquisitionOutcome::Installed { path, version })
    }
}

/// Compares the freshly-installed binary's SHA-256 against dbt Labs' published checksum
/// manifest for the matching release, so a corrupted or MITM'd download is a hard failure
/// rather than a binary we silently trust.
///
/// TODO(verify): the manifest URL/filename pattern (`checksums-<extension-version>.txt`) is
/// documented on the compatibility-matrix page but the literal path template wasn't confirmed
/// against a live fetch during this pass (no network egress to `public.cdn.getdbt.com` from
/// the environment this was authored in). Wire the real URL in before relying on this in
/// production; until then this function intentionally returns an explicit error rather than a
/// false "verified."
fn verify_against_published_checksum(_path: &PathBuf) -> Result<(), String> {
    Err("checksum-manifest URL not yet wired up — see TODO(verify) above; refusing to report \
         an unverified install as trusted"
        .to_owned())
}

#[allow(dead_code)]
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Minimal `shutil.which`-alike over the real `PATH` env var (kept local/tiny rather than
/// depending on `intentumdiff-lsp-client` from this crate — see the Cargo.toml rationale for
/// why this crate stays independent).
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let sep = if cfg!(windows) { ';' } else { ':' };
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    for dir in path_var.to_string_lossy().split(sep).filter(|d| !d.is_empty()) {
        for ext in exts {
            let candidate = PathBuf::from(dir).join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// A parsed Fusion version, e.g. `2.0.0-preview.92`. Orders by `(major, minor, patch,
/// preview)`, treating "no preview suffix" as newer than any preview of the same triple (a
/// stable `2.0.0` should outrank `2.0.0-preview.99`) — so `Ord`/`PartialOrd` are hand-written
/// below rather than derived: the derive would put `None < Some(_)`, the opposite of that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FusionVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub preview: Option<u32>,
}

impl FusionVersion {
    /// `preview` as a sort key where "no preview" (stable) outranks every preview build of
    /// the same `(major, minor, patch)`.
    fn preview_sort_key(&self) -> u32 {
        self.preview.map_or(u32::MAX, |p| p)
    }
}

impl PartialOrd for FusionVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FusionVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch, self.preview_sort_key()).cmp(&(
            other.major,
            other.minor,
            other.patch,
            other.preview_sort_key(),
        ))
    }
}

impl std::fmt::Display for FusionVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(p) = self.preview {
            write!(f, "-preview.{p}")?;
        }
        Ok(())
    }
}

impl FusionVersion {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches("dbt-fusion").trim();
        let (core, preview) = match s.split_once("-preview.") {
            Some((core, n)) => (core, Some(n.parse().ok()?)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        Some(Self { major, minor, patch, preview })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn version_parsing_and_ordering() {
        assert_eq!(
            FusionVersion::parse("2.0.0-preview.92"),
            Some(FusionVersion { major: 2, minor: 0, patch: 0, preview: Some(92) })
        );
        assert_eq!(
            FusionVersion::parse("2.1.3"),
            Some(FusionVersion { major: 2, minor: 1, patch: 3, preview: None })
        );
        assert_eq!(FusionVersion::parse("not-a-version"), None);
        assert!(
            FusionVersion::parse("2.0.0-preview.95").unwrap()
                > FusionVersion::parse("2.0.0-preview.90").unwrap()
        );
        assert!(
            FusionVersion::parse("2.0.0-preview.999").unwrap()
                < FusionVersion::parse("2.0.1-preview.1").unwrap()
        );
        // A stable release outranks any preview of the identical (major, minor, patch).
        assert!(
            FusionVersion::parse("2.0.0").unwrap() > FusionVersion::parse("2.0.0-preview.999").unwrap()
        );
    }

    #[test]
    fn min_version_display_matches_pinned_compatibility_matrix() {
        assert_eq!(MIN_FUSION_VERSION.to_string(), "2.0.0-preview.90");
    }

    /// Builds a fake `dbt` shell script on a temp PATH and points discovery at it, so the
    /// found/incompatible/not-found branches are exercised without a real Fusion install.
    fn fake_dbt_on_path(dir: &std::path::Path, script_body: &str) {
        let script = dir.join(if cfg!(windows) { "dbt.bat" } else { "dbt" });
        let mut file = std::fs::File::create(&script).unwrap();
        write!(file, "{script_body}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
    }

    /// Serializes every test that mutates the process-global `PATH` env var — `cargo test`
    /// runs tests on multiple threads by default, and env vars are process-wide, so without
    /// this a test running concurrently could see another test's temporary `PATH`.
    static PATH_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_path<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = PATH_MUTEX.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let original = std::env::var_os("PATH");
        std::env::set_var("PATH", dir);
        let result = f();
        match original {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        result
    }

    /// A unique scratch dir under the system temp dir, cleaned up on drop — avoids pulling in
    /// a `tempfile` dependency for what `specs.rs`'s own tests already do by hand.
    struct ScratchDir(PathBuf);
    impl ScratchDir {
        fn new(tag: &str) -> Self {
            // NB: this becomes a single-entry `PATH` value in `with_path` below, so it must
            // not contain the platform `PATH` separator (`:` on Unix, `;` on Windows) or
            // anything else that would make `PATH` parse as multiple garbage entries — hence
            // nanos-since-epoch rather than `{:?}`-formatting a `SystemTime` (which includes
            // literal colons).
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("lsp-provisioning-test-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[cfg(unix)]
    fn discovers_not_found_when_dbt_absent_from_path() {
        let dir = ScratchDir::new("not-found");
        let diag = with_path(dir.path(), || DbtFusionProvisioner::new().diagnose());
        assert_eq!(diag.status, InstallStatus::NotFound);
        assert!(diag.can_acquire);
        assert!(diag.message.contains("Fusion"));
    }

    #[test]
    #[cfg(unix)]
    fn discovers_compatible_fusion() {
        let dir = ScratchDir::new("compatible");
        fake_dbt_on_path(
            dir.path(),
            "#!/bin/sh\necho '{\"version\": \"2.0.0-preview.95\"}'\n",
        );
        let diag = with_path(dir.path(), || DbtFusionProvisioner::new().diagnose());
        match &diag.status {
            InstallStatus::Found { version, .. } => assert_eq!(version, "2.0.0-preview.95"),
            other => panic!("expected Found, got {other:?}"),
        }
        assert!(!diag.can_acquire);
        let cmd = DbtFusionProvisioner::new().resolve_launch_command(&diag).unwrap();
        assert!(cmd[0].ends_with("dbt"));
    }

    #[test]
    #[cfg(unix)]
    fn discovers_too_old_fusion_as_incompatible_not_not_found() {
        let dir = ScratchDir::new("too-old");
        fake_dbt_on_path(
            dir.path(),
            "#!/bin/sh\necho '{\"version\": \"2.0.0-preview.10\"}'\n",
        );
        let diag = with_path(dir.path(), || DbtFusionProvisioner::new().diagnose());
        match &diag.status {
            InstallStatus::FoundIncompatible { reason, .. } => {
                assert!(reason.contains("older than the pinned minimum"))
            }
            other => panic!("expected FoundIncompatible, got {other:?}"),
        }
        assert!(diag.can_acquire);
        assert!(DbtFusionProvisioner::new().resolve_launch_command(&diag).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn discovers_legacy_core_as_incompatible_with_a_distinct_reason() {
        let dir = ScratchDir::new("legacy-core");
        // Legacy Python dbt-core doesn't understand --format json; simulate its failure mode.
        fake_dbt_on_path(dir.path(), "#!/bin/sh\necho 'installed: 1.7.0' >&2\nexit 1\n");
        let diag = with_path(dir.path(), || DbtFusionProvisioner::new().diagnose());
        match &diag.status {
            InstallStatus::FoundIncompatible { reason, .. } => assert!(reason.contains("legacy dbt-core")),
            other => panic!("expected FoundIncompatible, got {other:?}"),
        }
    }

    #[test]
    fn acquire_refuses_without_explicit_consent() {
        let mut events = Vec::new();
        let result = DbtFusionProvisioner::new().acquire(
            AcquisitionConsent::default(),
            &mut |ev| events.push(ev),
            &CancellationFlag::new(),
        );
        assert_eq!(result, Err(AcquisitionError::ConsentRequired));
        assert!(events.is_empty(), "must not do or report ANY work without consent");
    }

    #[test]
    fn acquire_honours_pre_set_cancellation_before_touching_the_network() {
        let cancel = CancellationFlag::new();
        cancel.cancel();
        let mut events = Vec::new();
        let result = DbtFusionProvisioner::new().acquire(
            AcquisitionConsent { explicit: true },
            &mut |ev| events.push(ev),
            &cancel,
        );
        assert_eq!(result, Ok(AcquisitionOutcome::Cancelled));
        // Started is fine to report (it's not "work"); RunningInstaller must not appear.
        assert!(!events.iter().any(|e| matches!(e, AcquisitionEvent::RunningInstaller { .. })));
    }
}
