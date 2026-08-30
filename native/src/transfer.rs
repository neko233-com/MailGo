use anyhow::{anyhow, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

const TRANSFER_SCHEMA_VERSION: u32 = 1;
const MAX_TRANSFER_ACCOUNTS: usize = 64;
const MAX_TRANSFER_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
const MIN_PASSPHRASE_CHARS: usize = 12;
const MAX_PASSPHRASE_CHARS: usize = 256;
const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 12;
const KEY_BYTES: usize = 32;
const TRANSFER_AAD: &[u8] = b"MailGo encrypted account bundle v1";

#[derive(Debug, Clone)]
pub struct TransferAccount {
    pub account: crate::PersistedAccount,
    pub credential: String,
}

impl Drop for TransferAccount {
    fn drop(&mut self) {
        self.credential.zeroize();
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransferPayload {
    schema_version: u32,
    product: String,
    exported_at: String,
    accounts: Vec<TransferPayloadAccount>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TransferPayloadAccount {
    account: crate::PersistedAccount,
    credential: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransferEnvelope {
    schema_version: u32,
    product: String,
    kdf: String,
    cipher: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

fn validate_passphrase(passphrase: &str) -> Result<()> {
    let length = passphrase.chars().count();
    if !(MIN_PASSPHRASE_CHARS..=MAX_PASSPHRASE_CHARS).contains(&length) {
        return Err(anyhow!(
            "transfer passphrase must contain 12 to 256 characters"
        ));
    }
    Ok(())
}

fn derive_key(passphrase: &str, salt: &[u8]) -> Result<Zeroizing<[u8; KEY_BYTES]>> {
    let params = Params::new(19 * 1024, 3, 1, Some(KEY_BYTES))
        .map_err(|error| anyhow!("create transfer key derivation parameters: {error}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0_u8; KEY_BYTES]);
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut *key)
        .map_err(|error| anyhow!("derive transfer key: {error}"))?;
    Ok(key)
}

pub fn encrypt_accounts(accounts: &[TransferAccount], passphrase: &str) -> Result<String> {
    validate_passphrase(passphrase)?;
    if accounts.is_empty() {
        return Err(anyhow!("at least one account is required"));
    }
    if accounts.len() > MAX_TRANSFER_ACCOUNTS {
        return Err(anyhow!("too many accounts in transfer bundle"));
    }
    if accounts
        .iter()
        .any(|record| record.credential.is_empty() || record.credential.len() > 64 * 1024)
    {
        return Err(anyhow!("invalid account credential for transfer"));
    }

    let payload = TransferPayload {
        schema_version: TRANSFER_SCHEMA_VERSION,
        product: "MailGo".to_string(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        accounts: accounts
            .iter()
            .map(|record| TransferPayloadAccount {
                account: record.account.clone(),
                credential: record.credential.clone(),
            })
            .collect(),
    };
    let plaintext =
        Zeroizing::new(serde_json::to_vec(&payload).context("serialize encrypted account bundle")?);
    if plaintext.len() > MAX_TRANSFER_BUNDLE_BYTES {
        return Err(anyhow!("encrypted account bundle is too large"));
    }

    let mut salt = [0_u8; SALT_BYTES];
    let mut nonce = [0_u8; NONCE_BYTES];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);
    let key = derive_key(passphrase, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&*key));
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext.as_ref(),
                aad: TRANSFER_AAD,
            },
        )
        .map_err(|_| anyhow!("encrypt account bundle"))?;
    let envelope = TransferEnvelope {
        schema_version: TRANSFER_SCHEMA_VERSION,
        product: "MailGo".to_string(),
        kdf: "argon2id".to_string(),
        cipher: "chacha20poly1305".to_string(),
        salt: STANDARD.encode(salt),
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
    };
    let serialized =
        serde_json::to_string_pretty(&envelope).context("serialize encrypted account envelope")?;
    if serialized.len() > MAX_TRANSFER_BUNDLE_BYTES {
        return Err(anyhow!("encrypted account bundle is too large"));
    }
    Ok(serialized)
}

pub fn decrypt_accounts(serialized: &str, passphrase: &str) -> Result<Vec<TransferAccount>> {
    validate_passphrase(passphrase)?;
    if serialized.len() > MAX_TRANSFER_BUNDLE_BYTES {
        return Err(anyhow!("encrypted account bundle is too large"));
    }
    let envelope: TransferEnvelope =
        serde_json::from_str(serialized).context("parse encrypted account bundle")?;
    if envelope.schema_version != TRANSFER_SCHEMA_VERSION
        || envelope.product != "MailGo"
        || envelope.kdf != "argon2id"
        || envelope.cipher != "chacha20poly1305"
    {
        return Err(anyhow!("unsupported encrypted account bundle"));
    }
    let salt = STANDARD
        .decode(envelope.salt)
        .context("decode transfer salt")?;
    let nonce = STANDARD
        .decode(envelope.nonce)
        .context("decode transfer nonce")?;
    if salt.len() != SALT_BYTES || nonce.len() != NONCE_BYTES {
        return Err(anyhow!("invalid encrypted account bundle parameters"));
    }
    let ciphertext = STANDARD
        .decode(envelope.ciphertext)
        .context("decode transfer ciphertext")?;
    if ciphertext.is_empty() || ciphertext.len() > MAX_TRANSFER_BUNDLE_BYTES {
        return Err(anyhow!("invalid encrypted account bundle payload"));
    }
    let key = derive_key(passphrase, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&*key));
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: TRANSFER_AAD,
                },
            )
            .map_err(|_| anyhow!("invalid passphrase or corrupted account bundle"))?,
    );
    let payload: TransferPayload =
        serde_json::from_slice(&plaintext).context("parse encrypted account payload")?;
    if payload.schema_version != TRANSFER_SCHEMA_VERSION
        || payload.product != "MailGo"
        || payload.accounts.is_empty()
        || payload.accounts.len() > MAX_TRANSFER_ACCOUNTS
    {
        return Err(anyhow!("invalid encrypted account payload"));
    }
    Ok(payload
        .accounts
        .into_iter()
        .map(|record| TransferAccount {
            account: record.account,
            credential: record.credential,
        })
        .collect())
}

