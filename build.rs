//! Embeds assets/framecast.ico as the exe icon without any build dependency:
//! the .ico is converted to a compiled resource (.res) by hand and handed to the
//! MSVC linker, which accepts .res files directly.

use std::path::PathBuf;

fn res_header(out: &mut Vec<u8>, data_len: u32, ty: u16, name: u16, flags: u16) {
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes()); // header size
    out.extend_from_slice(&[0xFF, 0xFF]);
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&[0xFF, 0xFF]);
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // data version
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&0x0409u16.to_le_bytes()); // language: en-US
    out.extend_from_slice(&0u32.to_le_bytes()); // version
    out.extend_from_slice(&0u32.to_le_bytes()); // characteristics
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

fn main() {
    let ico_path = PathBuf::from("assets/framecast.ico");
    println!("cargo:rerun-if-changed={}", ico_path.display());
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows-msvc") {
        return;
    }
    let Ok(ico) = std::fs::read(&ico_path) else {
        println!("cargo:warning=assets/framecast.ico missing; building without an icon");
        return;
    };
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let mut res = Vec::new();
    res_header(&mut res, 0, 0, 0, 0); // mandatory empty first entry
    let mut group = vec![0, 0, 1, 0];
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let e = &ico[6 + i * 16..6 + (i + 1) * 16];
        let size = u32::from_le_bytes(e[8..12].try_into().unwrap());
        let off = u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize;
        let data = &ico[off..off + size as usize];
        let id = (i + 1) as u16;
        res_header(&mut res, size, 3, id, 0x1010); // RT_ICON, moveable | discardable
        res.extend_from_slice(data);
        pad4(&mut res);
        group.extend_from_slice(&e[0..12]); // width, height, colors, reserved, planes, bpp, bytes
        group.extend_from_slice(&id.to_le_bytes());
    }
    res_header(&mut res, group.len() as u32, 14, 1, 0x1030); // RT_GROUP_ICON, id 1
    res.extend_from_slice(&group);
    pad4(&mut res);
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("framecast.res");
    std::fs::write(&out, res).expect("write .res");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
