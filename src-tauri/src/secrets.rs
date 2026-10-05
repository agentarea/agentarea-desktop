//! The sign-in session (access + refresh token) lives in the OS keychain —
//! macOS Keychain, Windows Credential Manager, Secret Service on Linux — so it
//! survives restarts without sitting in a plain file other apps could read.
//! One entry per AgentArea environment (the key is its API base URL).

const SERVICE: &str = "AgentArea Desktop";

#[tauri::command]
pub fn token_save(key: String, value: String) -> Result<(), String> {
    crate::keychain::set(SERVICE, &key, &value)
}

#[tauri::command]
pub fn token_load(key: String) -> Result<Option<String>, String> {
    crate::keychain::get(SERVICE, &key)
}

#[tauri::command]
pub fn token_clear(key: String) -> Result<(), String> {
    crate::keychain::delete(SERVICE, &key)
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
