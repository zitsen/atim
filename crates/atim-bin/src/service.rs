use std::process::Command;

use std::path::PathBuf;

pub enum ServiceCommand {
    Install,
    Uninstall,
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
        // --system: use sc.exe (Windows Service Control Manager, requires admin)
        // default:   use Task Scheduler (user-level, no admin)
        for cmd in cmds {
            match cmd {
                ServiceCommand::Install => {
                    if system_level {
                        install_service_sc()?
                    } else {
                        install_service_windows()?
                    }
                }
                ServiceCommand::Uninstall => {
                    // Stop first
                    if system_level {
                        if !is_elevated() {
                            println!("Requesting administrator privileges (UAC)...");
                            elevate_self(&["service", "--system", "--uninstall"])?;
                            return Ok(());
                        }
                        sc(&["stop", "atim"]).ok();
                    } else {
                        let _ = Command::new("taskkill")
                            .args(["/im", "atim.exe", "/f"])
                            .stdout(std::process::Stdio::null())
                            .status();
                    }
                    // Remove the task/service/startup script
                    if system_level {
                        sc(&["delete", "atim"])?;
                        println!("Service 'atim' removed.");
                    } else {
                        // Remove Startup folder script
                        if let Some(appdata) = std::env::var_os("APPDATA") {
                            let script_path = PathBuf::from(appdata)
                                .join("Microsoft")
                                .join("Windows")
                                .join("Start Menu")
                                .join("Programs")
                                .join("Startup")
                                .join("atim.cmd");
                            if script_path.exists() {
                                let _ = std::fs::remove_file(&script_path);
                                println!("Removed startup script {}", script_path.display());
                            }
                        }
                        // Remove schtasks task (if present from older installs)
                        let status = Command::new("schtasks")
                            .args(["/delete", "/tn", "Atim", "/f"])
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped())
                            .status()?;
                        if status.success() {
                            println!("Atim task removed.");
                        }
                        println!("Atim uninstalled.");
                    }
                }
                ServiceCommand::Enable => enable_service_windows()?,
                ServiceCommand::Start => {
                    if system_level {
                        if !is_elevated() {
                            println!("Requesting administrator privileges (UAC)...");
                            elevate_self(&["service", "--system", "--start"])?;
                            return Ok(());
                        }
                        sc(&["start", "atim"])?;
                    } else {
                        // Check if already running; if not, launch the process.
                        let running = is_atim_running();
                        if running {
                            println!("Atim is already running.");
                        } else {
                            let exe = std::env::current_exe()
                                .map_err(|_| "Could not determine binary path".to_string())?;
                            let _ = Command::new(exe).spawn();
                            println!("Atim started.");
                        }
                    }
                }
                ServiceCommand::Stop => {
                    if system_level {
                        if !is_elevated() {
                            println!("Requesting administrator privileges (UAC)...");
                            elevate_self(&["service", "--system", "--stop"])?;
                            return Ok(());
                        }
                        sc(&["stop", "atim"])?;
                    } else {
                        let _ = Command::new("taskkill")
                            .args(["/im", "atim.exe", "/f"])
                            .stdout(std::process::Stdio::null())
                            .status();
                        println!("Atim task stopped.");
                    }
                }
                ServiceCommand::Restart => {
                    if system_level {
                        if !is_elevated() {
                            println!("Requesting administrator privileges (UAC)...");
                            elevate_self(&["service", "--system", "--restart"])?;
                            return Ok(());
                        }
                        sc(&["stop", "atim"]).ok();
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                        sc(&["start", "atim"])?;
                    } else {
                        let _ = Command::new("taskkill")
                            .args(["/im", "atim.exe", "/f"])
                            .stdout(std::process::Stdio::null())
                            .status();
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                        let exe = std::env::current_exe()
                            .map_err(|_| "Could not determine binary path".to_string())?;
                        let _ = Command::new(exe).spawn();
                        println!("Atim restarted.");
                    }
                }
                ServiceCommand::Status => {
                    if system_level {
                        let _ = sc(&["query", "atim"]);
                    } else {
                        // Check startup registration + running state
                        let mut registered = false;
                        if let Some(appdata) = std::env::var_os("APPDATA") {
                            let script_path = PathBuf::from(appdata)
                                .join("Microsoft")
                                .join("Windows")
                                .join("Start Menu")
                                .join("Programs")
                                .join("Startup")
                                .join("atim.cmd");
                            registered = script_path.exists();
                        }
                        // Also check schtasks (older installs)
                        let output = Command::new("schtasks")
                            .args(["/query", "/tn", "Atim", "/fo", "list"])
                            .output()?;
                        let text = String::from_utf8_lossy(&output.stdout);
                        let task_registered = text.contains("Atim");
                        let running = is_atim_running();
                        println!(
                            "Registered: {}",
                            if registered || task_registered {
                                "yes"
                            } else {
                                "no"
                            }
                        );
                        println!(
                            "Status: {}",
                            if running { "running" } else { "not running" }
                        );
                        if !registered && !task_registered {
                            println!("Not installed. Run `atim service --install`.");
                        }
                    }
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
                ServiceCommand::Uninstall => {
                    // Stop first
                    let mut base = systemctl_base(system_level);
                    base.push("stop".to_string());
                    base.push(unit_name().to_string());
                    let _ = Command::new(&base[0]).args(&base[1..]).status();
                    // Remove unit file
                    let dir = service_dir(system_level);
                    let unit_path = dir.join(unit_name());
                    if unit_path.exists() {
                        std::fs::remove_file(&unit_path)?;
                        println!("Removed {}", unit_path.display());
                        // Reload
                        let mut reload = systemctl_base(system_level);
                        reload.push("daemon-reload".to_string());
                        let _ = Command::new(&reload[0]).args(&reload[1..]).status();
                    } else {
                        println!("Service unit not found at {}", unit_path.display());
                    }
                }
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

// ── Windows ──

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
    let home = home::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let log_path = format!("{home}\\.atim\\atim-service.log");

    // ── Path 1: Startup folder (most reliable, no admin, no Task Scheduler) ──
    // %APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\atim.cmd
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let startup_dir = PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup");
        if let Ok(()) = std::fs::create_dir_all(&startup_dir) {
            let script = format!(
                "@echo off\r\n\"{}\" >> \"{}\" 2>&1\r\n",
                binary_path.display(),
                log_path
            );
            let script_path = startup_dir.join("atim.cmd");
            if std::fs::write(&script_path, &script).is_ok() {
                println!("Atim added to Startup folder. Starts on login.");
                println!("Logs: {}", log_path);
                return Ok(());
            }
            eprintln!(
                "[debug] failed to write startup script {}",
                script_path.display()
            );
        }
    }

    // ── Path 2: simple schtasks /create (no XML → no encoding issues) ──
    let task_name = "Atim";
    // Remove any existing task first — an existing "Atim" task (e.g. created
    // by a prior elevated run) makes /create fail with "Access is denied".
    let _ = Command::new("schtasks")
        .args(["/delete", "/tn", task_name, "/f"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Quote the binary path for /tr. /sc onlogon runs at logon.
    let tr_arg = format!("\"{}\"", binary_path.display());
    eprintln!(
        "[debug] schtasks /create /tn {} /tr {} /sc onlogon /rl limited /f",
        task_name, tr_arg
    );
    let output = Command::new("schtasks")
        .args([
            "/create", "/tn", task_name, "/tr", &tr_arg, "/sc", "onlogon", "/rl", "limited", "/f",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        eprintln!("[debug] schtasks simple create stdout: {stdout}");
        println!("Scheduled task 'Atim' created. Starts on login.");
        return Ok(());
    }
    let simple_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    eprintln!("[debug] schtasks simple create failed: {simple_err}");

    // ── Path 3: schtasks /create /xml (UTF-16 LE BOM) ──
    // Wrap in cmd.exe /c with explicit redirect. NOTE: XML requires & to be
    // escaped as &amp; (the `2>&1` redirect contains &).
    let cmd_arg = format!(
        "/c \"\"{}\" >> \"{}\" 2&gt;&amp;1\"",
        binary_path.display(),
        log_path
    );
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Atim - AI Agent Through IM</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>cmd.exe</Command>
      <Arguments>{}</Arguments>
      <WorkingDirectory>{}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>"#,
        cmd_arg, home,
    );

    let tmp_xml = std::env::temp_dir().join("atim-task.xml");
    let mut utf16: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16 LE BOM
    for unit in xml.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&tmp_xml, &utf16)?;

    eprintln!(
        "[debug] schtasks /create /tn {task_name} /xml {} /f",
        tmp_xml.display()
    );
    let output = Command::new("schtasks")
        .args([
            "/create",
            "/tn",
            task_name,
            "/xml",
            tmp_xml.to_str().unwrap_or(""),
            "/f",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()?;

    let _ = std::fs::remove_file(&tmp_xml);

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        eprintln!("[debug] schtasks xml create stdout: {stdout}");
        println!("Scheduled task 'Atim' created. Starts on login, auto-restarts on failure.");
        println!("Logs: {}", log_path);
        return Ok(());
    }
    let xml_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    eprintln!("[debug] schtasks xml create failed: {xml_err}");

    // ── Path 4: sc.exe fallback (requires admin) ──
    eprintln!(
        "[debug] startup + schtasks failed (simple: '{simple_err}', xml: '{xml_err}'), falling back to sc.exe"
    );
    install_service_sc()
}

#[cfg(windows)]
fn install_service_sc() -> Result<(), Box<dyn std::error::Error>> {
    let binary_path =
        std::env::current_exe().map_err(|_| "Could not determine binary path".to_string())?;

    // If not elevated, re-launch ourselves with UAC to get admin rights.
    if !is_elevated() {
        println!("Requesting administrator privileges (UAC)...");
        elevate_self(&["service", "--system", "--install"])?;
        // The elevated process handles the install; report as pending.
        println!("Elevated process launched. Check the UAC prompt.");
        return Ok(());
    }

    // sc.exe requires "KEY= VALUE" as single args (space after = is part of the syntax).
    let bin_path_arg = format!("binPath= \"{}\"", binary_path.display());
    let start_arg = "start= auto".to_string();
    let display_arg = "displayname= Atim - AI Agent Through IM".to_string();
    eprintln!(
        "[debug] sc create atim {} {} {}",
        start_arg, bin_path_arg, display_arg
    );
    let output = Command::new("sc")
        .args(["create", "atim", &start_arg, &bin_path_arg, &display_arg])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        eprintln!("[debug] sc create stdout: {stdout}");
        eprintln!("[debug] sc create stderr: {stderr}");
        eprintln!("Failed to install service. This requires administrator privileges.");
        std::process::exit(1);
    }

    println!("Service 'atim' installed. Start with `atim service --start`.");
    Ok(())
}

#[cfg(windows)]
fn is_elevated() -> bool {
    // whoami /groups prints integrity level SID; S-1-16-12288 = high (elevated),
    // S-1-16-8192 = medium (not elevated).
    if let Ok(out) = Command::new("whoami").args(["/groups"]).output() {
        let text = String::from_utf8_lossy(&out.stdout);
        return text.contains("S-1-16-12288");
    }
    false
}

#[cfg(windows)]
fn is_atim_running() -> bool {
    if let Ok(out) = Command::new("tasklist")
        .args(["/fi", "imagename eq atim.exe", "/fo", "csv"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        return text.contains("atim.exe");
    }
    false
}

#[cfg(windows)]
fn elevate_self(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe().map_err(|_| "Could not determine binary path".to_string())?;
    let args_str = args.join(" ");
    // Start-Process -Verb RunAs triggers the UAC elevation prompt.
    let ps = format!(
        "Start-Process -FilePath '{}' -ArgumentList '{}' -Verb RunAs",
        exe.display(),
        args_str.replace('\'', "''"),
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .status()?;
    if !status.success() {
        return Err("UAC elevation was declined or failed".into());
    }
    Ok(())
}

#[cfg(windows)]
fn enable_service_windows() -> Result<(), Box<dyn std::error::Error>> {
    println!("Service 'atim' is already enabled (starts on login).");
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
