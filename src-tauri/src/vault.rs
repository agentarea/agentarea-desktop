//! Secrets the user keeps for agents (API keys and the like). Values live in the
//! OS keychain — service "AgentArea Desktop Secrets", one entry per secret id —
//! and `~/AgentArea/secrets.json` holds only metadata: name, description, the
//! hosts a secret may be sent to, timestamps. Never a value.
//!
//! Nothing here hands a value back to the webview. Agents reach a secret only
//! through a one-time handle (secrets_mcp.rs), and `Redactor` swaps known values
//! for `[secret:NAME]` in text on its way to or from an agent CLI.
//!
//! Separate from secrets.rs, which holds the sign-in session.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "AgentArea Desktop Secrets";
/// Shorter values are left alone by the redactor: swapping every "1234" would
/// mangle ordinary text and code.
pub const MIN_REDACT_LEN: usize = 6;
const MAX_VALUE_LEN: usize = 64 * 1024;

/// One entry of secrets.json.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretMeta {
    pub id: String,
    /// UPPER_SNAKE, unique; what agents and `[secret:NAME]` markers call it.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hosts an agent may send the value to: `api.example.com` or `*.example.com`.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Unix milliseconds.
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    /// What the agent said it needed the secret for, last time it was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_purpose: Option<String>,
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// `api.example.com`, `localhost`, `127.0.0.1` or `*.example.com` (a wildcard
/// needs two labels under it, so `*.com` is out). No scheme, port or path.
fn valid_host(host: &str) -> bool {
    let (wild, rest) = match host.strip_prefix("*.") {
        Some(rest) => (true, rest),
        None => (false, host),
    };
    let labels: Vec<&str> = rest.split('.').collect();
    rest.len() <= 253
        && (!wild || labels.len() >= 2)
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// Trimmed, lowercased, deduplicated; an invalid one is an error naming it.
fn normalize_hosts(hosts: Vec<String>) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for h in hosts {
        let h = h.trim().to_ascii_lowercase();
        if h.is_empty() {
            continue;
        }
        if !valid_host(&h) {
            return Err(format!("invalid host: {h} (use api.example.com or *.example.com, no https:// or path)"));
        }
        if !out.contains(&h) {
            out.push(h);
        }
    }
    Ok(out)
}

fn normalize_description(d: Option<String>) -> Result<Option<String>, String> {
    let d = d.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
    if d.as_ref().is_some_and(|d| d.chars().count() > 500) {
        return Err("description is longer than 500 characters".into());
    }
    Ok(d)
}

fn check_value(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("value is required".into());
    }
    if value.len() > MAX_VALUE_LEN {
        return Err("value is larger than 64 KiB".into());
    }
    if value.contains('\0') {
        return Err("value must not contain NUL".into());
    }
    Ok(())
}

/// Entries of secrets.json; invalid ones and repeated names are skipped, so one
/// bad hand edit doesn't hide the rest.
pub fn parse_secrets(json: &str) -> Result<Vec<SecretMeta>, String> {
    let raw: Vec<serde_json::Value> = serde_json::from_str(json).map_err(|e| format!("secrets.json: {e}"))?;
    let mut out: Vec<SecretMeta> = Vec::new();
    for v in raw {
        let Ok(s) = serde_json::from_value::<SecretMeta>(v) else { continue };
        let id_ok = !s.id.is_empty() && s.id.len() <= 64 && s.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if id_ok && valid_name(&s.name) && !out.iter().any(|o| o.name == s.name || o.id == s.id) {
            out.push(s);
        }
    }
    Ok(out)
}

/// secrets.json is read-modify-written by UI commands and the MCP server.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// Where secrets live. Tests point it at a temp dir and a test keychain service.
#[derive(Clone, Debug)]
pub struct Vault {
    dir: PathBuf,
    service: String,
}

impl Vault {
    pub fn user() -> Result<Vault, String> {
        let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
        Ok(Vault { dir: PathBuf::from(home).join("AgentArea"), service: SERVICE.into() })
    }

    #[cfg(test)]
    pub fn at(dir: PathBuf, service: &str) -> Vault {
        Vault { dir, service: service.into() }
    }

