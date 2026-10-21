//! Uninstall as an ordered list of named steps, so the in-app button, the
//! Windows uninstaller and the CLI remove the same things in the same order.

use crate::claude::{Clients, ServerEntry};

/// One named piece of cleanup.
pub struct Step<'a> {
    name: String,
    action: Box<dyn FnOnce() -> anyhow::Result<()> + 'a>,
}

impl<'a> Step<'a> {
    pub fn new(name: impl Into<String>, action: impl FnOnce() -> anyhow::Result<()> + 'a) -> Self {
        Self {
            name: name.into(),
            action: Box::new(action),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The cleanup for one profile: Claude entries first, then the caller's login
/// item and secrets steps, then data only if the user asked for it.
pub fn uninstall_steps<'a>(
    clients: &'a Clients,
    entry: &'a ServerEntry,
    login_item: Step<'a>,
    secrets: Step<'a>,
    delete_data: Option<Step<'a>>,
) -> Vec<Step<'a>> {
    let unregister = clients.detect().into_iter().map(|client| {
        Step::new(format!("Remove from {client}"), move || {
            clients.unregister(client, entry).map(|_| ())
        })
    });
    unregister
        .chain([login_item, secrets])
        .chain(delete_data)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, path::PathBuf};

    use super::*;
    use crate::claude::{Platform, RunOutput, Runner};

    struct Succeeds;

    impl Runner for Succeeds {
        fn run(&self, _: &std::path::Path, _: &[String]) -> anyhow::Result<RunOutput> {
            Ok(RunOutput {
                success: true,
                ..RunOutput::default()
            })
        }
    }

    fn entry() -> ServerEntry {
        ServerEntry {
            name: "signal-personal".into(),
            command: "/bin/signal-agent".into(),
            args: vec!["mcp".into()],
        }
    }

    /// Claude entries go first so no client relaunches the agent mid-uninstall,
    /// and data is deleted only when the user ticked the box.
    #[test]
    fn uninstall_removes_entries_first_and_data_only_on_request() {
        let home = tempfile::tempdir().expect("temp home");
        std::fs::create_dir(home.path().join("Claude")).expect("Desktop dir");
        let clients = Clients::new(
            Platform::MacOs,
            Box::new(Succeeds),
            Some(PathBuf::from("claude")),
            Some(home.path().join("Claude")),
        );
        let entry = entry();
        for delete in [false, true] {
            let ran = RefCell::new(Vec::new());
            let step = |name: &'static str| {
                let ran = &ran;
                Step::new(name, move || {
                    ran.borrow_mut().push(name);
                    Ok(())
                })
            };
            let steps = uninstall_steps(
                &clients,
                &entry,
                step("login item"),
                step("secrets"),
                delete.then(|| step("data")),
            );
            let names: Vec<_> = steps.iter().map(|s| s.name().to_owned()).collect();
            let mut expected = vec![
                "Remove from Claude Code",
                "Remove from Claude Desktop",
                "login item",
                "secrets",
            ];
            if delete {
                expected.push("data");
            }
            assert_eq!(names, expected, "delete={delete}");
        }
    }
}
