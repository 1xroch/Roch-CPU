//! `rochcpu setup`: stop asking for the password at every launch.
//!
//! Without it the window starts its root helper through `pkexec`, and the desktop asks for a
//! password each time, as UAC does on Windows. Setup writes a polkit rule that lets the person who
//! ran it start exactly one program, with exactly the helper argument, from their own active
//! session, without a password. The program has to be one nobody but root can change, or the rule
//! would hand out root:
//!
//! * installed by a package (`/usr/bin/rochcpu`): it already is, so the rule names it, and an
//!   update that replaces it in place keeps working with no setup to repeat;
//! * anywhere else (`~/.local/bin`, where anything running as you could replace it): setup copies
//!   it to [`INSTALLED`], root-owned, and the rule names the copy. The window uses the copy only
//!   while it is this same build, so after an update it asks again until setup is run again.
//!
//! The trade: anything running as you can then start the helper silently and do what it allows
//! (see `direct`'s allowlists): change CPU ratios, voltages and power limits, and the DIMM rails.

use std::ffi::CStr;
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
use std::path::{Path, PathBuf};

use super::HELPER_ARG;

/// The root-owned copy setup installs for a program that is not already root-owned.
pub const INSTALLED: &str = "/usr/local/bin/rochcpu";
/// The polkit rule that lets its owner start the helper without a password.
pub const RULE_FILE: &str = "/etc/polkit-1/rules.d/49-rochcpu.rules";

const USAGE: &str = "\
rochcpu setup            stop asking for the password (run it with sudo)
rochcpu setup --status   say whether it is set up, and whether this build will use it
rochcpu setup --show     what setup would change, without changing it
rochcpu setup --undo     remove it (with sudo), so every launch asks again
";

/// The program to hand `pkexec`: this one when nobody but root can change it (a package install),
/// else the installed copy while it is this very build, else this executable, which brings the
/// password prompt back rather than run an older helper.
pub fn helper_program() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(if root_protected(&exe) || !installed_matches(&exe) {
        exe
    } else {
        PathBuf::from(INSTALLED)
    })
}

/// Root-owned and writable by nobody else: the file, and every folder above it, since whoever can
/// write a folder can swap the file inside it.
fn root_protected(path: &Path) -> bool {
    let safe = |meta: &std::fs::Metadata| meta.uid() == 0 && meta.mode() & 0o022 == 0;
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && safe(&meta) => {}
        _ => return false,
    }
    path.ancestors()
        .skip(1)
        .all(|folder| folder.as_os_str().is_empty() || std::fs::metadata(folder).is_ok_and(|meta| safe(&meta)))
}

/// The copy is root-owned, writable by nobody else, and byte for byte the running program.
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

/// A path as it can go into the rule as a plain string.
fn plain_path(path: &str) -> bool {
    path.starts_with('/') && !path.bytes().any(|b| b == b'"' || b == b'\\' || b.is_ascii_control())
}

fn rule_text(user: &str, program: &str) -> String {
    format!(
        "// Written by `rochcpu setup` for {user}; `sudo rochcpu setup --undo` removes it.\n\
         // Lets {user} start Roch CPU's root helper without a password: only {program}, which\n\
         // nobody but root can change, only with {HELPER_ARG}, and only from an active local session.\n\
         polkit.addRule(function (action, subject) {{\n\
         \x20   if (action.id == \"org.freedesktop.policykit.exec\" &&\n\
         \x20       action.lookup(\"program\") == \"{program}\" &&\n\
         \x20       action.lookup(\"command_line\") == \"{program} {HELPER_ARG}\" &&\n\
         \x20       subject.user == \"{user}\" && subject.local && subject.active) {{\n\
         \x20       return polkit.Result.YES;\n\
         \x20   }}\n\
         }});\n"
    )
}

