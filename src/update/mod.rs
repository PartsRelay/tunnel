use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use data_encoding::HEXLOWER;
use minisign_verify::PublicKey;
use ring::digest::{SHA256, digest};
use semver::Version;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::Config;
use crate::proxy::{self, ProxyUrl};
use crate::{TARGET, VERSION};

pub mod archive;
pub mod fetch;
pub mod install;
pub mod manifest;

pub use manifest::Manifest;

/// Releases of the tunnel, never read from the configuration.
pub const RELEASES_URL: &str = "https://github.com/PartsRelay/tunnel/releases";
const RELEASES_HOST: &str = "github.com";
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_SIGNATURE: u64 = 4 * 1024;
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// Finds, verifies and installs a newer signed release.
pub struct Updater {
    pub current: Version,
    pub target: String,
    pub base_url: String,
    pub keys: Vec<PublicKey>,
    pub exe: PathBuf,
    pub proxy: Option<ProxyUrl>,
}

impl Updater {
    /// The updater of this executable, with the key built in.
    pub fn embedded(cfg: &Config) -> Result<Self> {
        let env = |name: &str| std::env::var(name).ok();

        Ok(Self {
            current: Version::parse(VERSION)?,
            target: TARGET.into(),
            base_url: RELEASES_URL.into(),
            keys: manifest::public_keys(include_str!("../../minisign.pub"))
                .map_err(|error| anyhow!("the built in public key is not valid: {error}"))?,
            exe: std::env::current_exe().context("cannot find this executable")?,
            proxy: proxy::resolve(cfg.proxy.as_ref(), env, RELEASES_HOST),
        })
    }

    /// The manifest of the latest release, if it is newer.
    pub fn check(&self) -> Result<Option<Manifest>> {
        let agent = fetch::agent(self.proxy.as_ref())?;
        let url = format!("{}/latest/download/manifest.json", self.base_url);
        let manifest = fetch::get(&agent, &url, MAX_MANIFEST)?;
        let signature = fetch::get(&agent, &format!("{url}.minisig"), MAX_SIGNATURE)?;
        let signature = String::from_utf8(signature).context("the signature is not text")?;
        let manifest = manifest::verify(&manifest, &signature, &self.keys)?;

        // Never a downgrade, even if signed
        Ok((manifest.version > self.current).then_some(manifest))
    }

    /// Downloads, checks and puts in place the executable of `manifest`.
    pub fn apply(&self, manifest: &Manifest) -> Result<()> {
        let asset = manifest.assets.get(&self.target).with_context(|| {
            format!("release {} has no build for {}", manifest.tag, self.target)
        })?;
        let agent = fetch::agent(self.proxy.as_ref())?;
        let url = format!("{}/download/{}/{}", self.base_url, manifest.tag, asset.name);
        let archive = fetch::get(&agent, &url, asset.size)?;

        if archive.len() as u64 != asset.size {
            bail!("{} has the wrong size", asset.name);
        }
        if HEXLOWER.encode(digest(&SHA256, &archive).as_ref()) != asset.sha256.to_ascii_lowercase()
        {
            bail!("{} does not match its SHA-256", asset.name);
        }

        let binary = archive::extract(&archive)?;
        let staged = install::stage(&self.exe, &binary)
            .with_context(|| format!("cannot write next to {}", self.exe.display()))?;
        let installed = install::preflight(&staged, &manifest.version).and_then(|()| {
            install::replace_exe(&self.exe, &staged).context("cannot replace the executable")
        });

        if installed.is_err() {
            let _ = std::fs::remove_file(&staged);
        }

        installed
    }

    /// Installs the latest release if it is newer.
    pub fn update(&self) -> Result<Option<Version>> {
        match self.check()? {
            Some(manifest) => {
                self.apply(&manifest)?;
                Ok(Some(manifest.version))
            }
            None => Ok(None),
        }
    }
}

fn jitter(base: Duration, spread: Duration) -> Duration {
    base + Duration::from_secs(fastrand::u64(0..=spread.as_secs()))
}

/// Checks now and then. True once an update is installed.
pub async fn watch(updater: Updater, stop: CancellationToken) -> bool {
    let updater = std::sync::Arc::new(updater);
    let mut wait = jitter(Duration::from_secs(5), Duration::from_secs(25));

    loop {
        tokio::select! {
            _ = sleep(wait) => {}
            _ = stop.cancelled() => return false,
        }

        let checking = updater.clone();
        let outcome = tokio::task::spawn_blocking(move || checking.update()).await;

        wait = match outcome {
            Ok(Ok(Some(version))) => {
                info!(%version, "update installed, restarting");
                return true;
            }
            Ok(Ok(None)) => jitter(
                CHECK_EVERY - Duration::from_secs(30 * 60),
                Duration::from_secs(60 * 60),
            ),
            Ok(Err(error)) => {
                warn!(error = format!("{error:#}"), "update check failed");
                jitter(RETRY_AFTER, Duration::from_secs(10 * 60))
            }
            Err(error) => {
                warn!(%error, "update check failed");
                jitter(RETRY_AFTER, Duration::from_secs(10 * 60))
            }
        };
    }
}
