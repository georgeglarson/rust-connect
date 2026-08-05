//! API authentication
//!
//! Single Responsibility: Validate API keys from requests.

use crate::utils::errors::Error;

pub fn validate_api_key(key: &str, allowed_keys: &[String]) -> Result<(), Error> {
    // Fail closed: an empty key list must reject, never allow unauthenticated
    // access. Settings loading guarantees at least one key, so reaching this
    // branch means misconfiguration.
    if allowed_keys.is_empty() {
        return Err(Error::Unauthorized(
            "No API keys configured; refusing authentication".to_string(),
        ));
    }

    if constant_time_contains(allowed_keys, key.as_bytes()) {
        Ok(())
    } else {
        Err(Error::Unauthorized("Invalid API key".to_string()))
    }
}

pub fn constant_time_contains(keys: &[String], target: &[u8]) -> bool {
    let mut found = 0u8;
    for k in keys {
        let k_bytes = k.as_bytes();
        if k_bytes.len() == target.len() {
            let mut xor: u8 = 0;
            for (a, b) in k_bytes.iter().zip(target.iter()) {
                xor |= a ^ b;
            }
            if xor == 0 {
                found = 1;
            }
        }
    }
    found == 1
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_no_keys_configured_rejects() {
        let err = validate_api_key("any", &[]).expect_err("empty key list must fail closed");
        assert!(err.to_string().contains("No API keys configured"));
    }

    #[test]
    fn test_valid_key_passes() {
        let keys = vec!["secret123".to_string()];
        assert!(validate_api_key("secret123", &keys).is_ok());
    }

    #[test]
    fn test_wrong_key_fails() {
        let keys = vec!["secret123".to_string()];
        assert!(validate_api_key("wrong", &keys).is_err());
    }

    #[test]
    fn test_constant_time_contains_valid() {
        let keys = vec!["secret123".to_string()];
        assert!(constant_time_contains(&keys, b"secret123"));
    }

    #[test]
    fn test_constant_time_contains_invalid() {
        let keys = vec!["secret123".to_string()];
        assert!(!constant_time_contains(&keys, b"secret124"));
    }

    #[test]
    fn test_constant_time_contains_wrong_length() {
        let keys = vec!["secret123".to_string()];
        assert!(!constant_time_contains(&keys, b"secret12"));
        assert!(!constant_time_contains(&keys, b"secret1234"));
    }

    #[test]
    fn test_constant_time_contains_empty_keys() {
        let keys: Vec<String> = vec![];
        assert!(!constant_time_contains(&keys, b"secret123"));
    }

    #[test]
    fn test_constant_time_contains_multiple_keys() {
        let keys = vec!["key1".to_string(), "key2".to_string(), "key3".to_string()];
        assert!(constant_time_contains(&keys, b"key2"));
        assert!(!constant_time_contains(&keys, b"key4"));
    }
}
