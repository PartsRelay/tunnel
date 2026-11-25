use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use semver::Version;

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let extension = exe.extension().filter(|extension| *extension == "exe");
    let mut name = match extension {
        Some(_) => exe.file_stem().unwrap_or_default().to_os_string(),
        None => exe.file_name().unwrap_or_default().to_os_string(),
    };

    name.push(format!(".{suffix}"));
    if extension.is_some() {
        name.push(".exe");
    }

    exe.with_file_name(name)
}

/// Where the previous executable is kept after an update.
pub fn previous(exe: &Path) -> PathBuf {
    sibling(exe, "old")
}

/// Whether an update can be written next to the executable.
pub fn writable(exe: &Path) -> bool {
    exe.parent()
        .is_some_and(|directory| tempfile::tempfile_in(directory).is_ok())
}

/// Writes the new executable next to the running one.
pub fn stage(exe: &Path, binary: &[u8]) -> io::Result<PathBuf> {
    let staged = sibling(exe, "new");

    fs::write(&staged, binary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }

    Ok(staged)
}

/// Runs the staged executable once: it must report `version`.
pub fn preflight(staged: &Path, version: &Version) -> Result<()> {
    let mut attempts = 0;
    let output = loop {
        match Command::new(staged).arg("--version").output() {
            // Another thread may still hold the file open
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy && attempts < 5 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            output => break output.with_context(|| format!("cannot run {}", staged.display()))?,
        }
    };
    let printed = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() || printed.split_whitespace().nth(1) != Some(&version.to_string()) {
        bail!("the downloaded executable does not report version {version}");
    }

    Ok(())
}

/// Puts the staged executable in place, keeping the previous one.
#[cfg(unix)]
pub fn replace_exe(exe: &Path, staged: &Path) -> io::Result<()> {
    let old = previous(exe);

    let _ = fs::remove_file(&old);
    fs::hard_link(exe, &old)?;
    // Atomic: the path always holds an executable
    fs::rename(staged, exe).inspect_err(|_| {
        let _ = fs::remove_file(&old);
    })
}

/// Windows cannot overwrite a running executable, only rename it.
#[cfg(windows)]
pub fn replace_exe(exe: &Path, staged: &Path) -> io::Result<()> {
    let old = previous(exe);

    let _ = fs::remove_file(&old);
    fs::rename(exe, &old)?;
    fs::rename(staged, exe).inspect_err(|_| {
        let _ = fs::rename(&old, exe);
    })
}

/// Starts the installed version in place of this process.
#[cfg(unix)]
pub fn restart(exe: &Path, args: &[std::ffi::OsString]) -> io::Error {
    use std::os::unix::process::CommandExt;

    Command::new(exe).args(args).exec()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn names_the_staged_and_the_previous_executable() {
        assert_eq!(
            sibling(Path::new("/opt/bin/partsrelay-tunnel"), "new"),
            Path::new("/opt/bin/partsrelay-tunnel.new")
        );
        assert_eq!(
            previous(Path::new("C:/bin/partsrelay-tunnel.exe")),
            Path::new("C:/bin/partsrelay-tunnel.old.exe")
        );
    }

    #[test]
    fn replaces_the_executable_and_keeps_the_previous_one() {
        let directory = tempfile::tempdir().unwrap();
        let exe = directory.path().join("partsrelay-tunnel");

        fs::write(&exe, b"old").unwrap();
        let staged = stage(&exe, b"new").unwrap();
        replace_exe(&exe, &staged).unwrap();

        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert_eq!(fs::read(previous(&exe)).unwrap(), b"old");
        assert!(!staged.exists());
    }

    #[test]
    fn leaves_the_executable_alone_when_nothing_is_staged() {
        let directory = tempfile::tempdir().unwrap();
        let exe = directory.path().join("partsrelay-tunnel");

        fs::write(&exe, b"old").unwrap();

        assert!(replace_exe(&exe, &directory.path().join("missing")).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old");
        assert!(!previous(&exe).exists());
    }
}
