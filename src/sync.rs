//! Cross-machine sync: `wizard sync` packs the portable parts of `~/.wizard`
//! (config.toml, mcp.toml, the system prompt, and the skills/, commands/,
//! subagents/, tools/ directories) into a signed `.tar.gz` bundle, and pulls
//! one back on another machine.
//!
//! A bundle holds `manifest.json` (the file list with sha256s, plus the
//! signer's ed25519 public key), `manifest.sig` (a signature over the exact
//! manifest bytes), and the files under `payload/`. Trust is TOFU like SSH:
//! the first `pull` pins the signer's key into `~/.wizard/sync/trusted_keys`;
//! later pulls must be signed by a pinned key. Compare fingerprints out of
//! band with `wizard sync key`. Pulls are additive/overwrite only — replaced
//! files are backed up under `~/.wizard/sync/backups/`, nothing is deleted —
//! and every verification failure aborts before a single byte is written.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD as BASE64_NO_PAD};
use chrono::Utc;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::cli::SyncCmd;
use crate::config::Config;
use crate::update::sha256_hex;

/// Bundle format version; bump on incompatible manifest changes.
const BUNDLE_VERSION: u32 = 1;

/// HTTP connect timeout for a URL pull (mirrors `update::download_and_install`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Bundle format
// ---------------------------------------------------------------------------

/// `manifest.json`: everything needed to verify the payload. The signature in
/// `manifest.sig` covers the exact serialized bytes of this file, so the file
/// list, hashes, and embedded public key are all tamper-evident.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    created_at: String,
    wizard_version: String,
    host: String,
    includes_credentials: bool,
    /// base64 of the signer's raw 32-byte ed25519 public key.
    public_key: String,
    files: Vec<ManifestEntry>,
}

/// One payload file: its path relative to `~/.wizard`, sha256 (lowercase
/// hex), and size in bytes.
#[derive(Debug, Serialize, Deserialize)]
struct ManifestEntry {
    path: String,
    sha256: String,
    size: u64,
}

/// A parsed-but-unverified bundle: the raw manifest bytes (the signature
/// covers these exactly), the base64 signature line, and the payload files
/// keyed by their `~/.wizard`-relative path.
struct RawBundle {
    manifest_bytes: Vec<u8>,
    signature: String,
    payload: BTreeMap<String, Vec<u8>>,
}

// ---------------------------------------------------------------------------
// What gets packed
// ---------------------------------------------------------------------------

/// The portable file set, as paths relative to `wizard_dir`. Resolved from
/// the [`Config`] path helpers in production ([`SyncPaths::resolve`]); tests
/// build one over a temp dir. Anything not listed here — sessions, logs,
/// models, memory, sync state itself — never enters a bundle.
struct SyncPaths {
    wizard_dir: PathBuf,
    /// Single files (skipped silently when missing).
    files: Vec<PathBuf>,
    /// Directories packed recursively (skipped silently when missing).
    dirs: Vec<PathBuf>,
    /// Secret files, packed only with `--include-credentials`.
    credential_files: Vec<PathBuf>,
}

impl SyncPaths {
    /// Resolve against the real `~/.wizard`, deriving the relative names from
    /// the canonical path helpers so a moved file can never silently desync.
    fn resolve() -> Result<Self> {
        let wizard_dir = Config::wizard_dir()?;
        let rel = |path: PathBuf| -> Result<PathBuf> {
            Ok(path
                .strip_prefix(&wizard_dir)
                .with_context(|| {
                    format!("{} is not under {}", path.display(), wizard_dir.display())
                })?
                .to_path_buf())
        };
        Ok(Self {
            files: vec![
                rel(Config::path()?)?,
                rel(Config::mcp_config_path()?)?,
                rel(Config::system_prompt_path()?)?,
            ],
            dirs: vec![
                rel(Config::skills_dir()?)?,
                // User slash commands; no dedicated Config helper (discovery
                // in `crate::commands` joins the same name).
                PathBuf::from("commands"),
                rel(Config::subagents_dir()?)?,
                rel(Config::scripted_tools_dir()?)?,
            ],
            credential_files: vec![
                rel(crate::credentials::path()?)?,
                rel(crate::llm::xai_oauth::token_path()?)?,
            ],
            wizard_dir,
        })
    }
}

/// Relative paths that hold secrets: written 0600 on pull, and packed only
/// with `--include-credentials`.
fn is_credential_path(path: &str) -> bool {
    path == "credentials.toml" || path == "xai_oauth.json"
}

/// Collect the bundle payload: file contents keyed by `~/.wizard`-relative
/// path (forward slashes). Missing files and directories are skipped without
/// error — a fresh machine packs whatever it has.
fn collect_entries(
    paths: &SyncPaths,
    include_credentials: bool,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut entries = BTreeMap::new();
    for rel in &paths.files {
        add_file(&paths.wizard_dir, rel, &mut entries)?;
    }
    if include_credentials {
        for rel in &paths.credential_files {
            add_file(&paths.wizard_dir, rel, &mut entries)?;
        }
    }
    for rel in &paths.dirs {
        add_dir(&paths.wizard_dir, rel, &mut entries)?;
    }
    Ok(entries)
}

/// Add one regular file (following symlinks); anything else is skipped.
fn add_file(wizard_dir: &Path, rel: &Path, entries: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    let abs = wizard_dir.join(rel);
    match std::fs::metadata(&abs) {
        Ok(meta) if meta.is_file() => {
            let data = std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?;
            entries.insert(rel_string(rel)?, data);
        }
        // Missing, or not a regular file: skip without error.
        _ => {}
    }
    Ok(())
}

/// Recursively add a directory's regular files. A missing directory is fine;
/// unreadable entries (e.g. broken symlinks) are skipped.
fn add_dir(
    wizard_dir: &Path,
    rel_dir: &Path,
    entries: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let abs = wizard_dir.join(rel_dir);
    let read_dir = match std::fs::read_dir(&abs) {
        Ok(read_dir) => read_dir,
        Err(_) => return Ok(()),
    };
    for entry in read_dir {
        let entry = entry.with_context(|| format!("listing {}", abs.display()))?;
        let rel = rel_dir.join(entry.file_name());
        let Ok(meta) = std::fs::metadata(entry.path()) else {
            continue; // broken symlink or the like
        };
        if meta.is_dir() {
            add_dir(wizard_dir, &rel, entries)?;
        } else if meta.is_file() {
            add_file(wizard_dir, &rel, entries)?;
        }
    }
    Ok(())
}

