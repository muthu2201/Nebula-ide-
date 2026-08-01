//! BYOK key storage.
//!
//! Keys live in the operating system's own secret store — Keychain on macOS,
//! Credential Manager on Windows, Secret Service on Linux — and nowhere else.
//! Not in a config file, not in an environment variable Nebula writes, and
//! never transmitted anywhere except to the provider the key belongs to.
//!
//! [`ApiKey`] zeroes its buffer on drop. That does not make key extraction
//! impossible — a debugger attached to the process can read anything — but it
//! does keep the key out of a core dump or a swapped-out page after use, which
//! is a real and cheap improvement.

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::{AiError, Result};

/// The service name Nebula registers under in the OS keychain.
const SERVICE: &str = "dev.nebula.ide";

/// A model provider whose key can be stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Provider {
    /// Anthropic.
    Anthropic,
    /// OpenAI.
    OpenAi,
    /// Google.
    Google,
}

impl Provider {
    /// Every provider.
    pub const ALL: &'static [Provider] = &[Provider::Anthropic, Provider::OpenAi, Provider::Google];

    /// The stable key used in the keychain and in settings.
    pub const fn id(&self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAi => "openai",
            Provider::Google => "google",
        }
    }

    /// The name shown in the UI.
    pub const fn display_name(&self) -> &'static str {
        match self {
            Provider::Anthropic => "Anthropic",
            Provider::OpenAi => "OpenAI",
            Provider::Google => "Google",
        }
    }

    /// The environment variable this provider's own tooling uses.
    ///
    /// Read as a fallback so a developer who already has it exported does not
    /// have to enter the key twice. Nebula never *writes* it.
    pub const fn env_var(&self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Google => "GOOGLE_API_KEY",
        }
    }

    /// Look a provider up by identifier.
    pub fn from_id(id: &str) -> Option<Provider> {
        Self::ALL.iter().copied().find(|p| p.id() == id)
    }

    /// Whether `key` has the shape this provider's keys have.
    ///
    /// A cheap sanity check at entry time, so a user who pastes the wrong thing
    /// finds out immediately rather than through a 401 later.
    pub fn looks_plausible(&self, key: &str) -> bool {
        let key = key.trim();
        match self {
            Provider::Anthropic => key.starts_with("sk-ant-") && key.len() > 20,
            Provider::OpenAi => key.starts_with("sk-") && key.len() > 20,
            Provider::Google => key.len() > 20 && !key.contains(char::is_whitespace),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// An API key, zeroed when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wrap a key.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into().trim().to_string())
    }

    /// The key, for sending to its provider.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the key is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A redacted form safe to log or display.
    pub fn redacted(&self) -> String {
        let key = &self.0;
        if key.len() <= 12 {
            return "•".repeat(key.len().max(4));
        }
        format!("{}…{}", &key[..8], &key[key.len() - 4..])
    }
}

impl Drop for ApiKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

// `Debug` and `Display` must never print the key. A key that reaches a log file
// or a bug report is a leaked credential, and the easiest way for that to happen
// is a derived Debug on a struct that happens to contain one.
impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ApiKey({})", self.redacted())
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted())
    }
}

/// Reads and writes API keys.
///
/// The backend is an enum rather than a trait object so that tests can use an
/// in-memory store without the production path becoming dynamically dispatched
/// or, worse, without a test-only branch existing inside the real store.
#[derive(Debug, Clone, Default)]
pub enum KeyStore {
    /// The OS keychain. What ships.
    #[default]
    OsKeychain,
    /// An in-memory store, for tests and for headless CI where no keychain
    /// daemon is running.
    Memory(std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<String, String>>>),
}

impl KeyStore {
    /// A store backed by the OS keychain.
    pub fn os() -> Self {
        KeyStore::OsKeychain
    }

    /// An empty in-memory store.
    pub fn memory() -> Self {
        KeyStore::Memory(std::sync::Arc::new(parking_lot::Mutex::new(
            std::collections::HashMap::new(),
        )))
    }

