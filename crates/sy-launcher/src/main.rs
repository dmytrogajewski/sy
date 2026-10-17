//! Stable entry point for independently installed sy features.

use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

const HELP: &str = "sy — modular Fedora desktop and agent tools
Usage: sy <command> [arguments]

Optional packages:
  sy-system-power    sy power
  sy-system-agent    sy agt, sy policy, sy approve
  sy-system-aiplane  sy aiplane
  sy-system-auth     sy syauth
  sy-filemanager     sy file, sy plugin
  sy-system-knowledge  sy knowledge, sy auto
  sy-system-monitor    sy mon
  sy-desktop-shell     sy stack, sy wifi, desktop applets
  sy-desktop           desktop configuration commands

Run sy <command> --help for command options.
";

fn command_index(args: &[OsString]) -> Option<usize> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--root" || arg == "--target" {
            index += 2;
            continue;
        }
        let text = arg.to_string_lossy();
        if text.starts_with("--root=") || text.starts_with("--target=") {
            index += 1;
            continue;
        }
        if arg == "--" {
            return (index + 1 < args.len()).then_some(index + 1);
        }
        return Some(index);
    }
    None
}

fn feature_dir() -> PathBuf {
    std::env::var_os("SY_FEATURE_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/libexec/sy"))
}

fn run(exe: &Path, args: &[OsString], package: &str, argv0: &str) -> ! {
    if !exe.is_file() {
        eprintln!("sy: install {package} to use this command");
        process::exit(69);
    }
    let error = Command::new(exe).arg0(argv0).args(args).exec();
    eprintln!("sy: could not run {}: {error}", exe.display());
    process::exit(126);
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let index = command_index(&args);
    let command = index.and_then(|i| args.get(i)).map(OsString::as_os_str);
    if command.is_none()
        || matches!(command, Some(c) if c == OsStr::new("--help") || c == OsStr::new("-h"))
    {
        let desktop = feature_dir().join("sy-all");
        if desktop.is_file() {
            run(&desktop, &args, "sy-desktop", "sy");
        }
        print!("{HELP}");
        return;
    }
    if matches!(command, Some(c) if c == OsStr::new("--version") || c == OsStr::new("-V")) {
        println!("sy {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let dir = feature_dir();
    if command == Some(OsStr::new("power")) {
        let power_args = &args[index.expect("command index") + 1..];
        run(
            &dir.join("sy-system-power"),
            power_args,
            "sy-system-power",
            "sy-system-power",
        );
    }
    if matches!(command, Some(c) if c == OsStr::new("agt") || c == OsStr::new("policy") || c == OsStr::new("approve"))
    {
        run(&dir.join("sy-system-agent"), &args, "sy-system-agent", "sy");
    }
    if command == Some(OsStr::new("aiplane")) {
        run(
            &dir.join("sy-system-aiplane"),
            &args,
            "sy-system-aiplane",
            "sy",
        );
    }
    if command == Some(OsStr::new("syauth")) {
        run(&dir.join("sy-system-auth"), &args, "sy-system-auth", "sy");
    }
    if matches!(command, Some(c) if c == OsStr::new("file") || c == OsStr::new("plugin")) {
        run(&dir.join("sy-filemanager"), &args, "sy-filemanager", "sy");
    }
    if matches!(command, Some(c) if c == OsStr::new("knowledge") || c == OsStr::new("auto")) {
        run(
            &dir.join("sy-system-knowledge"),
            &args,
            "sy-system-knowledge",
            "sy",
        );
    }
    if command == Some(OsStr::new("mon")) {
        run(
            &dir.join("sy-system-monitor"),
            &args,
            "sy-system-monitor",
            "sy",
        );
    }
    if matches!(command, Some(c) if ["stack", "popup", "cal", "wifi", "net", "wallpaper", "vol", "bright", "bat", "profile", "pwr", "gpu", "npu", "disk", "notif", "bt", "wwan", "silent", "snd"].iter().any(|name| c == OsStr::new(name)))
    {
        run(
            &dir.join("sy-desktop-shell"),
            &args,
            "sy-desktop-shell",
            "sy",
        );
    }
    run(&dir.join("sy-all"), &args, "sy-desktop", "sy");
}