/// A relative path as the forward-slash string stored in the manifest.
fn rel_string(rel: &Path) -> Result<String> {
    Ok(rel
        .to_str()
        .with_context(|| format!("non-UTF-8 path under ~/.wizard: {}", rel.display()))?
        .to_string())
}

// ---------------------------------------------------------------------------
// Keys and trust
// ---------------------------------------------------------------------------

/// `~/.wizard/sync/` — signing key, trusted keys, pull backups.
fn sync_dir(wizard_dir: &Path) -> PathBuf {
    wizard_dir.join("sync")
}

/// `~/.wizard/sync/key` — base64 of this machine's 32-byte ed25519 seed.
fn key_path(wizard_dir: &Path) -> PathBuf {
    sync_dir(wizard_dir).join("key")
}

/// `~/.wizard/sync/trusted_keys` — one base64 public key per line, optional
/// ` # comment` suffix.
fn trusted_keys_path(wizard_dir: &Path) -> PathBuf {
    sync_dir(wizard_dir).join("trusted_keys")
}

/// Load the signing key, generating (and persisting, 0600) a fresh one when
/// the key file does not exist yet. The seed comes straight from the OS RNG.
fn load_or_generate_key(wizard_dir: &Path) -> Result<SigningKey> {
    let path = key_path(wizard_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let bytes = BASE64
                .decode(raw.trim())
                .with_context(|| format!("decoding {}", path.display()))?;
            let seed: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| anyhow!("{} does not hold a base64 32-byte seed", path.display()))?;
            Ok(SigningKey::from_bytes(&seed))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).map_err(|err| anyhow!("gathering key randomness: {err}"))?;
            write_secret_atomic(&path, format!("{}\n", BASE64.encode(seed)).as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(SigningKey::from_bytes(&seed))
        }
        Err(err) => Err(anyhow!(err).context(format!("reading {}", path.display()))),
    }
}

/// OpenSSH-style fingerprint of a public key: `SHA256:` + unpadded base64 of
/// sha256 over the raw 32 key bytes.
fn fingerprint(key: &VerifyingKey) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key.to_bytes());
    format!("SHA256:{}", BASE64_NO_PAD.encode(hasher.finalize()))
}

/// Decode a manifest's base64 public key into a verifying key.
fn decode_public_key(b64: &str) -> Result<VerifyingKey> {
    let bytes = BASE64
        .decode(b64)
        .context("decoding the bundle's public key")?;
    let raw: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("the bundle's public key is not 32 bytes"))?;
    VerifyingKey::from_bytes(&raw)
        .map_err(|_| anyhow!("the bundle's public key is not a valid ed25519 key"))
}

/// The keys in a `trusted_keys` file: one base64 key per line, `#` starts a
/// comment, blank lines ignored.
fn parse_trusted_keys(raw: &str) -> Vec<String> {
    raw.lines()
        .filter_map(|line| {
            let key = line.split('#').next().unwrap_or("").trim();
            (!key.is_empty()).then(|| key.to_string())
        })
        .collect()
}

/// Outcome of the trust check for a verified signing key.
#[derive(Debug, PartialEq, Eq)]
enum Trust {
    /// The key is already pinned in `trusted_keys`.
    AlreadyTrusted,
    /// First use: no keys were pinned yet, so this one was pinned now.
    Pinned,
    /// Dry run of the first-use case: the key would be pinned, nothing written.
    WouldPin,
}

