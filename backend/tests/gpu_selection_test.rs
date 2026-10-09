//! Guards for how Strom gets OpenGL onto the discrete GPU.
//!
//! On a Windows laptop with an integrated GPU and an NVIDIA card, the driver
//! runs a program on the integrated GPU unless the executable exports
//! `NvOptimusEnablement` (`AmdPowerXpressRequestHighPerformance` for AMD).
//! Without it GStreamer's GL lands on the integrated GPU while NVENC runs on the
//! NVIDIA card. The CUDA-GL interop probe must also run inside `strom` itself,
//! or it sees the integrated GPU and turns GPU conversion off.
#![cfg(not(target_os = "macos"))]

use std::process::Command;

/// Exit code clap uses for a command line it cannot parse.
const CLAP_USAGE_ERROR: i32 = 2;

/// `strom gpu-interop-probe` must exist. Without it the startup probe fails
/// with a usage error, which reads as "interop failed" and silently turns GPU
/// conversion off. Whether the pipeline itself passes depends on the machine
/// (no NVENC on most runners), so only the parse is asserted.
#[test]
fn interop_probe_subcommand_exists() {
    let output = Command::new(env!("CARGO_BIN_EXE_strom"))
        .arg(strom::gpu::INTEROP_PROBE_SUBCOMMAND)
        .output()
        .expect("run strom");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_ne!(
        output.status.code(),
        Some(CLAP_USAGE_ERROR),
        "strom does not accept the interop probe subcommand:\n{stderr}"
    );
}

/// `strom.exe` must export both symbols with the value 1. Drop the statics in
/// `main.rs` or the `/EXPORT:` arguments in `build.rs` and the driver finds
/// nothing, so this reads the export table of the built binary.
#[cfg(windows)]
#[test]
fn strom_exe_requests_the_discrete_gpu() {
    let image = std::fs::read(env!("CARGO_BIN_EXE_strom")).expect("read strom.exe");
    let exports = pe::exported_u32s(&image);

    for symbol in [
        "NvOptimusEnablement",
        "AmdPowerXpressRequestHighPerformance",
    ] {
        assert_eq!(
            exports
                .iter()
                .find(|(name, _)| name == symbol)
                .map(|(_, v)| *v),
            Some(1),
            "strom.exe must export {symbol} = 1; exports found: {exports:?}"
        );
    }
}

/// Just enough of the PE format to read named exports that point at a `u32`.
#[cfg(windows)]
mod pe {
    fn u16_at(b: &[u8], off: usize) -> u16 {
        u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
    }

    fn u32_at(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    /// Every named export, with the `u32` stored at its address.
    pub fn exported_u32s(b: &[u8]) -> Vec<(String, u32)> {
        let pe = u32_at(b, 0x3c) as usize;
        assert_eq!(&b[pe..pe + 4], b"PE\0\0", "not a PE image");
        let sections = u16_at(b, pe + 6) as usize;
        let optional = pe + 24;
        let optional_size = u16_at(b, pe + 20) as usize;
        let data_dirs = match u16_at(b, optional) {
            0x20b => optional + 112, // PE32+
            0x10b => optional + 96,  // PE32
            magic => panic!("unknown optional header magic {magic:#x}"),
        };

        let section_table = optional + optional_size;
        let offset = |rva: u32| -> usize {
            (0..sections)
                .map(|i| section_table + i * 40)
                .find_map(|s| {
                    let va = u32_at(b, s + 12);
                    let size = u32_at(b, s + 8).max(u32_at(b, s + 16));
                    (va..va + size)
                        .contains(&rva)
                        .then(|| (rva - va + u32_at(b, s + 20)) as usize)
                })
                .unwrap_or_else(|| panic!("RVA {rva:#x} is in no section"))
        };

        let export_rva = u32_at(b, data_dirs);
        if export_rva == 0 {
            return Vec::new();
        }
        let dir = offset(export_rva);
        let names = offset(u32_at(b, dir + 32));
        let ordinals = offset(u32_at(b, dir + 36));
        let functions = offset(u32_at(b, dir + 28));

        (0..u32_at(b, dir + 24) as usize)
            .map(|i| {
                let name_at = offset(u32_at(b, names + i * 4));
                let len = b[name_at..].iter().position(|&c| c == 0).unwrap();
                let name = String::from_utf8_lossy(&b[name_at..name_at + len]).into_owned();
                let ordinal = u16_at(b, ordinals + i * 2) as usize;
                let value = u32_at(b, offset(u32_at(b, functions + ordinal * 4)));
                (name, value)
            })
            .collect()
    }
}
