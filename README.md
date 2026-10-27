<p align="center">
  <img src="assets/logo.png" width="128" alt="Kipimo icon">
</p>

<h1 align="center">Kipimo</h1>

<p align="center">
  A system monitor and cleanup toolkit for the Mac menu bar, written in Rust.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/macOS-15%2B%20·%20Apple%20silicon-1c1c1e?logo=apple" alt="macOS 15 or later on Apple silicon">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-1c1c1e" alt="License: GPL-3.0-or-later"></a>
</p>

<p align="center">
  <a href="https://github.com/piedol/kipimo/releases/latest/download/Kipimo-macOS-arm64.zip"><img src="assets/readme/download-mac.svg" height="56" alt="Download for Mac"></a>
</p>

<p align="center">
  <img src="docs/images/showcase.png" width="880" alt="Kipimo's menu bar panel next to its Uninstall window">
</p>

Kipimo's menu bar panel shows free disk space, memory pressure, battery charge and power draw, CPU load and temperature, network speed, and the batteries of your Bluetooth devices. The tools clear out caches and logs, uninstall apps together with the files they leave behind, show what is filling your disk, list everything that starts automatically, and check your apps for updates.

Everything Kipimo removes goes to the Trash, where Finder's **Put Back** can restore it. Kipimo takes 4.7 MB on disk and about 9 MB of memory with the panel closed.

<p align="center"><sub><em>Kipimo</em> (kee-PEE-moh) is Swahili for "measurement" or "gauge".</sub></p>

## Contents

