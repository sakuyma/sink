use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-changed=resources.gresource.xml");
    println!("cargo:rerun-if-changed=icons");

    for entry in walk(Path::new("icons")) {
        println!("cargo:rerun-if-changed={}", entry.display());
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    let target = out_dir.join("sink.gresource");
    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir=icons")
        .arg("--target")
        .arg(&target)
        .arg("resources.gresource.xml")
        .status()
        .expect("glib-compile-resources is required to build sink");
    assert!(status.success(), "glib-compile-resources failed");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                paths.extend(walk(&path));
            } else {
                paths.push(path);
            }
        }
    }
    paths
}