    fn entry(&self, id: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(&self.service, id).map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Result<Vec<SecretMeta>, String> {
        match std::fs::read_to_string(self.dir.join("secrets.json")) {
            Ok(json) => parse_secrets(&json),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("secrets.json: {e}")),
        }
    }

    /// 0600 although it holds no values: names and hosts are still nobody's business.
    fn save(&self, secrets: &[SecretMeta]) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("vault dir: {e}"))?;
        let json = serde_json::to_string_pretty(secrets).map_err(|e| e.to_string())?;
        crate::write_private(&self.dir.join("secrets.json"), &json).map_err(|e| format!("save secrets.json: {e}"))
    }

    /// Change secrets.json under the file lock.
    fn edit<T>(&self, f: impl FnOnce(&mut Vec<SecretMeta>) -> Result<T, String>) -> Result<T, String> {
        let _guard = FILE_LOCK.lock().map_err(|_| "vault lock poisoned")?;
        let mut secrets = self.list()?;
        let out = f(&mut secrets)?;
        self.save(&secrets)?;
        Ok(out)
    }

    pub fn create(
        &self,
        name: String,
        description: Option<String>,
        allowed_hosts: Vec<String>,
        value: String,
    ) -> Result<SecretMeta, String> {
        let name = name.trim().to_string();
        if !valid_name(&name) {
            return Err(format!("invalid name: {name} (use UPPER_SNAKE_CASE, e.g. GITHUB_TOKEN)"));
        }
        check_value(&value)?;
        let meta = SecretMeta {
            id: uuid::Uuid::new_v4().simple().to_string(),
            name,
            description: normalize_description(description)?,
            allowed_hosts: normalize_hosts(allowed_hosts)?,
            created_at: now_ms(),
            last_used_at: None,
            last_purpose: None,
        };
        self.edit(|secrets| {
            if secrets.iter().any(|s| s.name == meta.name) {
                return Err(format!("a secret named {} already exists", meta.name));
            }
            // Keychain first: a listed secret without a value is worse than a
            // stray keychain entry.
            self.entry(&meta.id)?.set_password(&value).map_err(|e| format!("keychain: {e}"))?;
            secrets.push(meta.clone());
            Ok(meta.clone())
        })
    }

    /// `None` leaves a field as it is; an empty description clears it.
    pub fn update(
        &self,
        id: &str,
        description: Option<String>,
        allowed_hosts: Option<Vec<String>>,
        value: Option<String>,
    ) -> Result<SecretMeta, String> {
        let description = description.map(|d| normalize_description(Some(d))).transpose()?;
        let allowed_hosts = allowed_hosts.map(normalize_hosts).transpose()?;
        if let Some(v) = &value {
            check_value(v)?;
        }
        self.edit(|secrets| {
            let s = secrets.iter_mut().find(|s| s.id == id).ok_or_else(|| format!("no secret with id {id}"))?;
            if let Some(v) = &value {
                self.entry(&s.id)?.set_password(v).map_err(|e| format!("keychain: {e}"))?;
            }
            if let Some(d) = description {
                s.description = d;
            }
            if let Some(h) = allowed_hosts {
                s.allowed_hosts = h;
            }
            Ok(s.clone())
        })
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        self.edit(|secrets| {
            secrets.retain(|s| s.id != id);
            match self.entry(id)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(format!("keychain: {e}")),
            }
        })
    }

    /// The value, or `None` if its keychain entry is gone.
    pub fn value(&self, id: &str) -> Result<Option<String>, String> {
        match self.entry(id)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("keychain: {e}")),
        }
    }

    /// Every secret that has a value, with it — for redaction and exchange only.
    pub fn values(&self) -> Result<Vec<(SecretMeta, String)>, String> {
        let mut out = Vec::new();
        for meta in self.list()? {
            if let Some(v) = self.value(&meta.id)? {
                out.push((meta, v));
            }
        }
        Ok(out)
    }

    /// Record a successful exchange.
    pub fn touch(&self, id: &str, purpose: &str) -> Result<(), String> {
        self.edit(|secrets| {
            if let Some(s) = secrets.iter_mut().find(|s| s.id == id) {
                s.last_used_at = Some(now_ms());
                s.last_purpose = Some(purpose.to_string());
            }
            Ok(())
        })
    }
}

