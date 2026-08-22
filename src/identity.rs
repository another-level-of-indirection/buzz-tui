//! Where the secret key comes from.
//!
//! Two sources, in this order:
//!
//! 1. The OS keychain, under service `buzz-tui`. Opt-in via `--keychain-import`.
//! 2. `BUZZ_PRIVATE_KEY`, matching `buzz-cli`.
//!
//! The keychain wins because it is explicit: an entry only exists if someone
//! ran the import. The env var stays supported because it is what `buzz-cli`
//! uses and what CI and harnessed agents can actually set — but it is second
//! for a reason. An exported secret is visible in `ps` output on most systems
//! and is inherited by every child process the shell spawns.

use std::io::{IsTerminal, Read};

use anyhow::{bail, Context, Result};
use nostr::Keys;
use zeroize::Zeroize;

/// Keychain service name. Stable — changing it orphans everyone's stored key.
const SERVICE: &str = "buzz-tui";
/// Default keychain account, overridable so several identities can coexist.
const DEFAULT_ACCOUNT: &str = "default";

/// Which source a key was loaded from, for the `--probe` report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Keychain,
    Environment,
}

impl KeySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keychain => "os keychain",
            Self::Environment => "BUZZ_PRIVATE_KEY",
        }
    }
}

pub fn account() -> String {
    std::env::var("BUZZ_KEYCHAIN_ACCOUNT").unwrap_or_else(|_| DEFAULT_ACCOUNT.to_string())
}

/// Resolves the identity, preferring the keychain.
pub fn load() -> Result<(Keys, KeySource)> {
    if let Some(mut secret) = from_keychain()? {
        let keys = parse(&secret);
        secret.zeroize();
        return Ok((keys?, KeySource::Keychain));
    }
    let Ok(mut secret) = std::env::var("BUZZ_PRIVATE_KEY") else {
        // Naming both the durable fix and the per-shell reason it is needed:
        // an exported key is gone the moment you open a new tab, and nothing
        // about that looks like a mistake.
        bail!(
            "No key found.\n\n\
             BUZZ_PRIVATE_KEY is not set in this shell — an exported key does not survive a new \
             tab. To store it once and stop thinking about it:\n\n    \
             printf '%s' \"$BUZZ_PRIVATE_KEY\" | buzz-tui --keychain-import\n\n\
             run from a shell that still has it. Or export BUZZ_PRIVATE_KEY again for this one."
        );
    };
    let keys = parse(&secret);
    secret.zeroize();
    Ok((keys?, KeySource::Environment))
}

fn parse(secret: &str) -> Result<Keys> {
    Keys::parse(secret.trim()).context("key is not valid hex or nsec")
}

fn from_keychain() -> Result<Option<String>> {
    let entry = keyring::Entry::new(SERVICE, &account()).context("opening the keychain")?;
    match entry.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        // A locked or unavailable keychain must not silently fall through to
        // the env var: that turns "you denied the prompt" into "signed in as
        // somebody else", which is worse than an error.
        Err(error) => Err(error).context("reading the keychain"),
    }
}

/// Stores a key in the OS keychain, from a hidden prompt or from stdin.
///
/// Reading stdin when it is not a terminal is what makes this usable without
/// the key ever appearing on screen or in shell history:
///
/// ```text
/// printf '%s' "$BUZZ_PRIVATE_KEY" | buzz-tui --keychain-import
/// ```
pub fn import() -> Result<()> {
    let account = account();
    let mut secret = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!(
            "Nostr secret key (hex or nsec) for keychain account '{account}': "
        ))
        .context("reading the key")?
    } else {
        let mut piped = String::new();
        std::io::stdin()
            .read_to_string(&mut piped)
            .context("reading the key from stdin")?;
        piped
    };
    let parsed = parse(&secret);
    let outcome = match parsed {
        Ok(keys) => {
            let entry = keyring::Entry::new(SERVICE, &account).context("opening the keychain")?;
            entry
                .set_password(secret.trim())
                .context("writing to the keychain")?;
            println!(
                "stored {} in the {SERVICE} keychain",
                keys.public_key().to_hex()
            );
            println!("account: {account}");
            Ok(())
        }
        Err(error) => Err(error),
    };
    // Wipe on both paths — a rejected key is still a key.
    secret.zeroize();
    outcome
}

/// Service and blob key Buzz Desktop stores its identity under.
///
/// Desktop keeps every secret as one JSON map in a single keychain entry, so
/// exactly one prompt is needed no matter how many keys it holds.
const DESKTOP_SERVICE: &str = "buzz-desktop";
const DESKTOP_BLOB_ACCOUNT: &str = "secrets";
const DESKTOP_IDENTITY_KEY: &str = "identity";

/// Copies the key Buzz Desktop already has into this client's keychain entry.
///
/// A one-time migration, not a runtime dependency: reading Desktop's store on
/// every launch would work against how that store is designed, and would break
/// the moment this client ran anywhere Desktop is not installed. Copying once
/// is different — after this, the two are independent.
pub fn import_from_desktop() -> Result<()> {
    let entry = keyring::Entry::new(DESKTOP_SERVICE, DESKTOP_BLOB_ACCOUNT)
        .context("opening the Buzz Desktop keychain entry")?;
    let blob = match entry.get_password() {
        Ok(blob) => blob,
        Err(keyring::Error::NoEntry) => bail!(
            "Buzz Desktop has no key stored in this keychain (service `{DESKTOP_SERVICE}`).              Is it signed in on this machine?"
        ),
        Err(error) => return Err(error).context("reading the Buzz Desktop keychain entry"),
    };

    let secrets: std::collections::HashMap<String, String> = serde_json::from_str(&blob)
        .context("Buzz Desktop's keychain entry is not the map this expects")?;
    let mut secret = secrets
        .get(DESKTOP_IDENTITY_KEY)
        .cloned()
        .with_context(|| format!("no `{DESKTOP_IDENTITY_KEY}` key in Buzz Desktop's store"))?;

    let outcome = match parse(&secret) {
        Ok(keys) => {
            let account = account();
            let entry = keyring::Entry::new(SERVICE, &account).context("opening the keychain")?;
            entry
                .set_password(secret.trim())
                .context("writing to the keychain")?;
            println!(
                "copied {} from Buzz Desktop into the {SERVICE} keychain",
                keys.public_key().to_hex()
            );
            println!("account: {account}");
            Ok(())
        }
        Err(error) => Err(error),
    };
    secret.zeroize();
    outcome
}

/// Removes the stored key.
pub fn delete() -> Result<()> {
    let account = account();
    let entry = keyring::Entry::new(SERVICE, &account).context("opening the keychain")?;
    match entry.delete_credential() {
        Ok(()) => {
            println!("removed the {SERVICE} keychain entry for account '{account}'");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            println!("no {SERVICE} keychain entry for account '{account}'");
            Ok(())
        }
        Err(error) => Err(error).context("deleting the keychain entry"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_name_is_stable() {
        // Changing this orphans every stored key, so it is pinned rather than
        // left to drift with a rename.
        assert_eq!(SERVICE, "buzz-tui");
    }

    #[test]
    fn a_key_source_names_itself_for_the_probe_report() {
        assert_eq!(KeySource::Keychain.as_str(), "os keychain");
        assert_eq!(KeySource::Environment.as_str(), "BUZZ_PRIVATE_KEY");
    }
}
