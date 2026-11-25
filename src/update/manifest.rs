use std::collections::BTreeMap;

use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::Deserialize;

use crate::NAME;

/// What a release holds, signed as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub name: String,
    pub version: Version,
    pub tag: String,
    pub assets: BTreeMap<String, Asset>,
}

/// The archive of one target.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("the signature of the manifest is not valid")]
    Signature,
    #[error("the manifest is not the expected JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the manifest is not a release of {NAME}: {0}")]
    Content(&'static str),
}

/// Reads the public keys of a `minisign.pub` file.
pub fn public_keys(text: &str) -> Result<Vec<PublicKey>, minisign_verify::Error> {
    Ok(vec![PublicKey::decode(text.trim())?])
}

/// Accepts the manifest only if one key signed it.
pub fn verify(
    manifest: &[u8],
    signature: &str,
    keys: &[PublicKey],
) -> Result<Manifest, VerifyError> {
    let signature = Signature::decode(signature).map_err(|_| VerifyError::Signature)?;

    if !keys
        .iter()
        .any(|key| key.verify(manifest, &signature, false).is_ok())
    {
        return Err(VerifyError::Signature);
    }

    let manifest: Manifest = serde_json::from_slice(manifest)?;
    let plain = |name: &str| !name.is_empty() && !name.contains(['/', '\\']) && name != "..";

    if manifest.schema != 1 {
        return Err(VerifyError::Content("unknown schema"));
    }
    if manifest.name != NAME {
        return Err(VerifyError::Content("another name"));
    }
    if !manifest.version.pre.is_empty() {
        return Err(VerifyError::Content("a pre-release"));
    }
    if manifest.tag != format!("v{}", manifest.version) {
        return Err(VerifyError::Content("the tag is not its version"));
    }
    if !manifest.assets.values().all(|asset| plain(&asset.name)) {
        return Err(VerifyError::Content("an asset name with a path"));
    }

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use minisign::KeyPair;

    use super::*;

    const MANIFEST: &str = r#"{"schema":1,"name":"partsrelay-tunnel","version":"0.3.1","tag":"v0.3.1",
        "assets":{"x86_64-unknown-linux-musl":{"name":"partsrelay-tunnel-x86_64-unknown-linux-musl.tar.gz",
        "sha256":"00","size":3}}}"#;

    fn signed(manifest: &str) -> (String, Vec<PublicKey>, KeyPair) {
        let pair = KeyPair::generate_unencrypted_keypair().unwrap();
        let signature = minisign::sign(None, &pair.sk, Cursor::new(manifest), None, None).unwrap();
        let keys = public_keys(&pair.pk.to_box().unwrap().to_string()).unwrap();

        (signature.to_string(), keys, pair)
    }

    #[test]
    fn accepts_a_manifest_signed_by_one_of_the_keys() {
        let (signature, keys, _) = signed(MANIFEST);
        let (_, others, _) = signed(MANIFEST);
        let both: Vec<PublicKey> = others.into_iter().chain(keys).collect();

        let manifest = verify(MANIFEST.as_bytes(), &signature, &both).unwrap();

        assert_eq!(manifest.version, Version::new(0, 3, 1));
        assert_eq!(manifest.assets["x86_64-unknown-linux-musl"].size, 3);
    }

    #[test]
    fn refuses_a_changed_manifest_another_key_or_no_signature() {
        let (signature, keys, _) = signed(MANIFEST);
        let (_, others, _) = signed(MANIFEST);
        let changed = MANIFEST.replace("0.3.1", "0.3.2");

        assert!(matches!(
            verify(changed.as_bytes(), &signature, &keys),
            Err(VerifyError::Signature)
        ));
        assert!(matches!(
            verify(MANIFEST.as_bytes(), &signature, &others),
            Err(VerifyError::Signature)
        ));
        assert!(matches!(
            verify(MANIFEST.as_bytes(), "", &keys),
            Err(VerifyError::Signature)
        ));
        assert!(matches!(
            verify(MANIFEST.as_bytes(), &signature, &[]),
            Err(VerifyError::Signature)
        ));
    }

    #[test]
    fn refuses_a_signed_manifest_of_something_else() {
        let content = |manifest: String| {
            let (signature, keys, _) = signed(&manifest);
            match verify(manifest.as_bytes(), &signature, &keys) {
                Err(VerifyError::Content(reason)) => reason,
                other => panic!("unexpected {other:?}"),
            }
        };

        assert_eq!(
            content(MANIFEST.replace("\"schema\":1", "\"schema\":2")),
            "unknown schema"
        );
        assert_eq!(
            content(MANIFEST.replace("partsrelay-tunnel\",\"v", "other\",\"v")),
            "another name"
        );
        assert_eq!(
            content(MANIFEST.replace("0.3.1", "0.3.1-rc.1")),
            "a pre-release"
        );
        assert_eq!(
            content(MANIFEST.replace("v0.3.1", "v9.9.9")),
            "the tag is not its version"
        );
        assert_eq!(
            content(MANIFEST.replace("partsrelay-tunnel-x86", "../partsrelay-tunnel-x86")),
            "an asset name with a path"
        );
    }

    #[test]
    fn reads_the_public_key_kept_in_the_repository() {
        assert_eq!(
            public_keys(include_str!("../../minisign.pub"))
                .unwrap()
                .len(),
            1
        );
    }
}
