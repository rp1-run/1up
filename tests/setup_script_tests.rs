//! Black-box integration tests for `scripts/install/setup.sh`.
//!
//! These tests exercise the full install flow against a local in-process
//! HTTP fixture standing in for `github.com`. `curl` requests to GitHub
//! URLs are intercepted by a tiny wrapper script placed first on `PATH`
//! that rewrites the host to `http://127.0.0.1:<port>`. setup.sh itself
//! is not modified; only its environment is.
//!
//! Matrix coverage (REQ-001..014 via feature task T4):
//!   happy_path, idempotent_re_run, checksum_mismatch,
//!   missing_sha256sums_warn, unsupported_platform, pinned_version,
//!   pinned_version_missing, custom_install_dir.
//!
//! Tests are gated on Unix because setup.sh is a POSIX bash script and the
//! whole surface is out-of-scope for Windows per requirements §4.2.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

const FIXTURE_REPO: &str = "rp1-run/1up";
const FIXTURE_TAG: &str = "v9.9.9";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn setup_script() -> PathBuf {
    repo_root().join("scripts").join("install").join("setup.sh")
}

/// Target triple setup.sh will detect for the host running these tests.
/// `detect_target()` in setup.sh maps (uname -s, uname -m) -> one of four
/// triples; we mirror the same mapping here so the fixture archive we serve
/// matches what the script asks for.
fn host_target() -> &'static str {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "aarch64-apple-darwin"
        } else {
            "x86_64-apple-darwin"
        }
    } else if cfg!(target_os = "linux") {
        if cfg!(target_arch = "aarch64") {
            "aarch64-unknown-linux-gnu"
        } else {
            "x86_64-unknown-linux-gnu"
        }
    } else {
        // Non-Linux/macOS Unix (FreeBSD, etc.) is out of scope for setup.sh.
        "unsupported"
    }
}

/// A pre-built release tree served by the local HTTP fixture.
struct ReleaseFixture {
    /// Filesystem root the HTTP server serves from.
    serve_root: PathBuf,
    /// Archive filename setup.sh will request (e.g. `1up-v9.9.9-...tar.gz`).
    archive_name: String,
}

impl ReleaseFixture {
    /// Build a fixture with an archive whose inner `1up` binary is a shell
    /// stub that prints its first argument. `include_sha256sums` controls
    /// whether the fixture publishes a SHA256SUMS file.
    fn new(tag: &str, target: &str, include_sha256sums: bool) -> Self {
        Self::new_with_binary(tag, target, include_sha256sums, None)
    }

    /// Variant that allows an override to the binary stub body (for the
    /// checksum-mismatch case where we tamper with the archive after the
    /// SHA256SUMS file is written).
    fn new_with_binary(
        tag: &str,
        target: &str,
        include_sha256sums: bool,
        tampered_bytes: Option<&[u8]>,
    ) -> Self {
        let serve_root = tempfile::tempdir().unwrap().keep();

        // Build archive at <serve_root>/repos/<REPO>/releases/download/<tag>/<archive>
        let archive_name = format!("1up-{tag}-{target}.tar.gz");
        let staging = tempfile::tempdir().unwrap();
        let package_dir = staging.path().join(format!("1up-{tag}-{target}"));
        fs::create_dir_all(&package_dir).unwrap();

        // Shell-stub `1up` binary. `--version` prints a matching tag so
        // end-to-end verification can assert the installed binary is the
        // one we shipped.
        let bin_path = package_dir.join("1up");
        let stub = format!(
            "#!/usr/bin/env bash\nif [ \"$1\" = \"--version\" ]; then echo \"1up {tag}\"; else echo \"1up fixture stub\"; fi\n"
        );
        fs::write(&bin_path, stub).unwrap();
        fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755)).unwrap();

        let releases_dir = serve_root
            .join("repos")
            .join(FIXTURE_REPO)
            .join("releases")
            .join("download")
            .join(tag);
        fs::create_dir_all(&releases_dir).unwrap();

        let archive_path = releases_dir.join(&archive_name);
        // tar -czf <archive> -C <staging> <pkg_dir>
        let out = Command::new("tar")
            .arg("-czf")
            .arg(&archive_path)
            .arg("-C")
            .arg(staging.path())
            .arg(format!("1up-{tag}-{target}"))
            .output()
            .expect("tar is available on Unix test hosts");
        assert!(
            out.status.success(),
            "tar failed to build fixture archive: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // SHA256SUMS covers the honest archive content.
        if include_sha256sums {
            let mut hasher = Sha256::new();
            hasher.update(fs::read(&archive_path).unwrap());
            let hex = hex_encode(&hasher.finalize());
            // setup.sh parses lines with a literal two-space separator.
            let sums = format!("{hex}  {archive_name}\n");
            fs::write(releases_dir.join("SHA256SUMS"), sums).unwrap();
        }

        // Tampered-bytes override: replace the archive AFTER SHA256SUMS so
        // the recorded hash no longer matches the served bytes.
        if let Some(bytes) = tampered_bytes {
            fs::write(&archive_path, bytes).unwrap();
        }

        // `/repos/<REPO>/releases/latest` endpoint (GitHub API).
        let api_dir = serve_root
            .join("api")
            .join("repos")
            .join(FIXTURE_REPO)
            .join("releases");
        fs::create_dir_all(&api_dir).unwrap();
        fs::write(
            api_dir.join("latest"),
            format!("{{\"tag_name\":\"{tag}\"}}\n"),
        )
        .unwrap();

        Self {
            serve_root,
            archive_name,
        }
    }
}

