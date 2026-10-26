//! Integration tests through the public API, on synthetic LaunchAgent folders in a temp dir.
//! Nothing here reads or changes the real LaunchAgents folders, launchd or the Trash
//! (`live_state: false`; only read-only LaunchServices and Security framework lookups run).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use kipimo_startup::{
    BtmSource, Flag, Inventory, ItemKind, Roots, ScanOptions, Scope, SignatureStatus, SignerKind,
    StartupItem, scan,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir =
            std::env::temp_dir().join(format!("kipimo-startup-it-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(std::fs::canonicalize(dir).unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn options(base: &Path) -> ScanOptions {
    ScanOptions {
        roots: Roots::synthetic(base),
        live_state: false,
        kernel_extensions: false,
        sfltool_fallback: false,
        btm_user_uuid: None,
    }
}

fn run(base: &Path) -> Inventory {
    scan(&options(base), &AtomicBool::new(false), |_, _| {})
}

fn xml_agent(label: &str, program: &str, extra: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array><string>{program}</string><string>--flag</string></array>
<key>RunAtLoad</key><true/>{extra}
</dict></plist>"#
    )
}

fn item<'a>(inv: &'a Inventory, label: &str) -> &'a StartupItem {
    inv.items
        .iter()
        .find(|i| i.label == label)
        .unwrap_or_else(|| {
            panic!(
                "no item {label}: {:?}",
                inv.items.iter().map(|i| &i.label).collect::<Vec<_>>()
            )
        })
}

