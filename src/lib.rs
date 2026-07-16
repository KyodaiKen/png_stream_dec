use flate2::{Decompress, FlushDecompress};
use std::cmp;
use std::ffi::CStr;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::raw::c_char;
use std::ptr;

const PNG_MAGIC: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

pub struct PngDecoder {
    reader: BufReader<File>,
    decompressor: Decompress,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8,
    bytes_per_pixel: usize,
    bytes_per_scanline: usize,

    current_y: u32,
    idat_remaining: usize,

    uncompressed_buffer: Vec<u8>,
    prev_scanline: Vec<u8>,
    output_buffer: Vec<u8>,
}

#[repr(C)]
pub struct ScanlinesResult {
    pub data: *const u8,
    pub size: usize,
}

fn unfilter_scanline(
    filter_type: u8,
    bpp: usize,
    bytes_per_scanline: usize,
    prev_scanline: &[u8],
    raw: &[u8],
    out: &mut [u8],
) {
    for i in 0..bytes_per_scanline {
        let left = if i >= bpp { out[i - bpp] } else { 0 };
        let up = prev_scanline[i];
        let up_left = if i >= bpp { prev_scanline[i - bpp] } else { 0 };

        let val = match filter_type {
            0 => raw[i],
            1 => raw[i].wrapping_add(left),
            2 => raw[i].wrapping_add(up),
            3 => {
                let avg = ((left as u16 + up as u16) / 2) as u8;
                raw[i].wrapping_add(avg)
            }
            4 => {
                let p = left as i32 + up as i32 - up_left as i32;
                let pa = (p - left as i32).abs();
                let pb = (p - up as i32).abs();
                let pc = (p - up_left as i32).abs();
                let pr = if pa <= pb && pa <= pc { left } else if pb <= pc { up } else { up_left };
                raw[i].wrapping_add(pr)
            }
            _ => raw[i],
        };
        out[i] = val;
    }
}

impl PngDecoder {
    fn new(path: &str) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(file);

        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic).map_err(|_| "Failed to read magic".to_string())?;
        if magic != PNG_MAGIC {
            return Err("Not a valid PNG file".to_string());
        }

        let (len, chunk_type) = Self::read_chunk_header(&mut reader)?;
        if chunk_type != *b"IHDR" || len != 13 {
            return Err("First chunk must be IHDR".to_string());
        }

        let mut ihdr = [0u8; 13];
        reader.read_exact(&mut ihdr).unwrap();
        let width = u32::from_be_bytes(ihdr[0..4].try_into().unwrap());
        let height = u32::from_be_bytes(ihdr[4..8].try_into().unwrap());
        let bit_depth = ihdr[8];
        let color_type = ihdr[9];
        let interlace = ihdr[12];

        if interlace != 0 {
            return Err("Adam7 Interlacing is not supported".to_string());
        }

        let channels = match color_type {
            0 => 1, // Grayscale
            2 => 3, // RGB
            3 => 1, // Indexed
            4 => 2, // Grayscale + Alpha
            6 => 4, // RGBA
            _ => return Err("Unknown color type".to_string()),
        };

        let bits_per_pixel = channels * bit_depth as usize;
        let bytes_per_pixel = cmp::max(1, bits_per_pixel / 8);
        let bytes_per_scanline = (width as usize * bits_per_pixel + 7) / 8;

        reader.seek(SeekFrom::Current(4)).unwrap();

        Ok(Self {
            reader,
            decompressor: Decompress::new(true),
           width,
           height,
           bit_depth,
           color_type,
           bytes_per_pixel,
           bytes_per_scanline,
           current_y: 0,
           idat_remaining: 0,
           uncompressed_buffer: Vec::new(),
           prev_scanline: vec![0; bytes_per_scanline],
           output_buffer: Vec::new(),
        })
    }

    fn read_chunk_header(reader: &mut BufReader<File>) -> Result<(usize, [u8; 4]), String> {
        let mut head = [0u8; 8];
        if reader.read_exact(&mut head).is_err() {
            return Err("EOF reached".to_string());
        }
        let len = u32::from_be_bytes(head[0..4].try_into().unwrap()) as usize;
        let mut c_type = [0u8; 4];
        c_type.copy_from_slice(&head[4..8]);
        Ok((len, c_type))
    }
}

// =========================================================================
// FFI BOUNDARY - C API
// =========================================================================