/// Minimal HTTP server backed by a temp directory. Only handles GET; keeps
/// the test suite self-contained so no external language runtimes are
/// needed on CI hosts. setup.sh issues requests serially (archive, then
/// SHA256SUMS, optionally API), so a lightweight listener is sufficient.
struct LocalHttp {
    addr: String,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl LocalHttp {
    fn start(root: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = thread::spawn(move || {
            while !shutdown_clone.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let root = root.clone();
                        thread::spawn(move || handle_request(stream, &root));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            shutdown,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for LocalHttp {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_request(mut stream: TcpStream, root: &Path) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    // Drain headers.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }

    // `GET /path HTTP/1.1`
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    let clean = path.trim_start_matches('/');
    let file = root.join(clean);
    match fs::read(&file) {
        Ok(body) => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        }
        Err(_) => {
            let body = b"not found";
            let header = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body);
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Write a curl wrapper at `<dir>/curl` that rewrites GitHub hostnames to
/// the local fixture. setup.sh calls curl with `-fsSL <url>` and either
/// `-o <path>` or stdout; the wrapper only transforms the URL, then
/// delegates to real curl. Placed first on `$PATH` so the test controls
/// the transport without modifying setup.sh.
fn install_curl_wrapper(dir: &Path, base: &str) {
    let wrapper_path = dir.join("curl");
    let real_curl = which("curl");
    let body = format!(
        r#"#!/usr/bin/env bash
set -eu
REAL_CURL='{real_curl}'
BASE='{base}'
args=()
for a in "$@"; do
    case "$a" in
        https://github.com/*/releases/download/*)
            # https://github.com/<repo>/releases/download/<tag>/<asset>
            tail=${{a#https://github.com/}}
            args+=("$BASE/repos/$tail")
            ;;
        https://api.github.com/*)
            tail=${{a#https://api.github.com/}}
            args+=("$BASE/api/$tail")
            ;;
        *)
            args+=("$a")
            ;;
    esac
done
exec "$REAL_CURL" "${{args[@]}}"
"#,
        real_curl = real_curl.display(),
        base = base,
    );
    fs::write(&wrapper_path, body).unwrap();
    fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Write a uname wrapper that impersonates a requested platform so we can
/// exercise the unsupported-platform exit code without spoofing the host.
fn install_uname_wrapper(dir: &Path, s_value: &str, m_value: &str) {
    let wrapper_path = dir.join("uname");
    let real_uname = which("uname");
    let body = format!(
        r#"#!/usr/bin/env bash
case "${{1:-}}" in
    -s) echo '{s}' ;;
    -m) echo '{m}' ;;
    *)  exec '{real}' "$@" ;;
esac
"#,
        s = s_value,
        m = m_value,
        real = real_uname.display(),
    );
    fs::write(&wrapper_path, body).unwrap();
    fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn which(cmd: &str) -> PathBuf {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd}"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "missing required command on test host: {cmd}"
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run setup.sh with a controlled environment: `$PATH` prepended with the
/// wrapper dir, `$HOME`/`$SHELL`/`$TMPDIR` isolated to the provided temp
/// dirs, and optional `1UP_*` overrides.
struct RunInput<'a> {
    home: &'a Path,
    wrapper_dir: &'a Path,
    install_dir: Option<&'a Path>,
    version_pin: Option<&'a str>,
    shell_override: &'a str,
}

fn run_setup(input: RunInput) -> std::process::Output {
    let real_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{}", input.wrapper_dir.display(), real_path);

    let mut cmd = Command::new("bash");
    cmd.arg(setup_script())
        .env("HOME", input.home)
        .env("PATH", &path)
        .env("SHELL", input.shell_override)
        .env("TMPDIR", input.home) // keep mktemp scoped to the fixture
        .env_remove("1UP_INSTALL_DIR")
        .env_remove("1UP_VERSION")
        .env_remove("1UP_REPO")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(dir) = input.install_dir {
        cmd.env("1UP_INSTALL_DIR", dir);
    }
    if let Some(pin) = input.version_pin {
        cmd.env("1UP_VERSION", pin);
    }

    cmd.output().unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn setup_installs_binary_and_updates_path_on_happy_path() {
    // REQ-001, REQ-002, REQ-006, REQ-013: happy path installs the matching
    // binary, appends a PATH block to the detected rc, and prints
    // `Run: 1up start` as the final stdout line.
    let host_home = tempfile::tempdir().unwrap();
    let wrapper_dir = tempfile::tempdir().unwrap();
    let fixture = ReleaseFixture::new(FIXTURE_TAG, host_target(), true);
    let server = LocalHttp::start(fixture.serve_root.clone());
    install_curl_wrapper(wrapper_dir.path(), &server.url());

    let output = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: None,
        version_pin: Some(FIXTURE_TAG),
        shell_override: "/bin/zsh",
    });

    assert!(
        output.status.success(),
        "setup.sh should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let final_line = stdout.lines().last().unwrap_or("");
    assert_eq!(final_line, "Run: 1up start", "final stdout line: {stdout}");

    let installed = host_home.path().join(".1up/bin/1up");
    assert!(installed.is_file(), "binary should be installed");
    let mode = fs::metadata(&installed).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "installed binary must be 0755");

    let rc = fs::read_to_string(host_home.path().join(".zshrc")).unwrap();
    assert!(
        rc.contains("# >>> 1up install (managed) >>>"),
        "rc should have fenced PATH block: {rc}"
    );
    assert!(
        rc.matches("# >>> 1up install (managed) >>>").count() == 1,
        "exactly one PATH block expected"
    );
}

#[test]
fn setup_is_idempotent_on_second_run() {
    // REQ-007, REQ-008: second back-to-back run must replace the binary
    // atomically and must not append a duplicate PATH block.
    let host_home = tempfile::tempdir().unwrap();
    let wrapper_dir = tempfile::tempdir().unwrap();
    let fixture = ReleaseFixture::new(FIXTURE_TAG, host_target(), true);
    let server = LocalHttp::start(fixture.serve_root.clone());
    install_curl_wrapper(wrapper_dir.path(), &server.url());

    let first = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: None,
        version_pin: Some(FIXTURE_TAG),
        shell_override: "/bin/zsh",
    });
    assert!(first.status.success());

    let rc_path = host_home.path().join(".zshrc");
    let rc_after_first = fs::read_to_string(&rc_path).unwrap();

    let second = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: None,
        version_pin: Some(FIXTURE_TAG),
        shell_override: "/bin/zsh",
    });
    assert!(
        second.status.success(),
        "second run should succeed: {}",
        String::from_utf8_lossy(&second.stderr),
    );

    let rc_after_second = fs::read_to_string(&rc_path).unwrap();
    assert_eq!(
        rc_after_first, rc_after_second,
        "second run must not mutate rc file"
    );
    assert_eq!(
        rc_after_second
            .matches("# >>> 1up install (managed) >>>")
            .count(),
        1,
        "PATH block must remain single: {rc_after_second}"
    );

    // No staging leftovers in the install dir.
    let install_dir = host_home.path().join(".1up/bin");
    for entry in fs::read_dir(&install_dir).unwrap() {
        let name = entry.unwrap().file_name();
        let name_s = name.to_string_lossy();
        assert!(
            !name_s.starts_with(".1up.tmp."),
            "staging leftover in install dir: {name_s}"
        );
    }
}

#[test]
fn setup_replaces_path_block_on_rerun_with_new_install_dir() {
    // When a user reruns setup.sh with a different `1UP_INSTALL_DIR`, the
    // binary lands in the new dir but the rc PATH block must also be
    // rewritten to point at the new dir -- otherwise the old dir stays on
    // PATH and the freshly-installed binary is unreachable.
    let host_home = tempfile::tempdir().unwrap();
    let wrapper_dir = tempfile::tempdir().unwrap();
    let fixture = ReleaseFixture::new(FIXTURE_TAG, host_target(), true);
    let server = LocalHttp::start(fixture.serve_root.clone());
    install_curl_wrapper(wrapper_dir.path(), &server.url());

    // First run: default install dir.
    let first = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: None,
        version_pin: Some(FIXTURE_TAG),
        shell_override: "/bin/zsh",
    });
    assert!(
        first.status.success(),
        "first run should succeed: {}",
        String::from_utf8_lossy(&first.stderr),
    );

