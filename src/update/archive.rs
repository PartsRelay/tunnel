use std::io::Read;

use anyhow::{Context, Result, bail};

/// Largest executable an archive may hold.
pub const MAX_BINARY: u64 = 64 * 1024 * 1024;

pub fn binary_name() -> &'static str {
    if cfg!(windows) {
        "partsrelay-tunnel.exe"
    } else {
        "partsrelay-tunnel"
    }
}

pub fn archive_name(target: &str) -> String {
    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };

    format!("partsrelay-tunnel-{target}.{extension}")
}

fn capped(mut entry: impl Read) -> Result<Vec<u8>> {
    let mut binary = Vec::new();
    entry
        .by_ref()
        .take(MAX_BINARY + 1)
        .read_to_end(&mut binary)?;

    if binary.len() as u64 > MAX_BINARY {
        bail!("the executable in the archive is too large");
    }

    Ok(binary)
}

/// Reads only the executable out of a release archive.
#[cfg(unix)]
pub fn extract(archive: &[u8]) -> Result<Vec<u8>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));

    for entry in archive.entries().context("the archive is not a tar.gz")? {
        let entry = entry.context("the archive is damaged")?;
        let named = entry
            .path()?
            .file_name()
            .is_some_and(|name| name == binary_name());

        if named && entry.header().entry_type().is_file() {
            return capped(entry);
        }
    }

    bail!("the archive has no {}", binary_name())
}

#[cfg(windows)]
pub fn extract(archive: &[u8]) -> Result<Vec<u8>> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(archive)).context("the archive is not a zip")?;
    let entry = archive
        .by_name(binary_name())
        .with_context(|| format!("the archive has no {}", binary_name()))?;

    capped(entry)
}

#[cfg(all(test, unix))]
mod tests {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    pub fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));

        for (path, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, *content).unwrap();
        }

        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn takes_the_executable_and_nothing_else() {
        let archive = tarball(&[("README.md", b"read me"), ("partsrelay-tunnel", b"binary")]);

        assert_eq!(extract(&archive).unwrap(), b"binary");
    }

    #[test]
    fn refuses_an_archive_without_the_executable() {
        let archive = tarball(&[("README.md", b"read me"), ("other", b"binary")]);

        assert!(extract(&archive).is_err());
        assert!(extract(b"not an archive").is_err());
    }

    #[test]
    fn names_the_archive_of_each_target() {
        assert_eq!(
            archive_name("aarch64-unknown-linux-musl"),
            "partsrelay-tunnel-aarch64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            archive_name("x86_64-pc-windows-msvc"),
            "partsrelay-tunnel-x86_64-pc-windows-msvc.zip"
        );
    }
}
