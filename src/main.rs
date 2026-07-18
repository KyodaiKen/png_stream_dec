use clap::Parser;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use anstream::println;
use owo_colors::OwoColorize;

use pngstreamdec::{open_png, open_png_stream, decode_scanlines, close_png};

#[derive(Parser, Debug)]
#[command(author, version, about = "Stream PNG to PPM")]
struct Args {
    #[arg(
        short = 'm',
        long,
        default_value_t = 8.0,
        conflicts_with = "scanlines",
        help = "Max memory in MiB"
    )]
    mem_mib: f64,

    #[arg(
        short = 's',
        long,
        conflicts_with = "mem_mib",
        help = "Num scanlines per block"
    )]
    scanlines: Option<u32>,

    #[arg(help = "Input PNG file (use '-' for stdin)")]
    input: String,

    #[arg(help = "Output PPM file")]
    output: String,
}

// C-Callback mapping global std-in for our generic streaming FFI boundary
extern "C" fn stdin_read_cb(_user_data: *mut std::ffi::c_void, buf: *mut u8, len: usize) -> usize {
    let mut slice = unsafe { std::slice::from_raw_parts_mut(buf, len) };
    std::io::stdin().read(&mut slice).unwrap_or(0)
}

// Helper to convert and stream 8-bit color channels to standard 3-channel RGB PPM
fn write_rgb_8bit<W: Write>(writer: &mut W, raw_data: &[u8], color_type: u8) {
    match color_type {
        0 => { // Grayscale -> RGB (Y, Y, Y)
            let mut rgb = Vec::with_capacity(raw_data.len() * 3);
            for &y in raw_data {
                rgb.push(y);
                rgb.push(y);
                rgb.push(y);
            }
            writer.write_all(&rgb).unwrap();
        }
        2 | 3 => { // RGB -> RGB (Direct write)
            writer.write_all(raw_data).unwrap();
        }
        4 => { // Grayscale + Alpha -> RGBA
            let mut rgba = Vec::with_capacity((raw_data.len() / 2) * 4);
            for chunk in raw_data.chunks_exact(2) {
                let y = chunk[0];
                let a = chunk[1];
                rgba.push(y);
                rgba.push(y);
                rgba.push(y);
                rgba.push(a);
            }
            writer.write_all(&rgba).unwrap();
        }
        6 => { // RGBA -> RGBA (Direct write)
            writer.write_all(raw_data).unwrap();
        }
        _ => {}
    }
}

// Helper to convert and stream 16-bit color channels to standard Big-Endian 3-channel RGB PPM
fn write_rgb_16bit<W: Write>(writer: &mut W, raw_data: &[u8], color_type: u8) {
    match color_type {
        0 => { // Grayscale 16-bit -> RGB 16-bit
            let mut rgb = Vec::with_capacity(raw_data.len() * 3);
            for chunk in raw_data.chunks_exact(2) {
                let hi = chunk[0];
                let lo = chunk[1];
                for _ in 0..3 {
                    rgb.push(hi);
                    rgb.push(lo);
                }
            }
            writer.write_all(&rgb).unwrap();
        }
        2 => { // RGB 16-bit -> RGB 16-bit
            writer.write_all(raw_data).unwrap();
        }
        4 => { // Grayscale + Alpha 16-bit -> RGBA 16-bit
            let mut rgba = Vec::with_capacity(raw_data.len() * 2);
            for chunk in raw_data.chunks_exact(4) {
                let hi_y = chunk[0];
                let lo_y = chunk[1];
                let hi_a = chunk[2];
                let lo_a = chunk[3];
                for _ in 0..3 {
                    rgba.push(hi_y);
                    rgba.push(lo_y);
                }
                rgba.push(hi_a);
                rgba.push(lo_a);
            }
            writer.write_all(&rgba).unwrap();
        }
        6 => { // RGBA 16-bit -> RGBA 16-bit (Direct write)
            writer.write_all(raw_data).unwrap();
        }
        _ => {}
    }
}

