//! Passwords stored in the desktop keyring (GNOME Keyring / KWallet via Secret Service).

use std::time::Duration;

use anyhow::{Context, Result};

const APP: &str = "pueo";
/// Passwords saved before the app was renamed are moved over on first use.
const LEGACY_APP: &str = "tabula";

/// How long to wait for the keyring (including a possible unlock prompt).
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The database password.
    Database,
    /// The SSH password, or the passphrase of the SSH key.
    Ssh,
}

fn item_id(id: &str, slot: Slot) -> String {
    match slot {
        Slot::Database => id.to_string(),
        Slot::Ssh => format!("{id}:ssh"),
    }
}

fn attributes(id: &str) -> [(&'static str, &str); 2] {
    [("application", APP), ("connection-id", id)]
}

fn legacy_attributes(id: &str) -> [(&'static str, &str); 2] {
    [("application", LEGACY_APP), ("connection-id", id)]
}

pub async fn get_password(id: &str, slot: Slot) -> Result<Option<String>> {
    let id = item_id(id, slot);
    tokio::time::timeout(TIMEOUT, lookup(&id)).await.context("Timed out waiting for the keyring")?
}

async fn lookup(id: &str) -> Result<Option<String>> {
    let keyring = oo7::Keyring::new().await?;
    keyring.unlock().await?;
    if let Some(item) = keyring.search_items(&attributes(id)).await?.first() {
        let secret = item.secret().await?;
        return Ok(Some(String::from_utf8_lossy(&secret).into_owned()));
    }
    let Some(item) = keyring.search_items(&legacy_attributes(id)).await?.into_iter().next() else {
        return Ok(None);
    };
    let secret = String::from_utf8_lossy(&item.secret().await?).into_owned();
    // Re-save under the new name; the old item goes only once the copy exists.
    let label = item.label().await?.replacen("Tabula", "Pueo", 1);
    if keyring.create_item(&label, &attributes(id), secret.as_str(), true).await.is_ok() {
        let _ = keyring.delete(&legacy_attributes(id)).await;
    }
    Ok(Some(secret))
}

pub async fn set_password(id: &str, slot: Slot, label: &str, password: &str) -> Result<()> {
    let id = item_id(id, slot);
    let keyring = oo7::Keyring::new().await?;
    keyring.unlock().await?;
    // Whatever is set now replaces a password left under the old name.
    keyring.delete(&legacy_attributes(&id)).await?;
    if password.is_empty() {
        keyring.delete(&attributes(&id)).await?;
    } else {
        let label = match slot {
            Slot::Database => format!("Pueo: {label}"),
            Slot::Ssh => format!("Pueo SSH: {label}"),
        };
        keyring.create_item(&label, &attributes(&id), password, true).await?;
    }
    Ok(())
}

pub async fn delete_passwords(id: &str) -> Result<()> {
    let keyring = oo7::Keyring::new().await?;
    for slot in [Slot::Database, Slot::Ssh] {
        keyring.delete(&attributes(&item_id(id, slot))).await?;
        keyring.delete(&legacy_attributes(&item_id(id, slot))).await?;
    }
    Ok(())
}
