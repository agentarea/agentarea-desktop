//! What the user approves before a plugin's backend may be used, and the
//! copy of it that actually runs.
//!
//! Threat model (docs/plugin-backends.md): the approval is **consent and
//! integrity**. What runs is exactly what the user reviewed, and a plugin
//! can't be swapped for other code afterwards. It is not a sandbox against
//! the user's own agents, which already run shell commands as the user.
//!
//! **Tree.** A plugin folder is read as its regular files. A symlink is
//! followed when its target stays inside the folder: the target's content
//! counts, as if it were a file (or folder) at the link's path. A symlink
//! out of the folder, or a cycle, makes the folder unusable. `.DS_Store`
//! entries and a top-level `.git/` are skipped as entries (a link *to* them
//! still brings their content in). Limits: `MAX_FILES`, `MAX_BYTES`.
//!
//! **Fingerprint.** SHA-256 over the plugin id and every file's path,
//! executable bit and content hash, in sorted order. It is the same for the
//! live folder and for its snapshot, and doesn't depend on where either lives.
//!
//! **Snapshot.** Approving copies the tree into an app-owned folder,
//! `<app data>/plugin-snapshots/<id>/<fingerprint>/`, re-hashes the **copy**,
//! and marks it read-only. The backend always runs from there (cwd =
//! snapshot), and every start re-hashes it. The live folder can change
//! freely: a change shows up as "update available" and changes nothing that
//! runs until the user reviews and approves again.
//!
//! **Approval key and epochs.** One Keychain item holds the HMAC key that
//! signs approvals and, per plugin, a counter. Every approve and revoke bumps
//! the counter, and an approval is valid only for the current value, so a
//! revoked (or superseded) approval put back into plugins-state.json doesn't
//! count. The item is read once per launch and cached; a failed read isn't
//! cached, and with no item nothing is approved.

use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use ring::{digest, hmac};
use serde::{Deserialize, Serialize};

/// A Node plugin with its dependencies installed fits comfortably.
pub const MAX_FILES: usize = 100_000;
pub const MAX_BYTES: u64 = 512 << 20;
/// How long a cached live-folder fingerprint is trusted before re-stat'ing.
const RECHECK: Duration = Duration::from_secs(2);

fn excluded(rel: &str, top_level: bool) -> bool {
    rel.rsplit('/').next() == Some(".DS_Store") || (top_level && rel == ".git")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    s.len().is_multiple_of(2).then(|| (0..s.len() / 2).map(|i| u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()).collect()).flatten()
}

/// One file of a tree: its path in the tree, where its content is read
/// from (a symlink's target), size, mtime, executable.
struct File {
    rel: String,
    src: PathBuf,
    len: u64,
    mtime: Option<SystemTime>,
    exec: bool,
}

#[cfg(unix)]
fn is_exec(meta: &std::fs::Metadata) -> bool {
    std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_: &std::fs::Metadata) -> bool {
    false
}

