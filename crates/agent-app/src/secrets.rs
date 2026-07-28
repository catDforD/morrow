#[cfg(windows)]
use base64::Engine;
#[cfg(windows)]
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretProtection {
    Current,
    Legacy,
    Plaintext,
    Dpapi,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SecretString {
    value: String,
    protection: SecretProtection,
}

impl SecretString {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            protection: SecretProtection::Current,
        }
    }

    pub(crate) fn expose(&self) -> &str {
        &self.value
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub(crate) fn requires_rewrite(&self) -> bool {
        #[cfg(windows)]
        {
            !matches!(
                self.protection,
                SecretProtection::Current | SecretProtection::Dpapi
            )
        }
        #[cfg(not(windows))]
        {
            matches!(self.protection, SecretProtection::Legacy)
        }
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Serialize for SecretString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        protect(self.expose())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SerializedSecret::deserialize(deserializer)?;
        match value {
            SerializedSecret::Legacy(value) => Ok(Self {
                value,
                protection: SecretProtection::Legacy,
            }),
            SerializedSecret::Protected(value) => {
                let protection = match &value {
                    ProtectedSecret::Plaintext(_) => SecretProtection::Plaintext,
                    ProtectedSecret::Dpapi(_) => SecretProtection::Dpapi,
                };
                unprotect(value)
                    .map(|value| Self { value, protection })
                    .map_err(serde::de::Error::custom)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum SerializedSecret {
    Legacy(String),
    Protected(ProtectedSecret),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "protection", content = "value", rename_all = "snake_case")]
enum ProtectedSecret {
    Plaintext(String),
    Dpapi(String),
}

fn protect(value: &str) -> Result<ProtectedSecret, SecretError> {
    #[cfg(windows)]
    {
        return protect_dpapi(value.as_bytes())
            .map(|encrypted| ProtectedSecret::Dpapi(STANDARD.encode(encrypted)));
    }
    #[cfg(not(windows))]
    {
        Ok(ProtectedSecret::Plaintext(value.to_string()))
    }
}

fn unprotect(value: ProtectedSecret) -> Result<String, SecretError> {
    match value {
        ProtectedSecret::Plaintext(value) => Ok(value),
        ProtectedSecret::Dpapi(value) => {
            #[cfg(windows)]
            {
                let encrypted = STANDARD.decode(value).map_err(SecretError::Base64)?;
                let decrypted = unprotect_dpapi(&encrypted)?;
                String::from_utf8(decrypted).map_err(SecretError::Utf8)
            }
            #[cfg(not(windows))]
            {
                let _ = value;
                Err(SecretError::UnsupportedProtection)
            }
        }
    }
}

pub(crate) fn replace_file(temporary: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(temporary, target)
    }
    #[cfg(windows)]
    {
        if !target.exists() {
            return std::fs::rename(temporary, target);
        }
        use std::os::windows::ffi::OsStrExt;
        use std::ptr;
        use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

        let target = target
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let temporary = temporary
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let replaced = unsafe {
            ReplaceFileW(
                target.as_ptr(),
                temporary.as_ptr(),
                ptr::null(),
                REPLACEFILE_WRITE_THROUGH,
                ptr::null(),
                ptr::null(),
            )
        };
        if replaced == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
fn protect_dpapi(value: &[u8]) -> Result<Vec<u8>, SecretError> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };

    let mut input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(value.len()).map_err(|_| SecretError::TooLarge)?,
        pbData: value.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    let result = unsafe {
        CryptProtectData(
            &mut input,
            ptr::null(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if result == 0 {
        return Err(SecretError::Dpapi(std::io::Error::last_os_error()));
    }
    let encrypted = unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(output.pbData.cast());
        bytes
    };
    Ok(encrypted)
}

#[cfg(windows)]
fn unprotect_dpapi(value: &[u8]) -> Result<Vec<u8>, SecretError> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let mut input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(value.len()).map_err(|_| SecretError::TooLarge)?,
        pbData: value.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    let result = unsafe {
        CryptUnprotectData(
            &mut input,
            ptr::null_mut(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if result == 0 {
        return Err(SecretError::Dpapi(std::io::Error::last_os_error()));
    }
    let decrypted = unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(output.pbData.cast());
        bytes
    };
    Ok(decrypted)
}

#[derive(Debug, Error)]
enum SecretError {
    #[cfg(windows)]
    #[error("secret is too large")]
    TooLarge,
    #[cfg(windows)]
    #[error("Windows DPAPI failed: {0}")]
    Dpapi(#[source] std::io::Error),
    #[cfg(windows)]
    #[error("secret is not valid base64: {0}")]
    Base64(#[source] base64::DecodeError),
    #[cfg(windows)]
    #[error("secret is not valid UTF-8: {0}")]
    Utf8(#[source] std::string::FromUtf8Error),
    #[error("DPAPI-protected secrets can only be opened by Windows")]
    UnsupportedProtection,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_round_trip_and_debug_is_redacted() {
        let secret = SecretString::new("sensitive-value");
        let json = serde_json::to_string(&secret).expect("serialize secret");
        let restored = serde_json::from_str::<SecretString>(&json).expect("deserialize secret");
        assert_eq!(restored.expose(), "sensitive-value");
        assert_eq!(format!("{secret:?}"), "<redacted>");
        #[cfg(windows)]
        assert!(!json.contains("sensitive-value"));
    }

    #[test]
    fn legacy_plain_string_is_accepted() {
        let secret = serde_json::from_str::<SecretString>(r#""legacy""#).expect("legacy secret");
        assert_eq!(secret.expose(), "legacy");
    }
}
