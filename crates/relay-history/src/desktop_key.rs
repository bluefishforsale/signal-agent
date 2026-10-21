//! Recovers Signal Desktop's SQLCipher key. Desktop keeps it in config.json,
//! encrypted with Electron's safeStorage, whose scheme differs per OS; this is
//! the only place that knows those schemes.

use aes::cipher::{BlockModeDecrypt as _, KeyIvInit as _, block_padding::Pkcs7};
use aes_gcm::{Aes256Gcm, KeyInit as _, aead::Aead as _};
use anyhow::{Context, bail};
use base64::Engine as _;

/// Signal Desktop's database key: 64 ASCII hex characters, used as a raw
/// SQLCipher key (`PRAGMA key = "x'<key>'"`).
pub(crate) struct DatabaseKey(String);

impl DatabaseKey {
    // NOTE: the decrypted bytes already are the hex text SQLCipher wants.
    // Hex-encoding them again gives a 128-character key that opens nothing,
    // which is how earlier importers broke; the length check refuses that.
    fn new(plaintext: &[u8]) -> anyhow::Result<Self> {
        if plaintext.len() != 64 || !plaintext.iter().all(u8::is_ascii_hexdigit) {
            bail!(
                "Signal Desktop's database key should be 64 hex characters, got {} bytes",
                plaintext.len()
            );
        }
        Ok(Self(String::from_utf8_lossy(plaintext).into_owned()))
    }
}

impl std::fmt::Debug for DatabaseKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DatabaseKey(..)")
    }
}

/// Decrypts macOS safeStorage's value with the Keychain's "Signal Safe
/// Storage" password.
pub(crate) fn decrypt_macos(
    encrypted: &[u8],
    keychain_password: &[u8],
) -> anyhow::Result<DatabaseKey> {
    decrypt_cbc(encrypted, keychain_password, 1003)
}

