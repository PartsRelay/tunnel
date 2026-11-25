#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Cursor, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use data_encoding::HEXLOWER;
use flate2::Compression;
use flate2::write::GzEncoder;
use minisign::KeyPair;
use minisign_verify::PublicKey;
use partsrelay_tunnel::update::install::previous;
use partsrelay_tunnel::update::{Updater, manifest};
use ring::digest::{SHA256, digest};
use semver::Version;

const TARGET: &str = "test-target";
const ARCHIVE: &str = "partsrelay-tunnel-test-target.tar.gz";

/// An executable that reports `version`, as the tunnel does.
fn executable(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho \"partsrelay-tunnel {version} (test-target)\"\n").into_bytes()
}

fn archive(binary: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));

    for (path, content) in [
        ("README.md", b"read me".as_slice()),
        ("partsrelay-tunnel", binary),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, content).unwrap();
    }

    builder.into_inner().unwrap().finish().unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    HEXLOWER.encode(digest(&SHA256, bytes).as_ref())
}

/// Serves the files of a release like GitHub Releases does.
fn serve(files: HashMap<String, Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let files = Arc::new(files);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let files = files.clone();

            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                    line.clear();
                }

                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let (status, body) = match files.get(path) {
                    Some(body) => ("200 OK", body.as_slice()),
                    None => ("404 Not Found", b"missing".as_slice()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
            });
        }
    });

    format!("http://{address}")
}

struct Release {
    version: &'static str,
    archive: Vec<u8>,
    sha256: Option<String>,
    size: Option<u64>,
}

impl Release {
    fn of(version: &'static str) -> Self {
        Self {
            version,
            archive: archive(&executable(version)),
            sha256: None,
            size: None,
        }
    }

    /// Publishes the release, signed with a new key.
    fn publish(self) -> (String, Vec<PublicKey>) {
        let pair = KeyPair::generate_unencrypted_keypair().unwrap();
        let version = self.version;
        let sha256 = self.sha256.unwrap_or_else(|| sha256(&self.archive));
        let size = self.size.unwrap_or(self.archive.len() as u64);
        let manifest = format!(
            r#"{{"schema":1,"name":"partsrelay-tunnel","version":"{version}","tag":"v{version}",
            "assets":{{"{TARGET}":{{"name":"{ARCHIVE}","sha256":"{sha256}","size":{size}}}}}}}"#
        );
        let signature = minisign::sign(None, &pair.sk, Cursor::new(&manifest), None, None).unwrap();
        let files = HashMap::from([
            (
                "/latest/download/manifest.json".to_string(),
                manifest.into_bytes(),
            ),
            (
                "/latest/download/manifest.json.minisig".to_string(),
                signature.to_string().into_bytes(),
            ),
            (format!("/download/v{version}/{ARCHIVE}"), self.archive),
        ]);
        let keys = manifest::public_keys(&pair.pk.to_box().unwrap().to_string()).unwrap();

        (serve(files), keys)
    }
}

struct Installed {
    _directory: tempfile::TempDir,
    exe: PathBuf,
}

/// An install of version 1.0.0 in its own directory.
fn installed() -> Installed {
    let directory = tempfile::tempdir().unwrap();
    let exe = directory.path().join("partsrelay-tunnel");

    fs::write(&exe, executable("1.0.0")).unwrap();

    Installed {
        _directory: directory,
        exe,
    }
}

fn updater(exe: &Path, base_url: String, keys: Vec<PublicKey>) -> Updater {
    Updater {
        current: Version::new(1, 0, 0),
        target: TARGET.into(),
        base_url,
        keys,
        exe: exe.to_path_buf(),
        proxy: None,
    }
}

