//! The developer disk image. The accessibility, screen and input services iph
//! uses come from it, and the phone forgets it when it restarts.
//!
//! iph does not ship Apple's image. It uses Xcode's copy when the Mac has one,
//! and otherwise fetches the Cryptex variant once from the public
//! DeveloperDiskImage repository (pinned commit, every file checked against
//! its SHA-256) into its own folder. Apple's signing server personalizes it
//! for the phone either way.
//!
//! The Cryptex route (`cryptexd` over the tunnel) is the one that works from
//! iOS 17.4 on and is the only one for phones newer than the image's own
//! device list (iPhone18,5 for Xcode 27.1's image).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use idevice::services::cryptexd::{Cryptex1Assets, install_ddi, installed_ddi};
use sha2::{Digest, Sha256};

use crate::{device::conn::Tunnel, paths};

const REPO_COMMIT: &str = "a1020f81b6429f5ce40408cbe21c690512b613b5";
const REPO_DIR: &str = "PersonalizedImages/Xcode_iOS_DDI_Cryptex";

/// The files of the Cryptex image at `REPO_COMMIT`, with their SHA-256.
const FILES: [(&str, &str); 5] = [
    ("BuildManifest.plist", "537391eb2f37e604fc693f422c1568af97220067853e663391ba3e8258569d6e"),
    ("Image.dmg", "500d4139f7963650e6c796d950267ed38909e7d183ad26bba073257529bfe173"),
    ("Image.dmg.cryptex_info", "0180ee20ce5daa7978fe38f24c742361726789ea6af82c3c1d2afc480734a531"),
    ("Image.dmg.root_hash", "5f3281961396be5fe008e11641ffc25c7db24c11d3789ff0c4ff0fa6d47b94ca"),
    ("Image.dmg.trustcache", "2719aa3f1c99245432512116fb8a56d55ec224c706e545772d4c17c1233ac475"),
];

/// Where Xcode keeps the image (macOS only).
pub fn xcode_restore() -> Option<PathBuf> {
    [
        PathBuf::from("/Library/Developer/DeveloperDiskImages/iOS_DDI/Restore"),
        paths::home().join("Library/Developer/DeveloperDiskImages/iOS_DDI/Restore"),
    ]
    .into_iter()
    .find(|p| p.join("BuildManifest.plist").exists())
}

fn sha256_of(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

fn cache_ok(dir: &Path) -> bool {
    FILES
        .iter()
        .all(|(name, sum)| sha256_of(&dir.join(name)).is_ok_and(|s| s == *sum))
}

/// The fetched image's folder, fetching it first when it is missing or does
/// not match. Uses the system's `curl` (macOS, and Windows 10 and later).
pub async fn fetched() -> Result<PathBuf> {
    let dir = paths::ddi_cache().join(&REPO_COMMIT[..12]);
    if cache_ok(&dir) {
        return Ok(dir);
    }
    paths::ensure_private_dir(&dir)?;
    for (name, sum) in FILES {
        let target = dir.join(name);
        if sha256_of(&target).is_ok_and(|s| s == sum) {
            continue;
        }
        let url = format!("https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/{REPO_COMMIT}/{REPO_DIR}/{name}");
        let partial = dir.join(format!("{name}.part"));
        let status = tokio::process::Command::new("curl")
            .args(["-fsSL", "--retry", "2", "-o"])
            .arg(&partial)
            .arg(&url)
            .status()
            .await
            .context("run curl")?;
        if !status.success() {
            bail!("download {name} failed ({status})");
        }
        let got = sha256_of(&partial)?;
        if got != sum {
            let _ = std::fs::remove_file(&partial);
            bail!("{name}: checksum {got} does not match {sum}");
        }
        std::fs::rename(&partial, &target)?;
    }
    Ok(dir)
}

/// Install the image through `cryptexd`. Returns where the image came from.
pub async fn install(tunnel: &mut Tunnel) -> Result<String> {
    let mut adapter = tunnel.adapter.clone();
    if installed_ddi(&mut adapter, &mut tunnel.rsd).await.ok().flatten().is_some() {
        return Ok("already installed".into());
    }
    // Xcode's copy first; it is the same image when Xcode is current.
    let mut errors = Vec::new();
    let mut sources = Vec::new();
    if let Some(dir) = xcode_restore() {
        sources.push(("Xcode", dir));
    }
    match fetched().await {
        Ok(dir) => sources.push(("DeveloperDiskImage repository", dir)),
        Err(e) => errors.push(format!("fetch: {e:#}")),
    }
    for (label, dir) in sources {
        let assets = match Cryptex1Assets::load(&dir).await {
            Ok(a) => a,
            Err(e) => {
                errors.push(format!("{label}: {e:?}"));
                continue;
            }
        };
        let mut adapter = tunnel.adapter.clone();
        match install_ddi(&mut adapter, &mut tunnel.rsd, &assets).await {
            Ok(_) => return Ok(label.to_string()),
            Err(e) => errors.push(format!("{label}: {e:?}")),
        }
    }
    Err(anyhow!(errors.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Network: fetches about 18 MB. `cargo test ddi -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn fetch_and_load() {
        let dir = std::env::temp_dir().join(format!("iph-ddi-{}", std::process::id()));
        unsafe { std::env::set_var("IPH_STATE_DIR", &dir) };
        let restore = fetched().await.unwrap();
        assert!(cache_ok(&restore));
        let assets = Cryptex1Assets::load(&restore).await.unwrap();
        assert!(assets.image.len() > 1_000_000);
        // A second call finds the cache and downloads nothing.
        assert_eq!(fetched().await.unwrap(), restore);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
