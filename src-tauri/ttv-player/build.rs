use std::path::{Path, PathBuf};
use std::process::Command;

fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }

    let arch_dir = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => "x64",
    };

    if let Ok(sdk_dir) = std::env::var("WindowsSdkDir") {
        if let Ok(sdk_ver) = std::env::var("WindowsSDKVersion") {
            let ver = sdk_ver.trim_end_matches(['\\', '/']);
            let cand = PathBuf::from(&sdk_dir)
                .join("bin")
                .join(ver)
                .join(arch_dir)
                .join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }

    let kits_bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = std::fs::read_dir(kits_bin) {
        let mut versions: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| p.is_dir())
            .collect();
        versions.sort();
        for dir in versions.into_iter().rev() {
            let cand = dir.join(arch_dir).join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=shaders");
    println!("cargo:rerun-if-changed=assets");
    println!("cargo:rerun-if-changed=assets/ttv-player.ico");

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let ico_path = manifest_dir.join("assets").join("ttv-player.ico");
    if !ico_path.is_file() {
        return;
    }

    let Ok(out_dir) = std::env::var("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    let rc_path = out_dir.join("ttv_player_icon.rc");
    let res_path = out_dir.join("ttv_player_icon.res");
    let normalized_ico = ico_path.to_string_lossy().replace('\\', "/");
    let rc_body = format!("1 ICON \"{normalized_ico}\"\n");
    if std::fs::write(&rc_path, rc_body).is_err() {
        return;
    }

    if let Some(rc) = find_rc_exe() {
        let status = Command::new(&rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&res_path)
            .arg(&rc_path)
            .status();
        if matches!(status, Ok(s) if s.success()) && res_path.is_file() {
            println!("cargo:rustc-link-arg-bins={}", res_path.display());
        } else {
            println!(
                "cargo:warning=rc.exe failed to compile app icon resource at {}",
                ico_path.display()
            );
        }
    }
}