/// The tree under `dir`, sorted by path, symlinks inside followed.
fn collect(dir: &Path) -> Result<Vec<File>, String> {
    let base = dir.canonicalize().map_err(|e| format!("plugin folder: {e}"))?;
    let mut out = Vec::new();
    let mut total = 0u64;
    // (real folder, path in the tree, real folders above it: a cycle guard)
    let mut stack = vec![(base.clone(), String::new(), vec![base.clone()])];
    while let Some((real, rel, above)) = stack.pop() {
        for entry in std::fs::read_dir(&real).map_err(|e| format!("{}: {e}", if rel.is_empty() { "." } else { &rel }))? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if excluded(&child, rel.is_empty()) {
                continue;
            }
            let link = entry.path().symlink_metadata().map_err(|e| format!("{child}: {e}"))?.file_type().is_symlink();
            let target = if link {
                let t = entry.path().canonicalize().map_err(|_| format!("{child}: broken symlink"))?;
                if !t.starts_with(&base) {
                    return Err(format!("{child} links outside the plugin folder"));
                }
                t
            } else {
                entry.path()
            };
            let meta = std::fs::metadata(&target).map_err(|e| format!("{child}: {e}"))?;
            if meta.is_dir() {
                if above.contains(&target) {
                    return Err(format!("{child} links back into itself"));
                }
                let mut next = above.clone();
                next.push(target.clone());
                stack.push((target, child, next));
            } else if meta.is_file() {
                total += meta.len();
                out.push(File { rel: child, src: target, len: meta.len(), mtime: meta.modified().ok(), exec: is_exec(&meta) });
            } else {
                return Err(format!("{child} is not a regular file"));
            }
            if out.len() > MAX_FILES || total > MAX_BYTES {
                return Err(format!(
                    "the plugin folder is too large (over {MAX_FILES} files or {} MiB)",
                    MAX_BYTES >> 20
                ));
            }
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// A hashed tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Tree {
    pub fingerprint: String,
    pub files: usize,
    pub bytes: u64,
}

fn hash(id: &str, files: &[File]) -> Result<Tree, String> {
    let mut ctx = digest::Context::new(&digest::SHA256);
    let mut field = |b: &[u8]| {
        ctx.update(&(b.len() as u64).to_be_bytes());
        ctx.update(b);
    };
    field(b"aa-plugin-tree-v2");
    field(id.as_bytes());
    for f in files {
        field(f.rel.as_bytes());
        field(&[f.exec as u8]);
        let bytes = std::fs::read(&f.src).map_err(|e| format!("{}: {e}", f.rel))?;
        field(digest::digest(&digest::SHA256, &bytes).as_ref());
    }
    Ok(Tree { fingerprint: hex(ctx.finish().as_ref()), files: files.len(), bytes: files.iter().map(|f| f.len).sum() })
}

/// The tree of `dir` as it is now, every file read.
pub fn tree(id: &str, dir: &Path) -> Result<Tree, String> {
    hash(id, &collect(dir)?)
}

struct Cached {
    stats: u64,
    fingerprint: Result<String, String>,
    checked: Instant,
}

static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);

/// The live folder's fingerprint, reused while no file's size or mtime moved
/// (and without looking for `RECHECK`). Only for "update available".
pub fn fingerprint_cached(id: &str, dir: &Path) -> Result<String, String> {
    let key = dir.join(id);
    if let Some((at, fp)) = CACHE.lock().ok().and_then(|c| c.as_ref().and_then(|m| m.get(&key).map(|c| (c.checked, c.fingerprint.clone())))) {
        if at.elapsed() < RECHECK {
            return fp;
        }
    }
    let files = collect(dir);
    let stats = {
        let mut h = DefaultHasher::new();
        match &files {
            Ok(f) => f.iter().for_each(|f| (&f.rel, f.len, f.mtime, f.exec).hash(&mut h)),
            Err(e) => e.hash(&mut h),
        }
        h.finish()
    };
    if let Ok(mut guard) = CACHE.lock() {
        if let Some(c) = guard.get_or_insert_with(HashMap::new).get_mut(&key).filter(|c| c.stats == stats) {
            c.checked = Instant::now();
            return c.fingerprint.clone();
        }
    }
    let fingerprint = files.and_then(|f| hash(id, &f).map(|t| t.fingerprint));
    if let Ok(mut guard) = CACHE.lock() {
        guard.get_or_insert_with(HashMap::new).insert(key, Cached { stats, fingerprint: fingerprint.clone(), checked: Instant::now() });
    }
    fingerprint
}

// ── snapshots ────────────────────────────────────────────────────────────────

static SNAPSHOT_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Set once at startup to `<app data dir>/plugin-snapshots`.
pub fn set_snapshot_root(dir: PathBuf) {
    let _ = SNAPSHOT_ROOT.set(dir);
}

pub fn snapshot_root() -> PathBuf {
    SNAPSHOT_ROOT.get().cloned().unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Library/Application Support/com.jamakase.agentarea-desktop/plugin-snapshots")
    })
}

/// Where the snapshot of `id` with `fingerprint` lives.
pub fn snapshot_dir(root: &Path, id: &str, fingerprint: &str) -> PathBuf {
    root.join(id).join(fingerprint)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> std::io::Result<()> {
    Ok(())
}

/// Folders writable again (to replace or remove a snapshot).
fn make_writable(dir: &Path) {
    let _ = set_mode(dir, 0o755);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().is_dir() && !e.path().symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
                make_writable(&e.path());
            }
        }
    }
}

