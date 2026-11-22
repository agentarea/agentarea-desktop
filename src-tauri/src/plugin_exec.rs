//! Programs an "exec" plugin starts: only the argv its manifest declares under
//! `commands.<name>`, plus string arguments from the page. Output is kept per
//! run (last lines only) so the page can poll for progress and the result.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::plugins;

/// Lines of output kept per run.
const KEEP_LINES: usize = 200;
const MAX_ARGS: usize = 16;
const MAX_ARG_LEN: usize = 512;

#[derive(Default)]
struct Run {
    plugin: String,
    lines: VecDeque<String>,
    code: Option<i32>,
    done: bool,
}

static RUNS: Mutex<Option<HashMap<String, Arc<Mutex<Run>>>>> = Mutex::new(None);

#[derive(Serialize)]
pub struct RunStatus {
    running: bool,
    code: Option<i32>,
    lines: Vec<String>,
}

fn pump(run: Arc<Mutex<Run>>, out: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let mut r = run.lock().unwrap();
            if r.lines.len() == KEEP_LINES {
                r.lines.pop_front();
            }
            r.lines.push_back(line);
        }
    });
}

/// Start the plugin's declared command `name` with `args` appended; returns a run id.
#[tauri::command]
pub fn plugin_exec_start(id: String, name: String, args: Vec<String>) -> Result<String, String> {
    if args.len() > MAX_ARGS || args.iter().any(|a| a.len() > MAX_ARG_LEN || a.contains('\0')) {
        return Err("bad arguments".into());
    }
    let argv = plugins::plugin_command(&id, &name)?;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start {}: {e}", argv[0]))?;

    let run = Arc::new(Mutex::new(Run { plugin: id, ..Run::default() }));
    if let Some(out) = child.stdout.take() {
        pump(run.clone(), out);
    }
    if let Some(err) = child.stderr.take() {
        pump(run.clone(), err);
    }
    let waiter = run.clone();
    std::thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code());
        let mut r = waiter.lock().unwrap();
        r.code = code;
        r.done = true;
    });

    let run_id = uuid::Uuid::new_v4().to_string();
    let mut runs = RUNS.lock().unwrap();
    let runs = runs.get_or_insert_with(HashMap::new);
    // Forget finished runs nobody asked about for a while: keep the map small.
    if runs.len() > 64 {
        runs.retain(|_, r| !r.lock().unwrap().done);
    }
    runs.insert(run_id.clone(), run);
    Ok(run_id)
}

/// Progress of one of the plugin's runs: still running, exit code, last output lines.
#[tauri::command]
pub fn plugin_exec_status(id: String, run_id: String) -> Result<RunStatus, String> {
    let runs = RUNS.lock().unwrap();
    let run = runs.as_ref().and_then(|m| m.get(&run_id)).ok_or("unknown run")?;
    let r = run.lock().unwrap();
    if r.plugin != id {
        return Err("unknown run".into());
    }
    Ok(RunStatus { running: !r.done, code: r.code, lines: r.lines.iter().cloned().collect() })
}
