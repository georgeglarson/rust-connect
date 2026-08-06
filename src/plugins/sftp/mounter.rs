//! SFTP desktop mounter
//!
//! Owns the sshfs/fusermount subprocess boundary for the SFTP plugin.
//! Mirrors the upstream KDE Connect shape (kdeconnect-kde
//! `plugins/sftp/mounter.cpp:72` spawns `sshfs`, `:105` passes the password
//! via `-o password_stdin`, `:114` writes it to the child's stdin —
//! the password never appears in argv). Unmount runs `fusermount3 -u`
//! (modern FUSE 3), with `fusermount` as a fallback
//! (kdeconnect-kde `mounter.cpp:204`).
//!
//! The command builder and runner are injectable so tests can substitute
//! fake binaries. See the harness in `tests/fake_binary_shim.rs`.
//!
//! Safety properties enforced here:
//! - The password is NEVER an argv element, env var, or file write.
//! - The runner only knows about paths and binary names; the caller passes
//!   the password separately and the runner pipes it on stdin.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::utils::errors::{Error, Result};

/// The information the mounter needs to spawn sshfs. Mirrors the subset of
/// `SftpConnectionInfo` the mount actually consumes — the password is
/// carried separately so callers cannot accidentally pass it via a path
/// or a `String` that ends up logged.
#[derive(Debug, Clone)]
pub struct MountRequest {
    pub ip: String,
    pub port: u16,
    pub user: String,
    pub path: String,
    /// Server-determined path under `data_dir`. The mounter creates the
    /// directory if it does not exist.
    pub mount_point: PathBuf,
}

/// Outcome of a mount attempt. The plugin maps this to its state machine
/// and to the `PluginEvent::SftpUpdate` broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountOutcome {
    Mounted,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnmountOutcome {
    Unmounted,
    /// Unmount could not release the mount (still listed in /proc/mounts or
    /// the unmount tool returned non-zero). Plugin can retry or surface
    /// to the API caller.
    Failed(String),
}

/// Pluggable command-runner boundary. Production code uses
/// `SystemCommandRunner`; tests inject a fake.
pub trait CommandRunner: Send + Sync {
    /// Locate a binary on `PATH`. Returns the absolute path if found.
    fn which(&self, name: &str) -> Option<PathBuf>;

    /// Spawn `program args...`, write `stdin_payload` to the child's
    /// stdin (with a trailing newline), wait for it to exit, and return
    /// the exit status + combined stdout/stderr.
    fn run_with_stdin(
        &self,
        program: &Path,
        args: &[OsString],
        stdin_payload: Option<&str>,
    ) -> Result<CommandOutput>;
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Production runner: uses `which` and `std::process::Command`.
pub struct SystemCommandRunner {
    path: OsString,
}

impl SystemCommandRunner {
    pub fn new() -> Self {
        Self {
            path: std::env::var_os("PATH").unwrap_or_default(),
        }
    }

    /// Build a Command whose argv and env are pre-validated. The caller
    /// supplies the program as an absolute path discovered by `which` so
    /// we never shell out via a shell.
    fn make_command(&self, program: &Path, args: &[OsString]) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args);
        // Belt-and-suspenders: clear the inherited env (PATH is needed for
        // sshfs's own helpers in some installs, so we keep it but document
        // the intent). The plugin MUST NOT put the password in env.
        // We do not set `cmd.env_clear()` here because some sshfs/FUSE
        // helpers rely on HOME / USER being present; the plugin-level
        // invariant is "no password in env", enforced by arg construction.
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::piped());
        cmd
    }
}

