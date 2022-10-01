use byteorder::{ByteOrder, LittleEndian};
use std::iter::once;
use std::mem::MaybeUninit;
use std::str;
use winapi::shared::minwindef::{DWORD, FILETIME};
use winapi::shared::winerror::{
    ERROR_BAD_USERNAME, ERROR_INVALID_FLAGS, ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND,
    ERROR_NO_SUCH_LOGON_SESSION,
};
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::wincred::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_MAX_CREDENTIAL_BLOB_SIZE,
    CRED_MAX_GENERIC_TARGET_NAME_LENGTH, CRED_MAX_STRING_LENGTH, CRED_MAX_USERNAME_LENGTH,
    CRED_PERSIST_ENTERPRISE, CRED_TYPE_GENERIC, PCREDENTIALW, PCREDENTIAL_ATTRIBUTEW,
};

use super::credential::{Credential, CredentialApi, CredentialBuilder, CredentialBuilderApi};
use super::error::{Error as ErrorCode, Result};

/// Windows has only one credential store, and each credential is identified
/// by a single string called the "target name".  But generic credentials
/// also have three pieces of metadata with suggestive names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinCredential {
    pub username: String,
    pub target_name: String,
    pub target_alias: String,
    pub comment: String,
}

impl CredentialApi for WinCredential {
    // DWORD is u32
    // LPCWSTR is *const u16
    // BOOL is i32 (false = 0, true = 1)
    // PCREDENTIALW = *mut CREDENTIALW
    fn set_password(&self, password: &str) -> Result<()> {
        self.validate_attributes(password)?;
        let mut username = to_wstr(&self.username);
        let mut target_name = to_wstr(&self.target_name);
        let mut target_alias = to_wstr(&self.target_alias);
        let mut comment = to_wstr(&self.comment);
        // Password strings are converted to UTF-16, because that's the native
        // charset for Windows strings.  This allows editing of the password in
        // the Windows native UI.  But the storage for the credential is actually
        // a little-endian blob, because passwords can contain anything.
        let blob_u16 = to_wstr_no_null(password);
        let mut blob = vec![0; blob_u16.len() * 2];
        LittleEndian::write_u16_into(&blob_u16, &mut blob);
        let blob_len = blob.len() as u32;
        let flags = 0;
        let cred_type = CRED_TYPE_GENERIC;
        let persist = CRED_PERSIST_ENTERPRISE;
        // Ignored by CredWriteW
        let last_written = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        // TODO: Allow setting attributes on Windows credentials
        let attribute_count = 0;
        let attributes: PCREDENTIAL_ATTRIBUTEW = std::ptr::null_mut();
        let mut credential = CREDENTIALW {
            Flags: flags,
            Type: cred_type,
            TargetName: target_name.as_mut_ptr(),
            Comment: comment.as_mut_ptr(),
            LastWritten: last_written,
            CredentialBlobSize: blob_len,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: persist,
            AttributeCount: attribute_count,
            Attributes: attributes,
            TargetAlias: target_alias.as_mut_ptr(),
            UserName: username.as_mut_ptr(),
        };
        // raw pointer to credential, is coerced from &mut
        let p_credential: PCREDENTIALW = &mut credential;
        // Call windows API
        match unsafe { CredWriteW(p_credential, 0) } {
            0 => Err(decode_error()),
            _ => Ok(()),
        }
    }

    fn get_password(&self) -> Result<String> {
        self.extract_from_platform(extract_password)
    }

