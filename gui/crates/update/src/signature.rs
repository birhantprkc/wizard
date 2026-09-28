//! minisign verification of a release's `gui-checksums.txt`.
//!
//! The same format and the same rules as `wizard update` (the root crate's
//! `src/update.rs`): minisign is ed25519 over either the file (`Ed`) or its
//! blake2b-512 prehash (`ED`), the global signature covers the trusted
//! comment, and the trusted comment has to name the release being installed.
//! Every failure is a refusal; nothing here has a bypass.
//!
//! The key is compiled in (see `build.rs`): the repository's
//! `wizard-release.pub`, the key `.github/workflows/release.yml` signs
//! `gui-checksums.txt` with.

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, VerifyingKey};

/// The public key file this binary trusts.
const RELEASE_PUBLIC_KEY: &str = include_str!(concat!(env!("OUT_DIR"), "/release.pub"));

/// What `wizard-release.pub` holds before a release keypair exists.
const RELEASE_KEY_PLACEHOLDER: &str = "RELEASE-SIGNING-KEY-NOT-YET-GENERATED";

/// True when this build trusts a throwaway key instead of the release key
/// (`WIZARD_GUI_TEST_RELEASE_KEY` at build time).
pub const fn test_key_build() -> bool {
    cfg!(wizard_test_release_key)
}

/// A parsed minisign public key.
#[derive(Debug, Clone)]
pub struct ReleaseKey {
    id: [u8; 8],
    key: VerifyingKey,
}

/// The key compiled into this binary.
pub fn release_key() -> Result<ReleaseKey> {
    parse_public_key(RELEASE_PUBLIC_KEY)
}

/// A key id as `minisign -V` prints it.
fn key_id_hex(id: &[u8; 8]) -> String {
    format!("{:016X}", u64::from_le_bytes(*id))
}

/// One base64 line of a minisign file: two algorithm bytes, an 8-byte key id,
/// then `body_len` bytes.
fn decode_line(line: &str, body_len: usize, what: &str) -> Result<([u8; 2], [u8; 8], Vec<u8>)> {
    let raw = BASE64
        .decode(line.trim())
        .with_context(|| format!("{what} is not valid base64"))?;
    if raw.len() != 10 + body_len {
        bail!("{what} is {} bytes, expected {}", raw.len(), 10 + body_len);
    }
    let algorithm: [u8; 2] = raw[..2].try_into().expect("2 bytes");
    let id: [u8; 8] = raw[2..10].try_into().expect("8 bytes");
    Ok((algorithm, id, raw[10..].to_vec()))
}

/// Parse a minisign public key file: a comment line, then
/// `base64("Ed" || key id || 32-byte ed25519 key)`.
pub fn parse_public_key(text: &str) -> Result<ReleaseKey> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("untrusted comment:"))
        .context("the release public key file holds no key line")?;
    if line.starts_with(RELEASE_KEY_PLACEHOLDER) {
        bail!("this build embeds no release signing key, so it cannot verify any update");
    }
    let (algorithm, id, body) = decode_line(line, 32, "the release public key")?;
    if &algorithm != b"Ed" {
        bail!(
            "the release public key names algorithm {:?}, not minisign's ed25519",
            String::from_utf8_lossy(&algorithm)
        );
    }
    let raw: [u8; 32] = body.as_slice().try_into().expect("32 bytes");
    let key = VerifyingKey::from_bytes(&raw)
        .map_err(|_| anyhow!("the release public key is not a valid ed25519 key"))?;
    Ok(ReleaseKey { id, key })
}

/// A parsed detached `.minisig`.
struct ReleaseSignature {
    algorithm: [u8; 2],
    key_id: [u8; 8],
    signature: Signature,
    trusted_comment: String,
    global_signature: Signature,
}

