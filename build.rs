//! Compiles Parley's icons (data/icons) into a GResource bundled in the binary.

use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=data/icons");
    println!("cargo:rerun-if-changed=data/parley.gresource.xml");
    let target = PathBuf::from(env::var("OUT_DIR").unwrap()).join("parley.gresource");
    let status = Command::new("glib-compile-resources")
        .args(["--sourcedir", "data/icons/scalable/actions", "--target"])
        .arg(&target)
        .arg("data/parley.gresource.xml")
        .status()
        .expect("running glib-compile-resources (part of GLib's development tools)");
    assert!(status.success(), "glib-compile-resources failed");
}
