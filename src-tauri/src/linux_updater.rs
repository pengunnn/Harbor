use base64::Engine;
use serde::Serialize;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::time::Duration;
use tauri::{ipc::Channel, Manager, Resource, ResourceId};
use tauri_plugin_updater::Update;
use tokio_util::sync::CancellationToken;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::task::AbortOnDropHandle;

struct DeltaSession {
    update: Update,
    directory: tempfile::TempDir,
    public_key: minisign_verify::PublicKey,
    signature: minisign_verify::Signature,
    cancel: CancellationToken,
    started: AtomicBool,
    verified: AtomicBool,
}

struct DeltaSessionResource {
    session: Arc<DeltaSession>,
}

impl Resource for DeltaSessionResource {
    fn close(self: Arc<Self>) {
        self.session.cancel.cancel();
    }
}

impl Drop for DeltaSessionResource {
    fn drop(&mut self) {
        self.session.cancel.cancel();
    }
}

impl Drop for DeltaSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionInfo {
    rid: ResourceId,
    version: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeltaPhase {
    Reconstructing,
    Verifying,
    Ready,
}

#[derive(Debug, Serialize)]
pub struct DeltaError {
    kind: &'static str,
    message: String,
}

impl DeltaError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn cancelled() -> Self {
        Self::new("cancelled", "The delta update was cancelled.")
    }
}

fn decode_text(value: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|_| "Invalid signing metadata encoding.".to_string())?;

    String::from_utf8(bytes)
        .map_err(|_| "Invalid signing metadata text.".to_string())
}

pub(crate) fn create_session(
    app: &tauri::AppHandle,
    webview: &tauri::Webview,
    update_rid: ResourceId,
) -> Result<SessionInfo, String> {
    if webview.label() != "main" {
        return Err("Delta updates are only available from the main window.".into());
    }

    if crate::update_platform::updater_package_kind(app.clone()) != "appimage" {
        return Err("Delta updates require an AppImage installation.".into());
    }

    if !crate::update_platform::updater_delta_available(app.clone())? {
        return Err("This AppImage does not contain the delta helper.".into());
    }

    let update = webview
        .resources_table()
        .get::<Update>(update_rid)
        .map_err(|_| "The checked update is no longer available.".to_string())?;

    let url = &update.download_url;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(
            "Delta updates require a direct HTTPS asset URL without credentials, queries, or fragments."
                .into(),
        );
    }

    if !url.path().ends_with(".AppImage") {
        return Err("Delta updates currently require a direct AppImage asset.".into());
    }

    let configured_key = app
        .config()
        .plugins
        .0
        .get("updater")
        .and_then(|value| value.get("pubkey"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| "This build carries no updater public key.".to_string())?;

    let public_key =
        minisign_verify::PublicKey::decode(decode_text(configured_key)?.trim())
            .map_err(|_| "The bundled updater public key is invalid.".to_string())?;

    let signature =
        minisign_verify::Signature::decode(decode_text(&update.signature)?.trim())
            .map_err(|_| "The update signature is invalid.".to_string())?;

    // Reject incompatible signing metadata before downloading a large file.
    // This is not verification of the AppImage itself.
    public_key
        .verify_stream(&signature)
        .map_err(|_| {
            "The delta update requires a compatible streaming signature for this build's key."
                .to_string()
        })?;

    // Avoid assuming /tmp is disk-backed on memory-constrained machines.
    let cache = app
        .path()
        .app_cache_dir()
        .map_err(|error| format!("Resolve update cache: {error}"))?;

    std::fs::create_dir_all(&cache)
        .map_err(|error| format!("Create update cache: {error}"))?;

    let directory = tempfile::Builder::new()
        .prefix("harbor-delta-")
        .tempdir_in(cache)
        .map_err(|error| format!("Create update workspace: {error}"))?;

    let version = update.version.clone();
    let session = DeltaSession {
        update: (*update).clone(),
        directory,
        public_key,
        signature,
        cancel: CancellationToken::new(),
        started: AtomicBool::new(false),
        verified: AtomicBool::new(false),
    };

    let rid = webview.resources_table().add(DeltaSessionResource {
        session: Arc::new(session),
    });

    Ok(SessionInfo { rid, version })
}

// The helper gets its own process group so cancellation can stop its children too.
struct HelperGroup(Option<libc::pid_t>);

