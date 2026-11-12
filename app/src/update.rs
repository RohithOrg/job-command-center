//! Updates: once a day the app reads the suite's public catalog (one small JSON file on GitHub),
//! and when a newer version is listed the user can install it from inside the app. The installer
//! is downloaded, its SHA-256 and signature are checked against the release key built into the
//! app, and only then is it put in place and the app restarted. Nothing is sent anywhere; nothing
//! is installed without the user pressing the button.
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const APP_ID: &str = "job-command-center";
pub const CATALOG_URL: &str = "https://raw.githubusercontent.com/RohithOrg/releases/main/catalog.json";
pub const RELEASES_PAGE: &str = "https://github.com/RohithOrg/releases/releases";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const PUBLIC_KEY: &str = include_str!("../assets/release-key.pub");

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct Catalog { apps: Vec<AppEntry> }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct AppEntry { id: String, latest: Option<Release> }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Release {
    pub version: String,
    pub notes: String,
    /// By platform: "macos-aarch64", "windows-x86_64", "linux-x86_64".
    pub assets: BTreeMap<String, Asset>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Asset { pub url: String, pub size: u64, pub sha256: String, pub signature: String }

/// A newer release that has an installer for this computer.
#[derive(Clone, Debug)]
pub struct Available { pub version: String, pub notes: String, pub asset: Asset }

pub fn platform() -> String {
    let os = match std::env::consts::OS { "macos" => "macos", "windows" => "windows", _ => "linux" };
    format!("{os}-{}", std::env::consts::ARCH)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("JobCommandCenter/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

/// "0.1.4" > "0.1.3"; anything unparsable counts as older.
pub fn newer(latest: &str, installed: &str) -> bool {
    match (semver::Version::parse(latest.trim_start_matches('v')), semver::Version::parse(installed.trim_start_matches('v'))) {
        (Ok(l), Ok(i)) => l > i,
        (Ok(_), Err(_)) => true,
        _ => false,
    }
}

/// Ask the catalog whether a newer version exists for this computer.
pub async fn check() -> Result<Option<Available>> {
    let source = std::env::var("JCC_CATALOG").unwrap_or_else(|_| CATALOG_URL.to_string());
    let bytes = if source.starts_with("http") {
        let r = client().get(&source).timeout(Duration::from_secs(20)).send().await.context("couldn't reach the update list")?;
        if !r.status().is_success() { bail!("the update list answered {}", r.status()); }
        r.bytes().await?.to_vec()
    } else {
        std::fs::read(&source)?
    };
    let cat: Catalog = serde_json::from_slice(&bytes).context("the update list is damaged")?;
    let Some(rel) = cat.apps.into_iter().find(|a| a.id == APP_ID).and_then(|a| a.latest) else { return Ok(None) };
    if !newer(&rel.version, VERSION) { return Ok(None); }
    let Some(asset) = rel.assets.get(&platform()).cloned() else { return Ok(None) };
    Ok(Some(Available { version: rel.version, notes: rel.notes, asset }))
}

fn signed_message(version: &str, platform: &str, sha256: &str) -> String {
    format!("{APP_ID}\n{version}\n{platform}\n{}", sha256.to_lowercase())
}

fn verify(version: &str, asset: &Asset, hash: &str, len: u64) -> Result<()> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    if asset.size > 0 && len != asset.size { bail!("the download is incomplete ({len} of {} bytes)", asset.size); }
    if !hash.eq_ignore_ascii_case(asset.sha256.trim()) { bail!("the download doesn't match the published file"); }
    let key: [u8; 32] = hex::decode(PUBLIC_KEY.trim()).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| anyhow!("the app's release key is damaged"))?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| anyhow!("the app's release key is damaged"))?;
    let sig: [u8; 64] = hex::decode(asset.signature.trim()).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| anyhow!("the installer isn't signed"))?;
    key.verify(signed_message(version, &platform(), hash).as_bytes(), &Signature::from_bytes(&sig))
        .map_err(|_| anyhow!("the installer's signature doesn't match. Nothing was installed"))
}

/// Download the installer while hashing it; `progress` gets 0.0..1.0.
async fn download(dir: &Path, asset: &Asset, progress: &(dyn Fn(f32) + Send + Sync)) -> Result<(PathBuf, String, u64)> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    let name = asset.url.rsplit('/').next().filter(|n| !n.is_empty() && !n.contains("..")).unwrap_or("installer");
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    let r = client().get(&asset.url).send().await.context("couldn't start the download")?;
    if !r.status().is_success() { bail!("the download answered {}", r.status()); }
    let total = r.content_length().unwrap_or(asset.size).max(1);
    let mut file = std::fs::File::create(&path)?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut stream = r.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("the download was interrupted")?;
        std::io::Write::write_all(&mut file, &chunk)?;
        hasher.update(&chunk);
        done += chunk.len() as u64;
        progress(done as f32 / total as f32);
    }
    Ok((path, hex::encode(hasher.finalize()), done))
}

