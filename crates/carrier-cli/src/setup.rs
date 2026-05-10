//! First-run setup: auto-registration with Hub.
//!
//! No user interaction needed. Generates random credentials,
//! registers with Hub, obtains API key, writes config, enables auto-login.
//! User can change username/password later in the dashboard UI.

use colored::Colorize;
use std::path::Path;

/// Generate a random alphanumeric string of the given length.
fn random_string(len: usize) -> String {
    use rand::Rng;
    let charset = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

/// Generate a random password (alphanumeric, mixed case + digits).
fn random_password(len: usize) -> String {
    use rand::Rng;
    let charset = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

/// Run the first-run setup flow. Zero interaction — random identity.
/// Returns (username, password) if config was written.
pub fn run_first_time_setup(carrier_dir: &Path, hub_url: &str) -> Result<(String, String), String> {
    println!();
    println!(
        "  {} {}",
        ">>".bright_cyan().bold(),
        "Setting up Carrier".bold()
    );
    println!("  {}", "Registering with Hub...".dimmed());
    println!();

    // Auto-generate random credentials
    let username = format!("dev_{}", random_string(8));
    let password = random_password(16);
    let email = format!("{}@device.opencarrier", username);

    println!("  {} Registering with {}...", "-".bright_yellow(), hub_url);

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let api_key: String = rt.block_on(async {
        register_and_get_key(hub_url, &username, &email, &password).await
    })?;

    // Save API key to .env
    let env_path = carrier_dir.join(".env");
    let env_content = format!("OPENCLONE_HUB_KEY={}\n", api_key);
    std::fs::write(&env_path, &env_content).map_err(|e| e.to_string())?;
    crate::restrict_file_permissions(&env_path);

    // Write config.toml with auth enabled
    let password_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(password.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    let config_content = format!(
        r#"# Carrier Agent OS configuration
api_listen = "127.0.0.1:4200"

[brain]
config = "brain.json"

[memory]
decay_rate = 0.05

[auth]
enabled = true
username = "{username}"
password_hash = "{password_hash}"
session_ttl_hours = 168
"#
    );
    let config_path = carrier_dir.join("config.toml");
    std::fs::write(&config_path, &config_content).map_err(|e| e.to_string())?;
    crate::restrict_file_permissions(&config_path);

    // Persist to .env so the kernel picks it up on next start.
    // Called before tokio runtime starts, so no concurrent env access.
    let _ = carrier_kernel::dotenv::save_env_key("OPENCLONE_HUB_KEY", &api_key);

    println!(
        "  {} Device registered and API key saved!",
        "\u{2714}".bright_green()
    );
    println!("  {} Username: {}", "\u{2714}".bright_green(), username);
    println!();

    Ok((username, password))
}

/// Check if first-run setup is needed (no config.toml or no Hub API key).
pub fn needs_setup(carrier_dir: &Path) -> bool {
    let config_path = carrier_dir.join("config.toml");
    if !config_path.exists() {
        return true;
    }
    // Check if .env exists with OPENCLONE_HUB_KEY
    let env_path = carrier_dir.join(".env");
    if let Ok(content) = std::fs::read_to_string(&env_path) {
        for line in content.lines() {
            if let Some(key) = line.strip_prefix("OPENCLONE_HUB_KEY=") {
                if !key.trim().is_empty() {
                    return false;
                }
            }
        }
    }
    // Also check env var
    if let Ok(v) = std::env::var("OPENCLONE_HUB_KEY") {
        if !v.trim().is_empty() {
            return false;
        }
    }
    true
}

/// Save the plain password for auto-login (stored in restricted file).
pub fn save_login_secret(carrier_dir: &Path, password: &str) -> Result<(), String> {
    let secret_path = carrier_dir.join(".login");
    std::fs::write(&secret_path, password).map_err(|e| e.to_string())?;
    crate::restrict_file_permissions(&secret_path);
    Ok(())
}

/// Read the saved login password.
pub fn read_login_secret(carrier_dir: &Path) -> Option<String> {
    let secret_path = carrier_dir.join(".login");
    let password = std::fs::read_to_string(secret_path).ok()?;
    let p = password.trim().to_string();
    if p.is_empty() {
        None
    } else {
        Some(p)
    }
}

async fn register_and_get_key(
    hub_url: &str,
    username: &str,
    email: &str,
    password: &str,
) -> Result<String, String> {
    let jwt = match carrier_clone::hub::register(hub_url, username, email, password).await {
        Ok(token) => token,
        Err(e) => {
            // Check if it's a conflict (already registered)
            if e.to_string().contains("409") {
                println!(
                    "  {} Device already registered, logging in...",
                    "-".bright_yellow()
                );
                return login_and_get_key(hub_url, username, password).await;
            }
            return Err(format!("Registration failed: {e}"));
        }
    };

    carrier_clone::hub::create_api_key(hub_url, &jwt, "carrier")
        .await
        .map_err(|e| format!("Failed to create API key: {e}"))
}

async fn login_and_get_key(
    hub_url: &str,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let jwt = carrier_clone::hub::login(hub_url, username, password)
        .await
        .map_err(|e| format!("Login failed: {e}"))?;

    carrier_clone::hub::create_api_key(hub_url, &jwt, "carrier")
        .await
        .map_err(|e| format!("Failed to create API key: {e}"))
}

/// Check for updates on Hub. Returns the latest version string if newer than current.
pub async fn check_for_update(hub_url: &str) -> Option<String> {
    let current = env!("CARGO_PKG_VERSION");
    carrier_clone::hub::check_update(hub_url, current).await.ok().flatten()
}
