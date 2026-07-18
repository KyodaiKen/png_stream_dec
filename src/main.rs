use clap::Parser;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufWriter, Read, Write};

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
        4 => { // Grayscale + Alpha -> RGB (Ignore Alpha)
            let mut rgb = Vec::with_capacity((raw_data.len() / 2) * 3);
            for chunk in raw_data.chunks_exact(2) {
                let y = chunk[0];
                rgb.push(y);
                rgb.push(y);
                rgb.push(y);
            }
            writer.write_all(&rgb).unwrap();
        }
        6 => { // RGBA -> RGB (Ignore Alpha)
            let mut rgb = Vec::with_capacity((raw_data.len() / 4) * 3);
            for chunk in raw_data.chunks_exact(4) {
                rgb.push(chunk[0]);
                rgb.push(chunk[1]);
                rgb.push(chunk[2]);
            }
            writer.write_all(&rgb).unwrap();
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
        4 => { // Grayscale + Alpha 16-bit -> RGB 16-bit
            let mut rgb = Vec::with_capacity((raw_data.len() / 4) * 3);
            for chunk in raw_data.chunks_exact(4) {
                let hi = chunk[0];
                let lo = chunk[1];
                for _ in 0..3 {
                    rgb.push(hi);
                    rgb.push(lo);
                }
            }
            writer.write_all(&rgb).unwrap();
        }
        6 => { // RGBA 16-bit -> RGB 16-bit (Ignore Alpha, copy 6 bytes out of 8)
            let mut rgb = Vec::with_capacity((raw_data.len() / 8) * 6);
            for chunk in raw_data.chunks_exact(8) {
                rgb.extend_from_slice(&chunk[0..6]);
            }
            writer.write_all(&rgb).unwrap();
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



    let channels = match color_type {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        6 => 4,
        _ => 1,
    };

    println!(
        "PNG Opened: {} x {} / {} bits per channel / color type {} / {} color channels",
             width, height, bit_depth, color_type, channels
    );

    let num_scanlines = if let Some(s) = args.scanlines {
        s
    } else {
        // mem_mib is now just an f64, no need to check Some()
        let m = args.mem_mib;
        let max_bytes = ((m - 3.25) * 1024.0 * 1024.0) as u32;
        let s = max_bytes / (bytes_per_scanline as u32);
        if s == 0 { 1 } else { s }
    };

    println!("Streaming in batches of {} scanline(s), output stride is {}", num_scanlines, bytes_per_scanline);

    let mut out_file = BufWriter::new(File::create(&args.output).expect("Failed to create output"));

    let max_val = if bit_depth == 16 { 65535 } else { 255 };
    writeln!(out_file, "P6\n{} {}\n{}", width, height, max_val).unwrap();

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