    /// Store a key.
    pub fn set(&self, provider: Provider, key: &ApiKey) -> Result<()> {
        if key.is_empty() {
            return Err(AiError::Keychain("refusing to store an empty key".to_string()));
        }
        match self {
            KeyStore::OsKeychain => {
                let entry = keyring::Entry::new(SERVICE, provider.id())
                    .map_err(|e| AiError::Keychain(e.to_string()))?;
                entry.set_password(key.expose()).map_err(|e| AiError::Keychain(e.to_string()))
            }
            KeyStore::Memory(map) => {
                map.lock().insert(provider.id().to_string(), key.expose().to_string());
                Ok(())
            }
        }
    }

    /// Fetch a key.
    ///
    /// Falls back to the provider's own environment variable when the keychain
    /// has nothing, so an existing developer setup works without re-entry.
    pub fn get(&self, provider: Provider) -> Result<ApiKey> {
        let stored = match self {
            KeyStore::OsKeychain => keyring::Entry::new(SERVICE, provider.id())
                .ok()
                .and_then(|entry| entry.get_password().ok()),
            KeyStore::Memory(map) => map.lock().get(provider.id()).cloned(),
        };
        let from_env = std::env::var(provider.env_var()).ok();
        resolve_key(provider, stored, from_env)
    }

    /// Whether a key is available for `provider`.
    pub fn has(&self, provider: Provider) -> bool {
        self.get(provider).is_ok()
    }

    /// Remove a stored key.
    pub fn delete(&self, provider: Provider) -> Result<()> {
        match self {
            KeyStore::OsKeychain => {
                let entry = keyring::Entry::new(SERVICE, provider.id())
                    .map_err(|e| AiError::Keychain(e.to_string()))?;
                match entry.delete_credential() {
                    Ok(()) => Ok(()),
                    // Deleting a key that is not there is a success, not a
                    // failure — the desired end state is reached either way.
                    Err(keyring::Error::NoEntry) => Ok(()),
                    Err(e) => Err(AiError::Keychain(e.to_string())),
                }
            }
            KeyStore::Memory(map) => {
                map.lock().remove(provider.id());
                Ok(())
            }
        }
    }

    /// Which providers currently have a usable key.
    pub fn configured_providers(&self) -> Vec<Provider> {
        Provider::ALL.iter().copied().filter(|p| self.has(*p)).collect()
    }
}

