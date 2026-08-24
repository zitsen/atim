use std::process::Command;

use serde::Deserialize;

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

const REPO: &str = "zitsen/atim";

fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn target_triple() -> &'static str {
    if cfg!(target_os = "windows") {
        "x86_64-pc-windows-msvc"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-musl"
    } else {
        "x86_64-unknown-linux-musl"
    }
}

fn parse_version(tag: &str) -> Option<(u32, u32, u32)> {
    let v = tag.strip_prefix('v').unwrap_or(tag);
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

fn version_gt(a: &str, b: &str) -> bool {
    match (parse_version(a), parse_version(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

pub async fn run_update() -> Result<(), Box<dyn std::error::Error>> {
    let current = current_version();
    println!("Current version: v{current}");

    // Fetch latest release from GitHub
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let client = reqwest::Client::builder()
        .user_agent("atim-updater")
        .build()?;
    let resp = client.get(&url).send().await?;

    if !resp.status().is_success() {
        return Err(format!(
            "GitHub API returned {}: {}",
            resp.status(),
            resp.text().await?
        )
        .into());
    }

    let release: GithubRelease = resp.json().await?;
    let latest = &release.tag_name;
    println!("Latest version:  {latest}");

    if !version_gt(latest, &format!("v{current}")) {
        println!("Already up to date.");
        return Ok(());
    }

    // Find matching asset
    let triple = target_triple();
    let archive_name = format!("atim-{triple}.tar.gz");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == archive_name)
        .ok_or_else(|| {
            format!(
                "No asset found for {archive_name}. Available: {}",
                release
                    .assets
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    println!("Downloading {}...", asset.name);

    // Download to a temp file
    let tmp_dir = std::env::temp_dir();
    let archive_path = tmp_dir.join(&asset.name);
    let resp = client.get(&asset.browser_download_url).send().await?;
    if !resp.status().is_success() {
        return Err(format!("Download failed: HTTP {}", resp.status()).into());
    }
    let bytes = resp.bytes().await?;
    std::fs::write(&archive_path, &bytes)?;
    println!(
        "Downloaded {} bytes to {}",
        bytes.len(),
        archive_path.display()
    );

    // Extract binary
    let extract_dir = tmp_dir.join("atim-update");
    let _ = std::fs::remove_dir_all(&extract_dir);
    std::fs::create_dir_all(&extract_dir)?;

    let status = Command::new("tar")
        .args([
            "xzf",
            archive_path.to_str().unwrap(),
            "-C",
            extract_dir.to_str().unwrap(),
        ])
        .status()?;
    if !status.success() {
        return Err("Failed to extract archive".into());
    }

    let new_binary = extract_dir.join("atim");
    if !new_binary.exists() {
        return Err("Extracted archive does not contain 'atim' binary".into());
    }

    // Determine install path
    let install_path = std::env::current_exe()?;

    // Stop service
    println!("Stopping service...");
    let systemctl = if cfg!(target_os = "windows") {
        // Windows: kill process
        let _ = Command::new("taskkill")
            .args(["/im", "atim.exe", "/f"])
            .status();
        None
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", "atim.service"])
            .status();
        Some("systemctl")
    };

    // Replace binary
    println!("Installing to {}...", install_path.display());

    // On Linux, use cp + chmod to avoid issues with rename across filesystems
    let cp_status = Command::new("cp")
        .args([new_binary.to_str().unwrap(), install_path.to_str().unwrap()])
        .status()?;
    if !cp_status.success() {
        // Try direct copy as fallback
        std::fs::copy(&new_binary, &install_path)?;
    }

    // Make executable (Linux/macOS)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&install_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&install_path, perms)?;
    }

    // Restart service
    if systemctl.is_some() {
        println!("Restarting service...");
        let _ = Command::new("systemctl")
            .args(["--user", "start", "atim.service"])
            .status();
    }

    // Cleanup
    let _ = std::fs::remove_file(&archive_path);
    let _ = std::fs::remove_dir_all(&extract_dir);

    println!("Updated to {latest} successfully!");

    // Verify
    let output = Command::new(&install_path).args(["--version"]).output();
    if let Ok(out) = output {
        let ver = String::from_utf8_lossy(&out.stdout);
        println!("Verified: {}", ver.trim());
    }

    Ok(())
}
