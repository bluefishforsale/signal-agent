//! Windows: the key encrypted with DPAPI under the user's logon credentials,
//! in a file in the profile directory.

use std::{fs, io::ErrorKind, path::PathBuf, ptr, slice};

use anyhow::{Context, bail};
use relay_core::ProfileName;
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};

use crate::{SecretStore, StoreKey, key_path};

// Binds each blob to this use, so a DPAPI blob made by another app as the same
// user can't be swapped in, and ours can't be unwrapped by a generic call.
const ENTROPY: &[u8] = b"signal-agent store key v1";

/// Each profile's key as a DPAPI blob in `store.key.dpapi`.
/// The default keeps blobs in each profile's platform data directory.
#[derive(Default)]
pub struct DpapiFileStore {
    base: Option<PathBuf>,
}

impl DpapiFileStore {
    /// Blobs under `base` instead, laid out as `ProfilePaths::under` does.
    pub fn under(base: PathBuf) -> Self {
        Self { base: Some(base) }
    }
}

fn blob(bytes: &[u8]) -> anyhow::Result<CRYPT_INTEGER_BLOB> {
    Ok(CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).context("DPAPI input over 4 GiB")?,
        pbData: bytes.as_ptr().cast_mut(),
    })
}

/// Runs one DPAPI call and copies out the buffer it allocated.
fn dpapi_call(
    input: &[u8],
    call: impl FnOnce(
        *const CRYPT_INTEGER_BLOB,
        *const CRYPT_INTEGER_BLOB,
        *mut CRYPT_INTEGER_BLOB,
    ) -> windows_sys::core::BOOL,
) -> anyhow::Result<Vec<u8>> {
    let input = blob(input)?;
    let entropy = blob(ENTROPY)?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    if call(&input, &entropy, &mut output) == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: on success DPAPI returns a LocalAlloc'd buffer of cbData bytes,
    // which we copy and then free exactly once.
    let out = unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { LocalFree(output.pbData.cast()) };
    Ok(out)
}

impl SecretStore for DpapiFileStore {
    fn get(&self, profile: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
        let path = key_path(self.base.as_ref(), profile, "store.key.dpapi")?;
        let sealed = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("unable to read \"{}\"", path.display()));
            }
        };
        let bytes = dpapi_call(&sealed, |input, entropy, output| unsafe {
            CryptUnprotectData(
                input,
                ptr::null_mut(),
                entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                output,
            )
        })
        .with_context(|| format!("unable to decrypt \"{}\"", path.display()))?;
        let Ok(bytes) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            bail!(
                "\"{}\" decrypts to {} bytes, expected 32",
                path.display(),
                bytes.len()
            );
        };
        Ok(Some(StoreKey::from_bytes(bytes)))
    }

    fn set(&self, profile: &ProfileName, key: &StoreKey) -> anyhow::Result<()> {
        let path = key_path(self.base.as_ref(), profile, "store.key.dpapi")?;
        let sealed = dpapi_call(key.as_bytes(), |input, entropy, output| unsafe {
            CryptProtectData(
                input,
                ptr::null(),
                entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                output,
            )
        })
        .with_context(|| format!("unable to encrypt the key for {profile}"))?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
        }
        fs::write(&path, sealed).with_context(|| format!("unable to write \"{}\"", path.display()))
    }

    fn delete(&self, profile: &ProfileName) -> anyhow::Result<()> {
        let path = key_path(self.base.as_ref(), profile, "store.key.dpapi")?;
        match fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                Err(e).with_context(|| format!("unable to delete \"{}\"", path.display()))
            }
            _ => Ok(()),
        }
    }
}