impl HelperGroup {
    fn terminate(&mut self) {
        if let Some(pid) = self.0.take() {
            // SAFETY: pid is the positive PID returned for our own spawned
            // helper, whose process-group ID was explicitly set to that PID.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for HelperGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn emit_phase(
    session: &DeltaSession,
    on_event: &Channel<DeltaPhase>,
    phase: DeltaPhase,
) -> Result<(), DeltaError> {
    if session.cancel.is_cancelled() {
        return Err(DeltaError::cancelled());
    }

    on_event.send(phase).map_err(|_| {
        session.cancel.cancel();
        DeltaError::cancelled()
    })
}

fn verify_reconstructed_file(session: &DeltaSession) -> Result<(), DeltaError> {
    let path = session.directory.path().join("update.AppImage");
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| DeltaError::new("transfer", "The reconstructed AppImage is missing."))?;

    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return Err(DeltaError::new(
            "transfer",
            "The reconstructed AppImage is not a nonempty regular file.",
        ));
    }

    let mut verifier = session
        .public_key
        .verify_stream(&session.signature)
        .map_err(|_| {
            DeltaError::new("verification", "The update signature cannot be verified.")
        })?;

    let mut file = std::fs::File::open(path)
        .map_err(|_| DeltaError::new("storage", "Cannot open the reconstructed AppImage."))?;

    let mut buffer = vec![0_u8; 1024 * 1024];

    loop {
        if session.cancel.is_cancelled() {
            return Err(DeltaError::cancelled());
        }

        let count = file
            .read(&mut buffer)
            .map_err(|_| DeltaError::new("storage", "Cannot read the reconstructed AppImage."))?;

        if count == 0 {
            break;
        }

        verifier.update(&buffer[..count]);
    }

    verifier.finalize().map_err(|_| {
        DeltaError::new(
            "verification",
            "The reconstructed AppImage failed signature verification. It will not be installed.",
        )
    })
}

async fn capture_helper_tail<R: AsyncRead + Unpin>(
    reader: Option<R>,
) -> Vec<u8> {
    const LIMIT: usize = 8 * 1024;
    let Some(mut reader) = reader else {
        return Vec::new();
    };

    let mut tail = Vec::with_capacity(LIMIT);
    let mut buffer = [0_u8; 2048];

    loop {
        let count = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };

        let excess = (tail.len() + count).saturating_sub(LIMIT);
        if excess > 0 {
            tail.drain(..excess);
        }
        tail.extend_from_slice(&buffer[..count]);
    }

    tail
}

async fn finish_helper_tail(
    mut task: AbortOnDropHandle<Vec<u8>>,
) -> Vec<u8> {
    // Do not hang if a descendant unexpectedly keeps a pipe open.
    match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
        Ok(Ok(tail)) => tail,
        _ => Vec::new(),
    }
}

fn helper_failure_hint(stdout: &[u8], stderr: &[u8]) -> &'static str {
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr),
    )
    .to_ascii_lowercase();

    if output.contains("no space left on device") {
        "The helper reported insufficient disk space."
    } else if output.contains("error while loading shared libraries") {
        "The helper reported a missing runtime library."
    } else if output.contains("permission denied") {
        "The helper reported a permission problem."
    } else if output.contains("could not resolve host")
        || output.contains("couldn't resolve host")
        || output.contains("host not found")
    {
        "The helper reported a hostname-resolution problem."
    } else if output.contains("certificate verify failed")
        || output.contains("ssl certificate problem")
        || output.contains("ssl handshake failed")
    {
        "The helper reported a TLS certificate or handshake problem."
    } else {
        "The helper could not reconstruct the update."
    }
}

async fn wait_for_helper(
    child: &mut tokio::process::Child,
    group: &mut HelperGroup,
    cancel: &CancellationToken,
    limit: Duration,
) -> Result<std::process::ExitStatus, DeltaError> {
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(DeltaError::cancelled()),
        result = tokio::time::timeout(limit, child.wait()) => {
            match result {
                Ok(Ok(status)) => Ok(status),
                Ok(Err(_)) => Err(DeltaError::new(
                    "helper",
                    "Could not wait for the delta helper.",
                )),
                Err(_) => Err(DeltaError::new(
                    "transfer",
                    "The delta download exceeded its time limit.",
                )),
            }
        }
    };

    match outcome {
        Ok(status) => {
            group.disarm();
            Ok(status)
        }
        Err(error) => {
            group.terminate();
            let _ = child.wait().await;
            Err(error)
        }
    }
}

