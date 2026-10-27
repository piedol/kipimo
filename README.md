<p align="center">
  <img src="assets/logo.png" width="128" alt="Kipimo icon">
</p>

<h1 align="center">Kipimo</h1>

<p align="center">
  A system monitor and cleanup toolkit for the Mac menu bar.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/macOS-15%2B%20·%20Apple%20silicon-1c1c1e?logo=apple" alt="macOS 15 or later on Apple silicon">
  <a href="LICENSE.md"><img src="https://img.shields.io/badge/license-PolyForm%20Noncommercial-1c1c1e" alt="License: PolyForm Noncommercial 1.0.0"></a>
</p>

<p align="center">
  <img src="docs/images/showcase.png" width="880" alt="Kipimo's menu bar panel next to its Uninstall window">
</p>

Kipimo is a menu bar app for keeping an eye on your Mac and tidying it up. Its panel shows free disk space, memory pressure, battery charge and power draw, CPU load and temperature, network speed, and the batteries of your Bluetooth devices. Its tools clear out caches and logs, uninstall apps together with the files they leave behind, show what is filling your disk, list everything that starts automatically, and check your apps for updates.

Everything Kipimo removes goes to the Trash, where Finder's **Put Back** can restore it. The app is written in Rust against Apple's own frameworks; it takes 4.7 MB on disk and about 9 MB of memory while it waits in the menu bar.

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
| Mac Health | An overall status, with your Mac's model and memory |
| Protection | Whether XProtect, the malware protection built into macOS, is on, and when its definitions were updated |
| Startup disk | Free space, with a **Clean up** button |
| Memory | Memory pressure; hover for used, compressed and swap |
| Battery | Charge, charging state and the power the Mac is drawing, in watts |
| CPU | Load, chip temperature and the busiest app |
| Network | Download and upload speed |
| Devices | Battery levels of AirPods, keyboards, mice and other Bluetooth devices |
| Today's recommendation | One suggestion, such as a cleanup or waiting app updates; **Later** hides it until tomorrow |

The panel refreshes every 2 seconds while it's open (every second, if you choose that in Settings). When it's closed, Kipimo checks once a minute and colours the menu bar icon if something needs attention.

**Open Kipimo** opens the tools. The gear opens a menu with Settings and Quit.

<br clear="right">

## Tools

### Clean up

<p align="center">
  <img src="docs/images/tools-clean.png" width="820" alt="Clean up: categories of caches, logs and developer files, each with a risk label and a size">
</p>

Clean up finds files that rebuild themselves when they're needed: app and macOS caches, logs and crash reports, Xcode build data, old device symbols and simulators, and package manager caches (Homebrew, npm, Yarn, pnpm, Cargo, pip, Gradle, CocoaPods and Go). It also lists old Xcode archives, Mail's copies of attachments you've opened, and iPhone and iPad backups older than 90 days. Backups never start ticked, since one may be the only copy of a device's data.

Each category shows its size and a risk label, and expands to list every item. Items that would be slow to download again start unticked with a note saying why; these include browsers used for automated testing, AI models, and any single item over 1 GB. Caches that a running app is using are skipped.

**Clean** moves the ticked items to the Trash. **Empty Trash…** asks Finder to empty it, with Finder's own confirmation.

### Uninstall

<p align="center">
  <img src="docs/images/tools-uninstall.png" width="820" alt="Uninstall: installed apps with their size and last use, and the files one app keeps in the Library">
</p>

Pick an app to see everything it stores on your Mac: preferences, caches, saved window state, containers and more. The list sorts by name, size or last use.

An app's preferences and caches start ticked. Its data, such as Application Support folders and containers, starts unticked, so documents and message history aren't removed by accident. If the app or one of its background helpers is running, Kipimo says so and offers to stop it. Apps that come with macOS can't be removed.

When you drag an app to the Trash yourself, Kipimo notices. About 30 seconds later it sends a notification about the files the app left behind, and you can review them and move the ones you choose to the Trash as well.

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
  <img src="docs/images/tools-startup.png" width="820" alt="Startup items: launch agents, launch daemons and a system extension, each with its signer; a launch agent with an invalid signature is selected">
</p>

Startup items lists everything set to start on its own: apps that open at login, items allowed to run in the background, and launch agents and daemons. Each entry shows who signed it, whether it's running, and the program it runs. Items that are unsigned, signed only for this Mac, or that point at a missing file are flagged.

- **Disable and Move to Trash…** stops one of your launch agents and moves its file to the Trash. Put Back restores the file, and the item starts again at your next login.
- Login items and background items are managed in System Settings, and Kipimo opens the right page.
- Items installed for every user of the Mac need an administrator. Kipimo shows the Terminal commands to run rather than asking for your password itself.
- **Check Apps for Broken Signatures** verifies every app in Applications and lists the ones whose code signature no longer holds, so you know what to reinstall.

### Updates

<p align="center">
  <img src="docs/images/tools-updates.png" width="820" alt="Updates: apps with a newer version available, where it was found, and its release date">
</p>

Updates checks each app's own update feed (Sparkle, electron-updater and Visual Studio Code's) and the App Store for newer versions. Each result shows your version, the new version and its release date. Kipimo doesn't install anything; **Release Notes**, **Download Page** and **Open Updater** take you to the place to update the app yourself.

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

Each alert can be switched off and its limit changed in **Settings › Alerts**. An alert fires once when a value crosses its limit, and not again until the value has recovered.

Once a week (Sunday at 10:00 unless you change it), Kipimo sends one notification saying how much space a cleanup would free and how many app updates are waiting.

## Install

There's no prebuilt download yet, so Kipimo is built from source. You need:

- a Mac with Apple silicon and macOS 15 or later;
- the Xcode Command Line Tools (`xcode-select --install`);
- Rust 1.98 or later, from [rustup](https://rustup.rs).

```bash
git clone https://github.com/piedol/kipimo.git
```

```bash
cd kipimo && scripts/bundle.sh --install
```

The script builds Kipimo, signs it, copies it to `/Applications` and opens it. It signs with `KIPIMO_SIGN_IDENTITY` if that's set, or else with the first Developer ID Application certificate in your keychain. Without either, it uses an ad-hoc signature, which runs on your own Mac, but macOS then forgets Kipimo's Full Disk Access after every rebuild.

When Kipimo first opens:

1. Allow notifications when macOS asks, so alerts and the weekly summary can reach you.
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
- It never changes the contents of an app bundle.
- It runs as your user, with no helper tool, background service or administrator rights of its own.
- It goes online only to check for updates: once a day at most, and when you click **Check Now**. Each app's own update feed is asked about that app only. App Store apps are looked up through Apple's public lookup service in batches of up to 40, so Apple sees which App Store apps you have. There's no account and no analytics.

## Resource use

Measured on a MacBook Pro with an M5 Max:

| Measure | Value |
|---|---|
| App size | 4.7 MB |
| Memory, panel closed | about 9 MB |
| Memory, panel open | about 14 MB |
| CPU, panel closed | about 0.03% |

The tools run in a separate process while their window is open and quit when it closes, which keeps the menu bar app small.

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

Kipimo is source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE.md). You may use, change and share it for any noncommercial purpose. Commercial use needs a separate license; open an issue to ask about one. Anyone who shares Kipimo or a fork must include the license and its Required Notice.
