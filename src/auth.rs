use std::path::PathBuf;

use aes::cipher::{BlockCipherEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};

// AES-128-CFB8 as the Minecraft protocol uses it, with key == IV
pub struct Cfb8 {
    aes: Aes128,
    iv: [u8; 16],
    decrypt: bool,
}

impl Cfb8 {
    pub fn encryptor(key: &[u8; 16]) -> Self {
        Self::new(key, false)
    }

    pub fn decryptor(key: &[u8; 16]) -> Self {
        Self::new(key, true)
    }

    fn new(key: &[u8; 16], decrypt: bool) -> Self {
        Cfb8 {
            aes: Aes128::new(key.into()),
            iv: *key,
            decrypt,
        }
    }

    pub fn apply(&mut self, data: &mut [u8]) {
        for byte in data {
            let mut block = self.iv.into();
            self.aes.encrypt_block(&mut block);
            let out = *byte ^ block[0];
            let cipher_byte = if self.decrypt { *byte } else { out };
            self.iv.copy_within(1.., 0);
            self.iv[15] = cipher_byte;
            *byte = out;
        }
    }
}

// Minecraft's signed hex SHA-1, the "notchian" digest
pub fn server_hash(server_id: &str, secret: &[u8], public_key: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(server_id.as_bytes());
    h.update(secret);
    h.update(public_key);
    let mut digest: Vec<u8> = h.finalize().to_vec();
    let negative = digest[0] & 0x80 != 0;
    if negative {
        let mut carry = true;
        for b in digest.iter_mut().rev() {
            *b = !*b;
            if carry {
                let (v, c) = b.overflowing_add(1);
                *b = v;
                carry = c;
            }
        }
    }
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let hex = hex.trim_start_matches('0');
    let hex = if hex.is_empty() { "0" } else { hex };
    if negative {
        format!("-{hex}")
    } else {
        hex.to_string()
    }
}

#[derive(Clone)]
pub struct Account {
    pub name: String,
    pub uuid: String,
    token: String,
    pub expires: i64,
    pub active: bool,
    pub from_client: bool,
}

impl Account {
    pub fn from_client(name: &str, uuid: &str, token: &str) -> Self {
        Account {
            name: name.to_string(),
            uuid: uuid.replace('-', ""),
            token: token.to_string(),
            expires: 0,
            active: false,
            from_client: true,
        }
    }
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("name", &self.name)
            .field("uuid", &self.uuid)
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

pub fn prism_accounts_path() -> Option<PathBuf> {
    let p = dirs::data_dir()?.join("PrismLauncher/accounts.json");
    p.exists().then_some(p)
}

// the token stays in memory and is only ever sent to sessionserver.mojang.com
pub fn load_prism_accounts() -> Result<Vec<Account>> {
    let path = prism_accounts_path().context("PrismLauncher/accounts.json not found")?;
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let mut out = Vec::new();
    for acc in json["accounts"].as_array().into_iter().flatten() {
        let (Some(name), Some(uuid), Some(token)) = (
            acc["profile"]["name"].as_str(),
            acc["profile"]["id"].as_str(),
            acc["ygg"]["token"].as_str(),
        ) else {
            continue;
        };
        out.push(Account {
            name: name.to_string(),
            uuid: uuid.replace('-', ""),
            token: token.to_string(),
            expires: acc["ygg"]["exp"].as_i64().unwrap_or(0),
            active: acc["active"].as_bool().unwrap_or(false),
            from_client: false,
        });
    }
    Ok(out)
}

pub fn session_join(account: &Account, server_hash: &str) -> Result<()> {
    let body = serde_json::json!({
        "accessToken": account.token,
        "selectedProfile": account.uuid,
        "serverId": server_hash,
    });
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let resp = agent
        .post("https://sessionserver.mojang.com/session/minecraft/join")
        .send_json(&body)?;
    let status = resp.status().as_u16();
    if status != 204 && status != 200 {
        let text = resp.into_body().read_to_string().unwrap_or_default();
        bail!(
            "sessionserver answered {status}: {text}. expired token? restart the game from Prism"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(name: &str) -> String {
        server_hash(name, &[], &[])
    }

    #[test]
    fn notchian_digest() {
        assert_eq!(digest("Notch"), "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48");
        assert_eq!(digest("jeb_"), "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1");
        assert_eq!(digest("simon"), "88e16a1019277b15d58faf0541e11910eb756f6");
    }

    #[test]
    fn cfb8_roundtrip() {
        let key = [3u8; 16];
        let original: Vec<u8> = (0..=255).collect();
        let mut data = original.clone();
        Cfb8::encryptor(&key).apply(&mut data);
        assert_ne!(data, original);
        let (a, b) = data.split_at_mut(100);
        let mut dec = Cfb8::decryptor(&key);
        dec.apply(a);
        dec.apply(b);
        assert_eq!(data, original);
    }

    #[test]
    fn cfb8_matches_openssl() {
        // openssl enc -aes-128-cfb8 -K 0303..03 -iv 0303..03
        let mut data = b"hello minecraft cfb8 check!!".to_vec();
        Cfb8::encryptor(&[3u8; 16]).apply(&mut data);
        let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "c1097849ad2f4c77632911d6d869d4c0b51cbaff56e35e96a5f4c30d");
    }
}
