//! `rochcpu setup`: stop asking for the password at every launch.
//!
//! Without it the window starts its root helper through `pkexec`, and the desktop asks for a
//! password each time, as UAC does on Windows. Setup puts a root-owned copy of this program at
//! [`INSTALLED`] and a polkit rule that lets the person who ran it start exactly that copy, with
//! exactly the helper argument, from their own active session, without a password. A copy in
//! `~/.local/bin` could not be trusted that way: anything running as you can replace a file there,
//! so a rule naming it would hand out root. Nobody but root can change the copy here.
//!
//! The trade: anything running as you can then start the helper silently and do what it allows
//! (see `direct`'s allowlists): change CPU ratios, voltages and power limits, and the DIMM rails.
//! The window uses the installed copy only while it is this same build, so after an update it asks
//! again until setup is run again.

use std::ffi::CStr;
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
use std::path::{Path, PathBuf};

use super::HELPER_ARG;

/// The root-owned copy setup installs.
pub const INSTALLED: &str = "/usr/local/bin/rochcpu";
/// The polkit rule that lets its owner start [`INSTALLED`] as the helper without a password.
pub const RULE_FILE: &str = "/etc/polkit-1/rules.d/49-rochcpu.rules";

const USAGE: &str = "\
rochcpu setup            stop asking for the password (run it with sudo)
rochcpu setup --status   say whether it is set up, and whether this build will use it
rochcpu setup --show     what setup would change, without changing it
rochcpu setup --undo     remove it (with sudo), so every launch asks again
";

/// The program to hand `pkexec`: the installed copy while it is this very build, otherwise this
/// executable, which brings the password prompt back rather than run an older helper.
pub fn helper_program() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(if installed_matches(&exe) {
        PathBuf::from(INSTALLED)
    } else {
        exe
    })
}

/// Root-owned, writable by nobody else, and byte for byte the running program.
fn installed_matches(exe: &Path) -> bool {
    let installed = Path::new(INSTALLED);
    let Ok(meta) = std::fs::metadata(installed) else {
        return false;
    };
    if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return false;
    }
    if exe == installed {
        return true;
    }
    let Ok(own) = std::fs::metadata(exe) else {
        return false;
    };
    own.len() == meta.len()
        && matches!((std::fs::read(exe), std::fs::read(installed)), (Ok(a), Ok(b)) if a == b)
}

pub fn run(arguments: &[String]) -> i32 {
    match arguments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => install(),
        ["--status"] => status(),
        ["--show"] => show(),
        ["--undo"] => undo(),
        _ => {
            eprint!("{USAGE}");
            2
        }
    }
}

fn exe_text() -> String {
    std::env::current_exe()
        .map(|e| e.display().to_string())
        .unwrap_or_else(|_| "rochcpu".into())
}

fn require_root(verb: &str) -> bool {
    if crate::hw::native::is_root() {
        return true;
    }
    // sudo searches only its own short path, which a home install is not on.
    eprintln!("That needs root. Run: sudo {} setup{verb}", exe_text());
    false
}

/// Who asked: the user behind sudo or pkexec.
fn invoking_user() -> Option<String> {
    if let Ok(name) = std::env::var("SUDO_USER")
        && !name.is_empty()
        && name != "root"
    {
        return Some(name);
    }
    let uid: u32 = std::env::var("PKEXEC_UID").ok()?.parse().ok()?;
    // SAFETY: getpwuid returns static storage or null; the name is copied out before anything
    // else here could call it again.
    unsafe {
        let entry = libc::getpwuid(uid);
        (!entry.is_null()).then(|| {
            CStr::from_ptr((*entry).pw_name)
                .to_string_lossy()
                .into_owned()
        })
    }
}

/// A user name as polkit sees it, checked so it can go into the rule as a plain string.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn rule_text(user: &str) -> String {
    format!(
        "// Written by `rochcpu setup` for {user}; `sudo rochcpu setup --undo` removes it.\n\
         // Lets {user} start Roch CPU's root helper without a password: only the root-owned copy at\n\
         // {INSTALLED}, only with {HELPER_ARG}, and only from an active local session.\n\
         polkit.addRule(function (action, subject) {{\n\
         \x20   if (action.id == \"org.freedesktop.policykit.exec\" &&\n\
         \x20       action.lookup(\"program\") == \"{INSTALLED}\" &&\n\
         \x20       action.lookup(\"command_line\") == \"{INSTALLED} {HELPER_ARG}\" &&\n\
         \x20       subject.user == \"{user}\" && subject.local && subject.active) {{\n\
         \x20       return polkit.Result.YES;\n\
         \x20   }}\n\
         }});\n"
    )
}

