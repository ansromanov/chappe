//! Access tokens of agents: one per machine and tool.
//!
//! Tokens come from a directory (for example a Kubernetes Secret): one
//! file per token, named `<machine>.<tool>`, holding the token. The service
//! keeps only SHA-256 hashes and never prints a token.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::store::TokenEntry;

const MIN_TOKEN_LEN: usize = 32;
const MAX_TOKEN_LEN: usize = 256;
const MAX_NAME_PART_LEN: usize = 32;

pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The token from an `Authorization: Bearer <token>` header value.
pub fn bearer(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

#[derive(Debug)]
pub enum TokenDirError {
    Io(io::Error),
    BadName(String),
    BadToken(String),
    Duplicate(String, String),
}

impl fmt::Display for TokenDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenDirError::Io(e) => write!(f, "cannot read the token directory: {e}"),
            TokenDirError::BadName(n) => {
                write!(
                    f,
                    "token file {n:?}: the name must be <machine>.<tool>, lowercase letters, digits and '-'"
                )
            }
            TokenDirError::BadToken(n) => {
                write!(
                    f,
                    "token file {n:?}: the token must be {MIN_TOKEN_LEN}-{MAX_TOKEN_LEN} visible ASCII characters"
                )
            }
            TokenDirError::Duplicate(a, b) => {
                write!(f, "token files {a:?} and {b:?} hold the same token")
            }
        }
    }
}

impl std::error::Error for TokenDirError {}

impl From<io::Error> for TokenDirError {
    fn from(e: io::Error) -> Self {
        TokenDirError::Io(e)
    }
}

fn valid_part(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME_PART_LEN
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Splits `<machine>.<tool>`.
pub fn parse_name(name: &str) -> Option<(&str, &str)> {
    let (machine, tool) = name.split_once('.')?;
    (valid_part(machine) && valid_part(tool)).then_some((machine, tool))
}

fn valid_token(token: &str) -> bool {
    (MIN_TOKEN_LEN..=MAX_TOKEN_LEN).contains(&token.len())
        && token.bytes().all(|b| b.is_ascii_graphic())
}

/// Reads every token file in `dir`. Names starting with a dot are skipped:
/// a mounted Secret keeps its bookkeeping (`..data`) there.
pub fn load_dir(dir: &Path) -> Result<Vec<TokenEntry>, TokenDirError> {
    let mut entries = Vec::new();
    let mut seen: HashMap<[u8; 32], String> = HashMap::new();
    let mut names: Vec<String> = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<_>>()?;
    names.sort();
    for name in names.into_iter().filter(|n| !n.starts_with('.')) {
        let path = dir.join(&name);
        // Secret mounts are symlinks to files; `metadata` follows them.
        if !fs::metadata(&path)?.is_file() {
            continue;
        }
        let (machine, tool) =
            parse_name(&name).ok_or_else(|| TokenDirError::BadName(name.clone()))?;
        let content =
            fs::read_to_string(&path).map_err(|_| TokenDirError::BadToken(name.clone()))?;
        let token = content.trim();
        if !valid_token(token) {
            return Err(TokenDirError::BadToken(name));
        }
        let hash = hash_token(token);
        if let Some(other) = seen.insert(hash, name.clone()) {
            return Err(TokenDirError::Duplicate(other, name));
        }
        entries.push(TokenEntry {
            machine: machine.into(),
            tool: tool.into(),
            name,
            hash,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN_A: &str = "a-token-that-is-long-enough-0123456789";
    const TOKEN_B: &str = "b-token-that-is-long-enough-0123456789";

    fn write(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn hash_is_sha256() {
        let hex: String = hash_token("abc")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn bearer_header_parsing() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
        assert_eq!(bearer("Bearer "), None);
        assert_eq!(bearer("abc"), None);
    }

    #[test]
    fn name_parsing() {
        assert_eq!(parse_name("host-a.claude"), Some(("host-a", "claude")));
        assert_eq!(parse_name("mac.codex"), Some(("mac", "codex")));
        for bad in [
            "",
            "nodot",
            ".claude",
            "mac.",
            "Mac.claude",
            "a.b.c",
            "a b.c",
            "-a.b",
            "a/../b.c",
        ] {
            assert_eq!(parse_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn loads_tokens_and_trims_the_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "host-a.claude", &format!("{TOKEN_A}\n"));
        write(dir.path(), "mac.codex", TOKEN_B);
        let entries = load_dir(dir.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "host-a.claude");
        assert_eq!(entries[0].hash, hash_token(TOKEN_A));
        assert_eq!(entries[1].machine, "mac");
    }

    #[test]
    fn skips_secret_mount_bookkeeping_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "mac.codex", TOKEN_A);
        fs::create_dir(dir.path().join("..data")).unwrap();
        write(dir.path(), "..2026_11_12", "not a token");
        fs::create_dir(dir.path().join("subdir")).unwrap();
        assert_eq!(load_dir(dir.path()).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinks_like_a_secret_mount() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("..data")).unwrap();
        write(&dir.path().join("..data"), "mac.codex", TOKEN_A);
        std::os::unix::fs::symlink("..data/mac.codex", dir.path().join("mac.codex")).unwrap();
        assert_eq!(load_dir(dir.path()).unwrap()[0].hash, hash_token(TOKEN_A));
    }

    #[test]
    fn rejects_weak_tokens_and_bad_names_without_echoing_the_token() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "mac.codex", "short");
        let err = load_dir(dir.path()).unwrap_err();
        assert!(matches!(err, TokenDirError::BadToken(_)));
        assert!(!err.to_string().contains("short"));

        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Mac Codex", TOKEN_A);
        assert!(matches!(
            load_dir(dir.path()),
            Err(TokenDirError::BadName(_))
        ));

        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "mac.codex", &format!("{TOKEN_A} {TOKEN_B}"));
        assert!(matches!(
            load_dir(dir.path()),
            Err(TokenDirError::BadToken(_))
        ));
    }

    #[test]
    fn rejects_one_token_shared_by_two_names() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.claude", TOKEN_A);
        write(dir.path(), "b.claude", TOKEN_A);
        assert!(matches!(
            load_dir(dir.path()),
            Err(TokenDirError::Duplicate(..))
        ));
    }

    #[test]
    fn missing_directory_is_an_error() {
        assert!(matches!(
            load_dir(Path::new("/nonexistent/tokens")),
            Err(TokenDirError::Io(_))
        ));
    }
}
