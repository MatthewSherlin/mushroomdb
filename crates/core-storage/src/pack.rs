//! Length-prefixed little-endian packing for V7 snapshot images.

use crate::types::{GraphError, Result};

pub fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn push_u32s(out: &mut Vec<u8>, v: &[u32]) {
    push_u32(out, v.len() as u32);
    out.reserve(v.len().saturating_mul(4));
    for x in v {
        push_u32(out, *x);
    }
}

pub fn push_i64s(out: &mut Vec<u8>, v: &[i64]) {
    push_u32(out, v.len() as u32);
    out.reserve(v.len().saturating_mul(8));
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
}

pub fn push_f64s(out: &mut Vec<u8>, v: &[f64]) {
    push_u32(out, v.len() as u32);
    out.reserve(v.len().saturating_mul(8));
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
}

pub fn push_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    push_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

pub fn read_exact<'a>(src: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = pos.checked_add(n).ok_or_else(|| GraphError::Corrupt {
        detail: "snapshot: packed length overflow".into(),
    })?;
    let s = src.get(*pos..end).ok_or_else(|| GraphError::Corrupt {
        detail: "snapshot: truncated packed image".into(),
    })?;
    *pos = end;
    Ok(s)
}

pub fn read_u32(src: &[u8], pos: &mut usize) -> Result<u32> {
    let s = read_exact(src, pos, 4)?;
    Ok(u32::from_le_bytes(s.try_into().unwrap()))
}

pub fn read_u64(src: &[u8], pos: &mut usize) -> Result<u64> {
    let s = read_exact(src, pos, 8)?;
    Ok(u64::from_le_bytes(s.try_into().unwrap()))
}

pub fn read_u32s(src: &[u8], pos: &mut usize) -> Result<Vec<u32>> {
    let n = read_u32(src, pos)? as usize;
    let bytes = read_exact(src, pos, n.saturating_mul(4))?;
    let mut out = Vec::with_capacity(n);
    for chunk in bytes.chunks_exact(4) {
        out.push(u32::from_le_bytes(chunk.try_into().unwrap()));
    }
    Ok(out)
}

pub fn read_i64s(src: &[u8], pos: &mut usize) -> Result<Vec<i64>> {
    let n = read_u32(src, pos)? as usize;
    let bytes = read_exact(src, pos, n.saturating_mul(8))?;
    let mut out = Vec::with_capacity(n);
    for chunk in bytes.chunks_exact(8) {
        out.push(i64::from_le_bytes(chunk.try_into().unwrap()));
    }
    Ok(out)
}

pub fn read_f64s(src: &[u8], pos: &mut usize) -> Result<Vec<f64>> {
    let n = read_u32(src, pos)? as usize;
    let bytes = read_exact(src, pos, n.saturating_mul(8))?;
    let mut out = Vec::with_capacity(n);
    for chunk in bytes.chunks_exact(8) {
        out.push(f64::from_le_bytes(chunk.try_into().unwrap()));
    }
    Ok(out)
}

pub fn read_str(src: &[u8], pos: &mut usize) -> Result<String> {
    let n = read_u32(src, pos)? as usize;
    let bytes = read_exact(src, pos, n)?;
    String::from_utf8(bytes.to_vec()).map_err(|e| GraphError::Corrupt {
        detail: format!("snapshot: packed string utf8: {e}"),
    })
}