/// Enforce the TOFU trust model for a bundle key that already passed
/// signature verification. Empty/missing `trusted_keys` pins the key (unless
/// `dry_run`); a non-empty file must already list it.
fn check_trust(
    wizard_dir: &Path,
    public_key: &str,
    fingerprint: &str,
    source: &str,
    dry_run: bool,
) -> Result<Trust> {
    let path = trusted_keys_path(wizard_dir);
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let keys = parse_trusted_keys(&raw);
    if keys.iter().any(|key| key == public_key) {
        return Ok(Trust::AlreadyTrusted);
    }
    if !keys.is_empty() {
        bail!(
            "the bundle is signed by an UNTRUSTED key.\n  fingerprint: {fingerprint}\n\
             To trust it: run `wizard sync key` on the source machine, compare its\n\
             fingerprint with the one above, and if they match add this line to\n\
             {}:\n  {public_key}",
            path.display()
        );
    }
    if dry_run {
        return Ok(Trust::WouldPin);
    }
    ensure_sync_dir(wizard_dir)?;
    let line = format!(
        "{public_key} # pinned {} from {source}\n",
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(Trust::Pinned)
}

/// Create `~/.wizard/sync/` with 0700 permissions (mirrors
/// `credentials::write_at`).
fn ensure_sync_dir(wizard_dir: &Path) -> Result<()> {
    let dir = sync_dir(wizard_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting permissions on {}", dir.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Atomic writes
// ---------------------------------------------------------------------------

/// Write `data` to `dest` atomically: a temp file in the same directory
/// (created as needed), then a rename over the target.
fn write_file_atomic(dest: &Path, data: &[u8], secret: bool) -> Result<()> {
    let dir = dest
        .parent()
        .with_context(|| format!("{} has no parent directory", dest.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{} has no usable file name", dest.display()))?;
    let tmp = dir.join(format!(".{name}.sync.tmp"));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        // create(true) keeps the mode of a pre-existing file; enforce 0600.
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restricting permissions on {}", tmp.display()))?;
        }
        file.write_all(data)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, dest).with_context(|| format!("moving {} into place", dest.display()))?;
    Ok(())
}

/// Atomic write with 0600 perms and a 0700 parent (the key file).
fn write_secret_atomic(dest: &Path, data: &[u8]) -> Result<()> {
    let dir = dest
        .parent()
        .with_context(|| format!("{} has no parent directory", dest.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting permissions on {}", dir.display()))?;
    }
    write_file_atomic(dest, data, true)
}

// ---------------------------------------------------------------------------
// Pack
// ---------------------------------------------------------------------------

/// What `pack` produced, for the CLI summary.
struct PackOutcome {
    file_count: usize,
    payload_bytes: u64,
    bundle_bytes: u64,
    includes_credentials: bool,
    fingerprint: String,
}

/// Pack the portable file set into a signed bundle at `out`.
fn pack(paths: &SyncPaths, out: &Path, include_credentials: bool) -> Result<PackOutcome> {
    let key = load_or_generate_key(&paths.wizard_dir)?;
    let entries = collect_entries(paths, include_credentials)?;
    if entries.is_empty() {
        bail!(
            "nothing to pack — no portable files found under {}",
            paths.wizard_dir.display()
        );
    }
    // Reflect what actually landed, not just the flag: with the flag set but
    // no credential files on disk, the bundle carries no secrets.
    let includes_credentials = entries.keys().any(|path| is_credential_path(path));

    let manifest = Manifest {
        version: BUNDLE_VERSION,
        created_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        wizard_version: crate::update::current_version().to_string(),
        host: host_name(),
        includes_credentials,
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        files: entries
            .iter()
            .map(|(path, data)| ManifestEntry {
                path: path.clone(),
                sha256: sha256_hex(data),
                size: data.len() as u64,
            })
            .collect(),
    };
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).context("serializing the bundle manifest")?;
    let signature = BASE64.encode(key.sign(&manifest_bytes).to_bytes());
    let bundle = assemble_bundle(&manifest_bytes, &signature, &entries)?;

    // A bundle carrying API keys is itself a secret; write it 0600.
    write_file_atomic(out, &bundle, includes_credentials)
        .with_context(|| format!("writing {}", out.display()))?;

    Ok(PackOutcome {
        file_count: entries.len(),
        payload_bytes: entries.values().map(|data| data.len() as u64).sum(),
        bundle_bytes: bundle.len() as u64,
        includes_credentials,
        fingerprint: fingerprint(&key.verifying_key()),
    })
}

/// Serialize a bundle: gzip over a tar of `manifest.json`, `manifest.sig`,
/// and `payload/<path>` entries.
fn assemble_bundle(
    manifest_bytes: &[u8],
    signature: &str,
    payload: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    append_entry(&mut tar, "manifest.json", manifest_bytes)?;
    append_entry(
        &mut tar,
        "manifest.sig",
        format!("{signature}\n").as_bytes(),
    )?;
    for (path, data) in payload {
        append_entry(&mut tar, &format!("payload/{path}"), data)?;
    }
    let gz = tar.into_inner().context("finalizing the bundle tar")?;
    gz.finish().context("finalizing the bundle gzip")
}

/// Append one in-memory file to the bundle tar.
fn append_entry<W: Write>(tar: &mut tar::Builder<W>, path: &str, data: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(Utc::now().timestamp().max(0) as u64);
    header.set_cksum();
    tar.append_data(&mut header, path, data)
        .with_context(|| format!("adding {path} to the bundle"))
}

/// Best-effort hostname for the manifest; `unknown` when undeterminable.
fn host_name() -> String {
    if let Ok(host) = std::env::var("HOSTNAME")
        && !host.trim().is_empty()
    {
        return host.trim().to_string();
    }
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(raw) = std::fs::read_to_string(path)
            && !raw.trim().is_empty()
        {
            return raw.trim().to_string();
        }
    }
    "unknown".to_string()
}

// ---------------------------------------------------------------------------
// Pull: parse and verify
// ---------------------------------------------------------------------------

/// Split a bundle into manifest bytes, signature, and payload map. Structural
/// checks only (plus path safety on payload names); cryptographic and hash
/// verification happen in [`verify_bundle`].
fn parse_bundle(bytes: &[u8]) -> Result<RawBundle> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    let mut manifest_bytes = None;
    let mut signature = None;
    let mut payload: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for entry in archive.entries().context("reading the bundle")? {
        let mut entry = entry.context("reading a bundle entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = {
            let raw = entry.path().context("a bundle entry has a bad path")?;
            raw.to_str()
                .context("a bundle entry path is not UTF-8")?
                .to_string()
        };
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut data)
            .with_context(|| format!("reading {path} from the bundle"))?;
        match path.as_str() {
            "manifest.json" => manifest_bytes = Some(data),
            "manifest.sig" => {
                signature = Some(
                    String::from_utf8(data)
                        .context("manifest.sig is not UTF-8")?
                        .trim()
                        .to_string(),
                )
            }
            _ => {
                let Some(rel) = path.strip_prefix("payload/") else {
                    bail!("unexpected file {path:?} in the bundle");
                };
                validate_rel_path(rel)?;
                if payload.insert(rel.to_string(), data).is_some() {
                    bail!("duplicate payload entry {rel:?} in the bundle");
                }
            }
        }
    }
    Ok(RawBundle {
        manifest_bytes: manifest_bytes.context("the bundle has no manifest.json")?,
        signature: signature.context("the bundle has no manifest.sig")?,
        payload,
    })
}

/// Reject any bundle path that could escape `~/.wizard`: absolute paths,
/// `..` components, backslashes, and anything that is not a chain of plain
/// path components.
fn validate_rel_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("the bundle contains an empty file path");
    }
    if path.contains('\\') {
        bail!("bundle path {path:?} contains a backslash");
    }
    let rel = Path::new(path);
    if rel.is_absolute() {
        bail!("bundle path {path:?} is absolute");
    }
    for component in rel.components() {
        match component {
            Component::Normal(_) => {}
            Component::ParentDir => bail!("bundle path {path:?} contains a `..` component"),
            _ => bail!("bundle path {path:?} is not a plain relative path"),
        }
    }
    Ok(())
}

