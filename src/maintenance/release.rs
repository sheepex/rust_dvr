use super::{Error, Result};
use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::TempPath;

const API: &str = "https://api.github.com/repos/sheepex/rust_dvr/releases/latest";
const DOWNLOAD_ROOT: &str = "https://github.com/sheepex/rust_dvr/releases/download";
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET: Option<&str> = Some("rustdvr-update-linux-x86_64");
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const ASSET: Option<&str> = Some("rustdvr-update-windows-x86_64.exe");
#[cfg(not(target_arch = "x86_64"))]
const ASSET: Option<&str> = None;

#[derive(Deserialize)]
struct ReleaseInfo {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
}

pub(super) struct Release {
    pub(super) version: Version,
    url: String,
    size: u64,
    sha256: String,
}

pub(super) struct Download {
    pub(super) version: Version,
    pub(super) target: PathBuf,
    pub(super) old_sha256: String,
    pub(super) sha256: String,
    pub(super) staged: TempPath,
}

pub(super) fn supported() -> bool {
    ASSET.is_some()
}

fn client(timeout: Duration) -> Result<Client> {
    Ok(Client::builder()
        .https_only(true)
        .user_agent(concat!("RustDVR/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(8))
        .timeout(timeout)
        .build()?)
}

pub(super) fn latest() -> Result<Option<Release>> {
    let Some(asset_name) = ASSET else {
        return Ok(None);
    };
    let response = client(Duration::from_secs(15))?
        .get(API)
        .header("Accept", "application/vnd.github+json")
        .send()?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let info = serde_json::from_reader(response.error_for_status()?.take(1024 * 1024))?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    select(info, &current, asset_name)
}

fn select(info: ReleaseInfo, current: &Version, asset_name: &str) -> Result<Option<Release>> {
    let Some(version) = info
        .tag_name
        .strip_prefix('v')
        .and_then(|tag| Version::parse(tag).ok())
    else {
        return Ok(None);
    };
    if info.draft
        || info.prerelease
        || !version.pre.is_empty()
        || !version.cmp_precedence(current).is_gt()
    {
        return Ok(None);
    }
    let asset = info
        .assets
        .into_iter()
        .find(|asset| asset.name == asset_name)
        .ok_or("A release mais recente não oferece atualização automática para esta plataforma.")?;
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("O GitHub não informou um SHA-256 válido para o executável.")?;
    if !(1..=MAX_ASSET_BYTES).contains(&asset.size) {
        return Err("O tamanho do executável publicado é inválido.".into());
    }
    Ok(Some(Release {
        version,
        url: format!("{DOWNLOAD_ROOT}/{}/{asset_name}", info.tag_name),
        size: asset.size,
        sha256: sha256.to_owned(),
    }))
}

impl Release {
    pub(super) fn download(self, target: PathBuf) -> Result<Download> {
        let old_sha256 = file_sha256(&target)?;
        let mut staged = super::stage_executable(&target)?;
        let response = client(Duration::from_secs(180))?
            .get(self.url)
            .send()?
            .error_for_status()?;
        write_verified(response, staged.as_file_mut(), self.size, &self.sha256)?;
        super::finish_executable(staged.as_file())?;
        Ok(Download {
            version: self.version,
            target,
            old_sha256,
            sha256: self.sha256,
            staged: staged.into_temp_path(),
        })
    }
}

pub(super) fn file_sha256(path: &Path) -> io::Result<String> {
    let mut hash = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn write_verified(
    source: impl Read,
    output: &mut File,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<()> {
    let size = io::copy(&mut source.take(expected_size + 1), output)?;
    output.flush()?;
    if size != expected_size {
        return Err(Error::from(
            "O download tem tamanho diferente do informado pelo GitHub.",
        ));
    }
    let mut hash = Sha256::new();
    let mut file = &*output;
    use std::io::Seek;
    file.rewind()?;
    io::copy(&mut file, &mut hash)?;
    if !format!("{:x}", hash.finalize()).eq_ignore_ascii_case(expected_sha256) {
        return Err("O SHA-256 do download não corresponde ao informado pelo GitHub.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_a_newer_stable_release_with_an_integrity_digest() {
        let info = |tag: &str, digest: Option<String>| ReleaseInfo {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: "rustdvr-test".into(),
                size: 1,
                digest,
            }],
        };
        let current = Version::new(0, 1, 9);
        let digest = format!("sha256:{}", "a".repeat(64));
        let release = select(
            info("v0.1.10", Some(digest.clone())),
            &current,
            "rustdvr-test",
        )
        .unwrap()
        .unwrap();
        assert_eq!(release.version, Version::new(0, 1, 10));
        for tag in ["v0.1.9", "v0.1.2", "v0.2.0-beta.1", "latest"] {
            assert!(
                select(info(tag, Some(digest.clone())), &current, "rustdvr-test")
                    .unwrap()
                    .is_none()
            );
        }
        assert!(select(info("v0.2.0", None), &current, "rustdvr-test").is_err());
        assert!(select(info("v0.2.0", Some(digest)), &current, "another-asset").is_err());
    }

    #[test]
    fn detects_modified_and_incomplete_downloads() {
        let bytes = b"executable";
        let digest = format!("{:x}", Sha256::digest(bytes));
        let mut file = tempfile::tempfile().unwrap();
        assert!(write_verified(&bytes[..], &mut file, bytes.len() as u64, &digest).is_ok());

        for (input, size, sha256) in [
            (&bytes[..], bytes.len() as u64 + 1, digest.as_str()),
            (&bytes[..], bytes.len() as u64 - 1, digest.as_str()),
            (&bytes[..], bytes.len() as u64, "invalid"),
        ] {
            let mut file = tempfile::tempfile().unwrap();
            assert!(write_verified(input, &mut file, size, sha256).is_err());
        }
    }
}
