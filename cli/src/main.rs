use clap::Parser;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use anstream::println;
use owo_colors::OwoColorize;
use chrono::{Local, TimeZone};

use pngstreamdec::{open_png, open_png_stream, decode_scanlines, close_png, AuxiliaryMetadata};

#[derive(Parser, Debug)]
#[command(author, version, about = "Stream PNG to PPM")]
struct Args {
    #[arg(
    short = 'm',
    long,
    default_value_t = 12.0,
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
unsafe extern "C" fn stdin_read_cb(_user_data: *mut std::ffi::c_void, buf: *mut u8, len: usize) -> usize {
    let mut slice = unsafe { std::slice::from_raw_parts_mut(buf, len) };
    std::io::stdin().read(&mut slice).unwrap_or(0)
}

unsafe extern "C" {
    fn get_last_error() -> *const std::os::raw::c_char;
    fn free_error_string(ptr: *mut std::os::raw::c_char);
}

/// Prints auxiliary chunk components to the console, automatically formatting
/// timestamps to the execution environment's active locale and time zone.
pub fn print_metadata_to_console(meta: &AuxiliaryMetadata, width: u32, height: u32) {
    println!("\n================= PNG METADATA REPORT =================");

    // gAMA Chunk
    if meta.has_gamma {
        let gamma_value = meta.gamma as f64 / 100_000.0;
        println!(" Gamma (gAMA)         : {:.5}", gamma_value);
    }

    // sRGB Chunk
    if meta.has_srgb {
        let intent_name = match meta.srgb_intent {
            0 => "Perceptual",
            1 => "Relative Colorimetric",
            2 => "Saturation",
            3 => "Absolute Colorimetric",
            _ => "Unknown/Reserved",
        };
        println!(" sRGB Intent          : {} ({})", intent_name, meta.srgb_intent);
    }

    // cHRM Chunk
    if meta.has_chrm {
        println!(" Chromaticities (cHRM):");
        println!("   White Point : X={:.5}, Y={:.5}", meta.chrm_data[0] as f64 / 100_000.0, meta.chrm_data[1] as f64 / 100_000.0);
        println!("   Red Channel : X={:.5}, Y={:.5}", meta.chrm_data[2] as f64 / 100_000.0, meta.chrm_data[3] as f64 / 100_000.0);
        println!("   Green Chan. : X={:.5}, Y={:.5}", meta.chrm_data[4] as f64 / 100_000.0, meta.chrm_data[5] as f64 / 100_000.0);
        println!("   Blue Channel: X={:.5}, Y={:.5}", meta.chrm_data[6] as f64 / 100_000.0, meta.chrm_data[7] as f64 / 100_000.0);
    }

    // pHYs Chunk
    if meta.has_phys {
        println!("  Physical pixel dimensions (pHYs):");
        let unit_str = if meta.phys_unit == 1 { "pixels/meter" } else { "unknown" };
        println!(
            "   Resolution: {} x {} {}",
            meta.phys_x, meta.phys_y, unit_str
        );
        if meta.phys_unit == 1 && meta.phys_x > 0 && meta.phys_y > 0 {
            // Assumes 'width' and 'height' are available in the current scope
            let width_cm = (width as f64 / meta.phys_x as f64) * 100.0;
            let height_cm = (height as f64 / meta.phys_y as f64) * 100.0;
            println!("   Size: {:.3} cm x {:.3} cm", width_cm, height_cm);
        }
        if meta.phys_y > 0 {
            let aspect = meta.phys_x as f64 / meta.phys_y as f64;
            println!("   Aspect ratio: {:.6}", aspect);
        }
    }

    // hIST Chunk (Large block rule: Only print length)
    if !meta.histogram.is_empty() {
        println!(" Histogram (hIST)     : [Large Block - Elements: {}]", meta.histogram.len());
    }

    // bKGD Chunk (Small structural buffer, print inline contents)
    if !meta.bkgd_bytes.is_empty() {
        println!(" Background (bKGD)    : Raw Payload Bytes {:?}", meta.bkgd_bytes);
    }

    // tIME Chunk (Formatted to Local Timezone and Localized Long Format)
    if meta.has_time {
        match Local.timestamp_opt(meta.unix_epoch, 0) {
            chrono::LocalResult::Single(local_time) => {
                // Formatting Layout: Long readable date according to typical system preferences
                // %A: Full weekday, %B: Full month name, %d: Day, %Y: Year, %X: Regional Time
                let formatted_date = local_time.format("%A, %B %d, %Y %X %Z");
                println!(" Modification (tIME)  : {}", formatted_date);
            }
            _ => {
                println!(" Modification (tIME)  : Raw Timestamp Epoch ({} seconds)", meta.unix_epoch);
            }
        }
    }

    // Text Chunks (tEXt/zTXt/iTXt) - Explicitly un-truncated
    if !meta.text_chunks.is_empty() {
        println!(" Text Content Fields  :");
        for (i, chunk) in meta.text_chunks.iter().enumerate() {
            println!("   [{}] Keyword : \"{}\"", i, chunk.keyword);
            println!("       Text    : \"{}\"", chunk.text);
        }
    }

    println!("=======================================================\n");
}

// Helper to convert and stream 8-bit color channels to standard 3-channel RGB PPM
fn write_rgb_8bit<W: Write>(writer: &mut W, raw_data: &[u8], color_type: u8) -> std::io::Result<()> {
    match color_type {
        0 => { // Grayscale -> RGB (Y, Y, Y)
            let mut rgb = Vec::with_capacity(raw_data.len() * 3);
            for &y in raw_data {
                rgb.push(y);
                rgb.push(y);
                rgb.push(y);
            }
            writer.write_all(&rgb)?;
        }
        2 | 3 => { // RGB -> RGB (Direct write)
            writer.write_all(raw_data)?;
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
            writer.write_all(&rgba)?;
        }
        6 => { // RGBA -> RGBA (Direct write)
            writer.write_all(raw_data)?;
        }
        _ => {}
    }
    Ok(())
}

// Helper to convert and stream 16-bit color channels to standard Big-Endian 3-channel RGB PPM
fn write_rgb_16bit<W: Write>(writer: &mut W, raw_data: &[u8], color_type: u8) -> std::io::Result<()> {
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
            writer.write_all(&rgb)?;
        }
        2 => { // RGB 16-bit -> RGB 16-bit
            writer.write_all(raw_data)?;
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
            writer.write_all(&rgba)?;
        }
        6 => { // RGBA 16-bit -> RGBA 16-bit (Direct write)
            writer.write_all(raw_data)?;
        }
        _ => {}
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut width = 0;
    let mut height = 0;
    let mut bit_depth = 0;
    let mut color_type = 0;
    let mut bytes_per_scanline = 0;

