//! What the user approves before a plugin's backend may be used, and how that
//! approval is kept honest.
//!
//! **Fingerprint.** SHA-256 over the plugin id, its folder path and every file
//! in the folder (sorted paths; size and SHA-256 of each file; symlink targets),
//! which includes plugin.json and so the command, env and cloud instance. Not
//! covered (documented exclusion): `.DS_Store` files anywhere and a top-level
//! `.git/` folder. Any other change, including a new file, changes it. A
//! symlink that leads outside the folder, more than `MAX_FILES` files or more
//! than `MAX_BYTES` make the folder unverifiable, so it can't be approved.
//! Plugins keep their data in `AGENTAREA_DATA_DIR`, outside the folder.
//!
//! **Approval.** `{fingerprint, mac}` in plugins-state.json, where `mac` is
//! HMAC-SHA256 over the id and fingerprint with a key that lives only in the
//! macOS Keychain (one item, read once per launch and cached; a failed or
//! denied read is not cached). A same-user process that can write
//! ~/AgentArea can delete approvals but not make one.
//!
//! **Checks.** Status and routing compare against a cached fingerprint (re-
//! hashed when any file's size or mtime changes, re-checked at most every
//! `RECHECK`); a local server start always re-hashes the whole folder.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use ring::{digest, hmac};

pub const MAX_FILES: usize = 20_000;
pub const MAX_BYTES: u64 = 256 << 20;
/// How long a cached fingerprint is trusted before the folder is re-stat'ed.
const RECHECK: Duration = Duration::from_secs(2);

/// Not part of the fingerprint: Finder litter, and git metadata at the top.
fn excluded(rel: &str, top_level: bool) -> bool {
    rel.rsplit('/').next() == Some(".DS_Store") || (top_level && rel == ".git")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One walked entry: (relative path, kind, size, mtime).
type Stat = (String, char, u64, Option<SystemTime>);

/// Every file and symlink under `dir`, sorted by path; refuses links out.
fn walk(dir: &Path) -> Result<Vec<(Stat, PathBuf)>, String> {
    let base = dir.canonicalize().map_err(|e| format!("plugin folder: {e}"))?;
    let mut out = Vec::new();
    let mut stack = vec![(base.clone(), String::new())];
    let mut total = 0u64;
    while let Some((path, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&path).map_err(|e| format!("{rel}: {e}"))? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if excluded(&child, rel.is_empty()) {
                continue;
            }
            let meta = entry.path().symlink_metadata().map_err(|e| format!("{child}: {e}"))?;
            if meta.file_type().is_symlink() {
                let target = entry.path().canonicalize().map_err(|_| format!("{child}: broken symlink"))?;
                if !target.starts_with(&base) {
                    return Err(format!("{child} links outside the plugin folder"));
                }
                out.push(((child, 'L', 0, meta.modified().ok()), entry.path()));
            } else if meta.is_dir() {
                stack.push((entry.path(), child));
            } else {
                total += meta.len();
                out.push(((child, 'F', meta.len(), meta.modified().ok()), entry.path()));
            }
            if out.len() > MAX_FILES || total > MAX_BYTES {
                return Err(format!("the plugin folder is too large to verify (over {MAX_FILES} files or {} MiB)", MAX_BYTES >> 20));
            }
        }
    }
    out.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
    Ok(out)
}

/// The fingerprint of `dir` as it is now, every file read.
pub fn fingerprint(id: &str, dir: &Path) -> Result<String, String> {
    Ok(hash_walk(id, dir, &walk(dir)?)?.0)
}

/// (fingerprint, file count)
fn hash_walk(id: &str, dir: &Path, files: &[(Stat, PathBuf)]) -> Result<(String, usize), String> {
    let mut ctx = digest::Context::new(&digest::SHA256);
    let mut field = |b: &[u8]| {
        ctx.update(&(b.len() as u64).to_be_bytes());
        ctx.update(b);
    };
    field(b"aa-plugin-trust-v1");
    field(id.as_bytes());
    field(dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()).to_string_lossy().as_bytes());
    for ((rel, kind, _, _), path) in files {
        field(&[*kind as u8]);
        field(rel.as_bytes());
        if *kind == 'L' {
            let target = std::fs::read_link(path).map_err(|e| format!("{rel}: {e}"))?;
            field(target.to_string_lossy().as_bytes());
        } else {
            let bytes = std::fs::read(path).map_err(|e| format!("{rel}: {e}"))?;
            field(digest::digest(&digest::SHA256, &bytes).as_ref());
        }
    }
    Ok((hex(ctx.finish().as_ref()), files.len()))
}

