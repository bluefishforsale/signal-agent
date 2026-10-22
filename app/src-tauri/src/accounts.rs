//! The accounts on this computer as the Advanced list shows them, so a user
//! can tell which Claude entry is which account and remove one.

use relay_setup::Profiles;
use serde::Serialize;

/// One account in the list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Account {
    pub name: String,
    pub claude_name: String,
    pub set_up: bool,
}

/// Every account in `profiles`, in the order they were added.
pub fn list(profiles: &Profiles) -> anyhow::Result<Vec<Account>> {
    Ok(profiles
        .list()?
        .into_iter()
        .map(|profile| Account {
            claude_name: profile.mcp_server_name(),
            set_up: profiles.paths(&profile).load_access().is_ok(),
            name: profile.to_string(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use relay_core::prelude::*;

    use super::*;

    /// Setup lists a profile as soon as it starts, so an account whose setup
    /// was abandoned must show as unfinished rather than as working.
    #[test]
    fn the_list_names_each_accounts_claude_entry_and_whether_setup_finished() {
        let base = tempfile::tempdir().expect("temp data dir");
        let profiles = Profiles::under(base.path().into());
        let personal = profiles.ensure_default().expect("default profile");
        profiles.create("bot").expect("create bot");
        profiles
            .paths(&personal)
            .save_access(AccessMode::Full)
            .expect("finish personal");

        let account = |name: &str, set_up| Account {
            name: name.into(),
            claude_name: format!("signal-{name}"),
            set_up,
        };
        assert_eq!(
            list(&profiles).expect("list"),
            [account("personal", true), account("bot", false)]
        );
    }
}
