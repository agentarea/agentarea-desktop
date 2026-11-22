//! Secrets the user keeps for agents (API keys and the like). Values live in the
//! OS keychain and `~/AgentArea/secrets.json` holds only metadata: name,
//! description, the hosts a secret may be sent to, timestamps. Never a value.
//!
//! Keychain layout: one item, service "AgentArea Desktop Secrets", account
//! `vault.v2`, whose value is a JSON object `{secret_id: value}`. It is read
//! once per process and kept in memory, so the OS asks for access at most once
//! per launch instead of once per secret per turn; every change rewrites the
//! whole item and only then updates the in-memory copy. A failed or denied read
//! is not remembered, so the next call asks again.
//!
//! Older versions kept one item per secret id (account = id). When `vault.v2`
//! is missing, or lacks a secret listed in secrets.json, the value is copied
//! from that legacy item and `vault.v2` is written. Legacy items are left in
//! place (only deleting a secret removes its legacy item); nothing new is
//! written to them.
//!
//! Nothing here hands a value back to the webview. Agents reach a secret only
//! through a one-time handle (secrets_mcp.rs), and `Redactor` swaps known values
//! for `[secret:NAME]` in text on its way to or from an agent CLI.
//!
//! Separate from secrets.rs, which holds the sign-in session.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "AgentArea Desktop Secrets";
/// The keychain account holding every value as one JSON object. Secret ids are
/// alphanumeric and `-`, so it can't clash with a legacy per-id account.
const BLOB_ACCOUNT: &str = "vault.v2";
const DENIED: &str = "Keychain access was denied — allow AgentArea Desktop in the macOS prompt (Always Allow) and try again";
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

/// Decoded `vault.v2` items by keychain service, filled on first use. The lock
/// is held across keychain calls, so concurrent first reads wait for one
/// prompt instead of raising several.
static CACHE: Mutex<Option<HashMap<String, BTreeMap<String, String>>>> = Mutex::new(None);

/// Where keychain items are read and written: the OS keychain in the app, an
/// in-memory map in tests. Errors are ready to show the user.
pub trait Store: Send + Sync + std::fmt::Debug {
    /// `None` when there is no such item.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String>;
    fn set(&self, service: &str, account: &str, value: &str) -> Result<(), String>;
    /// A missing item is not an error.
    fn delete(&self, service: &str, account: &str) -> Result<(), String>;
}

#[derive(Debug)]
struct Keychain;

/// Cancelled (-128) or refused (-25293) by the user at the macOS prompt.
fn is_denied(detail: &str) -> bool {
    ["code: -128", "code: -25293", "User canceled", "denied"].iter().any(|s| detail.contains(s))
}

fn keychain_error(detail: String) -> String {
    if is_denied(&detail) {
        DENIED.into()
    } else {
        detail
    }
}

impl Store for Keychain {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        crate::keychain::get(service, account).map_err(keychain_error)
    }

    fn set(&self, service: &str, account: &str, value: &str) -> Result<(), String> {
        crate::keychain::set(service, account, value).map_err(keychain_error)
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), String> {
        crate::keychain::delete(service, account).map_err(keychain_error)
    }
}

/// Where secrets live. Tests point it at a temp dir and a test keychain service.
#[derive(Clone, Debug)]
pub struct Vault {
    dir: PathBuf,
    service: String,
    store: Arc<dyn Store>,
}

impl Vault {
    pub fn user() -> Result<Vault, String> {
        let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
        Ok(Vault { dir: PathBuf::from(home).join("AgentArea"), service: SERVICE.into(), store: Arc::new(Keychain) })
    }

    #[cfg(test)]
    pub fn at(dir: PathBuf, service: &str) -> Vault {
        Vault { dir, service: service.into(), store: Arc::new(Keychain) }
    }

    /// The in-memory cache is per service, so each test needs its own.
    #[cfg(test)]
    pub fn with_store(dir: PathBuf, service: &str, store: Arc<dyn Store>) -> Vault {
        Vault { dir, service: service.into(), store }
    }

