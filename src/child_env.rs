//! Environment for spawned MCP/LSP children.
//!
//! A child inherits every secret in the agent's environment by default
//! (`OPENAI_API_KEY`, cloud tokens, SSH agents). Third-party servers do not
//! need those, so only a documented allowlist crosses the process boundary;
//! per-server `env` entries are applied after this base.

use tokio::process::Command;

const EXACT: &[&str] = &[
    // Process basics.
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "COLORTERM",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LANGUAGE",
    "TZ",
    "XDG_CONFIG_HOME",
    "XDG_CONFIG_DIRS",
    "XDG_DATA_HOME",
    "XDG_DATA_DIRS",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    // Toolchains; these point at install locations, not credentials.
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "GOROOT",
    "GOPATH",
    "GOMODCACHE",
    "GOFLAGS",
    "JAVA_HOME",
    "NODE_PATH",
    "NVM_DIR",
    "NVM_BIN",
    "PYENV_ROOT",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "DOTNET_ROOT",
    "MAVEN_HOME",
    "GRADLE_HOME",
    "SDKROOT",
    // TLS trust stores.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
    // Windows basics.
    "PATHEXT",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
];

const PREFIXES: &[&str] = &["LC_"];

fn allowed(name: &str) -> bool {
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Replace the child's inherited environment with the allowlisted subset.
/// Call before applying per-server overrides.
pub fn scrub(cmd: &mut Command) {
    cmd.env_clear();
    for (key, value) in std::env::vars_os() {
        if allowed(&key.to_string_lossy()) {
            cmd.env(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_do_not_cross_but_toolchains_do() {
        assert!(allowed("PATH"));
        assert!(allowed("HOME"));
        assert!(allowed("LC_ALL"));
        assert!(allowed("XDG_CONFIG_HOME"));
        assert!(!allowed("XDG_RUNTIME_DIR"));
        assert!(!allowed("OPENAI_API_KEY"));
        assert!(!allowed("AWS_SECRET_ACCESS_KEY"));
        assert!(!allowed("SSH_AUTH_SOCK"));
        assert!(!allowed("RUSTC_WRAPPER"));
    }
}