    let rc_path = host_home.path().join(".zshrc");
    let rc_after_first = fs::read_to_string(&rc_path).unwrap();
    let default_install_dir = host_home.path().join(".1up/bin");
    assert!(
        rc_after_first.contains(default_install_dir.to_str().unwrap()),
        "first run rc should reference default install dir: {rc_after_first}"
    );

    // Second run: override install dir. Block must be rewritten.
    let alt_dir = host_home.path().join("alt-install");
    let second = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: Some(&alt_dir),
        version_pin: Some(FIXTURE_TAG),
        shell_override: "/bin/zsh",
    });
    assert!(
        second.status.success(),
        "second run with new install dir should succeed: {}",
        String::from_utf8_lossy(&second.stderr),
    );

    let rc_after_second = fs::read_to_string(&rc_path).unwrap();
    assert_eq!(
        rc_after_second
            .matches("# >>> 1up install (managed) >>>")
            .count(),
        1,
        "PATH block must remain single after rerun: {rc_after_second}"
    );
    assert!(
        rc_after_second.contains(alt_dir.to_str().unwrap()),
        "rc should now reference new install dir: {rc_after_second}"
    );
    assert!(
        !rc_after_second.contains(&format!(
            "export PATH=\"{}:$PATH\"",
            default_install_dir.to_str().unwrap()
        )),
        "old install-dir PATH export must be gone after rerun: {rc_after_second}"
    );

    // Script should log the replacement so the user sees the change.
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("Replaced PATH block"),
        "second-run stdout should name the rewrite: {stdout}"
    );
}