    /// The value map from the keychain, plus any secret listed in secrets.json
    /// that only has a legacy per-id item. `vault.v2` is (re)written when it
    /// was missing or gained values; an error leaves nothing behind but the
    /// legacy items, so an interrupted migration simply runs again.
    fn load(&self) -> Result<BTreeMap<String, String>, String> {
        let stored = self.store.get(&self.service, BLOB_ACCOUNT)?;
        let missing = stored.is_none();
        let mut values: BTreeMap<String, String> = match stored {
            Some(json) => serde_json::from_str(&json).map_err(|e| format!("keychain: {BLOB_ACCOUNT} is not valid JSON: {e}"))?,
            None => BTreeMap::new(),
        };
        let mut gained = false;
        for meta in self.list()? {
            if values.contains_key(&meta.id) {
                continue;
            }
            if let Some(v) = self.store.get(&self.service, &meta.id)? {
                values.insert(meta.id, v);
                gained = true;
            }
        }
        if missing || gained {
            self.write(&values)?;
        }
        Ok(values)
    }

    fn write(&self, values: &BTreeMap<String, String>) -> Result<(), String> {
        let json = serde_json::to_string(values).map_err(|e| e.to_string())?;
        self.store.set(&self.service, BLOB_ACCOUNT, &json)
    }

