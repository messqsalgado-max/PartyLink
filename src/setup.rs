//! The checker, installer and uninstaller.
//!
//! Checks only look and never change anything. Each install step runs only when the user asks for
//! it, and anything that needs administrator rights goes through the normal Windows permission prompt.

use crate::audio::{self, Buffer};
use crate::host;
use crate::proto;
use serde::Serialize;
use std::io::{self, BufRead, Write};
use std::net::{IpAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CABLE_DEVICE: &str = "CABLE Input";
const CABLE_ZIP_URL: &str = "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip";
const CABLE_PAGE: &str = "https://vb-audio.com/Cable/";
/// The publisher VB-Audio signs its installers with. Anything else is refused.
const CABLE_SIGNER: &str = "CN=BUREL VINCENT";
const LDPLAYER_PAGE: &str = "https://www.ldplayer.net/";
const RULE_NAME: &str = "PartyLink";
/// Stops console windows flashing up when the app runs a helper command.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Role {
    Host,
    Viewer,
}

impl Role {
    pub fn parse(text: &str) -> Option<Role> {
        match text.to_ascii_lowercase().as_str() {
            "host" | "1" => Some(Role::Host),
            "view" | "viewer" | "2" => Some(Role::Viewer),
            _ => None,
        }
    }
}

/// One line of a check: what was looked at, how it went, and how to fix it.
#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub label: String,
    /// "ok", "warn", "fail" or "info".
    pub state: &'static str,
    pub detail: String,
    pub fix: String,
    /// A step the app can run or open for the user: install, firewall, cable, ldplayer, network, sound.
    pub action: Option<&'static str>,
}

#[derive(Default)]
struct Report {
    items: Vec<Item>,
}

impl Report {
    fn add(&mut self, label: &str, state: &'static str, detail: &str, fix: &str, action: Option<&'static str>) {
        self.items.push(Item {
            label: label.into(),
            state,
            detail: detail.into(),
            fix: fix.into(),
            action,
        });
    }

    fn ok(&mut self, label: &str, detail: &str) {
        self.add(label, "ok", detail, "", None);
    }
}

/// Runs a PowerShell script. Values go in through environment variables, never into the script
/// text, so a path containing quotes can't change what the script does.
fn powershell(script: &str, env: &[(&str, &str)]) -> Result<String, String> {
    let full = format!("$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'; {script}");
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &full]).creation_flags(CREATE_NO_WINDOW);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().map_err(|e| format!("couldn't run PowerShell: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let error = String::from_utf8_lossy(&output.stderr);
        Err(error.lines().next().unwrap_or("PowerShell reported an error").trim().to_string())
    }
}

/// Checks whether this PC is ready. `host_ip` also tests the connection to the host from a viewer,
/// `mic_test` listens to the mic for 3 seconds, and `hosting` means this program holds the ports.
pub fn check_items(role: Role, host_ip: Option<&str>, mic_test: bool, hosting: bool) -> Vec<Item> {
    let mut report = Report::default();
    check_network(&mut report, role);
    check_installed(&mut report, role);
    match role {
        Role::Host => check_host(&mut report, hosting),
        Role::Viewer => check_viewer(&mut report, host_ip, mic_test),
    }
    report.items
}

/// The command-line checker. Returns true if nothing failed.
pub fn print_check(role: Role, host_ip: Option<&str>) -> bool {
    println!("PartyLink check: this PC as the {}", if role == Role::Host { "Host" } else { "Viewer" });
    println!();
    if role == Role::Viewer {
        println!("Testing your mic: say something for 3 seconds. Nothing is recorded or saved.");
    }
    let items = check_items(role, host_ip, true, false);
    for item in &items {
        let tag = match item.state {
            "ok" => "[ OK ]",
            "warn" => "[WARN]",
            "fail" => "[FAIL]",
            _ => "[ -- ]",
        };
        println!("  {tag}  {}: {}", item.label, item.detail);
        if !item.fix.is_empty() {
            println!("          Fix: {}", item.fix);
        }
    }
    let failed = items.iter().filter(|i| i.state == "fail").count();
    let warned = items.iter().filter(|i| i.state == "warn").count();
    println!();
    if failed == 0 && warned == 0 {
        println!("Everything looks ready.");
    } else if failed == 0 {
        println!("Ready to try, with {warned} warning(s) above.");
    } else {
        println!("{failed} problem(s) to fix, and {warned} warning(s).");
    }
    failed == 0
}