/// Decrypts Linux safeStorage's value. "v10" uses Chromium's hardcoded
/// password; "v11" uses the one in the keyring, which `keyring_password` fetches
/// only when needed.
pub(crate) fn decrypt_linux(
    encrypted: &[u8],
    keyring_password: impl FnOnce() -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<DatabaseKey> {
    let password = if encrypted.starts_with(b"v11") {
        keyring_password()?
    } else {
        b"peanuts".to_vec()
    };
    decrypt_cbc(encrypted, &password, 1)
}

/// Decrypts Windows safeStorage's value. Current Electron wraps it in
/// AES-256-GCM under a key kept DPAPI-protected in `Local State`; older builds
/// DPAPI-protected the value itself. `unprotect` is DPAPI's CryptUnprotectData
/// for the current user.
pub(crate) fn decrypt_windows(
    encrypted: &[u8],
    local_state: Option<&str>,
    unprotect: impl Fn(&[u8]) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<DatabaseKey> {
    let wrapped_key = local_state.map(local_state_key).transpose()?.flatten();
    let (Some(wrapped_key), Some(sealed)) = (wrapped_key, encrypted.strip_prefix(b"v10")) else {
        return DatabaseKey::new(&unprotect(encrypted)?);
    };
    let Some(wrapped_key) = wrapped_key.strip_prefix(b"DPAPI") else {
        bail!("Local State's os_crypt.encrypted_key lacks the DPAPI prefix");
    };
    let aes_key = unprotect(wrapped_key)?;
    let cipher = Aes256Gcm::new_from_slice(&aes_key)
        .ok()
        .context("Local State's os_crypt key is not 32 bytes")?;
    let Some((nonce, ciphertext)) = sealed.split_first_chunk::<12>() else {
        bail!("Signal Desktop's encryptedKey is too short for AES-GCM");
    };
    let plaintext = cipher
        .decrypt(&(*nonce).into(), ciphertext)
        .ok()
        .context("Signal Desktop's encryptedKey didn't decrypt under Local State's key")?;
    DatabaseKey::new(&plaintext)
}

/// The DPAPI-wrapped key in Local State's `os_crypt.encrypted_key`, if any.
fn local_state_key(local_state: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let json: serde_json::Value =
        serde_json::from_str(local_state).context("Signal Desktop's Local State is not JSON")?;
    let Some(b64) = json
        .pointer("/os_crypt/encrypted_key")
        .and_then(|v| v.as_str())
    else {
        return Ok(None);
    };
    let wrapped = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("Local State's os_crypt.encrypted_key is not base64")?;
    Ok(Some(wrapped))
}

/// Decrypts a Chromium "v10"/"v11" AES-128-CBC value, as macOS and Linux
/// safeStorage write it.
fn decrypt_cbc(encrypted: &[u8], password: &[u8], iterations: u32) -> anyhow::Result<DatabaseKey> {
    let Some(ciphertext) = encrypted
        .strip_prefix(b"v10")
        .or_else(|| encrypted.strip_prefix(b"v11"))
    else {
        bail!("Signal Desktop's encryptedKey has no v10/v11 prefix");
    };
    let mut aes_key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", iterations, &mut aes_key);
    let iv = [b' '; 16];
    let mut buf = ciphertext.to_vec();
    let plaintext = cbc::Decryptor::<aes::Aes128>::new(&aes_key.into(), &iv.into())
        .decrypt_padded::<Pkcs7>(&mut buf)
        .ok()
        .context("Signal Desktop's encryptedKey didn't decrypt; wrong safe-storage password")?;
    DatabaseKey::new(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DB_KEY: &str = "3a0aaac03a0aaac03a0aaac03a0aaac03a0aaac03a0aaac03a0aaac03a0aaac0";

    // Known-answer vectors made outside Rust, so a shared mistake in this
    // module and its tests can't cancel out: the key with Python's
    // hashlib.pbkdf2_hmac('sha1', password, b'saltysalt', iterations, 16), the
    // ciphertext with `openssl enc -aes-128-cbc -iv 2020...20` (16 spaces).
    const MAC_VECTOR: &str = "b3f16baa3295d650795e87c062550346f2b2b04b21ef8e3b03c8a0724e4b45fe19ca1d382c3af6d953557bb7ae547a2cefa71338b895919c4f4ca34f13f62f110cea79156a49ce164919d19acad2208b";

    const LINUX_V10_VECTOR: &str = "43fb2f60f0307fa4a0844f86e33b7bc5af3b95cb4cfa8e9e37626472ec7216d1070a2368c3860e13d417f10883ede0130409987ea65ec44d6f4f9692d9539f4400b66e2d72ed03895d74d44f172cd71c";

    const LINUX_V11_VECTOR: &str = "118e9e7f9956e1c17ea731c68b9a8e50717935067f960e9485be36c2dc3a88c4ba811cbd375f8fe4fff9b62574811473a798bb790d452592ca76a5cf0bbbab028cb06a5ec5c6df042f9413bd223d4898";

    // Python's cryptography AESGCM under key 00..1f, nonce 42 x 12, prefixed "v10".
    const WINDOWS_GCM_KEY: &str =
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const WINDOWS_GCM_VECTOR: &str = "76313042424242424242424242424229fd49a7ece4c6dc2b131e521cfd6c6d012bd2a787fd424c780ce943f7ef88a0d9287a1bd9dc9f89c2fcbcd9a11954a804fd85b0b028809dfdba124a25d35a6f4a70c845bd7b84ddb99796d1212862be";

    /// Stands in for DPAPI in tests: "protected" data is the plaintext.
    fn no_dpapi(blob: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(blob.to_vec())
    }

    fn local_state_with(wrapped_key: &[u8]) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(wrapped_key);
        format!(r#"{{"os_crypt":{{"encrypted_key":"{b64}"}}}}"#)
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn versioned(prefix: &[u8], ciphertext: &str) -> Vec<u8> {
        [prefix, &hex(ciphertext)].concat()
    }

    /// macOS: the Keychain's "Signal Safe Storage" password, PBKDF2-SHA1 with
    /// 1003 iterations, AES-128-CBC; the plaintext is the key as ASCII hex.
    #[test]
    fn macos_value_decrypts_with_1003_iterations_to_the_ascii_hex_key() {
        let key = decrypt_macos(&versioned(b"v10", MAC_VECTOR), b"pw").expect("decrypt");
        assert_eq!(key.0, DB_KEY);
    }

    /// Linux "v10" values predate keyrings: the password is the hardcoded
    /// "peanuts", one PBKDF2 iteration, and no keyring is asked.
    #[test]
    fn linux_v10_value_decrypts_with_peanuts_without_asking_the_keyring() {
        let key = decrypt_linux(&versioned(b"v10", LINUX_V10_VECTOR), || {
            panic!("v10 must not need the keyring")
        })
        .expect("decrypt");
        assert_eq!(key.0, DB_KEY);
    }

    /// Linux "v11" values use the keyring's password, still with one iteration.
    #[test]
    fn linux_v11_value_decrypts_with_the_keyring_password() {
        let key = decrypt_linux(&versioned(b"v11", LINUX_V11_VECTOR), || {
            Ok(b"libsecret-pw".to_vec())
        })
        .expect("decrypt");
        assert_eq!(key.0, DB_KEY);
    }

    /// Hex-encoding the decrypted key again (as an earlier importer's Windows
    /// path did) yields a key that opens nothing; it must be refused loudly.
    #[test]
    fn a_double_hex_encoded_key_is_refused() {
        let double: String = DB_KEY.bytes().map(|b| format!("{b:02x}")).collect();
        let err = DatabaseKey::new(double.as_bytes()).expect_err("accepted a 128-char key");
        assert!(err.to_string().contains("64 hex"), "unhelpful error: {err}");
    }

    /// Windows: Local State's DPAPI-wrapped AES key opens the "v10" AES-256-GCM
    /// value (nonce, then ciphertext and tag).
    #[test]
    fn windows_value_decrypts_with_the_local_state_gcm_key() {
        let local_state = local_state_with(&[b"DPAPI".as_slice(), &hex(WINDOWS_GCM_KEY)].concat());
        let key = decrypt_windows(&hex(WINDOWS_GCM_VECTOR), Some(&local_state), no_dpapi)
            .expect("decrypt");
        assert_eq!(key.0, DB_KEY);
    }
}