/// Full verification: manifest signature (over the exact manifest bytes,
/// against the embedded public key), manifest path safety, exact
/// payload/manifest correspondence, and per-file size + sha256. Any failure
/// is a hard error; nothing is written anywhere.
fn verify_bundle(raw: &RawBundle) -> Result<Manifest> {
    let manifest: Manifest =
        serde_json::from_slice(&raw.manifest_bytes).context("parsing manifest.json")?;
    if manifest.version != BUNDLE_VERSION {
        bail!(
            "unsupported bundle version {} (this wizard understands version {BUNDLE_VERSION}) \
             — update wizard on this machine",
            manifest.version
        );
    }

    let key = decode_public_key(&manifest.public_key)?;
    let sig_bytes = BASE64
        .decode(&raw.signature)
        .context("decoding manifest.sig")?;
    let signature = Signature::from_slice(&sig_bytes)
        .map_err(|_| anyhow!("manifest.sig is not a 64-byte ed25519 signature"))?;
    key.verify_strict(&raw.manifest_bytes, &signature)
        .map_err(|_| {
            anyhow!(
                "bundle signature verification FAILED — the manifest does not match its \
             signature; refusing to touch ~/.wizard"
            )
        })?;

    let mut listed: BTreeMap<&str, &ManifestEntry> = BTreeMap::new();
    for entry in &manifest.files {
        validate_rel_path(&entry.path)?;
        if listed.insert(entry.path.as_str(), entry).is_some() {
            bail!("the manifest lists {:?} twice", entry.path);
        }
    }
    for path in raw.payload.keys() {
        if !listed.contains_key(path.as_str()) {
            bail!("payload file {path:?} is not listed in the manifest");
        }
    }
    for (path, entry) in &listed {
        let Some(data) = raw.payload.get(*path) else {
            bail!("the manifest lists {path:?} but the payload does not contain it");
        };
        if data.len() as u64 != entry.size {
            bail!(
                "size mismatch for {path:?}: the manifest says {} bytes, the payload has {}",
                entry.size,
                data.len()
            );
        }
        if sha256_hex(data) != entry.sha256.to_ascii_lowercase() {
            bail!("sha256 mismatch for {path:?} — the bundle is corrupt or tampered with");
        }
    }
    Ok(manifest)
}

// ---------------------------------------------------------------------------
// Pull: diff and apply
// ---------------------------------------------------------------------------

/// How a payload file relates to what is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileState {
    New,
    Changed,
    Identical,
}

impl FileState {
    fn label(self) -> &'static str {
        match self {
            FileState::New => "new",
            FileState::Changed => "changed",
            FileState::Identical => "identical",
        }
    }
}

/// Classify every payload file against the live `wizard_dir`.
fn diff_against(
    wizard_dir: &Path,
    payload: &BTreeMap<String, Vec<u8>>,
) -> Vec<(String, FileState)> {
    payload
        .iter()
        .map(|(path, data)| {
            let state = match std::fs::read(wizard_dir.join(path)) {
                Ok(existing) if existing == *data => FileState::Identical,
                Ok(_) => FileState::Changed,
                Err(_) => FileState::New,
            };
            (path.clone(), state)
        })
        .collect()
}

/// What `apply` did, for the CLI summary.
struct ApplyOutcome {
    applied: usize,
    unchanged: usize,
    backup_dir: Option<PathBuf>,
}

