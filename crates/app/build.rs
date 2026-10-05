use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // rust-i18n reads the messages while compiling; without this, editing them alone rebuilds nothing.
    println!("cargo:rerun-if-changed=locales");
    println!("cargo:rerun-if-env-changed=CC_SAME_SKIP_RESOURCES");
    // `CC_SAME_SKIP_RESOURCES=1` lets `cargo check` run for Windows from another OS.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var_os("CC_SAME_SKIP_RESOURCES").is_none()
    {
        embed_windows_resources();
    }
}

/// The icon and version details of the `.exe`. The application manifest (Common Controls 6,
/// per-monitor DPI) comes from GPUI itself; a second one would clash at link time.
fn embed_windows_resources() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources").join("windows");
    let icon = dir.join("cc-same.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let esc = |p: &Path| p.to_string_lossy().replace('\\', "\\\\");
    let version = env!("CARGO_PKG_VERSION");
    let mut parts = version.split('.').map(|p| p.parse::<u16>().unwrap_or(0)).chain(std::iter::repeat(0));
    let numeric = format!(
        "{},{},{},{}",
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap()
    );
    let icon_line = if icon.exists() { format!("1 ICON \"{}\"\n", esc(&icon)) } else { String::new() };
    let rc = format!(
        r#"{icon_line}
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileDescription", "CC Same\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "InternalName", "cc-same-app\0"
            VALUE "OriginalFilename", "CC Same.exe\0"
            VALUE "ProductName", "CC Same\0"
            VALUE "ProductVersion", "{version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#,
    );
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("cc-same.rc");
    std::fs::write(&out, rc).expect("write the Windows resource script");
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_required()
        .expect("compile Windows resources (rc.exe or llvm-rc is required)");
}