fn reported(exe: &Path) -> String {
    let output = Command::new("sh")
        .arg(exe)
        .arg("--version")
        .output()
        .unwrap();

    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn unchanged(install: &Installed) -> bool {
    let files: Vec<_> = fs::read_dir(install.exe.parent().unwrap())
        .unwrap()
        .collect();

    files.len() == 1 && fs::read(&install.exe).unwrap() == executable("1.0.0")
}

#[test]
fn installs_a_newer_signed_release_and_keeps_the_previous_one() {
    let install = installed();
    let (url, keys) = Release::of("1.2.0").publish();
    let updater = updater(&install.exe, url, keys);

    assert_eq!(updater.update().unwrap(), Some(Version::new(1, 2, 0)));

    assert_eq!(
        reported(&install.exe),
        "partsrelay-tunnel 1.2.0 (test-target)"
    );
    assert_eq!(
        reported(&previous(&install.exe)),
        "partsrelay-tunnel 1.0.0 (test-target)"
    );
    assert_eq!(
        updater.exe.metadata().unwrap().len(),
        executable("1.2.0").len() as u64
    );
}

#[test]
fn leaves_a_release_that_is_not_newer() {
    for version in ["1.0.0", "0.9.9"] {
        let install = installed();
        let (url, keys) = Release::of(version).publish();

        assert_eq!(
            updater(&install.exe, url, keys).update().unwrap(),
            None,
            "{version}"
        );
        assert!(unchanged(&install));
    }
}

#[test]
fn refuses_a_release_signed_with_another_key() {
    let install = installed();
    let (url, _) = Release::of("1.2.0").publish();
    let (_, other_keys) = Release::of("1.2.0").publish();

    let error = updater(&install.exe, url, other_keys).update().unwrap_err();

    assert!(error.to_string().contains("signature"), "{error:#}");
    assert!(unchanged(&install));
}

#[test]
fn refuses_an_archive_that_is_not_the_one_of_the_manifest() {
    let cases = [
        (
            Release {
                sha256: Some("00".repeat(32)),
                ..Release::of("1.2.0")
            },
            "SHA-256",
        ),
        (
            Release {
                size: Some(10),
                ..Release::of("1.2.0")
            },
            "larger than",
        ),
        (
            Release {
                size: Some(1 << 20),
                ..Release::of("1.2.0")
            },
            "wrong size",
        ),
    ];

    for (release, reason) in cases {
        let install = installed();
        let (url, keys) = release.publish();
        let error = updater(&install.exe, url, keys).update().unwrap_err();

        assert!(
            format!("{error:#}").contains(reason),
            "{error:#} instead of {reason}"
        );
        assert!(unchanged(&install));
    }
}

#[test]
fn refuses_an_executable_that_is_another_version() {
    let install = installed();
    let release = Release {
        archive: archive(&executable("1.1.0")),
        ..Release::of("1.2.0")
    };
    let (url, keys) = release.publish();

    let error = updater(&install.exe, url, keys).update().unwrap_err();

    assert!(
        error.to_string().contains("does not report version 1.2.0"),
        "{error:#}"
    );
    assert!(unchanged(&install));
}

#[test]
fn tells_when_the_release_has_no_build_for_this_target() {
    let install = installed();
    let (url, keys) = Release::of("1.2.0").publish();
    let mut updater = updater(&install.exe, url, keys);
    updater.target = "riscv64gc-unknown-linux-gnu".into();

    let error = updater.update().unwrap_err();

    assert!(
        error
            .to_string()
            .contains("no build for riscv64gc-unknown-linux-gnu"),
        "{error:#}"
    );
    assert!(unchanged(&install));
}

#[test]
fn fails_without_touching_anything_when_the_release_is_unreachable() {
    let install = installed();
    let (_, keys) = Release::of("1.2.0").publish();
    let nowhere = serve(HashMap::new());

    assert!(updater(&install.exe, nowhere, keys).update().is_err());
    assert!(unchanged(&install));
}

fn has(tool: &str) -> bool {
    Command::new(tool).arg("-v").output().is_ok()
}

/// The release job rehearsed: the script, minisign, then an update.
#[test]
fn installs_what_the_release_scripts_publish() {
    if !has("minisign") || !has("jq") {
        eprintln!("skipped: minisign and jq are needed");
        return;
    }

    let work = tempfile::tempdir().unwrap();
    let dist = work.path().join("dist");
    let target = "x86_64-unknown-linux-musl";
    fs::create_dir(&dist).unwrap();
    for name in [
        "x86_64-unknown-linux-musl.tar.gz",
        "aarch64-unknown-linux-musl.tar.gz",
        "x86_64-apple-darwin.tar.gz",
        "aarch64-apple-darwin.tar.gz",
        "x86_64-pc-windows-msvc.zip",
        "aarch64-pc-windows-msvc.zip",
    ] {
        fs::write(
            dist.join(format!("partsrelay-tunnel-{name}")),
            archive(&executable("1.2.0")),
        )
        .unwrap();
    }

    let run = |command: &mut Command| {
        let output = command.current_dir(work.path()).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/make-manifest.sh");
    let manifest = run(Command::new("bash").args([script, "1.2.0", "v1.2.0", "dist"]));
    fs::write(dist.join("manifest.json"), &manifest).unwrap();
    run(Command::new("minisign").args([
        "-G",
        "-W",
        "-f",
        "-p",
        "release.pub",
        "-s",
        "release.key",
    ]));
    run(Command::new("minisign").args(["-S", "-s", "release.key", "-m", "dist/manifest.json"]));
    run(Command::new("minisign").args(["-V", "-p", "release.pub", "-m", "dist/manifest.json"]));

    let file = |name: &str| fs::read(dist.join(name)).unwrap();
    let archive_name = format!("partsrelay-tunnel-{target}.tar.gz");
    let url = serve(HashMap::from([
        ("/latest/download/manifest.json".to_string(), manifest),
        (
            "/latest/download/manifest.json.minisig".to_string(),
            file("manifest.json.minisig"),
        ),
        (
            format!("/download/v1.2.0/{archive_name}"),
            file(&archive_name),
        ),
    ]));
    let keys = manifest::public_keys(&fs::read_to_string(work.path().join("release.pub")).unwrap());
    let install = installed();
    let mut updater = updater(&install.exe, url, keys.unwrap());
    updater.target = target.into();

    assert_eq!(updater.update().unwrap(), Some(Version::new(1, 2, 0)));
    assert_eq!(
        reported(&install.exe),
        "partsrelay-tunnel 1.2.0 (test-target)"
    );
}
