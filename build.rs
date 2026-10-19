// Embeds the application manifest (requireAdministrator, Windows 10/11, long paths), the icon
// and the version resource into the executable - the same resources the .NET project carried.
// The release workflow checks FileVersion "X.Y.Z.0" and ProductVersion "X.Y.Z+<commit>", so the
// commit is taken from SOURCE_SHA (set by CI) when present.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=res/app.manifest");
    println!("cargo:rerun-if-changed=assets/RochCPU.ico");
    println!("cargo:rerun-if-env-changed=SOURCE_SHA");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let parts: Vec<u32> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    let (major, minor, patch) = (parts[0], parts.get(1).copied().unwrap_or(0), parts.get(2).copied().unwrap_or(0));
    let product_version = match env::var("SOURCE_SHA") {
        Ok(sha) if !sha.trim().is_empty() => format!("{version}+{}", sha.trim()),
        _ => version.clone(),
    };
    // rc.exe accepts forward slashes, which need no escaping inside its string literals.
    let esc = |p: PathBuf| p.display().to_string().replace(std::path::MAIN_SEPARATOR, "/");

    let rc = format!(
        r#"#include <winver.h>

1 ICON "{icon}"
1 24 "{manifest}"

VS_VERSION_INFO VERSIONINFO
 FILEVERSION {major},{minor},{patch},0
 PRODUCTVERSION {major},{minor},{patch},0
 FILEOS VOS_NT_WINDOWS32
 FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Roch Studio"
      VALUE "FileDescription", "Roch CPU"
      VALUE "FileVersion", "{major}.{minor}.{patch}.0"
      VALUE "ProductName", "Roch CPU"
      VALUE "ProductVersion", "{product_version}"
      VALUE "LegalCopyright", "Roch Studio - open CPU tuning for LGA1700 and AMD Ryzen"
      VALUE "OriginalFilename", "Roch CPU.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = esc(root.join("assets").join("RochCPU.ico")),
        manifest = esc(root.join("res").join("app.manifest")),
    );
    let rc_path = out.join("roch-cpu.rc");
    fs::write(&rc_path, rc).unwrap();
    embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional().unwrap();
}