pub fn remove_snapshot(dir: &Path) {
    make_writable(dir);
    let _ = std::fs::remove_dir_all(dir);
}

/// Copy the live folder into a fresh snapshot, hash the **copy**, and keep it
/// (read-only) as `<root>/<id>/<fingerprint>/`. A snapshot already there with
/// that fingerprint is reused after it is re-verified.
pub fn snapshot(id: &str, live: &Path, root: &Path) -> Result<(PathBuf, Tree), String> {
    let files = collect(live)?;
    let parent = root.join(id);
    std::fs::create_dir_all(&parent).map_err(|e| format!("snapshot folder: {e}"))?;
    let tmp = parent.join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
    let copied = (|| {
        for f in &files {
            let to = tmp.join(&f.rel);
            std::fs::create_dir_all(to.parent().unwrap_or(&tmp)).map_err(|e| format!("snapshot: {e}"))?;
            std::fs::copy(&f.src, &to).map_err(|e| format!("snapshot {}: {e}", f.rel))?;
            set_mode(&to, if f.exec { 0o555 } else { 0o444 }).map_err(|e| e.to_string())?;
        }
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        tree(id, &tmp)
    })();
    let tree = match copied {
        Ok(t) => t,
        Err(e) => {
            remove_snapshot(&tmp);
            return Err(e);
        }
    };
    let dest = snapshot_dir(root, id, &tree.fingerprint);
    if dest.exists() && verify(id, &dest, &tree.fingerprint).is_ok() {
        remove_snapshot(&tmp);
    } else {
        if dest.exists() {
            remove_snapshot(&dest);
        }
        std::fs::rename(&tmp, &dest).map_err(|e| format!("snapshot: {e}"))?;
        seal(&dest);
    }
    Ok((dest, tree))
}

/// Folders read-only (files already are).
fn seal(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                seal(&e.path());
            }
        }
    }
    let _ = set_mode(dir, 0o555);
}

/// The snapshot still is what was approved.
pub fn verify(id: &str, dir: &Path, fingerprint: &str) -> Result<(), String> {
    let t = tree(id, dir).map_err(|e| format!("approved copy unreadable ({e}); review and allow again"))?;
    if t.fingerprint == fingerprint {
        Ok(())
    } else {
        Err("the approved copy was modified; review and allow again".into())
    }
}

// ── the approval key and epochs ──────────────────────────────────────────────

const KEY_SERVICE: &str = "AgentArea Desktop Secrets";
/// Has dots, so it can't clash with a vault secret's id-named item.
const KEY_ACCOUNT: &str = "plugin-approvals.key.v1";

/// The Keychain item's content.
#[derive(Clone, Default, Deserialize, Serialize)]
struct Secret {
    /// hex HMAC key
    key: String,
    /// plugin id → current approval epoch
    #[serde(default)]
    epochs: BTreeMap<String, u64>,
}

static SECRET: Mutex<Option<Secret>> = Mutex::new(None);

#[cfg(not(test))]
fn read_item() -> Result<Option<String>, String> {
    match keyring::Entry::new(KEY_SERVICE, KEY_ACCOUNT).map_err(|e| format!("keychain: {e}"))?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keychain: {e}")),
    }
}

#[cfg(not(test))]
fn write_item(value: &str) -> Result<(), String> {
    keyring::Entry::new(KEY_SERVICE, KEY_ACCOUNT)
        .map_err(|e| format!("keychain: {e}"))?
        .set_password(value)
        .map_err(|e| format!("keychain: {e}"))
}

/// Tests never touch the Keychain: one in-memory item.
#[cfg(test)]
static TEST_ITEM: Mutex<Option<String>> = Mutex::new(None);

#[cfg(test)]
fn read_item() -> Result<Option<String>, String> {
    let _ = (KEY_SERVICE, KEY_ACCOUNT);
    Ok(TEST_ITEM.lock().unwrap().clone())
}

#[cfg(test)]
fn write_item(value: &str) -> Result<(), String> {
    *TEST_ITEM.lock().unwrap() = Some(value.to_string());
    Ok(())
}

