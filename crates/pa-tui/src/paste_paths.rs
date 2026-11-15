//! File-path pastes for the prompt editor: a paste whose whole payload is one or more
//! existing file paths is a file paste, not text. Supported images among them become
//! `[image #N]` attachments; every other file keeps its path as text. A terminal delivers
//! a dropped file as exactly that path text, so this module is the drag-and-drop route into
//! the editor. A paste that merely looks path-shaped (a single path with no supported
//! image, a partial match, any prose) keeps its text: the caller pastes it verbatim.

use std::path::PathBuf;

use crate::image_load::{load_image_from_path, LoadedImage};

/// The largest image file a paste attaches: a bigger file stays its path text rather than
/// filling the editor's attachment registry with one huge payload.
const MAX_PASTED_IMAGE_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// The most image file bytes one paste loads across its files: a bulk drop must not
/// buffer an unbounded payload before the editor's attachment registry sees it. Files
/// past the budget keep their path text.
const MAX_PASTED_IMAGE_PASTE_BYTES: u64 = 64 * 1024 * 1024;

/// The most paths one paste resolves: a file-manager drop never carries more, and a long
/// text paste must not stat every one of its lines on the input path.
const MAX_PASTED_PATHS: usize = 256;

/// One file named by a paste.
pub(crate) struct PastedFile {
    pub(crate) path: PathBuf,
    /// The attachment payload when the file is a supported image.
    pub(crate) image: Option<LoadedImage>,
}

impl PastedFile {
    /// The existing files among the first [`MAX_PASTED_PATHS`] paths, in order; a supported
    /// image within the paste's byte budget carries its loaded payload. A directory, a
    /// vanished path, an unreadable file, an oversized image, and an image past the budget
    /// all drop out — the caller compares lengths to tell a partial match from a whole one.
    pub(crate) fn existing(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PastedFile> {
        let mut image_budget = MAX_PASTED_IMAGE_PASTE_BYTES;
        paths
            .into_iter()
            .take(MAX_PASTED_PATHS)
            .filter_map(|path| {
                let metadata = std::fs::metadata(&path).ok()?;
                if !metadata.is_file() {
                    return None;
                }
                let image = (metadata.len() <= MAX_PASTED_IMAGE_FILE_BYTES
                    && metadata.len() <= image_budget)
                    .then(|| load_image_from_path(&path).ok().flatten())
                    .flatten();
                if let Some(image) = &image {
                    image_budget = image_budget.saturating_sub(image.data.len() as u64);
                }
                Some(PastedFile { path, image })
            })
            .collect()
    }
}

/// The files a paste names, when the WHOLE payload is existing file paths and at least one
/// of them is a supported image. `None` means an ordinary text paste.
pub(crate) fn pasted_files(text: &str) -> Option<Vec<PastedFile>> {
    let paths = pasted_paths(text)?;
    let expected = paths.len();
    let files = PastedFile::existing(paths);
    // One unresolvable token (a directory, a vanished file, an oversized image) keeps the
    // payload text: a partly-quoted paste is probably prose, not a file drop.
    (files.len() == expected && files.iter().any(|file| file.image.is_some())).then_some(files)
}

/// The path tokens of a payload, when the payload is nothing but path tokens: one token per
/// line, one or more quoted tokens covering the whole text (a multi-file drop quotes every
/// path), or a single bare token (a path with spaces included). A bare token may itself
/// contain a quote character — a path is not prose just because a file name has an
/// apostrophe.
fn pasted_paths(text: &str) -> Option<Vec<PathBuf>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('\n') {
        let lines: Vec<&str> = trimmed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if lines.is_empty() || lines.len() > MAX_PASTED_PATHS {
            return None;
        }
        return Some(lines.iter().map(|line| path_from_token(line)).collect());
    }
    if trimmed.starts_with(['"', '\'']) {
        let mut paths = Vec::new();
        let mut rest = trimmed;
        while !rest.is_empty() {
            let quote = rest.chars().next()?;
            if quote != '"' && quote != '\'' {
                return None;
            }
            let end = rest[1..].find(quote)? + 1;
            paths.push(path_from_token(&rest[1..end]));
            if paths.len() > MAX_PASTED_PATHS {
                return None;
            }
            rest = rest[end + 1..].trim_start();
        }
        return (!paths.is_empty()).then_some(paths);
    }
    Some(vec![path_from_token(trimmed)])
}

/// The file paths of a `text/uri-list` clipboard payload (`file://` URIs; `#` comments and
/// blank lines ignored).
pub(crate) fn uri_list_paths(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with("file://"))
        .filter_map(file_uri_path)
        .collect()
}

/// One pasted token as a path: a `file://` URI decodes, anything else is taken verbatim.
fn path_from_token(token: &str) -> PathBuf {
    file_uri_path(token).unwrap_or_else(|| PathBuf::from(token))
}