#[test]
fn synthetic_launch_agents() {
    let tmp = TempDir::new("agents");
    let roots = Roots::synthetic(&tmp.0);
    let agents = &roots.user_agents;
    std::fs::create_dir_all(agents).unwrap();
    std::fs::create_dir_all(&roots.system_daemons).unwrap();

    // Valid agent running an Apple binary.
    std::fs::write(
        agents.join("com.example.valid.plist"),
        xml_agent("com.example.valid", "/bin/ls", ""),
    )
    .unwrap();
    // Malformed (truncated XML).
    std::fs::write(
        agents.join("com.example.malformed.plist"),
        "<plist><dict><key>Label</key><str",
    )
    .unwrap();
    // Binary plist, written in binary format.
    let mut dict = plist::Dictionary::new();
    dict.insert("Label".into(), "com.example.binary".into());
    dict.insert("Program".into(), "/bin/cat".into());
    dict.insert("Disabled".into(), true.into());
    plist::Value::Dictionary(dict)
        .to_file_binary(agents.join("com.example.binary.plist"))
        .unwrap();
    assert_eq!(
        &std::fs::read(agents.join("com.example.binary.plist")).unwrap()[..8],
        b"bplist00"
    );
    // Missing program.
    std::fs::write(
        agents.join("com.example.missing.plist"),
        xml_agent(
            "com.example.missing",
            "/Applications/Kipimo Test Removed.app/Contents/MacOS/helper",
            "",
        ),
    )
    .unwrap();
    // Symlinked program (valid) and a dangling symlink.
    let bin = tmp.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/ls", bin.join("ls-link")).unwrap();
    std::os::unix::fs::symlink(bin.join("nowhere"), bin.join("dangling")).unwrap();
    std::fs::write(
        agents.join("com.example.symlink.plist"),
        xml_agent(
            "com.example.symlink",
            bin.join("ls-link").to_str().unwrap(),
            "",
        ),
    )
    .unwrap();
    std::fs::write(
        agents.join("com.example.dangling.plist"),
        xml_agent(
            "com.example.dangling",
            bin.join("dangling").to_str().unwrap(),
            "",
        ),
    )
    .unwrap();
    // A symlinked plist is loaded by launchd too.
    let elsewhere = tmp.0.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(
        elsewhere.join("real.plist"),
        xml_agent("com.example.linkedplist", "/bin/ls", ""),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        elsewhere.join("real.plist"),
        agents.join("com.example.linkedplist.plist"),
    )
    .unwrap();
    // A script runner whose script is gone, and one owned by an app that isn't installed.
    std::fs::write(
        agents.join("com.example.script.plist"),
        xml_agent("com.example.script", "/bin/sh", ""),
    )
    .unwrap();
    std::fs::write(
        agents.join("com.example.script.plist"),
        r#"<plist version="1.0"><dict><key>Label</key><string>com.example.script</string>
<key>ProgramArguments</key><array><string>/bin/sh</string><string>/nonexistent/kipimo/run.sh</string></array></dict></plist>"#,
    )
    .unwrap();
    std::fs::write(
        agents.join("com.example.orphan.plist"),
        xml_agent(
            "com.example.orphan",
            "/bin/ls",
            "<key>AssociatedBundleIdentifiers</key><array><string>com.example.kipimo.never-installed</string></array>",
        ),
    )
    .unwrap();
    // Ignored: other files, hidden files, folders.
    std::fs::write(agents.join("README.txt"), "not a plist").unwrap();
    std::fs::write(
        agents.join(".hidden.plist"),
        xml_agent("com.example.hidden", "/bin/ls", ""),
    )
    .unwrap();
    std::fs::create_dir_all(agents.join("folder.plist")).unwrap();
    // A daemon in the system folder.
    std::fs::write(
        roots.system_daemons.join("com.example.daemon.plist"),
        xml_agent("com.example.daemon", "/usr/sbin/cron", ""),
    )
    .unwrap();

    let mut progress = Vec::new();
    let inv = scan(&options(&tmp.0), &AtomicBool::new(false), |p, i| {
        progress.push((p.done, p.total, i.label.clone()))
    });
    assert_eq!(
        inv.items.len(),
        10,
        "{:?}",
        inv.items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
    assert_eq!(progress.len(), 10);
    assert!(progress.iter().all(|(_, total, _)| *total == 10));
    assert_eq!(
        inv.btm_source,
        BtmSource::Unavailable("not requested".into())
    );
    assert!(!inv.cancelled);

    let valid = item(&inv, "com.example.valid");
    assert_eq!(
        (valid.kind, valid.scope),
        (ItemKind::LaunchAgent, Scope::User)
    );
    assert_eq!(valid.program.as_deref(), Some(Path::new("/bin/ls")));
    assert_eq!(valid.arguments, vec!["/bin/ls", "--flag"]);
    assert_eq!(valid.state.enabled, Some(true));
    assert_eq!(valid.state.loaded, None, "live state is off");
    let sig = valid.signature.as_ref().unwrap();
    assert_eq!(
        (sig.status, sig.kind),
        (SignatureStatus::Valid, SignerKind::Apple)
    );
    assert!(valid.flags.is_empty());
    assert!(valid.is_apple() && !valid.needs_attention());

    let malformed = item(&inv, "com.example.malformed");
    assert!(malformed.flags.contains(&Flag::MalformedPlist));
    assert_eq!(malformed.state.enabled, None);
    assert!(
        malformed
            .state
            .detail
            .as_deref()
            .unwrap()
            .contains("parsed")
    );

    let binary = item(&inv, "com.example.binary");
    assert_eq!(binary.program.as_deref(), Some(Path::new("/bin/cat")));
    assert!(binary.state.disabled_key);
    assert_eq!(binary.state.enabled, Some(false));

    let missing = item(&inv, "com.example.missing");
    assert!(missing.flags.contains(&Flag::ProgramMissing));
    assert!(missing.flags.contains(&Flag::OwnerAppMissing));
    assert!(missing.is_orphaned());
    assert!(missing.signature.is_none());
    assert_eq!(missing.owner.as_ref().unwrap().name, "Kipimo Test Removed");

    let symlink = item(&inv, "com.example.symlink");
    assert!(symlink.flags.is_empty(), "{:?}", symlink.flags);
    assert_eq!(
        symlink.signature.as_ref().unwrap().path,
        PathBuf::from("/bin/ls")
    );
    assert_eq!(symlink.signature.as_ref().unwrap().kind, SignerKind::Apple);

    let dangling = item(&inv, "com.example.dangling");
    assert!(dangling.flags.contains(&Flag::ProgramMissing));

    let linked = item(&inv, "com.example.linkedplist");
    assert_eq!(
        linked.plist_path.as_deref(),
        Some(agents.join("com.example.linkedplist.plist").as_path())
    );

    let script = item(&inv, "com.example.script");
    assert_eq!(
        script.script.as_deref(),
        Some(Path::new("/nonexistent/kipimo/run.sh"))
    );
    assert!(
        script.flags.contains(&Flag::RunsScript) && script.flags.contains(&Flag::ScriptMissing)
    );

    let orphan = item(&inv, "com.example.orphan");
    assert!(orphan.flags.contains(&Flag::OwnerAppMissing));
    assert_eq!(
        orphan.owner.as_ref().unwrap().bundle_id.as_deref(),
        Some("com.example.kipimo.never-installed")
    );

    let daemon = item(&inv, "com.example.daemon");
    assert_eq!(
        (daemon.kind, daemon.scope),
        (ItemKind::LaunchDaemon, Scope::System)
    );

    // Plain data: serializes for the UI or a log.
    let json = serde_json::to_string(&inv).unwrap();
    assert!(json.contains("com.example.binary"));
}

#[test]
fn adhoc_signed_program_is_flagged() {
    let tmp = TempDir::new("adhoc");
    let roots = Roots::synthetic(&tmp.0);
    std::fs::create_dir_all(&roots.user_agents).unwrap();
    let src = tmp.0.join("tool.c");
    std::fs::write(&src, "int main(void) { return 0; }\n").unwrap();
    let exe = tmp.0.join("tool");
    let built = Command::new("/usr/bin/cc")
        .arg("-o")
        .arg(&exe)
        .arg(&src)
        .status();
    if !built.is_ok_and(|s| s.success()) {
        eprintln!("skipping: no C compiler");
        return;
    }
    // The linker already ad-hoc signs arm64 output; sign explicitly for x86_64 as well.
    let signed = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(&exe)
        .status();
    assert!(signed.unwrap().success());
    std::fs::write(
        roots.user_agents.join("com.example.adhoc.plist"),
        xml_agent("com.example.adhoc", exe.to_str().unwrap(), ""),
    )
    .unwrap();
    let inv = run(&tmp.0);
    let adhoc = item(&inv, "com.example.adhoc");
    let sig = adhoc.signature.as_ref().unwrap();
    assert_eq!(
        (sig.status, sig.kind),
        (SignatureStatus::Valid, SignerKind::AdHoc)
    );
    assert_eq!(adhoc.flags, vec![Flag::AdHocSigned]);
    assert!(adhoc.needs_attention());
}

#[test]
fn missing_folders_are_empty_not_errors() {
    let tmp = TempDir::new("empty");
    let inv = run(&tmp.0);
    assert!(inv.items.is_empty());
    assert!(inv.warnings.is_empty(), "{:?}", inv.warnings);
}