/// Run `f` on the Keychain item (read once per launch, created when missing);
/// with `save`, the changed item is written before the cache is updated.
fn with_secret<T>(save: bool, f: impl FnOnce(&mut Secret) -> T) -> Result<T, String> {
    let mut cached = SECRET.lock().map_err(|_| "approval key lock poisoned")?;
    if cached.is_none() {
        let loaded = match read_item()? {
            // An item from an earlier build held just the hex key.
            Some(v) if v.len() == 64 && unhex(&v).is_some() => Secret { key: v, epochs: BTreeMap::new() },
            Some(v) => serde_json::from_str::<Secret>(&v).map_err(|_| "keychain: bad plugin approval item".to_string())?,
            None => {
                let mut k = vec![0u8; 32];
                ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut k).map_err(|_| "no randomness")?;
                let s = Secret { key: hex(&k), epochs: BTreeMap::new() };
                write_item(&serde_json::to_string(&s).map_err(|e| e.to_string())?)?;
                s
            }
        };
        *cached = Some(loaded);
    }
    let mut next = cached.clone().ok_or("no approval key")?;
    let out = f(&mut next);
    if save {
        write_item(&serde_json::to_string(&next).map_err(|e| e.to_string())?)?;
    }
    *cached = Some(next);
    Ok(out)
}

/// The current approval epoch for `id`.
pub fn epoch(id: &str) -> Result<u64, String> {
    with_secret(false, |s| s.epochs.get(id).copied().unwrap_or(0))
}

/// Invalidate every approval of `id` so far; returns the new epoch.
pub fn bump(id: &str) -> Result<u64, String> {
    with_secret(true, |s| {
        let e = s.epochs.entry(id.to_string()).or_insert(0);
        *e += 1;
        *e
    })
}

fn hmac_key() -> Result<hmac::Key, String> {
    let k = with_secret(false, |s| s.key.clone())?;
    Ok(hmac::Key::new(hmac::HMAC_SHA256, &unhex(&k).ok_or("bad approval key")?))
}

/// The MAC over `message` (domain-separated).
pub fn sign(message: &[u8]) -> Result<String, String> {
    let mut m = b"aa-plugin-approval-v2\0".to_vec();
    m.extend_from_slice(message);
    Ok(hex(hmac::sign(&hmac_key()?, &m).as_ref()))
}

