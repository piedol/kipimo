//! Test helpers: temp folders and synthetic signed bundles. Tests never touch the real home
//! folder; everything lives under the system temp dir and is removed afterwards.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kipimo-startup-test-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Canonical (/private/var/...), so path comparisons with canonicalized paths hold.
        TempDir(std::fs::canonicalize(&dir).unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Test scratch only (under the temp dir), never user data.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The system C compiler, used to build test binaries (the linker ad-hoc signs them).
pub(crate) fn compiler() -> Option<PathBuf> {
    let cc = PathBuf::from("/usr/bin/cc");
    Command::new(&cc)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| cc)
}

/// Compiles a trivial program to `out`.
pub(crate) fn build_binary(cc: &Path, out: &Path) {
    let src = out.with_extension("c");
    std::fs::write(&src, "int main(void) { return 0; }\n").unwrap();
    let status = Command::new(cc)
        .arg("-o")
        .arg(out)
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success(), "cc failed");
    std::fs::remove_file(src).unwrap();
}

/// Development-time signing tool, only to *create* fixtures (checks use the Security
/// framework).
pub(crate) fn codesign(args: &[&str], path: &Path) {
    let status = Command::new("/usr/bin/codesign")
        .args(args)
        .arg(path)
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "codesign {args:?} failed");
}

/// Builds `<dir>/<name>.app` with a compiled executable, a data file and two localizations;
/// ad-hoc signs
/// the bundle (sealing its resources) when `sign`, else leaves it unsigned.
pub(crate) fn adhoc_app(cc: &Path, dir: &Path, name: &str, sign: bool) -> PathBuf {
    let app = dir.join(format!("{name}.app"));
    let contents = app.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    for lang in ["en", "de"] {
        let lproj = contents.join(format!("Resources/{lang}.lproj"));
        std::fs::create_dir_all(&lproj).unwrap();
        std::fs::write(
            lproj.join("Localizable.strings"),
            format!("\"hello\" = \"{lang}\";\n"),
        )
        .unwrap();
    }
    std::fs::write(
        contents.join("Resources/data.json"),
        b"{\"sealed\": true}\n",
    )
    .unwrap();
    std::fs::write(
        contents.join("Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{name}</string>
<key>CFBundleIdentifier</key><string>com.example.kipimo-test.{name}</string>
<key>CFBundleName</key><string>{name}</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#
        ),
    )
    .unwrap();
    let exe = contents.join("MacOS").join(name);
    build_binary(cc, &exe);
    if sign {
        codesign(&["--force", "--sign", "-"], &app);
    } else {
        codesign(&["--remove-signature"], &exe);
    }
    app
}
