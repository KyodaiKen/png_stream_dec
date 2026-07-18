use flate2::{Decompress, FlushDecompress};
use std::cmp;
use std::ffi::CStr;
use std::fs::File;
use std::io::{BufReader, Read};
use std::os::raw::c_char;
use std::ptr;
use std::cell::RefCell;
use std::ffi::CString;

thread_local! {
    static LAST_ERROR: RefCell<String> = RefCell::new(String::new());
}

fn set_last_error(msg: &str) {
    LAST_ERROR.with(|e| *e.borrow_mut() = msg.to_string());
}

const PNG_MAGIC: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

pub struct PngDecoder {
    reader: BufReader<Box<dyn Read>>,
    decompressor: Decompress,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8,
    bytes_per_pixel: usize,
    pub bytes_per_scanline: usize,
    pub output_stride: usize,

    current_y: u32,
    idat_remaining: usize,

    uncompressed_buffer: Vec<u8>,
    prev_scanline: Vec<u8>,
    pending_drain: usize,

    palette: Vec<[u8; 3]>,
    transparency: Vec<u8>,

    idat_crc: flate2::Crc,
    idat_corrupted: bool,

    pub meta: AuxiliaryMetadata,
}

#[repr(C)]
pub struct ScanlinesResult {
    pub data: *const u8,
    pub size: usize,
}

pub struct TextMetadata {
    pub keyword: String,
    pub text: String,
    pub language: String,
}

#[derive(Default)]
pub struct AuxiliaryMetadata {
    pub phys_x: u32,
    pub phys_y: u32,
    pub phys_unit: u8,
    pub has_phys: bool,

    pub gamma: u32,
    pub has_gamma: bool,
    pub srgb_intent: u8,
    pub has_srgb: bool,
    pub chrm_data: [u32; 8],
    pub has_chrm: bool,

    pub histogram: Vec<u16>,

    pub unix_epoch: i64,
    pub has_time: bool,

    pub bkgd_bytes: Vec<u8>,

