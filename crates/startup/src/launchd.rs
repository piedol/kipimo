//! Reading launchd plists (agents and daemons) from disk. Pure file parsing, no launchd calls,
//! so it's fully testable on synthetic folders.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use plist::Value;

/// launchd's search path for a relative `ProgramArguments[0]` (launchd.plist(5)).
const LAUNCHD_PATH: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// Plists larger than this aren't launchd jobs; refuse to read them into memory.
const MAX_PLIST_BYTES: u64 = 4 * 1024 * 1024;

/// One launchd job definition read from a plist file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct JobPlist {
    pub path: PathBuf,
    pub label: Option<String>,
    /// `Program`, or `ProgramArguments[0]` resolved against launchd's PATH.
    pub program: Option<PathBuf>,
    pub arguments: Vec<String>,
    /// `Disabled = true` in the plist.
    pub disabled: bool,
    /// `AssociatedBundleIdentifiers` (macOS 13+): the app(s) this job belongs to.
    pub associated_bundle_ids: Vec<String>,
    /// `BundleProgram`: a program path relative to the app bundle that registered the job.
    pub bundle_program: Option<String>,
    /// Why the plist is unusable (unparseable, not a dictionary, no Label, no program).
    pub problem: Option<String>,
}

/// Lists the `.plist` files directly inside `dir` (launchd ignores other files and
/// subfolders). Returns them sorted by file name. A missing folder is an empty list.
pub(crate) fn list_plists(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !name.to_ascii_lowercase().ends_with(".plist") {
            continue;
        }
        // Follow symlinks: a symlinked plist is still loaded by launchd. Skip folders.
        if fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
            continue;
        }
        out.push(path);
    }
    out.sort();
    Ok(out)
}

/// Reads and parses one job plist (XML, binary or old ASCII format).
pub(crate) fn read_job(path: &Path) -> JobPlist {
    let mut job = JobPlist {
        path: path.to_path_buf(),
        ..JobPlist::default()
    };
    let bytes = match fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_PLIST_BYTES => {
            job.problem = Some(format!("plist is too large ({} bytes)", meta.len()));
            return job;
        }
        Ok(_) => match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) => {
                job.problem = Some(format!("can't read plist: {e}"));
                return job;
            }
        },
        Err(e) => {
            job.problem = Some(format!("can't read plist: {e}"));
            return job;
        }
    };
    parse_job_into(&mut job, &bytes);
    job
}

#[cfg(test)]
pub(crate) fn parse_job(path: &Path, bytes: &[u8]) -> JobPlist {
    let mut job = JobPlist {
        path: path.to_path_buf(),
        ..JobPlist::default()
    };
    parse_job_into(&mut job, bytes);
    job
}

fn parse_job_into(job: &mut JobPlist, bytes: &[u8]) {
    let value = match Value::from_reader(Cursor::new(bytes)) {
        Ok(v) => v,
        Err(e) => {
            job.problem = Some(format!("plist can't be parsed: {e}"));
            return;
        }
    };
    let Some(dict) = value.as_dictionary() else {
        job.problem = Some("plist is not a dictionary".into());
        return;
    };
    job.label = dict
        .get("Label")
        .and_then(Value::as_string)
        .map(str::to_owned);
    job.disabled = dict
        .get("Disabled")
        .and_then(Value::as_boolean)
        .unwrap_or(false);
    job.arguments = dict
        .get("ProgramArguments")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_string)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    job.associated_bundle_ids = match dict.get("AssociatedBundleIdentifiers") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_string)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    job.bundle_program = dict
        .get("BundleProgram")
        .and_then(Value::as_string)
        .map(str::to_owned);
    let program = dict
        .get("Program")
        .and_then(Value::as_string)
        .map(str::to_owned)
        .or_else(|| job.arguments.first().cloned());
    job.program = program
        .filter(|p| !p.is_empty())
        .map(|p| resolve_program(&p));

    if job.label.as_deref().is_none_or(str::is_empty) {
        job.problem = Some("plist has no Label".into());
    } else if job.program.is_none() && job.bundle_program.is_none() {
        job.problem = Some("plist has no Program or ProgramArguments".into());
    }
}

