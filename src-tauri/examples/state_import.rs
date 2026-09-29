//! Import the legacy Channels files into ~/AgentArea/state.db and write the
//! export snapshots. Same code path as the app's first start.
//!
//!     cargo run --example state_import [-- --keep]   # --keep: don't move files to _legacy/
use agentarea_desktop_lib::state_db;
use serde_json::json;

fn main() {
    let keep = std::env::args().any(|a| a == "--keep");
    let home = std::env::var("HOME").expect("HOME");
    let root = std::path::PathBuf::from(home).join("AgentArea");
    let db = state_db::StateDb::open(&root).unwrap_or_else(|e| panic!("open: {e}"));
    let report = db
        .import_dir("import", &db.channels_dir(), !keep)
        .unwrap_or_else(|e| panic!("import: {e}"));
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    db.export_now().unwrap_or_else(|e| panic!("export: {e}"));
    let summary = db.call("import", "summary", &json!({})).unwrap();
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}
