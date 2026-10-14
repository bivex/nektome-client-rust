use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub fn auth_key(device_id: &str) -> String {
    let seed: [u8; 32] = Sha256::digest(device_id.as_bytes()).into();
    let signature = SigningKey::from_bytes(&seed).sign(&seed);
    signature
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn new_device_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_lowercase_128_hex_chars() {
        let key = auth_key("device-1");
        assert_eq!(key.len(), 128);
        assert!(
            key.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}
