//! CSRF tokens in Django's format, so a host that is (or replaces) a Django
//! site can share the cookie: a 32-character secret kept in a cookie, and
//! in each form a 64-character masked token (a 32-character mask followed
//! by the secret shifted by the mask, mod 62) that changes on every page.

use rand::Rng;
use subtle::ConstantTimeEq;

const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// How long the browser keeps the cookie (Django's `CSRF_COOKIE_AGE`).
pub const COOKIE_MAX_AGE_SECONDS: i64 = 31_449_600;

fn index(byte: u8) -> Option<usize> {
    ALNUM.iter().position(|candidate| *candidate == byte)
}

/// A new secret.
pub fn generate_token() -> String {
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| ALNUM[rng.gen_range(0..ALNUM.len())] as char)
        .collect()
}

/// A masked token carrying `secret`, for one form.
pub fn mask_token(secret: &str) -> String {
    let mask = generate_token();
    let cipher: String = mask
        .bytes()
        .zip(secret.bytes())
        .map(|(m, s)| {
            let sum = index(m).unwrap_or(0) + index(s).unwrap_or(0);
            ALNUM[sum % ALNUM.len()] as char
        })
        .collect();
    format!("{mask}{cipher}")
}

pub fn valid_token(token: &str) -> bool {
    matches!(token.len(), 32 | 64) && token.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The secret a 32-character token or a 64-character masked token carries.
pub fn token_secret(token: &str) -> Option<String> {
    match token.len() {
        32 => Some(token.to_string()),
        64 => {
            let (mask, cipher) = token.split_at(32);
            mask.bytes()
                .zip(cipher.bytes())
                .map(|(m, c)| {
                    let (m, c) = (index(m)? as i64, index(c)? as i64);
                    Some(ALNUM[(c - m).rem_euclid(ALNUM.len() as i64) as usize] as char)
                })
                .collect()
        }
        _ => None,
    }
}

/// True when two tokens (either form) carry the same secret: a cookie and
/// the value a form or header submitted.
pub fn tokens_match(cookie: &str, submitted: &str) -> bool {
    if !valid_token(cookie) || !valid_token(submitted) {
        return false;
    }
    match (token_secret(cookie), token_secret(submitted)) {
        (Some(a), Some(b)) => a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes())),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_token_round_trips_to_its_secret() {
        let secret = generate_token();
        let masked = mask_token(&secret);
        assert_eq!(masked.len(), 64);
        assert!(valid_token(&masked));
        assert_eq!(token_secret(&masked).unwrap(), secret);
        assert!(tokens_match(&secret, &masked));
        assert!(!tokens_match(&generate_token(), &masked));
    }

    #[test]
    fn rejects_non_alphanumeric_tokens() {
        assert!(!valid_token(&"-".repeat(32)));
        assert!(!valid_token("short"));
    }
}
