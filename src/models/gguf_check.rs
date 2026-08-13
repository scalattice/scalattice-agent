//! Lightweight GGUF sanity check (no llama.cpp): verify tensor payloads fit in the file.
//!
//! llama-cpp often surfaces both truncated files and EMFILE as the same Rust error
//! ("null result from llama cpp"). This check lets us quarantine only when the file
//! itself is incomplete.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const GGUF_MAGIC: [u8; 4] = *b"GGUF";

pub fn gguf_payload_in_bounds(path: &Path) -> std::io::Result<bool> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < 24 {
        return Ok(false);
    }

    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)?;
    if magic != GGUF_MAGIC {
        return Ok(false);
    }

    let version = read_u32(&mut file)?;
    if !(2..=3).contains(&version) {
        // Unknown version - don't claim corruption.
        return Ok(true);
    }

    let n_tensors = read_u64(&mut file)?;
    let n_kv = read_u64(&mut file)?;

    for _ in 0..n_kv {
        skip_string(&mut file)?;
        let value_type = read_u32(&mut file)?;
        skip_value(&mut file, value_type)?;
    }

    let mut max_end = 0u64;
    for _ in 0..n_tensors {
        skip_string(&mut file)?;
        let n_dims = read_u32(&mut file)?;
        if n_dims > 4 {
            return Ok(false);
        }
        let mut elements = 1u64;
        for _ in 0..n_dims {
            let dim = read_u64(&mut file)?;
            elements = elements.saturating_mul(dim.max(1));
        }
        let ggml_type = read_u32(&mut file)?;
        let offset = read_u64(&mut file)?;
        let Some(nbytes) = ggml_type_nbytes(ggml_type, elements) else {
            // Unknown quant (e.g. newer MXFP4 variants): skip this tensor's size
            // contribution but keep checking known tensors. Returning Ok(true) here
            // previously marked truncated gpt-oss files as healthy.
            continue;
        };
        let end = offset.saturating_add(nbytes);
        if end > max_end {
            max_end = end;
        }
    }

    // Tensor data block starts after the header; offsets are relative to data section
    // start which is aligned. Conservative check: header position + max offset+size
    // must not exceed the file. Offsets in GGUF are relative to the start of the
    // data section (aligned), not the file start - so compare against file_len using
    // current position as a lower bound for data start.
    let header_end = file.stream_position()?;
    let alignment = 32u64;
    let data_start = (header_end + alignment - 1) / alignment * alignment;
    let needed = data_start.saturating_add(max_end);
    Ok(needed <= file_len)
}

fn read_u32(file: &mut File) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    file.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(file: &mut File) -> std::io::Result<u64> {
    let mut buf = [0u8; 8];
    file.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn skip_string(file: &mut File) -> std::io::Result<()> {
    let len = read_u64(file)?;
    if len > 64 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "gguf string too large",
        ));
    }
    file.seek(SeekFrom::Current(len as i64))?;
    Ok(())
}

fn skip_value(file: &mut File, value_type: u32) -> std::io::Result<()> {
    // Matches ggml GGUF value types.
    match value_type {
        0 => {
            file.seek(SeekFrom::Current(1))?;
        } // UINT8
        1 => {
            file.seek(SeekFrom::Current(1))?;
        } // INT8
        2 => {
            file.seek(SeekFrom::Current(2))?;
        } // UINT16
        3 => {
            file.seek(SeekFrom::Current(2))?;
        } // INT16
        4 => {
            file.seek(SeekFrom::Current(4))?;
        } // UINT32
        5 => {
            file.seek(SeekFrom::Current(4))?;
        } // INT32
        6 => {
            file.seek(SeekFrom::Current(4))?;
        } // FLOAT32
        7 => {
            // BOOL
            file.seek(SeekFrom::Current(1))?;
        }
        8 => skip_string(file)?,                                      // STRING
        9 => skip_array(file)?,                                       // ARRAY
        10 => {
            file.seek(SeekFrom::Current(8))?;
        } // UINT64
        11 => {
            file.seek(SeekFrom::Current(8))?;
        } // INT64
        12 => {
            file.seek(SeekFrom::Current(8))?;
        } // FLOAT64
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown gguf value type",
            ));
        }
    }
    Ok(())
}

fn skip_array(file: &mut File) -> std::io::Result<()> {
    let elem_type = read_u32(file)?;
    let count = read_u64(file)?;
    if count > 10_000_000 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "gguf array too large",
        ));
    }
    for _ in 0..count {
        skip_value(file, elem_type)?;
    }
    Ok(())
}

fn ggml_type_nbytes(ggml_type: u32, nelements: u64) -> Option<u64> {
    // ggml type enum → (block_size, type_size in bytes). None = unknown / skip strict check.
    let (block_size, type_size) = match ggml_type {
        0 => (1u64, 4u64),   // F32
        1 => (1, 2),         // F16
        2 => (32, 18),       // Q4_0
        3 => (32, 20),       // Q4_1
        6 => (32, 22),       // Q5_0
        7 => (32, 24),       // Q5_1
        8 => (32, 34),       // Q8_0
        9 => (32, 36),       // Q8_1
        10 => (256, 84),     // Q2_K
        11 => (256, 110),    // Q3_K
        12 => (256, 144),    // Q4_K
        13 => (256, 176),    // Q5_K
        14 => (256, 210),    // Q6_K
        15 => (256, 292),    // Q8_K
        30 => (1, 2),        // BF16
        39 => (32, 17),      // MXFP4 (QK_MXFP4=32, 17 bytes/block)
        _ => return None,
    };
    let blocks = nelements.div_ceil(block_size);
    Some(blocks.saturating_mul(type_size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rejects_truncated_header() {
        let dir = std::env::temp_dir().join("slt-gguf-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tiny.gguf");
        let mut f = File::create(&path).unwrap();
        f.write_all(b"GGUF").unwrap();
        assert!(!gguf_payload_in_bounds(&path).unwrap_or(true));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn detects_onsite_truncated_gpt_oss_if_present() {
        let path = Path::new(
            "/home/romulus/.cache/scalattice/models/openai__gpt-oss-20b/openai_gpt-oss-20b-Q4_K_M.gguf",
        );
        if !path.is_file() {
            return;
        }
        let meta = path.metadata().unwrap();
        // Full bartowski Q4_K_M is ~11.7GB; this fixture is the known truncated copy.
        if meta.len() >= 10_000_000_000 {
            return;
        }
        assert!(
            !gguf_payload_in_bounds(path).unwrap_or(true),
            "truncated gpt-oss GGUF must fail bounds check"
        );
    }
}