#[test]
fn setup_fails_on_checksum_mismatch() {
    // REQ-003: published SHA256SUMS that disagrees with the served archive
    // must be fatal. Binary must NOT land in the install dir.
    let host_home = tempfile::tempdir().unwrap();
    let wrapper_dir = tempfile::tempdir().unwrap();
    // Build an honest fixture, then overwrite the archive bytes so the
    // recorded hash is stale.
    let fixture =
        ReleaseFixture::new_with_binary(FIXTURE_TAG, host_target(), true, Some(b"tampered\n"));
    let server = LocalHttp::start(fixture.serve_root.clone());
    install_curl_wrapper(wrapper_dir.path(), &server.url());

    let output = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: None,
        version_pin: Some(FIXTURE_TAG),
        shell_override: "/bin/bash",
    });
    assert!(
        !output.status.success(),
        "setup.sh must fail on checksum mismatch: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("checksum mismatch"),
        "stderr should name the failure: {stderr}"
    );

    let installed = host_home.path().join(".1up/bin/1up");
    assert!(
        !installed.exists(),
        "binary must not be installed on checksum failure"
    );
}

#[test]
fn setup_warns_and_installs_without_sha256sums() {
    // REQ-004: missing SHA256SUMS is a warn-and-continue path.
    let host_home = tempfile::tempdir().unwrap();
    let wrapper_dir = tempfile::tempdir().unwrap();
    let fixture = ReleaseFixture::new(FIXTURE_TAG, host_target(), false);
    let server = LocalHttp::start(fixture.serve_root.clone());
    install_curl_wrapper(wrapper_dir.path(), &server.url());

    let output = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: None,
        version_pin: Some(FIXTURE_TAG),
        shell_override: "/bin/bash",
    });
    assert!(
        output.status.success(),
        "setup.sh must succeed when SHA256SUMS is absent: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SHA256SUMS not published"),
        "stderr should carry the integrity warning: {stderr}"
    );
    assert!(
        host_home.path().join(".1up/bin/1up").is_file(),
        "binary must still be installed"
    );
}

