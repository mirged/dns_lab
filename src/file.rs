//! File transfers over the message stream.
//!
//! A file is split into fixed-size parts and each part travels as an ordinary
//! signed envelope of type [`MsgType::File`](crate::protocol::MsgType::File).
//! The body carries a small ASCII header (transfer id, name, total size, hash,
//! part index) followed by the raw bytes of that part:
//!
//! ```text
//! DLF1
//! 1a2b3c4d
//! report.pdf
//! 1048576
//! 9f2c…                                    (sha256 of the whole file, hex)
//! 3/128
//! 8192
//!                                        <- blank line
//! <raw part bytes>
//! ```
//!
//! Everything the receiver needs to place the file is in the header, so parts
//! can arrive out of order, be re-sent, or be skipped on a resume.

use sha2::{Digest, Sha256};

/// Marks the header format this module understands.
pub const MAGIC: &str = "DLF1";
/// Longest accepted file name (bytes, after sanitising).
pub const MAX_NAME: usize = 128;
/// Default payload per part. Small enough that one lost part costs little,
/// large enough that a file is not thousands of parts.
pub const DEFAULT_PART_BYTES: usize = 8 * 1024;
/// Hard cap on a transfer; also bounds `parts` at 4096 with the default size.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Hard cap on the number of parts.
pub const MAX_PARTS: u32 = 8192;
/// Longest accepted part payload, independent of the header.
pub const MAX_PART_BYTES: usize = 256 * 1024;

/// One part's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub transfer_id: String,
    pub name: String,
    pub total_bytes: u64,
    pub sha256: String,
    pub part: u32,
    pub parts: u32,
    pub part_bytes: u32,
}

impl Header {
    /// `true` for the part that finishes the transfer.
    pub fn is_last(&self) -> bool {
        self.part + 1 == self.parts
    }
}

/// Strip anything that could escape the download directory or break the
/// header: keep the base name, drop control characters, cap the length.
pub fn sanitize_name(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("file")
        .trim()
        .trim_start_matches('.');
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{0}')
        .collect();
    let cleaned = if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    };
    let mut out = String::new();
    for c in cleaned.chars() {
        if out.len() + c.len_utf8() > MAX_NAME {
            break;
        }
        out.push(c);
    }
    out
}

/// A human-readable size, used for chat rows and progress lines.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Build the body of one part.
pub fn encode_body(header: &Header, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 160);
    out.extend_from_slice(MAGIC.as_bytes());
    for field in [
        header.transfer_id.as_str(),
        header.name.as_str(),
        &header.total_bytes.to_string(),
        header.sha256.as_str(),
        &format!("{}/{}", header.part, header.parts),
        &header.part_bytes.to_string(),
    ] {
        out.push(b'\n');
        out.extend_from_slice(field.as_bytes());
    }
    out.extend_from_slice(b"\n\n");
    out.extend_from_slice(payload);
    out
}

/// Parse a part body back into its header and payload.
pub fn decode_body(body: &[u8]) -> Result<(Header, Vec<u8>), String> {
    let split = body
        .windows(2)
        .position(|w| w == b"\n\n")
        .ok_or("file part: missing header separator")?;
    let head = std::str::from_utf8(&body[..split]).map_err(|_| "file part: header is not utf-8")?;
    let payload = body[split + 2..].to_vec();
    if payload.len() > MAX_PART_BYTES {
        return Err(format!(
            "file part: {}-byte payload exceeds the {MAX_PART_BYTES}-byte limit",
            payload.len()
        ));
    }

    let mut lines = head.split('\n');
    let magic = lines.next().unwrap_or_default();
    if magic != MAGIC {
        return Err(format!("file part: unknown header {magic:?}"));
    }
    let mut next = |what: &str| -> Result<String, String> {
        lines
            .next()
            .map(|s| s.to_string())
            .ok_or_else(|| format!("file part: header is missing {what}"))
    };

    let transfer_id = next("transfer id")?;
    if transfer_id.len() != 8 || !transfer_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("file part: bad transfer id {transfer_id:?}"));
    }
    let name = sanitize_name(&next("name")?);
    let total_bytes: u64 = next("size")?
        .parse()
        .map_err(|_| "file part: bad size".to_string())?;
    if total_bytes > MAX_FILE_BYTES {
        return Err(format!(
            "file part: {total_bytes} bytes exceeds the {MAX_FILE_BYTES}-byte limit"
        ));
    }
    let sha256 = next("sha256")?;
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("file part: bad sha256".to_string());
    }
    let idx = next("part index")?;
    let (part, parts) = idx
        .split_once('/')
        .ok_or("file part: part index is not `n/total`")?;
    let part: u32 = part.parse().map_err(|_| "file part: bad part index")?;
    let parts: u32 = parts.parse().map_err(|_| "file part: bad part count")?;
    if parts == 0 || parts > MAX_PARTS || part >= parts {
        return Err(format!("file part: {part}/{parts} is out of range"));
    }
    let part_bytes: u32 = next("part size")?
        .parse()
        .map_err(|_| "file part: bad part size".to_string())?;
    if part_bytes == 0 || part_bytes as usize > MAX_PART_BYTES {
        return Err("file part: bad part size".to_string());
    }
    if lines.next().is_some() {
        return Err("file part: trailing junk in the header".to_string());
    }

    Ok((
        Header {
            transfer_id,
            name,
            total_bytes,
            sha256,
            part,
            parts,
            part_bytes,
        },
        payload,
    ))
}