/// Write every new/changed payload file into `wizard_dir`, backing up any
/// existing version first. Additive only: files on disk that are not in the
/// bundle are never touched.
fn apply(
    wizard_dir: &Path,
    payload: &BTreeMap<String, Vec<u8>>,
    diff: &[(String, FileState)],
) -> Result<ApplyOutcome> {
    let backup_root = sync_dir(wizard_dir)
        .join("backups")
        .join(Utc::now().format("%Y%m%d-%H%M%S").to_string());
    let mut outcome = ApplyOutcome {
        applied: 0,
        unchanged: 0,
        backup_dir: None,
    };
    for (path, state) in diff {
        if *state == FileState::Identical {
            outcome.unchanged += 1;
            continue;
        }
        let dest = wizard_dir.join(path);
        if dest.exists() {
            let backup = backup_root.join(path);
            let dir = backup
                .parent()
                .with_context(|| format!("{} has no parent directory", backup.display()))?;
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            std::fs::copy(&dest, &backup).with_context(|| {
                format!("backing up {} to {}", dest.display(), backup.display())
            })?;
            outcome.backup_dir = Some(backup_root.clone());
        }
        let data = payload
            .get(path)
            .with_context(|| format!("payload lost {path:?}"))?; // unreachable: diff comes from payload
        write_file_atomic(&dest, data, is_credential_path(path))?;
        outcome.applied += 1;
    }
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Source resolution
// ---------------------------------------------------------------------------

/// `[sync].source` from `~/.wizard/config.toml`, read directly (sync never
/// runs the full config load, which could trigger onboarding side effects).
fn configured_source() -> Option<String> {
    let path = Config::path().ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let config: Config = toml::from_str(&raw).ok()?;
    config
        .sync
        .source
        .filter(|source| !source.trim().is_empty())
}

/// Fetch the bundle bytes: http(s) URLs download, everything else is a local
/// path (`~` expands).
async fn fetch_source(source: &str) -> Result<Vec<u8>> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let client = reqwest::Client::builder()
            .user_agent(format!("wizard/{}", crate::update::current_version()))
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .context("building HTTP client")?;
        let bytes = client
            .get(source)
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .with_context(|| format!("downloading {source}"))?
            .bytes()
            .await
            .with_context(|| format!("reading {source}"))?;
        Ok(bytes.to_vec())
    } else {
        let path = PathBuf::from(shellexpand::tilde(source).into_owned());
        std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// CLI handlers
// ---------------------------------------------------------------------------

/// The `wizard sync` command handler. Returns the process exit code.
pub async fn run(cmd: SyncCmd) -> Result<i32> {
    match cmd {
        SyncCmd::Pack {
            out,
            include_credentials,
        } => pack_cli(out, include_credentials),
        SyncCmd::Pull { source, dry_run } => pull_cli(source, dry_run).await,
        SyncCmd::Key => key_cli(),
    }
}

fn pack_cli(out: Option<PathBuf>, include_credentials: bool) -> Result<i32> {
    let paths = SyncPaths::resolve()?;
    let out = out.unwrap_or_else(|| {
        PathBuf::from(format!(
            "wizard-sync-{}.tar.gz",
            chrono::Local::now().format("%Y%m%d")
        ))
    });
    let outcome = pack(&paths, &out, include_credentials)?;
    println!(
        "packed {} file(s) ({} payload) into {} ({})",
        outcome.file_count,
        format_size(outcome.payload_bytes),
        out.display(),
        format_size(outcome.bundle_bytes),
    );
    println!("signing key: {}", outcome.fingerprint);
    if outcome.includes_credentials {
        println!();
        println!("WARNING: this bundle contains API keys (credentials.toml / xai_oauth.json).");
        println!("         Transfer it privately and delete it after pulling; the file was");
        println!("         written with 0600 permissions.");
    } else {
        println!("credentials: not included (pass --include-credentials to add them)");
    }
    Ok(0)
}

async fn pull_cli(source: Option<String>, dry_run: bool) -> Result<i32> {
    let Some(source) = source.or_else(configured_source) else {
        bail!(
            "no bundle source: pass one (`wizard sync pull <path-or-url>`) or set\n  \
             [sync]\n  source = \"<path-or-url>\"\nin {}",
            Config::path()?.display()
        );
    };
    let bytes = fetch_source(&source).await?;
    let wizard_dir = Config::wizard_dir()?;
    pull_bundle(&wizard_dir, &bytes, &source, dry_run)
}

/// Verify `bytes` as a bundle and apply it into `wizard_dir` (or just report,
/// on `dry_run`). Split from [`pull_cli`] so tests can drive it against a
/// temp dir.
fn pull_bundle(wizard_dir: &Path, bytes: &[u8], source: &str, dry_run: bool) -> Result<i32> {
    let raw = parse_bundle(bytes)?;
    let manifest = verify_bundle(&raw)?;
    let key = decode_public_key(&manifest.public_key)?;
    let fingerprint = fingerprint(&key);

    println!(
        "bundle: {} file(s), created {} on {} (wizard v{})",
        manifest.files.len(),
        manifest.created_at,
        manifest.host,
        manifest.wizard_version
    );
    println!("signature: OK");
    if manifest.includes_credentials {
        println!("note: this bundle includes credentials (API keys).");
    }

    match check_trust(
        wizard_dir,
        &manifest.public_key,
        &fingerprint,
        source,
        dry_run,
    )? {
        Trust::AlreadyTrusted => println!("signing key: trusted ({fingerprint})"),
        Trust::Pinned => {
            println!("signing key: NOT previously seen — pinned it (trust on first use).");
            println!();
            println!("  fingerprint: {fingerprint}");
            println!();
            println!("  Verify this matches `wizard sync key` on the source machine.");
        }
        Trust::WouldPin => {
            println!(
                "signing key: not yet trusted — a real pull would pin it (trust on first use)."
            );
            println!("  fingerprint: {fingerprint}");
        }
    }

    let diff = diff_against(wizard_dir, &raw.payload);
    println!();
    for (path, state) in &diff {
        println!("  {:<10} {path}", state.label());
    }
    println!();

    if dry_run {
        println!("dry run — nothing applied.");
        return Ok(0);
    }

    let outcome = apply(wizard_dir, &raw.payload, &diff)?;
    println!(
        "applied {} file(s), {} unchanged.",
        outcome.applied, outcome.unchanged
    );
    if let Some(dir) = outcome.backup_dir {
        println!("replaced files backed up under {}", dir.display());
    }
    Ok(0)
}

fn key_cli() -> Result<i32> {
    let wizard_dir = Config::wizard_dir()?;
    let key = load_or_generate_key(&wizard_dir)?;
    let public = key.verifying_key();
    println!("public key:  {}", BASE64.encode(public.to_bytes()));
    println!("fingerprint: {}", fingerprint(&public));
    Ok(0)
}

/// Human byte size for the pack summary (`532 B`, `12.3 KiB`, `1.2 MiB`).
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `SyncPaths` over a temp `wizard_dir`, with the same relative layout
    /// [`SyncPaths::resolve`] derives from the `Config` helpers.
    fn test_paths(wizard_dir: &Path) -> SyncPaths {
        SyncPaths {
            wizard_dir: wizard_dir.to_path_buf(),
            files: ["config.toml", "mcp.toml", "system_prompt.md"]
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            dirs: ["skills", "commands", "subagents", "tools"]
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            credential_files: ["credentials.toml", "xai_oauth.json"]
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        }
    }

    /// Populate a fake `~/.wizard` with portable files, secrets, and some
    /// state that must never be packed.
    fn seed_wizard_dir(wizard_dir: &Path) {
        let write = |rel: &str, contents: &str| {
            let path = wizard_dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        write("config.toml", "model = \"qwen3.6:27b\"\n");
        write("mcp.toml", "[servers]\n");
        write("system_prompt.md", "You are Wizard.\n");
        write("skills/demo/SKILL.md", "# demo skill\n");
        write("skills/deep/nested/notes.md", "nested\n");
        write("commands/deploy.md", "deploy things\n");
        write("subagents/reviewer.toml", "name = \"reviewer\"\n");
        write("tools/hello.sh", "#!/bin/sh\necho hi\n");
        // Secrets: packed only with --include-credentials.
        write("credentials.toml", "[keys]\nopenai = \"sk-secret\"\n");
        write("xai_oauth.json", "{\"access_token\":\"tok\"}\n");
        // Never packed.
        write("sessions/2026.jsonl", "{}\n");
        write("logs/trace.log", "log\n");
        write("evolution.jsonl", "{}\n");
    }

    fn pack_to_bytes(wizard_dir: &Path, include_credentials: bool) -> Vec<u8> {
        let out = wizard_dir.join("out-bundle.tar.gz");
        pack(&test_paths(wizard_dir), &out, include_credentials).expect("pack");
        std::fs::read(&out).expect("read bundle")
    }

    #[test]
    fn pack_pull_round_trip_lands_all_portable_files() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src.path());

        let bundle = pack_to_bytes(src.path(), false);
        let code = pull_bundle(dst.path(), &bundle, "test", false).expect("pull");
        assert_eq!(code, 0);

        for rel in [
            "config.toml",
            "mcp.toml",
            "system_prompt.md",
            "skills/demo/SKILL.md",
            "skills/deep/nested/notes.md",
            "commands/deploy.md",
            "subagents/reviewer.toml",
            "tools/hello.sh",
        ] {
            let original = std::fs::read(src.path().join(rel)).expect("source file");
            let pulled =
                std::fs::read(dst.path().join(rel)).unwrap_or_else(|_| panic!("{rel} must arrive"));
            assert_eq!(original, pulled, "{rel} round-trips byte-identical");
        }
        // Non-portable state never travels.
        for rel in ["sessions/2026.jsonl", "logs/trace.log", "evolution.jsonl"] {
            assert!(!dst.path().join(rel).exists(), "{rel} must not travel");
        }
        // TOFU pinned the source key.
        let pinned = std::fs::read_to_string(trusted_keys_path(dst.path())).expect("trusted_keys");
        let source_key = load_or_generate_key(src.path()).expect("key");
        let source_b64 = BASE64.encode(source_key.verifying_key().to_bytes());
        assert!(
            parse_trusted_keys(&pinned).contains(&source_b64),
            "the source public key is pinned: {pinned}"
        );
        assert!(
            pinned.contains("# pinned"),
            "pin carries a comment: {pinned}"
        );
    }

    #[test]
    fn second_pull_is_idempotent_and_backs_up_changes() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src.path());

        let bundle = pack_to_bytes(src.path(), false);
        pull_bundle(dst.path(), &bundle, "test", false).expect("first pull");

        // Identical second pull: nothing applied, no backups.
        let raw = parse_bundle(&bundle).expect("parse");
        let diff = diff_against(dst.path(), &raw.payload);
        assert!(diff.iter().all(|(_, state)| *state == FileState::Identical));
        let outcome = apply(dst.path(), &raw.payload, &diff).expect("apply");
        assert_eq!(outcome.applied, 0);
        assert_eq!(outcome.unchanged, diff.len());
        assert!(outcome.backup_dir.is_none());

        // Local edit, then pull again: overwritten, and the edit is backed up.
        std::fs::write(dst.path().join("config.toml"), "model = \"edited\"\n").unwrap();
        pull_bundle(dst.path(), &bundle, "test", false).expect("second pull");
        let restored = std::fs::read_to_string(dst.path().join("config.toml")).unwrap();
        assert_eq!(restored, "model = \"qwen3.6:27b\"\n");
        let backups_root = sync_dir(dst.path()).join("backups");
        let stamp_dirs: Vec<_> = std::fs::read_dir(&backups_root)
            .expect("backups dir")
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(stamp_dirs.len(), 1, "one backup batch");
        let backed_up = std::fs::read_to_string(stamp_dirs[0].path().join("config.toml")).unwrap();
        assert_eq!(
            backed_up, "model = \"edited\"\n",
            "the local edit is preserved"
        );
    }

    #[test]
    fn dry_run_changes_nothing_and_pins_nothing() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src.path());

        let bundle = pack_to_bytes(src.path(), false);
        let code = pull_bundle(dst.path(), &bundle, "test", true).expect("dry run");
        assert_eq!(code, 0);
        assert!(
            !dst.path().join("config.toml").exists(),
            "dry run writes no payload files"
        );
        assert!(
            !trusted_keys_path(dst.path()).exists(),
            "dry run pins no key"
        );
    }

    #[test]
    fn tampered_manifest_fails_signature_verification() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src.path());

        let bundle = pack_to_bytes(src.path(), false);
        let raw = parse_bundle(&bundle).expect("parse");

        // Flip a value inside the signed manifest bytes, keeping the JSON
        // schema intact so the failure is the signature, not the parse.
        let original = String::from_utf8(raw.manifest_bytes.clone()).unwrap();
        let tampered_manifest = original.replace(
            "\"includes_credentials\": false",
            "\"includes_credentials\": true",
        );
        assert_ne!(tampered_manifest, original, "the tamper must change bytes");
        let tampered = assemble_bundle(tampered_manifest.as_bytes(), &raw.signature, &raw.payload)
            .expect("assemble");

        let err = pull_bundle(dst.path(), &tampered, "test", false)
            .expect_err("tampered manifest must fail");
        assert!(
            format!("{err:#}").contains("signature"),
            "error names the signature: {err:#}"
        );
        assert!(!dst.path().join("config.toml").exists(), "nothing written");
    }

    #[test]
    fn tampered_payload_fails_hash_verification() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src.path());

        let bundle = pack_to_bytes(src.path(), false);
        let raw = parse_bundle(&bundle).expect("parse");

        let mut payload = raw.payload.clone();
        // Same length so only the hash (not the size check) can catch it.
        payload.insert(
            "config.toml".to_string(),
            b"model = \"qwen0.0:00b\"\n".to_vec(),
        );
        assert_eq!(
            payload["config.toml"].len(),
            raw.payload["config.toml"].len(),
            "tamper keeps the size"
        );
        let tampered =
            assemble_bundle(&raw.manifest_bytes, &raw.signature, &payload).expect("assemble");

        let err = pull_bundle(dst.path(), &tampered, "test", false)
            .expect_err("tampered payload must fail");
        assert!(
            format!("{err:#}").contains("sha256 mismatch"),
            "error names the hash: {err:#}"
        );
        assert!(!dst.path().join("config.toml").exists(), "nothing written");
    }

    #[test]
    fn payload_and_manifest_must_correspond_exactly() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src.path());

        let bundle = pack_to_bytes(src.path(), false);
        let raw = parse_bundle(&bundle).expect("parse");

        // A payload file the manifest does not list.
        let mut extra = raw.payload.clone();
        extra.insert("smuggled.txt".to_string(), b"boo".to_vec());
        let tampered =
            assemble_bundle(&raw.manifest_bytes, &raw.signature, &extra).expect("assemble");
        let err = pull_bundle(dst.path(), &tampered, "test", false)
            .expect_err("unlisted payload must fail");
        assert!(
            format!("{err:#}").contains("not listed in the manifest"),
            "{err:#}"
        );

        // A manifest entry the payload does not carry.
        let mut missing = raw.payload.clone();
        missing.remove("config.toml");
        let tampered =
            assemble_bundle(&raw.manifest_bytes, &raw.signature, &missing).expect("assemble");
        let err = pull_bundle(dst.path(), &tampered, "test", false)
            .expect_err("missing payload must fail");
        assert!(format!("{err:#}").contains("does not contain"), "{err:#}");
    }

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        // The pure validator.
        assert!(validate_rel_path("skills/demo/SKILL.md").is_ok());
        assert!(validate_rel_path("config.toml").is_ok());
        for bad in [
            "../evil",
            "skills/../../evil",
            "/etc/passwd",
            "a\\b",
            "..",
            "./config.toml",
            "",
        ] {
            assert!(validate_rel_path(bad).is_err(), "{bad:?} must be rejected");
        }

        // A correctly signed bundle whose manifest lists a traversal path (and
        // an absolute one) still fails before anything is written.
        let dst = tempfile::tempdir().expect("tempdir");
        let keydir = tempfile::tempdir().expect("tempdir");
        let key = load_or_generate_key(keydir.path()).expect("key");
        for evil in ["../evil", "/etc/evil"] {
            let manifest = Manifest {
                version: BUNDLE_VERSION,
                created_at: Utc::now().to_rfc3339(),
                wizard_version: "0.0.0".to_string(),
                host: "test".to_string(),
                includes_credentials: false,
                public_key: BASE64.encode(key.verifying_key().to_bytes()),
                files: vec![ManifestEntry {
                    path: evil.to_string(),
                    sha256: sha256_hex(b"pwned"),
                    size: 5,
                }],
            };
            let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
            let signature = BASE64.encode(key.sign(&manifest_bytes).to_bytes());
            let bundle =
                assemble_bundle(&manifest_bytes, &signature, &BTreeMap::new()).expect("assemble");

            let err = pull_bundle(dst.path(), &bundle, "test", false)
                .expect_err("unsafe manifest path must fail");
            let message = format!("{err:#}");
            assert!(
                message.contains("..") || message.contains("absolute"),
                "{message}"
            );
        }
        assert!(
            !dst.path().parent().unwrap().join("evil").exists(),
            "nothing escapes the wizard dir"
        );

        // A tar entry smuggling `payload/../evil` (crafted by writing the raw
        // GNU header name, since tar::Builder itself refuses such paths) is
        // rejected at parse time.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let data = b"pwned";
        let mut header = tar::Header::new_gnu();
        let name = b"payload/../evil";
        header.as_gnu_mut().expect("gnu header").name[..name.len()].copy_from_slice(name);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &data[..]).expect("append raw entry");
        let bytes = tar.into_inner().unwrap().finish().unwrap();

        let Err(err) = parse_bundle(&bytes) else {
            panic!("traversal payload path must fail");
        };
        assert!(format!("{err:#}").contains(".."), "{err:#}");
    }

    #[test]
    fn unsupported_bundle_version_is_rejected_even_when_signed() {
        let dst = tempfile::tempdir().expect("tempdir");
        let keydir = tempfile::tempdir().expect("tempdir");
        let key = load_or_generate_key(keydir.path()).expect("key");
        let manifest = Manifest {
            version: BUNDLE_VERSION + 1,
            created_at: Utc::now().to_rfc3339(),
            wizard_version: "99.0.0".to_string(),
            host: "future".to_string(),
            includes_credentials: false,
            public_key: BASE64.encode(key.verifying_key().to_bytes()),
            files: Vec::new(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        let signature = BASE64.encode(key.sign(&manifest_bytes).to_bytes());
        let bundle =
            assemble_bundle(&manifest_bytes, &signature, &BTreeMap::new()).expect("assemble");

        let err = pull_bundle(dst.path(), &bundle, "test", false)
            .expect_err("a future version must fail");
        let message = format!("{err:#}");
        assert!(message.contains("unsupported bundle version"), "{message}");
        assert!(
            message.contains("update wizard"),
            "tells the user the way out: {message}"
        );
    }

    #[test]
    fn corrupt_signature_encoding_is_rejected() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src.path());
        let bundle = pack_to_bytes(src.path(), false);
        let raw = parse_bundle(&bundle).expect("parse");

        let not_base64 = assemble_bundle(&raw.manifest_bytes, "!!!not base64!!!", &raw.payload)
            .expect("assemble");
        let err = pull_bundle(dst.path(), &not_base64, "test", false).expect_err("bad encoding");
        assert!(format!("{err:#}").contains("manifest.sig"), "{err:#}");

        let wrong_length =
            assemble_bundle(&raw.manifest_bytes, &BASE64.encode([0u8; 10]), &raw.payload)
                .expect("assemble");
        let err = pull_bundle(dst.path(), &wrong_length, "test", false).expect_err("short sig");
        assert!(format!("{err:#}").contains("64-byte"), "{err:#}");
        assert!(!dst.path().join("config.toml").exists(), "nothing written");
    }

    #[test]
    fn payload_size_mismatch_is_detected() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src.path());
        let bundle = pack_to_bytes(src.path(), false);
        let raw = parse_bundle(&bundle).expect("parse");

        let mut payload = raw.payload.clone();
        payload.insert(
            "config.toml".to_string(),
            b"longer than the manifest says".to_vec(),
        );
        let tampered =
            assemble_bundle(&raw.manifest_bytes, &raw.signature, &payload).expect("assemble");
        let err = pull_bundle(dst.path(), &tampered, "test", false).expect_err("size lies");
        assert!(format!("{err:#}").contains("size mismatch"), "{err:#}");
        assert!(!dst.path().join("config.toml").exists(), "nothing written");
    }

    #[test]
    fn duplicate_payload_entries_are_rejected_at_parse_time() {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        append_entry(&mut tar, "payload/config.toml", b"first").expect("append");
        append_entry(&mut tar, "payload/config.toml", b"second").expect("append");
        let bytes = tar.into_inner().unwrap().finish().unwrap();

        let Err(err) = parse_bundle(&bytes) else {
            panic!("duplicates must fail");
        };
        assert!(
            format!("{err:#}").contains("duplicate payload entry"),
            "{err:#}"
        );
    }

    #[test]
    fn manifest_listing_a_path_twice_is_rejected() {
        let dst = tempfile::tempdir().expect("tempdir");
        let keydir = tempfile::tempdir().expect("tempdir");
        let key = load_or_generate_key(keydir.path()).expect("key");
        let data = b"model = \"x\"\n";
        let entry = || ManifestEntry {
            path: "config.toml".to_string(),
            sha256: sha256_hex(data),
            size: data.len() as u64,
        };
        let manifest = Manifest {
            version: BUNDLE_VERSION,
            created_at: Utc::now().to_rfc3339(),
            wizard_version: "0.0.0".to_string(),
            host: "test".to_string(),
            includes_credentials: false,
            public_key: BASE64.encode(key.verifying_key().to_bytes()),
            files: vec![entry(), entry()],
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        let signature = BASE64.encode(key.sign(&manifest_bytes).to_bytes());
        let payload = BTreeMap::from([("config.toml".to_string(), data.to_vec())]);
        let bundle = assemble_bundle(&manifest_bytes, &signature, &payload).expect("assemble");

        let err = pull_bundle(dst.path(), &bundle, "test", false).expect_err("double listing");
        assert!(format!("{err:#}").contains("twice"), "{err:#}");
        assert!(!dst.path().join("config.toml").exists(), "nothing written");
    }

    #[test]
    fn tofu_pins_first_key_and_rejects_a_different_one() {
        let src_a = tempfile::tempdir().expect("tempdir");
        let src_c = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src_a.path());
        seed_wizard_dir(src_c.path());

        // First pull pins machine A's key.
        let bundle_a = pack_to_bytes(src_a.path(), false);
        pull_bundle(dst.path(), &bundle_a, "test", false).expect("first pull");

        // Machine C generates its own key; its bundle must be rejected.
        let bundle_c = pack_to_bytes(src_c.path(), false);
        let err = pull_bundle(dst.path(), &bundle_c, "test", false)
            .expect_err("a different key must be rejected");
        let message = format!("{err:#}");
        assert!(message.contains("UNTRUSTED"), "{message}");
        assert!(
            message.contains("wizard sync key"),
            "error explains how to trust: {message}"
        );
        assert!(
            message.contains("trusted_keys"),
            "error names the trust file: {message}"
        );

        // Pulls from A keep working.
        pull_bundle(dst.path(), &bundle_a, "test", false).expect("trusted pull still works");

        // Manually trusting C's key (with a comment) unblocks it.
        let key_c = load_or_generate_key(src_c.path()).expect("key");
        let line = format!(
            "{} # machine C\n",
            BASE64.encode(key_c.verifying_key().to_bytes())
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(trusted_keys_path(dst.path()))
            .unwrap();
        file.write_all(line.as_bytes()).unwrap();
        pull_bundle(dst.path(), &bundle_c, "test", false).expect("manually trusted pull");
    }

    #[test]
    fn credentials_stay_out_unless_asked_for() {
        let src = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src.path());

        // Default: no secrets in the payload, manifest says so.
        let bundle = pack_to_bytes(src.path(), false);
        let raw = parse_bundle(&bundle).expect("parse");
        let manifest = verify_bundle(&raw).expect("verify");
        assert!(!manifest.includes_credentials);
        assert!(!raw.payload.contains_key("credentials.toml"));
        assert!(!raw.payload.contains_key("xai_oauth.json"));

        // Opt-in: both secrets travel and the manifest flags it.
        let bundle = pack_to_bytes(src.path(), true);
        let raw = parse_bundle(&bundle).expect("parse");
        let manifest = verify_bundle(&raw).expect("verify");
        assert!(manifest.includes_credentials);
        assert!(raw.payload.contains_key("credentials.toml"));
        assert!(raw.payload.contains_key("xai_oauth.json"));

        // Pulled secrets land 0600.
        let dst = tempfile::tempdir().expect("tempdir");
        pull_bundle(dst.path(), &bundle, "test", false).expect("pull");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for rel in ["credentials.toml", "xai_oauth.json"] {
                let mode = std::fs::metadata(dst.path().join(rel))
                    .expect("stat")
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{rel} must be 0600");
            }
            let mode = std::fs::metadata(dst.path().join("config.toml"))
                .expect("stat")
                .permissions()
                .mode();
            assert_ne!(mode & 0o777, 0o600, "non-secrets keep normal perms");
        }
    }

    #[cfg(unix)]
    #[test]
    fn credential_bundles_are_written_0600() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().expect("tempdir");
        seed_wizard_dir(src.path());

        let out = src.path().join("secret-bundle.tar.gz");
        pack(&test_paths(src.path()), &out, true).expect("pack");
        let mode = std::fs::metadata(&out).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "a bundle holding keys must be 0600");
    }

    #[test]
    fn key_file_is_0600_and_stable_across_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = load_or_generate_key(dir.path()).expect("generate");
        let second = load_or_generate_key(dir.path()).expect("reload");
        assert_eq!(
            first.verifying_key().to_bytes(),
            second.verifying_key().to_bytes(),
            "the key survives reloads"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key_mode = std::fs::metadata(key_path(dir.path()))
                .expect("stat key")
                .permissions()
                .mode();
            assert_eq!(key_mode & 0o777, 0o600, "the seed file must be 0600");
            let dir_mode = std::fs::metadata(sync_dir(dir.path()))
                .expect("stat sync dir")
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700, "the sync dir must be 0700");
        }
    }

    #[test]
    fn fingerprint_is_openssh_shaped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = load_or_generate_key(dir.path()).expect("generate");
        let fp = fingerprint(&key.verifying_key());
        assert!(fp.starts_with("SHA256:"), "{fp}");
        let b64 = fp.strip_prefix("SHA256:").unwrap();
        assert!(!b64.ends_with('='), "no padding: {fp}");
        // sha256 → 32 bytes → 43 unpadded base64 chars.
        assert_eq!(b64.len(), 43, "{fp}");
    }

    #[test]
    fn trusted_keys_parsing_handles_comments_and_blanks() {
        let raw = "\
# a full-line comment
KEYAAA # pinned 2026-07-09 from test

KEYBBB
   KEYCCC   #trailing
";
        assert_eq!(parse_trusted_keys(raw), vec!["KEYAAA", "KEYBBB", "KEYCCC"]);
        assert!(parse_trusted_keys("").is_empty());
        assert!(parse_trusted_keys("# only comments\n").is_empty());
    }

    #[test]
    fn garbage_and_truncated_bundles_fail_cleanly() {
        let dst = tempfile::tempdir().expect("tempdir");
        assert!(pull_bundle(dst.path(), b"not a bundle", "test", false).is_err());

        // A valid gzip+tar with no manifest.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        append_entry(&mut tar, "payload/x", b"y").expect("append");
        let bytes = tar.into_inner().unwrap().finish().unwrap();
        let err = pull_bundle(dst.path(), &bytes, "test", false).expect_err("no manifest");
        assert!(format!("{err:#}").contains("manifest"), "{err:#}");
    }

    #[test]
    fn format_size_is_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(532), "532 B");
        assert_eq!(format_size(12 * 1024 + 307), "12.3 KiB");
        assert_eq!(format_size(6 * 1024 * 1024 / 5), "1.2 MiB");
    }
}
