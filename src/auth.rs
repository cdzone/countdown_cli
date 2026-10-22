//! Shared bearer-token table: parse the TOML table and look up a presented
//! token by its SHA-256 digest. Pure logic, no HTTP types, so both the API and
//! the MCP binary can wrap it in their own middleware.
//!
//! Table format (one `[[token]]` per caller):
//!
//! ```toml
//! [[token]]
//! id = "mcp-svc"
//! sha256 = "<64 hex chars, from: echo -n \"$TOKEN\" | sha256sum>"
//! scopes = ["countdowns:read"]
//! ```
//!
//! Errors never echo file contents (digests or scope strings); they name the
//! entry by `id`, or by position when the id itself is invalid.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::{fmt, io, path::Path};

use serde_derive::Deserialize;
use sha2::{Digest, Sha256};

/// Upper bound for a token `id`; it ends up in audit lines and request headers.
const MAX_ID_LEN: usize = 64;

/// What a token may do. `CountdownsWrite` implies `CountdownsRead`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    CountdownsRead,
    CountdownsWrite,
}

impl Scope {
    /// Wire name, as written in the token table and in `WWW-Authenticate`.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::CountdownsRead => "countdowns:read",
            Scope::CountdownsWrite => "countdowns:write",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "countdowns:read" => Some(Scope::CountdownsRead),
            "countdowns:write" => Some(Scope::CountdownsWrite),
            _ => None,
        }
    }
}

/// The caller a valid token resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    id: String,
    scopes: BTreeSet<Scope>,
}

impl Principal {
    /// Audit name of the caller; not a secret.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether this caller may act with `required`, honouring write ⊇ read.
    pub fn allows(&self, required: Scope) -> bool {
        self.scopes.contains(&required)
            || (required == Scope::CountdownsRead && self.scopes.contains(&Scope::CountdownsWrite))
    }
}

/// SHA-256 of a token. `Debug` is redacted so a digest never reaches logs.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct TokenDigest([u8; 32]);

impl TokenDigest {
    fn of(token: &str) -> Self {
        TokenDigest(Sha256::digest(token.as_bytes()).into())
    }

    fn from_hex(s: &str) -> Option<Self> {
        // `from_str_radix` alone would accept a leading '+', so check digits first.
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut out = [0u8; 32];
        for (byte, &[hi, lo]) in out.iter_mut().zip(s.as_bytes().as_chunks::<2>().0) {
            *byte = u8::from_str_radix(std::str::from_utf8(&[hi, lo]).ok()?, 16).ok()?;
        }
        Some(TokenDigest(out))
    }
}

impl fmt::Debug for TokenDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenDigest(<redacted>)")
    }
}

/// A validated, non-empty token table, keyed by token digest.
#[derive(Debug, Clone)]
pub struct TokenTable {
    by_digest: HashMap<TokenDigest, Principal>,
}

impl TokenTable {
    /// Read and parse the table at `path`.
    pub fn load(path: &Path) -> Result<Self, TokenTableError> {
        let text = std::fs::read_to_string(path).map_err(TokenTableError::Io)?;
        Self::parse(&text)
    }

    /// Parse a table from TOML text; rejects anything ambiguous or empty.
    pub fn parse(text: &str) -> Result<Self, TokenTableError> {
        let raw: RawTable = toml::from_str(text).map_err(|err| TokenTableError::Syntax {
            line: err
                .span()
                .map(|span| text[..span.start].matches('\n').count() + 1),
        })?;
        if raw.token.is_empty() {
            return Err(TokenTableError::Empty);
        }

        let empty_token = TokenDigest::of("");
        let mut ids = HashSet::new();
        let mut by_digest: HashMap<TokenDigest, Principal> = HashMap::new();
        for (index, entry) in raw.token.into_iter().enumerate() {
            if !is_valid_id(&entry.id) {
                return Err(TokenTableError::InvalidId { entry: index + 1 });
            }
            if !ids.insert(entry.id.clone()) {
                return Err(TokenTableError::DuplicateId { id: entry.id });
            }
            let Some(digest) = TokenDigest::from_hex(&entry.sha256) else {
                return Err(TokenTableError::InvalidDigest { id: entry.id });
            };
            if digest == empty_token {
                return Err(TokenTableError::EmptyTokenDigest { id: entry.id });
            }
            let mut scopes = BTreeSet::new();
            for scope in &entry.scopes {
                match Scope::parse(scope) {
                    Some(scope) => scopes.insert(scope),
                    None => return Err(TokenTableError::UnknownScope { id: entry.id }),
                };
            }
            let principal = Principal {
                id: entry.id,
                scopes,
            };
            if let Some(first) = by_digest.get(&digest) {
                return Err(TokenTableError::DuplicateDigest {
                    first: first.id.clone(),
                    second: principal.id,
                });
            }
            by_digest.insert(digest, principal);
        }
        Ok(TokenTable { by_digest })
    }

