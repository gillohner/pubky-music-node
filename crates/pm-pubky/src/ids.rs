//! ID generation mirroring `pubky-app-specs` byte for byte.

use base32::Alphabet;
use std::time::{SystemTime, UNIX_EPOCH};

/// Microseconds since the Unix epoch, the unit of every `created_at`.
pub fn now_micros() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before 1970").as_micros() as i64
}

/// Timestamp ID: microseconds → 8 big-endian bytes → Crockford base32 (13 chars).
pub fn timestamp_id(micros: i64) -> String {
    base32::encode(Alphabet::Crockford, &micros.to_be_bytes())
}

/// Hash ID: Blake3 of `data`, first 16 bytes → Crockford base32 (26 chars).
pub fn hash_id(data: &str) -> String {
    let hash = blake3::hash(data.as_bytes());
    base32::encode(Alphabet::Crockford, &hash.as_bytes()[..16])
}

const OCT_FIRST_2024_MICROS: i64 = 1_727_740_800_000_000;
const MAX_FUTURE_MICROS: i64 = 2 * 60 * 60 * 1_000_000;

/// Validate a timestamp ID the way `pubky-app-specs` does.
pub fn validate_timestamp_id(id: &str) -> Result<i64, String> {
    if id.len() != 13 {
        return Err("Validation Error: Invalid ID length: must be 13 characters".into());
    }
    let bytes = base32::decode(Alphabet::Crockford, id).ok_or("Validation Error: Invalid Crockford Base32 encoding")?;
    let bytes: [u8; 8] = bytes.try_into().map_err(|_| "Validation Error: Invalid ID length after decoding".to_string())?;
    let micros = i64::from_be_bytes(bytes);
    if micros < OCT_FIRST_2024_MICROS {
        return Err("Validation Error: Invalid ID, timestamp must be after October 1st, 2024".into());
    }
    if micros > now_micros() + MAX_FUTURE_MICROS {
        return Err("Validation Error: Invalid ID, timestamp is too far in the future".into());
    }
    Ok(micros)
}

/// Validate a hash ID against the data it must hash to.
pub fn validate_hash_id(id: &str, data: &str) -> Result<(), String> {
    let expected = hash_id(data);
    if expected != id {
        return Err(format!("Invalid ID: expected {expected}, found {id}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_id_matches_app_specs_vector() {
        // pubky-app-specs test vector: tag("pubky://user_id/pub/pubky.app/posts/post_id", "cool")
        let uri = "pubky://user_id/pub/pubky.app/posts/post_id";
        assert_eq!(hash_id(&format!("{uri}:cool")), "CBYS8P6VJPHC5XXT4WDW26662W");
        // bookmark(uri)
        assert_eq!(hash_id(uri), "2GN0JCHX9NYXPECQDS8KSMSE7M");
    }

    #[test]
    fn timestamp_id_roundtrip() {
        let now = now_micros();
        let id = timestamp_id(now);
        assert_eq!(id.len(), 13);
        assert_eq!(validate_timestamp_id(&id).unwrap(), now);
        assert!(validate_timestamp_id("0000000000000").is_err());
        assert!(validate_timestamp_id(&timestamp_id(now + 3 * 3600 * 1_000_000)).is_err());
        assert!(validate_hash_id("X", "y").is_err());
    }
}
