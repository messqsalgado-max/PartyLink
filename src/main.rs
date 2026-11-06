//! PartyLink: use the PlayStation App running in LDPlayer on another PC as if it were on this one.
//!
//! Double-click to open the app. There are also a few commands for use in a terminal:
//!
//!   partylink host [--window <title text>]    host without the app window
//!   partylink view <host IP> [--code <code>]  open the app and connect straight away
//!   partylink windows                         list windows that can be shared
//!   partylink check <host|view> [host IP]     check this PC
//!   partylink setup <host|view> [--yes] [--dry-run]
//!   partylink uninstall [--yes]

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod audio;
mod host;
mod proto;
mod setup;
mod view;

use setup::Role;
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

const DEFAULT_WINDOW: &str = "LDPlayer";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let is_app = matches!(args.first().map(String::as_str), None | Some("view") | Some("--no-audio"));
    if !is_app {
        // This is a windowed program, so terminal commands have to reconnect to the terminal to print.
        unsafe {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
    let result = match args.first().map(String::as_str) {
        None | Some("--no-audio") => app::run(app::Launch { connect: None, no_audio: has(&args, "--no-audio") }),
        Some("view") => match args.get(1).filter(|a| !a.starts_with("--")) {
            Some(ip) => app::run(app::Launch {
                connect: flag(&args, "--code").map(|code| (ip.clone(), code)),
                no_audio: has(&args, "--no-audio"),
            }),
            None => app::run(app::Launch::default()),
        },
        Some("host") => host::run(&flag(&args, "--window").unwrap_or_else(|| DEFAULT_WINDOW.into())),
        Some("windows") => host::list_windows(),
        Some("check") => run_check(&args),
        Some("setup") => match args.get(1).and_then(|r| Role::parse(r)) {
            Some(role) => setup::setup(role, has(&args, "--yes"), has(&args, "--dry-run")),
            None => Err("Usage: partylink setup <host|view> [--yes] [--dry-run]".into()),
        },
        Some("uninstall") => setup::uninstall(has(&args, "--yes")),
        Some("-h" | "--help" | "help") => {
            print_help();
            Ok(())
        }
        Some(other) => Err(format!("Unknown command \"{other}\". Run `partylink --help`.")),
    };
    if let Err(message) = result {
        eprintln!("\n{message}");
        std::process::exit(1);
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].clone())
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn run_check(args: &[String]) -> Result<(), String> {
    let Some(role) = args.get(1).and_then(|r| Role::parse(r)) else {
        return Err("Usage: partylink check <host|view> [host IP]".into());
    };
    if !setup::print_check(role, args.get(2).map(String::as_str)) {
        std::process::exit(1);
    }
    Ok(())
}

fn print_help() {
    println!("PartyLink: use LDPlayer on another PC with your own screen, mouse, keyboard, headset and mic.");
    println!();
    println!("Double-click PartyLink.exe to open the app. Terminal commands:");
    println!("  partylink host [--window <title text>]    host without the app window");
    println!("  partylink view <host IP> [--code <code>]  open the app and connect straight away");
    println!("  partylink windows                         list windows that can be shared");
    println!("  partylink check <host|view> [host IP]     check this PC (changes nothing)");
    println!("  partylink setup <host|view> [--yes] [--dry-run]  install and fix what it can");
    println!("  partylink uninstall [--yes]               remove the installed copy, shortcut and firewall rule");
}
