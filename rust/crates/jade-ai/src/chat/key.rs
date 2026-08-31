//! Anthropic credential resolution.
//!
//! Order: `ANTHROPIC_API_KEY` → the macOS keychain → nothing.
//!
//! The key deliberately does NOT live in `~/.config/jade/ai.json`. That file is
//! plaintext, mode 644, rewritten by ordinary UI toggles, and lands in dotfile
//! repositories and backups — and [`crate::AiPrefs`]-style loaders swallow every
//! error, which is the wrong posture for a billing credential. The keychain is
//! reached through `security(1)` rather than the Security framework so this
//! costs no new crate, the same trade [`crate::backend`] makes when it shells
//! out to find `llama-server`.

use std::process::{Command, Stdio};

/// The keychain service and account the key is stored under.
const KEYCHAIN_SERVICE: &str = "jade-ai";
const KEYCHAIN_ACCOUNT: &str = "anthropic";

/// An API key that will not print itself.
///
/// `Debug` is implemented by hand because these values reach logs, panic
/// messages, and `#[derive(Debug)]` on every struct that holds one.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(raw: impl Into<String>) -> Self {
        ApiKey(raw.into())
    }

    /// The raw secret. Only the HTTP header builder should call this.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// A safe display form: the first 8 characters, then a fixed mask. The
    /// prefix is enough to tell two keys apart without revealing either.
    pub fn redacted(&self) -> String {
        let head: String = self.0.chars().take(8).collect();
        format!("{head}…")
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiKey({})", self.redacted())
    }
}

/// Where a resolved key came from — shown in the AI menu so the user can tell
/// which one is in play when both exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Env,
    Keychain,
    None,
}

impl KeySource {
    pub fn label(self) -> &'static str {
        match self {
            KeySource::Env => "ANTHROPIC_API_KEY",
            KeySource::Keychain => "keychain",
            KeySource::None => "not set",
        }
    }
}

/// Resolve the key, environment first.
///
/// Env wins so a shell, a test, or a CI job can override the stored key
/// without touching the keychain — the same convention every Anthropic SDK
/// follows.
pub fn resolve() -> (Option<ApiKey>, KeySource) {
    resolve_with(
        || std::env::var("ANTHROPIC_API_KEY").ok(),
        keychain_read,
    )
}

/// The resolution order, with both lookups injected so it is testable without
/// touching the real environment or the real keychain.
pub fn resolve_with(
    env: impl Fn() -> Option<String>,
    keychain: impl Fn() -> Option<String>,
) -> (Option<ApiKey>, KeySource) {
    if let Some(k) = env().filter(|k| !k.trim().is_empty()) {
        return (Some(ApiKey::new(k.trim())), KeySource::Env);
    }
    if let Some(k) = keychain().filter(|k| !k.trim().is_empty()) {
        return (Some(ApiKey::new(k.trim())), KeySource::Keychain);
    }
    (None, KeySource::None)
}

/// `security find-generic-password -s jade-ai -a anthropic -w`.
///
/// A missing item exits non-zero, which is the common case on a fresh machine
/// and must not be noisy. Any failure is `None`.
fn keychain_read() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Store (or replace) the key in the keychain. `-U` updates an existing item
/// instead of failing with a duplicate error.
///
/// The secret is passed as an argument, which is visible in `ps` for the
/// lifetime of the child. `security(1)` offers no stdin path for `-w`, so the
/// alternative is linking the Security framework; the exposure is a few
/// milliseconds on the user's own machine, at their explicit request.
pub fn store(key: &str) -> Result<(), String> {
    let out = Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
            key,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run security(1): {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Remove the stored key. A missing item is success — the end state is what
/// the caller asked for either way.
pub fn clear() -> Result<(), String> {
    let out = Command::new("/usr/bin/security")
        .args([
            "delete-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run security(1): {e}"))?;
    let _ = out;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "sk-ant-api03-supersecretvalue";

    #[test]
    fn env_wins_over_keychain() {
        let (k, src) = resolve_with(|| Some(SECRET.into()), || Some("from-keychain".into()));
        assert_eq!(src, KeySource::Env);
        assert_eq!(k.unwrap().expose(), SECRET);
    }

    #[test]
    fn keychain_is_the_fallback() {
        let (k, src) = resolve_with(|| None, || Some(SECRET.into()));
        assert_eq!(src, KeySource::Keychain);
        assert_eq!(k.unwrap().expose(), SECRET);
    }

    #[test]
    fn nothing_set() {
        let (k, src) = resolve_with(|| None, || None);
        assert_eq!(src, KeySource::None);
        assert!(k.is_none());
    }

    /// An exported-but-empty variable is the shape `export ANTHROPIC_API_KEY=`
    /// leaves behind. It must fall through, not resolve to an empty key.
    #[test]
    fn blank_env_falls_through() {
        let (k, src) = resolve_with(|| Some("   ".into()), || Some(SECRET.into()));
        assert_eq!(src, KeySource::Keychain);
        assert_eq!(k.unwrap().expose(), SECRET);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        // `security -w` emits a trailing newline.
        let (k, _) = resolve_with(|| None, || Some(format!("{SECRET}\n")));
        assert_eq!(k.unwrap().expose(), SECRET);
    }

    /// The whole point of the newtype.
    #[test]
    fn debug_never_prints_the_secret() {
        let k = ApiKey::new(SECRET);
        let shown = format!("{k:?}");
        assert!(!shown.contains(SECRET), "{shown}");
        assert!(!shown.contains("supersecret"), "{shown}");
        assert!(shown.starts_with("ApiKey(sk-ant-a"));
    }

    /// A struct that derives Debug and holds a key must stay safe too.
    #[test]
    fn debug_is_safe_when_nested() {
        #[derive(Debug)]
        struct Holder {
            #[allow(dead_code)]
            key: ApiKey,
        }
        let shown = format!("{:?}", Holder { key: ApiKey::new(SECRET) });
        assert!(!shown.contains(SECRET), "{shown}");
    }

    #[test]
    fn redacted_distinguishes_keys_without_revealing_them() {
        let a = ApiKey::new("sk-ant-aaaaaaaaaaaaaaaa");
        let b = ApiKey::new("sk-ant-bbbbbbbbbbbbbbbb");
        assert_ne!(a.redacted(), b.redacted());
        assert!(!a.redacted().contains("aaaaaaaaa"));
    }
}
