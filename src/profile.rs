//! Fedora power-profile frontend.
//!
//! This module deliberately contains no policy engine and never writes sysfs.
//! It only reads and sets the standard PowerProfiles D-Bus property exported
//! by Fedora's `tuned-ppd` compatibility service.

use std::{fmt, process::Command, str::FromStr};

use anyhow::{bail, Context, Result};
use serde_json::json;
use zbus::blocking::{Connection, Proxy};

const DESTINATION: &str = "net.hadess.PowerProfiles";
const OBJECT_PATH: &str = "/net/hadess/PowerProfiles";
const INTERFACE: &str = "net.hadess.PowerProfiles";
const WAYBAR_SIGNAL: &str = "-RTMIN+18";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    PowerSaver,
    Balanced,
    Performance,
}

impl Profile {
    fn as_str(self) -> &'static str {
        match self {
            Self::PowerSaver => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    fn short(self) -> &'static str {
        match self {
            Self::PowerSaver => "P:sav",
            Self::Balanced => "P:bal",
            Self::Performance => "P:max",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::PowerSaver => Self::Balanced,
            Self::Balanced => Self::Performance,
            Self::Performance => Self::PowerSaver,
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Profile {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "power-saver" => Ok(Self::PowerSaver),
            "balanced" => Ok(Self::Balanced),
            "performance" => Ok(Self::Performance),
            _ => {
                bail!("unknown profile {value:?} (expected power-saver, balanced, or performance)")
            }
        }
    }
}

pub fn run(action: Option<&str>, waybar: bool, json_output: bool) -> Result<()> {
    if waybar {
        let output = match current() {
            Ok(profile) => waybar_json(profile),
            Err(error) => unavailable_waybar_json(&format!("{error:#}")),
        };
        println!("{output}");
        return Ok(());
    }

    match action.unwrap_or("menu") {
        "menu" => menu(),
        "status" => print_profile(current()?, json_output),
        "next" => apply(current()?.next(), json_output),
        value => apply(value.parse()?, json_output),
    }
}

fn current() -> Result<Profile> {
    let value: String = proxy()?
        .get_property("ActiveProfile")
        .context("read tuned-ppd ActiveProfile")?;
    value.parse()
}

fn apply(profile: Profile, json_output: bool) -> Result<()> {
    proxy()?
        .set_property("ActiveProfile", profile.as_str())
        .with_context(|| format!("set tuned-ppd profile to {profile}"))?;
    refresh_waybar();
    print_profile(profile, json_output)
}

fn proxy() -> Result<Proxy<'static>> {
    let connection = Connection::system().context("connect to the system D-Bus")?;
    Proxy::new_owned(connection, DESTINATION, OBJECT_PATH, INTERFACE)
        .context("connect to Fedora tuned-ppd")
}

fn menu() -> Result<()> {
    let active = current()?;
    let choices = [
        (Profile::PowerSaver, "Power saver"),
        (Profile::Balanced, "Balanced"),
        (Profile::Performance, "Performance"),
    ];
    let rows = choices
        .iter()
        .map(|(profile, label)| {
            let mark = if *profile == active { "●" } else { " " };
            format!("{mark} {label}")
        })
        .collect::<Vec<_>>();
    let picked = crate::wifi::run_fuzzel(&rows.join("\n"), "profile » ", false)?;
    let Some(index) = rows.iter().position(|row| row == picked.trim()) else {
        return Ok(());
    };
    apply(choices[index].0, false)
}

fn print_profile(profile: Profile, json_output: bool) -> Result<()> {
    if json_output {
        println!(
            "{}",
            json!({"backend": "tuned-ppd", "profile": profile.as_str()})
        );
    } else {
        println!("{profile}");
    }
    Ok(())
}

fn waybar_json(profile: Profile) -> String {
    json!({
        "alt": profile.as_str(),
        "class": profile.as_str(),
        "text": profile.short(),
        "tooltip": format!(
            "Fedora power profile: {profile}\nleft-click: cycle · middle-click: balanced · right-click: choose"
        ),
    })
    .to_string()
}

fn unavailable_waybar_json(error: &str) -> String {
    json!({
        "alt": "unavailable",
        "class": "unavailable",
        "text": "P:?",
        "tooltip": format!("Fedora power profiles unavailable: {error}"),
    })
    .to_string()
}

fn refresh_waybar() {
    let _ = Command::new("pkill")
        .args([WAYBAR_SIGNAL, "-x", "waybar"])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_standard_power_profiles() {
        assert_eq!(
            "power-saver".parse::<Profile>().unwrap(),
            Profile::PowerSaver
        );
        assert_eq!("balanced".parse::<Profile>().unwrap(), Profile::Balanced);
        assert_eq!(
            "performance".parse::<Profile>().unwrap(),
            Profile::Performance
        );
        assert!("turbo".parse::<Profile>().is_err());
    }

    #[test]
    fn next_cycles_through_all_standard_profiles() {
        assert_eq!(Profile::PowerSaver.next(), Profile::Balanced);
        assert_eq!(Profile::Balanced.next(), Profile::Performance);
        assert_eq!(Profile::Performance.next(), Profile::PowerSaver);
    }

    #[test]
    fn waybar_output_identifies_active_profile() {
        let value: serde_json::Value =
            serde_json::from_str(&waybar_json(Profile::Balanced)).unwrap();
        assert_eq!(value["text"], "P:bal");
        assert_eq!(value["class"], "balanced");
        assert_eq!(value["alt"], "balanced");
    }

    #[test]
    fn unavailable_waybar_output_stays_visible() {
        let value: serde_json::Value =
            serde_json::from_str(&unavailable_waybar_json("service missing")).unwrap();
        assert_eq!(value["text"], "P:?");
        assert_eq!(value["class"], "unavailable");
        assert!(value["tooltip"]
            .as_str()
            .unwrap()
            .contains("service missing"));
    }
}