/// Decide which key to use, given what the store and the environment hold.
///
/// Split out as a pure function so the precedence rules can be tested without
/// mutating the process environment — which is a data race in a threaded test
/// runner, and is `unsafe` in this edition for exactly that reason.
fn resolve_key(
    provider: Provider,
    stored: Option<String>,
    from_env: Option<String>,
) -> Result<ApiKey> {
    // An explicitly stored key always wins over an ambient one.
    if let Some(key) = stored.filter(|k| !k.trim().is_empty()) {
        return Ok(ApiKey::new(key));
    }
    if let Some(key) = from_env.filter(|k| !k.trim().is_empty()) {
        tracing::debug!(provider = provider.id(), "using the API key from {}", provider.env_var());
        return Ok(ApiKey::new(key));
    }
    Err(AiError::NoApiKey(provider.id().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_round_trip_through_their_ids() {
        for provider in Provider::ALL {
            assert_eq!(Provider::from_id(provider.id()), Some(*provider));
        }
        assert_eq!(Provider::from_id("nope"), None);
    }

    #[test]
    fn keys_are_never_printed_in_full() {
        let key = ApiKey::new("sk-ant-api03-SECRETSECRETSECRETSECRET1234");

        let debug = format!("{key:?}");
        let display = format!("{key}");
        for rendering in [&debug, &display] {
            assert!(!rendering.contains("SECRETSECRET"), "a key reached a log line: {rendering}");
        }
        assert!(debug.contains("sk-ant-a"), "a prefix is fine and helps identify the key");
        assert!(debug.contains('…'));
    }

    #[test]
    fn a_short_key_is_fully_masked() {
        let key = ApiKey::new("short");
        assert!(!key.redacted().contains("short"));
        assert!(key.redacted().chars().all(|c| c == '•'));
    }

    #[test]
    fn the_key_itself_is_still_reachable_for_the_request() {
        let key = ApiKey::new("sk-ant-api03-value");
        assert_eq!(key.expose(), "sk-ant-api03-value");
    }

    #[test]
    fn surrounding_whitespace_is_stripped_on_entry() {
        // Pasting from a web page routinely brings a trailing newline, which
        // produces a 401 that looks like a wrong key.
        let key = ApiKey::new("  sk-ant-api03-value\n");
        assert_eq!(key.expose(), "sk-ant-api03-value");
    }

    #[test]
    fn plausibility_checks_catch_an_obviously_wrong_paste() {
        assert!(Provider::Anthropic.looks_plausible("sk-ant-api03-aaaaaaaaaaaaaaaaaaaa"));
        assert!(!Provider::Anthropic.looks_plausible("sk-proj-openai-key-here-aaaa"));
        assert!(!Provider::Anthropic.looks_plausible("hello"));
        assert!(!Provider::Anthropic.looks_plausible(""));
        assert!(Provider::OpenAi.looks_plausible("sk-proj-aaaaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn keys_round_trip_through_the_store() {
        let store = KeyStore::memory();
        assert!(!store.has(Provider::Anthropic));

        store.set(Provider::Anthropic, &ApiKey::new("sk-ant-api03-test-key-value")).unwrap();
        assert!(store.has(Provider::Anthropic));
        assert_eq!(store.get(Provider::Anthropic).unwrap().expose(), "sk-ant-api03-test-key-value");
    }

    #[test]
    fn storing_an_empty_key_is_refused() {
        let store = KeyStore::memory();
        assert!(store.set(Provider::Anthropic, &ApiKey::new("   ")).is_err());
    }

    #[test]
    fn deleting_removes_the_key_and_is_idempotent() {
        let store = KeyStore::memory();
        store.set(Provider::Anthropic, &ApiKey::new("sk-ant-api03-value-here")).unwrap();

        store.delete(Provider::Anthropic).unwrap();
        assert!(!store.has(Provider::Anthropic));
        store.delete(Provider::Anthropic).unwrap();
    }

    #[test]
    fn providers_are_isolated_from_each_other() {
        let store = KeyStore::memory();
        store.set(Provider::Anthropic, &ApiKey::new("sk-ant-api03-anthropic-key")).unwrap();
        store.set(Provider::OpenAi, &ApiKey::new("sk-proj-openai-key-value")).unwrap();

        assert_eq!(store.get(Provider::Anthropic).unwrap().expose(), "sk-ant-api03-anthropic-key");
        assert_eq!(store.get(Provider::OpenAi).unwrap().expose(), "sk-proj-openai-key-value");

        store.delete(Provider::Anthropic).unwrap();
        assert!(store.has(Provider::OpenAi), "deleting one key must not affect another");
    }

    #[test]
    fn a_missing_key_is_reported_by_provider_name() {
        match resolve_key(Provider::Google, None, None) {
            Err(AiError::NoApiKey(provider)) => assert_eq!(provider, "google"),
            other => panic!("expected NoApiKey, got {other:?}"),
        }
    }

    #[test]
    fn an_explicitly_stored_key_beats_the_environment() {
        let key = resolve_key(
            Provider::OpenAi,
            Some("sk-from-the-keychain".to_string()),
            Some("sk-from-the-environment".to_string()),
        )
        .unwrap();
        assert_eq!(key.expose(), "sk-from-the-keychain");
    }

    #[test]
    fn the_environment_is_used_when_nothing_is_stored() {
        // An already-exported key should not have to be entered a second time.
        let key = resolve_key(Provider::OpenAi, None, Some("sk-from-the-environment".to_string()))
            .unwrap();
        assert_eq!(key.expose(), "sk-from-the-environment");
    }

    #[test]
    fn blank_values_count_as_absent_in_both_sources() {
        assert!(resolve_key(Provider::Anthropic, Some("   ".into()), None).is_err());
        assert!(resolve_key(Provider::Anthropic, None, Some("\n".into())).is_err());
        // A blank stored value must fall through to the environment rather than
        // shadowing it.
        let key =
            resolve_key(Provider::Anthropic, Some("".into()), Some("sk-ant-real".into())).unwrap();
        assert_eq!(key.expose(), "sk-ant-real");
    }

    #[test]
    fn configured_providers_lists_only_the_ones_with_keys() {
        let store = KeyStore::memory();
        store.set(Provider::Anthropic, &ApiKey::new("sk-ant-api03-value-here")).unwrap();
        assert!(
            store.configured_providers().contains(&Provider::Anthropic),
            "a stored key must show as configured"
        );
    }
}
