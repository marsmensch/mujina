//! CRC-32 used by the Aura ASIC protocol.
//!
//! Reflected CRC-32/IEEE-802.3 style with polynomial `0xEDB88320`,
//! initial value `0xFFFFFFFF` and no final XOR. Applied to the frame
//! bytes preceding the trailing CRC word (bytes `[0..len-4]`) and
//! transmitted little-endian.

/// Aura CRC-32 over `data`.
///
/// Bitwise reflected CRC-32 with polynomial `0xEDB88320`, init
/// `0xFFFFFFFF`, xorout `0`. The result is written to the wire
/// little-endian.
pub fn aura_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::aura_crc32;

    #[test]
    fn known_value() {
        // "123456789" check value for this CRC-32 variant (init all-ones,
        // xorout 0, reflected poly 0xEDB88320).
        assert_eq!(aura_crc32(b"123456789"), 0x340b_c6d9);
    }
}
