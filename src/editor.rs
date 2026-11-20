//! Text files for the pop-up editor: which files are text, and how their
//! bytes become the text on screen and back.

/// Larger files are not opened in the editor.
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;

const TEXT_EXTENSIONS: &[&str] = &[
    "txt",
    "md",
    "markdown",
    "rst",
    "log",
    "csv",
    "tsv",
    "json",
    "jsonl",
    "yaml",
    "yml",
    "toml",
    "xml",
    "ini",
    "cfg",
    "conf",
    "config",
    "properties",
    "env",
    "sh",
    "bash",
    "zsh",
    "fish",
    "bat",
    "cmd",
    "ps1",
    "go",
    "rs",
    "php",
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "tsx",
    "vue",
    "svelte",
    "html",
    "htm",
    "css",
    "scss",
    "less",
    "sql",
    "py",
    "rb",
    "pl",
    "lua",
    "java",
    "kt",
    "c",
    "h",
    "cpp",
    "hpp",
    "cs",
    "swift",
    "dart",
    "slint",
    "tf",
    "service",
    "lock",
    "diff",
    "patch",
    "tex",
    "srt",
    "gitignore",
    "htaccess",
    "editorconfig",
];

const TEXT_NAMES: &[&str] = &[
    "makefile",
    "dockerfile",
    "license",
    "readme",
    "authorized_keys",
];

/// Whether the name alone says the file is text.
pub fn is_text_name(name: &str) -> bool {
    let name = name.to_lowercase();
    // `.gitignore` counts as the extension `gitignore`.
    let ext = name.rsplit_once('.').map_or("", |(_, ext)| ext);
    TEXT_EXTENSIONS.contains(&ext) || TEXT_NAMES.contains(&name.as_str())
}

/// A file as the editor shows it.
#[derive(Debug, PartialEq)]
pub struct Doc {
    /// Lines end in `\n` here, also when the file uses `\r\n`.
    pub text: String,
    /// Every line of the file ends in `\r\n`; saving puts that back.
    pub crlf: bool,
}

/// Why the file is refused, in words that follow its name.
pub fn decode(name: &str, bytes: Vec<u8>) -> Result<Doc, &'static str> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err("is larger than 2 MB and cannot be opened in the editor.");
    }
    // An unknown kind of file: binary files have NUL bytes near the start.
    if !is_text_name(name) && bytes[..bytes.len().min(8192)].contains(&0) {
        return Err("is not a text file.");
    }
    let text = String::from_utf8(bytes).map_err(|_| "is not UTF-8 text.")?;
    let newlines = text.matches('\n').count();
    // A file that mixes both endings is left exactly as it is.
    let crlf = newlines > 0 && text.matches("\r\n").count() == newlines;
    Ok(Doc {
        text: if crlf {
            text.replace("\r\n", "\n")
        } else {
            text
        },
        crlf,
    })
}

/// The bytes to write for the text in the editor.
pub fn encode(text: &str, crlf: bool) -> Vec<u8> {
    if crlf {
        text.replace('\n', "\r\n").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

pub fn line_count(text: &str) -> usize {
    text.matches('\n').count() + 1
}

/// `1\n2\n3…`, shown next to the text.
pub fn line_numbers(count: usize) -> String {
    let numbers: Vec<String> = (1..=count).map(|n| n.to_string()).collect();
    numbers.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_names() {
        for name in [
            "a.txt",
            "NOTES.MD",
            "x.tar.json",
            ".gitignore",
            "Makefile",
            "nginx.conf",
        ] {
            assert!(is_text_name(name), "{name}");
        }
        for name in ["a.zip", "photo.png", "data", "a.bin"] {
            assert!(!is_text_name(name), "{name}");
        }
    }

    #[test]
    fn decoding() {
        let doc = decode("a.txt", b"one\r\ntwo\r\n".to_vec()).unwrap();
        assert_eq!((doc.text.as_str(), doc.crlf), ("one\ntwo\n", true));
        assert_eq!(encode(&doc.text, doc.crlf), b"one\r\ntwo\r\n");

        let doc = decode("a.txt", "satu\ndua é".as_bytes().to_vec()).unwrap();
        assert_eq!((doc.text.as_str(), doc.crlf), ("satu\ndua é", false));
        assert_eq!(encode(&doc.text, doc.crlf), "satu\ndua é".as_bytes());

        // Mixed endings pass through untouched.
        let doc = decode("a.txt", b"a\r\nb\nc".to_vec()).unwrap();
        assert_eq!(encode(&doc.text, doc.crlf), b"a\r\nb\nc");

        // Unknown names are sniffed; a text file without extension is fine.
        assert!(decode("data", b"plain words".to_vec()).is_ok());
        assert_eq!(
            decode("data", b"PK\x03\x04\0\0".to_vec()),
            Err("is not a text file.")
        );
        assert_eq!(
            decode("a.txt", vec![0xff, 0xfe, b'a']),
            Err("is not UTF-8 text.")
        );
        let big = vec![b'a'; MAX_BYTES as usize + 1];
        assert!(
            decode("a.txt", big)
                .unwrap_err()
                .contains("larger than 2 MB")
        );
    }

    #[test]
    fn numbering() {
        assert_eq!(line_count(""), 1);
        assert_eq!(line_count("a\nb\n"), 3);
        assert_eq!(line_numbers(3), "1\n2\n3");
    }
}
