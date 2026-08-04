use std::process::Command;

#[cfg(not(windows))]
use std::path::PathBuf;

pub enum ServiceCommand {
    Install,
    Enable,
    Start,
    Stop,
    Restart,
    Status,
}

pub fn run_service(
    cmds: &[ServiceCommand],
    system_level: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    {
        let _ = system_level; // system-level is N/A on Windows (sc.exe is always system)
        for cmd in cmds {
            match cmd {
                ServiceCommand::Install => install_service_windows()?,
                ServiceCommand::Enable => enable_service_windows()?,
                ServiceCommand::Start => sc(&["start", "atim"])?,
                ServiceCommand::Stop => sc(&["stop", "atim"])?,
                ServiceCommand::Restart => {
                    sc(&["stop", "atim"]).ok();
                    sc(&["start", "atim"])?;
                }
                ServiceCommand::Status => {
                    let _ = sc(&["query", "atim"]);
                }
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        for cmd in cmds {
            match cmd {
                ServiceCommand::Install => install_service(system_level)?,
                ServiceCommand::Enable => enable_service(system_level)?,
                _ => {
                    let mut base = systemctl_base(system_level);
                    let action = match cmd {
                        ServiceCommand::Start => "start",
                        ServiceCommand::Stop => "stop",
                        ServiceCommand::Restart => "restart",
                        ServiceCommand::Status => "status",
                        _ => unreachable!(),
                    };
                    base.push(action.to_string());
                    base.push(unit_name().to_string());

                    let status = Command::new(&base[0])
                        .args(&base[1..])
                        .stdin(std::process::Stdio::inherit())
                        .stdout(std::process::Stdio::inherit())
                        .stderr(std::process::Stdio::inherit())
                        .status()?;

                    if !status.success()
                        && let Some(code) = status.code()
                    {
                        std::process::exit(code);
                    }
                }
            }
        }
        Ok(())
    }
}

// ── Windows (sc.exe) ──

#[cfg(windows)]
fn sc(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("sc")
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;
    if !status.success()
        && let Some(code) = status.code()
    {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(windows)]
fn install_service_windows() -> Result<(), Box<dyn std::error::Error>> {
    let binary_path =
        std::env::current_exe().map_err(|_| "Could not determine binary path".to_string())?;

    // Register as a Windows service. `sc create` needs `binPath=` with a
    // space before the value: `binPath= "C:\...\atim.exe"`.
    let bin_path_arg = format!("binPath= \"{}\"", binary_path.display());
    let status = Command::new("sc")
        .args([
            "create",
            "atim",
            "start=",
            "auto",
            &bin_path_arg,
            "displayname=",
            "Atim - AI Agent Through IM",
        ])
        .status()?;
    if !status.success()
        && let Some(code) = status.code()
    {
        std::process::exit(code);
    }

    println!("Service 'atim' installed. Start with `atim service --start`.");
    Ok(())
}

#[cfg(windows)]
fn enable_service_windows() -> Result<(), Box<dyn std::error::Error>> {
    // sc create with start= auto already enables on boot.
    println!("Service 'atim' is already enabled (start= auto).");
    Ok(())
}

// ── Linux/macOS (systemd) ──

#[cfg(not(windows))]
fn enable_service(system_level: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut daemon_reload_args = systemctl_base(system_level);
    daemon_reload_args.push("daemon-reload".to_string());
    let reload = Command::new(&daemon_reload_args[0])
        .args(&daemon_reload_args[1..])
        .status()?;
    if !reload.success() {
        eprintln!("Warning: systemctl daemon-reload failed");
    }

    let mut enable_args = systemctl_base(system_level);
    enable_args.push("enable".to_string());
    enable_args.push(unit_name().to_string());
    let status = Command::new(&enable_args[0])
        .args(&enable_args[1..])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;

    if !status.success()
        && let Some(code) = status.code()
    {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(not(windows))]
fn unit_name() -> &'static str {
    "atim.service"
}

#[cfg(not(windows))]
fn systemctl_base(system_level: bool) -> Vec<String> {
    let mut args = vec!["systemctl".to_string()];
    if !system_level {
        args.push("--user".to_string());
    }
    args
}

#[cfg(not(windows))]
fn service_dir(system_level: bool) -> PathBuf {
    if system_level {
        PathBuf::from("/etc/systemd/system")
    } else if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("systemd").join("user")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".config")
            .join("systemd")
            .join("user")
    } else {
        panic!("Cannot determine service directory (no HOME)");
    }
}

#[cfg(not(windows))]
fn install_service(system_level: bool) -> Result<(), Box<dyn std::error::Error>> {
    let binary_path =
        std::env::current_exe().map_err(|_| "Could not determine binary path".to_string())?;

    let wanted_by = if system_level {
        "multi-user.target"
    } else {
        "default.target"
    };

    // Capture the current PATH so the systemd service inherits it
    let current_path = std::env::var("PATH").unwrap_or_default();
    let mut extra_env = String::new();
    extra_env.push_str(&format!("Environment=PATH={current_path}\n"));

    let unit_content = format!(
        r#"[Unit]
Description=Atim - AI Agent Through IM
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
KillMode=process
ExecStart={}
Restart=on-failure
RestartSec=5
EnvironmentFile=-~/.atim/.env
{}
[Install]
WantedBy={}
"#,
        binary_path.display(),
        extra_env,
        wanted_by,
    );

    let dir = service_dir(system_level);
    std::fs::create_dir_all(&dir)?;

    let unit_path = dir.join(unit_name());
    std::fs::write(&unit_path, &unit_content)?;

    // Reload systemd so the new/updated unit is picked up
    let mut daemon_reload_args = systemctl_base(system_level);
    daemon_reload_args.push("daemon-reload".to_string());
    let daemon_reload = Command::new(&daemon_reload_args[0])
        .args(&daemon_reload_args[1..])
        .status()?;
    if !daemon_reload.success() {
        eprintln!("Warning: systemctl daemon-reload failed");
    }

    println!("Service unit installed at {:?}", unit_path);
    println!(
        "Run `atim service --{}start` to start the service",
        if system_level { "system --" } else { "" },
    );
    println!(
        "Run `systemctl {}enable atim.service` to enable on boot",
        if system_level { "" } else { "--user " },
    );

    Ok(())
}
