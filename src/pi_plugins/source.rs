//! Where a Pi package comes from, and getting its files onto disk.
//!
//! The spec grammar is Pi's own (`pi install` takes the same strings): `npm:`
//! names with an optional version, `git:` and protocol URLs with an optional
//! `@ref`, and local paths. A bare npm name gains `npm:` and a
//! `host.tld/owner/repo` shorthand gains `git:`, the way the desktop app's
//! installer normalizes what people type.
//!
//! Fetching mirrors what Pi does before it runs `npm install`: the npm
//! tarball from the registry (checked against the published integrity hash),
//! a shallow `git clone`, or the local directory read in place. Nothing here
//! runs a package's install scripts or its dependencies' — Wizard only reads
//! the files it maps.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use sha2::Digest as _;

const MAX_SOURCE_LEN: usize = 512;
/// A Pi package is markdown and a little TypeScript. Anything bigger than
/// this is not what the installer was built for.
const MAX_TARBALL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
const REGISTRY: &str = "https://registry.npmjs.org";
const GIT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// A parsed package spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `npm:<name>[@<version|range|tag>]`.
    Npm {
        name: String,
        version: Option<String>,
    },
    /// A git repository and an optional ref.
    Git {
        /// What `git clone` takes.
        url: String,
        /// `host/owner/repo`, the identity Pi records.
        repo: String,
        /// The spec Pi is handed, without the ref.
        spec: String,
        reference: Option<String>,
    },
    /// A directory on this machine.
    Local(PathBuf),
}

impl Source {
    /// The spec as Pi records it, which is also what `pi install` and
    /// `pi remove` are handed.
    pub fn spec(&self) -> String {
        match self {
            Source::Npm { name, version } => match version {
                Some(version) => format!("npm:{name}@{version}"),
                None => format!("npm:{name}"),
            },
            Source::Git {
                spec, reference, ..
            } => match reference {
                Some(reference) => format!("{spec}@{reference}"),
                None => spec.clone(),
            },
            Source::Local(path) => path.display().to_string(),
        }
    }

    /// A name for the package before its manifest has been read.
    pub fn fallback_name(&self) -> String {
        match self {
            Source::Npm { name, .. } => name.clone(),
            Source::Git { repo, .. } => repo.rsplit('/').next().unwrap_or(repo).to_string(),
            Source::Local(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        }
    }
}

/// Parse what a user typed (or a gallery pick) into a [`Source`].
///
/// Refuses anything that could read as a flag or carries whitespace, since
/// the spec is later handed to `pi` and `git` as an argument.
pub fn parse(input: &str) -> Result<Source> {
    const HINT: &str = "use npm:<package>, git:<host>/<owner>/<repo>, a git URL, or a local path";
    let spec = input.trim();
    if spec.is_empty() {
        bail!("no package given: {HINT}");
    }
    if spec.len() > MAX_SOURCE_LEN {
        bail!("that package spec is too long");
    }
    if spec.starts_with('-') {
        bail!("`{spec}` looks like a flag: {HINT}");
    }
    if spec.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("a package spec can't contain spaces");
    }
    if let Some(rest) = spec.strip_prefix("npm:") {
        return npm(rest).ok_or_else(|| anyhow!("`{rest}` isn't a valid npm package name"));
    }
    if let Some(rest) = spec.strip_prefix("github:") {
        return git(&format!("github.com/{rest}")).ok_or_else(|| anyhow!("{HINT}"));
    }
    if let Some(rest) = spec.strip_prefix("git:") {
        return git(rest).ok_or_else(|| anyhow!("{HINT}"));
    }
    if ["https://", "http://", "ssh://", "git://", "git@"]
        .iter()
        .any(|prefix| spec.starts_with(prefix))
    {
        return git(spec).ok_or_else(|| anyhow!("{HINT}"));
    }
    if let Some(path) = spec.strip_prefix("file:") {
        return Ok(Source::Local(expand_home(path)));
    }
    if ["./", "../", "/", "~/"]
        .iter()
        .any(|prefix| spec.starts_with(prefix))
        || spec == "."
        || (cfg!(windows) && spec.get(1..3) == Some(":\\"))
    {
        return Ok(Source::Local(expand_home(spec)));
    }
    if let Some((host, _)) = spec.split_once('/')
        && host.contains('.')
        && !host.starts_with('@')
    {
        return git(spec).ok_or_else(|| anyhow!("{HINT}"));
    }
    npm(spec).ok_or_else(|| anyhow!("{HINT}"))
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

fn npm(spec: &str) -> Option<Source> {
    let name = npm_name(spec);
    let version = &spec[name.len()..];
    let version = match version.strip_prefix('@') {
        Some("") => return None,
        Some(version) => Some(version.to_string()),
        None if version.is_empty() => None,
        None => return None,
    };
    let valid_part = |part: &str| {
        !part.is_empty()
            && part.len() <= 214
            && !part.starts_with(['.', '_'])
            && part.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_' | '~')
            })
    };
    let valid = match name.strip_prefix('@') {
        Some(scoped) => scoped
            .split_once('/')
            .is_some_and(|(scope, pkg)| valid_part(scope) && valid_part(pkg)),
        None => valid_part(name),
    };
    valid.then(|| Source::Npm {
        name: name.to_string(),
        version,
    })
}