#[no_mangle]
pub extern "C" fn open_png(
    filename: *const c_char,
    width: *mut u32,
    height: *mut u32,
    bit_depth: *mut u8,
    color_type: *mut u8,
) -> *mut PngDecoder {
    let c_str = unsafe { CStr::from_ptr(filename) };
    let path = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    match PngDecoder::new(path) {
        Ok(decoder) => unsafe {
            if !width.is_null() { *width = decoder.width; }
            if !height.is_null() { *height = decoder.height; }
            if !bit_depth.is_null() { *bit_depth = decoder.bit_depth; }
            if !color_type.is_null() { *color_type = decoder.color_type; }
            Box::into_raw(Box::new(decoder))
        },
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn decode_scanlines(
    handle: *mut PngDecoder,
    mut num_scanlines: u32,
) -> ScanlinesResult {
    let dec = unsafe { &mut *handle };
    dec.output_buffer.clear();

    if dec.current_y >= dec.height {
        return ScanlinesResult { data: ptr::null(), size: 0 };
    }

    if dec.current_y + num_scanlines > dec.height {
        num_scanlines = dec.height - dec.current_y;
    }

    let bytes_needed_per_line = dec.bytes_per_scanline + 1;
    let total_bytes_needed = (num_scanlines as usize) * bytes_needed_per_line;

    let mut in_buf = [0u8; 8192];
    let mut out_buf = [0u8; 16384];

    while dec.uncompressed_buffer.len() < total_bytes_needed {
        if dec.idat_remaining == 0 {
            if dec.current_y > 0 || !dec.uncompressed_buffer.is_empty() {
                dec.reader.seek(SeekFrom::Current(4)).unwrap();
            }

            loop {
                match PngDecoder::read_chunk_header(&mut dec.reader) {
                    Ok((len, ctype)) => {
                        if ctype == *b"IEND" {
                            break;
                        } else if ctype == *b"IDAT" {
                            dec.idat_remaining = len;
                            break;
                        } else {
                            dec.reader.seek(SeekFrom::Current(len as i64 + 4)).unwrap();
                        }
                    }
                    Err(_) => break,
                }
            }
        }

        if dec.idat_remaining == 0 { break; }

        let to_read = cmp::min(dec.idat_remaining, in_buf.len());
        let n = dec.reader.read(&mut in_buf[..to_read]).unwrap();
        dec.idat_remaining -= n;

        let mut in_pos = 0;
        while in_pos < n {
            let before_out = dec.decompressor.total_out();
            let before_in = dec.decompressor.total_in();

            let _ = dec.decompressor.decompress(
                &in_buf[in_pos..n],
                &mut out_buf,
                FlushDecompress::None,
            ).unwrap();

            let consumed = (dec.decompressor.total_in() - before_in) as usize;
            let produced = (dec.decompressor.total_out() - before_out) as usize;

            in_pos += consumed;
            dec.uncompressed_buffer.extend_from_slice(&out_buf[..produced]);
        }
    }

    let lines_to_process = cmp::min(
        num_scanlines as usize,
        dec.uncompressed_buffer.len() / bytes_needed_per_line
    );

    dec.output_buffer.resize(lines_to_process * dec.bytes_per_scanline, 0);

    {
        let bytes_per_scanline = dec.bytes_per_scanline;
        let bpp = dec.bytes_per_pixel;
        let uncompressed = &dec.uncompressed_buffer;
        let output = &mut dec.output_buffer;
        let prev = &mut dec.prev_scanline;

        for i in 0..lines_to_process {
            let start = i * bytes_needed_per_line;
            let filter_type = uncompressed[start];
            let raw_scanline = &uncompressed[start + 1 .. start + bytes_needed_per_line];

            let out_start = i * bytes_per_scanline;
            let out_slice = &mut output[out_start .. out_start + bytes_per_scanline];

            unfilter_scanline(
                filter_type,
                bpp,
                bytes_per_scanline,
                prev,
                raw_scanline,
                out_slice,
            );
            prev.copy_from_slice(out_slice);
        }
    }

    dec.uncompressed_buffer.drain(..lines_to_process * bytes_needed_per_line);
    dec.current_y += lines_to_process as u32;

    ScanlinesResult {
        data: dec.output_buffer.as_ptr(),
        size: dec.output_buffer.len(),
    }
}

#[no_mangle]
pub extern "C" fn close_png(handle: *mut PngDecoder) -> bool {
    if handle.is_null() {
        return false;
    }
    unsafe {
        let _ = Box::from_raw(handle);
    }
    true
}