fn check_network(report: &mut Report, role: Role) {
    match host::local_ip() {
        Some(ip) if is_private_ip(ip) => report.ok("Network", &format!("connected, this PC is {ip}")),
        Some(ip) => report.add("Network", "warn", &format!("{ip} doesn't look like a home network address"),
            "Connect both PCs to the same home router.", None),
        None => report.add("Network", "fail", "no connection found", "Connect this PC to your home network.", None),
    }
    let public = powershell(
        "@(Get-NetConnectionProfile | Where-Object NetworkCategory -eq 'Public').Name -join ', '",
        &[],
    );
    match public {
        Ok(names) if !names.is_empty() && role == Role::Host => report.add(
            "Network type",
            "fail",
            &format!("\"{names}\" is set to Public, so the other PC will be blocked"),
            "In Windows Settings, open your connection's properties and choose Private network.",
            Some("network"),
        ),
        Ok(names) if !names.is_empty() => report.ok("Network type", &format!("\"{names}\" is Public, which is fine on this PC")),
        Ok(_) => report.ok("Network type", "Private"),
        Err(e) => report.add("Network type", "warn", &format!("couldn't read it ({e})"),
            "Make sure your network is set to Private in Windows Settings.", Some("network")),
    }
}

fn check_installed(report: &mut Report, role: Role) {
    let current = std::env::current_exe().ok();
    let installed = install_dir().map(|dir| dir.join("PartyLink.exe"));
    let running_installed = match (&current, &installed) {
        (Some(current), Some(installed)) => same_path(current, installed),
        _ => false,
    };
    let in_menu = shortcut_path().is_some_and(|path| path.exists());
    if running_installed && in_menu {
        report.ok("PartyLink", "installed and in your Start menu");
    } else {
        // The firewall rule is tied to one copy of the program, so it matters more on the Host.
        let state = if role == Role::Host { "warn" } else { "info" };
        report.add("PartyLink", state, "running from this folder, not installed",
            "Install it so it's in your Start menu and stays in one place.", Some("install"));
    }
}

fn check_host(report: &mut Report, hosting: bool) {
    let virtualization = powershell(
        "[bool]((Get-CimInstance Win32_ComputerSystem).HypervisorPresent -or (Get-CimInstance Win32_Processor | Select-Object -First 1).VirtualizationFirmwareEnabled)",
        &[],
    );
    match virtualization.as_deref() {
        Ok("True") => report.ok("Virtualization", "on, which LDPlayer needs to run well"),
        Ok(_) => report.add("Virtualization", "warn", "off, so LDPlayer may be slow or refuse to start",
            "Turn on VT-x (Intel) or SVM (AMD) in this PC's BIOS settings.", None),
        Err(e) => report.add("Virtualization", "warn", &format!("couldn't read it ({e})"),
            "Check Task Manager > Performance > CPU.", None),
    }

    if ldplayer_installed() {
        report.ok("LDPlayer", "installed");
        if host::find_window("LDPlayer").is_some() {
            report.ok("LDPlayer window", "open and visible");
        } else {
            report.add("LDPlayer window", "warn", "not open, or minimized", "Open LDPlayer and keep it visible while hosting.", None);
        }
    } else {
        report.add("LDPlayer", "fail", "not installed",
            "Get it from the official site. Read each screen of its installer and decline any extra software.", Some("ldplayer"));
    }

    if cable_installed() {
        report.ok("VB-Cable", "installed");
    } else {
        report.add("VB-Cable", "fail", "not installed, so nobody will hear you",
            "It's the free virtual mic that carries your voice into LDPlayer. Restart this PC after installing.", Some("cable"));
    }

    match audio::default_output() {
        Some(device) => match audio::start_capture(&device, Buffer::default(), true) {
            Ok(_stream) => report.ok("Party audio", &format!("can record \"{}\"", audio::device_name(&device))),
            Err(e) => report.add("Party audio", "fail", &e, "Check this PC's default speakers in Windows sound settings.", Some("sound")),
        },
        None => report.add("Party audio", "fail", "no output device found", "Connect or enable an output device.", Some("sound")),
    }

    match rule_output() {
        Some(rule) if rule_allows(&rule, &current_exe_text()) => report.ok("Firewall", "this copy of PartyLink is allowed on private networks"),
        Some(_) => report.add("Firewall", "warn", "the rule points at a different copy of PartyLink",
            "Update the rule so this copy can accept the connection.", Some("firewall")),
        None => report.add("Firewall", "warn", "no PartyLink rule yet",
            "Allow PartyLink on private networks, limited to its own two ports.", Some("firewall")),
    }

    if !hosting {
        let free = [proto::CONTROL_PORT, proto::AUDIO_PORT]
            .iter()
            .all(|&port| TcpListener::bind(("0.0.0.0", port)).is_ok());
        if free {
            report.ok("Ports", &format!("{} and {} are free", proto::CONTROL_PORT, proto::AUDIO_PORT));
        } else {
            report.add("Ports", "warn", "in use", "PartyLink may already be running as the Host. Close the other copy.", None);
        }
    }
}

