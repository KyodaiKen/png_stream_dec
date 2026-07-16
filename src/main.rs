use clap::Parser;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufWriter, Write};

// By importing from the crate module path, Cargo cleanly links the binary with our local library.
use pngstreamdec::{open_png, decode_scanlines, close_png};

#[derive(Parser, Debug)]
#[command(author, version, about = "Stream PNG to PPM")]
struct Args {
    #[arg(short = 'm', long, help = "Max memory in MiB (mutually exclusive with -s)")]
    mem_mib: Option<f64>,

    #[arg(short = 's', long, help = "Num scanlines per block (mutually exclusive with -m)")]
    scanlines: Option<u32>,

    #[arg(help = "Input PNG file")]
    input: String,

    #[arg(help = "Output PPM file")]
    output: String,
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
        2 => { // RGB -> RGB (Direct write)
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

    if args.mem_mib.is_some() && args.scanlines.is_some() {
        eprintln!("Error: Cannot use -m and -s together.");
        std::process::exit(1);
    }

    let c_input = CString::new(args.input).unwrap();
    let mut width = 0;
    let mut height = 0;
    let mut bit_depth = 0;
    let mut color_type = 0;

    let handle = unsafe {
        open_png(
            c_input.as_ptr(),
                 &mut width,
                 &mut height,
                 &mut bit_depth,
                 &mut color_type,
        )
    };

    if handle.is_null() {
        eprintln!("Error: Could not open or parse PNG file.");
        std::process::exit(1);
    }

    println!(
        "PNG Opened: {}x{} ({} bit, color type {})",
             width, height, bit_depth, color_type
    );

    // Calculate the source bytes per scanline
    let channels = match color_type {
        0 => 1, // Grayscale
        2 => 3, // RGB
        3 => 1, // Indexed
        4 => 2, // Grayscale + Alpha
        6 => 4, // RGBA
        _ => 1,
    };
    let bits_per_pixel = channels * bit_depth as usize;
    let bytes_per_scanline = (width as usize * bits_per_pixel + 7) / 8;

    // Determine batch size based on memory or input constraints
    let num_scanlines = if let Some(s) = args.scanlines {
        s
    } else if let Some(m) = args.mem_mib {
        let max_bytes = (m * 1024.0 * 1024.0) as u32;
        let s = max_bytes / (bytes_per_scanline as u32);
        if s == 0 { 1 } else { s }
    } else {
        10 // Default block size
    };

    println!("Streaming in batches of {} scanline(s)", num_scanlines);

    let mut out_file = BufWriter::new(File::create(&args.output).expect("Failed to create output"));

    // Write standard PPM P6 Header (Max Val is 255 for 8-bit, 65535 for 16-bit)
    let max_val = if bit_depth == 16 { 65535 } else { 255 };
    writeln!(out_file, "P6\n{} {}\n{}", width, height, max_val).unwrap();

    let mut total_decoded = 0;

    loop {
        let result = unsafe { decode_scanlines(handle, num_scanlines) };
        if result.size == 0 || result.data.is_null() {
            break; // EOF reached
        }

        let raw_slice = unsafe { std::slice::from_raw_parts(result.data, result.size) };

        // Transform the streamed block on-the-fly into clean RGB and write to file
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

    println!("\nSuccess!");

    unsafe {
        close_png(handle);
    }
}
