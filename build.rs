//! Compiles Parley's icons (data/icons) into a GResource bundled in the binary, and on
//! Windows gives the program its icon and version details (Explorer, the taskbar).

use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=data/icons");
    println!("cargo:rerun-if-changed=data/parley.gresource.xml");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let status = Command::new("glib-compile-resources")
        .args(["--sourcedir", "data/icons/scalable/actions", "--target"])
        .arg(out.join("parley.gresource"))
        .arg("data/parley.gresource.xml")
        .status()
        .expect("running glib-compile-resources (part of GLib's development tools)");
    assert!(status.success(), "glib-compile-resources failed");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources(&out);
    }
}

fn windows_resources(out: &std::path::Path) {
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let n: Vec<u32> = version.split(['.', '-']).take(3).map(|p| p.parse().unwrap_or(0)).collect();
    let (a, b, c) = (n[0], n.get(1).copied().unwrap_or(0), n.get(2).copied().unwrap_or(0));
    let ico = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("data/icons/parley.ico");
    let rc = format!(
        r#"1 ICON "{ico}"
1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Parley"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "parley"
      VALUE "LegalCopyright", "GPL-3.0-or-later"
      VALUE "OriginalFilename", "parley.exe"
      VALUE "ProductName", "Parley"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = ico.display().to_string().replace('\\', "/")
    );
    let (rc_path, obj) = (out.join("parley.rc"), out.join("parley-res.o"));
    std::fs::write(&rc_path, rc).unwrap();
    let status = Command::new("windres")
        .args(["-O", "coff", "-i"])
        .arg(&rc_path)
        .arg("-o")
        .arg(&obj)
        .status()
        .expect("running windres (part of the MSYS2 toolchain)");
    assert!(status.success(), "windres failed");
    println!("cargo:rustc-link-arg-bins={}", obj.display());
}