#[test]
fn setup_rejects_unsupported_platform() {
    // REQ-002, REQ-010: unsupported platform exits non-zero and names the
    // detected platform in the error. Stubs uname so the test host's real
    // OS/arch is never detected.
    let host_home = tempfile::tempdir().unwrap();
    let wrapper_dir = tempfile::tempdir().unwrap();
    // No HTTP fixture needed: the script should fail in stage 2 before
    // download. Install curl wrapper pointing at a non-routable address so
    // if the script did reach the network, the test would fail loudly.
    install_curl_wrapper(wrapper_dir.path(), "http://127.0.0.1:1");
    install_uname_wrapper(wrapper_dir.path(), "FreeBSD", "amd64");

    let output = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: None,
        version_pin: Some(FIXTURE_TAG),
        shell_override: "/bin/bash",
    });
    assert!(!output.status.success(), "must fail on FreeBSD");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported platform") && stderr.contains("FreeBSD"),
        "stderr should name platform: {stderr}"
    );
    assert!(
        !host_home.path().join(".1up/bin").exists(),
        "install dir must not be created on unsupported-platform failure"
    );
}

#[test]
fn setup_honors_pinned_version() {
    // REQ-009: 1UP_VERSION selects the requested tag. We publish only the
    // pinned tag's archive under the fixture, so a successful install
    // implies the script asked for that specific archive.
    let host_home = tempfile::tempdir().unwrap();
    let wrapper_dir = tempfile::tempdir().unwrap();
    let pinned = "v0.1.7";
    let fixture = ReleaseFixture::new(pinned, host_target(), true);
    let server = LocalHttp::start(fixture.serve_root.clone());
    install_curl_wrapper(wrapper_dir.path(), &server.url());

    let output = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: None,
        version_pin: Some(pinned),
        shell_override: "/bin/bash",
    });
    assert!(
        output.status.success(),
        "pinned install should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("Installed 1up {pinned}")),
        "final message should name pinned tag: {stdout}"
    );

    // Sanity: the stub binary prints the tag it was built for.
    let installed = host_home.path().join(".1up/bin/1up");
    let version_out = Command::new(&installed).arg("--version").output().unwrap();
    let version_stdout = String::from_utf8_lossy(&version_out.stdout);
    assert!(
        version_stdout.contains(pinned),
        "installed binary should report pinned tag: {version_stdout}"
    );
}

#[test]
fn setup_fails_cleanly_on_missing_pinned_version() {
    // REQ-009 negative path: pinning to a tag that has no published release
    // asset must exit non-zero and name the missing artefact, with no binary
    // installed. The fixture only publishes FIXTURE_TAG, so requesting any
    // other tag drives the archive download into a 404.
    let host_home = tempfile::tempdir().unwrap();
    let wrapper_dir = tempfile::tempdir().unwrap();
    let fixture = ReleaseFixture::new(FIXTURE_TAG, host_target(), true);
    let server = LocalHttp::start(fixture.serve_root.clone());
    install_curl_wrapper(wrapper_dir.path(), &server.url());

    let missing_tag = "v99.99.99";
    let output = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: None,
        version_pin: Some(missing_tag),
        shell_override: "/bin/bash",
    });
    assert!(
        !output.status.success(),
        "setup.sh must fail when the pinned tag has no release asset: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("release asset not found") && stderr.contains(missing_tag),
        "stderr should name the missing tag and asset: {stderr}"
    );
    assert!(
        !host_home.path().join(".1up/bin/1up").exists(),
        "binary must not be installed when the pinned release is missing"
    );
    assert!(
        !host_home.path().join(".bashrc").exists() && !host_home.path().join(".zshrc").exists(),
        "rc files must not be touched when install fails before configure_path"
    );
}