fn main() {
    let args = Args::parse();
    let mut width = 0;
    let mut height = 0;
    let mut bit_depth = 0;
    let mut color_type = 0;
    let mut bytes_per_scanline = 0;

    let handle = if args.input == "-" {
        {
            open_png_stream(
                stdin_read_cb,
                std::ptr::null_mut(),
                            &mut width,
                            &mut height,
                            &mut bit_depth,
                            &mut color_type,
                            &mut bytes_per_scanline,
            )
        }
    } else {
        let c_input = CString::new(args.input).unwrap();
        {
            open_png(
                c_input.as_ptr(),
                     &mut width,
                     &mut height,
                     &mut bit_depth,
                     &mut color_type,
                     &mut bytes_per_scanline,
            )
        }
    };

    if handle.is_null() {
        eprintln!("Error: Could not open or parse PNG file.");
        std::process::exit(1);
    }

    let (channels, color_name) = match color_type {
        0 => (1, "Grayscale"),
        2 => (3, "RGB"),
        3 => (1, "Indexed (Paletted)"),
        4 => (2, "Grayscale + Alpha"),
        6 => (4, "RGB + Alpha (RGBA)"),
        _ => (1, "Unknown"),
    };

    println!(
        "PNG Opened: {} x {} / {} bpc / {} bpp / {}, {} ch",
             width, height, bit_depth, bit_depth * channels, color_name, channels
    );

    let num_scanlines = if let Some(s) = args.scanlines {
        s
    } else {
        // 1. True baseline process overhead (independent of scanline count)
        let process_rss_footprint = 2_800_000; // Mapped binary pages, system allocator metadata, & libc
        let zlib_overhead = 45_000;            // Internal history window & tables for zlib inflate
        let io_overhead = 65_536;              // Upper cushion for standard I/O channels (BufReader/BufWriter)
        let baseline_bytes = process_rss_footprint + zlib_overhead + io_overhead;

        // 2. Dynamic transient memory multiplier per scanline in main.rs
        let transient_multiplier = match color_type {
            0 => 3, // Grayscale: main.rs allocates a temporary vector 3x the size of the stride
            4 => 2, // Grayscale + Alpha: main.rs allocates a temporary vector 2x the size of the stride
            _ => 0, // Types 2 (RGB), 3 (Indexed), and 6 (RGBA) write directly with 0 extra allocations
        };

        // 3. Account for Vector growth capacity slack (Vec doubling strategy adds ~20% overhead)
        let vec_growth_slack = 1.20;
        let active_ram_per_line = bytes_per_scanline + (bytes_per_scanline * transient_multiplier);
        let ram_per_scanline = (active_ram_per_line as f64 * vec_growth_slack) as usize;

        // 4. Convert user MiB target to raw bytes and calculate batch capacity
        let total_budget_bytes = (args.mem_mib * 1024.0 * 1024.0) as usize;

        if total_budget_bytes <= baseline_bytes {
            println!(
                "{}",
                format!("WARNING: Given memory budget is too low for baseline execution. Falling back to 1 scanline.").bright_yellow()
            );
            1
        } else {
            let max_bytes_for_scanlines = total_budget_bytes - baseline_bytes;
            let s = (max_bytes_for_scanlines / ram_per_scanline) as u32;
            if s == 0 { 1 } else { s }
        }
    };

    println!("Streaming in batches of {} scanline(s), output stride is {}", num_scanlines, bytes_per_scanline);

    // Convert the output string into a PathBuf for safe extension manipulation
    let mut output_path = std::path::PathBuf::from(&args.output);

    // Detect alpha color types (4 = Gray+Alpha, 6 = RGBA)
    if (color_type == 4 || color_type == 6) && output_path.extension().map_or(false, |ext| ext.eq_ignore_ascii_case("ppm")) {
        output_path.set_extension("pam");
        println!(
            "{}",
            format!(
                "WARNING: Alpha channel detected in source image. Swapping output extension from .ppm to .pam ({})",
                    output_path.file_name().unwrap_or_default().to_string_lossy()
            ).bright_yellow()
        );
    }

    // Pass the modified path to the file creator instead of the raw args.output string
    let mut out_file = BufWriter::new(File::create(&output_path).expect("Failed to create output"));

    let max_val = if bit_depth == 16 { 65535 } else { 255 };

    if color_type == 4 || color_type == 6 {
        // Write PAM (P7) header for RGBA support
        writeln!(
            out_file,
            "P7\nWIDTH {}\nHEIGHT {}\nDEPTH 4\nMAXVAL {}\nTUPLTYPE RGB_ALPHA\nENDHDR",
            width, height, max_val
        ).unwrap();
    } else {
        // Write standard PPM (P6) header for RGB support
        writeln!(out_file, "P6\n{} {}\n{}", width, height, max_val).unwrap();
    }

    let mut total_decoded = 0;

    loop {
        let result = decode_scanlines(handle, num_scanlines);
        if result.size == 0 || result.data.is_null() {
            break;
        }

        let raw_slice = unsafe { std::slice::from_raw_parts(result.data, result.size) };

        if bit_depth == 16 {
            write_rgb_16bit(&mut out_file, raw_slice, color_type);
        } else {
            write_rgb_8bit(&mut out_file, raw_slice, color_type);
        }

        let lines_returned = (result.size) / bytes_per_scanline;
        total_decoded += lines_returned;

        print!("\rDecoded {}/{} scanlines...", total_decoded, height);
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
    }

    if total_decoded > 0 {
        println!("\nSuccess!");
    }

    close_png(handle);
}