async fn reconstruct(
    session: Arc<DeltaSession>,
    helper: PathBuf,
    seed: PathBuf,
    on_event: &Channel<DeltaPhase>,
) -> Result<(), DeltaError> {
    emit_phase(&session, on_event, DeltaPhase::Reconstructing)?;

    let mut metadata_url = session.update.download_url.clone();
    metadata_url.set_path(&format!("{}.zsync", session.update.download_url.path()));

    let output = session.directory.path().join("update.AppImage");
    let mut command = tokio::process::Command::new(helper);

    command
        .arg("--force-update")
        .arg("-i")
        .arg(seed)
        .arg("-o")
        .arg(output)
        .arg("-u")
        .arg(session.update.download_url.as_str())
        .arg(metadata_url.as_str())
        .current_dir(session.directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Do not make the extracted helper inherit Harbor's AppImage identity
    // or loader overrides. Its AppRun establishes its own runtime paths.
    for name in ["APPIMAGE", "APPDIR", "ARGV0", "OWD", "LD_LIBRARY_PATH", "LD_PRELOAD"] {
        command.env_remove(name);
    }

    command.as_std_mut().process_group(0);

    let mut child = command
        .spawn()
        .map_err(|_| DeltaError::new("helper", "The bundled delta helper could not start."))?;

    let pid = child
        .id()
        .ok_or_else(|| DeltaError::new("helper", "The delta helper has no process ID."))?;

    let mut group = HelperGroup(Some(pid as libc::pid_t));

    let stdout_task = AbortOnDropHandle::new(tokio::spawn(
        capture_helper_tail(child.stdout.take()),
    ));
    let stderr_task = AbortOnDropHandle::new(tokio::spawn(
        capture_helper_tail(child.stderr.take()),
    ));

    let status = wait_for_helper(
        &mut child,
        &mut group,
        &session.cancel,
        Duration::from_secs(90 * 60),
    )
    .await?;

    let (stdout_tail, stderr_tail) = tokio::join!(
        finish_helper_tail(stdout_task),
        finish_helper_tail(stderr_task),
    );

    if session.cancel.is_cancelled() {
        return Err(DeltaError::cancelled());
    }

    if !status.success() {
        let hint = helper_failure_hint(&stdout_tail, &stderr_tail);
        let exit = match status.code() {
            Some(code) => format!("Exit code: {code}."),
            None => "The helper terminated without an exit code.".to_string(),
        };

        return Err(DeltaError::new(
            "transfer",
            format!("{hint} {exit}"),
        ));
    }

    emit_phase(&session, on_event, DeltaPhase::Verifying)?;

    let verification_session = Arc::clone(&session);
    tokio::task::spawn_blocking(move || {
        verify_reconstructed_file(&verification_session)
    })
    .await
    .map_err(|_| DeltaError::new("internal", "The verification task failed."))??;

    if session.cancel.is_cancelled() {
        return Err(DeltaError::cancelled());
    }

    session.verified.store(true, Ordering::Release);
    emit_phase(&session, on_event, DeltaPhase::Ready)?;

    Ok(())
}

pub(crate) async fn download_session(
    app: tauri::AppHandle,
    webview: tauri::Webview,
    session_rid: ResourceId,
    on_event: Channel<DeltaPhase>,
) -> Result<(), DeltaError> {
    if webview.label() != "main" {
        return Err(DeltaError::new(
            "state",
            "Delta updates are only available from the main window.",
        ));
    }

    // Do not retain the wrapper while working: its destruction must be able
    // to signal cancellation independently of the worker's session reference.
    let session = {
        let resource = webview
            .resources_table()
            .get::<DeltaSessionResource>(session_rid)
            .map_err(|_| DeltaError::new("state", "The delta session is no longer available."))?;

        Arc::clone(&resource.session)
    };

    if session.started.swap(true, Ordering::AcqRel) {
        return Err(DeltaError::new(
            "state",
            "This delta session has already been used.",
        ));
    }

    let result = async {
        let seed = app
            .env()
            .appimage
            .clone()
            .ok_or_else(|| DeltaError::new("state", "The running AppImage is unavailable."))?;

        let seed = PathBuf::from(seed);
        if !seed.is_absolute() || !seed.is_file() {
            return Err(DeltaError::new("state", "The running AppImage cannot be used as a seed."));
        }

        let helper = app
            .path()
            .resolve(
                "linux-updater/zsync2/AppRun",
                tauri::path::BaseDirectory::Resource,
            )
            .map_err(|_| DeltaError::new("helper", "Cannot resolve the bundled delta helper."))?;

        reconstruct(Arc::clone(&session), helper, seed, &on_event).await
    }
    .await;

    if result.is_err() {
        session.cancel.cancel();
        let _ = webview.resources_table().close(session_rid);
    }

    result
}

// Process-wide: only one installation may run, including across sessions.
const INSTALL_IDLE: u8 = 0;
const INSTALL_RUNNING: u8 = 1;
const INSTALL_COMPLETE: u8 = 2;

static INSTALL_STATE: AtomicU8 = AtomicU8::new(INSTALL_IDLE);

struct InstallGuard {
    completed: bool,
}

impl InstallGuard {
    fn acquire() -> Result<Self, DeltaError> {
        INSTALL_STATE
            .compare_exchange(
                INSTALL_IDLE,
                INSTALL_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|state| {
                let message = if state == INSTALL_COMPLETE {
                    "An update has already been installed. Restart Harbor."
                } else {
                    "Another update installation is already running."
                };
                DeltaError::new("state", message)
            })?;

        Ok(Self { completed: false })
    }

    fn complete(&mut self) {
        self.completed = true;
        INSTALL_STATE.store(INSTALL_COMPLETE, Ordering::Release);
    }
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        if !self.completed {
            INSTALL_STATE.store(INSTALL_IDLE, Ordering::Release);
        }
    }
}