fn parse_signature(text: &str) -> Result<ReleaseSignature> {
    let mut lines = text.lines().map(|line| line.trim_end_matches(['\r', '\n']));
    if !lines
        .next()
        .unwrap_or_default()
        .starts_with("untrusted comment:")
    {
        bail!("the signature does not start with an untrusted comment line");
    }
    let (algorithm, key_id, body) = decode_line(
        lines
            .next()
            .context("the signature has no signature line")?,
        64,
        "the signature",
    )?;
    let signature = Signature::from_slice(&body).expect("64 bytes");
    let trusted_comment = lines
        .next()
        .and_then(|line| line.strip_prefix("trusted comment: "))
        .context("the signature has no trusted comment")?
        .to_string();
    let global = BASE64
        .decode(
            lines
                .next()
                .context("the signature has no global signature line")?
                .trim(),
        )
        .context("the global signature is not valid base64")?;
    let global_signature = Signature::from_slice(&global)
        .map_err(|_| anyhow!("the global signature is not a 64-byte ed25519 signature"))?;
    Ok(ReleaseSignature {
        algorithm,
        key_id,
        signature,
        trusted_comment,
        global_signature,
    })
}

fn blake2b512(data: &[u8]) -> [u8; 64] {
    use blake2::{Blake2b512, Digest as _};
    Blake2b512::digest(data).into()
}

/// Verify `data` against the detached signature text under `key`, returning
/// the trusted comment. Callers still have to check what the comment names
/// ([`binds_to_tag`]).
pub fn verify(key: &ReleaseKey, data: &[u8], signature_text: &str) -> Result<String> {
    let signature = parse_signature(signature_text)?;
    if signature.key_id != key.id {
        bail!(
            "the release is signed by key {}, but this build trusts {}",
            key_id_hex(&signature.key_id),
            key_id_hex(&key.id)
        );
    }
    let message = match &signature.algorithm {
        b"Ed" => data.to_vec(),
        b"ED" => blake2b512(data).to_vec(),
        other => bail!(
            "the signature names an unknown minisign algorithm {:?}",
            String::from_utf8_lossy(other)
        ),
    };
    key.key
        .verify_strict(&message, &signature.signature)
        .map_err(|_| anyhow!("gui-checksums.txt does not match its signature"))?;
    let mut global = signature.signature.to_bytes().to_vec();
    global.extend_from_slice(signature.trusted_comment.as_bytes());
    key.key
        .verify_strict(&global, &signature.global_signature)
        .map_err(|_| anyhow!("the signature's trusted comment has been altered"))?;
    Ok(signature.trusted_comment)
}