// ── Redaction ────────────────────────────────────────────────────────────────

/// Unreserved characters stay, every other byte becomes `%XX`.
pub fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The shapes a value commonly takes in text: raw, JSON-escaped, base64
/// (standard, unpadded, URL-safe) and URL-encoded (`%20` and form `+`).
fn forms(value: &str) -> Vec<String> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let json = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into());
    let mut out = vec![
        value.to_string(),
        json[1..json.len() - 1].to_string(),
        STANDARD.encode(value),
        STANDARD_NO_PAD.encode(value),
        URL_SAFE.encode(value),
        URL_SAFE_NO_PAD.encode(value),
        percent_encode(value),
        percent_encode(value).replace("%20", "+"),
    ];
    out.sort();
    out.dedup();
    out
}

/// Swaps known secret values (in all their `forms`) for `[secret:NAME]`.
/// Longest needle first, so a padded base64 form isn't half-replaced by its
/// unpadded prefix.
#[derive(Clone, Default)]
pub struct Redactor {
    needles: Vec<(String, String)>,
}

impl Redactor {
    pub fn new<'a>(secrets: impl IntoIterator<Item = (&'a str, &'a str)>) -> Redactor {
        let mut needles: Vec<(String, String)> = secrets
            .into_iter()
            .filter(|(_, value)| value.chars().count() >= MIN_REDACT_LEN)
            .flat_map(|(name, value)| forms(value).into_iter().map(move |f| (f, format!("[secret:{name}]"))))
            .collect();
        needles.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        Redactor { needles }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (needle, marker) in &self.needles {
            if out.contains(needle.as_str()) {
                out = out.replace(needle.as_str(), marker);
            }
        }
        out
    }
}

/// Appended to a turn's prompt when the vault has secrets, so the agent knows
/// what the markers mean and how to use a secret without seeing it.
pub fn agent_hint(names: &[String], tools: bool) -> String {
    let mut hint = format!(
        "[AgentArea vault] The user's vault holds these secrets: {}. `[secret:NAME]` in this conversation stands for a vault secret whose value is hidden from you.",
        names.join(", ")
    );
    if tools {
        hint.push_str(
            " To use one, call the agentarea_secrets tools: request_secret(name, purpose) gives a one-time handle; put the handle where the secret goes (a header value, the URL query or the body) in one http_request call to a host the secret allows. Never ask the user to paste a secret.",
        );
    }
    hint
}

// ── Tauri commands ───────────────────────────────────────────────────────────

#[tauri::command]
pub fn vault_list() -> Result<Vec<SecretMeta>, String> {
    Vault::user()?.list()
}

#[tauri::command]
pub fn vault_create(
    name: String,
    description: Option<String>,
    allowed_hosts: Vec<String>,
    value: String,
) -> Result<SecretMeta, String> {
    Vault::user()?.create(name, description, allowed_hosts, value)
}

#[tauri::command]
pub fn vault_update(
    id: String,
    description: Option<String>,
    allowed_hosts: Option<Vec<String>>,
    value: Option<String>,
) -> Result<SecretMeta, String> {
    Vault::user()?.update(&id, description, allowed_hosts, value)
}