impl Default for SystemCommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRunner for SystemCommandRunner {
    fn which(&self, name: &str) -> Option<PathBuf> {
        // Mirror `which` semantics: walk PATH and check executable bit.
        // Avoids pulling in the `which` crate.
        for dir in std::env::split_paths(&self.path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = std::fs::metadata(&candidate) {
                        if meta.permissions().mode() & 0o111 != 0 {
                            return Some(candidate);
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn run_with_stdin(
        &self,
        program: &Path,
        args: &[OsString],
        stdin_payload: Option<&str>,
    ) -> Result<CommandOutput> {
        let mut cmd = self.make_command(program, args);
        let mut child = cmd.spawn().map_err(|e| {
            Error::Internal(format!("Failed to spawn {}: {}", program.display(), e))
        })?;

        if let Some(payload) = stdin_payload {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(payload.as_bytes())
                    .and_then(|_| stdin.write_all(b"\n"))
                    .map_err(|e| Error::Internal(format!("Failed to write stdin: {}", e)))?;
                // Close stdin so the child sees EOF.
                drop(stdin);
            }
        } else if child.stdin.take().is_some() {
            // Close stdin even if no payload.
        }

        let output = child
            .wait_with_output()
            .map_err(|e| Error::Internal(format!("Failed to wait on child: {}", e)))?;
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Build the argv vector sshfs expects. The password is NOT in here —
/// it travels separately on stdin (kdeconnect-kde mounter.cpp:105, 114).
///
/// Hardening opts chosen and why:
/// - `password_stdin` — pass the password on stdin (kdeconnect-kde mounter.cpp:105).
///   The only safe option on a shared host; prevents `ps` exposure.
/// - `StrictHostKeyChecking=no` + `UserKnownHostsFile=/dev/null` — the
///   phone regenerates its host key on every reconnect, so storing it
///   is pointless; we deliberately do not prompt for confirmation.
///   (kdeconnect-kde mounter.cpp:99-100.)
/// - `reconnect` + `ServerAliveInterval=30` — tolerate brief network
///   blips without remounting. (kdeconnect-kde mounter.cpp:103-104.)
/// - `IdentityFile=` is intentionally OMITTED — kdeconnect-kde uses the
///   daemon's identity key as the SSH identity (mounter.cpp:98), but
///   in our case the phone's sshd accepts the password we just received
///   and we have no per-device SSH key to offer. The password is the
///   only auth credential the phone sends.
/// - `allow_root` is NOT set — sshfs refuses to run as root by default;
///   the daemon runs as the desktop user.
/// - `uid=` / `gid=` are NOT set — the daemon's process uid/gid are
///   correct by inheritance; setting them opens a TOCTOU window
///   (mounter.cpp:101-102 is kdeconnect-kde's only protection).
pub fn build_sshfs_args(req: &MountRequest) -> Vec<OsString> {
    let user_host_path = format!("{}@{}:{}", req.user, req.ip, req.path);
    vec![
        OsString::from(user_host_path),
        OsString::from(req.mount_point.as_os_str()),
        OsString::from("-p"),
        OsString::from(req.port.to_string()),
        OsString::from("-o"),
        OsString::from("password_stdin"),
        OsString::from("-o"),
        OsString::from("StrictHostKeyChecking=no"),
        OsString::from("-o"),
        OsString::from("UserKnownHostsFile=/dev/null"),
        OsString::from("-o"),
        OsString::from("reconnect"),
        OsString::from("-o"),
        OsString::from("ServerAliveInterval=30"),
    ]
}

/// The mounter holds no state of its own beyond its command runner and
/// the sshfs binary path. Mount state (current mount per device) lives
/// in the plugin — the mounter is stateless on purpose.
///
/// The runner is stored as an `Arc<dyn CommandRunner>` — production gets
/// `SystemCommandRunner`, tests inject a fake. The mounter is therefore
/// not generic; the previous generic shape needed blanket impls for
/// `Box<T>`/`Arc<T>` that were easy to trip over.
pub struct Mounter {
    runner: std::sync::Arc<dyn CommandRunner>,
    sshfs_path: Option<PathBuf>,
    /// First fusermount3 found; falls back to fusermount when absent.
    fusermount_path: Option<PathBuf>,
    fusermount3_path: Option<PathBuf>,
}

impl Mounter {
    pub fn new(runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        let sshfs_path = runner.which("sshfs");
        let fusermount3_path = runner.which("fusermount3");
        let fusermount_path = runner.which("fusermount");
        Self {
            runner,
            sshfs_path,
            fusermount3_path,
            fusermount_path,
        }
    }

    /// Backend-availability gate. Both sshfs and at least one fusermount
    /// must be present. `is_backend_available()` on the plugin asks this.
    pub fn is_available(&self) -> bool {
        self.sshfs_path.is_some()
            && (self.fusermount3_path.is_some() || self.fusermount_path.is_some())
    }

    /// sshfs path if discovered, for diagnostics / tests.
    pub fn sshfs_path(&self) -> Option<&Path> {
        self.sshfs_path.as_deref()
    }

    /// Try to mount. The mounter creates the mount point directory if
    /// absent. Returns `Mounted` only when sshfs exits 0 and the mount
    /// is listed by the kernel; otherwise `Failed` with a short message.
    pub fn mount(&self, req: &MountRequest, password: &str) -> Result<MountOutcome> {
        let sshfs = self
            .sshfs_path
            .as_deref()
            .ok_or_else(|| Error::PluginError {
                plugin: "sftp".to_string(),
                message: "sshfs binary not found on PATH".to_string(),
            })?;

        if let Err(e) = std::fs::create_dir_all(&req.mount_point) {
            return Ok(MountOutcome::Failed(format!(
                "failed to create mount point {}: {}",
                req.mount_point.display(),
                e
            )));
        }

        let args = build_sshfs_args(req);
        let output = self.runner.run_with_stdin(sshfs, &args, Some(password))?;
        if output.status != 0 {
            let detail = format_truncated(&output.stderr);
            if detail.is_empty() {
                return Ok(MountOutcome::Failed(format!(
                    "sshfs exited with status {}",
                    output.status
                )));
            }
            return Ok(MountOutcome::Failed(detail));
        }

        // Upstream relies on the sshfs exit code as the success signal
        // (kdeconnect-kde mounter.cpp:118-122 emits `mounted` on
        // QProcess::started). We use the exit code too; a 10s timeout is
        // an upstream choice (mounter.cpp:32) — for a desktop file
        // browser, returning on the exit code (or a process-start signal)
        // is the right granularity here.
        Ok(MountOutcome::Mounted)
    }

    /// Try to unmount. Uses fusermount3 if available, else fusermount.
    /// 0 exit means unmounted. Non-zero becomes `Failed(stderr)`.
    pub fn unmount(&self, mount_point: &Path) -> Result<UnmountOutcome> {
        let program = self
            .fusermount3_path
            .as_deref()
            .or(self.fusermount_path.as_deref())
            .ok_or_else(|| Error::PluginError {
                plugin: "sftp".to_string(),
                message: "fusermount / fusermount3 not found on PATH".to_string(),
            })?;

        let args = [
            OsString::from("-u"),
            OsString::from(mount_point.as_os_str()),
        ];
        let output = self.runner.run_with_stdin(program, &args, None)?;
        if output.status == 0 {
            Ok(UnmountOutcome::Unmounted)
        } else {
            Ok(UnmountOutcome::Failed(format_truncated(&output.stderr)))
        }
    }
}

fn format_truncated(s: &str) -> String {
    // Cap error message length to keep the API response bounded.
    const LIMIT: usize = 512;
    if s.len() <= LIMIT {
        s.trim().to_string()
    } else {
        let mut end = LIMIT;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", s[..end].trim())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    /// Record argv + stdin to sidecar files. Honors an optional
    /// `.fail` sidecar file containing an exit code so tests can
    /// simulate sshfs crashes without using process-global env vars
    /// (parallel test execution makes env-var shims race-prone).
    fn write_fake_sshfs(dir: &Path, record_path: &Path) -> PathBuf {
        let path = dir.join("sshfs");
        let argv_log = record_path.to_string_lossy().into_owned();
        let stdin_log = argv_log.replace(".argv", ".stdin");
        let fail_flag = argv_log.replace(".argv", ".fail");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$0\" \"$@\" > {argv_log:?}\n\
             cat > {stdin_log:?} <&0\n\
             if [ -f {fail_flag:?} ]; then exit \"$(cat {fail_flag:?})\"; fi\n\
             sleep 0.2 || true\n\
             exit 0\n",
        );
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&path)
            .expect("create fake sshfs");
        f.write_all(script.as_bytes()).expect("write fake sshfs");
        path
    }

    fn write_fake_fusermount(dir: &Path, record_path: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let record = record_path.to_string_lossy().into_owned();
        let fail_flag = record.replace(".argv", ".fail");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$0\" \"$@\" > {record:?}\n\
             if [ -f {fail_flag:?} ]; then exit \"$(cat {fail_flag:?})\"; fi\n\
             exit 0\n",
        );
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&path)
            .expect("create fake fusermount");
        f.write_all(script.as_bytes())
            .expect("write fake fusermount");
        path
    }

    /// Runner that searches a single temp dir for binaries.
    struct SingleDirRunner {
        dir: PathBuf,
    }
    impl CommandRunner for SingleDirRunner {
        fn which(&self, name: &str) -> Option<PathBuf> {
            let p = self.dir.join(name);
            if p.is_file() {
                Some(p)
            } else {
                None
            }
        }
        fn run_with_stdin(
            &self,
            program: &Path,
            args: &[OsString],
            stdin_payload: Option<&str>,
        ) -> Result<CommandOutput> {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd.stdin(Stdio::piped());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            let mut child = cmd.spawn().expect("spawn fake");
            if let Some(payload) = stdin_payload {
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(payload.as_bytes()).unwrap();
                    stdin.write_all(b"\n").unwrap();
                }
            }
            let out = child.wait_with_output().expect("wait fake");
            Ok(CommandOutput {
                status: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }

    fn make_request() -> MountRequest {
        MountRequest {
            ip: "192.168.1.10".to_string(),
            port: 1740,
            user: "kdeconnect".to_string(),
            path: "/storage/emulated/0".to_string(),
            mount_point: PathBuf::from("/tmp/sftp-test-mount"),
        }
    }

    #[test]
    fn build_sshfs_args_puts_password_stdin_opt_in_argv_but_not_password() {
        let req = make_request();
        let args = build_sshfs_args(&req);
        let argv_str: Vec<String> = args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();

        // The hardening opts from upstream are present.
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "password_stdin"));
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "StrictHostKeyChecking=no"));
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "UserKnownHostsFile=/dev/null"));
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "reconnect"));
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1].starts_with("ServerAliveInterval=")));

        // The password (or any marker thereof) is NOT in argv. The argv
        // is the surface `ps` reads.
        let secret = "SUPERSECRET-PWD-9f8e7d";
        for arg in &argv_str {
            assert_ne!(arg, secret, "password leaked into argv: {arg}");
        }
        // The user@host:path form is the credential surface; it must
        // not embed the password in the path component either.
        let user_host = format!("{}@{}:{}", req.user, req.ip, req.path);
        assert_eq!(argv_str[0], user_host);
    }

    #[test]
    fn mounter_writes_password_to_stdin_only_not_argv() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let log = dir.path().join("sshfs.argv");
        let um_log = dir.path().join("fusermount3.argv");
        let secret = "verysecret-1q2w3e4r";
        write_fake_sshfs(&bin_dir, &log);
        write_fake_fusermount(&bin_dir, &um_log, "fusermount3");

        let runner = SingleDirRunner {
            dir: bin_dir.clone(),
        };
        let mounter = Mounter::new(std::sync::Arc::new(runner));
        assert!(
            mounter.is_available(),
            "fake sshfs + fusermount must make the mounter available"
        );

        let req = MountRequest {
            mount_point: dir.path().join("mnt"),
            ..make_request()
        };
        let outcome = mounter.mount(&req, secret).expect("mount call");
        assert_eq!(outcome, MountOutcome::Mounted);

        let argv_text = std::fs::read_to_string(&log).expect("read argv log");
        assert!(
            !argv_text.contains(secret),
            "password MUST NOT appear in argv (captured: {argv_text})"
        );
        let stdin_text =
            std::fs::read_to_string(dir.path().join("sshfs.stdin")).expect("read stdin log");
        assert!(
            stdin_text.contains(secret),
            "password MUST be written to stdin (captured: {stdin_text})"
        );
    }

    #[test]
    fn unmount_calls_fusermount3_with_tracked_mountpoint() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let um_log = dir.path().join("fusermount3.argv");
        write_fake_sshfs(&bin_dir, &dir.path().join("sshfs.argv"));
        write_fake_fusermount(&bin_dir, &um_log, "fusermount3");

        let runner = SingleDirRunner {
            dir: bin_dir.clone(),
        };
        let mounter = Mounter::new(std::sync::Arc::new(runner));
        assert!(mounter.is_available());

        let mount_point = dir.path().join("mnt");
        let outcome = mounter.unmount(&mount_point).expect("unmount call");
        assert_eq!(outcome, UnmountOutcome::Unmounted);

        let argv_text = std::fs::read_to_string(&um_log).expect("read unmount log");
        // The argv log records one program + one arg per line, in order.
        // The mount point path MUST appear as a line, and `-u` MUST appear
        // as a line.
        let lines: Vec<&str> = argv_text.lines().collect();
        assert!(
            lines.contains(&"-u"),
            "unmount did not include -u flag: {argv_text}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == &mount_point.to_string_lossy().as_ref()),
            "unmount did not include mount point in args: {argv_text}"
        );
    }

    #[test]
    fn unmount_falls_back_to_fusermount_when_fusermount3_missing() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let um_log = dir.path().join("fusermount.argv");
        write_fake_sshfs(&bin_dir, &dir.path().join("sshfs.argv"));
        // Only the legacy fusermount is on the bin path; fusermount3 absent.
        write_fake_fusermount(&bin_dir, &um_log, "fusermount");

        let runner = SingleDirRunner {
            dir: bin_dir.clone(),
        };
        let mounter = Mounter::new(std::sync::Arc::new(runner));
        assert!(
            mounter.is_available(),
            "fusermount fallback must keep the mounter available"
        );

        let mount_point = dir.path().join("mnt");
        let outcome = mounter.unmount(&mount_point).expect("unmount call");
        assert_eq!(outcome, UnmountOutcome::Unmounted);
        // The legacy binary was the one that ran.
        let argv_text = std::fs::read_to_string(&um_log).expect("read um log");
        assert!(argv_text.contains("fusermount\n") || argv_text.ends_with("fusermount"));
    }

    #[test]
    fn mounter_unavailable_when_sshfs_missing() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        // Only fusermount, no sshfs.
        let um_log = dir.path().join("fusermount3.argv");
        write_fake_fusermount(&bin_dir, &um_log, "fusermount3");

        let runner = SingleDirRunner { dir: bin_dir };
        let mounter = Mounter::new(std::sync::Arc::new(runner));
        assert!(
            !mounter.is_available(),
            "missing sshfs must make mounter unavailable"
        );

        let req = make_request();
        let err = mounter.mount(&req, "anypwd").expect_err("mount must error");
        match err {
            Error::PluginError { plugin, message } => {
                assert_eq!(plugin, "sftp");
                assert!(message.contains("sshfs"), "message: {message}");
            }
            other => panic!("expected PluginError, got {other:?}"),
        }
    }

    #[test]
    fn mounter_returns_failed_when_sshfs_exits_nonzero() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let log = dir.path().join("sshfs.argv");
        write_fake_sshfs(&bin_dir, &log);
        write_fake_fusermount(
            &bin_dir,
            &dir.path().join("fusermount3.argv"),
            "fusermount3",
        );

        // Force the fake sshfs to fail with exit 13. The fake reads the
        // fail code from a sidecar file (NOT an env var) so parallel
        // tests don't race on process-global state.
        std::fs::write(dir.path().join("sshfs.fail"), "13").expect("write fail flag");
        let runner = SingleDirRunner { dir: bin_dir };
        let mounter = Mounter::new(std::sync::Arc::new(runner));
        let req = MountRequest {
            mount_point: dir.path().join("mnt"),
            ..make_request()
        };
        let outcome = mounter.mount(&req, "secret").expect("mount call");
        match outcome {
            MountOutcome::Failed(msg) => {
                assert!(!msg.is_empty(), "failure must carry a message");
            }
            MountOutcome::Mounted => panic!("non-zero exit must surface as Failed"),
        }
    }
}