fn read_verified_install_bytes(
    path: &std::path::Path,
    public_key: &minisign_verify::PublicKey,
    signature: &minisign_verify::Signature,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, DeltaError> {
    if cancel.is_cancelled() {
        return Err(DeltaError::cancelled());
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| {
            DeltaError::new("storage", "Cannot open the staged AppImage for installation.")
        })?;

    let metadata = file.metadata().map_err(|_| {
        DeltaError::new("storage", "Cannot inspect the staged AppImage.")
    })?;

    if !metadata.is_file() || metadata.len() == 0 {
        return Err(DeltaError::new(
            "storage",
            "The staged AppImage is not a nonempty regular file.",
        ));
    }

    let expected = usize::try_from(metadata.len()).map_err(|_| {
        DeltaError::new("storage", "The staged AppImage is too large to load.")
    })?;

    let mut bytes = Vec::new();
    bytes.try_reserve_exact(expected).map_err(|_| {
        DeltaError::new("memory", "Not enough memory to prepare the update for installation.")
    })?;

    let mut buffer = vec![0_u8; 1024 * 1024];

    loop {
        if cancel.is_cancelled() {
            return Err(DeltaError::cancelled());
        }

        let count = file.read(&mut buffer).map_err(|_| {
            DeltaError::new("storage", "Cannot read the staged AppImage.")
        })?;

        if count == 0 {
            break;
        }

        if count > expected.saturating_sub(bytes.len()) {
            return Err(DeltaError::new(
                "verification",
                "The staged AppImage changed while being read.",
            ));
        }

        bytes.extend_from_slice(&buffer[..count]);
    }

    if bytes.len() != expected {
        return Err(DeltaError::new(
            "verification",
            "The staged AppImage changed while being read.",
        ));
    }

    public_key
        .verify(&bytes, signature, true)
        .map_err(|_| {
            DeltaError::new(
                "verification",
                "The staged AppImage failed its final signature check. Installation was blocked.",
            )
        })?;

    Ok(bytes)
}

fn ensure_install_ready(
    verified: bool,
    cancel: &CancellationToken,
) -> Result<(), DeltaError> {
    if cancel.is_cancelled() {
        return Err(DeltaError::cancelled());
    }

    if !verified {
        return Err(DeltaError::new(
            "state",
            "The delta update has not completed verification.",
        ));
    }

    Ok(())
}

pub(crate) async fn install_session(
    webview: tauri::Webview,
    session_rid: ResourceId,
) -> Result<(), DeltaError> {
    if webview.label() != "main" {
        return Err(DeltaError::new(
            "state",
            "Delta updates are only available from the main window.",
        ));
    }

    let session = {
        let resource = webview
            .resources_table()
            .get::<DeltaSessionResource>(session_rid)
            .map_err(|_| {
                DeltaError::new("state", "The delta session is no longer available.")
            })?;

        Arc::clone(&resource.session)
    };

    ensure_install_ready(
        session.verified.load(Ordering::Acquire),
        &session.cancel,
    )?;

    let mut guard = InstallGuard::acquire()?;

    let result = tokio::task::spawn_blocking(move || {
        let bytes = read_verified_install_bytes(
            &session.directory.path().join("update.AppImage"),
            &session.public_key,
            &session.signature,
            &session.cancel,
        )?;

        if session.cancel.is_cancelled() {
            return Err(DeltaError::cancelled());
        }

        // Installation is the commit point. Do not interrupt file replacement
        // after handing the verified bytes to Tauri.
        session.update.install(&bytes).map_err(|_| {
            DeltaError::new(
                "installation",
                "Tauri could not replace the AppImage. Check storage and permissions before retrying.",
            )
        })?;

        session.verified.store(false, Ordering::Release);
        guard.complete();

        Ok::<(), DeltaError>(())
    })
    .await
    .map_err(|_| {
        DeltaError::new("internal", "The installation task failed.")
    })?;

    if result.is_ok() {
        let _ = webview.resources_table().close(session_rid);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn helper_output_keeps_only_the_last_eight_kibibytes() {
        let input: Vec<u8> = (0..30_000)
            .map(|index| (index % 251) as u8)
            .collect();

        let tail = capture_helper_tail(Some(input.as_slice())).await;

        assert_eq!(tail.len(), 8 * 1024);
        assert_eq!(tail.as_slice(), &input[input.len() - 8 * 1024..]);
    }

    #[tokio::test]
    async fn helper_output_preserves_short_output() {
        let input = b"short diagnostic";
        let tail = capture_helper_tail(Some(input.as_slice())).await;

        assert_eq!(tail.as_slice(), input);
    }

    #[tokio::test]
    async fn missing_helper_output_is_empty() {
        let tail = capture_helper_tail::<tokio::io::Empty>(None).await;
        assert!(tail.is_empty());
    }

    #[test]
    fn diagnostic_hint_does_not_expose_raw_output() {
        let raw = b"Permission denied: https://example.invalid/update?token=TEST_SECRET";

        assert_eq!(
            helper_failure_hint(b"", raw),
            "The helper reported a permission problem.",
        );

        assert_eq!(
            helper_failure_hint(b"unknown error TEST_SECRET", b""),
            "The helper could not reconstruct the update.",
        );
    }

    #[test]
    fn install_guard_blocks_overlap_allows_retry_and_blocks_after_success() {
        struct ResetState;

        impl Drop for ResetState {
            fn drop(&mut self) {
                INSTALL_STATE.store(INSTALL_IDLE, Ordering::Release);
            }
        }

        let _reset = ResetState;
        assert_eq!(INSTALL_STATE.load(Ordering::Acquire), INSTALL_IDLE);

        let first = InstallGuard::acquire().expect("first installation lock");

        let competing_attempt = std::thread::spawn(|| {
            InstallGuard::acquire().is_err()
        });

        assert!(
            competing_attempt.join().expect("competing thread"),
            "a concurrent installation must be rejected",
        );

        // Leaving without success must release the lock.
        drop(first);
        assert_eq!(INSTALL_STATE.load(Ordering::Acquire), INSTALL_IDLE);

        let mut retry = InstallGuard::acquire().expect("retry installation lock");
        retry.complete();
        drop(retry);

        assert_eq!(
            INSTALL_STATE.load(Ordering::Acquire),
            INSTALL_COMPLETE,
        );

        assert!(
            InstallGuard::acquire().is_err(),
            "a completed installation must require a restart",
        );
    }
    // Public prehashed test vector from minisign-verify's own tests.
    // This key is a test fixture, never the application's updater key.
    fn signing_fixture() -> (
        minisign_verify::PublicKey,
        minisign_verify::Signature,
    ) {
        let key = minisign_verify::PublicKey::from_base64(
            "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3",
        )
        .expect("public fixture key");

        let signature = minisign_verify::Signature::decode(concat!(
            "untrusted comment: signature from minisign secret key\n",
            "RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/",
            "z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n",
            "trusted comment: timestamp:1556193335\tfile:test\n",
            "y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/",
            "17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==",
        ))
        .expect("public fixture signature");

        (key, signature)
    }

    #[test]
    fn final_verification_accepts_valid_signed_bytes() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("fixture.AppImage");
        std::fs::write(&path, b"test").expect("write fixture");

        let (key, signature) = signing_fixture();
        let cancel = CancellationToken::new();

        let bytes = read_verified_install_bytes(
            &path, &key, &signature, &cancel,
        )
        .expect("valid signed fixture");

        assert_eq!(bytes, b"test");
    }

    #[test]
    fn final_verification_rejects_changed_truncated_and_extended_bytes() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("fixture.AppImage");
        let (key, signature) = signing_fixture();
        let cancel = CancellationToken::new();

        for contents in [
            b"Test".as_slice(),
            b"tes".as_slice(),
            b"test!".as_slice(),
        ] {
            std::fs::write(&path, contents).expect("write altered fixture");

            let error = read_verified_install_bytes(
                &path, &key, &signature, &cancel,
            )
            .expect_err("altered bytes must be rejected");

            assert_eq!(error.kind, "verification");
        }
    }

    #[test]
    fn cancelled_verification_stops_before_opening_the_file() {
        let directory = tempfile::tempdir().expect("test directory");
        let missing_path = directory.path().join("does-not-exist.AppImage");
        let (key, signature) = signing_fixture();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = read_verified_install_bytes(
            &missing_path, &key, &signature, &cancel,
        )
        .expect_err("cancelled operation must stop");

        // A file-open attempt would instead produce a storage error.
        assert_eq!(error.kind, "cancelled");
        assert!(!missing_path.exists());
    }

    #[test]
    fn final_verification_rejects_a_symlink() {
        let directory = tempfile::tempdir().expect("test directory");
        let target = directory.path().join("signed-file");
        let link = directory.path().join("fixture.AppImage");

        std::fs::write(&target, b"test").expect("write fixture");
        std::os::unix::fs::symlink(&target, &link).expect("create test symlink");

        let (key, signature) = signing_fixture();
        let cancel = CancellationToken::new();

        let error = read_verified_install_bytes(
            &link, &key, &signature, &cancel,
        )
        .expect_err("symlink must be rejected");

        assert_eq!(error.kind, "storage");
        assert_eq!(std::fs::read(&target).expect("read fixture"), b"test");
    }

    fn sleeping_helper() -> (tokio::process::Child, HelperGroup) {
        let mut command = tokio::process::Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        command.as_std_mut().process_group(0);

        let child = command.spawn().expect("start test sleep process");
        let pid = child.id().expect("test process ID");

        (child, HelperGroup(Some(pid as libc::pid_t)))
    }

    #[tokio::test]
    async fn cancellation_terminates_and_reaps_running_helper() {
        use std::os::unix::process::ExitStatusExt;

        let (mut child, mut group) = sleeping_helper();
        let cancel = CancellationToken::new();

        let operation = async {
            let trigger = async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                cancel.cancel();
            };

            let (result, ()) = tokio::join!(
                wait_for_helper(
                    &mut child,
                    &mut group,
                    &cancel,
                    Duration::from_secs(30),
                ),
                trigger,
            );

            result
        };

        let error = tokio::time::timeout(Duration::from_secs(5), operation)
            .await
            .expect("cancellation must finish promptly")
            .expect_err("cancelled helper must not succeed");

        assert_eq!(error.kind, "cancelled");

        let status = child
            .try_wait()
            .expect("inspect child")
            .expect("child must have exited");

        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[tokio::test]
    async fn timeout_terminates_and_reaps_running_helper() {
        use std::os::unix::process::ExitStatusExt;

        let (mut child, mut group) = sleeping_helper();
        let cancel = CancellationToken::new();

        let error = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_helper(
                &mut child,
                &mut group,
                &cancel,
                Duration::from_millis(50),
            ),
        )
        .await
        .expect("timeout cleanup must finish promptly")
        .expect_err("timed-out helper must not succeed");

        assert_eq!(error.kind, "transfer");
        assert!(error.message.contains("time limit"));

        let status = child
            .try_wait()
            .expect("inspect child")
            .expect("child must have exited");

        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn installation_requires_verification_and_an_uncancelled_session() {
        let cancel = CancellationToken::new();

        let error = ensure_install_ready(false, &cancel)
            .expect_err("unverified installation must be rejected");
        assert_eq!(error.kind, "state");

        ensure_install_ready(true, &cancel)
            .expect("verified, uncancelled session passes the eligibility check");

        cancel.cancel();

        let error = ensure_install_ready(true, &cancel)
            .expect_err("cancellation must override verified status");
        assert_eq!(error.kind, "cancelled");
    }
}
