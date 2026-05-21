use std::path::PathBuf;
use std::process::Command;

pub enum ServiceCommand {
    Install,
    Start,
    Stop,
    Restart,
    Status,
}

pub fn run_service(cmd: ServiceCommand, system_level: bool) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ServiceCommand::Install => install_service(system_level),
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

            if !status.success() {
                if let Some(code) = status.code() {
                    std::process::exit(code);
                }
            }
            Ok(())
        }
    }
}

fn unit_name() -> &'static str {
    "atim.service"
}

fn systemctl_base(system_level: bool) -> Vec<String> {
    let mut args = vec!["systemctl".to_string()];
    if !system_level {
        args.push("--user".to_string());
    }
    args
}

fn service_dir(system_level: bool) -> PathBuf {
    if system_level {
        PathBuf::from("/etc/systemd/system")
    } else if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("systemd").join("user")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config").join("systemd").join("user")
    } else {
        panic!("Cannot determine service directory (no HOME)");
    }
}

fn install_service(system_level: bool) -> Result<(), Box<dyn std::error::Error>> {
    let binary_path = std::env::current_exe()
        .map_err(|_| "Could not determine binary path".to_string())?;

    let wanted_by = if system_level {
        "multi-user.target"
    } else {
        "default.target"
    };

    let unit_content = format!(
        r#"[Unit]
Description=Atim - AI Agent Through IM
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={}
Restart=on-failure
RestartSec=5

[Install]
WantedBy={}
"#,
        binary_path.display(),
        wanted_by,
    );

    let dir = service_dir(system_level);
    std::fs::create_dir_all(&dir)?;

    let unit_path = dir.join(unit_name());
    std::fs::write(&unit_path, &unit_content)?;

    println!("Service unit installed at {:?}", unit_path);
    println!(
        "Run `atim service --{}start` to start the service",
        if system_level { "system --" } else { "" }
    );
    println!(
        "Run `systemctl {}enable atim.service` to enable on boot",
        if system_level { "" } else { "--user " }
    );

    Ok(())
}
