use flate2::{Decompress, FlushDecompress};
use std::cmp;
use std::ffi::CStr;
use std::fs::File;
use std::io::{BufReader, Read};
use std::os::raw::c_char;
use std::ptr;

const PNG_MAGIC: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

pub struct PngDecoder {
    reader: BufReader<Box<dyn Read>>,
    decompressor: Decompress,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8,
    bytes_per_pixel: usize,
    pub bytes_per_scanline: usize, // Input stride
    pub output_stride: usize,      // Output stride

    current_y: u32,
    idat_remaining: usize,

    uncompressed_buffer: Vec<u8>,
    prev_scanline: Vec<u8>,
    pending_drain: usize, // Tracks bytes to remove on the next call to avoid re-allocating

    palette: Vec<[u8; 3]>,
    transparency: Vec<u8>,
}

#[repr(C)]
pub struct ScanlinesResult {
    pub data: *const u8,
    pub size: usize,
}

pub type PngReadCallback = extern "C" fn(user_data: *mut std::ffi::c_void, buf: *mut u8, len: usize) -> usize;

struct FfiReader {
    cb: PngReadCallback,
    user_data: *mut std::ffi::c_void,
}

impl Read for FfiReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let bytes_read = (self.cb)(self.user_data, buf.as_mut_ptr(), buf.len());
        Ok(bytes_read)
    }
}

fn skip_bytes<R: Read>(reader: &mut R, count: u64) -> Result<(), String> {
    let mut take = reader.take(count);
    std::io::copy(&mut take, &mut std::io::sink()).map_err(|e| e.to_string())?;
    Ok(())
}

// Runs the filter logic strictly in-place with zero loop branching and zero modulo operations.
fn unfilter_scanline_inplace(
    filter_type: u8,
    bpp: usize,
    bytes_per_scanline: usize,
    prev_scanline: &mut [u8],
    buffer: &mut [u8],
    src_start: usize,
    dest_start: usize,
) {
    match filter_type {
        0 => {
            // None: Direct copy/shift
            for i in 0..bytes_per_scanline {
                let val = buffer[src_start + i];
                buffer[dest_start + i] = val;
                prev_scanline[i] = val;
            }
        }
        1 => {
            // Sub: Depends only on 'left'
            for i in 0..bytes_per_scanline {
                let left = if i >= bpp { buffer[dest_start + i - bpp] } else { 0 };
                let val = buffer[src_start + i].wrapping_add(left);
                buffer[dest_start + i] = val;
                prev_scanline[i] = val;
            }
        }
        2 => {
            // Up: Depends only on 'up'
            for i in 0..bytes_per_scanline {
                let up = prev_scanline[i];
                let val = buffer[src_start + i].wrapping_add(up);
                buffer[dest_start + i] = val;
                prev_scanline[i] = val;
            }
        }
        3 => {
            // Average: Depends on 'left' and 'up'
            for i in 0..bytes_per_scanline {
                let left = if i >= bpp { buffer[dest_start + i - bpp] } else { 0 };
                let up = prev_scanline[i];
                let avg = ((left as u16 + up as u16) / 2) as u8;
                let val = buffer[src_start + i].wrapping_add(avg);
                buffer[dest_start + i] = val;
                prev_scanline[i] = val;
            }
        }
        4 => {
            // Paeth: Depends on 'left', 'up', and 'up_left'
            let mut up_left_buf = [0u8; 8];
            let mut up_left_idx = 0;

            for i in 0..bytes_per_scanline {
                let left = if i >= bpp { buffer[dest_start + i - bpp] } else { 0 };
                let up = prev_scanline[i];
                let up_left = if i >= bpp { up_left_buf[up_left_idx] } else { 0 };

                let p = left as i32 + up as i32 - up_left as i32;
                let pa = (p - left as i32).abs();
                let pb = (p - up as i32).abs();
                let pc = (p - up_left as i32).abs();

                let pr = if pa <= pb && pa <= pc { left } else if pb <= pc { up } else { up_left };
                let val = buffer[src_start + i].wrapping_add(pr);

                // Safe sliding pointer avoids expensive modulo (%) operations
                if bpp > 0 {
                    up_left_buf[up_left_idx] = up;
                    up_left_idx += 1;
                    if up_left_idx == bpp {
                        up_left_idx = 0;
                    }
                }

                buffer[dest_start + i] = val;
                prev_scanline[i] = val;
            }
        }
        _ => {}
    }
}

