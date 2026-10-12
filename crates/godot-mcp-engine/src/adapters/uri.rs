//! `file://` URIs and project paths the way Godot's language server and debug
//! adapter spell and read them.
//!
//! Godot 4.5 and later build every file URI as `file:///` and the absolute path
//! with each component percent-encoded, a Windows drive letter included
//! (`file:///C%3A/Users/...`); 4.4 prefixes the client's own root URI instead.
//! It reads a URI back by percent-decoding its path. On Windows it compares
//! paths with forward slashes, without a leading slash before the drive, and
//! without regard to case — and the editor's own resource path is spelled
//! `C:/...` there.

/// How the host spells an absolute path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStyle {
    /// `/`-separated, case-sensitive.
    Unix,
    /// A drive letter, `\` or `/` separators, case-insensitive.
    Windows,
}

impl PathStyle {
    /// This host's style.
    #[must_use]
    pub const fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// `path` with forward slashes where the host allows backslashes.
fn forward(path: &str, style: PathStyle) -> String {
    match style {
        PathStyle::Unix => path.to_owned(),
        PathStyle::Windows => path.replace('\\', "/"),
    }
}

/// Godot's `String::uri_encode`: letters, digits, `_`, `.`, `-` and `~` stay,
/// every other byte of the UTF-8 is `%XX` in upper-case hex.
fn encode_component(component: &str) -> String {
    let mut encoded = String::with_capacity(component.len());
    for byte in component.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Godot's `String::uri_file_decode`: every `%XX` is that byte, nothing else
/// changes.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            decoded.push(byte);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// The `file://` URI of the absolute `path`, spelled as Godot spells it.
#[must_use]
pub fn file_uri(path: &str, style: PathStyle) -> String {
    let forward = forward(path, style);
    let components: Vec<String> = forward
        .trim_start_matches('/')
        .split('/')
        .map(encode_component)
        .collect();
    format!("file:///{}", components.join("/"))
}

/// The path a `file://` URI names, decoded the way Godot decodes it, with the
/// leading slash before a Windows drive letter dropped. Anything that is not a
/// `file://` URI is returned as it is.
#[must_use]
pub fn path_of(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_owned();
    };
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let path = decode(rest);
    match path.as_bytes() {
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => path[1..].to_owned(),
        _ => path,
    }
}

/// Every spelling of the absolute `root` that Godot or this server may write
/// in an answer, without a trailing separator, the most specific first so a
/// URI is replaced before the bare path inside it.
#[must_use]
pub fn root_spellings(root: &str, style: PathStyle) -> Vec<String> {
    let root = root.trim_end_matches(['/', '\\']);
    let forward = forward(root, style);
    let mut spellings = vec![file_uri(root, style)];
    match style {
        PathStyle::Unix => spellings.push(format!("file://{root}")),
        PathStyle::Windows => {
            spellings.push(format!("file:///{forward}"));
            spellings.push(format!("file://{root}"));
        }
    }
    spellings.push(forward);
    spellings.push(root.to_owned());
    let mut unique: Vec<String> = Vec::with_capacity(spellings.len());
    for spelling in spellings {
        if !unique.contains(&spelling) {
            unique.push(spelling);
        }
    }
    unique
}

/// `path` relative to `root`, with forward slashes, when it lies strictly
/// below it. Windows compares without regard to case and accepts either
/// separator.
#[must_use]
pub fn relative_to(root: &str, path: &str, style: PathStyle) -> Option<String> {
    let root = forward(root, style);
    let root = root.trim_end_matches('/');
    let path = forward(path, style);
    let head = path.get(..root.len())?;
    let matches = match style {
        PathStyle::Unix => head == root,
        PathStyle::Windows => head.eq_ignore_ascii_case(root),
    };
    if !matches {
        return None;
    }
    let relative = path[root.len()..].strip_prefix('/')?;
    (!relative.is_empty()).then(|| relative.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_uri_is_spelled_as_godot_spells_it() {
        assert_eq!(
            file_uri("/tmp/godot-mcp-m5-x/game/player.gd", PathStyle::Unix),
            "file:///tmp/godot-mcp-m5-x/game/player.gd"
        );
        assert_eq!(
            file_uri("/home/qa/my game/a.gd", PathStyle::Unix),
            "file:///home/qa/my%20game/a.gd"
        );
        assert_eq!(
            file_uri(r"C:\Users\qa\game\scripts/player.gd", PathStyle::Windows),
            "file:///C%3A/Users/qa/game/scripts/player.gd"
        );
        assert_eq!(
            file_uri(r"C:\Users\qa\game", PathStyle::Windows),
            "file:///C%3A/Users/qa/game"
        );
    }

    #[test]
    fn a_file_uri_names_the_path_godot_reads_from_it() {
        assert_eq!(path_of("file:///tmp/game/a.gd"), "/tmp/game/a.gd");
        assert_eq!(path_of("file:///home/qa/my%20game"), "/home/qa/my game");
        assert_eq!(path_of("file:///C%3A/Users/qa/game"), "C:/Users/qa/game");
        assert_eq!(path_of("file:///C:/Users/qa/game"), "C:/Users/qa/game");
        assert_eq!(path_of("file://localhost/tmp/game"), "/tmp/game");
        assert_eq!(path_of("/already/a/path"), "/already/a/path");
        assert_eq!(path_of("res://player.gd"), "res://player.gd");
    }

    #[test]
    fn every_spelling_of_a_root_is_listed_most_specific_first() {
        assert_eq!(
            root_spellings("/tmp/game", PathStyle::Unix),
            ["file:///tmp/game", "/tmp/game"]
        );
        assert_eq!(
            root_spellings("/tmp/my game", PathStyle::Unix),
            [
                "file:///tmp/my%20game",
                "file:///tmp/my game",
                "/tmp/my game"
            ]
        );
        assert_eq!(
            root_spellings(r"C:\Users\qa\game", PathStyle::Windows),
            [
                "file:///C%3A/Users/qa/game",
                "file:///C:/Users/qa/game",
                r"file://C:\Users\qa\game",
                "C:/Users/qa/game",
                r"C:\Users\qa\game",
            ]
        );
    }

    #[test]
    fn a_path_is_inside_a_root_only_below_it() {
        assert_eq!(
            relative_to("/tmp/game", "/tmp/game/levels/one.gd", PathStyle::Unix).as_deref(),
            Some("levels/one.gd")
        );
        assert_eq!(
            relative_to("/tmp/game", "/tmp/gameplay/a.gd", PathStyle::Unix),
            None
        );
        assert_eq!(relative_to("/tmp/game", "/tmp/game", PathStyle::Unix), None);
        assert_eq!(
            relative_to("/tmp/game", "/Tmp/game/a.gd", PathStyle::Unix),
            None
        );
        assert_eq!(
            relative_to(
                r"C:\Users\qa\game",
                "c:/users/qa/game/levels/one.gd",
                PathStyle::Windows
            )
            .as_deref(),
            Some("levels/one.gd")
        );
        assert_eq!(
            relative_to(
                r"C:\Users\qa\game",
                r"C:\Users\qa\game\a.gd",
                PathStyle::Windows
            )
            .as_deref(),
            Some("a.gd")
        );
        assert_eq!(
            relative_to(
                r"C:\Users\qa\game",
                "C:/Users/qa/gameplay/a.gd",
                PathStyle::Windows
            ),
            None
        );
    }
}