/// launchd runs a relative `ProgramArguments[0]` through its PATH; resolve it the same way
/// (to the first existing match, else `/usr/bin/<name>`).
fn resolve_program(program: &str) -> PathBuf {
    if program.starts_with('/') || program.contains('/') {
        return PathBuf::from(program);
    }
    LAUNCHD_PATH
        .iter()
        .map(|dir| Path::new(dir).join(program))
        .find(|p| p.exists())
        .unwrap_or_else(|| Path::new(LAUNCHD_PATH[0]).join(program))
}

const SHELLS: [&str; 8] = ["sh", "bash", "zsh", "dash", "ksh", "csh", "tcsh", "fish"];

fn is_shell(name: &str) -> bool {
    SHELLS.contains(&name)
}

/// Interpreters whose first non-option argument is the script they run.
fn is_interpreter(name: &str) -> bool {
    const LANGS: [&str; 6] = ["python", "ruby", "perl", "node", "php", "deno"];
    is_shell(name)
        || name == "osascript"
        || name == "env"
        || LANGS.iter().any(|lang| {
            name.strip_prefix(lang)
                .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
        })
}

/// When the program is an interpreter (`/bin/sh`, `/usr/bin/python3`, `node`, `env python3`),
/// the script (or, for `sh -c "cmd ..."`, the command) it runs.
pub(crate) fn script_target(program: &Path, arguments: &[String]) -> Option<PathBuf> {
    let name = program.file_name()?.to_str()?;
    if !is_interpreter(name) {
        return None;
    }
    // ProgramArguments[0] is the program itself when it's listed there.
    let mut args = arguments.iter().peekable();
    if args
        .peek()
        .is_some_and(|first| Path::new(first.as_str()).file_name() == program.file_name())
    {
        args.next();
    }
    let mut is_env = name == "env";
    while let Some(arg) = args.next() {
        if arg == "-c" && !is_shell(name) && !is_env {
            return None;
        }
        if arg == "-c" {
            // `sh -c "/path/to/tool --flag"`: the first word of the command.
            let command = args.next()?;
            let first = command
                .split_whitespace()
                .next()?
                .trim_matches(|c| c == '"' || c == '\'');
            return first.starts_with('/').then(|| PathBuf::from(first));
        }
        // Inline code (`node -e`, `perl -e`, `python -c`, `python -m module`) has no script
        // file. For shells `-e` is just "exit on error".
        if !is_shell(name) && !is_env && matches!(arg.as_str(), "-e" | "-m" | "--eval") {
            return None;
        }
        if arg.starts_with('-') {
            continue;
        }
        if is_env {
            // `env [VAR=value ...] python3 script.py`
            if arg.contains('=') {
                continue;
            }
            is_env = false;
            if is_interpreter(Path::new(arg.as_str()).file_name()?.to_str()?) {
                continue;
            }
            return Some(PathBuf::from(arg));
        }
        return Some(PathBuf::from(arg));
    }
    None
}

