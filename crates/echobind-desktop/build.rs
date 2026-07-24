use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    // The ScreenCaptureKit bridge is a static Swift library. Cargo does not
    // propagate dependency build-script rpaths to the final executable, so the
    // binary crate must add the Swift runtime locations itself.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    let developer_dir = env::var_os("DEVELOPER_DIR")
        .map(PathBuf::from)
        .or_else(selected_developer_dir);
    let Some(developer_dir) = developer_dir else {
        println!(
            "cargo:warning=Unable to locate Xcode; the desktop executable may not find the Swift concurrency runtime"
        );
        return;
    };

    let toolchain = developer_dir.join("Toolchains/XcodeDefault.xctoolchain/usr/lib");
    for relative in ["swift-5.5/macosx", "swift/macosx"] {
        let runtime = toolchain.join(relative);
        if runtime.is_dir() {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", runtime.display());
        }
    }
}

fn selected_developer_dir() -> Option<PathBuf> {
    let output = Command::new("xcode-select").arg("-p").output().ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}
