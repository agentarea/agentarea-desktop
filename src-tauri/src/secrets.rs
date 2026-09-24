//! The sign-in session (access + refresh token) lives in the OS keychain —
//! macOS Keychain, Windows Credential Manager, Secret Service on Linux — so it
//! survives restarts without sitting in a plain file other apps could read.
//! One entry per AgentArea environment (the key is its API base URL).

const SERVICE: &str = "AgentArea Desktop";

fn entry(key: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, key).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn token_save(key: String, value: String) -> Result<(), String> {
    entry(&key)?.set_password(&value).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn token_load(key: String) -> Result<Option<String>, String> {
    match entry(&key)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn token_clear(key: String) -> Result<(), String> {
    match entry(&key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches the real OS keychain, so it is opt-in: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        let key = format!("test:{}", std::process::id());
        token_save(key.clone(), "{\"accessToken\":\"x\"}".into()).unwrap();
        assert_eq!(token_load(key.clone()).unwrap().as_deref(), Some("{\"accessToken\":\"x\"}"));
        token_clear(key.clone()).unwrap();
        assert_eq!(token_load(key).unwrap(), None);
    }
}