/// Split `data` into part payloads `(part, bytes)`.
pub fn split_parts(data: &[u8], part_bytes: usize) -> Vec<(u32, &[u8])> {
    let part_bytes = part_bytes.clamp(1, MAX_PART_BYTES);
    data.chunks(part_bytes)
        .enumerate()
        .map(|(i, chunk)| (i as u32, chunk))
        .collect()
}

/// `sha256` of a whole file, as lowercase hex.
pub fn digest(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Verify an assembled file against the hash in its header.
pub fn verify(data: &[u8], sha256: &str) -> bool {
    digest(data).eq_ignore_ascii_case(sha256)
}

/// Number of parts a transfer of `total` bytes needs at `part_bytes`.
pub fn part_count(total: u64, part_bytes: usize) -> u32 {
    let step = part_bytes.clamp(1, MAX_PART_BYTES) as u64;
    total.div_ceil(step).max(1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            transfer_id: "0a1b2c3d".to_string(),
            name: "notes.txt".to_string(),
            total_bytes: 5,
            sha256: digest(b"hello"),
            part: 0,
            parts: 1,
            part_bytes: DEFAULT_PART_BYTES as u32,
        }
    }

    #[test]
    fn a_part_round_trips() {
        let h = header();
        let body = encode_body(&h, b"hello");
        let (back, payload) = decode_body(&body).unwrap();
        assert_eq!(back, h);
        assert_eq!(payload, b"hello");
        assert!(verify(&payload, &h.sha256));
    }

    #[test]
    fn binary_payloads_survive_with_newlines_inside() {
        let h = header();
        let payload: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let body = encode_body(&h, &payload);
        let (_, back) = decode_body(&body).unwrap();
        assert_eq!(
            back, payload,
            "the header split must be the first blank line"
        );
    }

    #[test]
    fn names_cannot_escape_the_download_directory() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("/tmp/abs/file.bin"), "file.bin");
        assert_eq!(sanitize_name(".."), "file");
        assert_eq!(sanitize_name("with\u{7}bell"), "withbell");
        assert_eq!(sanitize_name("C:\\Users\\a\\b.txt"), "b.txt");
        assert!(sanitize_name(&"x".repeat(500)).len() <= MAX_NAME);
    }

    #[test]
    fn hashes_match_the_known_vector() {
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(!verify(b"abd", &digest(b"abc")));
    }

    #[test]
    fn parts_cover_the_file_exactly() {
        let data = vec![7u8; 20];
        let parts = split_parts(&data, 8);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.iter().map(|(_, p)| p.len()).sum::<usize>(), 20);
        assert_eq!(part_count(20, 8), 3);
        assert_eq!(part_count(0, 8), 1, "an empty file is still one part");
    }

    #[test]
    fn bad_headers_are_rejected() {
        assert!(decode_body(b"nope").is_err());
        assert!(decode_body(b"DLF2\n0a1b2c3d\na\n5\n").is_err());

        let mut h = header();
        h.part = 3;
        h.parts = 2;
        let out = decode_body(&encode_body(&h, b"x"));
        assert!(out.is_err(), "part index beyond the part count");

        let mut h = header();
        h.sha256 = "abc".to_string();
        assert!(decode_body(&encode_body(&h, b"x")).is_err());

        let mut h = header();
        h.transfer_id = "zz".to_string();
        assert!(decode_body(&encode_body(&h, b"x")).is_err());
    }

    #[test]
    fn human_sizes_read_like_a_file_manager() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }
}