- [The panel](#the-panel)
- [Tools](#tools): [Clean up](#clean-up) · [Uninstall](#uninstall) · [Space](#space) · [Startup items](#startup-items) · [Updates](#updates)
- [Alerts and the weekly summary](#alerts-and-the-weekly-summary)
- [Install](#install) · [Uninstall Kipimo](#uninstall-kipimo)
- [Privacy and safety](#privacy-and-safety)
- [Resource use](#resource-use)
- [Troubleshooting](#troubleshooting)
- [Development](#development)
- [License](#license)

## The panel

<img src="docs/images/panel.png" width="320" align="right" alt="The Kipimo panel: Mac Health, Protection, the startup disk, Memory, Battery, CPU, Network, Devices and today's recommendation">

Click the menu bar icon to open the panel.

| Card | Shows |
|---|---|
| Mac Health | An overall status, with your Mac's model and memory, or what needs attention |
| Protection | Whether XProtect, the malware protection built into macOS, is on, and when its definitions were updated |
| Startup disk | Free space, with a **Clean up** button |
| Memory | Memory pressure; hover for used, compressed and swap |
| Battery | Charge, charging state and the power the Mac is drawing, in watts |
| CPU | Load, chip temperature and the busiest app |
| Network | Download and upload speed |
| Devices | Battery levels of AirPods, keyboards, mice and other Bluetooth devices |
| Today's recommendation | A suggestion, such as a cleanup or waiting app updates. **Later** hides it until tomorrow |

The panel refreshes every 2 seconds while it's open (every second, if you choose that in Settings). When it's closed, Kipimo checks once a minute and colours the menu bar icon if something needs attention.

**Open Kipimo** opens the tools. The gear opens a menu with Settings and Quit.

<br clear="right">

## Tools

### Clean up

<p align="center">
  <img src="docs/images/tools-clean.png" width="820" alt="Clean up: categories of caches, logs and developer files, each with a risk label and a size">
</p>

Clean up finds files that apps and macOS recreate when they need them: app and macOS caches, logs and crash reports, Xcode build data, old device symbols and simulators, and package manager caches (Homebrew, npm, Yarn, pnpm, Cargo, pip, Gradle, CocoaPods and Go). It also lists old Xcode archives, Mail's copies of attachments you've opened, and iPhone and iPad backups older than 90 days. Backups always start unticked, because a backup may be the only copy of a device's data.

Each category shows its size and a risk label, and expands to list every item. Items that would be slow to download again start unticked with a note saying why. These include browsers used for automated testing, AI models, and items over 1 GB. Caches that a running app is using are skipped.

**Clean** moves the ticked items to the Trash. **Empty Trash…** asks Finder to empty the Trash, and Finder asks you to confirm.

### Uninstall

<p align="center">
  <img src="docs/images/tools-uninstall.png" width="820" alt="Uninstall: installed apps with their size and last use, and the files an app keeps in the Library">
</p>

Pick an app to see everything it stores on your Mac: preferences, caches, saved window state, containers and more. The list sorts by name, size or last use.

An app's preferences and caches start ticked. Its data, such as Application Support folders and containers, starts unticked, so documents and message history aren't removed by accident. If the app or any of its background helpers is running, Kipimo says so and offers to stop it. Apps that come with macOS can't be removed.

When you drag an app to the Trash yourself, Kipimo sends a notification about 30 seconds later listing the files the app left behind. You can review them and move the ones you choose to the Trash.

<p align="center">
  <img src="docs/images/review.png" width="620" alt="The leftovers review for an example app: caches, preferences and saved state ticked; data folders unticked">
</p>

### Space

<p align="center">
  <img src="docs/images/tools-space.png" width="820" alt="Space: the System Applications folder with its largest items first, their sizes and file counts">
</p>

Space shows what is using your disk, folder by folder, largest first. Choose your home folder, a disk or any other folder, then open folders to go deeper, or switch to **Largest files**. Sizing `/System/Applications` (13,850 files, 726 MB) took 0.2 seconds on an M5 Max.

**Show in Finder** reveals the selected item, and **Move to Trash…** removes it after you confirm. Kipimo won't move iCloud Drive's top-level folders, app containers, running apps or system folders.

### Startup items

<p align="center">
  <img src="docs/images/tools-startup.png" width="820" alt="Startup items: launch agents and daemons from Google, Valve, Zoom and OrbStack, each with its signer; Steam's agent is selected">
</p>

Startup items lists everything set to start automatically: apps that open at login, items allowed to run in the background, and launch agents and daemons. Each entry shows who signed it, whether it's running, and the program it runs. Kipimo flags items that are unsigned, signed only for this Mac, have an invalid signature, or point at a missing file.

- **Disable and Move to Trash…** stops a launch agent of yours and moves its file to the Trash. Put Back restores the file, and the item starts again at your next login.
- Login items and background items are managed in System Settings, and Kipimo opens the right page.
- Removing items installed for all users needs an administrator. Kipimo shows the Terminal commands to run and doesn't ask for your password.
- **Check Apps for Broken Signatures** verifies the apps in Applications and lists those whose code signature is broken.
- **Ignore** is for a flagged item, or an app from the signature check, that you already know about. It stays in the list, dimmed and marked Ignored, and stops counting toward "need a look", the menu bar icon's colour, Mac Health and Today's recommendation. An ignored app also stops counting as broken. The item counts again if its problem changes, for example a new flag, a rebuilt program, or an app that was updated or reinstalled. **Stop Ignoring** undoes it.

**Reinstall…** opens a source for a good copy of an app whose signature broke:

- its App Store page, for an App Store app;
- the download its update feed offers, for an app the Updates tool can check;
- otherwise its developer's site, if the app or its feed names it.

If Kipimo has none of these, it shows the app in Finder, and you can replace it with a new copy from its developer.

**Download and Replace…** appears when the feed offers the app as a zip. Quit the app first. Kipimo downloads the zip, unpacks it, and checks that the new copy's signature is intact and is a Developer ID signature from the same team as your copy. Then it moves the old copy to the Trash and puts the new copy in its place. If a step fails, nothing changes, and after a replacement, Put Back restores the old copy.

Kipimo doesn't re-sign broken apps. A new signature made on your Mac would replace the developer's, which can break the app's keychain access, its entitlements and its updates.

### Updates

<p align="center">
  <img src="docs/images/tools-updates.png" width="820" alt="Updates: apps with a newer version available, where it was found, and its release date">
</p>

Updates checks each app's update feed (Sparkle, electron-updater and Visual Studio Code's) and the App Store for newer versions. Each result shows your version, the new version and its release date. Kipimo doesn't install updates. **Release Notes**, **Download Page** and **Open Updater** open the place where you update the app.

## Alerts and the weekly summary

<p align="center">
  <img src="docs/images/settings-alerts.png" width="520" alt="Settings, Alerts tab: startup disk, Trash, Bluetooth battery, external drives and leftovers">
</p>

Kipimo can notify you when:

- the startup disk has less than 20 GB free;
- the Trash grows past 2 GB;
- a Bluetooth device's battery drops below 20%;
- an external drive is connected or ejected;
- an app you moved to the Trash left files behind.

Each alert can be switched off and its limit changed in **Settings › Alerts**. An alert fires when a value crosses its limit, and not again until the value recovers.

Once a week (Sunday at 10:00 unless you change it), Kipimo sends a notification with how much space a cleanup would free and how many app updates are waiting.

## Install

Download [Kipimo-macOS-arm64.zip](https://github.com/piedol/kipimo/releases/latest/download/Kipimo-macOS-arm64.zip) from the [latest release](https://github.com/piedol/kipimo/releases/latest). It needs a Mac with Apple silicon and macOS 15 or later. Unzip it, drag **Kipimo** into Applications and open it. Kipimo is signed and notarized by Apple.

### Build from source

You need:

- a Mac with Apple silicon and macOS 15 or later;
- the Xcode Command Line Tools (`xcode-select --install`);
- Rust 1.98 or later, from [rustup](https://rustup.rs).

```bash
git clone https://github.com/piedol/kipimo.git
```

```bash
cd kipimo && scripts/bundle.sh --install
```

The script builds Kipimo, signs it, copies it to `/Applications` and opens it. It signs with `KIPIMO_SIGN_IDENTITY` if that's set, or else with the first Developer ID Application certificate in your keychain. Without either, it uses an ad-hoc signature, which works on your Mac, but macOS then forgets Kipimo's Full Disk Access after every rebuild.

### First launch

When Kipimo first opens:

1. Allow notifications when macOS asks. Alerts and the weekly summary use them.
2. If Settings › General says Kipimo can't read the Trash, click **Open Full Disk Access…** and turn Kipimo on. It needs this to measure the Trash and to notice apps you drag there.
3. To start Kipimo when you log in, turn on **Launch Kipimo at login** in Settings › General.

Kipimo has no Dock icon. To quit, open the panel and choose **Quit Kipimo** from the gear menu.

### Uninstall Kipimo

1. Turn off **Launch Kipimo at login** in Settings › General, then quit Kipimo from the gear menu.
2. Move `/Applications/Kipimo.app` to the Trash.
3. Move `~/Library/Caches/top.piedol.kipimo` to the Trash, and run `defaults delete top.piedol.kipimo` to remove its settings.
4. Remove Kipimo from System Settings › Privacy & Security › Full Disk Access, if you added it.

## Privacy and safety

- Kipimo moves files to the Trash and never deletes them. Emptying the Trash is left to you and Finder.
- It acts only on the items you tick, and asks before it moves anything. Cancel is the default button in every confirmation, so pressing Return never removes files.
- It never changes the contents of an app bundle or re-signs an app. Download and Replace swaps the whole app for its developer's copy, after checking that copy's signature.
- It runs as your user, without a privileged helper or administrator rights.
- It goes online only to check for updates and, when you ask, to download a replacement app. Update checks run at most once a day, when you click **Check Now**, and when you click **Reinstall…** for an app whose feed it hasn't checked in the last day. Each update feed is asked only about its own app. **Download and Replace…** downloads the app from the link in its feed, and only when you click it. App Store apps are looked up through Apple's public lookup service in batches of up to 40, so Apple sees which App Store apps you have. Kipimo sends no analytics.

## Resource use

Measured on a MacBook Pro with an M5 Max:

| Measure | Value |
|---|---|
| App size | 4.7 MB |
| Memory, panel closed | about 9 MB |
| Memory, panel open | about 14 MB |
| CPU, panel closed | about 0.03% |

The tools run in a separate process that quits when their window closes, so the menu bar app stays at about 9 MB.

## Troubleshooting

| Problem | Fix |
|---|---|
| The panel says Kipimo can't read the Trash | Click **Open Full Disk Access…** in Settings › General and turn Kipimo on. |
| Uninstall says macOS blocked the move | Allow Kipimo in System Settings › Privacy & Security › App Management. Kipimo offers to open that page. |
| No notifications arrive | Check System Settings › Notifications › Kipimo, then click **Send a Test Notification** in Settings › Alerts. |
| You removed something you need | Open the Trash in Finder, select the item and choose **Put Back**. |
| Battery watts or CPU temperature are missing | macOS didn't report them on this Mac. The rest of the panel keeps working. |

## Development

```text
crates/
├── app        the AppKit interface (objc2): panel, tools window, Settings, notifications
├── sys        readers: CPU, memory, disk, network, battery and power, temperature, Bluetooth, XProtect
├── clean      Clean up, Uninstall and the leftover finder
├── space      folder sizes for Space
├── startup    startup items, code signatures and the broken-signature check
└── updates    update feeds and App Store versions
```

The engine crates (`clean`, `space`, `startup` and `updates`) have no UI code, and each documents its API at the top of its `lib.rs`. Tests run against synthetic folders in a temporary directory and never touch your home folder.

Power and temperature come from private IOKit interfaces (IOReport and the SMC), so those two readings may stop working in a future version of macOS.

### Contributing

Bug reports and pull requests are welcome. Before opening a pull request, run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Credits

Kipimo calls Apple's frameworks through [objc2](https://github.com/madsmtm/objc2). It uses [ureq](https://github.com/algesten/ureq) with [rustls](https://github.com/rustls/rustls) for update checks, and [plist](https://github.com/ebarnard/rust-plist) and [quick-xml](https://github.com/tafia/quick-xml) to read app metadata and update feeds.

## License

Copyright 2026 piedol.

Kipimo is free software under the [GNU General Public License v3.0 or later](LICENSE). You can use, study, share and change it, including at work. If you distribute a modified version, you must release its source under the same license.