/// The outermost `.app` bundle containing `path`, if any
/// (`/Applications/zoom.us.app/Contents/Library/LaunchAgents/ZoomUpdater.app/...` →
/// `/Applications/zoom.us.app`).
pub(crate) fn enclosing_app(path: &Path) -> Option<PathBuf> {
    let mut found = None;
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        if current
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("app"))
        {
            found = Some(current.clone());
            break;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn interpreter_names() {
        for name in [
            "sh",
            "bash",
            "zsh",
            "python3",
            "python3.12",
            "node",
            "perl5.34",
            "osascript",
        ] {
            assert!(is_interpreter(name), "{name}");
        }
        for name in ["nodejs-helper", "pythonista", "JumpConnect", "launchctl"] {
            assert!(!is_interpreter(name), "{name}");
        }
    }

    #[test]
    fn script_targets() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            script_target(
                &p("/bin/bash"),
                &args(&["/bin/bash", "/Users/me/x.sh", "sweep"])
            ),
            Some(p("/Users/me/x.sh"))
        );
        assert_eq!(
            script_target(
                &p("/bin/sh"),
                &args(&["sh", "-c", "\"/Applications/A.app/Contents/MacOS/a\" --x"])
            ),
            Some(p("/Applications/A.app/Contents/MacOS/a"))
        );
        assert_eq!(
            script_target(
                &p("/usr/bin/env"),
                &args(&["/usr/bin/env", "FOO=1", "python3", "-u", "/opt/s.py"])
            ),
            Some(p("/opt/s.py"))
        );
        assert_eq!(
            script_target(
                &p("/usr/bin/osascript"),
                &args(&["osascript", "-e", "beep"])
            ),
            None
        );
        assert_eq!(
            script_target(
                &p("/usr/bin/python3"),
                &args(&["python3", "-c", "print(1)"])
            ),
            None
        );
        assert_eq!(
            script_target(&p("/usr/local/bin/node"), &args(&["node", "-e", "1"])),
            None
        );
        assert_eq!(
            script_target(&p("/bin/bash"), &args(&["bash", "-e", "/opt/run.sh"])),
            Some(p("/opt/run.sh"))
        );
        assert_eq!(
            script_target(
                &p("/Applications/A.app/Contents/MacOS/a"),
                &args(&["a", "/tmp/x"])
            ),
            None
        );
        // Node from nvm, as in the owner's own agent.
        assert_eq!(
            script_target(
                &p("/Users/me/.nvm/versions/node/v24.16.0/bin/node"),
                &args(&[
                    "/Users/me/.nvm/versions/node/v24.16.0/bin/node",
                    "/Users/me/run.js",
                    "--scheduled"
                ])
            ),
            Some(p("/Users/me/run.js"))
        );
    }

    #[test]
    fn outermost_app() {
        assert_eq!(
            enclosing_app(&p(
                "/Applications/zoom.us.app/Contents/Library/LaunchAgents/ZoomUpdater.app/Contents/MacOS/ZoomUpdater"
            )),
            Some(p("/Applications/zoom.us.app"))
        );
        assert_eq!(enclosing_app(&p("/Library/PrivilegedHelperTools/x")), None);
    }

    #[test]
    fn relative_program_uses_launchd_path() {
        assert_eq!(resolve_program("sh"), p("/bin/sh"));
        assert_eq!(
            resolve_program("no-such-tool-kipimo"),
            p("/usr/bin/no-such-tool-kipimo")
        );
        assert_eq!(resolve_program("/opt/x"), p("/opt/x"));
    }

    #[test]
    fn parse_variants() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.example.agent</string>
<key>ProgramArguments</key><array><string>/opt/example/agent</string><string>--daemon</string></array>
<key>AssociatedBundleIdentifiers</key><string>com.example.app</string>
<key>Disabled</key><true/>
</dict></plist>"#;
        let job = parse_job(&p("/x.plist"), xml);
        assert_eq!(job.problem, None);
        assert_eq!(job.label.as_deref(), Some("com.example.agent"));
        assert_eq!(job.program, Some(p("/opt/example/agent")));
        assert_eq!(job.arguments, vec!["/opt/example/agent", "--daemon"]);
        assert_eq!(job.associated_bundle_ids, vec!["com.example.app"]);
        assert!(job.disabled);

        let ascii = br#"{ Label = "com.example.ascii"; Program = "/opt/ascii"; }"#;
        let job = parse_job(&p("/a.plist"), ascii);
        assert_eq!(job.problem, None, "{:?}", job.problem);
        assert_eq!(job.program, Some(p("/opt/ascii")));

        let job = parse_job(&p("/b.plist"), b"<plist><dict><key>Label</key>");
        assert!(job.problem.unwrap().contains("parsed"));

        let job = parse_job(&p("/c.plist"), br#"<plist version="1.0"><array/></plist>"#);
        assert_eq!(job.problem.as_deref(), Some("plist is not a dictionary"));

        let job = parse_job(
            &p("/d.plist"),
            br#"<plist version="1.0"><dict><key>Program</key><string>/x</string></dict></plist>"#,
        );
        assert_eq!(job.problem.as_deref(), Some("plist has no Label"));

        let job = parse_job(
            &p("/e.plist"),
            br#"<plist version="1.0"><dict><key>Label</key><string>x</string></dict></plist>"#,
        );
        assert_eq!(
            job.problem.as_deref(),
            Some("plist has no Program or ProgramArguments")
        );
    }
}
