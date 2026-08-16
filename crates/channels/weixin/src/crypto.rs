//! AES-128-ECB encryption/decryption for WeChat iLink CDN media.
//!
//! All CDN media uses AES-128-ECB with PKCS7-style padding.
//! Ciphertext size = ceil((plaintext_size + 1) / 16) * 16.

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
use base64::Engine;
use reqwest::Client;
use tracing::warn;

use types::error::{CarrierError, CarrierResult};

use crate::models::CDN_BASE_URL;

type Aes128 = aes::Aes128;

/// Compute AES-ECB padded ciphertext size.
pub fn aes_ecb_padded_size(plaintext_len: usize) -> usize {
    (plaintext_len + 1).div_ceil(16) * 16
}

/// AES-128-ECB decrypt. Returns plaintext (trailing zeros trimmed).
pub fn aes_128_ecb_decrypt(ciphertext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut buf = ciphertext.to_vec();

    for chunk in buf.chunks_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.decrypt_block(block);
    }

    // Trim trailing zeros
    let end = buf
        .iter()
        .rposition(|&b| b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    buf.truncate(end);
    buf
}

/// Parse AES key from CDNMedia.aes_key field.
///
/// The key can be:
/// - Raw 16 bytes: base64_decode(aes_key) = 16 raw bytes
/// - Hex-encoded: base64_decode(aes_key) = 32 ASCII hex chars → 16 bytes
pub fn parse_aes_key(aes_key_b64: &str) -> Option<[u8; 16]> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(aes_key_b64)
        .ok()?;

    match decoded.len() {
        16 => {
            let mut key = [0u8; 16];
            key.copy_from_slice(&decoded);
            Some(key)
        }
        32 => {
            // Check if it's hex-encoded
            let s = std::str::from_utf8(&decoded).ok()?;
            if s.chars().all(|c| c.is_ascii_hexdigit()) {
                hex::decode_to_slice(s, &mut [0u8; 16]).ok()?;
                let mut key = [0u8; 16];
                hex::decode_to_slice(s, &mut key).ok()?;
                Some(key)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Download and decrypt a file from CDN.
pub async fn cdn_download(http: &Client, url: &str, key: &[u8; 16]) -> CarrierResult<Vec<u8>> {
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("CDN download failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(CarrierError::Network(format!(
            "CDN download HTTP {status}: {body}"
        )));
    }

    let ciphertext = resp
        .bytes()
        .await
        .map_err(|e| CarrierError::Network(format!("CDN download read error: {e}")))?;

    Ok(aes_128_ecb_decrypt(&ciphertext, key))
}

/// Upload encrypted file to CDN. Returns the download encrypted_query_param.
pub async fn cdn_upload(
    http: &Client,
    upload_url: &str,
    ciphertext: &[u8],
) -> CarrierResult<String> {
    let resp = http
        .post(upload_url)
        .header("Content-Type", "application/octet-stream")
        .body(ciphertext.to_vec())
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("CDN upload failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(CarrierError::Network(format!(
            "CDN upload HTTP {status}: {body}"
        )));
    }

    // Extract download param from response header
    let download_param = resp
        .headers()
        .get("x-encrypted-param")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if let Some(ref err) = resp.headers().get("x-error-message") {
        warn!(error = ?err, "CDN upload warning");
    }

    download_param.ok_or_else(|| {
        CarrierError::Internal("CDN upload: no x-encrypted-param in response".to_string())
    })
}

/// Build CDN download URL from encrypt_query_param.
pub fn cdn_download_url(encrypt_query_param: &str) -> String {
    format!(
        "{}/download?encrypted_query_param={}",
        CDN_BASE_URL,
        urlencoding::encode(encrypt_query_param)
    )
}

