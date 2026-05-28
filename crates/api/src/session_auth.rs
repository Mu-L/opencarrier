//! Stateless session token authentication for the dashboard.
//! Tokens are HMAC-SHA256 signed and contain tenant_id + role + username + expiry.
//!
//! Password hashing uses Argon2id for security against brute-force attacks.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Information extracted from a verified session token.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The tenant's ID (None for admin/no-auth).
    pub tenant_id: Option<String>,
    /// The tenant's role.
    pub role: String,
    /// The login username.
    pub username: String,
}

/// Create a session token: base64(tenant_id:role:username:expiry_unix:hmac_hex)
///
/// Format is backward-compatible: old tokens with 3 segments (username:expiry:sig)
/// are still accepted by `verify_session_token` and treated as admin sessions.
///
/// Returns None if the HMAC secret is empty.
pub fn create_session_token(
    tenant_id: Option<&str>,
    role: &str,
    username: &str,
    secret: &str,
    ttl_hours: u64,
) -> Option<String> {
    use base64::Engine;
    let expiry = chrono::Utc::now().timestamp() + (ttl_hours as i64 * 3600);
    let tid = tenant_id.unwrap_or("_");
    let payload = format!("{tid}:{role}:{username}:{expiry}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(payload.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());
    Some(base64::engine::general_purpose::STANDARD.encode(format!("{payload}:{signature}")))
}

/// Verify a session token. Returns session info if valid and not expired.
///
/// Supports both new format (5 segments: tenant_id:role:username:expiry:sig)
/// and legacy format (3 segments: username:expiry:sig, treated as admin).
pub fn verify_session_token(token: &str, secret: &str) -> Option<SessionInfo> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token)
        .ok()?;
    let decoded_str = String::from_utf8(decoded).ok()?;
    let parts: Vec<&str> = decoded_str.splitn(5, ':').collect();

    match parts.len() {
        // New format: tenant_id:role:username:expiry:sig
        5 => {
            let (tenant_id_str, role, username, expiry_str, provided_sig) =
                (parts[0], parts[1], parts[2], parts[3], parts[4]);

            let expiry: i64 = expiry_str.parse().ok()?;
            if chrono::Utc::now().timestamp() > expiry {
                return None;
            }

            let payload = format!("{}:{}:{}:{}", tenant_id_str, role, username, expiry_str);
            let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
            mac.update(payload.as_bytes());
            let expected_sig = hex::encode(mac.finalize().into_bytes());

            use subtle::ConstantTimeEq;
            if provided_sig.len() != expected_sig.len() {
                return None;
            }
            if provided_sig
                .as_bytes()
                .ct_eq(expected_sig.as_bytes())
                .into()
            {
                let tenant_id = if tenant_id_str == "_" {
                    None
                } else {
                    Some(tenant_id_str.to_string())
                };
                Some(SessionInfo {
                    tenant_id,
                    role: role.to_string(),
                    username: username.to_string(),
                })
            } else {
                None
            }
        }
        // Legacy format: username:expiry:sig
        3 => {
            let (username, expiry_str, provided_sig) = (parts[0], parts[1], parts[2]);

            let expiry: i64 = expiry_str.parse().ok()?;
            if chrono::Utc::now().timestamp() > expiry {
                return None;
            }

            let payload = format!("{username}:{expiry_str}");
            let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
            mac.update(payload.as_bytes());
            let expected_sig = hex::encode(mac.finalize().into_bytes());

            use subtle::ConstantTimeEq;
            if provided_sig.len() != expected_sig.len() {
                return None;
            }
            if provided_sig
                .as_bytes()
                .ct_eq(expected_sig.as_bytes())
                .into()
            {
                Some(SessionInfo {
                    tenant_id: None,
                    role: "admin".to_string(),
                    username: username.to_string(),
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Hash a password with Argon2id for secure config storage.
///
/// SECURITY: Uses Argon2id with recommended parameters:
/// - Memory cost: 64 MB
/// - Time cost: 3 iterations
/// - Parallelism: 4 lanes
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .expect("Argon2 hashing should not fail")
        .to_string()
}

/// Result of password verification with optional auto-upgrade.
pub enum PasswordResult {
    /// Password did not match.
    Invalid,
    /// Password verified against Argon2id hash.
    Valid,
    /// Password verified against legacy SHA256 hash; caller should store the new Argon2id hash.
    Upgraded { new_hash: String },
}

/// Verify a password against a stored hash.
///
/// Supports Argon2id hashes (preferred) and legacy SHA256 hashes (auto-upgraded).
pub fn verify_password(password: &str, stored_hash: &str) -> PasswordResult {
    // Try Argon2id first (new format starts with $argon2)
    if stored_hash.starts_with("$argon2") {
        let parsed = match PasswordHash::new(stored_hash) {
            Ok(p) => p,
            Err(_) => return PasswordResult::Invalid,
        };
        return if Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
        {
            PasswordResult::Valid
        } else {
            PasswordResult::Invalid
        };
    }

    // Legacy SHA256 fallback for migration
    if stored_hash.len() == 64 && stored_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        use sha2::Digest;
        let computed = hex::encode(Sha256::digest(password.as_bytes()));
        use subtle::ConstantTimeEq;
        if computed.len() != stored_hash.len() {
            return PasswordResult::Invalid;
        }
        if computed.as_bytes().ct_eq(stored_hash.as_bytes()).into() {
            tracing::warn!(
                "Legacy SHA256 password hash verified — auto-upgrading to Argon2id"
            );
            let new_hash = hash_password(password);
            return PasswordResult::Upgraded { new_hash };
        }
    }

    PasswordResult::Invalid
}

/// Simple boolean check for password verification (convenience wrapper).
pub fn verify_password_bool(password: &str, stored_hash: &str) -> bool {
    matches!(
        verify_password(password, stored_hash),
        PasswordResult::Valid | PasswordResult::Upgraded { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_and_verify_password() {
        let hash = hash_password("secret123");
        assert!(matches!(verify_password("secret123", &hash), PasswordResult::Valid));
        assert!(matches!(verify_password("wrong", &hash), PasswordResult::Invalid));
    }

    #[test]
    fn test_create_and_verify_new_token() {
        let token = create_session_token(Some("tenant-123"), "tenant", "user1", "my-secret", 1).unwrap();
        let info = verify_session_token(&token, "my-secret").unwrap();
        assert_eq!(info.tenant_id, Some("tenant-123".to_string()));
        assert_eq!(info.role, "tenant");
        assert_eq!(info.username, "user1");
    }

    #[test]
    fn test_create_and_verify_admin_token() {
        let token = create_session_token(None, "admin", "admin", "my-secret", 1).unwrap();
        let info = verify_session_token(&token, "my-secret").unwrap();
        assert_eq!(info.tenant_id, None);
        assert_eq!(info.role, "admin");
        assert_eq!(info.username, "admin");
    }

    #[test]
    fn test_legacy_token_compatibility() {
        // Old format: base64(username:expiry:sig)
        let token = {
            use base64::Engine;
            let expiry = chrono::Utc::now().timestamp() + 3600;
            let payload = format!("admin:{expiry}");
            let mut mac = HmacSha256::new_from_slice(b"my-secret").unwrap();
            mac.update(payload.as_bytes());
            let sig = hex::encode(mac.finalize().into_bytes());
            base64::engine::general_purpose::STANDARD.encode(format!("{payload}:{sig}"))
        };
        let info = verify_session_token(&token, "my-secret").unwrap();
        assert_eq!(info.tenant_id, None);
        assert_eq!(info.role, "admin");
        assert_eq!(info.username, "admin");
    }

    #[test]
    fn test_token_wrong_secret() {
        let token = create_session_token(None, "admin", "admin", "my-secret", 1).unwrap();
        assert!(verify_session_token(&token, "wrong-secret").is_none());
    }

    #[test]
    fn test_token_invalid_base64() {
        assert!(verify_session_token("not-valid-base64!!!", "secret").is_none());
    }

    #[test]
    fn test_password_hash_length_mismatch() {
        assert!(matches!(verify_password("x", "short"), PasswordResult::Invalid));
    }
}
