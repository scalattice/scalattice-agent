# Scalattice Agent Updater

## Overview

The `scalattice-updater` is a separate, minimal binary that handles updates to the main `scalattice-agent` with automatic rollback protection. This architecture prevents failed updates from bricking the agent.

## Architecture

```
┌─────────────────────┐
│ scalattice-updater  │  ← Minimal, stable, independent
│  (watchdog/updater) │     Rarely needs updates itself
└──────────┬──────────┘
           │ manages
           ↓
┌─────────────────────┐
│ scalattice-agent    │  ← Main application
│                     │     Can be safely updated
└─────────────────────┘
```

## How It Works

### Update Flow

1. **Download**: Agent downloads new version to staging area
2. **Backup**: Updater creates backup of current agent binary
3. **Install**: Updater installs new version atomically
4. **Monitor**: Updater watches new version for 10 minutes
5. **Rollback**: If crashes detected, automatic rollback to backup
6. **Commit**: After successful probation, backup is removed

### Files

```
~/.local/bin/
├── scalattice-agent        # Main binary (updated frequently)
├── scalattice-updater      # Updater binary (stable)
└── scalattice-agent.backup # Automatic backup (temporary)
```

## Usage

### Automatic (Recommended)

Updates are handled automatically by the agent. The updater runs transparently:

```bash
scalattice-agent update
```

The agent will:
1. Download the new version
2. Delegate installation to the updater
3. Restart with the new version
4. Monitor for crashes (automatic rollback if needed)

### Manual Operations

Check if updater is installed:
```bash
which scalattice-updater
scalattice-updater --version
```

Manual rollback to previous version:
```bash
scalattice-updater rollback
```

Monitor agent health after update:
```bash
scalattice-updater monitor
```

Install a specific binary (advanced):
```bash
scalattice-updater install /path/to/new-agent-binary
```

## Rollback Protection

### Automatic Rollback Triggers

- Agent process crashes 3+ times within 10 minutes
- Agent fails to start after update

### Probation Period

New versions run in "probation" for 10 minutes after installation:
- ✓ No crashes → Update committed (backup removed)
- ✗ Crashes detected → Automatic rollback

### Manual Rollback

If issues occur after probation period:

```bash
# Stop the agent
sudo systemctl stop scalattice-agent

# Rollback to previous version
scalattice-updater rollback

# Start the agent
sudo systemctl start scalattice-agent
```

## Benefits

### For Users
- **Never Brick**: Bad updates don't break the system
- **Automatic Recovery**: Crashes trigger automatic rollback
- **No Downtime**: Updates are atomic (instant swap)
- **Safety Net**: Can manually rollback if needed

### For Developers
- **Safe Deployments**: Can push updates with confidence
- **Fast Rollback**: Issues are detected and fixed automatically
- **Separation**: Update logic isolated from main application
- **Testing**: Probation period catches crashes early

## Implementation Details

### Why Separate Binary?

The updater is separate because:
1. **Stability**: Rarely needs updates itself (simple, stable code)
2. **Independence**: Can fix broken agent without working agent
3. **Reliability**: Self-updating is risky (can't fix itself if broken)
4. **Clean**: Update logic separate from application logic

### Updater Updates

The updater itself can be updated (though rare):
1. New updater included in agent release archive
2. Agent installs new updater during its update
3. Updater doesn't update itself (that would be risky)

### Fallback Behavior

If updater is not present:
- Agent falls back to direct self-update (old behavior)
- No rollback protection
- Less safe but still functional

This ensures compatibility with existing installations.

## Building

Both binaries are built from the same repository:

```bash
# Build both binaries
cargo build --release

# Outputs:
# - target/release/scalattice-agent
# - target/release/scalattice-updater
```

Release archives include both binaries:
```
scalattice-agent-x86_64-unknown-linux-gnu.tar.gz
├── scalattice-agent      # Main binary
├── scalattice-updater    # Updater binary
└── lib/                  # Shared libraries
```

## Technical Notes

### Process Monitoring

Linux: Uses `pgrep` to detect if agent is running  
Windows: Uses `tasklist` to detect if agent is running

### File Operations

- Atomic renames (no partial installs)
- Permission preservation (executable bits)
- Cleanup of staging files

### Edge Cases Handled

- No backup exists (skip rollback)
- Updater not present (fallback to direct update)
- Multiple consecutive failures (rollback threshold)
- Probation timeout (commit successful update)

## Future Enhancements

Potential improvements:
- Health check via HTTP endpoint (more reliable than process check)
- Configurable probation period
- Multiple backup versions (rollback to any previous version)
- Diff-based updates (download only changed files)
- Update scheduling (install at low-usage times)
