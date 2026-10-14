//! SSH tunnels via the system `ssh` client, so ~/.ssh/config, the agent and known_hosts all apply.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::config::{SshAuth, SshConfig};

/// Environment variable carrying the secret to the askpass helper.
pub const ASKPASS_ENV: &str = "PUEO_ASKPASS_SECRET";

/// A running `ssh -L` forward. The ssh process is killed when this is dropped.
pub struct Tunnel {
    _child: std::sync::Mutex<Child>,
    pub local_port: u16,
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// A private copy of a key file, deleted when dropped.
struct KeyCopy(PathBuf);

impl Drop for KeyCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// OpenSSH refuses keys that other users can read ("UNPROTECTED PRIVATE KEY FILE"), which is
/// common for downloaded keys. Such a key
/// is copied into the user-only runtime dir with 0600 permissions; the original stays as is.
fn usable_key(path: &str) -> Result<(PathBuf, Option<KeyCopy>)> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = PathBuf::from(path);
    let meta = std::fs::metadata(&path).with_context(|| format!("Cannot read the SSH key {}", path.display()))?;
    if meta.permissions().mode() & 0o077 == 0 {
        return Ok((path, None));
    }
    let key = std::fs::read(&path).with_context(|| format!("Cannot read the SSH key {}", path.display()))?;
    let copy = crate::platform::runtime_dir()?.join(format!("pueo-key-{}", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&copy)?;
    let guard = KeyCopy(copy.clone());
    file.write_all(&key)?;
    Ok((copy, Some(guard)))
}

/// ssh's error output without the noise (host key notes, post-quantum advisories, banners).
fn ssh_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("Warning: Permanently added")
                && !l.starts_with("**")
                && !l.chars().all(|c| c == '@')
        })
        .collect();
    if lines.is_empty() { "no output".into() } else { lines.join("\n") }
}

/// A helper that ssh runs to obtain the password; it prints the secret passed in the environment.
fn askpass_script() -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = crate::platform::runtime_dir()?.join("pueo-askpass.sh");
    let content = format!("#!/bin/sh\nprintf '%s\\n' \"${ASKPASS_ENV}\"\n");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(content.as_str()) {
        std::fs::write(&path, &content)?;
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

pub async fn open(ssh: &SshConfig, secret: Option<String>, remote_host: &str, remote_port: u16) -> Result<Tunnel> {
    if ssh.host.trim().is_empty() {
        bail!("SSH host is empty");
    }
    let local_port = free_port()?;
    let remote_host = if remote_host.is_empty() { "localhost" } else { remote_host };
    let target = if ssh.user.is_empty() { ssh.host.clone() } else { format!("{}@{}", ssh.user, ssh.host) };

    let mut cmd = Command::new("ssh");
    cmd.arg("-N");
    // Extra `-o` options for debugging/tests; ssh uses the first value given, so these win.
    if let Ok(extra) = std::env::var("PUEO_SSH_OPTIONS") {
        for opt in extra.split_whitespace() {
            cmd.args(["-o", opt]);
        }
    }
    cmd
        .args(["-L", &format!("127.0.0.1:{local_port}:{remote_host}:{remote_port}")])
        .args(["-p", &ssh.port.to_string()])
        .args(["-o", "ExitOnForwardFailure=yes"])
        .args(["-o", "ServerAliveInterval=30"])
        .args(["-o", "ConnectTimeout=10"])
        .args(["-o", "StrictHostKeyChecking=accept-new"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if ssh.auth == SshAuth::Key {
        if ssh.key_path.trim().is_empty() {
            bail!("No SSH private key selected");
        }
    }
    // Held until the tunnel is up (or failed): ssh only reads the key while authenticating.
    let mut _key_copy = None;
    if ssh.auth == SshAuth::Key {
        let (key, copy) = usable_key(ssh.key_path.trim())?;
        _key_copy = copy;
        cmd.arg("-i").arg(key).args(["-o", "IdentitiesOnly=yes"]);
    }
    if ssh.auth == SshAuth::Password {
        cmd.args(["-o", "PubkeyAuthentication=no"])
            .args(["-o", "PreferredAuthentications=password,keyboard-interactive"])
            .args(["-o", "NumberOfPasswordPrompts=1"]);
    }
    let secret = secret.filter(|s| !s.is_empty());
    match (&secret, ssh.auth) {
        (Some(secret), SshAuth::Password | SshAuth::Key) => {
            cmd.env("SSH_ASKPASS", askpass_script()?)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(ASKPASS_ENV, secret);
        }
        _ => {
            if ssh.auth == SshAuth::Password {
                bail!("No SSH password saved for this connection");
            }
            cmd.args(["-o", "BatchMode=yes"]);
        }
    }
    cmd.arg(&target);

    let mut child = cmd.spawn().context("Could not start ssh (is openssh-client installed?)")?;

    // Wait until the forward is listening, or ssh gives up.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait()? {
            let mut err = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut err).await;
            }
            bail!("SSH tunnel failed ({status}): {}", ssh_error(&err));
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", local_port)).await.is_ok() {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            bail!("Timed out opening SSH tunnel to {target}");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Ok(Tunnel { _child: std::sync::Mutex::new(child), local_port })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_key_is_copied_privately() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("pueo-test-key-{}", std::process::id()));
        std::fs::write(&path, "KEY").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (used, copy) = usable_key(path.to_str().unwrap()).unwrap();
        assert_ne!(used, path);
        assert_eq!(std::fs::metadata(&used).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&used).unwrap(), "KEY");
        drop(copy);
        assert!(!used.exists(), "copy removed");
        // A private key is used where it is.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(usable_key(path.to_str().unwrap()).unwrap().0, path);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn error_noise_is_dropped() {
        let err = "Warning: Permanently added 'h' (ED25519) to the list of known hosts.\n\
                   ** WARNING: connection is not using a post-quantum key exchange algorithm.\n\
                   @@@@@@@@@@\n\
                   user@h: Permission denied (publickey).";
        assert_eq!(ssh_error(err), "user@h: Permission denied (publickey).");
    }
}
