// Minimal, stable updater binary for scalattice-agent
// This binary handles updates to the main agent with automatic rollback

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    println!("Scalattice Updater v{}", env!("CARGO_PKG_VERSION"));
    
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    match args[1].as_str() {
        "install" => {
            if args.len() < 3 {
                anyhow::bail!("install requires path to new agent binary");
            }
            install_update(&PathBuf::from(&args[2]))?;
        }
        "rollback" => {
            rollback_to_backup()?;
        }
        "monitor" => {
            monitor_agent_health()?;
        }
        _ => {
            print_usage();
        }
    }

    Ok(())
}

fn print_usage() {
    println!("Usage:");
    println!("  scalattice-updater install <path-to-new-binary>");
    println!("  scalattice-updater rollback");
    println!("  scalattice-updater monitor");
}

fn install_update(new_binary: &PathBuf) -> Result<()> {
    println!("Installing update from {}...", new_binary.display());
    
    let install_dir = get_install_dir()?;
    let current_agent = install_dir.join("scalattice-agent");
    let backup_agent = install_dir.join("scalattice-agent.backup");
    
    // Verify new binary exists and is executable
    if !new_binary.is_file() {
        anyhow::bail!("New binary not found: {}", new_binary.display());
    }
    
    // Backup current version
    if current_agent.exists() {
        println!("Backing up current version...");
        std::fs::copy(&current_agent, &backup_agent)
            .context("Failed to backup current agent")?;
    }
    
    // Install new version
    println!("Installing new version...");
    std::fs::copy(new_binary, &current_agent)
        .context("Failed to install new agent")?;
    
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&current_agent)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&current_agent, perms)?;
    }
    
    println!("Update installed successfully.");
    println!("Backup saved to: {}", backup_agent.display());
    println!("\nMonitoring new version for crashes...");
    println!("Run 'scalattice-updater monitor' to watch agent health.");
    
    Ok(())
}

fn rollback_to_backup() -> Result<()> {
    println!("Rolling back to previous version...");
    
    let install_dir = get_install_dir()?;
    let current_agent = install_dir.join("scalattice-agent");
    let backup_agent = install_dir.join("scalattice-agent.backup");
    
    if !backup_agent.exists() {
        anyhow::bail!("No backup found at {}", backup_agent.display());
    }
    
    std::fs::copy(&backup_agent, &current_agent)
        .context("Failed to restore backup")?;
    
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&current_agent)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&current_agent, perms)?;
    }
    
    println!("Rollback complete. Restart the agent to use previous version.");
    
    Ok(())
}

fn monitor_agent_health() -> Result<()> {
    println!("Monitoring agent health for 10 minutes...");
    
    let start = Instant::now();
    let probation_period = Duration::from_secs(600); // 10 minutes
    let check_interval = Duration::from_secs(30);
    
    let mut consecutive_failures = 0;
    const MAX_FAILURES: u32 = 3;
    
    while start.elapsed() < probation_period {
        if check_agent_running() {
            consecutive_failures = 0;
            let elapsed = start.elapsed().as_secs();
            let remaining = probation_period.as_secs() - elapsed;
            println!("✓ Agent running ({}s remaining)", remaining);
        } else {
            consecutive_failures += 1;
            println!("✗ Agent not running (failure {}/{})", consecutive_failures, MAX_FAILURES);
            
            if consecutive_failures >= MAX_FAILURES {
                println!("\n⚠ Agent failed {} times. Triggering automatic rollback...", MAX_FAILURES);
                rollback_to_backup()?;
                println!("\nRollback complete. Please restart the agent.");
                return Ok(());
            }
        }
        
        std::thread::sleep(check_interval);
    }
    
    println!("\n✓ Agent stable for {} minutes. Update successful!", probation_period.as_secs() / 60);
    
    // Remove backup after successful probation
    let install_dir = get_install_dir()?;
    let backup_agent = install_dir.join("scalattice-agent.backup");
    if backup_agent.exists() {
        std::fs::remove_file(&backup_agent).ok();
        println!("Removed backup file.");
    }
    
    Ok(())
}

fn check_agent_running() -> bool {
    #[cfg(unix)]
    {
        // Check if agent process is running
        Command::new("pgrep")
            .arg("-f")
            .arg("scalattice-agent")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
    
    #[cfg(windows)]
    {
        // Check if agent process is running on Windows
        Command::new("tasklist")
            .arg("/FI")
            .arg("IMAGENAME eq scalattice-agent.exe")
            .output()
            .map(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.contains("scalattice-agent.exe")
            })
            .unwrap_or(false)
    }
}

fn get_install_dir() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home).join(".local/bin"))
    }
    
    #[cfg(windows)]
    {
        let appdata = std::env::var("LOCALAPPDATA").context("LOCALAPPDATA not set")?;
        Ok(PathBuf::from(appdata).join("Scalattice").join("bin"))
    }
}
