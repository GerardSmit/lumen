//! Fixed-width native GOT relocation records. Code pages are never relocation targets.

pub const RECORD_LEN: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Kind {
    Helper = 1,
    Static = 2,
    Atom = 3,
    Function = 4,
    IcCell = 5,
    NativeImport = 6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reloc {
    pub slot: u32,
    pub kind: Kind,
    pub index: u32,
}

pub fn decode(bytes: &[u8]) -> Result<Vec<Reloc>, &'static str> {
    if bytes.len() % RECORD_LEN != 0 || bytes.len() / RECORD_LEN > u32::MAX as usize {
        return Err("invalid GOT relocation length");
    }
    let mut relocs = Vec::with_capacity(bytes.len() / RECORD_LEN);
    for record in bytes.chunks_exact(RECORD_LEN) {
        let slot = u32::from_le_bytes(record[..4].try_into().unwrap());
        if slot != relocs.len() as u32 {
            return Err("missing or duplicate GOT slot");
        }
        let kind = match u32::from_le_bytes(record[4..8].try_into().unwrap()) {
            1 => Kind::Helper,
            2 => Kind::Static,
            3 => Kind::Atom,
            4 => Kind::Function,
            5 => Kind::IcCell,
            6 => Kind::NativeImport,
            _ => return Err("unknown GOT relocation kind"),
        };
        let index = u32::from_le_bytes(record[8..12].try_into().unwrap());
        relocs.push(Reloc { slot, kind, index });
    }
    Ok(relocs)
}

pub fn encode(relocs: &[Reloc]) -> Result<Vec<u8>, &'static str> {
    let capacity = relocs
        .len()
        .checked_mul(RECORD_LEN)
        .ok_or("too many GOT relocations")?;
    let mut bytes = Vec::with_capacity(capacity);
    for (slot, reloc) in relocs.iter().enumerate() {
        if reloc.slot as usize != slot {
            return Err("missing or duplicate GOT slot");
        }
        bytes.extend_from_slice(&reloc.slot.to_le_bytes());
        bytes.extend_from_slice(&(reloc.kind as u32).to_le_bytes());
        bytes.extend_from_slice(&reloc.index.to_le_bytes());
    }
    Ok(bytes)
}

/// Resolve the whole table before publishing any pointer to the runtime.
pub fn fill(
    got: &mut [usize],
    relocs: &[Reloc],
    mut resolve: impl FnMut(Kind, u32) -> Option<usize>,
) -> Result<(), &'static str> {
    if got.len() != relocs.len() {
        return Err("GOT size mismatch");
    }
    let mut addresses = Vec::with_capacity(relocs.len());
    for (slot, reloc) in relocs.iter().enumerate() {
        if reloc.slot as usize != slot {
            return Err("missing or duplicate GOT slot");
        }
        addresses.push(resolve(reloc.kind, reloc.index).ok_or("unresolved GOT symbol")?);
    }
    got.copy_from_slice(&addresses);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_atomic_fill() {
        let relocs = [
            Reloc {
                slot: 0,
                kind: Kind::Helper,
                index: 7,
            },
            Reloc {
                slot: 1,
                kind: Kind::Function,
                index: 3,
            },
        ];
        let bytes = encode(&relocs).unwrap();
        assert_eq!(decode(&bytes).unwrap(), relocs);
        let mut got = [11, 22];
        assert!(fill(&mut got, &relocs, |_, index| (index == 7).then_some(33)).is_err());
        assert_eq!(got, [11, 22]);
        fill(&mut got, &relocs, |kind, index| {
            Some(kind as usize + index as usize)
        })
        .unwrap();
        assert_eq!(got, [8, 7]);
    }

    #[test]
    fn rejects_bad_records() {
        let valid = encode(&[Reloc {
            slot: 0,
            kind: Kind::Atom,
            index: 1,
        }])
        .unwrap();
        for len in 1..RECORD_LEN {
            assert!(decode(&valid[..len]).is_err());
        }
        let mut bad = valid.clone();
        bad[..4].copy_from_slice(&1u32.to_le_bytes());
        assert!(decode(&bad).is_err());
        let mut bad = valid;
        bad[4..8].copy_from_slice(&7u32.to_le_bytes());
        assert!(decode(&bad).is_err());
    }
}