#[tauri::command]
pub fn vault_delete(id: String) -> Result<(), String> {
    Vault::user()?.delete(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_secrets_json() {
        let json = r#"[
          {"id":"a1","name":"GITHUB_TOKEN","description":"repo access","allowedHosts":["api.github.com"],"createdAt":1,"lastUsedAt":2},
          {"id":"b2","name":"OPENAI_KEY","createdAt":3},
          {"id":"c3","name":"GITHUB_TOKEN","createdAt":4},
          {"id":"a1","name":"OTHER","createdAt":5},
          {"id":"../x","name":"EVIL","createdAt":6},
          {"id":"d4","name":"lower","createdAt":7},
          {"id":"e5","name":"NO_TS"},
          "junk"
        ]"#;
        let s = parse_secrets(json).unwrap();
        assert_eq!(s.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["GITHUB_TOKEN", "OPENAI_KEY"]);
        assert_eq!(s[0].allowed_hosts, ["api.github.com"]);
        assert_eq!(s[0].last_used_at, Some(2));
        assert!(s[1].allowed_hosts.is_empty() && s[1].description.is_none());
        assert!(parse_secrets("{}").is_err(), "must be an array");

        // Round-trips in camelCase without inventing empty optionals.
        let out = serde_json::to_value(&s[1]).unwrap();
        assert!(out.get("allowedHosts").is_some() && out.get("createdAt").is_some());
        assert!(out.get("description").is_none() && out.get("lastUsedAt").is_none() && out.get("lastPurpose").is_none());
        assert_eq!(parse_secrets(&serde_json::to_string(&s).unwrap()).unwrap(), s);
    }

    #[test]
    fn validates_names_and_hosts() {
        for ok in ["A", "GITHUB_TOKEN", "KEY_2"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "github", "_X", "2X", "A-B", "A B", &"A".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
        for ok in ["api.github.com", "localhost", "127.0.0.1", "*.example.com", "a-b.io"] {
            assert!(valid_host(ok), "{ok}");
        }
        for bad in ["", "*", "*.com", "https://x.com", "x.com/path", "x.com:443", "-a.com", "a..com", "X.COM", "a b.com"] {
            assert!(!valid_host(bad), "{bad}");
        }
        assert_eq!(
            normalize_hosts(vec![" API.GitHub.com ".into(), "".into(), "api.github.com".into(), "*.x.io".into()]).unwrap(),
            ["api.github.com", "*.x.io"]
        );
        assert!(normalize_hosts(vec!["https://api.github.com".into()]).unwrap_err().contains("invalid host"));
    }

    #[test]
    fn redacts_raw_base64_urlencoded_and_json_forms() {
        use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
        let value = "s3cr3t/value+with space\"q";
        let r = Redactor::new([("API_KEY", value), ("SHORT", "abc")]);
        let text = format!(
            "raw {value} | b64 {} | b64url {} | url {} | form {} | json {} | short abc",
            STANDARD.encode(value),
            URL_SAFE_NO_PAD.encode(value),
            percent_encode(value),
            percent_encode(value).replace("%20", "+"),
            serde_json::to_string(value).unwrap(),
        );
        let out = r.redact(&text);
        assert_eq!(
            out,
            "raw [secret:API_KEY] | b64 [secret:API_KEY] | b64url [secret:API_KEY] | url [secret:API_KEY] | form [secret:API_KEY] | json \"[secret:API_KEY]\" | short abc"
        );
        assert!(!out.contains("s3cr3t"));
        assert_eq!(Redactor::default().redact("unchanged"), "unchanged");
    }

    /// Touches the real OS keychain, so it is opt-in: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn keychain_vault_round_trip() {
        let dir = std::env::temp_dir().join(format!("aa-vault-{}", uuid::Uuid::new_v4().simple()));
        let v = Vault::at(dir.clone(), "AgentArea Desktop Secrets (test)");
        let s = v.create("TEST_KEY".into(), Some(" d ".into()), vec!["API.x.io".into()], "value-123".into()).unwrap();
        assert_eq!((s.description.as_deref(), s.allowed_hosts.as_slice()), (Some("d"), &["api.x.io".to_string()][..]));
        assert!(v.create("TEST_KEY".into(), None, vec![], "other".into()).unwrap_err().contains("already exists"));
        assert_eq!(v.value(&s.id).unwrap().as_deref(), Some("value-123"));
        let raw = std::fs::read_to_string(dir.join("secrets.json")).unwrap();
        assert!(!raw.contains("value-123"), "values never go to the file");

        let u = v.update(&s.id, Some("".into()), None, Some("value-456".into())).unwrap();
        assert_eq!(u.description, None);
        assert_eq!(v.values().unwrap()[0].1, "value-456");
        v.touch(&s.id, "why").unwrap();
        let listed = &v.list().unwrap()[0];
        assert!(listed.last_used_at.is_some() && listed.last_purpose.as_deref() == Some("why"));

        v.delete(&s.id).unwrap();
        assert!(v.list().unwrap().is_empty());
        assert_eq!(v.value(&s.id).unwrap(), None, "keychain entry removed too");
        let _ = std::fs::remove_dir_all(dir);
    }
}
