use std::collections::HashMap;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

/// Aim Hook — Claude Code SessionStart hook.
///
/// Claude Code invokes this script at the start of each session, piping a JSON
/// payload to stdin with `{ "session_id": "uuid" }`.
///
/// The hook:
///   1. Reads the JSON from stdin
///   2. Validates the session_id
///   3. Gets the current tmux window_id via `tmux display-message`
///   4. Writes the mapping to `session_map.json` atomically
///
/// Install with: `aim-hook --install`
fn main() {
    if std::env::args().any(|a| a == "--install" || a == "install") {
        install_hook();
        return;
    }

    if let Err(e) = run() {
        eprintln!("aim-hook error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read stdin
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let input = input.trim().to_string();

    if input.is_empty() {
        return Err("no input from stdin".into());
    }

    // 2. Parse JSON
    let payload: serde_json::Value = serde_json::from_str(&input)?;
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("missing session_id field")?;

    // Validate UUID format (basic)
    if !is_valid_uuid(session_id) {
        return Err(format!("invalid session_id format: {session_id}").into());
    }

    // 3. Get tmux window ID
    let window_id = get_tmux_window_id()?;

    // 4. Read existing session map
    let map_path = session_map_path();
    let mut map: HashMap<String, String> = if map_path.exists() {
        let data = std::fs::read_to_string(&map_path)?;
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        HashMap::new()
    };

    // 5. Update and write
    map.insert(window_id, session_id.to_string());
    atomic_write_json(&map_path, &map)?;

    Ok(())
}

fn get_tmux_window_id() -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{window_id}"])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // May not be running in tmux — that's ok for testing
        eprintln!("warning: not in tmux? {stderr}");
        return Ok("unknown".into());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn session_map_path() -> PathBuf {
    if let Ok(dir) = std::env::var("AIM_DIR") {
        PathBuf::from(dir).join("session_map.json")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".aim").join("session_map.json")
    }
}

fn is_valid_uuid(s: &str) -> bool {
    // Basic UUID format: 8-4-4-4-12 hex digits
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn atomic_write_json(path: &PathBuf, data: &HashMap<String, String>) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(data)?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("tmp");
    let file = std::fs::File::create(&tmp_path)?;
    // Exclusive flock for concurrent-safe writes with aim-server
    let fd = file.as_raw_fd();
    unsafe { libc::flock(fd, libc::LOCK_EX) };
    std::fs::write(&tmp_path, &json)?;
    file.sync_all()?;
    unsafe { libc::flock(fd, libc::LOCK_UN) };
    drop(file);

    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn install_hook() {
    let hook_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("claude")
        .join("hooks");

    let hook_path = hook_dir.join("SessionStart");
    let binary_path = std::env::current_exe().ok();

    if std::fs::create_dir_all(&hook_dir).is_err() {
        eprintln!("Could not create hook directory: {:?}", hook_dir);
        std::process::exit(1);
    }

    match &binary_path {
        Some(path) => {
            let script = format!("#!/bin/sh\nexec {} \"$@\"\n", path.display());
            if std::fs::write(&hook_path, &script).is_err() {
                eprintln!("Could not write hook script to {:?}", hook_path);
                std::process::exit(1);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755));
            }
            println!("Hook installed at {:?}", hook_path);
        }
        None => {
            eprintln!("Could not determine binary path");
            std::process::exit(1);
        }
    }
}

// Use minimal dirs-like approach to avoid extra dependency
mod dirs {
    use std::path::PathBuf;

    pub fn config_dir() -> Option<PathBuf> {
        std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config"))
            })
    }
}
