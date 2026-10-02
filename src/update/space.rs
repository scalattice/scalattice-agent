//! Free disk space before downloading an agent self-update.
//!
//! Need is sized from the real release asset (download + extract/staging), not a
//! fixed ~4 GB guess. Machines that filled their disk with model weights otherwise
//! fail the update forever. We:
//!   1. clear old update temp dirs
//!   2. finish deleting any staged `.purging.` model dirs
//!   3. remove incomplete / failed weight downloads
//!   4. if still short, delete the largest complete model caches until enough
//!      free space exists (weights re-download after the new agent is up)

use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// When Cloud/GitHub omit asset size, keep a conservative floor (not the old 4 GB).
pub const UPDATE_FALLBACK_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Extra headroom for filesystem overhead / partial writes.
pub const UPDATE_SLACK_BYTES: u64 = 256 * 1024 * 1024;
/// Never ask for less than this even for tiny assets.
const UPDATE_MIN_NEED_BYTES: u64 = 512 * 1024 * 1024;

/// Free bytes required given the compressed download size of the platform asset.
///
/// Download stays on disk while we extract (tar.gz) or stage the installer, so
/// budget ≈ 3× compressed + slack. Falls back when size is unknown.
pub fn free_bytes_needed_for_asset(compressed_bytes: u64) -> u64 {
    if compressed_bytes == 0 {
        return UPDATE_FALLBACK_FREE_BYTES;
    }
    compressed_bytes
        .saturating_mul(3)
        .saturating_add(UPDATE_SLACK_BYTES)
        .max(UPDATE_MIN_NEED_BYTES)
}

pub async fn ensure_disk_for_update(need_bytes: u64) -> Result<()> {
    let need = if need_bytes == 0 {
        UPDATE_FALLBACK_FREE_BYTES
    } else {
        need_bytes.max(UPDATE_MIN_NEED_BYTES)
    };
    let before = crate::specs::disk_avail_bytes().unwrap_or(0);
    if before >= need {
        return Ok(());
    }
    info!(
        free_gb = format!("{:.2}", before as f64 / (1024.0 * 1024.0 * 1024.0)),
        need_gb = format!("{:.2}", need as f64 / (1024.0 * 1024.0 * 1024.0)),
        "low disk before agent update; freeing space"
    );

    clear_stale_update_temps();
    delete_staged_purge_dirs_sync();
    purge_incomplete_model_caches();

    let mut guard = 0u32;
    while crate::specs::disk_avail_bytes().unwrap_or(0) < need {
        guard += 1;
        if guard > 64 {
            break;
        }
        let Some(victim) = next_model_cache_victim() else {
            break;
        };
        let label = victim.display().to_string();
        let size = dir_size_bytes(&victim);
        match fs::remove_dir_all(&victim) {
            Ok(()) => {
                info!(
                    path = %label,
                    freed_gb = format!("{:.2}", size as f64 / (1024.0 * 1024.0 * 1024.0)),
                    "removed model cache to free space for agent update"
                );
                println!(
                    "Freed {:.1} GB of model weights so the update can download…",
                    size as f64 / (1024.0 * 1024.0 * 1024.0)
                );
            }
            Err(err) => {
                warn!(path = %label, error = %err, "failed removing model cache for update space");
                // Avoid spinning on the same path.
                break;
            }
        }
        // Give the filesystem a moment to settle free-space accounting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let after = crate::specs::disk_avail_bytes().unwrap_or(0);
    if after < need {
        let have = after as f64 / (1024.0 * 1024.0 * 1024.0);
        let need_gb = need as f64 / (1024.0 * 1024.0 * 1024.0);
        bail!(
            "disk_full: need about {need_gb:.1} GB free for the agent update (have {have:.1} GB). \
             Free disk space or remove models in the dashboard, then retry."
        );
    }
    info!(
        free_gb = format!("{:.2}", after as f64 / (1024.0 * 1024.0 * 1024.0)),
        need_gb = format!("{:.2}", need as f64 / (1024.0 * 1024.0 * 1024.0)),
        "disk space OK for agent update"
    );
    Ok(())
}

fn update_temp_roots() -> Vec<PathBuf> {
    let tmp = std::env::temp_dir();
    vec![
        tmp.join("scalattice").join("updates"),
        tmp.join("Scalattice").join("updates"),
    ]
}

fn clear_stale_update_temps() {
    for root in update_temp_roots() {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let _ = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
        }
        info!(path = %root.display(), "cleared stale agent update temp dir");
    }
}