    let handle = if args.input == "-" {
        open_png_stream(
            stdin_read_cb,
            std::ptr::null_mut(),
                        &mut width,
                        &mut height,
                        &mut bit_depth,
                        &mut color_type,
                        &mut bytes_per_scanline,
        )
    } else {
        let c_input = CString::new(args.input).map_err(|e| format!("Invalid input string: {}", e))?;
        open_png(
            c_input.as_ptr(),
                    &mut width,
                    &mut height,
                    &mut bit_depth,
                    &mut color_type,
                    &mut bytes_per_scanline,
        )
    };

    if handle.is_null() {
        unsafe {
            let err_ptr = get_last_error();
            if !err_ptr.is_null() {
                let c_str = std::ffi::CStr::from_ptr(err_ptr);
                eprintln!("{}", format!("Error: {}", c_str.to_string_lossy()).bright_red());
                free_error_string(err_ptr as *mut _);
            } else {
                eprintln!("Error: Could not open or parse PNG file.");
            }
        }
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

    unsafe {
        let dec = &*handle;
        print_metadata_to_console(&dec.meta, width, height);
    }

    let num_scanlines = if let Some(s) = args.scanlines {
        s
    } else {
        let process_rss_footprint = 2_800_000;
        let zlib_overhead = 45_000;
        let io_overhead = 65_536;
        let baseline_bytes = process_rss_footprint + zlib_overhead + io_overhead;

        let transient_multiplier = match color_type {
            0 => 3,
            4 => 2,
            _ => 0,
        };

        let vec_growth_slack = 1.20;
        let active_ram_per_line = bytes_per_scanline + (bytes_per_scanline * transient_multiplier);
        let ram_per_scanline = (active_ram_per_line as f64 * vec_growth_slack) as usize;

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

    let mut output_path = std::path::PathBuf::from(&args.output);

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

    let out_file_raw = File::create(&output_path)?;
    let mut out_file = BufWriter::new(out_file_raw);

    let max_val = if bit_depth == 16 { 65535 } else { 255 };

    if color_type == 4 || color_type == 6 {
        writeln!(
            out_file,
            "P7\nWIDTH {}\nHEIGHT {}\nDEPTH 4\nMAXVAL {}\nTUPLTYPE RGB_ALPHA",
            width, height, max_val
        )?;

        unsafe {
            let dec = &*handle;
            if dec.meta.has_phys {
                let unit_str = if dec.meta.phys_unit == 1 { "pixels/meter" } else { "unknown" };
                writeln!(
                    out_file,
                    "\n# Resolution: {} x {} {}",
                    dec.meta.phys_x, dec.meta.phys_y, unit_str
                )?;
            }
        }
        writeln!(out_file, "ENDHDR")?;
    } else {
        writeln!(out_file, "P6")?;

        unsafe {
            let dec = &*handle;
            if dec.meta.has_phys {
                let unit_str = if dec.meta.phys_unit == 1 { "pixels/meter" } else { "unknown" };
                writeln!(
                    out_file,
                    "\n# Resolution: {} x {} {}",
                    dec.meta.phys_x, dec.meta.phys_y, unit_str
                )?;
                if dec.meta.phys_y > 0 {
                    let aspect = dec.meta.phys_x as f64 / dec.meta.phys_y as f64;
                    writeln!(out_file, "\n# AspectRatio: {:.5}", aspect)?;
                }
            }
        }

        writeln!(out_file, "\n{} {}\n{}", width, height, max_val)?;
    }

    let mut total_decoded = 0;

    loop {
        let result = decode_scanlines(handle, num_scanlines);
        if result.size == 0 || result.data.is_null() {
            break;
        }

        let raw_slice = unsafe { std::slice::from_raw_parts(result.data, result.size) };

        if bit_depth == 16 {
            write_rgb_16bit(&mut out_file, raw_slice, color_type)?;
        } else {
            write_rgb_8bit(&mut out_file, raw_slice, color_type)?;
        }

        let lines_returned = (result.size) / bytes_per_scanline;
        total_decoded += lines_returned;

        print!("\rDecoded {}/{} scanlines...", total_decoded, height);
        std::io::Write::flush(&mut std::io::stdout())?;
    }

    if total_decoded > 0 {
        println!("\nSuccess!");
    }

    close_png(handle);
    Ok(())
}