/// A `file://` URI as a path, percent-decoded. `file:///C:/shot.png` keeps the slash the
/// URI grammar puts before a drive letter; a filesystem path drops it.
fn file_uri_path(token: &str) -> Option<PathBuf> {
    let rest = token.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    let bytes = decoded.as_bytes();
    let drive = decoded.starts_with('/')
        && bytes.get(1).is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(2) == Some(&b':');
    Some(PathBuf::from(if drive {
        decoded.trim_start_matches('/').to_string()
    } else {
        decoded
    }))
}

/// Decode `%XX` escapes into UTF-8 bytes; a malformed escape stays literal.
fn percent_decode(input: &str) -> String {
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    };
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (digit(bytes[index + 1]), digit(bytes[index + 2])) {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0];

    /// A file under `dir` that lives for the test (the dir guard drops it after).
    fn file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write fixture");
        path
    }

    #[test]
    fn a_quoted_path_with_spaces_attaches_its_image() {
        let dir = tempfile::tempdir().expect("temp dir");
        let image = file(dir.path(), "screen shot.png", PNG);
        let pasted = format!("\"{}\"", image.display());
        let files = pasted_files(&pasted).expect("the quoted image path is a file paste");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, image);
        assert_eq!(
            files[0]
                .image
                .as_ref()
                .map(|image| image.mime_type.as_str()),
            Some("image/png")
        );
    }

    #[test]
    fn a_multi_file_drop_attaches_images_and_keeps_other_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        let image = file(dir.path(), "a.png", PNG);
        let notes = file(dir.path(), "b notes.txt", b"text");
        let pasted = format!("\"{}\" \"{}\"", image.display(), notes.display());
        let files = pasted_files(&pasted).expect("the drop is a file paste");
        assert_eq!(
            files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>(),
            vec![image, notes]
        );
        assert!(files[0].image.is_some());
        assert_eq!(files[1].image, None, "a non-image file keeps its path");
    }

    #[test]
    fn one_path_per_line_is_a_file_paste() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = file(dir.path(), "one.png", PNG);
        let second = file(dir.path(), "two.png", PNG);
        let pasted = format!("{}\n{}\n", first.display(), second.display());
        let files = pasted_files(&pasted).expect("the line-per-path payload is a file paste");
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn prose_and_partial_paths_keep_their_text() {
        let dir = tempfile::tempdir().expect("temp dir");
        let image = file(dir.path(), "a.png", PNG);
        let notes = file(dir.path(), "notes.txt", b"text");
        let cases = [
            // A non-image file alone is ordinary text (typing a path stays typing it).
            notes.display().to_string(),
            // Prose around a path is prose.
            format!("look at {}", image.display()),
            format!("look at \"{}\"", image.display()),
            // A quoted token plus a bare one does not cover the payload.
            format!("\"{}\" tail", image.display()),
            // A path that exists among ones that do not is a partial match.
            format!(
                "{}\n{}",
                image.display(),
                dir.path().join("gone.png").display()
            ),
            // A directory is not a pasted file.
            format!("\"{}\"", dir.path().display()),
            String::new(),
        ];
        for case in cases {
            assert!(pasted_files(&case).is_none(), "stays text: {case:?}");
        }
    }

    #[test]
    fn image_signatures_decide_not_extensions() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = file(dir.path(), "fake.png", b"not an image");
        assert!(
            pasted_files(&fake.display().to_string()).is_none(),
            "an existing non-image file alone stays text"
        );
        let real = file(dir.path(), "photo.txt", PNG);
        assert!(
            pasted_files(&real.display().to_string()).is_some(),
            "the content, not the name, makes the attachment"
        );
    }

    #[test]
    fn a_bare_path_with_an_apostrophe_still_attaches_its_image() {
        let dir = tempfile::tempdir().expect("temp dir");
        let image = file(dir.path(), "jev's shot.png", PNG);
        let files = pasted_files(&image.display().to_string())
            .expect("a file name's apostrophe does not make the payload prose");
        assert_eq!(files.len(), 1);
        assert!(files[0].image.is_some());
    }

    #[test]
    fn uri_lists_decode_percent_escapes_and_drive_roots() {
        assert_eq!(
            uri_list_paths(
                "file:///home/me/my%20shot.png\r\n# comment\r\nhttps://example.com/x.png\n"
            ),
            vec![PathBuf::from("/home/me/my shot.png")]
        );
        assert_eq!(
            uri_list_paths("file:///C:/shots/a%2Bb.png"),
            vec![PathBuf::from("C:/shots/a+b.png")]
        );
        assert_eq!(uri_list_paths(""), Vec::<PathBuf>::new());
    }

    #[test]
    fn percent_decode_leaves_malformed_escapes_literal() {
        assert_eq!(percent_decode("a%zz%4"), "a%zz%4");
        assert_eq!(percent_decode("%E4%B8%AD"), "\u{4e2d}");
    }
}
