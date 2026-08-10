use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    compile_windows_resources("resticpal backup service", "resticpal-service.exe");
}

fn compile_windows_resources(description: &str, original_filename: &str) {
    println!("cargo:rerun-if-changed=../../assets/resticpal.ico");

    let version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let mut components = version.split('.').map(|part| {
        part.parse::<u16>()
            .expect("resticpal version components must be numeric")
    });
    let major = components.next().expect("major version");
    let minor = components.next().expect("minor version");
    let patch = components.next().expect("patch version");
    assert!(
        components.next().is_none(),
        "resticpal version must have three parts"
    );

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let icon = manifest_dir
        .join("../../assets/resticpal.ico")
        .canonicalize()
        .expect("resticpal icon");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("build output directory"))
        .join("resticpal-version.rc");
    fs::write(
        &output,
        version_resource(
            &icon,
            description,
            original_filename,
            &version,
            major,
            minor,
            patch,
        ),
    )
    .expect("write generated Windows resources");

    embed_resource::compile(&output, embed_resource::NONE)
        .manifest_optional()
        .expect("resticpal service resources should compile");
}

#[allow(clippy::too_many_arguments)]
fn version_resource(
    icon: &Path,
    description: &str,
    original_filename: &str,
    version: &str,
    major: u16,
    minor: u16,
    patch: u16,
) -> String {
    let icon = icon.to_string_lossy().replace('\\', "/");
    format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName", "StackFoundry LLC\0"
            VALUE "FileDescription", "{description}\0"
            VALUE "FileVersion", "{version}.0\0"
            VALUE "InternalName", "{original_filename}\0"
            VALUE "LegalCopyright", "Copyright (c) 2026 Yann Ramin\0"
            VALUE "OriginalFilename", "{original_filename}\0"
            VALUE "ProductName", "resticpal\0"
            VALUE "ProductVersion", "{version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    )
}
