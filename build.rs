// Embeds the application manifest (requireAdministrator, Windows 10/11, long paths) and the
// icon into the executable, the same resources the .NET project carried.
fn main() {
    println!("cargo:rerun-if-changed=res/roch-cpu.rc");
    println!("cargo:rerun-if-changed=res/app.manifest");
    println!("cargo:rerun-if-changed=assets/RochCPU.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("res/roch-cpu.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