fn check_viewer(report: &mut Report, host_ip: Option<&str>, mic_test: bool) {
    match audio::default_output() {
        Some(device) => match audio::start_playback(&device, Buffer::default()) {
            Ok(_stream) => report.ok("Speakers", &format!("\"{}\" works", audio::device_name(&device))),
            Err(e) => report.add("Speakers", "fail", &e, "Check your default output in Windows sound settings.", Some("sound")),
        },
        None => report.add("Speakers", "fail", "no output device found", "Plug in your headset.", Some("sound")),
    }
    match audio::default_input() {
        Some(device) => {
            let name = audio::device_name(&device);
            let buffer = Buffer::default();
            match audio::start_capture(&device, buffer.clone(), false) {
                Ok(_stream) if mic_test => {
                    let peak = loudest_sound(&buffer, Duration::from_secs(3));
                    if peak > 0.02 {
                        report.ok("Microphone", &format!("\"{name}\" hears you"));
                    } else {
                        report.add("Microphone", "warn", &format!("\"{name}\" works, but heard almost nothing"),
                            "Check it isn't muted, and that it's the default mic in Windows sound settings.", Some("sound"));
                    }
                }
                Ok(_stream) => report.ok("Microphone", &format!("\"{name}\" is available")),
                Err(e) => report.add("Microphone", "fail", &e, "Check Windows Settings > Privacy & security > Microphone.", Some("sound")),
            }
        }
        None => report.add("Microphone", "fail", "no microphone found", "Plug in your headset.", Some("sound")),
    }
    if let Some(ip) = host_ip {
        for (name, port) in [("Host video and controls", proto::CONTROL_PORT), ("Host audio", proto::AUDIO_PORT)] {
            if can_reach(ip, port) {
                report.ok(name, &format!("reached {ip}:{port}"));
            } else {
                report.add(name, "fail", &format!("can't reach {ip}:{port}"),
                    "Start hosting on the other PC, check the IP, and run its check.", None);
            }
        }
    }
}

fn loudest_sound(buffer: &Buffer, duration: Duration) -> f32 {
    let started = Instant::now();
    let mut peak = 0.0f32;
    while started.elapsed() < duration {
        thread::sleep(Duration::from_millis(40));
        peak = peak.max(buffer.take_level());
        buffer.take_all();
    }
    peak
}

fn can_reach(ip: &str, port: u16) -> bool {
    let Ok(addresses) = (ip, port).to_socket_addrs() else { return false };
    addresses.into_iter().any(|a| TcpStream::connect_timeout(&a, Duration::from_secs(3)).is_ok())
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(_) => false,
    }
}

