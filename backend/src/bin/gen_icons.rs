//! Generate all icon sizes from strom-icon-1024.png
//! Run with: cargo run --bin gen-icons
//!
//! Regenerates all icon sizes from the master 1024x1024 icon.

use image::{imageops::FilterType, ImageFormat, RgbaImage};
use std::fs::File;
use std::io::{BufWriter, Write};

fn main() {
    let source_path = "assets/strom-icon-1024.png";

    println!("Loading {}...", source_path);
    let img = image::open(source_path)
        .expect("Failed to open image")
        .into_rgba8();
    println!("Source size: {}x{}", img.width(), img.height());

    // Generate PNG icons
    let png_sizes = [
        (512, "assets/icon-512.png"),
        (192, "assets/icon-192.png"),
        (180, "assets/apple-touch-icon-180.png"),
        (128, "assets/icon-128.png"),
        (64, "assets/icon-64.png"),
        (32, "assets/favicon-32.png"),
        (16, "assets/favicon-16.png"),
    ];

    for (size, path) in png_sizes {
        let resized = resize_rgba(&img, size);
        resized
            .save_with_format(path, ImageFormat::Png)
            .unwrap_or_else(|e| panic!("Failed to save {}: {}", path, e));
        println!("Generated {}x{} -> {}", size, size, path);
    }

    // Frontend icon
    let frontend_icon = resize_rgba(&img, 128);
    frontend_icon
        .save_with_format("frontend/src/icon.png", ImageFormat::Png)
        .expect("Failed to save frontend icon");
    println!("Generated 128x128 -> frontend/src/icon.png");

    // Generate ICO files
    generate_ico(
        "assets/favicon.ico",
        &["assets/favicon-16.png", "assets/favicon-32.png"],
    );
    generate_ico(
        "assets/strom.ico",
        &[
            "assets/favicon-16.png",
            "assets/favicon-32.png",
            "assets/icon-64.png",
            "assets/icon-128.png",
        ],
    );

    println!("\nDone!");
}

/// Resize RGBA image preserving alpha channel
fn resize_rgba(img: &RgbaImage, size: u32) -> RgbaImage {
    image::imageops::resize(img, size, size, FilterType::Lanczos3)
}

/// Generate ICO file from PNG files (ICO supports embedded PNG)
fn generate_ico(output_path: &str, png_paths: &[&str]) {
    let mut images: Vec<Vec<u8>> = Vec::new();
    for path in png_paths {
        images.push(std::fs::read(path).unwrap_or_else(|_| panic!("Failed to read {}", path)));
    }

    let mut file = BufWriter::new(File::create(output_path).expect("Failed to create ICO"));

    // ICO header: reserved (2) + type (2) + count (2)
    file.write_all(&[0, 0, 1, 0]).unwrap(); // reserved=0, type=1 (ICO)
    file.write_all(&(images.len() as u16).to_le_bytes())
        .unwrap();

    let mut offset = 6 + (16 * images.len()); // header + directory entries

    // Directory entries (16 bytes each)
    for (i, png_data) in images.iter().enumerate() {
        let img = image::load_from_memory(png_data).expect("Failed to decode PNG");
        let w = if img.width() >= 256 {
            0
        } else {
            img.width() as u8
        };
        let h = if img.height() >= 256 {
            0
        } else {
            img.height() as u8
        };

        file.write_all(&[w, h, 0, 0]).unwrap(); // width, height, palette, reserved
        file.write_all(&[1, 0]).unwrap(); // color planes = 1
        file.write_all(&[32, 0]).unwrap(); // bits per pixel = 32
        file.write_all(&(png_data.len() as u32).to_le_bytes())
            .unwrap();
        file.write_all(&(offset as u32).to_le_bytes()).unwrap();

        println!("  {} - {}x{}", png_paths[i], img.width(), img.height());
        offset += png_data.len();
    }

    // Image data (raw PNG)
    for png_data in &images {
        file.write_all(png_data).unwrap();
    }

    file.flush().unwrap();
    println!("Generated {}", output_path);
}