    pub text_chunks: Vec<TextMetadata>,
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

fn utc_to_epoch(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> i64 {
    let y = year as i64;
    let m = month as i64;
    let d = day as i64;

    let m_adj = (m + 9) % 12;
    let y_adj = y - m / 10;

    let days = 365 * y_adj + y_adj / 4 - y_adj / 100 + y_adj / 400 + (m_adj * 306 + 5) / 10 + d - 1;
    let days_since_epoch = days - 719468;

    days_since_epoch * 86400 + (hour as i64) * 3600 + (minute as i64) * 60 + (second as i64)
}

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
            for i in 0..bytes_per_scanline {
                let val = buffer[src_start + i];
                buffer[dest_start + i] = val;
                prev_scanline[i] = val;
            }
        }
        1 => {
            for i in 0..bytes_per_scanline {
                let left = if i >= bpp { buffer[dest_start + i - bpp] } else { 0 };
                let val = buffer[src_start + i].wrapping_add(left);
                buffer[dest_start + i] = val;
                prev_scanline[i] = val;
            }
        }
        2 => {
            for i in 0..bytes_per_scanline {
                let up = prev_scanline[i];
                let val = buffer[src_start + i].wrapping_add(up);
                buffer[dest_start + i] = val;
                prev_scanline[i] = val;
            }
        }
        3 => {
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

        let mut write_idx = dst_row_start + expanded_row_stride;

        for i in (0..packed_row_stride).rev() {
            let byte = unsafe { *ptr.add(src_row_start + i) };

            for p in (0..pixels_per_byte).rev() {
                let shift = p * bit_depth as usize;
                let idx = (byte as usize >> shift) & mask;

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
        reader.read_exact(&mut ihdr).map_err(|_| "Failed to read IHDR payload".to_string())?;

        let mut ihdr_crc = flate2::Crc::new();
        ihdr_crc.update(b"IHDR");
        ihdr_crc.update(&ihdr);
        let mut crc_bytes = [0u8; 4];
        reader.read_exact(&mut crc_bytes).map_err(|_| "Failed to read IHDR CRC".to_string())?;
        if ihdr_crc.sum() != u32::from_be_bytes(crc_bytes) {
            return Err("Corrupt IHDR chunk (CRC mismatch)".to_string());
        }

        let width = u32::from_be_bytes(ihdr[0..4].try_into().map_err(|_| "Malformed IHDR structural width".to_string())?);
        let height = u32::from_be_bytes(ihdr[4..8].try_into().map_err(|_| "Malformed IHDR structural height".to_string())?);
        let bit_depth = ihdr[8];
        let color_type = ihdr[9];
        let interlace = ihdr[12];
        let idat_remaining;

        if interlace != 0 {
            return Err("Adam7 Interlacing is not supported".to_string());
        }

        let channels = match color_type {
            0 => 1,
            2 => 3,
            3 => 1,
            4 => 2,
            6 => 4,
            _ => return Err("Unknown color type".to_string()),
        };

        let bits_per_pixel = channels * bit_depth as usize;
        let bytes_per_pixel = cmp::max(1, bits_per_pixel / 8);

        let mut palette = Vec::new();
        let mut transparency = Vec::new();
        let mut idat_crc = flate2::Crc::new();
        let mut meta = AuxiliaryMetadata::default();

        loop {
            let (len, chunk_type) = Self::read_chunk_header(&mut reader)?;

            if chunk_type == *b"IDAT" {
                idat_remaining = len;
                idat_crc.update(b"IDAT");
                break;
            }

            let mut payload = vec![0u8; len];
            reader.read_exact(&mut payload).map_err(|_| "Failed reading payload".to_string())?;
            let mut chunk_crc_bytes = [0u8; 4];
            reader.read_exact(&mut chunk_crc_bytes).map_err(|_| "Failed reading CRC".to_string())?;

            let mut crc_check = flate2::Crc::new();
            crc_check.update(&chunk_type);
            crc_check.update(&payload);
            if crc_check.sum() != u32::from_be_bytes(chunk_crc_bytes) {
                continue;
            }

            match &chunk_type {
                b"PLTE" => {
                    for chunk in payload.chunks_exact(3) {
                        palette.push([chunk[0], chunk[1], chunk[2]]);
                    }
                }
                b"tRNS" => {
                    transparency = payload;
                }
                b"pHYs" => {
                    if len == 9 {
                        meta.phys_x = u32::from_be_bytes(payload[0..4].try_into().map_err(|_| "Malformed pHYs width".to_string())?);
                        meta.phys_y = u32::from_be_bytes(payload[4..8].try_into().map_err(|_| "Malformed pHYs height".to_string())?);
                        meta.phys_unit = payload[8];
                        meta.has_phys = true;
                    }
                }
                b"gAMA" => {
                    if len == 4 {
                        meta.gamma = u32::from_be_bytes(payload[0..4].try_into().map_err(|_| "Malformed gAMA configuration".to_string())?);
                        meta.has_gamma = true;
                    }
                }
                b"sRGB" => {
                    if len == 1 {
                        meta.srgb_intent = payload[0];
                        meta.has_srgb = true;
                    }
                }
                b"tIME" => {
                    if len == 7 {
                        let year = u16::from_be_bytes(payload[0..2].try_into().map_err(|_| "Malformed tIME year sequence".to_string())?);
                        let month = payload[2];
                        let day = payload[3];
                        let hour = payload[4];
                        let minute = payload[5];
                        let second = payload[6];

                        meta.unix_epoch = utc_to_epoch(year, month, day, hour, minute, second);
                        meta.has_time = true;
                    }
                }
                b"hIST" => {
                    meta.histogram = payload.chunks_exact(2)
                    .map(|c| {
                        let arr: [u8; 2] = c.try_into().unwrap_or([0, 0]);
                        u16::from_be_bytes(arr)
                    })
                    .collect();
                }
                b"bKGD" => {
                    meta.bkgd_bytes = payload;
                }
                b"tEXt" => {
                    if let Some(split) = payload.iter().position(|&b| b == 0) {
                        let keyword = String::from_utf8_lossy(&payload[..split]).into_owned();
                        let text = String::from_utf8_lossy(&payload[split + 1..]).into_owned();
                        meta.text_chunks.push(TextMetadata { keyword, text, language: String::new() });
                    }
                }
                b"zTXt" => {
                    if let Some(split) = payload.iter().position(|&b| b == 0) {
                        let keyword = String::from_utf8_lossy(&payload[..split]).into_owned();
                        let comp_method = payload[split + 1];
                        if comp_method == 0 {
                            let mut deflater = flate2::Decompress::new(true);
                            let mut uncompressed_text = Vec::new();
                            if deflater.decompress(&payload[split + 2..], &mut uncompressed_text, flate2::FlushDecompress::Finish).is_ok() {
                                let text = String::from_utf8_lossy(&uncompressed_text).into_owned();
                                meta.text_chunks.push(TextMetadata { keyword, text, language: String::new() });
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let bytes_per_scanline = (width as usize * bits_per_pixel + 7) / 8;
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
           idat_remaining,
           uncompressed_buffer: Vec::new(),
           prev_scanline: vec![0; bytes_per_scanline],
           pending_drain: 0,
           palette,
           transparency,
           idat_crc,
           idat_corrupted: false,
           meta,
        })
    }

    fn read_chunk_header<R: Read>(reader: &mut R) -> Result<(usize, [u8; 4]), String> {
        let mut head = [0u8; 8];
        if reader.read_exact(&mut head).is_err() {
            return Err("EOF reached".to_string());
        }
        let len = u32::from_be_bytes(head[0..4].try_into().map_err(|_| "Invalid chunk length extraction".to_string())?) as usize;
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
        Err(e) => {
            set_last_error(&e);
            ptr::null_mut()
        }
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
        Err(e) => {
            set_last_error(&e);
            ptr::null_mut()
        }
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

    if dec.idat_corrupted {
        let lines_to_process = num_scanlines as usize;
        dec.uncompressed_buffer.clear();
        let total_size = lines_to_process * dec.output_stride;
        dec.uncompressed_buffer.resize(total_size, 0);
        dec.current_y += lines_to_process as u32;
        dec.pending_drain = total_size;
        return ScanlinesResult {
            data: dec.uncompressed_buffer.as_ptr(),
            size: total_size,
        };
    }

    let bytes_needed_per_line = dec.bytes_per_scanline + 1;
    let total_bytes_needed = (num_scanlines as usize) * bytes_needed_per_line;

    let mut in_buf = [0u8; 8192];
    let mut out_buf = [0u8; 16384];

    while dec.uncompressed_buffer.len() < total_bytes_needed && !dec.idat_corrupted {
        if dec.idat_remaining == 0 {
            if dec.current_y > 0 || !dec.uncompressed_buffer.is_empty() {
                let mut crc_bytes = [0u8; 4];
                if dec.reader.read_exact(&mut crc_bytes).is_ok() {
                    let expected_crc = u32::from_be_bytes(crc_bytes);
                    if dec.idat_crc.sum() != expected_crc {
                        dec.idat_corrupted = true;
                        break;
                    }
                } else {
                    dec.idat_corrupted = true;
                    break;
                }
            }

            loop {
                match PngDecoder::read_chunk_header(&mut dec.reader) {
                    Ok((len, ctype)) => {
                        if ctype == *b"IEND" {
                            break;
                        } else if ctype == *b"IDAT" {
                            dec.idat_remaining = len;
                            dec.idat_crc = flate2::Crc::new();
                            dec.idat_crc.update(b"IDAT");
                            break;
                        } else {
                            let mut chunk_data = vec![0u8; len];
                            if dec.reader.read_exact(&mut chunk_data).is_err() { break; }
                            let mut crc_bytes = [0u8; 4];
                            if dec.reader.read_exact(&mut crc_bytes).is_err() { break; }
                        }
                    }
                    Err(_) => {
                        dec.idat_corrupted = true;
                        break;
                    }
                }
            }
        }

        if dec.idat_remaining == 0 || dec.idat_corrupted { break; }

        let to_read = cmp::min(dec.idat_remaining, in_buf.len());
        let n = match dec.reader.read(&mut in_buf[..to_read]) {
            Ok(n) => n,
            Err(_) => {
                dec.idat_corrupted = true;
                break;
            }
        };
        if n == 0 {
            dec.idat_corrupted = true;
            break;
        }
        dec.idat_remaining -= n;
        dec.idat_crc.update(&in_buf[..n]);

        let mut in_pos = 0;
        while in_pos < n {
            let before_out = dec.decompressor.total_out();
            let before_in = dec.decompressor.total_in();

            let res = dec.decompressor.decompress(
                &in_buf[in_pos..n],
                &mut out_buf,
                FlushDecompress::None,
            );

            if res.is_err() {
                dec.idat_corrupted = true;
                break;
            }

            let consumed = (dec.decompressor.total_in() - before_in) as usize;
            let produced = (dec.decompressor.total_out() - before_out) as usize;

            in_pos += consumed;
            dec.uncompressed_buffer.extend_from_slice(&out_buf[..produced]);
        }
    }

    if dec.idat_corrupted {
        let lines_to_process = num_scanlines as usize;
        dec.uncompressed_buffer.clear();
        let total_size = lines_to_process * dec.output_stride;
        dec.uncompressed_buffer.resize(total_size, 0);
        dec.current_y += lines_to_process as u32;
        dec.pending_drain = total_size;
        return ScanlinesResult {
            data: dec.uncompressed_buffer.as_ptr(),
            size: total_size,
        };
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

        dec.uncompressed_buffer.resize(expanded_size + remainder_size, 0);

        if remainder_size > 0 {
            dec.uncompressed_buffer.copy_within(
                packed_size_with_filters..original_len,
                expanded_size
            );
        }

        expand_indexed_inplace(
            &dec.palette,
            &dec.transparency,
            dec.bit_depth,
            &mut dec.uncompressed_buffer[..expanded_size],
            lines_to_process,
            dec.bytes_per_scanline,
            dec.output_stride,
        );

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

#[no_mangle]
pub extern "C" fn get_last_error() -> *const c_char {
    LAST_ERROR.with(|e| {
        let err = e.borrow();
        // Strip out internal null bytes so initializing CString is structurally guaranteed to pass
        let sanitized: Vec<u8> = err.bytes().filter(|&b| b != 0).collect();
        match CString::new(sanitized) {
            Ok(c_str) => c_str.into_raw(),
                    Err(_) => ptr::null()
        }
    })
}

#[no_mangle]
pub extern "C" fn free_error_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe { let _ = CString::from_raw(ptr); }
    }
}

#[no_mangle]
pub extern "C" fn png_get_physics(
    handle: *mut PngDecoder,
    x: *mut u32,
    y: *mut u32,
    unit: *mut u8
) -> bool {
    let dec = unsafe { &*handle };
    if dec.meta.has_phys {
        unsafe {
            if !x.is_null() { *x = dec.meta.phys_x; }
            if !y.is_null() { *y = dec.meta.phys_y; }
            if !unit.is_null() { *unit = dec.meta.phys_unit; }
        }
        true
    } else {
        false
    }
}

#[no_mangle]
pub extern "C" fn png_get_time(handle: *mut PngDecoder, out_epoch: *mut i64) -> bool {
    let dec = unsafe { &*handle };
    if dec.meta.has_time {
        unsafe { *out_epoch = dec.meta.unix_epoch; }
        true
    } else {
        false
    }
}

#[no_mangle]
pub extern "C" fn png_get_text_count(handle: *mut PngDecoder) -> usize {
    let dec = unsafe { &*handle };
    dec.meta.text_chunks.len()
}

#[no_mangle]
pub extern "C" fn png_get_text_data(
    handle: *mut PngDecoder,
    index: usize,
    out_keyword: *mut *const c_char,
    out_text: *mut *const c_char
) -> bool {
    let dec = unsafe { &*handle };
    if let Some(item) = dec.meta.text_chunks.get(index) {
        // Sanitize string content bytes to completely bypass formatting crash scenarios
        let clean_key: Vec<u8> = item.keyword.bytes().filter(|&b| b != 0).collect();
        let clean_text: Vec<u8> = item.text.bytes().filter(|&b| b != 0).collect();

        if let (Ok(c_keyword), Ok(c_text)) = (CString::new(clean_key), CString::new(clean_text)) {
            unsafe {
                *out_keyword = c_keyword.into_raw();
                *out_text = c_text.into_raw();
            }
            true
        } else {
            false
        }
    } else {
        false
    }
}