fn expand_indexed_inplace(
    palette: &[[u8; 3]],
    transparency: &[u8],
    bit_depth: u8,
    buffer: &mut [u8],
    num_rows: usize,
    packed_row_stride: usize,
    expanded_row_stride: usize,
) {
    let pixels_per_byte = 8 / bit_depth as usize;
    let mask = (1usize << bit_depth as usize) - 1;
    let pixel_stride = if !transparency.is_empty() { 4 } else { 3 };

    let ptr = buffer.as_mut_ptr();

    for row in (0..num_rows).rev() {
        let src_row_start = row * packed_row_stride;
        let dst_row_start = row * expanded_row_stride;

        // Start write pointer at the very end of the current expanded scanline
        let mut write_idx = dst_row_start + expanded_row_stride;

        for i in (0..packed_row_stride).rev() {
            let byte = unsafe { *ptr.add(src_row_start + i) };

            for p in (0..pixels_per_byte).rev() {
                let shift = p * bit_depth as usize;
                let idx = (byte as usize >> shift) & mask;

                // Unconditionally move write pointer back by the stride
                write_idx -= pixel_stride;

                let color = palette.get(idx).unwrap_or(&[0, 0, 0]);
                unsafe {
                    *ptr.add(write_idx) = color[0];
                    *ptr.add(write_idx + 1) = color[1];
                    *ptr.add(write_idx + 2) = color[2];
                    if pixel_stride == 4 {
                        *ptr.add(write_idx + 3) = *transparency.get(idx).unwrap_or(&255);
                    }
                }
            }
        }
    }
}

impl PngDecoder {
    fn new(reader: Box<dyn Read>) -> Result<Self, String> {
        let mut reader = BufReader::new(reader);

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
        let idat_remaining;

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

        skip_bytes(&mut reader, 4)?; // Skip IHDR CRC

        // Parse Palette Chunks
        let mut palette = Vec::new();
        let mut transparency = Vec::new();

        // We must parse chunks between IHDR and IDAT
        loop {
            let (len, chunk_type) = Self::read_chunk_header(&mut reader)?;
            match &chunk_type {
                b"PLTE" => {
                    let mut data = vec![0u8; len];
                    reader.read_exact(&mut data).map_err(|_| "Failed to read PLTE")?;
                    skip_bytes(&mut reader, 4)?; // CRC
                    for chunk in data.chunks_exact(3) {
                        palette.push([chunk[0], chunk[1], chunk[2]]);
                    }
                }
                b"tRNS" => {
                    let mut data = vec![0u8; len];
                    reader.read_exact(&mut data).map_err(|_| "Failed to read tRNS")?;
                    skip_bytes(&mut reader, 4)?; // CRC
                    transparency = data;
                }
                b"IDAT" => {
                    // Backtrack the reader or store the chunk to handle in decode_scanlines
                    // For simplicity, stop here and handle IDAT in decode_scanlines
                    idat_remaining = len;
                    break;
                }
                _ => { skip_bytes(&mut reader, (len + 4) as u64)?; }
            }
        }

        let bytes_per_scanline = (width as usize * bits_per_pixel + 7) / 8; // Input Stride
        let output_stride = if color_type == 3 {
            let pixel_stride = if !transparency.is_empty() { 4 } else { 3 };
            width as usize * pixel_stride
        } else {
            bytes_per_scanline
        };

        Ok(Self {
            reader,
            decompressor: Decompress::new(true),
            width,
            height,
            bit_depth,
            color_type,
            bytes_per_pixel,
            bytes_per_scanline,
            output_stride,
            current_y: 0,
            idat_remaining: idat_remaining,
            uncompressed_buffer: Vec::new(),
            prev_scanline: vec![0; bytes_per_scanline],
            pending_drain: 0,
            palette: palette,
            transparency: transparency,
        })
    }

