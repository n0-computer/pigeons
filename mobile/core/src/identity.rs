//! One key, two protocols.
//!
//! The app's iroh endpoint ID and its SSH identity are the same ed25519 key.
//! That key is generated and persisted by `iroh_pigeons` in the app's data
//! directory, exactly like a roost's key, and here it is re-expressed in
//! OpenSSH form so `sshd` on the roost can authenticate it. The user adds the
//! single `authorized_keys` line this module prints and the phone is known.

use anyhow::Context;
use iroh::SecretKey;
use russh::keys::{PrivateKey, ssh_key::private::Ed25519Keypair};

/// Comment on the published public key, so it can be recognised in
/// `authorized_keys`.
pub const KEY_COMMENT: &str = "pigeons-mobile";

/// The SSH private key for an iroh secret key.
pub fn ssh_private_key(secret: &SecretKey) -> PrivateKey {
    let keypair = Ed25519Keypair::from_seed(&secret.to_bytes());
    let mut key = PrivateKey::from(keypair);
    key.set_comment(KEY_COMMENT);
    key
}

/// The `authorized_keys` line for `key`: `ssh-ed25519 <base64> pigeons-mobile`.
pub fn authorized_key_line(key: &PrivateKey) -> anyhow::Result<String> {
    key.public_key().to_openssh().context("encoding public key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_key_shares_the_iroh_public_key() {
        let secret = SecretKey::generate();
        let key = ssh_private_key(&secret);
        let ed = key.public_key().key_data().ed25519().expect("ed25519 key");
        assert_eq!(ed.as_ref(), secret.public().as_bytes());
    }

    #[test]
    fn authorized_line_is_openssh_shaped() {
        let secret = SecretKey::generate();
        let line = authorized_key_line(&ssh_private_key(&secret)).unwrap();
        let parts: Vec<_> = line.split(' ').collect();
        assert_eq!(parts.len(), 3, "{line}");
        assert_eq!(parts[0], "ssh-ed25519");
        assert_eq!(parts[2], KEY_COMMENT);
    }
}
