# PartyLink

Use LDPlayer on your second PC from your main PC, in a Discord-style app: you see its window, click and type in it, hear what it plays, and talk through your own mic. It's meant for using the PlayStation App's party voice without sitting at the other PC.

There's one self-contained program, **`dist\PartyLink.exe`**. Copy it to both PCs and double-click it. A setup wizard walks you through the rest.

## Setup

The wizard asks which PC this is, checks it, and gives you a button for anything it can fix. Nothing changes until you click.

### Second PC (the one running LDPlayer)

Choose **The PC running LDPlayer**. The wizard covers:

| Item | What the button does |
|---|---|
| PartyLink | Copies it to `%LOCALAPPDATA%\Programs\PartyLink`, adds it to the Start menu, and reopens from there |
| Firewall | Adds a rule limited to PartyLink, its two ports and private networks. Windows asks for permission |
| VB-Cable | Downloads it from VB-Audio's site, confirms the installer is signed by VB-Audio's publisher, and opens it. Click **Install Driver**, then restart the PC |
| LDPlayer | Opens its official download page. It isn't installed for you, because its installer can offer extra software: read each screen and decline anything you didn't ask for |
| Network type | Opens Windows Settings. Choose **Private network** for your connection |

Two steps have to be done inside LDPlayer, so the wizard lists them for you to tick:

1. Set LDPlayer's microphone to **CABLE Output**.
2. Confirm the PlayStation App signs in and can join a party. **Test this first, with a headset on that PC.** It's unverified: there are old reports of the app refusing to log in on emulators, and Sony doesn't support running it that way. If this fails, PartyLink can't fix it. A second PSN account is the safer way to test.

PartyLink then shows a **pairing code** and **IP address**. Keep LDPlayer open and visible, with nothing on top of it.

### Main PC (the one you sit at)

Choose **The PC I sit at**. The wizard checks your speakers and mic, then shows a live mic meter and a test sound. Use a **headset**: with speakers, your mic picks up the party and they hear themselves. This PC needs no firewall rule or VB-Cable.

To connect, open the gear's **Connection** tab and enter the IP address and pairing code from the other PC.

## Using it

- **Friends** (top-left button) is reserved for your online and offline PSN friends. It's empty for now, because that needs a PlayStation sign-in PartyLink doesn't have yet.
- **The sidebar** is for available parties, which PartyLink can't list yet either, and your second PC. Click **Second PC** to see its picture.
- **Click the picture** to control the other PC. Click outside it to get your keyboard back.
- **Mic and headset buttons** (bottom left) mute and deafen, and the speaking ring shows when you're talking.
- **The hang-up button** (above them) disconnects. The Host keeps running, and the code stays the same until it restarts.
- **The gear** (next to the headset button) opens Settings:
  - **Connection:** on the main PC, enter the IP and code or disconnect. On the second PC, the pairing code, shared window and activity.
  - **Audio:** the mic meter and test sound. PartyLink uses your Windows default mic and speakers.
  - **Setup:** re-run the checks any time, or switch this PC's role.

Until the lists are filled in, your parties and chats are inside the PlayStation App, on the picture.

## Good to know

- **Both PCs need to be on the same home network.** Don't forward the ports to the internet: connections aren't encrypted, and the only protection is the pairing code.
- **The code changes each time the Host starts** and is never saved. Only one Viewer can connect at a time, and wrong codes are slowed down.
- **While connected, the Viewer controls the second PC's real mouse.** Anyone using that PC at the same time will fight over the cursor.
- **Everything playing on the second PC gets sent**, so close anything else that makes sound there.
- **Video is 10 frames per second**, which is plenty for tapping through an app, not for playing games.
- **If clicks do nothing in LDPlayer**, it may be running as administrator. Run PartyLink as administrator on that PC too, because Windows blocks input from normal programs to elevated ones.
- **It needs the WebView2 Runtime**, which comes with Windows 11. If it's missing, PartyLink tells you.
- **Windows may warn the first time you run it** on another PC, because the exe isn't code-signed.

## What's been tested

| Tested on one PC | Result |
|---|---|
| Real app: wizard running real checks | Works |
| Real app: Host screen with a live code and IP | Works |
| Real app: Viewer pairing with a running Host over 127.0.0.1 and showing its window | Works |
| Pairing code not saved to disk | Confirmed |
| Host: wrong code refused, about 10 fps, malformed input ignored, party audio sent | Works |
| Every screen and state, against a mock backend | Looks right |
| VB-Cable download and signature check (dry run) | Works |
| Command-line install and uninstall | Works, nothing left behind |
| Unit tests | 17 pass |

**Not tested yet:**

- Clicks and typing landing in LDPlayer (nothing was sent, so the mouse on the test PC wasn't touched)
- Hearing the party in the app, the mute and deafen buttons, and the mic meter with real audio
- Your voice reaching LDPlayer through VB-Cable
- Two separate PCs
- The wizard's Install, Firewall and VB-Cable buttons (Install also reopens the app from its new location)
- Anything inside LDPlayer or the PlayStation App

## An existing alternative

[Parsec](https://parsec.app) is a free remote desktop tool whose hosts support microphone passthrough natively, with no VB-Cable. It's worth trying if PartyLink gives you trouble. Its mic feature needs its virtual USB driver on the host, and you sign in with the same Parsec account on both PCs.

## Terminal commands

```
PartyLink.exe host [--window <title text>]    host without the app window
PartyLink.exe view <host IP> [--code <code>]  open the app and connect straight away
PartyLink.exe windows                         list windows that can be shared
PartyLink.exe check <host|view> [host IP]     check this PC (changes nothing)
PartyLink.exe setup <host|view> [--yes] [--dry-run]   install and fix what it can
PartyLink.exe uninstall [--yes]               remove the installed copy, shortcut and firewall rule
```

`--dry-run` changes nothing, but still downloads VB-Cable and checks its signature. App settings live in `%LOCALAPPDATA%\PartyLink`; delete that folder to run the wizard again from the start. `uninstall` doesn't remove it, or VB-Cable and LDPlayer.

## Building

```powershell
$env:CARGO_TARGET_DIR = "$env:TEMP\pl-build"
cargo build --release
cargo test --release
```

Keep the short build folder: long paths can make the Windows linker fail.