    /// This vault's cached values, loaded first if need be.
    fn cached<'a>(
        &self,
        cache: &'a mut Option<HashMap<String, BTreeMap<String, String>>>,
    ) -> Result<&'a mut BTreeMap<String, String>, String> {
        use std::collections::hash_map::Entry;
        Ok(match cache.get_or_insert_with(HashMap::new).entry(self.service.clone()) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(self.load()?),
        })
    }

    fn with_values<T>(&self, f: impl FnOnce(&BTreeMap<String, String>) -> T) -> Result<T, String> {
        let mut cache = CACHE.lock().map_err(|_| "vault cache poisoned")?;
        Ok(f(self.cached(&mut cache)?))
    }

    /// Change the values: the keychain is written first and the cache follows
    /// only if that worked.
    fn change_values(&self, f: impl FnOnce(&mut BTreeMap<String, String>)) -> Result<(), String> {
        let mut cache = CACHE.lock().map_err(|_| "vault cache poisoned")?;
        let values = self.cached(&mut cache)?;
        let mut next = values.clone();
        f(&mut next);
        self.write(&next)?;
        *values = next;
        Ok(())
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
            // stray keychain value.
            self.change_values(|values| {
                values.insert(meta.id.clone(), value);
            })?;
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
            if let Some(v) = value {
                self.change_values(|values| {
                    values.insert(s.id.clone(), v);
                })?;
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
            self.change_values(|values| {
                values.remove(id);
            })?;
            // Best effort: an unlisted id is never read back from its legacy item.
            let _ = self.store.delete(&self.service, id);
            Ok(())
        })
    }

    /// The value, or `None` if the keychain has none for it.
    #[allow(dead_code)] // kept for callers; the app itself uses `values`
    pub fn value(&self, id: &str) -> Result<Option<String>, String> {
        self.with_values(|values| values.get(id).cloned())
    }

    /// Every secret that has a value, with it — for redaction and exchange only.
    /// An empty vault never touches the keychain.
    pub fn values(&self) -> Result<Vec<(SecretMeta, String)>, String> {
        let metas = self.list()?;
        if metas.is_empty() {
            return Ok(Vec::new());
        }
        self.with_values(|values| {
            metas.into_iter().filter_map(|meta| values.get(&meta.id).cloned().map(|v| (meta, v))).collect()
        })
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

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Keychain stand-in that counts reads and can refuse them.
    #[derive(Debug, Default)]
    struct MemStore {
        items: Mutex<HashMap<(String, String), String>>,
        reads: AtomicUsize,
        deny: AtomicBool,
        fail_writes: AtomicBool,
    }

    impl Store for MemStore {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.deny.load(Ordering::SeqCst) {
                return Err(DENIED.into());
            }
            Ok(self.items.lock().unwrap().get(&(service.into(), account.into())).cloned())
        }
        fn set(&self, service: &str, account: &str, value: &str) -> Result<(), String> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err("keychain: write failed".into());
            }
            self.items.lock().unwrap().insert((service.into(), account.into()), value.into());
            Ok(())
        }
        fn delete(&self, service: &str, account: &str) -> Result<(), String> {
            self.items.lock().unwrap().remove(&(service.to_string(), account.to_string()));
            Ok(())
        }
    }

    impl MemStore {
        fn item(&self, service: &str, account: &str) -> Option<String> {
            self.items.lock().unwrap().get(&(service.into(), account.into())).cloned()
        }
        fn put(&self, service: &str, account: &str, value: &str) {
            self.items.lock().unwrap().insert((service.into(), account.into()), value.into());
        }
        fn blob(&self, service: &str) -> BTreeMap<String, String> {
            serde_json::from_str(&self.item(service, BLOB_ACCOUNT).expect("vault.v2 written")).unwrap()
        }
    }

    /// A vault on a fresh dir, fresh in-memory store and its own service (the
    /// cache is per service).
    fn mem_vault() -> (Vault, Arc<MemStore>, String, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aa-vault-mem-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let service = format!("test-{}", uuid::Uuid::new_v4().simple());
        let store = Arc::new(MemStore::default());
        (Vault::with_store(dir.clone(), &service, store.clone()), store, service, dir)
    }

    fn write_meta(dir: &std::path::Path, entries: &[(&str, &str)]) {
        let json: Vec<_> =
            entries.iter().map(|(id, name)| serde_json::json!({"id": id, "name": name, "createdAt": 1})).collect();
        std::fs::write(dir.join("secrets.json"), serde_json::to_string(&json).unwrap()).unwrap();
    }

    #[test]
    fn migrates_legacy_items_once() {
        let (v, store, service, dir) = mem_vault();
        write_meta(&dir, &[("a1", "ALPHA_KEY"), ("b2", "BETA_KEY"), ("c3", "GONE_KEY")]);
        store.put(&service, "a1", "alpha-value");
        store.put(&service, "b2", "beta-value");
        store.put(&service, "zz", "unlisted-value");

        let vals = v.values().unwrap();
        assert_eq!(
            vals.iter().map(|(m, v)| (m.name.as_str(), v.as_str())).collect::<Vec<_>>(),
            [("ALPHA_KEY", "alpha-value"), ("BETA_KEY", "beta-value")],
            "missing legacy item is just absent"
        );
        let blob = store.blob(&service);
        assert_eq!(blob.len(), 2, "unlisted legacy items are not pulled in");
        assert_eq!(blob["a1"], "alpha-value");
        assert_eq!(store.item(&service, "a1").as_deref(), Some("alpha-value"), "legacy items stay");

        // A second process: the blob exists, so only the still-missing id is
        // looked up again.
        CACHE.lock().unwrap().as_mut().unwrap().remove(&service);
        store.reads.store(0, Ordering::SeqCst);
        assert_eq!(v.values().unwrap().len(), 2);
        assert_eq!(store.reads.load(Ordering::SeqCst), 2, "vault.v2 + the one id it lacks");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn heals_a_secret_only_in_a_legacy_item() {
        let (v, store, service, dir) = mem_vault();
        write_meta(&dir, &[("a1", "ALPHA_KEY"), ("b2", "BETA_KEY")]);
        store.put(&service, BLOB_ACCOUNT, r#"{"a1":"alpha-value"}"#);
        store.put(&service, "b2", "beta-from-older-build");
        assert_eq!(v.value("b2").unwrap().as_deref(), Some("beta-from-older-build"));
        assert_eq!(store.blob(&service).len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn interrupted_migration_runs_again() {
        let (v, store, service, dir) = mem_vault();
        write_meta(&dir, &[("a1", "ALPHA_KEY")]);
        store.put(&service, "a1", "alpha-value");
        store.fail_writes.store(true, Ordering::SeqCst);
        assert!(v.values().is_err(), "can't write vault.v2");
        assert!(store.item(&service, BLOB_ACCOUNT).is_none());
        store.fail_writes.store(false, Ordering::SeqCst);
        assert_eq!(v.values().unwrap()[0].1, "alpha-value");
        assert_eq!(store.blob(&service)["a1"], "alpha-value");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_hit_skips_the_store() {
        let (v, store, _service, dir) = mem_vault();
        let s = v.create("CACHED_KEY".into(), None, vec![], "cached-value".into()).unwrap();
        let after_create = store.reads.load(Ordering::SeqCst);
        for _ in 0..5 {
            assert_eq!(v.values().unwrap()[0].1, "cached-value");
            assert_eq!(v.value(&s.id).unwrap().as_deref(), Some("cached-value"));
        }
        assert_eq!(store.reads.load(Ordering::SeqCst), after_create, "no keychain reads after the first load");
        // A clone (as the MCP server holds) shares the cache.
        assert_eq!(v.clone().value(&s.id).unwrap().as_deref(), Some("cached-value"));
        assert_eq!(store.reads.load(Ordering::SeqCst), after_create);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn create_update_delete_round_trip() {
        let (v, store, service, dir) = mem_vault();
        assert!(v.values().unwrap().is_empty());
        assert_eq!(store.reads.load(Ordering::SeqCst), 0, "an empty vault never asks the keychain");

        let a = v.create("A_KEY".into(), Some(" d ".into()), vec!["API.x.io".into()], "value-aaa".into()).unwrap();
        let b = v.create("B_KEY".into(), None, vec![], "value-bbb".into()).unwrap();
        assert_eq!((a.description.as_deref(), a.allowed_hosts.as_slice()), (Some("d"), &["api.x.io".to_string()][..]));
        assert!(v.create("A_KEY".into(), None, vec![], "other".into()).unwrap_err().contains("already exists"));
        assert!(!std::fs::read_to_string(dir.join("secrets.json")).unwrap().contains("value-aaa"));
        assert!(store.item(&service, &a.id).is_none(), "new values go only to vault.v2");

        let u = v.update(&a.id, Some("".into()), None, Some("value-a2".into())).unwrap();
        assert_eq!(u.description, None);
        assert_eq!(v.value(&a.id).unwrap().as_deref(), Some("value-a2"));
        assert_eq!(store.blob(&service)[&a.id], "value-a2", "written through");
        v.update(&b.id, Some("only meta".into()), None, None).unwrap();
        assert_eq!(v.value(&b.id).unwrap().as_deref(), Some("value-bbb"));

        v.touch(&a.id, "why").unwrap();
        assert_eq!(v.list().unwrap()[0].last_purpose.as_deref(), Some("why"));

        store.put(&service, &b.id, "legacy-bbb");
        v.delete(&b.id).unwrap();
        assert_eq!(v.value(&b.id).unwrap(), None);
        assert!(!store.blob(&service).contains_key(&b.id));
        assert!(store.item(&service, &b.id).is_none(), "its legacy item goes too");
        assert_eq!(v.values().unwrap().iter().map(|(m, _)| m.name.as_str()).collect::<Vec<_>>(), ["A_KEY"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_write_leaves_cache_and_list_unchanged() {
        let (v, store, service, dir) = mem_vault();
        let a = v.create("A_KEY".into(), None, vec![], "value-aaa".into()).unwrap();
        store.fail_writes.store(true, Ordering::SeqCst);
        assert!(v.update(&a.id, None, None, Some("value-new".into())).is_err());
        assert!(v.create("B_KEY".into(), None, vec![], "value-bbb".into()).is_err());
        assert!(v.delete(&a.id).is_err());
        assert_eq!(v.value(&a.id).unwrap().as_deref(), Some("value-aaa"));
        assert_eq!(v.list().unwrap().len(), 1);
        assert_eq!(store.blob(&service).len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn denied_read_is_not_cached() {
        let (v, store, service, dir) = mem_vault();
        write_meta(&dir, &[("a1", "ALPHA_KEY")]);
        store.put(&service, BLOB_ACCOUNT, r#"{"a1":"alpha-value"}"#);
        store.deny.store(true, Ordering::SeqCst);
        let err = v.values().unwrap_err();
        assert!(err.contains("Always Allow"), "{err}");
        assert!(v.value("a1").is_err(), "retried, still denied");
        assert_eq!(store.reads.load(Ordering::SeqCst), 2);
        store.deny.store(false, Ordering::SeqCst);
        assert_eq!(v.values().unwrap()[0].1, "alpha-value");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn blob_json_round_trips() {
        let (v, store, service, dir) = mem_vault();
        let tricky = "line1\nline2 \"quoted\" \\ back ünïcode 🔑 {\"json\":true}";
        let s = v.create("TRICKY_KEY".into(), None, vec![], tricky.into()).unwrap();
        let raw = store.item(&service, BLOB_ACCOUNT).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&raw).unwrap().is_object());
        assert_eq!(store.blob(&service)[&s.id], tricky);
        CACHE.lock().unwrap().as_mut().unwrap().remove(&service);
        assert_eq!(v.value(&s.id).unwrap().as_deref(), Some(tricky), "decoded from the store");

        store.put(&service, BLOB_ACCOUNT, "not json");
        CACHE.lock().unwrap().as_mut().unwrap().remove(&service);
        assert!(v.value(&s.id).unwrap_err().contains("not valid JSON"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recognises_denied_keychain_errors() {
        assert!(is_denied(r#"Error { code: -128, message: "User canceled the operation." } User canceled the operation."#));
        assert!(is_denied("Error { code: -25293 }"));
        assert!(!is_denied("Error { code: -25291 }"));
        assert!(!is_denied(""));
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
        let _ = Keychain.delete("AgentArea Desktop Secrets (test)", BLOB_ACCOUNT);
        let _ = std::fs::remove_dir_all(dir);
    }
}
