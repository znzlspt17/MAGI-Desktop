use serde::Serialize;
use std::io::{Read, Seek, SeekFrom};

const MAX_ITEMS: u64 = 1_000_000;
const MAX_STRING: u64 = 1_048_576;
const MAX_ARRAY: u64 = 1_000_000;
const MAX_HEADER_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct TensorInfo {
    pub name: String,
    pub dimensions: Vec<u64>,
    pub ggml_type: u32,
    pub offset: u64,
}
#[derive(Debug, Serialize)]
pub struct ModelInfo {
    pub version: u32,
    pub architecture: Option<String>,
    pub alignment: u32,
    pub tensors: Vec<TensorInfo>,
    pub inference_supported: bool,
    pub unsupported_reason: String,
}

struct Parser<'a, R> {
    input: &'a mut R,
    len: u64,
}
impl<R: Read + Seek> Parser<'_, R> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut b = [0; N];
        let pos = self.input.stream_position().map_err(|e| e.to_string())?;
        if pos
            .checked_add(N as u64)
            .filter(|&n| n <= MAX_HEADER_BYTES)
            .is_none()
        {
            return Err("header exceeds limit".into());
        }
        self.input
            .read_exact(&mut b)
            .map_err(|_| "truncated GGUF".to_string())?;
        Ok(b)
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.bytes()?))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.bytes()?))
    }
    fn skip(&mut self, n: u64) -> Result<(), String> {
        let pos = self.input.stream_position().map_err(|e| e.to_string())?;
        let end = pos.checked_add(n).ok_or("offset overflow")?;
        if end > MAX_HEADER_BYTES {
            return Err("header exceeds limit".into());
        }
        if end > self.len {
            return Err("truncated GGUF".into());
        }
        self.input
            .seek(SeekFrom::Start(end))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    fn string(&mut self) -> Result<String, String> {
        let n = self.u64()?;
        if n > MAX_STRING {
            return Err("string exceeds limit".into());
        }
        let pos = self.input.stream_position().map_err(|e| e.to_string())?;
        if n > MAX_HEADER_BYTES.saturating_sub(pos) {
            return Err("header exceeds limit".into());
        }
        if n > self.len.saturating_sub(pos) {
            return Err("truncated string".into());
        }
        let mut b = vec![0; n as usize];
        self.input.read_exact(&mut b).map_err(|e| e.to_string())?;
        String::from_utf8(b).map_err(|_| "invalid UTF-8 string".into())
    }
    fn value(&mut self, kind: u32, depth: u32) -> Result<Option<String>, String> {
        match kind {
            0 | 1 | 7 => self.skip(1)?,
            2 | 3 => self.skip(2)?,
            4..=6 => self.skip(4)?,
            10..=12 => self.skip(8)?,
            8 => return Ok(Some(self.string()?)),
            9 => {
                if depth != 0 {
                    return Err("nested arrays unsupported".into());
                }
                let t = self.u32()?;
                if t > 12 || t == 9 {
                    return Err("invalid array element type".into());
                }
                let n = self.u64()?;
                if n > MAX_ARRAY {
                    return Err("array exceeds limit".into());
                }
                for _ in 0..n {
                    self.value(t, depth + 1)?;
                }
            }
            _ => return Err(format!("unknown metadata value type {kind}")),
        };
        Ok(None)
    }
}

/// Inspect bounded GGUF metadata without loading tensor payloads or assuming a custom packing ABI.
pub fn inspect<R: Read + Seek>(input: &mut R) -> Result<ModelInfo, String> {
    let len = input.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    input.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut p = Parser { input, len };
    if &p.bytes::<4>()? != b"GGUF" {
        return Err("invalid GGUF magic".into());
    }
    let version = p.u32()?;
    if version != 2 && version != 3 {
        return Err(format!("unsupported GGUF version {version}"));
    }
    let nt = p.u64()?;
    let nm = p.u64()?;
    if nt > MAX_ITEMS || nm > MAX_ITEMS {
        return Err("item count exceeds limit".into());
    }
    let mut architecture = None;
    let mut alignment = 32u32;
    for _ in 0..nm {
        let key = p.string()?;
        let kind = p.u32()?;
        if key == "general.alignment" {
            if kind != 4 {
                return Err("invalid alignment type".into());
            }
            alignment = p.u32()?;
        } else {
            let value = p.value(kind, 0)?;
            if key == "general.architecture" {
                architecture = Some(value.ok_or("invalid architecture type")?);
            }
        }
    }
    if alignment == 0 || !alignment.is_power_of_two() || alignment > 1_048_576 {
        return Err("invalid alignment".into());
    }
    let mut tensors = Vec::new();
    for _ in 0..nt {
        let name = p.string()?;
        let ndim = p.u32()?;
        if !(1..=4).contains(&ndim) {
            return Err("invalid tensor dimensions".into());
        }
        let mut dimensions = Vec::new();
        let mut elements = 1u64;
        for _ in 0..ndim {
            let d = p.u64()?;
            if d == 0 {
                return Err("zero tensor dimension".into());
            }
            elements = elements.checked_mul(d).ok_or("tensor dimension overflow")?;
            dimensions.push(d);
        }
        let ggml_type = p.u32()?;
        let offset = p.u64()?;
        if offset % u64::from(alignment) != 0 {
            return Err("misaligned tensor offset".into());
        }
        tensors.push(TensorInfo {
            name,
            dimensions,
            ggml_type,
            offset,
        });
    }
    if !tensors.is_empty() {
        let pos = p.input.stream_position().map_err(|e| e.to_string())?;
        let a = u64::from(alignment);
        let data = pos.checked_add(a - 1).ok_or("alignment overflow")? & !(a - 1);
        for t in &tensors {
            let start = data.checked_add(t.offset).ok_or("tensor offset overflow")?;
            if start >= len {
                return Err("tensor offset outside file".into());
            }
            // Only fixed-width, unquantized formats have known lengths here; custom formats stay opaque.
            let width = match t.ggml_type {
                0 => Some(4u64),
                1 => Some(2),
                _ => None,
            };
            if let Some(width) = width {
                let count = t
                    .dimensions
                    .iter()
                    .try_fold(1u64, |a, &b| a.checked_mul(b))
                    .ok_or("tensor size overflow")?;
                let end = count
                    .checked_mul(width)
                    .and_then(|s| start.checked_add(s))
                    .ok_or("tensor size overflow")?;
                if end > len {
                    return Err("tensor payload outside file".into());
                }
            }
        }
    }
    Ok(ModelInfo {
        version,
        architecture,
        alignment,
        tensors,
        inference_supported: false,
        unsupported_reason: "Gemma inference and custom ternary storage ABI are not implemented"
            .into(),
    })
}

mod ternary;

pub use ternary::ternary_dot;

pub mod hip;