fn ldplayer_installed() -> bool {
    powershell(
        "[bool](Get-ItemProperty 'HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*','HKLM:\\SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*','HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*' -ErrorAction SilentlyContinue | Where-Object DisplayName -match 'LDPlayer')",
        &[],
    )
    .is_ok_and(|answer| answer == "True")
}

pub fn cable_installed() -> bool {
    audio::find_output(CABLE_DEVICE).is_some()
}

fn current_exe_text() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

/// The text of the PartyLink firewall rule, if there is one.
fn rule_output() -> Option<String> {
    let output = Command::new("netsh")
        .args(["advfirewall", "firewall", "show", "rule", &format!("name={RULE_NAME}"), "verbose"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn rule_allows(rule: &str, exe: &str) -> bool {
    !exe.is_empty() && rule.to_lowercase().contains(&exe.to_lowercase())
}

/// Accepts only a valid signature from VB-Audio's publisher.
fn trusted_cable_signature(status: &str, subject: &str) -> bool {
    status == "Valid" && subject.starts_with(CABLE_SIGNER)
}

pub fn install_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Programs").join("PartyLink"))
}

fn shortcut_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join(r"Microsoft\Windows\Start Menu\Programs\PartyLink.lnk"))
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn open_url(url: &str) {
    let _ = Command::new("rundll32.exe").args(["url.dll,FileProtocolHandler", url]).spawn();
}

pub fn open_ldplayer_page() {
    open_url(LDPLAYER_PAGE);
}

pub fn open_network_settings() {
    open_url("ms-settings:network-status");
}

pub fn open_sound_settings() {
    open_url("ms-settings:sound");
}

/// Copies this program to its install folder and adds a Start menu entry. Returns where it lives.
pub fn install_program() -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = install_dir().ok_or("couldn't find your local app folder")?;
    let target = dir.join("PartyLink.exe");
    let shortcut = shortcut_path().ok_or("couldn't find your Start menu folder")?;
    if !same_path(&current, &target) {
        std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
        std::fs::copy(&current, &target).map_err(|e| format!("couldn't copy PartyLink (is an installed copy running?): {e}"))?;
    }
    let target_text = target.display().to_string();
    let shortcut_text = shortcut.display().to_string();
    powershell(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut($env:PL_LINK); $s.TargetPath = $env:PL_EXE; $s.Description = 'PartyLink'; $s.Save()",
        &[("PL_LINK", &shortcut_text), ("PL_EXE", &target_text)],
    )
    .map_err(|e| format!("couldn't add the Start menu entry: {e}"))?;
    Ok(target)
}

/// Allows `exe` through the firewall on private networks, on PartyLink's ports only.
pub fn add_firewall_rule(exe: &Path) -> Result<(), String> {
    let exe_text = exe.display().to_string();
    // These characters could change the meaning of the elevated command line, so don't risk it.
    if exe_text.contains(['"', '%']) {
        return Err("the install path has unusual characters. Choose Allow when Windows asks instead.".into());
    }
    powershell(
        "$a = '/c netsh advfirewall firewall delete rule name=PartyLink & netsh advfirewall firewall add rule name=PartyLink dir=in action=allow program=\"' + $env:PL_EXE + '\" protocol=TCP localport=47811,47812 profile=private enable=yes'; $p = Start-Process cmd.exe -Verb RunAs -WindowStyle Hidden -ArgumentList $a -Wait -PassThru; exit $p.ExitCode",
        &[("PL_EXE", &exe_text)],
    )
    .map(|_| ())
    .map_err(|e| format!("the firewall rule wasn't added ({e})"))
}