/// Download, verify and install `a`, then arrange for the app to restart. The caller quits the
/// event loop when this returns Ok.
pub async fn install(data_dir: PathBuf, a: Available, progress: impl Fn(f32) + Send + Sync + 'static) -> Result<()> {
    let (file, hash, len) = download(&data_dir.join("Updates"), &a.asset, &progress).await?;
    verify(&a.version, &a.asset, &hash, len)?;
    let f = file.clone();
    tokio::task::spawn_blocking(move || put_in_place(&f)).await??;
    relaunch(&file)
}

// ---------- macOS: swap the .app bundle, reopen it ----------

#[cfg(target_os = "macos")]
fn own_bundle() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let app = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()).map(Path::to_path_buf)
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| anyhow!("this copy isn't installed as an app; download the new version from {RELEASES_PAGE}"))?;
    Ok(app)
}

#[cfg(target_os = "macos")]
fn put_in_place(file: &Path) -> Result<()> {
    let target = own_bundle()?;
    let mount = std::env::temp_dir().join(format!("jcc-update-{}", std::process::id()));
    let _ = Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(&mount).status();
    std::fs::create_dir_all(&mount)?;
    let ok = Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet", "-mountpoint"]).arg(&mount).arg(file).status()?;
    if !ok.success() { bail!("couldn't open the downloaded disk image"); }
    let result = (|| {
        let source = std::fs::read_dir(&mount)?.flatten().map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .ok_or_else(|| anyhow!("the disk image has no app in it"))?;
        let parent = target.parent().ok_or_else(|| anyhow!("bad install location"))?;
        let bundle = target.file_name().unwrap_or_default().to_string_lossy().to_string();
        let staged = parent.join(format!(".{bundle}.new"));
        let old = parent.join(format!(".{bundle}.old"));
        let _ = std::fs::remove_dir_all(&staged);
        // ditto keeps the code signature, symlinks and permissions intact.
        if !Command::new("ditto").arg(&source).arg(&staged).status()?.success() {
            bail!("couldn't copy the new version into {}", parent.display());
        }
        let _ = std::fs::remove_dir_all(&old);
        std::fs::rename(&target, &old).context("couldn't replace the current version")?;
        if let Err(e) = std::fs::rename(&staged, &target) {
            let _ = std::fs::rename(&old, &target);
            return Err(e).context("couldn't put the new version in place");
        }
        let _ = std::fs::remove_dir_all(&old);
        Ok(())
    })();
    let _ = Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(&mount).status();
    let _ = std::fs::remove_dir(&mount);
    result
}

#[cfg(target_os = "macos")]
fn relaunch(_file: &Path) -> Result<()> {
    let bundle = own_bundle()?;
    Command::new("sh").arg("-c").arg(format!("sleep 1; open -n '{}'", bundle.display().to_string().replace('\'', "'\\''"))).spawn()?;
    Ok(())
}

// ---------- Windows: the installer runs after the app quits, then starts it again ----------

#[cfg(windows)]
fn put_in_place(_file: &Path) -> Result<()> { Ok(()) }

#[cfg(windows)]
fn relaunch(file: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    Command::new("cmd")
        .args(["/C", &format!("timeout /t 2 /nobreak >NUL & \"{}\" /S & start \"\" \"{}\"", file.display(), exe.display())])
        .spawn()?;
    Ok(())
}

// ---------- Linux: replace the AppImage beside itself, start it again ----------

#[cfg(all(unix, not(target_os = "macos")))]
fn own_appimage() -> Result<PathBuf> {
    std::env::var_os("APPIMAGE").map(PathBuf::from)
        .ok_or_else(|| anyhow!("this copy isn't running as an AppImage; download the new version from {RELEASES_PAGE}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn put_in_place(file: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let target = own_appimage()?;
    let staged = target.with_extension("AppImage.new");
    std::fs::copy(file, &staged)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&staged, &target)?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn relaunch(_file: &Path) -> Result<()> {
    let me = own_appimage()?;
    Command::new("sh").arg("-c").arg(format!("sleep 1; '{}' &", me.display().to_string().replace('\'', "'\\''"))).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("0.1.4", "0.1.3"));
        assert!(newer("v1.0.0", "0.9.9"));
        assert!(!newer("0.1.3", "0.1.3"));
        assert!(!newer("0.1.2", "0.1.3"));
    }

    #[test]
    fn release_key_is_valid() {
        let key: [u8; 32] = hex::decode(PUBLIC_KEY.trim()).unwrap().try_into().unwrap();
        ed25519_dalek::VerifyingKey::from_bytes(&key).unwrap();
    }
}