fn show() -> i32 {
    println!("Setup would:");
    match std::env::current_exe() {
        Ok(exe) if root_protected(&exe) => println!(
            "  leave this program where it is: {} is root-owned, so the rule can name it",
            exe.display()
        ),
        _ => println!("  copy this program to {INSTALLED}, owned by root and writable only by root"),
    }
    println!(
        "  write {RULE_FILE}, letting you start that program as the helper ({HELPER_ARG}) without a password"
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
        Ok((program, copied)) => {
            println!("Wrote {RULE_FILE}. Roch CPU will no longer ask {user} for a password.");
            if copied {
                println!(
                    "It names the copy at {program}: run setup again after updating Roch CPU."
                );
            } else {
                println!("It names {program}, so an update installed in its place keeps working.");
            }
            println!("`sudo rochcpu setup --undo` removes it.");
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

/// The program the rule names, and whether it had to be copied there.
fn install_files(user: &str) -> Result<(String, bool), String> {
    let exe = std::env::current_exe().map_err(|e| format!("could not find this program: {e}"))?;
    let (program, copied) = if root_protected(&exe) {
        // A package's copy: a stale one setup made earlier would only confuse, so it goes.
        let _ = std::fs::remove_file(INSTALLED);
        (exe.display().to_string(), false)
    } else {
        let bytes = std::fs::read(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
        put(Path::new(INSTALLED), &bytes, 0o755)?;
        (INSTALLED.to_string(), true)
    };
    if !plain_path(&program) {
        return Err(format!("{program} cannot be named in a polkit rule"));
    }
    put(Path::new(RULE_FILE), rule_text(user, &program).as_bytes(), 0o644)?;
    Ok((program, copied))
}

fn status() -> i32 {
    let exe = std::env::current_exe().unwrap_or_default();
    let installed = Path::new(INSTALLED).exists();
    // The rules folder is readable only by root and polkit, so as yourself the rule cannot be seen.
    let rule = match std::fs::metadata(RULE_FILE) {
        Ok(_) => Some(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    };
    if root_protected(&exe) {
        match rule {
            Some(true) => println!("Set up: {RULE_FILE} is in place."),
            Some(false) => println!("Not set up: it asks for a password each time. Run `sudo rochcpu setup`."),
            None => println!("{RULE_FILE} is readable only by root; `sudo rochcpu setup --status` shows it."),
        }
        println!(
            "{} is root-owned, so with the rule in place it starts without asking, updates included.",
            exe.display()
        );
        return 0;
    }
    match (installed, rule) {
        (false, Some(false)) => println!("Not set up: it asks for a password each time."),
        (true, Some(true)) => println!("Set up: {INSTALLED} and {RULE_FILE} are in place."),
        (true, None) => println!("Set up: {INSTALLED} is in place ({RULE_FILE} is readable only by root)."),
        (false, _) => println!("Not set up: {INSTALLED} is missing. Run `sudo rochcpu setup`."),
        (true, Some(false)) => println!("Partly set up: {RULE_FILE} is missing. Run `sudo rochcpu setup` again."),
    }
    if installed {
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
    fn the_rule_names_only_one_program_for_one_user() {
        let rule = rule_text("local", "/usr/bin/rochcpu");
        assert!(rule.contains(
            "action.lookup(\"command_line\") == \"/usr/bin/rochcpu --hardware-helper\""
        ));
        assert!(rule.contains("action.lookup(\"program\") == \"/usr/bin/rochcpu\""));
        assert!(rule.contains("subject.user == \"local\" && subject.local && subject.active"));
        assert!(!rule.contains(".local/bin"));
    }

    #[test]
    fn only_plain_names_and_paths_go_into_the_rule() {
        assert!(plain_name("local") && plain_name("first.last") && plain_name("a_b-1"));
        assert!(!plain_name("") && !plain_name("x\" || true || \"") && !plain_name("a b"));
        assert!(plain_path("/usr/bin/rochcpu") && !plain_path("bin/rochcpu") && !plain_path("/a\"b"));
    }

    #[test]
    fn a_package_install_is_protected_and_a_home_install_is_not() {
        // /usr/bin/sh is root-owned in root-owned folders on any Linux this runs on.
        assert!(root_protected(Path::new("/usr/bin/sh")) || !Path::new("/usr/bin/sh").exists());
        let home = std::env::temp_dir().join(format!("rochcpu-setup-test-{}", std::process::id()));
        std::fs::write(&home, b"x").unwrap();
        assert!(!root_protected(&home));
        let _ = std::fs::remove_file(home);
    }
}