fn show() -> i32 {
    println!("Setup would:");
    println!("  copy this program to {INSTALLED}, owned by root and writable only by root");
    println!(
        "  write {RULE_FILE}, letting you start that copy as the helper ({HELPER_ARG}) without a password"
    );
    println!("Anything running as you could then change CPU and DIMM settings without asking.");
    0
}

fn install() -> i32 {
    if !require_root("") {
        return 1;
    }
    let Some(user) = invoking_user().filter(|u| plain_name(u)) else {
        eprintln!(
            "Could not tell who this is for: run it through sudo or pkexec rather than as root directly."
        );
        return 1;
    };
    match install_files(&user) {
        Ok(()) => {
            println!(
                "Installed {INSTALLED} and {RULE_FILE}. Roch CPU will no longer ask {user} for a password."
            );
            println!(
                "Run it again after updating Roch CPU; `sudo rochcpu setup --undo` removes it."
            );
            0
        }
        Err(error) => {
            eprintln!("Setup failed: {error}");
            1
        }
    }
}

/// Writes `bytes` to `target` through a temporary name in the same folder, root-owned with
/// `mode`, so what is there is always whole.
fn put(target: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let folder = target.parent().ok_or("no folder")?;
    std::fs::create_dir_all(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let temporary = folder.join(format!(".{name}.new"));
    let _ = std::fs::remove_file(&temporary);
    let result = (|| {
        std::fs::write(&temporary, bytes).map_err(|e| format!("{}: {e}", temporary.display()))?;
        chown(&temporary, Some(0), Some(0)).map_err(|e| format!("chown: {e}"))?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod: {e}"))?;
        std::fs::rename(&temporary, target).map_err(|e| format!("{}: {e}", target.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn install_files(user: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("could not find this program: {e}"))?;
    let program = std::fs::read(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    put(Path::new(INSTALLED), &program, 0o755)?;
    put(Path::new(RULE_FILE), rule_text(user).as_bytes(), 0o644)
}

fn status() -> i32 {
    let installed = Path::new(INSTALLED).exists();
    // The rules folder is readable only by root and polkit, so as yourself the rule cannot be seen.
    let rule = match std::fs::metadata(RULE_FILE) {
        Ok(_) => Some(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    };
    match (installed, rule) {
        (false, Some(false)) => println!("Not set up: it asks for a password each time."),
        (true, Some(true)) => println!("Set up: {INSTALLED} and {RULE_FILE} are in place."),
        (true, None) => println!("Set up: {INSTALLED} is in place ({RULE_FILE} is readable only by root)."),
        (false, _) => println!("Not set up: {INSTALLED} is missing. Run `sudo rochcpu setup`."),
        (true, Some(false)) => println!("Partly set up: {RULE_FILE} is missing. Run `sudo rochcpu setup` again."),
    }
    if installed {
        let exe = std::env::current_exe().unwrap_or_default();
        if installed_matches(&exe) {
            println!("This build matches the installed copy, so it starts without asking.");
        } else {
            println!(
                "This build differs from the installed copy, so it asks. Run `sudo {} setup` to update it.",
                exe.display()
            );
        }
    }
    0
}

fn undo() -> i32 {
    if !require_root(" --undo") {
        return 1;
    }
    let mut failed = false;
    for path in [RULE_FILE, INSTALLED] {
        match std::fs::remove_file(path) {
            Ok(()) => println!("Removed {path}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("{path}: {e}");
                failed = true;
            }
        }
    }
    i32::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rule_names_only_the_installed_helper_for_one_user() {
        let rule = rule_text("local");
        assert!(rule.contains(
            "action.lookup(\"command_line\") == \"/usr/local/bin/rochcpu --hardware-helper\""
        ));
        assert!(rule.contains("subject.user == \"local\" && subject.local && subject.active"));
        assert!(!rule.contains(".local/bin"));
    }

    #[test]
    fn only_plain_user_names_go_into_the_rule() {
        assert!(plain_name("local") && plain_name("first.last") && plain_name("a_b-1"));
        assert!(!plain_name("") && !plain_name("x\" || true || \"") && !plain_name("a b"));
    }
}