    /// The caller that owns `token`, if any. Lookup is by digest, so no
    /// constant-time comparison of secrets is involved.
    pub fn lookup(&self, token: &str) -> Option<&Principal> {
        self.by_digest.get(&TokenDigest::of(token))
    }
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    #[serde(default)]
    token: Vec<RawEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    id: String,
    sha256: String,
    scopes: Vec<String>,
}

/// Why a token table was rejected. Messages carry no file contents.
#[derive(Debug)]
pub enum TokenTableError {
    Io(io::Error),
    /// Not valid TOML, or not the `[[token]]` schema.
    Syntax {
        line: Option<usize>,
    },
    Empty,
    /// 1-based position of the `[[token]]` entry with a bad id.
    InvalidId {
        entry: usize,
    },
    DuplicateId {
        id: String,
    },
    InvalidDigest {
        id: String,
    },
    /// The digest is SHA-256 of the empty string, i.e. an unset `$TOKEN`.
    EmptyTokenDigest {
        id: String,
    },
    DuplicateDigest {
        first: String,
        second: String,
    },
    UnknownScope {
        id: String,
    },
}

impl fmt::Display for TokenTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenTableError::Io(err) => write!(f, "cannot read token table: {err}"),
            TokenTableError::Syntax { line: Some(line) } => {
                write!(f, "invalid token table at line {line}")
            }
            TokenTableError::Syntax { line: None } => f.write_str("invalid token table"),
            TokenTableError::Empty => f.write_str("token table has no [[token]] entries"),
            TokenTableError::InvalidId { entry } => write!(
                f,
                "token entry #{entry}: id must be 1-{MAX_ID_LEN} chars of [A-Za-z0-9._-]"
            ),
            TokenTableError::DuplicateId { id } => write!(f, "duplicate token id '{id}'"),
            TokenTableError::InvalidDigest { id } => {
                write!(f, "token '{id}': sha256 must be 64 hex chars")
            }
            TokenTableError::EmptyTokenDigest { id } => {
                write!(f, "token '{id}': sha256 is the digest of an empty token")
            }
            TokenTableError::DuplicateDigest { first, second } => {
                write!(f, "tokens '{first}' and '{second}' have the same sha256")
            }
            TokenTableError::UnknownScope { id } => write!(
                f,
                "token '{id}': unknown scope (expected countdowns:read or countdowns:write)"
            ),
        }
    }
}

impl std::error::Error for TokenTableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TokenTableError::Io(err) => Some(err),
            _ => None,
        }
    }
}