/// Whether `mac` is ours for `message`. No key, no approval.
pub fn verify_mac(message: &[u8], mac: &str) -> bool {
    let (Ok(k), Some(tag)) = (hmac_key(), unhex(mac)) else { return false };
    let mut m = b"aa-plugin-approval-v2\0".to_vec();
    m.extend_from_slice(message);
    hmac::verify(&k, &m, &tag).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn temp() -> PathBuf {
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
        let t = tree("p", &dir).unwrap();
        assert_eq!((t.fingerprint.len(), t.files), (64, 3));
        assert_eq!(tree("p", &dir).unwrap(), t, "stable");
        assert_ne!(tree("q", &dir).unwrap().fingerprint, t.fingerprint, "bound to the id");

        std::fs::write(dir.join("lib/util.mjs"), "export {x}").unwrap();
        let edited = tree("p", &dir).unwrap().fingerprint;
        assert_ne!(edited, t.fingerprint, "nested file edited");
        std::fs::write(dir.join("new.js"), "").unwrap();
        assert_ne!(tree("p", &dir).unwrap().fingerprint, edited, "file added");
        std::fs::remove_file(dir.join("new.js")).unwrap();
        assert_eq!(tree("p", &dir).unwrap().fingerprint, edited);
        // Skipped entries.
        std::fs::write(dir.join(".DS_Store"), "x").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref").unwrap();
        assert_eq!(tree("p", &dir).unwrap().fingerprint, edited);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// NEW-2: a link counts as its target's content, wherever the target is
    /// in the folder (excluded entries included); links out are refused.
    #[test]
    fn symlinks_count_by_content() {
        let dir = temp();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/payload.mjs"), "benign()").unwrap();
        std::os::unix::fs::symlink(dir.join(".git/payload.mjs"), dir.join("entry.mjs")).unwrap();
        let before = tree("p", &dir).unwrap().fingerprint;
        std::fs::write(dir.join(".git/payload.mjs"), "evil()").unwrap();
        assert_ne!(tree("p", &dir).unwrap().fingerprint, before, "editing a link target under .git changes it");
        std::fs::write(dir.join(".DS_Store"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join(".DS_Store"), dir.join("lib/x.js")).unwrap();
        let with_ds = tree("p", &dir).unwrap().fingerprint;
        std::fs::write(dir.join(".DS_Store"), "y").unwrap();
        assert_ne!(tree("p", &dir).unwrap().fingerprint, with_ds, "a link to .DS_Store counts by content");

        // A link to a folder inside is walked; a cycle is refused.
        std::os::unix::fs::symlink(dir.join("lib"), dir.join("lib2")).unwrap();
        assert_eq!(tree("p", &dir).unwrap().files, 7, "lib2/ repeats lib/ (2 files)");
        std::os::unix::fs::symlink(dir.join("lib"), dir.join("lib/loop")).unwrap();
        assert!(tree("p", &dir).unwrap_err().contains("links back into itself"));
        std::fs::remove_file(dir.join("lib/loop")).unwrap();

        // Out of the folder: refused, file or folder.
        let outside = temp();
        std::os::unix::fs::symlink(outside.join("server.mjs"), dir.join("out.mjs")).unwrap();
        assert!(tree("p", &dir).unwrap_err().contains("links outside the plugin folder"));
        std::fs::remove_file(dir.join("out.mjs")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("outdir")).unwrap();
        assert!(tree("p", &dir).unwrap_err().contains("links outside the plugin folder"));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(outside);
    }

    /// NEW-1: the snapshot is a hashed, read-only copy without links; it keeps
    /// its fingerprint whatever happens to the live folder.
    #[test]
    fn snapshots_are_independent_copies() {
        let dir = temp();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/real.mjs"), "real()").unwrap();
        std::os::unix::fs::symlink(dir.join(".git/real.mjs"), dir.join("linked.mjs")).unwrap();
        let root = dir.with_extension("snaps");
        let (snap, t) = snapshot("p", &dir, &root).unwrap();
        assert_eq!(t, tree("p", &dir).unwrap(), "copy hashes like the live folder");
        assert_eq!(snap, snapshot_dir(&root, "p", &t.fingerprint));
        assert!(!snap.join("linked.mjs").symlink_metadata().unwrap().file_type().is_symlink(), "content copied, not the link");
        assert_eq!(std::fs::read_to_string(snap.join("linked.mjs")).unwrap(), "real()");
        assert!(!snap.join(".git").exists());
        assert!(std::fs::write(snap.join("server.mjs"), "x").is_err(), "read-only");
        assert!(std::fs::write(snap.join("new.txt"), "x").is_err(), "no new files either");

        std::fs::write(dir.join("server.mjs"), "changed").unwrap();
        assert!(verify("p", &snap, &t.fingerprint).is_ok(), "live edits don't touch the snapshot");
        // The same content again reuses the snapshot.
        std::fs::write(dir.join("server.mjs"), "console.log(1)").unwrap();
        assert_eq!(snapshot("p", &dir, &root).unwrap().0, snap);
        remove_snapshot(&root);
        assert!(!root.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cached_fingerprint_follows_edits() {
        let dir = temp();
        let fp = fingerprint_cached("p", &dir).unwrap();
        assert_eq!(fp, tree("p", &dir).unwrap().fingerprint);
        std::fs::write(dir.join("server.mjs"), "console.log('changed, and longer')").unwrap();
        std::thread::sleep(RECHECK + Duration::from_millis(100));
        assert_ne!(fingerprint_cached("p", &dir).unwrap(), fp, "picked up after RECHECK");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn macs_and_epochs() {
        let mac = sign(b"one").unwrap();
        assert!(verify_mac(b"one", &mac));
        assert!(!verify_mac(b"two", &mac));
        assert!(!verify_mac(b"one", &"0".repeat(64)), "forged");
        assert!(!verify_mac(b"one", "zz"), "malformed");
        let e = epoch("epoch-test").unwrap();
        assert_eq!(bump("epoch-test").unwrap(), e + 1);
        assert_eq!(epoch("epoch-test").unwrap(), e + 1);
    }
}