fn delete_staged_purge_dirs_sync() {
    let models = crate::models::models_dir();
    let Ok(entries) = fs::read_dir(&models) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if crate::models::is_purging_cache_key(name) {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn purge_incomplete_model_caches() {
    let models = crate::models::models_dir();
    let Ok(entries) = fs::read_dir(&models) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if name == "hub" || name.starts_with('.') || crate::models::is_purging_cache_key(&name) {
            continue;
        }
        let runtime = name.replace("__", "/");
        if crate::models::model_weights_ready(&runtime) {
            continue;
        }
        // Drop partial downloads entirely — they cannot serve jobs.
        crate::models::purge_failed_download(&runtime);
    }

    // Incomplete HF image hub clones under models/hub.
    let hub = models.join("hub");
    let Ok(hub_entries) = fs::read_dir(&hub) else {
        return;
    };
    for entry in hub_entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Keep complete blob stores; remove empty / tiny incomplete clones.
        let size = dir_size_bytes(&path);
        if size > 32 * 1024 * 1024 {
            continue;
        }
        let _ = fs::remove_dir_all(&path);
    }
}

#[derive(Debug)]
struct CacheVictim {
    path: PathBuf,
    bytes: u64,
    complete: bool,
}

fn next_model_cache_victim() -> Option<PathBuf> {
    let mut victims = list_cache_victims();
    if victims.is_empty() {
        return None;
    }
    // Prefer incomplete / leftover first, then largest complete cache.
    victims.sort_by(|a, b| match (a.complete, b.complete) {
        (false, true) => std::cmp::Ordering::Less,
        (true, false) => std::cmp::Ordering::Greater,
        _ => b.bytes.cmp(&a.bytes),
    });
    victims.into_iter().next().map(|v| v.path)
}

fn list_cache_victims() -> Vec<CacheVictim> {
    let models = crate::models::models_dir();
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&models) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if name.starts_with('.') || crate::models::is_purging_cache_key(&name) {
            continue;
        }
        if name == "hub" {
            if let Ok(hub_entries) = fs::read_dir(&path) {
                for hub_entry in hub_entries.flatten() {
                    let hub_path = hub_entry.path();
                    if !hub_path.is_dir() {
                        continue;
                    }
                    let bytes = dir_size_bytes(&hub_path);
                    if bytes == 0 {
                        continue;
                    }
                    out.push(CacheVictim {
                        path: hub_path,
                        bytes,
                        complete: bytes > 512 * 1024 * 1024,
                    });
                }
            }
            continue;
        }
        let runtime = name.replace("__", "/");
        let complete = crate::models::model_weights_ready(&runtime);
        let bytes = dir_size_bytes(&path);
        if bytes == 0 {
            continue;
        }
        out.push(CacheVictim {
            path,
            bytes,
            complete,
        });
    }
    out
}

fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            total = total.saturating_add(dir_size_bytes(&path));
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn update_temp_roots_include_scalattice() {
        let roots = update_temp_roots();
        assert!(roots.iter().any(|p| p.ends_with("updates")));
    }

    #[test]
    fn free_bytes_scales_with_asset() {
        let need = free_bytes_needed_for_asset(400 * 1024 * 1024);
        assert!(need >= 400 * 1024 * 1024 * 3);
        assert!(need < UPDATE_FALLBACK_FREE_BYTES.saturating_mul(3));
        assert_eq!(
            free_bytes_needed_for_asset(0),
            UPDATE_FALLBACK_FREE_BYTES
        );
    }

    #[test]
    fn dir_size_counts_files() {
        let dir = std::env::temp_dir().join(format!(
            "slt-update-space-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut f = fs::File::create(dir.join("blob.bin")).unwrap();
        f.write_all(&[0u8; 4096]).unwrap();
        assert!(dir_size_bytes(&dir) >= 4096);
        let _ = fs::remove_dir_all(&dir);
    }
}