/// Downloads VB-Cable, verifies the installer's signature and opens it. Returns a message for the
/// user. With `dry_run`, it stops after the signature check.
pub fn install_cable(dry_run: bool) -> Result<String, String> {
    if cable_installed() {
        return Ok("VB-Cable is already installed.".into());
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let work = std::env::temp_dir().join(format!("PartyLink-setup-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| format!("couldn't create a temporary folder: {e}"))?;
    let result = install_cable_from(&work, dry_run);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn install_cable_from(work: &Path, dry_run: bool) -> Result<String, String> {
    let zip = work.join("vbcable.zip").display().to_string();
    let extracted = work.join("files");
    let extracted_text = extracted.display().to_string();
    powershell(
        "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -UseBasicParsing -Uri $env:PL_URL -OutFile $env:PL_ZIP; Expand-Archive -LiteralPath $env:PL_ZIP -DestinationPath $env:PL_DIR",
        &[("PL_URL", CABLE_ZIP_URL), ("PL_ZIP", &zip), ("PL_DIR", &extracted_text)],
    )
    .map_err(|e| format!("couldn't download VB-Cable ({e}). You can install it yourself from {CABLE_PAGE}."))?;

    let setup_exe = extracted.join("VBCABLE_Setup_x64.exe");
    if !setup_exe.exists() {
        return Err(format!("the download didn't contain the expected installer. You can install it yourself from {CABLE_PAGE}."));
    }
    let setup_text = setup_exe.display().to_string();
    let signature = powershell(
        "$s = Get-AuthenticodeSignature -LiteralPath $env:PL_EXE; $s.Status.ToString(); $s.SignerCertificate.Subject",
        &[("PL_EXE", &setup_text)],
    )
    .map_err(|e| format!("couldn't check the installer's signature ({e}), so it wasn't opened."))?;
    let mut lines = signature.lines();
    let (status, subject) = (lines.next().unwrap_or("").trim(), lines.next().unwrap_or("").trim());
    if !trusted_cable_signature(status, subject) {
        return Err(format!("the installer's signature didn't match VB-Audio's (status: {status}), so it wasn't opened."));
    }
    if dry_run {
        return Ok("Signature verified: signed by VB-Audio's publisher. Dry run, so the installer wasn't opened.".into());
    }
    powershell("Start-Process -FilePath $env:PL_EXE -Verb RunAs -Wait", &[("PL_EXE", &setup_text)])
        .map_err(|e| format!("the installer didn't run ({e})."))?;
    if cable_installed() {
        Ok("VB-Cable is installed. Restart this PC if it doesn't work straight away.".into())
    } else {
        Ok("VB-Cable isn't showing up yet. If you finished its installer, restart this PC and check again.".into())
    }
}

fn confirm(question: &str, yes: bool) -> bool {
    if yes {
        println!("{question} yes");
        return true;
    }
    print!("{question} [y/n] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// The command-line setup. The app's wizard runs the same steps with buttons.
pub fn setup(role: Role, yes: bool, dry_run: bool) -> Result<(), String> {
    println!("PartyLink setup: this PC as the {}", if role == Role::Host { "Host" } else { "Viewer" });
    if dry_run {
        println!("Dry run: nothing will be installed or changed.");
    }
    println!();

    let mut exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if dry_run {
        println!("Would copy PartyLink to its install folder and add it to the Start menu.");
    } else if confirm("Install PartyLink and add it to the Start menu?", yes) {
        exe = install_program()?;
        println!("Installed to {}.", exe.display());
    }
    if role == Role::Host {
        if dry_run {
            println!("Would allow PartyLink through the firewall on private networks only.");
        } else if !rule_output().is_some_and(|rule| rule_allows(&rule, &exe.display().to_string()))
            && confirm("Add a firewall rule, limited to PartyLink's ports on private networks?", yes)
        {
            match add_firewall_rule(&exe) {
                Ok(()) => println!("Firewall rule added."),
                Err(e) => println!("{e}. Choose Allow on private networks when Windows asks."),
            }
        }
        if dry_run || confirm("Download VB-Cable from VB-Audio's site and open its installer?", yes) {
            println!("{}", install_cable(dry_run)?);
        }
        if !ldplayer_installed() {
            println!("LDPlayer isn't installed. Get it from {LDPLAYER_PAGE} and decline any extra software.");
        }
    }
    println!();
    print_check(role, None);
    Ok(())
}

/// Removes what setup added, except VB-Cable and LDPlayer, which are separate programs.
pub fn uninstall(yes: bool) -> Result<(), String> {
    if !confirm("Remove PartyLink's installed copy, Start menu entry and firewall rule?", yes) {
        println!("Nothing was removed.");
        return Ok(());
    }
    if let Some(shortcut) = shortcut_path()
        && shortcut.exists()
    {
        std::fs::remove_file(&shortcut).map_err(|e| format!("couldn't remove the Start menu entry: {e}"))?;
        println!("Removed the Start menu entry.");
    }
    if rule_output().is_some() {
        println!("Removing the firewall rule. Windows will ask for permission.");
        let result = powershell(
            "$p = Start-Process netsh.exe -Verb RunAs -WindowStyle Hidden -ArgumentList 'advfirewall firewall delete rule name=PartyLink' -Wait -PassThru; exit $p.ExitCode",
            &[],
        );
        match result {
            Ok(_) => println!("Removed the firewall rule."),
            Err(e) => println!("The firewall rule wasn't removed ({e})."),
        }
    }
    if let Some(dir) = install_dir() {
        let target = dir.join("PartyLink.exe");
        let current = std::env::current_exe().map_err(|e| e.to_string())?;
        if same_path(&current, &target) {
            // A running program can't delete itself, so a short background command does it after exit.
            let dir_text = dir.display().to_string();
            powershell(
                "$c = 'timeout /t 2 /nobreak >nul & del /q \"' + $env:PL_EXE + '\" & rmdir \"' + $env:PL_DIR + '\"'; Start-Process cmd.exe -WindowStyle Hidden -ArgumentList ('/c ' + $c)",
                &[("PL_EXE", &target.display().to_string()), ("PL_DIR", &dir_text)],
            )
            .map_err(|e| format!("couldn't remove the installed copy: {e}"))?;
            println!("The installed copy will be removed in a moment.");
        } else if target.exists() {
            std::fs::remove_file(&target).map_err(|e| format!("couldn't remove {}: {e}", target.display()))?;
            let _ = std::fs::remove_dir(&dir);
            println!("Removed {}.", target.display());
        }
    }
    println!("Done. VB-Cable and LDPlayer are still installed; remove them separately if you want to.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_parse() {
        assert_eq!(Role::parse("host"), Some(Role::Host));
        assert_eq!(Role::parse("VIEW"), Some(Role::Viewer));
        assert_eq!(Role::parse("2"), Some(Role::Viewer));
        assert_eq!(Role::parse("other"), None);
    }

    #[test]
    fn only_vb_audio_signatures_are_trusted() {
        let subject = "CN=BUREL VINCENT Entrepreneur individuel, O=BUREL VINCENT Entrepreneur individuel, C=FR";
        assert!(trusted_cable_signature("Valid", subject));
        assert!(!trusted_cable_signature("NotSigned", subject));
        assert!(!trusted_cable_signature("HashMismatch", subject));
        assert!(!trusted_cable_signature("Valid", "CN=Somebody Else"));
        assert!(!trusted_cable_signature("Valid", "CN=Evil, O=CN=BUREL VINCENT"));
        assert!(!trusted_cable_signature("", ""));
    }

    #[test]
    fn private_addresses_are_recognised() {
        assert!(is_private_ip("10.0.0.144".parse().unwrap()));
        assert!(is_private_ip("192.168.1.5".parse().unwrap()));
        assert!(is_private_ip("172.16.0.2".parse().unwrap()));
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn firewall_rule_must_name_this_program() {
        let rule = "Rule Name: PartyLink\nProgram: C:\\Users\\Me\\AppData\\Local\\Programs\\PartyLink\\PartyLink.exe\n";
        assert!(rule_allows(rule, "c:\\users\\me\\appdata\\local\\programs\\partylink\\partylink.exe"));
        assert!(!rule_allows(rule, "D:\\Other\\PartyLink.exe"));
        assert!(!rule_allows(rule, ""));
    }
}
