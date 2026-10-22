//! The Linux one-liner (`install.sh`) run against a fake release directory, so
//! its arch detection and checksum check are proven without a real release.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const VERSION: &str = "0.1.0";

struct Machine {
    dir: tempfile::TempDir,
}

impl Machine {
    /// A home directory, a release directory holding a tarball per arch and
    /// their SHA256SUMS, and a `uname` that reports `os` and `arch`.
    fn new(os: &str, arch: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = Self { dir };
        fs::create_dir_all(m.home()).expect("home");
        fs::create_dir_all(m.release()).expect("release");
        fs::create_dir_all(m.fake_bin()).expect("bin");
        script(
            &m.fake_bin().join("uname"),
            &format!("case $1 in -s) echo {os} ;; -m) echo {arch} ;; esac"),
        );
        for arch in ["x86_64", "aarch64"] {
            m.package(arch);
        }
        m.write_sums();
        m
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }
    fn home(&self) -> PathBuf {
        self.path("home")
    }
    fn release(&self) -> PathBuf {
        self.path("release")
    }
    fn fake_bin(&self) -> PathBuf {
        self.path("fakebin")
    }
    fn installed(&self) -> PathBuf {
        self.home().join(".local/bin/signal-agent")
    }

    /// A tarball whose `signal-agent` records which arch it is and its args.
    fn package(&self, arch: &str) {
        let name = format!("signal-agent-{VERSION}-linux-{arch}");
        let stage = self.path("stage").join(&name);
        fs::create_dir_all(&stage).expect("stage");
        script(
            &stage.join("signal-agent"),
            &format!("echo \"{arch} $*\" >\"$HOME/ran\""),
        );
        let status = Command::new("tar")
            .arg("-czf")
            .arg(self.release().join(format!("{name}.tar.gz")))
            .arg("-C")
            .arg(self.path("stage"))
            .arg(&name)
            .status()
            .expect("tar");
        assert!(status.success(), "tar failed");
    }

    fn write_sums(&self) {
        let mut sums = String::new();
        for arch in ["x86_64", "aarch64"] {
            let file = format!("signal-agent-{VERSION}-linux-{arch}.tar.gz");
            sums.push_str(&format!(
                "{}  {file}\n",
                sha256(&self.release().join(&file))
            ));
        }
        fs::write(self.release().join("SHA256SUMS"), sums).expect("sums");
    }

    fn install(&self, setup: bool) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install.sh");
        let path = format!(
            "{}:{}",
            self.fake_bin().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("sh");
        cmd.arg(script)
            .env("HOME", self.home())
            .env("PATH", path)
            .env(
                "SIGNAL_AGENT_RELEASE_URL",
                format!("file://{}", self.release().display()),
            )
            .stdin(std::process::Stdio::null());
        if !setup {
            cmd.env("SIGNAL_AGENT_NO_SETUP", "1");
        }
        cmd.output().expect("run install.sh")
    }

    fn ran(&self) -> Option<String> {
        fs::read_to_string(self.home().join("ran")).ok()
    }
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn sha256(path: &Path) -> String {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .or_else(|_| Command::new("sha256sum").arg(path).output())
        .expect("a sha256 tool");
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().next().expect("digest").to_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Each supported machine name gets its own arch's binary, installed for the
/// user without root.
#[test]
fn each_supported_arch_installs_its_own_binary_under_the_home_directory() {
    for (machine, arch) in [
        ("x86_64", "x86_64"),
        ("amd64", "x86_64"),
        ("aarch64", "aarch64"),
        ("arm64", "aarch64"),
    ] {
        let m = Machine::new("Linux", machine);
        let out = m.install(false);
        assert!(out.status.success(), "{machine}: {}", stderr(&out));
        let ran = Command::new(m.installed())
            .env("HOME", m.home())
            .output()
            .expect("installed binary runs");
        assert!(ran.status.success(), "{machine}: installed binary failed");
        assert_eq!(
            m.ran().as_deref(),
            Some(format!("{arch} \n").as_str()),
            "{machine}"
        );
    }
}

/// A tarball that doesn't match the release's checksum must never be
/// installed or run.
#[test]
fn a_tampered_download_is_refused_and_nothing_is_installed() {
    let m = Machine::new("Linux", "x86_64");
    let tarball = m
        .release()
        .join(format!("signal-agent-{VERSION}-linux-x86_64.tar.gz"));
    let mut bytes = fs::read(&tarball).expect("tarball");
    bytes.extend_from_slice(b"tampered");
    fs::write(&tarball, bytes).expect("tamper");
    let out = m.install(true);
    assert!(!out.status.success(), "a tampered download installed");
    assert!(stderr(&out).contains("does not match"), "{}", stderr(&out));
    assert!(!m.installed().exists(), "tampered binary installed");
    assert_eq!(m.ran(), None, "tampered binary ran");
}

/// A release whose SHA256SUMS lacks this arch is refused, not trusted.
#[test]
fn a_tarball_missing_from_the_checksums_is_refused() {
    let m = Machine::new("Linux", "aarch64");
    fs::write(m.release().join("SHA256SUMS"), "").expect("empty sums");
    let out = m.install(false);
    assert!(!out.status.success(), "installed without a checksum");
    assert!(stderr(&out).contains("no checksum"), "{}", stderr(&out));
    assert!(!m.installed().exists());
}

/// Other OSes and arches fail with a message saying what is supported,
/// before downloading anything.
#[test]
fn unsupported_os_or_arch_fails_clearly() {
    for (os, arch, says) in [
        ("Darwin", "arm64", "for Linux"),
        ("Linux", "riscv64", "x86_64 and aarch64"),
        ("Linux", "i686", "x86_64 and aarch64"),
    ] {
        let m = Machine::new(os, arch);
        let out = m.install(false);
        assert!(!out.status.success(), "{os} {arch} installed");
        assert!(stderr(&out).contains(says), "{os} {arch}: {}", stderr(&out));
        assert!(!m.installed().exists());
    }
}

/// After installing, the one-liner runs setup on the terminal; with no
/// terminal at all it says how to run setup instead of hanging.
#[test]
fn install_then_runs_setup_on_the_terminal_or_says_how() {
    let m = Machine::new("Linux", "x86_64");
    let out = m.install(true);
    assert!(m.installed().exists(), "{}", stderr(&out));
    match m.ran() {
        Some(ran) => assert_eq!(ran, "x86_64 setup\n"),
        None => {
            assert!(!out.status.success());
            assert!(
                stderr(&out).contains("signal-agent setup"),
                "{}",
                stderr(&out)
            );
        }
    }
}