struct Cached {
    stats: u64,
    fingerprint: Result<String, String>,
    checked: Instant,
}

static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);

/// The fingerprint, reusing the last one while no file's size or mtime moved
/// (and without even looking for `RECHECK`). For status and routing; a start
/// uses `fingerprint`.
pub fn fingerprint_cached(id: &str, dir: &Path) -> Result<String, String> {
    let key = dir.join(id);
    if let Some(c) = CACHE.lock().ok().and_then(|c| c.as_ref().and_then(|m| m.get(&key).map(|c| (c.checked, c.fingerprint.clone())))) {
        if c.0.elapsed() < RECHECK {
            return c.1;
        }
    }
    let files = walk(dir);
    let stats = {
        let mut h = DefaultHasher::new();
        match &files {
            Ok(f) => f.iter().for_each(|(s, _)| s.hash(&mut h)),
            Err(e) => e.hash(&mut h),
        }
        h.finish()
    };
    let mut guard = CACHE.lock().map_err(|_| "poisoned")?;
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(c) = map.get_mut(&key).filter(|c| c.stats == stats) {
        c.checked = Instant::now();
        return c.fingerprint.clone();
    }
    drop(guard);
    let fingerprint = files.and_then(|f| hash_walk(id, dir, &f).map(|(fp, _)| fp));
    if let Ok(mut guard) = CACHE.lock() {
        guard.get_or_insert_with(HashMap::new).insert(key, Cached { stats, fingerprint: fingerprint.clone(), checked: Instant::now() });
    }
    fingerprint
}

/// Number of files the fingerprint covers (shown with the approval).
pub fn file_count(dir: &Path) -> Result<usize, String> {
    Ok(walk(dir)?.len())
}

// ── the approval key ─────────────────────────────────────────────────────────

const KEY_SERVICE: &str = "AgentArea Desktop Secrets";
/// Has dots, so it can't clash with a vault secret's id-named item.
const KEY_ACCOUNT: &str = "plugin-approvals.key.v1";

static KEY: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// The HMAC key: from the Keychain once per launch (created on first use).
/// Held under the lock across the Keychain call so concurrent first uses wait
/// for one prompt; an error is not cached.
fn key() -> Result<Vec<u8>, String> {
    let mut cached = KEY.lock().map_err(|_| "approval key lock poisoned")?;
    if let Some(k) = cached.as_ref() {
        return Ok(k.clone());
    }
    let k = load_key()?;
    *cached = Some(k.clone());
    Ok(k)
}

#[cfg(not(test))]
fn load_key() -> Result<Vec<u8>, String> {
    let entry = keyring::Entry::new(KEY_SERVICE, KEY_ACCOUNT).map_err(|e| format!("keychain: {e}"))?;
    match entry.get_password() {
        Ok(hexkey) if hexkey.len() == 64 => {
            (0..32).map(|i| u8::from_str_radix(&hexkey[i * 2..i * 2 + 2], 16).map_err(|_| "keychain: bad approval key".to_string())).collect()
        }
        Ok(_) => Err("keychain: bad plugin approval key".into()),
        Err(keyring::Error::NoEntry) => {
            let mut k = vec![0u8; 32];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut k).map_err(|_| "no randomness")?;
            entry.set_password(&hex(&k)).map_err(|e| format!("keychain: {e}"))?;
            Ok(k)
        }
        Err(e) => Err(format!("keychain: {e}")),
    }
}

/// Tests never touch the Keychain.
#[cfg(test)]
fn load_key() -> Result<Vec<u8>, String> {
    let _ = (KEY_SERVICE, KEY_ACCOUNT);
    Ok(b"test-only plugin approval key 32".to_vec())
}

