#[cfg(unix)]
mod common;

use std::io::Write;
use std::process::Command;

const TUNNEL: &str = env!("CARGO_BIN_EXE_partsrelay-tunnel");

fn config_file(content: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();

    file.write_all(content.as_bytes()).unwrap();
    file
}

#[test]
fn reports_its_version_and_target() {
    let output = Command::new(TUNNEL).arg("--version").output().unwrap();
    let printed = String::from_utf8(output.stdout).unwrap();
    let mut words = printed.split_whitespace();

    assert!(output.status.success());
    assert_eq!(words.next(), Some("partsrelay-tunnel"));
    assert_eq!(words.next(), Some(env!("CARGO_PKG_VERSION")));
    assert!(
        words
            .next()
            .is_some_and(|target| target.starts_with('(') && target.ends_with(')'))
    );
}

#[test]
fn ends_with_code_2_on_a_broken_configuration() {
    let file = config_file("token = \"prt\"\nallow = [\"erp.local\"]\n");
    let output = Command::new(TUNNEL)
        .arg("--config")
        .arg(file.path())
        .output()
        .unwrap();
    let logged = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2));
    assert!(logged.contains("erp.local: expected host:port"), "{logged}");
}

// SIGTERM is how systemd and launchd stop the tunnel
#[cfg(unix)]
mod process {
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;
    use tokio::time::sleep;

    use super::common::{Connectors, TOKEN, echo, read_all};
    use super::{TUNNEL, config_file};

    async fn exit_code(child: &mut Child) -> Option<i32> {
        for _ in 0..100 {
            if let Some(status) = child.try_wait().unwrap() {
                return status.code();
            }
            sleep(Duration::from_millis(100)).await;
        }

        let _ = child.kill();
        panic!("the tunnel did not stop in time");
    }

    #[tokio::test]
    async fn relays_as_a_process_and_stops_cleanly_on_sigterm() {
        let (erp, _) = echo().await;
        let mut connectors = Connectors::start().await;
        let file = config_file(&format!(
            "server = \"{}\"\ntoken = \"{TOKEN}\"\nallow = [\"{erp}\"]\nauto_update = false\n",
            connectors.url()
        ));
        let mut child = Command::new(TUNNEL)
            .arg("--config")
            .arg(file.path())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let mut session = connectors.tunnel().await;
        let (mut stream, response) = session.open(&erp.to_string()).await;
        assert!(response.ok);
        stream.write_all(b"from a process").await.unwrap();
        assert_eq!(read_all(&mut stream, 14).await, b"from a process");
        stream.shutdown().await.unwrap();
        drop(stream);

        Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status()
            .unwrap();

        assert_eq!(exit_code(&mut child).await, Some(0));
        let logged = std::io::read_to_string(child.stderr.take().unwrap()).unwrap();
        assert!(logged.contains("connected"), "{logged}");
        assert!(!logged.contains(TOKEN), "the token reached the logs");
    }
}
