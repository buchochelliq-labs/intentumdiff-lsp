//! Acceptance-criteria integration coverage for dbt-sql/dbt-yaml:
//! - the missing-server case produces an actionable diagnostic instead of a raw spawn error;
//! - a genuine subprocess, real stdio pipes, real `initialize` + hover hand-off works end to
//!   end for both languages, using the `fake-dbt-lsp` fixture built alongside this crate
//!   (see its module docs for exactly what "real" does and doesn't mean here — CI has no
//!   dbt Fusion install and no network path to dbt Labs' CDN, so this is the closest
//!   equivalent that doesn't depend on either).
//!
//! An `#[ignore]`d companion test drives the SAME exchange against whatever `dbt` a
//! developer actually has on `PATH`, for real validation outside CI:
//! `cargo test --manifest-path crates/lsp-provisioning/Cargo.toml -- --ignored real_dbt`

use std::path::PathBuf;
use std::time::Duration;

use intentumdiff_lsp_client::{enrich_targets, HoverTarget, LspClient};
use intentumdiff_lsp_provisioning::dbt_fusion::DbtFusionProvisioner;
use intentumdiff_lsp_provisioning::{InstallStatus, ServerProvisioner};

/// Serializes every test in this file that mutates the process-global `PATH` — see the same
/// rationale in `src/dbt_fusion.rs`'s unit tests.
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

struct ScratchDir(PathBuf);
impl ScratchDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("dbt-enrichment-test-{tag}-{}-{nanos}", std::process::id()));
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
fn missing_server_is_an_actionable_diagnostic_not_a_raw_spawn_error() {
    // Acceptance: "Discover an existing installation and provide actionable setup
    // diagnostics when absent." An empty PATH must never surface as a bare OS "No such file
    // or directory" — the caller gets a `Diagnostic` to show the user *before* ever
    // attempting a spawn.
    let empty = ScratchDir::new("missing");
    let diagnostic = with_path(empty.path(), || DbtFusionProvisioner::new().diagnose());

    assert_eq!(diagnostic.status, InstallStatus::NotFound);
    assert!(diagnostic.can_acquire, "a missing install must offer a next step, not a dead end");
    assert!(
        diagnostic.message.to_lowercase().contains("fusion"),
        "message should explain WHAT to install, not just that something is missing: {}",
        diagnostic.message
    );
    assert!(
        diagnostic.doc_url.contains("docs.getdbt.com"),
        "must point at current upstream docs, not the stale install-dbt-extension link"
    );
    // No launch command is offered when there's nothing to launch.
    assert!(DbtFusionProvisioner::new().resolve_launch_command(&diagnostic).is_none());
}

/// Places a copy of the `fake-dbt-lsp` fixture binary at `<dir>/dbt` so discovery and the
/// subsequent stdio launch both resolve to it, exactly as they would a real Fusion install.
#[cfg(unix)]
fn install_fake_dbt(dir: &std::path::Path) -> PathBuf {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_fake-dbt-lsp"));
    let dest = dir.join("dbt");
    std::fs::copy(&fixture, &dest).expect("copy fake-dbt-lsp fixture into place");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).unwrap();
    }
    dest
}

/// Drives one real `initialize` → `didOpen` → hover(s) → `didClose` → `shutdown` exchange
/// against `command`, over a genuine OS subprocess and real stdio pipes, for the given
/// language id. Shared by both the fixture-backed test and the `#[ignore]`d real-dbt test.
async fn run_enrichment_exchange(command: &[String], language_id: &str) {
    let client = LspClient::connect_stdio(command, None, "file:///dbt-project", Duration::from_secs(5))
        .await
        .unwrap_or_else(|e| panic!("connect_stdio failed for {command:?}: {e}"));

    let targets =
        vec![HoverTarget { id: "col-0".into(), line: 0, col: 0 }];
    let uri = format!("file:///dbt-project/models/orders.{}", if language_id == "dbt-yaml" { "yml" } else { "sql" });
    let map = enrich_targets(&client, &uri, language_id, "select 1 as order_id\n", &targets)
        .await
        .expect("enrichment transport should not error even if the server has nothing to say");

    assert_eq!(map.len(), 1, "expected exactly one hover hit, got {map:?}");
    assert!(
        map["col-0"].contains("varchar"),
        "expected the fixture's deterministic hover payload, got {:?}",
        map["col-0"]
    );

    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
#[cfg(unix)]
async fn real_subprocess_initialize_and_enrichment_exchange_dbt_sql() {
    let dir = ScratchDir::new("real-exchange-sql");
    let dbt = install_fake_dbt(dir.path());
    // Mirrors what `DbtFusionProvisioner::resolve_launch_command` would hand back for a
    // `Found` diagnostic — see the STDIO_COMMAND_CANDIDATE caveat in dbt_fusion.rs.
    let command = vec![dbt.to_string_lossy().into_owned(), "lsp-server".into(), "--stdio".into()];
    run_enrichment_exchange(&command, "dbt-sql").await;
}

#[tokio::test]
#[cfg(unix)]
async fn real_subprocess_initialize_and_enrichment_exchange_dbt_yaml() {
    let dir = ScratchDir::new("real-exchange-yaml");
    let dbt = install_fake_dbt(dir.path());
    let command = vec![dbt.to_string_lossy().into_owned(), "lsp-server".into(), "--stdio".into()];
    run_enrichment_exchange(&command, "dbt-yaml").await;
}

/// Ties discovery directly to the launch, the way a real caller would: diagnose(), require
/// `Found`, resolve_launch_command(), then actually spawn that exact command. Exercises the
/// seam between `lsp-provisioning` and `lsp-client` rather than either in isolation.
#[tokio::test]
#[cfg(unix)]
async fn discovery_resolved_command_is_actually_launchable() {
    let dir = ScratchDir::new("discovery-to-launch");
    install_fake_dbt(dir.path());
    let provisioner = DbtFusionProvisioner::new();

    let diagnostic = with_path(dir.path(), || provisioner.diagnose());
    let InstallStatus::Found { .. } = &diagnostic.status else {
        panic!("expected Found, got {:?}", diagnostic.status);
    };
    let command = provisioner
        .resolve_launch_command(&diagnostic)
        .expect("a Found diagnostic must resolve to a launch command");

    run_enrichment_exchange(&command, "dbt-sql").await;
}

/// Real-world validation, opt-in only: drives the identical exchange against whatever `dbt`
/// the developer actually has on `PATH`. Skipped (not failed) when none is found or it's
/// incompatible, since CI intentionally never runs this. This is the test that would catch
/// `STDIO_COMMAND_CANDIDATE` being wrong against an actual Fusion binary — run it by hand
/// before relying on that constant.
#[tokio::test]
#[ignore = "requires a real dbt Fusion install; run explicitly with -- --ignored"]
async fn real_dbt_initialize_and_enrichment_exchange_if_available() {
    let provisioner = DbtFusionProvisioner::new();
    let diagnostic = provisioner.diagnose();
    let InstallStatus::Found { .. } = &diagnostic.status else {
        eprintln!("SKIPPED: no compatible dbt Fusion install found ({:?})", diagnostic.status);
        return;
    };
    let command = provisioner.resolve_launch_command(&diagnostic).unwrap();
    run_enrichment_exchange(&command, "dbt-sql").await;
}
