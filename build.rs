// Draws PartyLink's icon (two linked rings on a dark tile) and embeds it in the exe using rc.exe from
// the Windows SDK. Skipped with a warning if the SDK isn't there; the app still works without it.
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let ico = out.join("partylink.ico");
    let rc = out.join("partylink.rc");
    let res = out.join("partylink.res");
    if fs::write(&ico, make_ico()).is_err()
        || fs::write(&rc, format!("1 ICON \"{}\"\n", ico.display().to_string().replace('\\', "/"))).is_err()
    {
        println!("cargo:warning=Could not write the icon files");
        return;
    }
    let mut compilers: Vec<PathBuf> = fs::read_dir(r"C:\Program Files (x86)\Windows Kits\10\bin")
        .map(|dir| dir.flatten().map(|e| e.path().join(r"x64\rc.exe")).filter(|p| p.exists()).collect())
        .unwrap_or_default();
    compilers.sort();
    let Some(compiler) = compilers.pop() else {
        println!("cargo:warning=rc.exe not found; building without an icon");
        return;
    };
    let ok = Command::new(compiler).arg("/nologo").arg("/fo").arg(&res).arg(&rc).status().is_ok_and(|s| s.success());
    if ok {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        println!("cargo:warning=rc.exe failed; building without an icon");
    }
}

fn coverage(distance: f32) -> f32 {
    (distance + 0.5).clamp(0.0, 1.0)
}

fn make_ico() -> Vec<u8> {
    const SIDE: usize = 256;
    const TOP: [f32; 3] = [108.0, 124.0, 255.0];
    const BOTTOM: [f32; 3] = [72.0, 82.0, 205.0];
    let mut dib = Vec::with_capacity(40 + SIDE * SIDE * 4 + SIDE * SIDE / 8);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(SIDE as i32).to_le_bytes());
    dib.extend_from_slice(&((SIDE * 2) as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&((SIDE * SIDE * 4) as u32).to_le_bytes());
    dib.extend_from_slice(&[0u8; 16]);
    let ring = |x: f32, y: f32, cx: f32, cy: f32| {
        let distance = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        coverage(13.0 - (distance - 46.0).abs())
    };
    for y in (0..SIDE).rev() {
        for x in 0..SIDE {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // A rounded square tile.
            let half = 118.0 - 56.0;
            let (qx, qy) = ((px - 128.0).abs() - half, (py - 128.0).abs() - half);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            let tile = coverage(-(outside + qx.max(qy).min(0.0) - 56.0));
            let left = ring(px, py, 96.0, 128.0);
            let right = ring(px, py, 160.0, 128.0);
            // The right ring passes under the left one at the top crossing, so they read as linked.
            let right = if py < 128.0 && px < 128.0 { right * (1.0 - left) } else { right };
            let mark = left.max(right);
            let t = py / SIDE as f32;
            let mut rgb = [0.0f32; 3];
            for i in 0..3 {
                let ground = TOP[i] + (BOTTOM[i] - TOP[i]) * t;
                rgb[i] = ground + (255.0 - ground) * mark;
            }
            // 32-bit ICO pixels are BGRA with straight alpha.
            dib.extend_from_slice(&[rgb[2] as u8, rgb[1] as u8, rgb[0] as u8, (255.0 * tile) as u8]);
        }
    }
    dib.resize(dib.len() + SIDE * SIDE / 8, 0);
    let mut ico = Vec::with_capacity(22 + dib.len());
    ico.extend_from_slice(&[0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 1, 0, 32, 0]);
    ico.extend_from_slice(&(dib.len() as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());
    ico.extend_from_slice(&dib);
    ico
}
