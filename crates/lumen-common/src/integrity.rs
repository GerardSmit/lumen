//! Subresource Integrity metadata verification over the canonical digest and base64 codecs.

use crate::hash::{digest, Algo};

fn metadata(input: &str) -> impl Iterator<Item = (u8, &str)> {
    input.split(|ch| matches!(ch, '\t' | '\n' | '\u{c}' | '\r' | ' ')).filter_map(|item| {
        let expression=item.split('?').next()?;
        let mut parts=expression.split('-');
        let algorithm=parts.next()?;
        let rank=if algorithm.eq_ignore_ascii_case("sha256") {1}
            else if algorithm.eq_ignore_ascii_case("sha384") {2}
            else if algorithm.eq_ignore_ascii_case("sha512") {3}
            else {return None};
        Some((rank,parts.next().unwrap_or("")))
    })
}

/// Unknown algorithms act as absent metadata; only the strongest supported
/// algorithm may match. Unknown options do not affect the expected digest.
/// The empty-metadata fast path performs no hashing or allocation.
pub fn matches(bytes: &[u8], input: &str) -> bool {
    let strongest=metadata(input).map(|(rank,_)|rank).max();
    let Some(rank)=strongest else {return true};
    let algorithm=match rank {1=>Algo::Sha256,2=>Algo::Sha384,_=>Algo::Sha512};
    let actual=crate::codec::base64_encode(&digest(algorithm,bytes),false,true);
    metadata(input).any(|(candidate,expected)|candidate==rank && expected==actual)
}

#[cfg(test)]
mod tests {
    #[test]
    fn specification_integrity_selects_strongest_supported_metadata_over_original_bytes() {
        let sha256="sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";
        assert!(super::matches(b"abc",sha256));
        assert!(!super::matches(b"abcd",sha256));
        assert!(super::matches(b"abc",&format!("unknown-no-hash \t{sha256}?future-option")));
        assert!(super::matches(b"abc","unknown-no-hash"));
        assert!(!super::matches(b"abc",&format!("{sha256} sha512-invalid")),"a matching weak hash cannot override a stronger mismatch");
        assert!(super::matches(b"abc",&format!("sha256-invalid {sha256}")),"any strongest digest may match");
        assert!(!super::matches(b"abc","sha256"),"supported algorithm with an empty digest is a mismatch");
    }
}