pub fn clear_credentials(accounts: &mut [TransferAccount]) {
    for account in accounts {
        account.credential.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_account() -> crate::PersistedAccount {
        crate::PersistedAccount {
            id: "fixture-google".to_string(),
            provider: "google".to_string(),
            label: "Google fixture".to_string(),
            email: "fixture@example.invalid".to_string(),
            unread: 0,
            accent: "#5f70ee".to_string(),
            status: "synced".to_string(),
            last_sync: "fixture".to_string(),
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            authentication: Some("oauth2".to_string()),
        }
    }

    #[test]
    fn encrypted_bundle_round_trips_without_plaintext_secret() {
        let accounts = vec![TransferAccount {
            account: fixture_account(),
            credential: "fixture-token-value".to_string(),
        }];
        let bundle = encrypt_accounts(&accounts, "correct horse battery").unwrap();
        assert!(!bundle.contains("fixture-token-value"));
        let mut restored = decrypt_accounts(&bundle, "correct horse battery").unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].account.id, "fixture-google");
        assert_eq!(restored[0].credential, "fixture-token-value");
        clear_credentials(&mut restored);
        assert!(restored[0].credential.is_empty());
    }

    #[test]
    fn wrong_passphrase_is_rejected() {
        let accounts = vec![TransferAccount {
            account: fixture_account(),
            credential: "fixture-token-value".to_string(),
        }];
        let bundle = encrypt_accounts(&accounts, "correct horse battery").unwrap();
        assert!(decrypt_accounts(&bundle, "incorrect horse battery").is_err());
    }

    #[test]
    fn weak_passphrases_are_rejected() {
        let accounts = vec![TransferAccount {
            account: fixture_account(),
            credential: "fixture-token-value".to_string(),
        }];
        assert!(encrypt_accounts(&accounts, "too-short").is_err());
    }
}
