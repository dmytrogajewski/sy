//! Session actions exposed through one menu and a scriptable CLI.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::Write;
use std::process::{Command, Stdio};

fn pick(input: &str, prompt: &str) -> Result<String> {
    let mut child = Command::new("fuzzel")
        .args(["--dmenu", &format!("--prompt={prompt}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawn fuzzel")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())?;
    let output = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Lock,
    Logout,
    Suspend,
    Reboot,
    Poweroff,
}

impl Action {
    const ALL: [Self; 5] = [
        Self::Lock,
        Self::Logout,
        Self::Suspend,
        Self::Reboot,
        Self::Poweroff,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::Logout => "logout",
            Self::Suspend => "suspend",
            Self::Reboot => "reboot",
            Self::Poweroff => "poweroff",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Lock => "Lock",
            Self::Logout => "Log out",
            Self::Suspend => "Suspend",
            Self::Reboot => "Reboot",
            Self::Poweroff => "Power off",
        }
    }

    fn command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Lock => ("swaylock", &["-f"]),
            Self::Logout => ("niri", &["msg", "action", "quit", "--skip-confirmation"]),
            Self::Suspend => ("systemctl", &["suspend"]),
            Self::Reboot => ("systemctl", &["reboot"]),
            Self::Poweroff => ("systemctl", &["poweroff"]),
        }
    }
}

fn parse(name: &str) -> Result<Action> {
    Action::ALL
        .into_iter()
        .find(|a| a.name() == name)
        .ok_or_else(|| {
            anyhow::anyhow!("unknown power action {name:?} (lock|logout|suspend|reboot|poweroff)")
        })
}

pub fn run(action: Option<&str>, dry_run: bool, yes: bool, json_output: bool) -> Result<()> {
    let action = match action {
        Some(name) => parse(name)?,
        None => {
            let rows = Action::ALL.map(Action::label);
            let picked = pick(&rows.join("\n"), "power » ")?;
            let Some(action) = Action::ALL.into_iter().find(|a| a.label() == picked.trim()) else {
                return Ok(());
            };
            action
        }
    };

    if !dry_run && !yes && action != Action::Lock {
        let prompt = format!("Confirm {}", action.label());
        if pick(&format!("{prompt}\nCancel"), "confirm » ")?.trim() != prompt {
            return Ok(());
        }
    }

    let (program, args) = action.command();
    if dry_run {
        if json_output {
            let command = [program]
                .into_iter()
                .chain(args.iter().copied())
                .collect::<Vec<_>>();
            println!(
                "{}",
                json!({"action": action.name(), "command": command, "dry_run": true})
            );
        } else {
            println!("{} {}", program, args.join(" "));
        }
        return Ok(());
    }
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("run {program}"))?;
    if !status.success() {
        bail!("{program} {} exited with {status}", args.join(" "));
    }
    if json_output {
        println!("{}", json!({"action": action.name(), "ok": true}));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_have_unique_names_and_commands() {
        let names: Vec<_> = Action::ALL.map(Action::name).into();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), Action::ALL.len());
        for action in Action::ALL {
            assert_eq!(parse(action.name()).unwrap(), action);
            assert!(!action.command().0.is_empty());
        }
        assert!(parse("hibernate").is_err());
    }
}
