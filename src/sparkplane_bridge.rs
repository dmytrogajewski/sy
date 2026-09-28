//! Process-only Sparkplane integration; no model, engine or appliance policy.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs,
    io::Read,
    os::unix::{
        fs::{symlink, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
};

const BRIDGE_PROTOCOL: &str = "sparkplane.bridge/v1";
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const PROTOCOL_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const PROTOCOL_BUSY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Integrations {
    pub sparkplane: Option<Integration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Integration {
    #[serde(default)]
    pub enabled: bool,
    pub release: Option<ReleasePin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePin {
    pub version: String,
    pub target: String,
    pub sha256: String,
    pub public_key: String,
}

impl ReleasePin {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.version.is_empty()
                && self.version.len() < 80
                && self
                    .version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)),
            "invalid Sparkplane version"
        );
        ensure!(
            matches!(
                self.target.as_str(),
                "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu"
            ),
            "unsupported Sparkplane target"
        );
        ensure!(
            self.target.starts_with(std::env::consts::ARCH),
            "Sparkplane target does not match this machine"
        );
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invalid Sparkplane SHA-256"
        );
        minisign_verify::PublicKey::from_base64(&self.public_key)
            .context("invalid Sparkplane public key")?;
        Ok(())
    }

    fn artifact(&self) -> String {
        format!("sparkplane-{}", self.target)
    }
    fn directory(&self) -> String {
        format!("{}-{}-{}", self.version, self.target, self.sha256)
    }
}

pub fn data_root() -> Result<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
        .context("HOME or XDG_DATA_HOME must be set")?;
    ensure!(
        data.is_absolute(),
        "Sparkplane data directory must be absolute"
    );
    Ok(data.join("sparkplane/client"))
}

fn verify(pin: &ReleasePin, manifest: &[u8], signature: &str, binary: &[u8]) -> Result<()> {
    pin.validate()?;
    minisign_verify::PublicKey::from_base64(&pin.public_key)?
        .verify(
            manifest,
            &minisign_verify::Signature::decode(signature)?,
            false,
        )
        .context("Sparkplane release signature verification failed")?;
    let artifact = pin.artifact();
    let matches: Vec<_> = std::str::from_utf8(manifest)?
        .lines()
        .filter_map(|line| {
            let (digest, path) = line.split_once("  ")?;
            (path == artifact).then_some(digest)
        })
        .collect();
    ensure!(
        matches == [pin.sha256.as_str()],
        "Sparkplane signed inventory disagrees with release pin"
    );
    ensure!(
        format!("{:x}", Sha256::digest(binary)) == pin.sha256,
        "Sparkplane executable digest mismatch"
    );
    Ok(())
}

pub fn prepare(root: &Path, args: &[OsString]) -> Result<Command> {
    let current = fs::canonicalize(root.join("current"))
        .context("Sparkplane is not installed; enable integrations.sparkplane and run sy apply")?;
    ensure!(
        current.starts_with(fs::canonicalize(root.join("releases"))?),
        "Sparkplane current link escapes managed releases"
    );
    let pin: ReleasePin =
        serde_json::from_slice(&fs::read(current.join("release.json")).context(
            "Sparkplane is not installed; enable integrations.sparkplane and run sy apply",
        )?)?;
    let binary = current.join("sparkplane");
    verify(
        &pin,
        &fs::read(current.join("SHA256SUMS"))?,
        &fs::read_to_string(current.join("SHA256SUMS.minisig"))?,
        &fs::read(&binary)?,
    )?;
    verify_protocol(&binary)?;
    let mut command = Command::new(binary);
    command.args(args);
    for (name, value) in std::env::vars_os() {
        if let Some(suffix) = name
            .to_str()
            .and_then(|name| name.strip_prefix("SY_SPARK_"))
        {
            let canonical = format!("SPARKPLANE_{suffix}");
            if std::env::var_os(&canonical).is_none() {
                command.env(canonical, value);
            }
        }
    }
    Ok(command)
}

fn verify_protocol(binary: &Path) -> Result<()> {
    let started = std::time::Instant::now();
    // Concurrent process creation can briefly retain an inherited writable fd.
    // ETXTBSY means exec never started; no other errors or protocol failures retry.
    let protocol = loop {
        match Command::new(binary).arg("--bridge-protocol").output() {
            Err(error)
                if error.raw_os_error() == Some(libc::ETXTBSY)
                    && started.elapsed() < PROTOCOL_BUSY_TIMEOUT =>
            {
                std::thread::sleep(PROTOCOL_BUSY_INTERVAL);
            }
            result => break result.context("inspect Sparkplane bridge protocol")?,
        }
    };
    ensure!(
        protocol.status.success() && protocol.stdout == format!("{BRIDGE_PROTOCOL}\n").as_bytes(),
        "incompatible Sparkplane bridge protocol; update the pin and run sy apply"
    );
    Ok(())
}

