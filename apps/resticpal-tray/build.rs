use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    compile_windows_resources("resticpal tray application", "resticpal-tray.exe");
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
    let processor_architecture = match env::var("CARGO_CFG_TARGET_ARCH")
        .expect("Cargo target architecture")
        .as_str()
    {
        "x86_64" => "amd64",
        "x86" => "x86",
        "aarch64" => "arm64",
        architecture => panic!("unsupported Windows manifest architecture: {architecture}"),
    };

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let icon = manifest_dir
        .join("../../assets/resticpal.ico")
        .canonicalize()
        .expect("resticpal icon");
    let output_dir = PathBuf::from(env::var_os("OUT_DIR").expect("build output directory"));
    let manifest = output_dir.join("resticpal-tray.manifest");
    fs::write(
        &manifest,
        application_manifest(&version, processor_architecture, major, minor, patch),
    )
    .expect("write generated tray application manifest");
    let output = output_dir.join("resticpal-version.rc");
    fs::write(
        &output,
        version_resource(
            &icon,
            &manifest,
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
        .manifest_required()
        .expect("resticpal tray resources and DPI manifest should compile");
}

fn application_manifest(
    version: &str,
    processor_architecture: &str,
    major: u16,
    minor: u16,
    patch: u16,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly manifestVersion="1.0" xmlns="urn:schemas-microsoft-com:asm.v1">
    <assemblyIdentity
        type="win32"
        name="resticpal.tray"
        version="{major}.{minor}.{patch}.0"
        processorArchitecture="{processor_architecture}" />
    <description>resticpal tray application {version}</description>
    <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
        <security>
            <requestedPrivileges>
                <requestedExecutionLevel level="asInvoker" uiAccess="false" />
            </requestedPrivileges>
        </security>
    </trustInfo>
    <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
        <application>
            <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}" />
        </application>
    </compatibility>
    <application xmlns="urn:schemas-microsoft-com:asm.v3">
        <windowsSettings>
            <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
            <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
            <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
        </windowsSettings>
    </application>
    <dependency>
        <dependentAssembly>
            <assemblyIdentity
                type="win32"
                name="Microsoft.Windows.Common-Controls"
                version="6.0.0.0"
                processorArchitecture="*"
                publicKeyToken="6595b64144ccf1df"
                language="*" />
        </dependentAssembly>
    </dependency>
</assembly>
"#,
    )
}

#[allow(clippy::too_many_arguments)]
fn version_resource(
    icon: &Path,
    manifest: &Path,
    description: &str,
    original_filename: &str,
    version: &str,
    major: u16,
    minor: u16,
    patch: u16,
) -> String {
    let icon = icon.to_string_lossy().replace('\\', "/");
    let manifest = manifest.to_string_lossy().replace('\\', "/");
    format!(
        r#"1 24 "{manifest}"
1 ICON "{icon}"
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
