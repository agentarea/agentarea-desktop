//! One door to the OS keychain for the sign-in session, the vault and the
//! plugin-approval key.
//!
//! On macOS every build goes through `/usr/bin/security`, as Claude Code does.
//! An item's "Always Allow" is tied to the app's code signature, and for an app
//! without an Apple Team ID (every dev build, and releases until they are
//! Developer ID signed) the keychain's partition list pins it to the exact
//! binary hash — so each rebuild or update asked for the password again.
//! `security` is Apple-signed and never changes, so one "Always Allow" sticks.
//! The cost: anything that can run `security` as the user can read these items
//! without a prompt, the same trade-off Claude Code makes for its credentials.
//!
//! Windows and Linux use the keyring crate (Credential Manager, Secret Service).

/// The item's value, `None` when there is no such item.
pub fn get(service: &str, account: &str) -> Result<Option<String>, String> {
    imp::get(service, account)
}

/// Create or replace the item.
pub fn set(service: &str, account: &str, value: &str) -> Result<(), String> {
    imp::set(service, account, value)
}

/// Remove the item; a missing one is not an error.
pub fn delete(service: &str, account: &str) -> Result<(), String> {
    imp::delete(service, account)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::Command;

    const SECURITY: &str = "/usr/bin/security";
    /// `security` exits with this when the item doesn't exist.
    const NOT_FOUND: i32 = 44;

    fn run(args: &[&str]) -> Result<std::process::Output, String> {
        Command::new(SECURITY).args(args).output().map_err(|e| format!("keychain: {e}"))
    }

    fn failure(out: &std::process::Output) -> String {
        format!("keychain: {}", String::from_utf8_lossy(&out.stderr).trim())
    }

    pub fn get(service: &str, account: &str) -> Result<Option<String>, String> {
        let out = run(&["find-generic-password", "-s", service, "-a", account, "-w"])?;
        if out.status.code() == Some(NOT_FOUND) {
            return Ok(None);
        }
        if !out.status.success() {
            return Err(failure(&out));
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        Ok(Some(decode(raw.strip_suffix('\n').unwrap_or(&raw))))
    }

    /// `security -w` prints a value with non-printable (e.g. non-ASCII) bytes as
    /// hex. Everything stored here is text, so hex that decodes to UTF-8 JSON
    /// is that case; anything else is the value as is.
    pub(super) fn decode(s: &str) -> String {
        let hexish = !s.is_empty() && s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit());
        if hexish {
            let bytes: Option<Vec<u8>> =
                (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect();
            if let Some(Ok(text)) = bytes.map(String::from_utf8) {
                if text.starts_with('{') || text.starts_with('[') {
                    return text;
                }
            }
        }
        s.to_string()
    }

    pub fn set(service: &str, account: &str, value: &str) -> Result<(), String> {
        // -U replaces an existing item. The value goes in as hex (-X) so any
        // byte survives; it is briefly visible to processes of this same user.
        let hex: String = value.bytes().map(|b| format!("{b:02x}")).collect();
        let out = run(&["add-generic-password", "-U", "-s", service, "-a", account, "-X", &hex])?;
        if out.status.success() { Ok(()) } else { Err(failure(&out)) }
    }

    pub fn delete(service: &str, account: &str) -> Result<(), String> {
        let out = run(&["delete-generic-password", "-s", service, "-a", account])?;
        if out.status.success() || out.status.code() == Some(NOT_FOUND) { Ok(()) } else { Err(failure(&out)) }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    fn entry(service: &str, account: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(service, account).map_err(describe)
    }

    /// Keep the platform detail (e.g. "User canceled") so callers can tell a
    /// denied prompt from other failures.
    fn describe(e: keyring::Error) -> String {
        match &e {
            keyring::Error::PlatformFailure(inner) | keyring::Error::NoStorageAccess(inner) => {
                format!("keychain: {e} ({inner:?} {inner})")
            }
            _ => format!("keychain: {e}"),
        }
    }

    pub fn get(service: &str, account: &str) -> Result<Option<String>, String> {
        match entry(service, account)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(describe(e)),
        }
    }

    pub fn set(service: &str, account: &str, value: &str) -> Result<(), String> {
        entry(service, account)?.set_password(value).map_err(describe)
    }

    pub fn delete(service: &str, account: &str) -> Result<(), String> {
        match entry(service, account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(describe(e)),
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::imp::decode;

    #[test]
    fn decodes_hex_printed_json_only() {
        let json = r#"{"a":"привет"}"#;
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(decode(&hex), json);
        assert_eq!(decode(json), json);
        // A plain hex value (e.g. a key) stays as it is.
        assert_eq!(decode("deadbeef"), "deadbeef");
        assert_eq!(decode(""), "");
    }
}
