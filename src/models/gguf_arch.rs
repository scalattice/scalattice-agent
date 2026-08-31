//! Read transformer shape from a GGUF header (no llama.cpp). Used to size KV
//! cache before we risk a CUDA alloc that abort()s the worker.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

const GGUF_MAGIC: [u8; 4] = *b"GGUF";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GgufShape {
    pub n_layer: u32,
    pub n_embd: u32,
    pub n_head: u32,
    pub n_head_kv: u32,
}

impl GgufShape {
    pub fn head_dim(self) -> u32 {
        if self.n_head == 0 {
            128
        } else {
            self.n_embd / self.n_head.max(1)
        }
    }

    pub fn usable(self) -> bool {
        self.n_layer > 0 && self.n_embd > 0 && self.n_head > 0 && self.n_head_kv > 0
    }
}

#[derive(Clone, Copy)]
struct ShapeCacheEntry {
    len: u64,
    mtime_secs: u64,
    shape: Option<GgufShape>,
}

fn shape_cache() -> &'static Mutex<HashMap<PathBuf, ShapeCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, ShapeCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn gguf_shape(path: &Path) -> Option<GgufShape> {
    let meta = std::fs::metadata(path).ok()?;
    let len = meta.len();
    let mtime = mtime_secs(&meta);
    if let Ok(cache) = shape_cache().lock() {
        if let Some(hit) = cache.get(path) {
            if hit.len == len && hit.mtime_secs == mtime {
                return hit.shape;
            }
        }
    }
    let shape = gguf_shape_uncached(path).ok().flatten();
    if let Ok(mut cache) = shape_cache().lock() {
        cache.insert(
            path.to_path_buf(),
            ShapeCacheEntry {
                len,
                mtime_secs: mtime,
                shape,
            },
        );
    }
    shape
}

fn gguf_shape_uncached(path: &Path) -> std::io::Result<Option<GgufShape>> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)?;
    if magic != GGUF_MAGIC {
        return Ok(None);
    }
    let version = read_u32(&mut file)?;
    if !(2..=3).contains(&version) {
        return Ok(None);
    }
    let _n_tensors = read_u64(&mut file)?;
    let n_kv = read_u64(&mut file)?;

    let mut arch = String::new();
    let mut scalars: HashMap<String, u64> = HashMap::new();

    for _ in 0..n_kv {
        let key = read_string(&mut file)?;
        let value_type = read_u32(&mut file)?;
        if key == "general.architecture" && value_type == 8 {
            arch = read_string(&mut file)?;
            continue;
        }
        if let Some(n) = read_unsigned(&mut file, value_type)? {
            scalars.insert(key, n);
        } else {
            skip_value(&mut file, value_type)?;
        }
    }

    if arch.is_empty() {
        arch = "llama".into();
    }
    let Some(n_layer) = scalar(&scalars, &arch, "block_count") else {
        return Ok(None);
    };
    let Some(n_embd) = scalar(&scalars, &arch, "embedding_length") else {
        return Ok(None);
    };
    let Some(n_head) = scalar(&scalars, &arch, "attention.head_count") else {
        return Ok(None);
    };
    let n_head_kv = scalar(&scalars, &arch, "attention.head_count_kv").unwrap_or(n_head);
    let shape = GgufShape {
        n_layer: n_layer as u32,
        n_embd: n_embd as u32,
        n_head: n_head as u32,
        n_head_kv: n_head_kv as u32,
    };
    Ok(if shape.usable() { Some(shape) } else { None })
}

fn scalar(map: &HashMap<String, u64>, arch: &str, suffix: &str) -> Option<u64> {
    let key = format!("{arch}.{suffix}");
    map.get(&key).copied().or_else(|| {
        map.iter()
            .find(|(k, _)| k.ends_with(suffix) && k.contains('.'))
            .map(|(_, v)| *v)
    })
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

fn read_string(file: &mut File) -> std::io::Result<String> {
    let len = read_u64(file)?;
    if len > 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "gguf string too large",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn read_unsigned(file: &mut File, value_type: u32) -> std::io::Result<Option<u64>> {
    match value_type {
        4 => Ok(Some(u64::from(read_u32(file)?))),
        5 => {
            let mut buf = [0u8; 4];
            file.read_exact(&mut buf)?;
            Ok(Some(i32::from_le_bytes(buf).max(0) as u64))
        }
        10 => Ok(Some(read_u64(file)?)),
        11 => {
            let mut buf = [0u8; 8];
            file.read_exact(&mut buf)?;
            Ok(Some(i64::from_le_bytes(buf).max(0) as u64))
        }
        _ => Ok(None),
    }
}

fn skip_value(file: &mut File, value_type: u32) -> std::io::Result<()> {
    match value_type {
        0 | 1 | 7 => {
            file.seek(SeekFrom::Current(1))?;
        }
        2 | 3 => {
            file.seek(SeekFrom::Current(2))?;
        }
        4 | 5 | 6 => {
            file.seek(SeekFrom::Current(4))?;
        }
        8 => {
            let len = read_u64(file)?;
            file.seek(SeekFrom::Current(len as i64))?;
        }
        9 => skip_array(file)?,
        10 | 11 | 12 => {
            file.seek(SeekFrom::Current(8))?;
        }
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
    for _ in 0..count.min(10_000_000) {
        skip_value(file, elem_type)?;
    }
    Ok(())
}