#[test]
fn setup_honors_custom_install_dir() {
    // REQ-014: 1UP_INSTALL_DIR override lands the binary elsewhere and
    // the rc PATH block references the same dir.
    let host_home = tempfile::tempdir().unwrap();
    let wrapper_dir = tempfile::tempdir().unwrap();
    let alt_dir = host_home.path().join("alt-install");
    let fixture = ReleaseFixture::new(FIXTURE_TAG, host_target(), true);
    let server = LocalHttp::start(fixture.serve_root.clone());
    install_curl_wrapper(wrapper_dir.path(), &server.url());

    let output = run_setup(RunInput {
        home: host_home.path(),
        wrapper_dir: wrapper_dir.path(),
        install_dir: Some(&alt_dir),
        version_pin: Some(FIXTURE_TAG),
        shell_override: "/bin/bash",
    });
    assert!(
        output.status.success(),
        "custom install dir should work: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    let binary = alt_dir.join("1up");
    assert!(binary.is_file(), "binary must land at 1UP_INSTALL_DIR");
    let rc = fs::read_to_string(host_home.path().join(".bashrc")).unwrap();
    assert!(
        rc.contains(alt_dir.to_str().unwrap()),
        "rc PATH block should reference custom dir: {rc}"
    );
}

// ---------------------------------------------------------------------------
// bash 3.2 compatibility smoke (REQ-011)
// ---------------------------------------------------------------------------

/// Static-lint smoke that exercises setup.sh's bash 3.2 guardrails without
/// requiring a real 3.2 runtime on every CI host: parse under the system
/// bash with `--posix` and the script's own `set -eu`, and also run `bash
/// -n` as a syntax check. The T4 acceptance criterion also calls for a
/// container-backed bash 3.2 run; that gate lives in CI (`.github/
/// workflows/ci.yml`) rather than this test crate so Cargo tests remain
/// hermetic.
#[test]
fn setup_script_parses_without_syntax_errors() {
    let script = setup_script();
    let out = Command::new("bash")
        .arg("-n")
        .arg(&script)
        .output()
        .expect("bash -n is available on Unix hosts");
    assert!(
        out.status.success(),
        "bash -n failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    // Reject bashisms banned by REQ-011 via a regex sweep. `mapfile` /
    // `readarray` / `${var,,}` / `${var^^}` / `declare -A` are all bash 4+.
    let body = fs::read_to_string(&script).unwrap();
    for bad in &["mapfile ", "readarray ", "declare -A", "${!"] {
        assert!(
            !body.contains(bad),
            "setup.sh contains bash-4-only construct: {bad}"
        );
    }
    // Parameter-case expansion (${v,,} / ${v^^}) would break macOS bash 3.2.
    for needle in &[",,}", "^^}"] {
        assert!(
            !body.contains(needle),
            "setup.sh uses bash-4-only parameter expansion: {needle}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test harness utilities -- exercised by the cases above but also kept as
// invariants so future changes to these helpers surface as compile-time or
// test failures, not silent test skips.
// ---------------------------------------------------------------------------

#[test]
fn local_http_serves_fixture_files() {
    // Sanity-check the fixture harness itself so a broken server surfaces
    // as a dedicated failure instead of manifesting as spurious setup.sh
    // timeouts.
    let fixture = ReleaseFixture::new(FIXTURE_TAG, host_target(), true);
    let server = LocalHttp::start(fixture.serve_root.clone());

    // Basic poll to avoid accept-race on slow CI runners.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut body = Vec::new();
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(&server.addr) {
            let req = format!(
                "GET /repos/{FIXTURE_REPO}/releases/download/{FIXTURE_TAG}/{} HTTP/1.0\r\nHost: localhost\r\n\r\n",
                fixture.archive_name
            );
            stream.write_all(req.as_bytes()).unwrap();
            stream.read_to_end(&mut body).unwrap();
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let response = String::from_utf8_lossy(&body);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "archive should be served: {response}"
    );
}