/// `@scope/name@1.2.3` → `@scope/name`; `name@^1` → `name`.
pub(crate) fn npm_name(spec: &str) -> &str {
    let from = usize::from(spec.starts_with('@'));
    match spec[from..].find('@') {
        Some(at) => &spec[..from + at],
        None => spec,
    }
}

/// A git source: scp-like `git@host:owner/repo`, a protocol URL, or
/// `host/owner/repo` shorthand, each with an optional `@ref` (or `#ref`).
fn git(spec: &str) -> Option<Source> {
    let (spec, hash_ref) = match spec.split_once('#') {
        Some((spec, reference)) => (spec, Some(reference)),
        None => (spec, None),
    };
    let (scheme, rest) = if let Some(rest) = spec.strip_prefix("git@") {
        ("git@", rest)
    } else if let Some(at) = spec.find("://") {
        (&spec[..at + 3], &spec[at + 3..])
    } else {
        ("", spec)
    };
    let (authority, path) = if scheme == "git@" {
        rest.split_once(':')?
    } else {
        rest.split_once('/')?
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split(':').next().unwrap_or(host);
    let (path, at_ref) = match path.split_once('@') {
        Some((path, reference)) => (path, Some(reference)),
        None => (path, None),
    };
    let path = path.trim_matches('/').trim_end_matches(".git");
    if host.is_empty()
        || path.split('/').filter(|s| !s.is_empty()).count() < 2
        || path.split('/').any(|s| s == "..")
    {
        return None;
    }
    let reference = at_ref
        .or(hash_ref)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    if reference.as_deref().is_some_and(|r| r.starts_with('-')) {
        return None;
    }
    let (url, spec) = match scheme {
        "" => (
            format!("https://{host}/{path}"),
            format!("git:{host}/{path}"),
        ),
        "git@" => {
            let url = format!("git@{authority}:{path}");
            (url.clone(), format!("git:{url}"))
        }
        scheme => {
            let url = format!("{scheme}{authority}/{path}");
            (url.clone(), url)
        }
    };
    Some(Source::Git {
        url,
        repo: format!("{host}/{path}"),
        spec,
        reference,
    })
}

/// A package's files on disk. Staged copies are deleted on drop; a local
/// source is read where it is.
pub struct Fetched {
    pub root: PathBuf,
    staging: Option<PathBuf>,
}

impl Drop for Fetched {
    fn drop(&mut self) {
        if let Some(dir) = &self.staging {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Get `source`'s files onto disk under `scratch`.
pub async fn fetch(source: &Source, scratch: &Path) -> Result<Fetched> {
    match source {
        Source::Local(path) => {
            let root = std::fs::canonicalize(path)
                .with_context(|| format!("{} doesn't exist", path.display()))?;
            if !root.is_dir() {
                bail!(
                    "{} is a file; Wizard installs package directories",
                    root.display()
                );
            }
            Ok(Fetched {
                root,
                staging: None,
            })
        }
        Source::Npm { name, version } => {
            let staging = new_staging(scratch)?;
            let fetched = Fetched {
                root: staging.join("package"),
                staging: Some(staging),
            };
            let tarball = npm_tarball(name, version.as_deref()).await?;
            unpack_npm(&tarball.bytes, &fetched.root)?;
            Ok(fetched)
        }
        Source::Git { url, reference, .. } => {
            let staging = new_staging(scratch)?;
            let fetched = Fetched {
                root: staging.join("repo"),
                staging: Some(staging),
            };
            clone(url, reference.as_deref(), &fetched.root).await?;
            Ok(fetched)
        }
    }
}

fn new_staging(scratch: &Path) -> Result<PathBuf> {
    let dir = scratch.join(uuid::Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn registry() -> String {
    std::env::var("npm_config_registry")
        .or_else(|_| std::env::var("NPM_CONFIG_REGISTRY"))
        .ok()
        .map(|url| url.trim_end_matches('/').to_string())
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
        .unwrap_or_else(|| REGISTRY.to_string())
}

pub(crate) fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("wizard/{}", crate::update::current_version()))
        .timeout(Duration::from_secs(60))
        .build()
        .context("building the HTTP client")
}

struct Tarball {
    bytes: Vec<u8>,
}

/// Resolve `name@version` against the registry and download its tarball,
/// verified against `dist.integrity` (sha512) or `dist.shasum` (sha1-less
/// registries publish at least one of the two; a tarball with neither is
/// refused).
async fn npm_tarball(name: &str, version: Option<&str>) -> Result<Tarball> {
    let client = http()?;
    let base = registry();
    let encoded = name.replacen('/', "%2f", 1);
    let packument: serde_json::Value = client
        .get(format!("{base}/{encoded}"))
        .header("accept", "application/vnd.npm.install-v1+json")
        .send()
        .await
        .with_context(|| format!("couldn't reach the npm registry for {name}"))?
        .error_for_status()
        .map_err(|err| match err.status() {
            Some(reqwest::StatusCode::NOT_FOUND) => anyhow!("{name} isn't on npm"),
            _ => anyhow!("npm registry: {err}"),
        })?
        .json()
        .await
        .context("reading the npm registry's answer")?;
    let chosen = pick_version(&packument, version)
        .ok_or_else(|| anyhow!("no published version of {name} matches {version:?}"))?;
    let dist = &packument["versions"][&chosen]["dist"];
    let url = dist["tarball"]
        .as_str()
        .ok_or_else(|| anyhow!("{name}@{chosen} has no tarball"))?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {name}@{chosen}"))?
        .error_for_status()?;
    if response
        .content_length()
        .is_some_and(|len| len > MAX_TARBALL_BYTES)
    {
        bail!("{name}@{chosen} is larger than Wizard's 64 MB limit for a Pi package");
    }
    let bytes = response.bytes().await?.to_vec();
    if bytes.len() as u64 > MAX_TARBALL_BYTES {
        bail!("{name}@{chosen} is larger than Wizard's 64 MB limit for a Pi package");
    }
    verify_integrity(&bytes, dist["integrity"].as_str())
        .with_context(|| format!("{name}@{chosen}"))?;
    Ok(Tarball { bytes })
}

/// The version `wanted` names in `packument`: an exact version, a dist-tag,
/// or the highest version a semver range admits. `None` wanted is `latest`.
pub(crate) fn pick_version(packument: &serde_json::Value, wanted: Option<&str>) -> Option<String> {
    let versions = packument.get("versions")?.as_object()?;
    let wanted = wanted.unwrap_or("latest");
    if versions.contains_key(wanted) {
        return Some(wanted.to_string());
    }
    if let Some(tagged) = packument
        .get("dist-tags")
        .and_then(|tags| tags.get(wanted))
        .and_then(|v| v.as_str())
    {
        return versions.contains_key(tagged).then(|| tagged.to_string());
    }
    let range = semver::VersionReq::parse(&wanted.replace(".x", ".*")).ok()?;
    versions
        .keys()
        .filter_map(|v| semver::Version::parse(v).ok())
        .filter(|v| range.matches(v))
        .max()
        .map(|v| v.to_string())
}

/// Check a tarball against npm's SRI string (`sha512-<base64>`).
pub(crate) fn verify_integrity(bytes: &[u8], integrity: Option<&str>) -> Result<()> {
    let Some(integrity) = integrity else {
        bail!("the registry published no integrity hash, so the download can't be checked");
    };
    for entry in integrity.split_whitespace() {
        if let Some(expected) = entry.strip_prefix("sha512-") {
            let actual =
                base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(bytes));
            if actual == expected {
                return Ok(());
            }
            bail!("the download doesn't match its published sha512");
        }
    }
    bail!("the registry's integrity hash isn't sha512 ({integrity})")
}

/// Unpack an npm tarball into `dest`, dropping the leading `package/`
/// directory npm puts every file under. Only regular files and directories
/// are written: links, devices and anything that would land outside `dest`
/// are skipped.
pub(crate) fn unpack_npm(bytes: &[u8], dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut total: u64 = 0;
    for entry in archive.entries().context("reading the package tarball")? {
        let mut entry = entry.context("reading the package tarball")?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        let path = entry.path()?.into_owned();
        let Some(relative) = strip_first_component(&path) else {
            continue;
        };
        let out = dest.join(&relative);
        if kind.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        total += entry.header().size().unwrap_or(0);
        if total > MAX_UNPACKED_BYTES {
            bail!("the package unpacks to more than 256 MB");
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        std::fs::write(&out, data).with_context(|| format!("writing {}", out.display()))?;
    }
    Ok(())
}

/// `package/skills/x/SKILL.md` → `skills/x/SKILL.md`; `None` for the root
/// itself or any path with `..`, a root, or a prefix in it.
fn strip_first_component(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    match components.next()? {
        Component::Normal(_) => {}
        _ => return None,
    }
    let mut out = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

async fn clone(url: &str, reference: Option<&str>, dest: &Path) -> Result<()> {
    let run = |args: Vec<String>, cwd: Option<PathBuf>| async move {
        let mut command = tokio::process::Command::new("git");
        command
            .args(&args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
            .await
            .map_err(|_| anyhow!("git didn't finish within 5 minutes"))?
            .map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => anyhow!("git isn't installed"),
                _ => anyhow!("couldn't run git: {err}"),
            })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr.lines().rev().find(|l| !l.trim().is_empty());
        Err(anyhow!(
            "git {} failed: {}",
            args.first().map(String::as_str).unwrap_or(""),
            last.unwrap_or("no output").trim()
        ))
    };
    let dest_arg = dest.display().to_string();
    let mut shallow = vec!["clone".to_string(), "--depth".into(), "1".into()];
    if let Some(reference) = reference {
        shallow.push("--branch".into());
        shallow.push(reference.to_string());
    }
    shallow.extend(["--".to_string(), url.to_string(), dest_arg.clone()]);
    match run(shallow, None).await {
        Ok(()) => Ok(()),
        // A commit sha is not a branch or tag, so `--branch` can't take it:
        // clone the whole history and check it out instead.
        Err(_) if reference.is_some() => {
            let _ = std::fs::remove_dir_all(dest);
            run(
                vec!["clone".into(), "--".into(), url.to_string(), dest_arg],
                None,
            )
            .await?;
            run(
                vec![
                    "checkout".into(),
                    "--detach".into(),
                    reference.unwrap_or_default().to_string(),
                ],
                Some(dest.to_path_buf()),
            )
            .await
        }
        Err(err) => Err(err),
    }
}
