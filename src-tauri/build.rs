use std::fs;
use std::path::Path;
use std::time::SystemTime;

fn is_newer(src: &Path, dst: &Path) -> bool {
    if !dst.is_file() {
        return true;
    }
    let sm = fs::metadata(src)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let dm = fs::metadata(dst)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    sm > dm
}

fn sync_file_if_needed(src: &Path, dst: &Path) {
    if !src.is_file() {
        println!(
            "cargo:warning=ttv-player staged asset source missing: {}",
            src.display()
        );
        return;
    }
    if is_newer(src, dst) {
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(err) = fs::copy(src, dst) {
            println!(
                "cargo:warning=failed to stage {} -> {}: {err}",
                src.display(),
                dst.display()
            );
        }
    }
}

fn stage_ttv_player_resources() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let exe_src = manifest.join("ttv-player/target/release/ttv-player.exe");
    let dll_src = manifest.join("native-player/libmpv-2.dll");
    let stage_dir = manifest.join("resources/ttv-player");
    let _ = fs::create_dir_all(&stage_dir);
    let exe_dst = stage_dir.join("ttv-player.exe");
    let dll_dst = stage_dir.join("libmpv-2.dll");

    println!("cargo:rerun-if-changed={}", exe_src.display());
    println!("cargo:rerun-if-changed={}", dll_src.display());
    println!("cargo:rerun-if-changed={}", exe_dst.display());
    println!("cargo:rerun-if-changed={}", dll_dst.display());

    sync_file_if_needed(&exe_src, &exe_dst);
    sync_file_if_needed(&dll_src, &dll_dst);
}

fn main() {
    // 独立 MPV 播放器助手仅在 Windows 下暂存；macOS 构建跳过以免无意义告警。
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        stage_ttv_player_resources();
    }
    tauri_build::build()
}