/// Require the trusted comment to name `tag` as a whole word. Without this a
/// host could answer a request for v2 with v1's genuinely signed files and
/// hold the app on an old release.
pub fn binds_to_tag(trusted_comment: &str, tag: &str) -> Result<()> {
    let wanted = tag.trim();
    if wanted.is_empty() {
        bail!("cannot check a signature against an empty tag");
    }
    let names_tag = trusted_comment
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_')))
        .any(|word| word == wanted);
    if !names_tag {
        bail!("the signature was made for {trusted_comment:?}, not for {wanted}");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing {
    //! A throwaway key and a signer that writes real minisign files, so tests
    //! can mint releases the verifier must accept and ones it must refuse.
    use super::*;
    use ed25519_dalek::Signer as _;

    pub struct TestKey {
        signing: ed25519_dalek::SigningKey,
        id: [u8; 8],
        pub public: ReleaseKey,
    }

    impl TestKey {
        pub fn new(seed: u8) -> Self {
            Self::with_id(seed, [seed, 1, 2, 3, 4, 5, 6, 7])
        }

        pub fn with_id(seed: u8, id: [u8; 8]) -> Self {
            let signing = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
            let mut raw = b"Ed".to_vec();
            raw.extend_from_slice(&id);
            raw.extend_from_slice(&signing.verifying_key().to_bytes());
            let text = format!("untrusted comment: test\n{}\n", BASE64.encode(&raw));
            let public = parse_public_key(&text).unwrap();
            Self {
                signing,
                id,
                public,
            }
        }

        /// A prehashed (`ED`, minisign's default) signature over `data`.
        pub fn sign(&self, data: &[u8], trusted: &str) -> String {
            let signature = self.signing.sign(&blake2b512(data));
            let mut line = b"ED".to_vec();
            line.extend_from_slice(&self.id);
            line.extend_from_slice(&signature.to_bytes());
            let mut global = signature.to_bytes().to_vec();
            global.extend_from_slice(trusted.as_bytes());
            format!(
                "untrusted comment: test\n{}\ntrusted comment: {trusted}\n{}\n",
                BASE64.encode(&line),
                BASE64.encode(self.signing.sign(&global).to_bytes())
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TestKey;
    use super::*;

    const V361_CHECKSUMS: &[u8] = include_bytes!("fixtures/gui-checksums-v3.6.1.txt");
    const V361_SIGNATURE: &str = include_str!("fixtures/gui-checksums-v3.6.1.txt.minisig");

    /// The published v3.6.1 files, signed by release.yml with the real key,
    /// verify under the key this crate embeds.
    #[cfg(not(wizard_test_release_key))]
    #[test]
    fn the_published_release_verifies_under_the_embedded_key() {
        let key = release_key().unwrap();
        let comment = verify(&key, V361_CHECKSUMS, V361_SIGNATURE).unwrap();
        assert_eq!(
            comment,
            "wizard v3.6.1 gui checksums, signed by the wizard release key"
        );
        binds_to_tag(&comment, "v3.6.1").unwrap();
    }

    #[test]
    fn an_injected_key_verifies_its_own_signature_and_nothing_else() {
        let key = TestKey::new(7);
        let data = b"abc  wizard-gui-9.0.0-linux-x86_64.tar.gz\n";
        let signature = key.sign(data, "wizard v9.0.0 gui checksums");
        assert_eq!(
            verify(&key.public, data, &signature).unwrap(),
            "wizard v9.0.0 gui checksums"
        );
        // The real release's signature is not the test key's.
        let err = verify(&key.public, V361_CHECKSUMS, V361_SIGNATURE).unwrap_err();
        assert!(err.to_string().contains("signed by key"), "{err}");
        // A different key claiming the same id still fails the ed25519 check.
        let impostor = TestKey::with_id(9, [7, 1, 2, 3, 4, 5, 6, 7]);
        let forged = impostor.sign(data, "wizard v9.0.0 gui checksums");
        let err = verify(&key.public, data, &forged).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn a_changed_byte_or_comment_is_refused() {
        let key = TestKey::new(3);
        let data = b"abc  wizard-gui-9.0.0-linux-x86_64.tar.gz\n";
        let signature = key.sign(data, "wizard v9.0.0 gui checksums");
        let err = verify(
            &key.public,
            b"abd  wizard-gui-9.0.0-linux-x86_64.tar.gz\n",
            &signature,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        let rewritten = signature.replace("v9.0.0", "v9.9.9");
        let err = verify(&key.public, data, &rewritten).unwrap_err();
        assert!(err.to_string().contains("trusted comment"), "{err}");
        assert!(verify(&key.public, data, "not a signature").is_err());
    }

    #[test]
    fn the_comment_must_name_the_release() {
        binds_to_tag("wizard v3.6.1 gui checksums", "v3.6.1").unwrap();
        assert!(binds_to_tag("wizard v3.6.1 gui checksums", "v3.6.2").is_err());
        assert!(binds_to_tag("wizard v3.6.10 gui checksums", "v3.6.1").is_err());
        assert!(binds_to_tag("wizard gui checksums", "v3.6.1").is_err());
        assert!(binds_to_tag("wizard v3.6.1 gui checksums", "").is_err());
    }

    #[test]
    fn the_placeholder_key_refuses_by_name() {
        let err = parse_public_key("untrusted comment: x\nRELEASE-SIGNING-KEY-NOT-YET-GENERATED\n")
            .unwrap_err();
        assert!(err.to_string().contains("no release signing key"), "{err}");
    }
}