fn message(id: &str, fingerprint: &str) -> Vec<u8> {
    format!("aa-plugin-approval-v1\0{id}\0{fingerprint}").into_bytes()
}

/// The MAC that makes an approval of `fingerprint` for `id` count.
pub fn sign(id: &str, fingerprint: &str) -> Result<String, String> {
    let k = hmac::Key::new(hmac::HMAC_SHA256, &key()?);
    Ok(hex(hmac::sign(&k, &message(id, fingerprint)).as_ref()))
}

/// Whether `mac` is ours for (`id`, `fingerprint`). No key, no approval.
pub fn verify(id: &str, fingerprint: &str, mac: &str) -> bool {
    let Ok(k) = key() else { return false };
    let Some(tag) = (mac.len() == 64)
        .then(|| (0..32).map(|i| u8::from_str_radix(&mac[i * 2..i * 2 + 2], 16).ok()).collect::<Option<Vec<u8>>>())
        .flatten()
    else {
        return false;
    };
    hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, &k), &message(id, fingerprint), &tag).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-trust-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("plugin.json"), r#"{"id":"p","title":"P"}"#).unwrap();
        std::fs::write(dir.join("server.mjs"), "console.log(1)").unwrap();
        std::fs::write(dir.join("lib/util.mjs"), "export {}").unwrap();
        dir
    }

    #[test]
    fn any_file_change_changes_the_fingerprint() {
        let dir = temp();
        let fp = fingerprint("p", &dir).unwrap();
        assert_eq!(fp.len(), 64);
        assert_eq!(fingerprint("p", &dir).unwrap(), fp, "stable");
        assert_ne!(fingerprint("q", &dir).unwrap(), fp, "bound to the id");
        assert_eq!(file_count(&dir).unwrap(), 3);

        std::fs::write(dir.join("lib/util.mjs"), "export {x}").unwrap();
        let edited = fingerprint("p", &dir).unwrap();
        assert_ne!(edited, fp, "nested file edited");
        std::fs::write(dir.join("new.js"), "").unwrap();
        assert_ne!(fingerprint("p", &dir).unwrap(), edited, "file added");
        std::fs::remove_file(dir.join("new.js")).unwrap();
        assert_eq!(fingerprint("p", &dir).unwrap(), edited, "back to the same content");

        // The documented exclusions.
        std::fs::write(dir.join(".DS_Store"), "x").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref").unwrap();
        assert_eq!(fingerprint("p", &dir).unwrap(), edited);
        // …but a nested .git is covered.
        std::fs::create_dir_all(dir.join("lib/.git")).unwrap();
        std::fs::write(dir.join("lib/.git/x"), "1").unwrap();
        assert_ne!(fingerprint("p", &dir).unwrap(), edited);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn symlinks_out_of_the_folder_are_refused() {
        let dir = temp();
        let outside = temp();
        std::os::unix::fs::symlink(dir.join("server.mjs"), dir.join("inside.mjs")).unwrap();
        assert!(fingerprint("p", &dir).is_ok(), "a link inside is fine");
        std::os::unix::fs::symlink(outside.join("server.mjs"), dir.join("out.mjs")).unwrap();
        assert!(fingerprint("p", &dir).unwrap_err().contains("outside the plugin folder"));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn cached_fingerprint_follows_edits() {
        let dir = temp();
        let fp = fingerprint_cached("p", &dir).unwrap();
        assert_eq!(fp, fingerprint("p", &dir).unwrap());
        std::fs::write(dir.join("server.mjs"), "console.log('changed, and longer')").unwrap();
        std::thread::sleep(RECHECK + Duration::from_millis(100));
        assert_ne!(fingerprint_cached("p", &dir).unwrap(), fp, "picked up after RECHECK");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn macs_bind_id_and_fingerprint() {
        let mac = sign("p", "aa").unwrap();
        assert!(verify("p", "aa", &mac));
        assert!(!verify("p", "ab", &mac), "other fingerprint");
        assert!(!verify("q", "aa", &mac), "other plugin");
        assert!(!verify("p", "aa", &"0".repeat(64)), "forged");
        assert!(!verify("p", "aa", "zz"), "malformed");
    }
}
