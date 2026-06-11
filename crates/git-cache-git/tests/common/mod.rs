//! Shared helpers for git-cache-git integration tests.

use std::process::Command;

/// Configures a hermetic git environment on a setup command: no user or
/// system config, no credential prompts, and no background maintenance
/// (detached auto-gc races with concurrent fetch/push in tests).
pub fn configure_git_env(command: &mut Command) {
    command
        .env_clear()
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "gc.auto")
        .env("GIT_CONFIG_VALUE_0", "0")
        .env("GIT_CONFIG_KEY_1", "gc.autoDetach")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("GIT_CONFIG_KEY_2", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_2", "false")
        .env("GIT_ASKPASS", "/bin/false")
        .env("SSH_ASKPASS", "/bin/false")
        .env("HOME", "/nonexistent");

    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
}
