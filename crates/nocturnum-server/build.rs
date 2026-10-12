//! Refuses to embed a console build that holds a symlink, since rust-embed follows links and
//! would carry whatever they point at into the binary, served without a key.

use std::fs;
use std::path::Path;

fn check(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = fs::symlink_metadata(&path).map(|m| m.file_type());
        match kind {
            Ok(t) if t.is_symlink() => panic!(
                "{} is a symlink, and the console build must hold only plain files",
                path.display()
            ),
            Ok(t) if t.is_dir() => check(&path),
            _ => {}
        }
    }
}

fn main() {
    let build = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../console/build");
    println!("cargo:rerun-if-changed={}", build.display());
    if fs::symlink_metadata(&build).is_ok_and(|m| m.file_type().is_symlink()) {
        panic!(
            "{} is a symlink, build the console in place",
            build.display()
        );
    }
    check(&build);
}
