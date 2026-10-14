//! Native credentials, retaining the original Secret Service format on Linux.
#[cfg(target_os = "linux")]
#[path = "secrets_linux.rs"]
mod native;
#[cfg(target_os = "linux")]
pub use native::*;

#[cfg(target_os = "macos")]
mod native {
    use anyhow::Result;
    use security_framework::passwords::{
        PasswordOptions, delete_generic_password, generic_password, set_generic_password,
    };
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Slot {
        Database,
        Ssh,
    }
    fn account(id: &str, slot: Slot) -> String {
        match slot {
            Slot::Database => id.into(),
            Slot::Ssh => format!("{id}:ssh"),
        }
    }
    fn missing(error: &security_framework::base::Error) -> bool {
        error.code() == -25300
    }
    fn remove(service: &str, id: &str) -> Result<()> {
        match delete_generic_password(service, id) {
            Ok(()) => Ok(()),
            Err(e) if missing(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub async fn get_password(id: &str, slot: Slot) -> Result<Option<String>> {
        let id = account(id, slot);
        tokio::task::spawn_blocking(move || {
            for service in ["pueo", "tabula"] {
                match generic_password(PasswordOptions::new_generic_password(service, &id)) {
                    Ok(secret) => {
                        if service == "tabula" {
                            set_generic_password("pueo", &id, &secret)?;
                            remove("tabula", &id)?;
                        }
                        return Ok(Some(String::from_utf8(secret)?));
                    }
                    Err(e) if missing(&e) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(None)
        })
        .await?
    }
    pub async fn set_password(id: &str, slot: Slot, _label: &str, password: &str) -> Result<()> {
        let (id, password) = (account(id, slot), password.to_owned());
        tokio::task::spawn_blocking(move || {
            if password.is_empty() {
                remove("pueo", &id)?;
            } else {
                set_generic_password("pueo", &id, password.as_bytes())?;
            }
            remove("tabula", &id)
        })
        .await?
    }
    pub async fn delete_passwords(id: &str) -> Result<()> {
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            for slot in [Slot::Database, Slot::Ssh] {
                for service in ["pueo", "tabula"] {
                    remove(service, &account(&id, slot))?;
                }
            }
            Ok(())
        })
        .await?
    }
}
#[cfg(target_os = "macos")]
pub use native::*;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Pueo currently supports Linux and macOS");