/// `WWW-Authenticate` value for a 401, shared by the API and MCP so both
/// challenge the same way. `invalid_token` is set when a bearer token was
/// presented (empty or unknown); `scope` hints what a caller should ask for.
pub fn bearer_challenge(invalid_token: bool, scope: Option<Scope>) -> String {
    let mut value = String::from("Bearer realm=\"countdown\"");
    if invalid_token {
        value.push_str(", error=\"invalid_token\"");
    }
    if let Some(scope) = scope {
        value.push_str(&format!(", scope=\"{}\"", scope.as_str()));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ_TOKEN: &str = "read-token-0123456789abcdef0123456789abcdef";
    const WRITE_TOKEN: &str = "write-token-0123456789abcdef0123456789abcdef";

    fn hex(token: &str) -> String {
        TokenDigest::of(token)
            .0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn entry(id: &str, sha256: &str, scopes: &str) -> String {
        format!("[[token]]\nid = \"{id}\"\nsha256 = \"{sha256}\"\nscopes = {scopes}\n")
    }

    fn two_token_table() -> String {
        entry("ops-read", &hex(READ_TOKEN), r#"["countdowns:read"]"#)
            + &entry("admin", &hex(WRITE_TOKEN), r#"["countdowns:write"]"#)
    }

    #[test]
    fn digest_matches_sha256sum_output() {
        // echo -n "abc" | sha256sum
        assert_eq!(
            hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn lookup_resolves_each_token_to_its_principal() {
        let table = TokenTable::parse(&two_token_table()).unwrap();

        let reader = table.lookup(READ_TOKEN).unwrap();
        assert_eq!(reader.id(), "ops-read");
        assert!(reader.allows(Scope::CountdownsRead));
        assert!(!reader.allows(Scope::CountdownsWrite));

        let admin = table.lookup(WRITE_TOKEN).unwrap();
        assert_eq!(admin.id(), "admin");
        assert!(admin.allows(Scope::CountdownsWrite));
        assert!(admin.allows(Scope::CountdownsRead), "write implies read");
    }

    #[test]
    fn lookup_rejects_unknown_empty_and_near_miss_tokens() {
        let table = TokenTable::parse(&two_token_table()).unwrap();
        assert!(table.lookup("not-a-token").is_none());
        assert!(table.lookup("").is_none());
        assert!(table.lookup(&format!("{READ_TOKEN}\n")).is_none());
        assert!(table.lookup(&READ_TOKEN.to_uppercase()).is_none());
        // The digest itself is not a token.
        assert!(table.lookup(&hex(READ_TOKEN)).is_none());
    }

    #[test]
    fn uppercase_hex_digest_is_accepted() {
        let text = entry(
            "ops",
            &hex(READ_TOKEN).to_uppercase(),
            r#"["countdowns:read"]"#,
        );
        let table = TokenTable::parse(&text).unwrap();
        assert_eq!(table.lookup(READ_TOKEN).unwrap().id(), "ops");
    }

    #[test]
    fn empty_scope_list_authenticates_but_allows_nothing() {
        let table = TokenTable::parse(&entry("mcp-in", &hex(READ_TOKEN), "[]")).unwrap();
        let p = table.lookup(READ_TOKEN).unwrap();
        assert!(!p.allows(Scope::CountdownsRead));
        assert!(!p.allows(Scope::CountdownsWrite));
    }

    #[test]
    fn scope_wire_names() {
        assert_eq!(Scope::CountdownsRead.as_str(), "countdowns:read");
        assert_eq!(Scope::CountdownsWrite.as_str(), "countdowns:write");
    }

    #[test]
    fn rejects_empty_tables() {
        for text in ["", "# nothing here\n", "token = []\n"] {
            assert!(
                matches!(TokenTable::parse(text), Err(TokenTableError::Empty)),
                "{text:?}"
            );
        }
    }

    #[test]
    fn rejects_bad_toml_and_wrong_schema() {
        let good = hex(READ_TOKEN);
        let cases = [
            "[[token]\n".to_owned(),
            // typo in the table name must not silently yield an empty table
            entry("a", &good, "[]").replace("[[token]]", "[[tokens]]"),
            // missing scopes
            format!("[[token]]\nid = \"a\"\nsha256 = \"{good}\"\n"),
            // unknown field
            entry("a", &good, "[]") + "token = \"plaintext\"\n",
            // scopes must be a list
            entry("a", &good, r#""countdowns:read""#),
        ];
        for text in cases {
            assert!(
                matches!(
                    TokenTable::parse(&text),
                    Err(TokenTableError::Syntax { .. })
                ),
                "{text:?}"
            );
        }
    }

    #[test]
    fn syntax_error_reports_line_not_content() {
        let text = format!("{}\nid = oops-{READ_TOKEN}\n", two_token_table());
        let err = TokenTable::parse(&text).unwrap_err();
        assert!(
            matches!(err, TokenTableError::Syntax { line: Some(_) }),
            "{err:?}"
        );
        assert!(!err.to_string().contains(READ_TOKEN));
    }

    #[test]
    fn rejects_duplicate_ids() {
        let text = entry("dup", &hex(READ_TOKEN), "[]") + &entry("dup", &hex(WRITE_TOKEN), "[]");
        assert!(matches!(
            TokenTable::parse(&text),
            Err(TokenTableError::DuplicateId { id }) if id == "dup"
        ));
    }

    #[test]
    fn rejects_duplicate_digests_even_with_different_case() {
        let text = entry("a", &hex(READ_TOKEN), "[]")
            + &entry(
                "b",
                &hex(READ_TOKEN).to_uppercase(),
                r#"["countdowns:write"]"#,
            );
        assert!(matches!(
            TokenTable::parse(&text),
            Err(TokenTableError::DuplicateDigest { first, second }) if first == "a" && second == "b"
        ));
    }

    #[test]
    fn rejects_invalid_ids() {
        let long = "x".repeat(MAX_ID_LEN + 1);
        for id in ["", "has space", "new\\nline", "名字", long.as_str()] {
            let text = two_token_table() + &entry(id, &hex("third-token"), "[]");
            assert!(
                matches!(
                    TokenTable::parse(&text),
                    Err(TokenTableError::InvalidId { entry: 3 })
                ),
                "{id:?}"
            );
        }
        let max = "x".repeat(MAX_ID_LEN);
        assert!(TokenTable::parse(&entry(&max, &hex(READ_TOKEN), "[]")).is_ok());
    }

    #[test]
    fn rejects_malformed_digests_without_echoing_them() {
        let good = hex(READ_TOKEN);
        let cases = [
            READ_TOKEN.to_owned(), // plaintext pasted by mistake
            good[..63].to_owned(), // too short
            format!("{good}0"),    // too long
            format!("{}g", &good[..63]),
            format!("+{}", &good[..63]),
            format!("{} -", &good[..62]),
            "é".repeat(32),
        ];
        for bad in cases {
            let err = TokenTable::parse(&entry("a", &bad, "[]")).unwrap_err();
            assert!(
                matches!(&err, TokenTableError::InvalidDigest { id } if id == "a"),
                "{bad:?}: {err:?}"
            );
            assert!(!err.to_string().contains(&bad));
        }
    }

    #[test]
    fn rejects_digest_of_empty_token() {
        // echo -n "" | sha256sum, what an unset $TOKEN produces
        let text = entry(
            "unset",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "[]",
        );
        assert!(matches!(
            TokenTable::parse(&text),
            Err(TokenTableError::EmptyTokenDigest { id }) if id == "unset"
        ));
    }

    #[test]
    fn rejects_unknown_scopes() {
        for scopes in [
            r#"["countdowns:admin"]"#,
            r#"["COUNTDOWNS:READ"]"#,
            r#"["countdowns:read", "mcp:write"]"#,
            r#"[""]"#,
        ] {
            assert!(
                matches!(
                    TokenTable::parse(&entry("a", &hex(READ_TOKEN), scopes)),
                    Err(TokenTableError::UnknownScope { id }) if id == "a"
                ),
                "{scopes}"
            );
        }
    }

    #[test]
    fn debug_output_never_contains_digests() {
        let table = TokenTable::parse(&two_token_table()).unwrap();
        let dump = format!("{table:?} {:?}", table.lookup(READ_TOKEN));
        for token in [READ_TOKEN, WRITE_TOKEN] {
            assert!(!dump.contains(token));
            let digest = TokenDigest::of(token);
            assert!(!dump.contains(&hex(token)));
            assert!(!dump.contains(&format!("{:?}", digest.0)));
            assert_eq!(format!("{digest:?}"), "TokenDigest(<redacted>)");
        }
        assert!(dump.contains("ops-read"));
    }

    #[test]
    fn load_reads_file_and_reports_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.toml");
        assert!(matches!(
            TokenTable::load(&path),
            Err(TokenTableError::Io(err)) if err.kind() == io::ErrorKind::NotFound
        ));
        std::fs::write(&path, two_token_table()).unwrap();
        let table = TokenTable::load(&path).unwrap();
        assert_eq!(table.lookup(WRITE_TOKEN).unwrap().id(), "admin");
    }
}