    fn delete_password(&self) -> Result<()> {
        self.validate_attributes("")?;
        let target_name = to_wstr(&self.target_name);
        let cred_type = CRED_TYPE_GENERIC;
        match unsafe { CredDeleteW(target_name.as_ptr(), cred_type, 0) } {
            0 => Err(decode_error()),
            _ => Ok(()),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl WinCredential {
    fn validate_attributes(&self, password: &str) -> Result<()> {
        if self.username.len() > CRED_MAX_USERNAME_LENGTH as usize {
            return Err(ErrorCode::TooLong(
                String::from("username"),
                CRED_MAX_USERNAME_LENGTH,
            ));
        }
        if self.target_name.is_empty() {
            return Err(ErrorCode::Invalid(
                "target".to_string(),
                "cannot be empty".to_string(),
            ));
        }
        if self.target_name.len() > CRED_MAX_GENERIC_TARGET_NAME_LENGTH as usize {
            return Err(ErrorCode::TooLong(
                String::from("target"),
                CRED_MAX_GENERIC_TARGET_NAME_LENGTH,
            ));
        }
        if self.target_alias.len() > CRED_MAX_STRING_LENGTH as usize {
            return Err(ErrorCode::TooLong(
                String::from("target alias"),
                CRED_MAX_STRING_LENGTH,
            ));
        }
        if self.comment.len() > CRED_MAX_STRING_LENGTH as usize {
            return Err(ErrorCode::TooLong(
                String::from("comment"),
                CRED_MAX_STRING_LENGTH,
            ));
        }
        if password.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
            return Err(ErrorCode::TooLong(
                String::from("password"),
                CRED_MAX_CREDENTIAL_BLOB_SIZE,
            ));
        }
        Ok(())
    }

    pub fn get_credential(&self) -> Result<Self> {
        self.extract_from_platform(Self::extract_credential)
    }

    fn extract_from_platform<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&CREDENTIALW) -> Result<T>,
    {
        self.validate_attributes("")?;
        let mut p_credential = MaybeUninit::uninit();
        // at this point, p_credential is just a pointer to nowhere.
        // The allocation happens in the `CredReadW` call below.
        let result = {
            let cred_type = CRED_TYPE_GENERIC;
            let target_name = to_wstr(&self.target_name);
            unsafe {
                CredReadW(
                    target_name.as_ptr(),
                    cred_type,
                    0,
                    p_credential.as_mut_ptr(),
                )
            }
        };
        match result {
            0 => {
                // `CredReadW` failed, so no allocation has been done, so no free needs to be done
                Err(decode_error())
            }
            _ => {
                // `CredReadW` succeeded, so p_credential points at an allocated credential.
                // To do anything with it, we need to cast it to the right type.  That takes two steps:
                // first we remove the "uninitialized" guard from around it, then we reinterpret it as a
                // pointer to the right structure type.
                let p_credential = unsafe { p_credential.assume_init() };
                let w_credential: CREDENTIALW = unsafe { *p_credential };
                // Now we can apply the passed extractor function to the credential.
                let result = f(&w_credential);
                // Finally, we free the allocated credential.
                unsafe {
                    CredFree(p_credential as *mut _);
                }
                result
            }
        }
    }

    fn extract_credential(w_credential: &CREDENTIALW) -> Result<Self> {
        Ok(Self {
            username: unsafe { from_wstr(w_credential.UserName) },
            target_name: unsafe { from_wstr(w_credential.TargetName) },
            target_alias: unsafe { from_wstr(w_credential.TargetAlias) },
            comment: unsafe { from_wstr(w_credential.Comment) },
        })
    }

    pub fn new_with_target(
        target: Option<&str>,
        service: &str,
        user: &str,
    ) -> Result<WinCredential> {
        const VERSION: &str = env!("CARGO_PKG_VERSION");
        let metadata = format!(
            "keyring-rs v{} for service '{}', user '{}'",
            VERSION, service, user
        );
        let credential = if let Some(target) = target {
            // if target.is_empty() {
            //     return Err(ErrorCode::Invalid(
            //         "target".to_string(),
            //         "cannot be empty".to_string(),
            //     ));
            // }
            Self {
                // On Windows, the target name is all that's used to
                // search for the credential, so we allow clients to
                // specify it if they want a different convention.
                username: user.to_string(),
                target_name: target.to_string(),
                target_alias: String::new(),
                comment: metadata,
            }
        } else {
            Self {
                // Note: default concatenation of user and service name is
                // used because windows uses target_name as sole identifier.
                // See the module docs for more rationale.  Also see this issue
                // for Python: https://github.com/jaraco/keyring/issues/47
                //
                // Note that it's OK to have an empty user or service name,
                // because the format for the target name will not be empty.
                // But it's certainly not recommended.
                username: user.to_string(),
                target_name: format!("{}.{}", user, service),
                target_alias: String::new(),
                comment: metadata,
            }
        };
        credential.validate_attributes("")?;
        Ok(credential)
    }
}

pub struct WinCredentialBuilder {}

pub fn default_credential_builder() -> Box<CredentialBuilder> {
    Box::new(WinCredentialBuilder {})
}

impl CredentialBuilderApi for WinCredentialBuilder {
    fn build(&self, target: Option<&str>, service: &str, user: &str) -> Result<Box<Credential>> {
        Ok(Box::new(WinCredential::new_with_target(
            target, service, user,
        )?))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn extract_password(credential: &CREDENTIALW) -> Result<String> {
    // get password blob
    let blob_pointer: *const u8 = credential.CredentialBlob;
    let blob_len: usize = credential.CredentialBlobSize as usize;
    let blob = unsafe { std::slice::from_raw_parts(blob_pointer, blob_len) };
    // 3rd parties may write credential data with an odd number of bytes,
    // so we make sure that we don't try to decode those as utf16
    if blob.len() % 2 != 0 {
        let err = ErrorCode::BadEncoding(blob.to_vec());
        return Err(err);
    }
    // Now we know this _can_ be a UTF-16 string, so convert it to
    // as UTF-16 vector and then try to decode it.
    let mut blob_u16 = vec![0; blob.len() / 2];
    LittleEndian::read_u16_into(blob, &mut blob_u16);
    String::from_utf16(&blob_u16).map_err(|_| ErrorCode::BadEncoding(blob.to_vec()))
}

fn to_wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

fn to_wstr_no_null(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

unsafe fn from_wstr(ws: *const u16) -> String {
    // null pointer case, return empty string
    if ws.is_null() {
        return String::new();
    }
    // this code from https://stackoverflow.com/a/48587463/558006
    let len = (0..).take_while(|&i| *ws.offset(i) != 0).count();
    let slice = std::slice::from_raw_parts(ws, len);
    String::from_utf16_lossy(slice)
}

#[derive(Debug)]
pub struct Error(u32); // Windows error codes are long ints

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.0 {
            ERROR_NO_SUCH_LOGON_SESSION => write!(f, "Windows ERROR_NO_SUCH_LOGON_SESSION"),
            ERROR_NOT_FOUND => write!(f, "Windows ERROR_NOT_FOUND"),
            ERROR_BAD_USERNAME => write!(f, "Windows ERROR_BAD_USERNAME"),
            ERROR_INVALID_FLAGS => write!(f, "Windows ERROR_INVALID_FLAGS"),
            ERROR_INVALID_PARAMETER => write!(f, "Windows ERROR_INVALID_PARAMETER"),
            err => write!(f, "Windows error code {}", err),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

fn decode_error() -> ErrorCode {
    match unsafe { GetLastError() } {
        ERROR_NOT_FOUND => ErrorCode::NoEntry,
        ERROR_NO_SUCH_LOGON_SESSION => {
            ErrorCode::NoStorageAccess(wrap(ERROR_NO_SUCH_LOGON_SESSION))
        }
        err => ErrorCode::PlatformFailure(wrap(err)),
    }
}

fn wrap(code: DWORD) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(Error(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tests::{generate_random_string, generate_random_string_of_len};
    use crate::{Credential, Entry};

    fn entry_new(service: &str, user: &str) -> Entry {
        entry_new_with_target(None, service, user)
    }

    fn entry_new_with_target(target: Option<&str>, service: &str, user: &str) -> Entry {
        match WinCredential::new_with_target(target, service, user) {
            Ok(credential) => {
                let credential: Box<Credential> = Box::new(credential);
                Entry::new_with_credential(credential)
            }
            Err(err) => {
                panic!("Couldn't create entry (service: {service}, user: {user}): {err:?}")
            }
        }
    }

    #[test]
    fn test_bad_password() {
        // the first malformed sequence can't be UTF-16 because it has an odd number of bytes.
        // the second malformed sequence has a first surrogate marker (0xd800) without a matching
        // companion (it's taken from the String::fromUTF16 docs).
        let odd_bytes = b"1".to_vec();
        let malformed_utf16 = [0xD834, 0xDD1E, 0x006d, 0x0075, 0xD800, 0x0069, 0x0063];
        let mut malformed_bytes: Vec<u8> = vec![0; malformed_utf16.len() * 2];
        LittleEndian::write_u16_into(&malformed_utf16, &mut malformed_bytes);
        for bytes in [&odd_bytes, &malformed_bytes] {
            let credential = make_platform_credential(bytes.clone());
            match extract_password(&credential) {
                Err(ErrorCode::BadEncoding(str)) => assert_eq!(&str, bytes),
                Err(other) => panic!(
                    "Bad password ({:?}) decode gave wrong error: {}",
                    bytes, other
                ),
                Ok(s) => panic!("Bad password ({:?}) decode gave results: {:?}", bytes, &s),
            }
        }
    }

    fn make_platform_credential(mut password: Vec<u8>) -> CREDENTIALW {
        let last_written = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let attribute_count = 0;
        let attributes: PCREDENTIAL_ATTRIBUTEW = std::ptr::null_mut();
        CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: std::ptr::null_mut(),
            Comment: std::ptr::null_mut(),
            LastWritten: last_written,
            CredentialBlobSize: password.len() as u32,
            CredentialBlob: password.as_mut_ptr(),
            Persist: CRED_PERSIST_ENTERPRISE,
            AttributeCount: attribute_count,
            Attributes: attributes,
            TargetAlias: std::ptr::null_mut(),
            UserName: std::ptr::null_mut(),
        }
    }

    #[test]
    fn test_bad_inputs() {
        let cred = WinCredential {
            username: "username".to_string(),
            target_name: "target_name".to_string(),
            target_alias: "target_alias".to_string(),
            comment: "comment".to_string(),
        };
        for (attr, len) in [
            ("username", CRED_MAX_USERNAME_LENGTH),
            ("target name", CRED_MAX_GENERIC_TARGET_NAME_LENGTH),
            ("target alias", CRED_MAX_STRING_LENGTH),
            ("comment", CRED_MAX_STRING_LENGTH),
            ("password", CRED_MAX_CREDENTIAL_BLOB_SIZE),
        ] {
            let long_string = generate_random_string_of_len(1 + len as usize);
            let mut bad_cred = cred.clone();
            let mut password = "password";
            match attr {
                "username" => bad_cred.username = long_string.clone(),
                "target name" => bad_cred.target_name = long_string.clone(),
                "target alias" => bad_cred.target_alias = long_string.clone(),
                "comment" => bad_cred.comment = long_string.clone(),
                "password" => password = &long_string,
                other => panic!("unexpected attribute: {}", other),
            }
            let credential: Box<Credential> = Box::new(bad_cred);
            let entry = Entry::new_with_credential(credential);
            validate_attribute_too_long(entry.set_password(password), attr, len);
        }
    }

    fn validate_attribute_too_long(result: Result<()>, attr: &str, len: u32) {
        match result {
            Err(ErrorCode::TooLong(arg, val)) => {
                assert_eq!(&arg, attr, "Error names wrong attribute");
                assert_eq!(val, len, "Error names wrong limit");
            }
            Err(other) => panic!("Error is not '{} too long': {}", attr, other),
            Ok(_) => panic!("No error when {} too long", attr),
        }
    }

    #[test]
    fn test_invalid_parameter() {
        let credential = WinCredential::new_with_target(None, "", "");
        assert!(
            credential.is_err(),
            "Secret service doesn't allow empty attributes"
        );
        assert!(
            credential.is_ok(),
            "Secret service does allow empty attributes"
        );
        assert!(
            matches!(credential, Err(ErrorCode::Invalid(_, _))),
            "Created credential with empty service"
        );
        let credential = WinCredential::new_with_target(None, "service", "");
        assert!(
            matches!(credential, Err(ErrorCode::Invalid(_, _))),
            "Created entry with empty user"
        );
        let credential = WinCredential::new_with_target(Some(""), "service", "user");
        assert!(
            matches!(credential, Err(ErrorCode::Invalid(_, _))),
            "Created entry with empty target"
        );
    }

    #[test]
    fn test_missing_entry() {
        let name = generate_random_string();
        let entry = entry_new(&name, &name);
        assert!(
            matches!(entry.get_password(), Err(ErrorCode::NoEntry)),
            "Missing entry has password"
        )
    }

    #[test]
    fn test_empty_password() {
        let name = generate_random_string();
        let entry = entry_new(&name, &name);
        let in_pass = "";
        entry
            .set_password(in_pass)
            .expect("Can't set empty password");
        let out_pass = entry.get_password().expect("Can't get empty password");
        assert_eq!(
            in_pass, out_pass,
            "Retrieved and set empty passwords don't match"
        );
        entry.delete_password().expect("Can't delete password");
        assert!(
            matches!(entry.get_password(), Err(ErrorCode::NoEntry)),
            "Able to read a deleted password"
        )
    }

    #[test]
    fn test_round_trip_ascii_password() {
        let name = generate_random_string();
        let entry = entry_new(&name, &name);
        let password = "test ascii password";
        entry
            .set_password(password)
            .expect("Can't set ascii password");
        let stored_password = entry.get_password().expect("Can't get ascii password");
        assert_eq!(
            stored_password, password,
            "Retrieved and set ascii passwords don't match"
        );
        entry
            .delete_password()
            .expect("Can't delete ascii password");
        assert!(
            matches!(entry.get_password(), Err(ErrorCode::NoEntry)),
            "Able to read a deleted ascii password"
        )
    }

    #[test]
    fn test_round_trip_non_ascii_password() {
        let name = generate_random_string();
        let entry = entry_new(&name, &name);
        let password = "このきれいな花は桜です";
        entry
            .set_password(password)
            .expect("Can't set non-ascii password");
        let stored_password = entry.get_password().expect("Can't get non-ascii password");
        assert_eq!(
            stored_password, password,
            "Retrieved and set non-ascii passwords don't match"
        );
        entry
            .delete_password()
            .expect("Can't delete non-ascii password");
        assert!(
            matches!(entry.get_password(), Err(ErrorCode::NoEntry)),
            "Able to read a deleted non-ascii password"
        )
    }

    #[test]
    fn test_update() {
        let name = generate_random_string();
        let entry = entry_new(&name, &name);
        let password = "test ascii password";
        entry
            .set_password(password)
            .expect("Can't set initial ascii password");
        let stored_password = entry.get_password().expect("Can't get ascii password");
        assert_eq!(
            stored_password, password,
            "Retrieved and set initial ascii passwords don't match"
        );
        let password = "このきれいな花は桜です";
        entry
            .set_password(password)
            .expect("Can't update ascii with non-ascii password");
        let stored_password = entry.get_password().expect("Can't get non-ascii password");
        assert_eq!(
            stored_password, password,
            "Retrieved and updated non-ascii passwords don't match"
        );
        entry
            .delete_password()
            .expect("Can't delete updated password");
        assert!(
            matches!(entry.get_password(), Err(ErrorCode::NoEntry)),
            "Able to read a deleted updated password"
        )
    }

    #[test]
    fn test_get_credential() {
        let name = generate_random_string();
        let entry = entry_new(&name, &name);
        let password = "test get password";
        entry
            .set_password(password)
            .expect("Can't set test get password");
        let credential: &WinCredential = entry
            .inner
            .as_any()
            .downcast_ref()
            .expect("Not a windows credential");
        let actual = credential.get_credential().expect("Can't read credential");
        assert_eq!(
            actual.target_name, credential.target_name,
            "Target names don't match"
        );
        assert_eq!(
            actual.target_alias, credential.target_alias,
            "Target aliases don't match"
        );
        assert_eq!(
            actual.username, credential.username,
            "Usernames don't match"
        );
        assert_eq!(actual.comment, credential.comment, "Comments don't match");
    }
}