    fn read_chunk_header<R: Read>(reader: &mut R) -> Result<(usize, [u8; 4]), String> {
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
    bytes_per_scanline: *mut usize,
) -> *mut PngDecoder {
    let c_str = unsafe { CStr::from_ptr(filename) };
    let path = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return ptr::null_mut(),
    };

    match PngDecoder::new(Box::new(file)) {
        Ok(decoder) => unsafe {
            if !width.is_null() { *width = decoder.width; }
            if !height.is_null() { *height = decoder.height; }
            if !bit_depth.is_null() { *bit_depth = decoder.bit_depth; }
            if !color_type.is_null() { *color_type = decoder.color_type; }
            if !bytes_per_scanline.is_null() { *bytes_per_scanline = decoder.output_stride }
            Box::into_raw(Box::new(decoder))
        },
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn open_png_stream(
    read_cb: PngReadCallback,
    user_data: *mut std::ffi::c_void,
    width: *mut u32,
    height: *mut u32,
    bit_depth: *mut u8,
    color_type: *mut u8,
    bytes_per_scanline: *mut usize,
) -> *mut PngDecoder {
    let reader = Box::new(FfiReader { cb: read_cb, user_data });

    match PngDecoder::new(reader) {
        Ok(decoder) => unsafe {
            if !width.is_null() { *width = decoder.width; }
            if !height.is_null() { *height = decoder.height; }
            if !bit_depth.is_null() { *bit_depth = decoder.bit_depth; }
            if !color_type.is_null() { *color_type = decoder.color_type; }
            if !bytes_per_scanline.is_null() { *bytes_per_scanline = decoder.output_stride }
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

    if dec.pending_drain > 0 {
        dec.uncompressed_buffer.drain(..dec.pending_drain);
        dec.pending_drain = 0;
    }

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
                skip_bytes(&mut dec.reader, 4).unwrap();
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
                            skip_bytes(&mut dec.reader, (len + 4) as u64).unwrap();
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

    let bpp = dec.bytes_per_pixel;
    let uncompressed = &mut dec.uncompressed_buffer;
    let prev = &mut dec.prev_scanline;

    for i in 0..lines_to_process {
        let src_start = i * bytes_needed_per_line;
        let filter_type = uncompressed[src_start];

        let dest_start = i * dec.bytes_per_scanline;

        unfilter_scanline_inplace(
            filter_type,
            bpp,
            dec.bytes_per_scanline,
            prev,
            uncompressed,
            src_start + 1,
            dest_start,
        );
    }

    dec.pending_drain = lines_to_process * bytes_needed_per_line;
    dec.current_y += lines_to_process as u32;
    if dec.color_type == 3 {
        let packed_size_with_filters = lines_to_process * bytes_needed_per_line;
        let expanded_size = lines_to_process * dec.output_stride;

        let original_len = dec.uncompressed_buffer.len();
        let remainder_size = original_len - packed_size_with_filters;

        // Resize single buffer to hold the expanded pixels PLUS the zlib remainder
        dec.uncompressed_buffer.resize(expanded_size + remainder_size, 0);

        // Shift the untouched zlib stream data safely out of the way
        if remainder_size > 0 {
            dec.uncompressed_buffer.copy_within(
                packed_size_with_filters..original_len,
                expanded_size
            );
        }

        // Expand the compactly packed unfiltered pixels strictly in-place
        expand_indexed_inplace(
            &dec.palette,
            &dec.transparency,
            dec.bit_depth,
            &mut dec.uncompressed_buffer[..expanded_size],
            lines_to_process,
            dec.bytes_per_scanline,
            dec.output_stride,
        );

        // Set the drain window to match the expanded output footprint
        dec.pending_drain = expanded_size;

        ScanlinesResult {
            data: dec.uncompressed_buffer.as_ptr(),
            size: expanded_size,
        }
    } else {
        ScanlinesResult {
            data: dec.uncompressed_buffer.as_ptr(),
            size: lines_to_process * dec.bytes_per_scanline,
        }
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
