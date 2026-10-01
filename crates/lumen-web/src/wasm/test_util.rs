//! A tiny module assembler for the decoder/validator/interpreter tests.

pub fn uleb(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// A module from `(section id, payload)` pairs.
pub fn module(sections: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut m = b"\0asm\x01\0\0\0".to_vec();
    for (id, payload) in sections {
        m.push(*id);
        uleb(payload.len() as u64, &mut m);
        m.extend_from_slice(payload);
    }
    m
}

/// A vector: count, then the items' bytes.
pub fn vec(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    uleb(items.len() as u64, &mut out);
    for i in items {
        out.extend_from_slice(i);
    }
    out
}

pub fn func_type(params: &[u8], results: &[u8]) -> Vec<u8> {
    let mut t = vec![0x60];
    t.extend(vec(&params.iter().map(|&p| vec![p]).collect::<Vec<_>>()));
    t.extend(vec(&results.iter().map(|&r| vec![r]).collect::<Vec<_>>()));
    t
}

/// A code-section entry: no declared locals, `body` followed by `end`.
pub fn body(code: &[u8]) -> Vec<u8> {
    let mut b = vec![0x00];
    b.extend_from_slice(code);
    b.push(0x0b);
    let mut out = Vec::new();
    uleb(b.len() as u64, &mut out);
    out.extend(b);
    out
}

pub fn export(name: &str, kind: u8, index: u32) -> Vec<u8> {
    let mut e = Vec::new();
    uleb(name.len() as u64, &mut e);
    e.extend_from_slice(name.as_bytes());
    e.push(kind);
    uleb(index as u64, &mut e);
    e
}

/// Functions `fN` of the given `(params, results, code)`, each with its own type, exported by
/// name, plus a one-page memory when `memory`.
pub fn funcs(fs: &[(&[u8], &[u8], &[u8])], memory: bool) -> Vec<u8> {
    let types: Vec<_> = fs.iter().map(|(p, r, _)| func_type(p, r)).collect();
    let indices: Vec<_> = (0..fs.len()).map(|i| vec![i as u8]).collect();
    let exports: Vec<_> = (0..fs.len())
        .map(|i| export(&format!("f{i}"), 0, i as u32))
        .collect();
    let bodies: Vec<_> = fs.iter().map(|(_, _, c)| body(c)).collect();
    let mut s = vec![(1, vec(&types)), (3, vec(&indices))];
    if memory {
        s.push((5, vec![1, 0, 1]));
    }
    s.push((7, vec(&exports)));
    s.push((10, vec(&bodies)));
    module(&s)
}

/// One exported function `f0`.
pub fn func(params: &[u8], results: &[u8], code: &[u8]) -> Vec<u8> {
    funcs(&[(params, results, code)], false)
}

pub const I32: u8 = 0x7f;
pub const I64: u8 = 0x7e;