pub fn dispatch(args: &[OsString]) -> Result<()> {
    let error = prepare(&data_root()?, args)?.exec();
    Err(error).context("execute the verified Sparkplane client")
}

fn fetch(url: &str, limit: u64) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let mut bytes = Vec::new();
    client
        .get(url)
        .send()?
        .error_for_status()?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Sparkplane release asset exceeds its size limit"
    );
    Ok(bytes)
}

pub fn apply_cli(integration: Option<&Integration>, dry: bool, json: bool) -> Result<()> {
    let enabled = integration.is_some_and(|config| config.enabled);
    if enabled {
        apply(integration, &data_root()?, dry)?;
    }
    if json {
        println!(
            "{}",
            serde_json::json!({"schema":"sy.integration-apply/v1", "integration":"sparkplane", "enabled":enabled, "dry_run":dry})
        );
    }
    Ok(())
}

pub fn apply(integration: Option<&Integration>, root: &Path, dry: bool) -> Result<()> {
    let Some(config) = integration.filter(|config| config.enabled) else {
        return Ok(());
    };
    let pin = config
        .release
        .as_ref()
        .context("enabled Sparkplane integration requires a signed release pin")?;
    pin.validate()?;
    if dry {
        eprintln!(
            "Sparkplane: install verified {} ({})",
            pin.version, pin.target
        );
        return Ok(());
    }
    let receipt = root.join("current/release.json");
    if receipt.try_exists()? {
        let installed: ReleasePin = serde_json::from_slice(&fs::read(receipt)?)?;
        if installed == *pin {
            prepare(root, &[])?;
            return Ok(());
        }
    }
    let url = format!(
        "https://github.com/Sumatoshi-tech/sparkplane/releases/download/v{}",
        pin.version
    );
    let manifest = fetch(&format!("{url}/SHA256SUMS"), 1024 * 1024)?;
    let signature = String::from_utf8(fetch(&format!("{url}/SHA256SUMS.minisig"), 16 * 1024)?)?;
    let binary = fetch(&format!("{url}/{}", pin.artifact()), MAX_BINARY_BYTES)?;
    install_verified(root, pin, &manifest, &signature, &binary)
}

pub fn install_verified(
    root: &Path,
    pin: &ReleasePin,
    manifest: &[u8],
    signature: &str,
    binary: &[u8],
) -> Result<()> {
    verify(pin, manifest, signature, binary)?;
    fs::create_dir_all(root.join("releases"))?;
    let destination = root.join("releases").join(pin.directory());
    if destination.exists() {
        let installed: ReleasePin =
            serde_json::from_slice(&fs::read(destination.join("release.json"))?)?;
        ensure!(
            installed == *pin,
            "existing Sparkplane receipt disagrees with release pin"
        );
        verify(
            pin,
            &fs::read(destination.join("SHA256SUMS"))?,
            &fs::read_to_string(destination.join("SHA256SUMS.minisig"))?,
            &fs::read(destination.join("sparkplane"))?,
        )?;
        ensure!(
            fs::read(destination.join("sparkplane"))? == binary,
            "existing Sparkplane release is corrupt; refusing overwrite"
        );
    } else {
        let stage = root
            .join("releases")
            .join(format!(".stage-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&stage)?;
        let result = (|| -> Result<()> {
            for (name, bytes) in [
                ("sparkplane", binary),
                ("SHA256SUMS", manifest),
                ("SHA256SUMS.minisig", signature.as_bytes()),
                ("release.json", &serde_json::to_vec(pin)?),
            ] {
                fs::write(stage.join(name), bytes)?;
                fs::set_permissions(
                    stage.join(name),
                    fs::Permissions::from_mode(if name == "sparkplane" { 0o555 } else { 0o444 }),
                )?;
                fs::File::open(stage.join(name))?.sync_all()?;
            }
            verify_protocol(&stage.join("sparkplane"))?;
            fs::File::open(&stage)?.sync_all()?;
            fs::rename(&stage, &destination)?;
            fs::File::open(root.join("releases"))?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            bail!(
                "Sparkplane staging failed at {}: {error:#}",
                stage.display()
            );
        }
    }
    verify_protocol(&destination.join("sparkplane"))?;
    let link = root.join(format!(".current-{}", uuid::Uuid::new_v4()));
    symlink(Path::new("releases").join(pin.directory()), &link)?;
    fs::rename(link, root.join("current"))?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}
