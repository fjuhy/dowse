//! Native reader for Microsoft Outlook `.pst` / `.ost` Personal Folders (`[MS-PST]`),
//! standalone `.msg` (`[MS-OXMSG]` via CFB), and `.eml` (RFC 5322 MIME) files.
//!
//! Design invariants:
//! - Uses bounded random-access reads (`Seek` + page/block reads) for `.pst`/`.ost`,
//!   never loading multi-GB archives into memory.
//! - Supports ANSI 32-bit (`wVer` 14/15), Unicode 64-bit (`wVer` 21..=23), and
//!   Outlook 2013+ 4K-page compressed Unicode (`wVer` 36).
//! - Supports `NDB_CRYPT_NONE` (0), `NDB_CRYPT_PERMUTE` (1, compressible encryption),
//!   and `NDB_CRYPT_CYCLIC` (2).
//! - Extracts plain text (`PidTagBody`), HTML (`PidTagHtml`), or LZFu-compressed RTF
//!   (`PidTagRtfCompressed`), folder hierarchy paths, sender/recipients, timestamps,
//!   and attachment filenames.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use flate2::read::ZlibDecoder;

use crate::extract::{
    DocProps, ExtractedDoc, attachment_extension, clean_html_fragment, decode_bytes_with_codepage,
    extract_attachment,
};

const PST_MAGIC: &[u8; 4] = b"!BDN";
const MAX_PST_MESSAGES: usize = 50_000;
const MAX_MESSAGE_BODY_CHARS: usize = 32_000;
const MAX_BLOCK_STREAM_BYTES: usize = 4 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
const MAX_ATTACHMENT_TEXT_BYTES: usize = 64 * 1024;
const MAX_MESSAGE_ATTACHMENT_TEXT_BYTES: usize = 128 * 1024;
const ATTACH_DATA_BIN: u16 = 0x3701;
const ATTACH_METHOD: u16 = 0x3705;
const UNIX_EPOCH_FT_TICKS: u64 = 116_444_736_000_000_000;

/// Outlook compressible-encryption substitution table (`[MS-PST] 5.1`).
const COMP_ENC: [u8; 256] = [
    0x47, 0xf1, 0xb4, 0xe6, 0x0b, 0x6a, 0x72, 0x48, 0x85, 0x4e, 0x9e, 0xeb, 0xe2, 0xf8, 0x94, 0x53,
    0xe0, 0xbb, 0xa0, 0x02, 0xe8, 0x5a, 0x09, 0xab, 0xdb, 0xe3, 0xba, 0xc6, 0x7c, 0xc3, 0x10, 0xdd,
    0x39, 0x05, 0x96, 0x30, 0xf5, 0x37, 0x60, 0x82, 0x8c, 0xc9, 0x13, 0x4a, 0x6b, 0x1d, 0xf3, 0xfb,
    0x8f, 0x26, 0x97, 0xca, 0x91, 0x17, 0x01, 0xc4, 0x32, 0x2d, 0x6e, 0x31, 0x95, 0xff, 0xd9, 0x23,
    0xd1, 0x00, 0x5e, 0x79, 0xdc, 0x44, 0x3b, 0x1a, 0x28, 0xc5, 0x61, 0x57, 0x20, 0x90, 0x3d, 0x83,
    0xb9, 0x43, 0xbe, 0x67, 0xd2, 0x46, 0x42, 0x76, 0xc0, 0x6d, 0x5b, 0x7e, 0xb2, 0x0f, 0x16, 0x29,
    0x3c, 0xa9, 0x03, 0x54, 0x0d, 0xda, 0x5d, 0xdf, 0xf6, 0xb7, 0xc7, 0x62, 0xcd, 0x8d, 0x06, 0xd3,
    0x69, 0x5c, 0x86, 0xd6, 0x14, 0xf7, 0xa5, 0x66, 0x75, 0xac, 0xb1, 0xe9, 0x45, 0x21, 0x70, 0x0c,
    0x87, 0x9f, 0x74, 0xa4, 0x22, 0x4c, 0x6f, 0xbf, 0x1f, 0x56, 0xaa, 0x2e, 0xb3, 0x78, 0x33, 0x50,
    0xb0, 0xa3, 0x92, 0xbc, 0xcf, 0x19, 0x1c, 0xa7, 0x63, 0xcb, 0x1e, 0x4d, 0x3e, 0x4b, 0x1b, 0x9b,
    0x4f, 0xe7, 0xf0, 0xee, 0xad, 0x3a, 0xb5, 0x59, 0x04, 0xea, 0x40, 0x55, 0x25, 0x51, 0xe5, 0x7a,
    0x89, 0x38, 0x68, 0x52, 0x7b, 0xfc, 0x27, 0xae, 0xd7, 0xbd, 0xfa, 0x07, 0xf4, 0xcc, 0x8e, 0x5f,
    0xef, 0x35, 0x9c, 0x84, 0x2b, 0x15, 0xd5, 0x77, 0x34, 0x49, 0xb6, 0x12, 0x0a, 0x7f, 0x71, 0x88,
    0xfd, 0x9d, 0x18, 0x41, 0x7d, 0x93, 0xd8, 0x58, 0x2c, 0xce, 0xfe, 0x24, 0xaf, 0xde, 0xb8, 0x36,
    0xc8, 0xa1, 0x80, 0xa6, 0x99, 0x98, 0xa8, 0x2f, 0x0e, 0x81, 0x65, 0x73, 0xe4, 0xc2, 0xa2, 0x8a,
    0xd4, 0xe1, 0x11, 0xd0, 0x08, 0x8b, 0x2a, 0xf2, 0xed, 0x9a, 0x64, 0x3f, 0xc1, 0x6c, 0xf9, 0xec,
];

const COMP_HIGH1: [u8; 256] = [
    0x41, 0x36, 0x13, 0x62, 0xa8, 0x21, 0x6e, 0xbb, 0xf4, 0x16, 0xcc, 0x04, 0x7f, 0x64, 0xe8, 0x5d,
    0x1e, 0xf2, 0xcb, 0x2a, 0x74, 0xc5, 0x5e, 0x35, 0xd2, 0x95, 0x47, 0x9e, 0x96, 0x2d, 0x9a, 0x88,
    0x4c, 0x7d, 0x84, 0x3f, 0xdb, 0xac, 0x31, 0xb6, 0x48, 0x5f, 0xf6, 0xc4, 0xd8, 0x39, 0x8b, 0xe7,
    0x23, 0x3b, 0x38, 0x8e, 0xc8, 0xc1, 0xdf, 0x25, 0xb1, 0x20, 0xa5, 0x46, 0x60, 0x4e, 0x9c, 0xfb,
    0xaa, 0xd3, 0x56, 0x51, 0x45, 0x7c, 0x55, 0x00, 0x07, 0xc9, 0x2b, 0x9d, 0x85, 0x9b, 0x09, 0xa0,
    0x8f, 0xad, 0xb3, 0x0f, 0x63, 0xab, 0x89, 0x4b, 0xd7, 0xa7, 0x15, 0x5a, 0x71, 0x66, 0x42, 0xbf,
    0x26, 0x4a, 0x6b, 0x98, 0xfa, 0xea, 0x77, 0x53, 0xb2, 0x70, 0x05, 0x2c, 0xfd, 0x59, 0x3a, 0x86,
    0x7e, 0xce, 0x06, 0xeb, 0x82, 0x78, 0x57, 0xc7, 0x8d, 0x43, 0xaf, 0xb4, 0x1c, 0xd4, 0x5b, 0xcd,
    0xe2, 0xe9, 0x27, 0x4f, 0xc3, 0x08, 0x72, 0x80, 0xcf, 0xb0, 0xef, 0xf5, 0x28, 0x6d, 0xbe, 0x30,
    0x4d, 0x34, 0x92, 0xd5, 0x0e, 0x3c, 0x22, 0x32, 0xe5, 0xe4, 0xf9, 0x9f, 0xc2, 0xd1, 0x0a, 0x81,
    0x12, 0xe1, 0xee, 0x91, 0x83, 0x76, 0xe3, 0x97, 0xe6, 0x61, 0x8a, 0x17, 0x79, 0xa4, 0xb7, 0xdc,
    0x90, 0x7a, 0x5c, 0x8c, 0x02, 0xa6, 0xca, 0x69, 0xde, 0x50, 0x1a, 0x11, 0x93, 0xb9, 0x52, 0x87,
    0x58, 0xfc, 0xed, 0x1d, 0x37, 0x49, 0x1b, 0x6a, 0xe0, 0x29, 0x33, 0x99, 0xbd, 0x6c, 0xd9, 0x94,
    0xf3, 0x40, 0x54, 0x6f, 0xf0, 0xc6, 0x73, 0xb8, 0xd6, 0x3e, 0x65, 0x18, 0x44, 0x1f, 0xdd, 0x67,
    0x10, 0xf1, 0x0c, 0x19, 0xec, 0xae, 0x03, 0xa1, 0x14, 0x7b, 0xa9, 0x0b, 0xff, 0xf8, 0xa3, 0xc0,
    0xa2, 0x01, 0xf7, 0x2e, 0xbc, 0x24, 0x68, 0x75, 0x0d, 0xfe, 0xba, 0x2f, 0xb5, 0xd0, 0xda, 0x3d,
];

const COMP_HIGH2: [u8; 256] = [
    0x14, 0x53, 0x0f, 0x56, 0xb3, 0xc8, 0x7a, 0x9c, 0xeb, 0x65, 0x48, 0x17, 0x16, 0x15, 0x9f, 0x02,
    0xcc, 0x54, 0x7c, 0x83, 0x00, 0x0d, 0x0c, 0x0b, 0xa2, 0x62, 0xa8, 0x76, 0xdb, 0xd9, 0xed, 0xc7,
    0xc5, 0xa4, 0xdc, 0xac, 0x85, 0x74, 0xd6, 0xd0, 0xa7, 0x9b, 0xae, 0x9a, 0x96, 0x71, 0x66, 0xc3,
    0x63, 0x99, 0xb8, 0xdd, 0x73, 0x92, 0x8e, 0x84, 0x7d, 0xa5, 0x5e, 0xd1, 0x5d, 0x93, 0xb1, 0x57,
    0x51, 0x50, 0x80, 0x89, 0x52, 0x94, 0x4f, 0x4e, 0x0a, 0x6b, 0xbc, 0x8d, 0x7f, 0x6e, 0x47, 0x46,
    0x41, 0x40, 0x44, 0x01, 0x11, 0xcb, 0x03, 0x3f, 0xf7, 0xf4, 0xe1, 0xa9, 0x8f, 0x3c, 0x3a, 0xf9,
    0xfb, 0xf0, 0x19, 0x30, 0x82, 0x09, 0x2e, 0xc9, 0x9d, 0xa0, 0x86, 0x49, 0xee, 0x6f, 0x4d, 0x6d,
    0xc4, 0x2d, 0x81, 0x34, 0x25, 0x87, 0x1b, 0x88, 0xaa, 0xfc, 0x06, 0xa1, 0x12, 0x38, 0xfd, 0x4c,
    0x42, 0x72, 0x64, 0x13, 0x37, 0x24, 0x6a, 0x75, 0x77, 0x43, 0xff, 0xe6, 0xb4, 0x4b, 0x36, 0x5c,
    0xe4, 0xd8, 0x35, 0x3d, 0x45, 0xb9, 0x2c, 0xec, 0xb7, 0x31, 0x2b, 0x29, 0x07, 0x68, 0xa3, 0x0e,
    0x69, 0x7b, 0x18, 0x9e, 0x21, 0x39, 0xbe, 0x28, 0x1a, 0x5b, 0x78, 0xf5, 0x23, 0xca, 0x2a, 0xb0,
    0xaf, 0x3e, 0xfe, 0x04, 0x8c, 0xe7, 0xe5, 0x98, 0x32, 0x95, 0xd3, 0xf6, 0x4a, 0xe8, 0xa6, 0xea,
    0xe9, 0xf3, 0xd5, 0x2f, 0x70, 0x20, 0xf2, 0x1f, 0x05, 0x67, 0xad, 0x55, 0x10, 0xce, 0xcd, 0xe3,
    0x27, 0x3b, 0xda, 0xba, 0xd7, 0xc2, 0x26, 0xd4, 0x91, 0x1d, 0xd2, 0x1c, 0x22, 0x33, 0xf8, 0xfa,
    0xf1, 0x5a, 0xef, 0xcf, 0x90, 0xb6, 0x8b, 0xb5, 0xbd, 0xc0, 0xbf, 0x08, 0x97, 0x1e, 0x6c, 0xe2,
    0x61, 0xe0, 0xc6, 0xc1, 0x59, 0xab, 0xbb, 0x58, 0xde, 0x5f, 0xdf, 0x60, 0x79, 0x7e, 0xb2, 0x8a,
];

const LZFU_PREBUF: &[u8] = b"{\\rtf1\\ansi\\mac\\deff0\\deftab720{\\fonttbl;}{\\f0\\fnil \\froman \\fswiss \\fmodern \\fscript \\fdecor MS Sans SerifSymbolArialTimes New RomanCourier{\\colortbl\\red0\\green0\\blue0\n\r\\par \\pard\\plain\\f0\\fs20\\b\\i\\u\\tab\\tx";

#[derive(Debug, Clone)]
pub(crate) struct PstEmail {
    pub nid: u64,
    pub folder_path: String,
    pub subject: String,
    pub sender: Option<String>,
    pub timestamp_ms: Option<i64>,
    pub created_ms: Option<i64>,
    pub modified_ms: Option<i64>,
    pub content: String,
}

#[derive(Debug)]
pub(crate) struct PstArchiveExtract {
    pub summary_text: String,
    pub emails: Vec<PstEmail>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PstFormat {
    Ansi32,
    Unicode64,
    Unicode4k,
}

#[derive(Clone, Copy)]
struct BbtEntry {
    offset: u64,
    stored_size: u16,
    inflated_size: u16,
}

#[derive(Clone, Copy)]
struct NbtEntry {
    nid: u64,
    bid_data: u64,
    bid_sub: u64,
    nid_parent: u32,
}

struct PstReader {
    file: File,
    file_len: u64,
    format: PstFormat,
    crypt: u8,
    bbt: HashMap<u64, BbtEntry>,
    nbt: Vec<NbtEntry>,
}

pub(crate) fn is_pst_container(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pst") || e.eq_ignore_ascii_case("ost"))
}

pub(crate) fn extract_pst_archive(path: &Path) -> Option<PstArchiveExtract> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        extract_pst_archive_inner(path)
    }))
    .ok()
    .flatten()
}

fn extract_pst_archive_inner(path: &Path) -> Option<PstArchiveExtract> {
    let mut reader = PstReader::open(path)?;

    // 1. Reconstruct folder names (`(nid & 0x1F) == 0x02` or `0x03`)
    let mut folder_raw: HashMap<u32, (u32, String)> = HashMap::new();
    let nbt_snapshot = reader.nbt.clone();
    for entry in &nbt_snapshot {
        let nid_type = entry.nid & 0x1F;
        if (nid_type == 0x02 || nid_type == 0x03) && entry.bid_data != 0 {
            if let Some(props) = reader.read_property_context(entry.bid_data, entry.bid_sub)
                && let Some(name) = props.get_string(0x3001)
            {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    folder_raw.insert(entry.nid as u32, (entry.nid_parent, trimmed.to_string()));
                }
            }
        }
    }

    let resolve_folder_path = |mut cur: u32| -> String {
        let mut parts = Vec::new();
        let mut depth = 0usize;
        while let Some((parent, name)) = folder_raw.get(&cur) {
            if !matches!(
                name.as_str(),
                "Top of Personal Folders"
                    | "Top of Outlook data file"
                    | "개인 폴더의 맨 위"
                    | "Outlook 데이터 파일의 맨 위"
                    | "IPM_SUBTREE"
                    | "Root - Mailbox"
            ) {
                parts.push(name.as_str());
            }
            if *parent == 0 || *parent == cur || depth >= 16 {
                break;
            }
            cur = *parent;
            depth += 1;
        }
        parts.reverse();
        parts.join(" / ")
    };

    // 2. Stream normal message nodes (`(nid & 0x1F) == 0x04`)
    let mut emails = Vec::new();
    let mut folder_counts: HashMap<String, usize> = HashMap::new();

    for entry in &nbt_snapshot {
        if emails.len() >= MAX_PST_MESSAGES {
            break;
        }
        if (entry.nid & 0x1F) != 0x04 || entry.bid_data == 0 {
            continue;
        }
        let Some(props) = reader.read_property_context(entry.bid_data, entry.bid_sub) else {
            continue;
        };

        let folder_path = resolve_folder_path(entry.nid_parent);
        let subject = props
            .get_subject()
            .unwrap_or_else(|| "(제목 없음)".to_string());
        let sender = props.get_sender();
        let to_line = props.get_string(0x0E04).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let cc_line = props.get_string(0x0E03).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let received_ms = props.get_filetime_ms(0x0E06);
        let sent_ms = props.get_filetime_ms(0x0039);
        let created_ms = props.get_filetime_ms(0x3007);
        let modified_ms = props.get_filetime_ms(0x3008);
        let timestamp_ms = received_ms.or(sent_ms).or(modified_ms).or(created_ms);
        use std::fmt::Write as _;

        let (attachments, attachment_text) = reader.read_attachments(entry.bid_sub);
        let body = props.get_best_body().unwrap_or_default();

        if subject == "(제목 없음)" && body.trim().is_empty() && attachments.is_empty() {
            continue;
        }

        let folder_key = if folder_path.is_empty() {
            "(기본 편지함)".to_string()
        } else {
            folder_path.clone()
        };
        *folder_counts.entry(folder_key).or_insert(0) += 1;

        let mut text = String::with_capacity(body.len().min(MAX_MESSAGE_BODY_CHARS) + 256);
        if !folder_path.is_empty() {
            text.push_str(&format!("편지함: {folder_path}\n"));
        }
        text.push_str(&format!("제목: {subject}\n"));
        if let Some(ref s) = sender {
            text.push_str(&format!("보낸사람: {s}\n"));
        }
        if let Some(ref to) = to_line {
            text.push_str(&format!("받는사람: {to}\n"));
        }
        if let Some(ref cc) = cc_line {
            text.push_str(&format!("참조: {cc}\n"));
        }
        for (label, timestamp) in [
            ("보낸 시각", sent_ms),
            ("받은 시각", received_ms),
            ("메일 생성 시각", created_ms),
            ("메일 수정 시각", modified_ms),
        ] {
            if let Some(utc) = timestamp.and_then(format_utc_ymd_hm) {
                let _ = writeln!(text, "{label}: {utc} UTC");
            }
        }
        if !attachments.is_empty() {
            text.push_str(&format!("첨부파일: {}\n", attachments.join(", ")));
        }
        text.push('\n');
        for ch in body.chars().take(MAX_MESSAGE_BODY_CHARS) {
            text.push(ch);
        }
        text.push_str(&attachment_text);

        emails.push(PstEmail {
            nid: entry.nid,
            folder_path,
            subject,
            sender,
            timestamp_ms,
            created_ms,
            modified_ms,
            content: text,
        });
    }

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "outlook.pst".to_string());

    let mut summary = format!(
        "Outlook 데이터 파일 ({file_name}) 요약: 총 {}통의 메일/항목 색인됨\n",
        emails.len()
    );
    if !folder_counts.is_empty() {
        let mut sorted_folders: Vec<(String, usize)> = folder_counts.into_iter().collect();
        sorted_folders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        summary.push_str("편지함 구성:\n");
        for (fname, cnt) in sorted_folders.iter().take(40) {
            summary.push_str(&format!("- {fname}: {cnt}통\n"));
        }
    }
    if !emails.is_empty() {
        summary.push_str("\n주요 최근/수록 메일 제목:\n");
        for mail in emails.iter().take(30) {
            summary.push_str("- ");
            if !mail.folder_path.is_empty() {
                summary.push('[');
                summary.push_str(&mail.folder_path);
                summary.push_str("] ");
            }
            summary.push_str(&mail.subject);
            if let Some(ref s) = mail.sender {
                summary.push_str(" — ");
                summary.push_str(s);
            }
            summary.push('\n');
        }
    }

    Some(PstArchiveExtract {
        summary_text: summary,
        emails,
    })
}

impl PstReader {
    fn open(path: &Path) -> Option<Self> {
        let mut file = File::open(path).ok()?;
        let file_len = file.metadata().ok()?.len();
        if file_len < 564 {
            return None;
        }
        let mut hdr = [0u8; 564];
        file.read_exact(&mut hdr).ok()?;
        if &hdr[0..4] != PST_MAGIC {
            return None;
        }

        let w_ver = u16::from_le_bytes([hdr[10], hdr[11]]);
        let (format, crypt, nbt_root, bbt_root) = match w_ver {
            14 | 15 => {
                let crypt = hdr[461];
                let nbt = u32::from_le_bytes(hdr[188..192].try_into().ok()?) as u64;
                let bbt = u32::from_le_bytes(hdr[196..200].try_into().ok()?) as u64;
                (PstFormat::Ansi32, crypt, nbt, bbt)
            }
            21..=35 => {
                let crypt = hdr[513];
                let nbt = u64::from_le_bytes(hdr[224..232].try_into().ok()?);
                let bbt = u64::from_le_bytes(hdr[240..248].try_into().ok()?);
                (PstFormat::Unicode64, crypt, nbt, bbt)
            }
            36..=40 => {
                let crypt = hdr[513];
                let nbt = u64::from_le_bytes(hdr[224..232].try_into().ok()?);
                let bbt = u64::from_le_bytes(hdr[240..248].try_into().ok()?);
                (PstFormat::Unicode4k, crypt, nbt, bbt)
            }
            _ => return None,
        };

        let mut reader = Self {
            file,
            file_len,
            format,
            crypt,
            bbt: HashMap::new(),
            nbt: Vec::new(),
        };
        reader.walk_btree(bbt_root, false, 0);
        reader.walk_btree(nbt_root, true, 0);
        if reader.bbt.is_empty() {
            return None;
        }
        Some(reader)
    }

    fn page_size(&self) -> usize {
        match self.format {
            PstFormat::Unicode4k => 4096,
            PstFormat::Ansi32 | PstFormat::Unicode64 => 512,
        }
    }

    fn read_exact_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        if len == 0 || offset.saturating_add(len as u64) > self.file_len {
            return None;
        }
        self.file.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = vec![0u8; len];
        self.file.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    fn walk_btree(&mut self, offset: u64, is_nbt: bool, depth: u8) {
        if depth > 12 || offset == 0 {
            return;
        }
        let page_sz = self.page_size();
        let Some(page) = self.read_exact_at(offset, page_sz) else {
            return;
        };

        let (c_ent, cb_ent, c_level, ptype) = match self.format {
            PstFormat::Ansi32 => (
                page[0x1F0] as usize,
                page[0x1F2] as usize,
                page[0x1F3],
                page[0x1F4],
            ),
            PstFormat::Unicode64 => (
                page[0x1E8] as usize,
                page[0x1EA] as usize,
                page[0x1EB],
                page[0x1F0],
            ),
            PstFormat::Unicode4k => (
                u16::from_le_bytes([page[0xFD8], page[0xFD9]]) as usize,
                page[0xFDC] as usize,
                page[0xFDD],
                page[0xFE8],
            ),
        };

        let expected_ptype = if is_nbt { 0x81 } else { 0x80 };
        if ptype != expected_ptype || cb_ent == 0 {
            return;
        }

        if c_level > 0 {
            let step = match self.format {
                PstFormat::Ansi32 => 12,
                PstFormat::Unicode64 | PstFormat::Unicode4k => 24,
            };
            let mut child_offsets = Vec::with_capacity(c_ent.min(256));
            for i in 0..c_ent {
                let pos = i * step;
                if pos + step > page_sz {
                    break;
                }
                let child_ib = match self.format {
                    PstFormat::Ansi32 => {
                        u32::from_le_bytes(page[pos + 8..pos + 12].try_into().unwrap()) as u64
                    }
                    PstFormat::Unicode64 | PstFormat::Unicode4k => {
                        u64::from_le_bytes(page[pos + 16..pos + 24].try_into().unwrap())
                    }
                };
                if child_ib != 0 && child_ib != offset {
                    child_offsets.push(child_ib);
                }
            }
            for child_ib in child_offsets {
                self.walk_btree(child_ib, is_nbt, depth + 1);
            }
        } else if is_nbt {
            let step = match self.format {
                PstFormat::Ansi32 => 16,
                PstFormat::Unicode64 | PstFormat::Unicode4k => 32,
            };
            for i in 0..c_ent {
                let pos = i * step;
                if pos + step > page_sz {
                    break;
                }
                let entry = match self.format {
                    PstFormat::Ansi32 => NbtEntry {
                        nid: u32::from_le_bytes(page[pos..pos + 4].try_into().unwrap()) as u64,
                        bid_data: u32::from_le_bytes(page[pos + 4..pos + 8].try_into().unwrap())
                            as u64,
                        bid_sub: u32::from_le_bytes(page[pos + 8..pos + 12].try_into().unwrap())
                            as u64,
                        nid_parent: u32::from_le_bytes(
                            page[pos + 12..pos + 16].try_into().unwrap(),
                        ),
                    },
                    PstFormat::Unicode64 | PstFormat::Unicode4k => NbtEntry {
                        nid: u64::from_le_bytes(page[pos..pos + 8].try_into().unwrap()),
                        bid_data: u64::from_le_bytes(page[pos + 8..pos + 16].try_into().unwrap()),
                        bid_sub: u64::from_le_bytes(page[pos + 16..pos + 24].try_into().unwrap()),
                        nid_parent: u32::from_le_bytes(
                            page[pos + 24..pos + 28].try_into().unwrap(),
                        ),
                    },
                };
                if entry.nid != 0 {
                    self.nbt.push(entry);
                }
            }
        } else {
            let step = match self.format {
                PstFormat::Ansi32 => 12,
                PstFormat::Unicode64 | PstFormat::Unicode4k => 24,
            };
            for i in 0..c_ent {
                let pos = i * step;
                if pos + step > page_sz {
                    break;
                }
                match self.format {
                    PstFormat::Ansi32 => {
                        let bid =
                            (u32::from_le_bytes(page[pos..pos + 4].try_into().unwrap()) as u64)
                                & !1;
                        let ib =
                            u32::from_le_bytes(page[pos + 4..pos + 8].try_into().unwrap()) as u64;
                        let cb = u16::from_le_bytes([page[pos + 8], page[pos + 9]]);
                        self.bbt.insert(
                            bid,
                            BbtEntry {
                                offset: ib,
                                stored_size: cb,
                                inflated_size: cb,
                            },
                        );
                    }
                    PstFormat::Unicode64 => {
                        let bid =
                            u64::from_le_bytes(page[pos..pos + 8].try_into().unwrap()) & !1;
                        let ib = u64::from_le_bytes(page[pos + 8..pos + 16].try_into().unwrap());
                        let cb = u16::from_le_bytes([page[pos + 16], page[pos + 17]]);
                        self.bbt.insert(
                            bid,
                            BbtEntry {
                                offset: ib,
                                stored_size: cb,
                                inflated_size: cb,
                            },
                        );
                    }
                    PstFormat::Unicode4k => {
                        let bid =
                            u64::from_le_bytes(page[pos..pos + 8].try_into().unwrap()) & !1;
                        let ib = u64::from_le_bytes(page[pos + 8..pos + 16].try_into().unwrap());
                        let cb_stored = u16::from_le_bytes([page[pos + 16], page[pos + 17]]);
                        let cb_inflated = u16::from_le_bytes([page[pos + 18], page[pos + 19]]);
                        self.bbt.insert(
                            bid,
                            BbtEntry {
                                offset: ib,
                                stored_size: cb_stored,
                                inflated_size: cb_inflated,
                            },
                        );
                    }
                }
            }
        }
    }

    fn read_single_block(&mut self, raw_bid: u64) -> Option<Vec<u8>> {
        let bid = raw_bid & !1;
        let bbt = *self.bbt.get(&bid)?;
        let mut buf = self.read_exact_at(bbt.offset, bbt.stored_size as usize)?;

        // Decompress 4K-format zlib blocks before or after decryption as applicable
        if self.format == PstFormat::Unicode4k
            && bbt.inflated_size > bbt.stored_size
            && buf.len() >= 2
            && buf[0] == 0x78
        {
            let mut decoded = Vec::with_capacity(bbt.inflated_size as usize);
            if ZlibDecoder::new(&buf[..])
                .take(u64::from(bbt.inflated_size) + 1)
                .read_to_end(&mut decoded)
                .is_ok()
                && decoded.len() == bbt.inflated_size as usize
            {
                buf = decoded;
            }
        }

        let is_internal = (bid & 2) != 0;
        if !is_internal {
            decrypt_pst_block(bid, &mut buf, self.crypt);
        }
        Some(buf)
    }

    fn collect_data_blocks(&mut self, bid: u64, out: &mut Vec<Vec<u8>>, total_bytes: &mut usize, depth: u8) {
        if bid == 0 || depth > 5 || *total_bytes >= MAX_BLOCK_STREAM_BYTES {
            return;
        }
        let norm_bid = bid & !1;
        let Some(block) = self.read_single_block(norm_bid) else {
            return;
        };
        let is_internal = (norm_bid & 2) != 0;
        if is_internal && block.len() >= 8 && block[0] == 0x01 && (block[1] == 0x01 || block[1] == 0x02) {
            let c_ent = u16::from_le_bytes([block[2], block[3]]) as usize;
            let step = if self.format == PstFormat::Ansi32 { 4 } else { 8 };
            let mut children = Vec::with_capacity(c_ent.min(2048));
            for i in 0..c_ent {
                let pos = 8 + i * step;
                if pos + step > block.len() {
                    break;
                }
                let child_bid = if step == 4 {
                    u32::from_le_bytes(block[pos..pos + 4].try_into().unwrap()) as u64
                } else {
                    u64::from_le_bytes(block[pos..pos + 8].try_into().unwrap())
                };
                children.push(child_bid);
            }
            for child in children {
                self.collect_data_blocks(child, out, total_bytes, depth + 1);
                if *total_bytes >= MAX_BLOCK_STREAM_BYTES {
                    break;
                }
            }
        } else {
            *total_bytes += block.len();
            out.push(block);
        }
    }

    fn read_blocks(&mut self, bid: u64) -> Vec<Vec<u8>> {
        let mut blocks = Vec::new();
        let mut total = 0usize;
        self.collect_data_blocks(bid, &mut blocks, &mut total, 0);
        blocks
    }

    // Attachment streams must be complete. The heap reader above can stop after a
    // prefix; passing a truncated binary document to an extractor would be wrong.
    fn append_attachment_data(&mut self, bid: u64, out: &mut Vec<u8>, limit: usize, depth: u8) -> Option<()> {
        if depth > 2 {
            return None;
        }
        let block = self.read_single_block(bid)?;
        if bid & 2 == 0 {
            if block.len() > limit.checked_sub(out.len())? {
                return None;
            }
            out.extend_from_slice(&block);
            return Some(());
        }
        if !matches!(block.get(..2), Some([1, 1 | 2])) {
            return None;
        }
        let count = u16::from_le_bytes(block.get(2..4)?.try_into().ok()?) as usize;
        let size = u32::from_le_bytes(block.get(4..8)?.try_into().ok()?) as usize;
        let end = out.len().checked_add(size)?;
        if end > limit || count == 0 {
            return None;
        }
        let bid_width = if self.format == PstFormat::Ansi32 { 4 } else { 8 };
        let children = block.get(8..8 + count * bid_width)?;
        if depth == 0 {
            out.reserve_exact(size);
        }
        for child in children.chunks_exact(bid_width) {
            let mut raw = [0; 8];
            raw[..bid_width].copy_from_slice(child);
            self.append_attachment_data(u64::from_le_bytes(raw), out, end, depth + 1)?;
        }
        (out.len() == end).then_some(())
    }

    fn read_subnodes(&mut self, bid_sub: u64, out: &mut HashMap<u32, (u64, u64)>, depth: u8) {
        if bid_sub == 0 || depth > 5 {
            return;
        }
        let Some(block) = self.read_single_block(bid_sub) else {
            return;
        };
        if block.len() < 8 || block[0] != 0x02 {
            return;
        }
        let c_level = block[1];
        let c_ent = u16::from_le_bytes([block[2], block[3]]) as usize;
        let base = if self.format == PstFormat::Ansi32 { 4 } else { 8 };

        if c_level == 0 {
            // SLBLOCK
            let step = if self.format == PstFormat::Ansi32 { 12 } else { 24 };
            for i in 0..c_ent {
                let pos = base + i * step;
                if pos + step > block.len() {
                    break;
                }
                if self.format == PstFormat::Ansi32 {
                    let sub_nid = u32::from_le_bytes(block[pos..pos + 4].try_into().unwrap());
                    let b_data =
                        u32::from_le_bytes(block[pos + 4..pos + 8].try_into().unwrap()) as u64;
                    let b_sub =
                        u32::from_le_bytes(block[pos + 8..pos + 12].try_into().unwrap()) as u64;
                    out.insert(sub_nid, (b_data, b_sub));
                } else {
                    let sub_nid = u64::from_le_bytes(block[pos..pos + 8].try_into().unwrap()) as u32;
                    let b_data = u64::from_le_bytes(block[pos + 8..pos + 16].try_into().unwrap());
                    let b_sub = u64::from_le_bytes(block[pos + 16..pos + 24].try_into().unwrap());
                    out.insert(sub_nid, (b_data, b_sub));
                }
            }
        } else if c_level == 1 {
            // SIBLOCK
            let step = if self.format == PstFormat::Ansi32 { 8 } else { 16 };
            let mut sl_bids = Vec::with_capacity(c_ent.min(512));
            for i in 0..c_ent {
                let pos = base + i * step;
                if pos + step > block.len() {
                    break;
                }
                let child_bid = if self.format == PstFormat::Ansi32 {
                    u32::from_le_bytes(block[pos + 4..pos + 8].try_into().unwrap()) as u64
                } else {
                    u64::from_le_bytes(block[pos + 8..pos + 16].try_into().unwrap())
                };
                sl_bids.push(child_bid);
            }
            for child in sl_bids {
                self.read_subnodes(child, out, depth + 1);
            }
        }
    }

    fn read_attachments(&mut self, bid_sub: u64) -> (Vec<String>, String) {
        let mut subnodes = HashMap::new();
        self.read_subnodes(bid_sub, &mut subnodes, 0);
        let mut attachments: Vec<_> = subnodes.into_iter()
            .filter(|(nid, (data, _))| nid & 0x1f == 0x05 && *data != 0)
            .collect();
        attachments.sort_unstable_by_key(|(nid, _)| *nid);
        let mut names = Vec::<String>::new();
        let mut text = String::new();
        for (_, (data, sub)) in attachments {
            let Some(pc) = self.read_property_context(data, sub) else { continue };
            let Some(name) = pc.get_string(0x3707).or_else(|| pc.get_string(0x3704)) else { continue };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if !names.iter().any(|old| old.eq_ignore_ascii_case(name)) {
                names.push(name.to_owned());
            }
            let remaining = MAX_MESSAGE_ATTACHMENT_TEXT_BYTES - text.len();
            if remaining <= name.len() + 32 {
                continue;
            }
            let mut section = String::new();
            use std::fmt::Write as _;
            let _ = writeln!(section, "\n\n[첨부파일: {name}]");
            let mut field = |label: &str, value: &str| {
                let _ = write!(section, "{label}: ");
                section.extend(value.trim().chars().take(256).map(|ch| if ch.is_control() { ' ' } else { ch }));
                section.push('\n');
            };
            if let Some(mime) = pc.get_string(0x370E) {
                field("첨부파일 형식", &mime);
            }
            if let Some(PropVal::Int(size)) = pc.items.get(&0x0E20)
                && *size >= 0
            {
                field("첨부 항목 크기(바이트)", &size.to_string());
            }
            if matches!(pc.items.get(&ATTACH_METHOD), None | Some(PropVal::Int(1)))
                && let Some(ext) = attachment_extension(name)
            {
                let bytes = match pc.items.get(&ATTACH_DATA_BIN) {
                    Some(PropVal::Bytes { bytes, .. }) => Some(std::borrow::Cow::Borrowed(bytes.as_slice())),
                    Some(PropVal::AttachmentBlock(bid)) => {
                        let mut bytes = Vec::new();
                        self.append_attachment_data(*bid, &mut bytes, MAX_ATTACHMENT_BYTES, 0)
                            .map(|()| std::borrow::Cow::Owned(bytes))
                    }
                    _ => None,
                };
                if let Some(bytes) = bytes {
                    field("첨부 파일 크기(바이트)", &bytes.len().to_string());
                    if let Some(doc) = extract_attachment(&ext, &bytes, remaining.min(MAX_ATTACHMENT_TEXT_BYTES)) {
                        if let Some(title) = doc.props.title {
                            field("첨부 문서 제목", &title);
                        }
                        if let Some(author) = doc.props.author {
                            field("첨부 문서 작성자", &author);
                        }
                        for (label, timestamp) in [
                            ("첨부 문서 생성 시각", doc.props.created),
                            ("첨부 문서 수정 시각", doc.props.modified),
                        ] {
                            if let Some(mut utc) = timestamp.and_then(format_utc_ymd_hm) {
                                utc.push_str(" UTC");
                                field(label, &utc);
                            }
                        }
                        if !doc.text.trim().is_empty() {
                            section.push_str("첨부 문서 본문:\n");
                            section.push_str(&doc.text);
                        }
                    }
                }
            }
            let mut end = section.len().min(remaining);
            while !section.is_char_boundary(end) {
                end -= 1;
            }
            text.push_str(&section[..end]);
        }
        (names, text)
    }

    fn read_property_context(&mut self, bid_data: u64, bid_sub: u64) -> Option<PropMap> {
        let blocks = self.read_blocks(bid_data);
        let first = blocks.first()?;
        if first.len() < 12 || first[2] != 0xEC || first[3] != 0xBC {
            return None;
        }
        let hid_user_root = u32::from_le_bytes(first[4..8].try_into().ok()?);
        let mut subnodes = HashMap::new();
        if bid_sub != 0 {
            self.read_subnodes(bid_sub, &mut subnodes, 0);
        }

        let bth_hdr = self.resolve_hnid(&blocks, &subnodes, hid_user_root)?;
        if bth_hdr.len() < 8 || bth_hdr[0] != 0xB5 {
            return None;
        }
        let cb_key = bth_hdr[1] as usize;
        let cb_ent = bth_hdr[2] as usize;
        let idx_levels = bth_hdr[3];
        let hid_root = u32::from_le_bytes(bth_hdr[4..8].try_into().ok()?);
        if cb_key != 2 || cb_ent != 6 || hid_root == 0 {
            return None;
        }

        let mut raw_records = Vec::new();
        self.collect_bth_leaves(&blocks, &subnodes, hid_root, idx_levels, cb_key, cb_ent, &mut raw_records);

        let mut map = PropMap {
            items: HashMap::with_capacity(raw_records.len()),
        };
        for rec in raw_records {
            if rec.len() < 8 {
                continue;
            }
            let prop_id = u16::from_le_bytes([rec[0], rec[1]]);
            let prop_type = u16::from_le_bytes([rec[2], rec[3]]);
            let val_hnid = u32::from_le_bytes(rec[4..8].try_into().unwrap());

            let prop_val = match prop_type {
                0x0002 => PropVal::Int((val_hnid & 0xFFFF) as i64),
                0x0003 | 0x000A => PropVal::Int(val_hnid as i32 as i64),
                0x000B => PropVal::Bool((val_hnid & 0xFF) != 0),
                // Avoid loading external payloads until the attachment name and method are known.
                0x0102 if prop_id == ATTACH_DATA_BIN && val_hnid & 0x1f != 0 => {
                    let Some(&(bid, _)) = subnodes.get(&val_hnid) else { continue };
                    PropVal::AttachmentBlock(bid)
                }
                0x001E | 0x001F | 0x0040 | 0x0102 => {
                    if let Some(bytes) = self.resolve_hnid(&blocks, &subnodes, val_hnid) {
                        PropVal::Bytes { prop_type, bytes }
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            map.items.insert(prop_id, prop_val);
        }

        Some(map)
    }

    fn collect_bth_leaves(
        &mut self,
        blocks: &[Vec<u8>],
        subnodes: &HashMap<u32, (u64, u64)>,
        hid: u32,
        level: u8,
        cb_key: usize,
        cb_ent: usize,
        out: &mut Vec<Vec<u8>>,
    ) {
        if hid == 0 || level > 6 {
            return;
        }
        let Some(node_bytes) = self.resolve_hnid(blocks, subnodes, hid) else {
            return;
        };
        if level == 0 {
            let step = cb_key + cb_ent;
            for chunk in node_bytes.chunks_exact(step) {
                out.push(chunk.to_vec());
            }
        } else {
            let step = cb_key + 4;
            for chunk in node_bytes.chunks_exact(step) {
                let next_hid =
                    u32::from_le_bytes(chunk[cb_key..cb_key + 4].try_into().unwrap());
                self.collect_bth_leaves(blocks, subnodes, next_hid, level - 1, cb_key, cb_ent, out);
            }
        }
    }

    fn resolve_hnid(
        &mut self,
        blocks: &[Vec<u8>],
        subnodes: &HashMap<u32, (u64, u64)>,
        hnid: u32,
    ) -> Option<Vec<u8>> {
        if hnid == 0 {
            return Some(Vec::new());
        }
        if let Some(&(sub_data_bid, _)) = subnodes.get(&hnid) {
            let parts = self.read_blocks(sub_data_bid);
            let total_len: usize = parts.iter().map(Vec::len).sum();
            let mut merged = Vec::with_capacity(total_len);
            for p in parts {
                merged.extend_from_slice(&p);
            }
            return Some(merged);
        }
        if (hnid & 0x1F) != 0 {
            return None;
        }

        let block_idx = (hnid >> 16) as usize;
        let item_idx = ((hnid & 0xFFFF) >> 5) as usize;
        if item_idx == 0 {
            return None;
        }
        let block = blocks.get(block_idx)?;
        if block.len() < 4 {
            return None;
        }
        let ib_hnpm = u16::from_le_bytes([block[0], block[1]]) as usize;
        if ib_hnpm + 4 > block.len() {
            return None;
        }
        let c_alloc = u16::from_le_bytes([block[ib_hnpm], block[ib_hnpm + 1]]) as usize;
        if item_idx > c_alloc {
            return None;
        }
        let off_pos = ib_hnpm + 4 + (item_idx - 1) * 2;
        if off_pos + 4 > block.len() {
            return None;
        }
        let start = u16::from_le_bytes([block[off_pos], block[off_pos + 1]]) as usize;
        let end = u16::from_le_bytes([block[off_pos + 2], block[off_pos + 3]]) as usize;
        if start > end || end > block.len() {
            return None;
        }
        Some(block[start..end].to_vec())
    }
}

enum PropVal {
    Int(i64),
    #[allow(dead_code)]
    Bool(bool),
    Bytes { prop_type: u16, bytes: Vec<u8> },
    AttachmentBlock(u64),
}

struct PropMap {
    items: HashMap<u16, PropVal>,
}

impl PropMap {
    fn codepage(&self) -> u32 {
        for tag in [0x3FFD, 0x3FDE, 0x66C3] {
            if let Some(PropVal::Int(cp)) = self.items.get(&tag)
                && *cp > 0
            {
                return *cp as u32;
            }
        }
        0
    }

    fn get_string(&self, tag: u16) -> Option<String> {
        let PropVal::Bytes { prop_type, bytes } = self.items.get(&tag)? else {
            return None;
        };
        decode_mapi_string(*prop_type, bytes, self.codepage())
    }

    fn get_bytes(&self, tag: u16) -> Option<&[u8]> {
        match self.items.get(&tag)? {
            PropVal::Bytes { bytes, .. } => Some(bytes.as_slice()),
            _ => None,
        }
    }

    fn get_filetime_ms(&self, tag: u16) -> Option<i64> {
        let bytes = self.get_bytes(tag)?;
        if bytes.len() < 8 {
            return None;
        }
        let ft = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let ticks_since_unix = ft.checked_sub(UNIX_EPOCH_FT_TICKS)?;
        Some((ticks_since_unix / 10_000) as i64)
    }

    fn get_subject(&self) -> Option<String> {
        if let Some(PropVal::Bytes { prop_type, bytes }) = self.items.get(&0x0037) {
            let slice = if *prop_type == 0x001F && bytes.len() >= 4 && bytes[0] == 0x01 && bytes[1] == 0x00 {
                &bytes[4..]
            } else if *prop_type == 0x001E && bytes.len() >= 2 && bytes[0] == 0x01 {
                &bytes[2..]
            } else {
                bytes.as_slice()
            };
            if let Some(s) = decode_mapi_string(*prop_type, slice, self.codepage()) {
                let t = s.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        for fallback_tag in [0x0E1D, 0x0070] {
            if let Some(s) = self.get_string(fallback_tag) {
                let t = s.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    }

    fn get_sender(&self) -> Option<String> {
        let name = self
            .get_string(0x0C1A)
            .or_else(|| self.get_string(0x0042))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let addr = self
            .get_string(0x5D01)
            .or_else(|| self.get_string(0x0C1F))
            .or_else(|| self.get_string(0x0065))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s.contains('@'));

        match (name, addr) {
            (Some(n), Some(a)) if !n.eq_ignore_ascii_case(&a) => Some(format!("{n} <{a}>")),
            (Some(n), _) => Some(n),
            (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }

    fn get_best_body(&self) -> Option<String> {
        let cp = self.codepage();
        if let Some(plain) = self.get_string(0x1000) {
            let trimmed = plain.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(PropVal::Bytes { prop_type, bytes }) = self.items.get(&0x1013) {
            let html_str = if *prop_type == 0x001F {
                decode_utf16le(bytes)
            } else {
                decode_bytes_with_codepage(bytes, cp)
            };
            let cleaned = clean_html_fragment(&html_str);
            if !cleaned.is_empty() {
                return Some(cleaned);
            }
        }
        if let Some(rtf_raw) = self.get_bytes(0x1009)
            && let Some(rtf_bytes) = decompress_lzfu(rtf_raw)
        {
            let plain = strip_rtf_to_text(&rtf_bytes, cp);
            if !plain.is_empty() {
                return Some(plain);
            }
        }
        None
    }
}

fn decrypt_pst_block(bid: u64, buf: &mut [u8], crypt: u8) {
    match crypt {
        1 => {
            for b in buf {
                *b = COMP_ENC[*b as usize];
            }
        }
        2 => {
            // `[MS-PST] 5.2` Cyclic algorithm
            let mut key = (bid as u32) ^ ((bid >> 32) as u32);
            let mut pos = (key ^ (key >> 16)) as u16;
            for b in buf {
                let mut c = *b;
                c = c.wrapping_add((pos & 0xFF) as u8);
                c = COMP_ENC[c as usize];
                c = c.wrapping_add(((pos >> 8) & 0xFF) as u8);
                c = COMP_HIGH1[c as usize];
                c = c.wrapping_sub(((pos >> 8) & 0xFF) as u8);
                c = COMP_HIGH2[c as usize];
                c = c.wrapping_sub((pos & 0xFF) as u8);
                *b = c;
                pos = pos.wrapping_add(1);
                let _ = &mut key;
            }
        }
        _ => {}
    }
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn decode_mapi_string(prop_type: u16, bytes: &[u8], codepage: u32) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let s = if prop_type == 0x001F {
        decode_utf16le(bytes)
    } else {
        let slice = bytes
            .iter()
            .position(|&b| b == 0)
            .map_or(bytes, |nul| &bytes[..nul]);
        decode_bytes_with_codepage(slice, codepage)
    };
    let trimmed = s.trim_matches('\0').trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Decompresses Outlook MAPI `PidTagRtfCompressed` (`LZFu`, `[MS-OXRTFCP]`).
pub(crate) fn decompress_lzfu(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 16 {
        return None;
    }
    let raw_size = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
    let magic = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let capped_size = raw_size.min(MAX_BLOCK_STREAM_BYTES);

    if magic == 0x414C_454D {
        // "MELA" uncompressed RTF
        let end = (16 + capped_size).min(data.len());
        return Some(data[16..end].to_vec());
    }
    if magic != 0x7546_5A4C {
        // Not "LZFu"
        return None;
    }

    let mut dict = [0u8; 4096];
    let pre_len = LZFU_PREBUF.len();
    dict[..pre_len].copy_from_slice(LZFU_PREBUF);
    let mut write_cursor = pre_len;
    let mut out = Vec::with_capacity(capped_size);
    let mut pos = 16usize;

    while pos < data.len() && out.len() < capped_size {
        let flags = data[pos];
        pos += 1;
        for bit in 0..8 {
            if out.len() >= capped_size || pos >= data.len() {
                break;
            }
            if ((flags >> bit) & 1) == 1 {
                if pos + 1 >= data.len() {
                    return Some(out);
                }
                let b1 = data[pos] as usize;
                let b2 = data[pos + 1] as usize;
                pos += 2;
                let mut ref_off = ((b1 << 4) | (b2 >> 4)) & 0xFFF;
                if ref_off == write_cursor {
                    // End-of-stream marker in LZFu
                    return Some(out);
                }
                let ref_len = (b2 & 0x0F) + 2;
                for _ in 0..ref_len {
                    if out.len() >= capped_size {
                        break;
                    }
                    let byte = dict[ref_off];
                    out.push(byte);
                    dict[write_cursor] = byte;
                    write_cursor = (write_cursor + 1) & 0xFFF;
                    ref_off = (ref_off + 1) & 0xFFF;
                }
            } else {
                let byte = data[pos];
                pos += 1;
                out.push(byte);
                dict[write_cursor] = byte;
                write_cursor = (write_cursor + 1) & 0xFFF;
            }
        }
    }

    Some(out)
}

/// Extracts readable Unicode/codepage text from RTF bytes (supports `\ansicpgN`, `\'xx`, `\uN`, `\par`, `\line`, `\tab`, and HTML-in-RTF `\fromhtml1`).
pub(crate) fn strip_rtf_to_text(rtf: &[u8], fallback_cp: u32) -> String {
    let mut cp = if fallback_cp != 0 { fallback_cp } else { 949 };
    // Detect \ansicpgNNN in header
    if let Some(pos) = rtf
        .windows(8)
        .take(512)
        .position(|w| w == b"\\ansicpg")
    {
        let mut idx = pos + 8;
        let mut val = 0u32;
        let mut digits = 0;
        while idx < rtf.len() && rtf[idx].is_ascii_digit() && digits < 6 {
            val = val * 10 + (rtf[idx] - b'0') as u32;
            idx += 1;
            digits += 1;
        }
        if val > 0 {
            cp = val;
        }
    }

    let mut out = String::with_capacity(rtf.len() / 2);
    let mut byte_run: Vec<u8> = Vec::new();
    let flush_bytes = |run: &mut Vec<u8>, out: &mut String, cp: u32| {
        if !run.is_empty() {
            out.push_str(&decode_bytes_with_codepage(run, cp));
            run.clear();
        }
    };

    let mut uc_skip: usize = 1;
    let mut group_stack: Vec<(usize, bool)> = Vec::new();
    let mut ignore_group = false;
    let mut i = 0usize;

    while i < rtf.len() {
        match rtf[i] {
            b'{' => {
                flush_bytes(&mut byte_run, &mut out, cp);
                group_stack.push((uc_skip, ignore_group));
                i += 1;
                if i + 1 < rtf.len() && rtf[i] == b'\\' && rtf[i + 1] == b'*' {
                    // Check if it's `{\*\htmltag...}` (keep content? No, in `\fromhtml1`, real text sits outside `\*\htmltag`!)
                    ignore_group = true;
                    i += 2;
                }
            }
            b'}' => {
                flush_bytes(&mut byte_run, &mut out, cp);
                if let Some((prev_uc, prev_ign)) = group_stack.pop() {
                    uc_skip = prev_uc;
                    ignore_group = prev_ign;
                }
                i += 1;
            }
            b'\r' | b'\n' => {
                i += 1;
            }
            b'\\' => {
                i += 1;
                if i >= rtf.len() {
                    break;
                }
                match rtf[i] {
                    b'\\' | b'{' | b'}' => {
                        if !ignore_group {
                            byte_run.push(rtf[i]);
                        }
                        i += 1;
                    }
                    b'\'' => {
                        if i + 2 < rtf.len() {
                            if let Ok(hex_str) = std::str::from_utf8(&rtf[i + 1..i + 3])
                                && let Ok(b) = u8::from_str_radix(hex_str, 16)
                                && !ignore_group
                            {
                                byte_run.push(b);
                            }
                            i += 3;
                        } else {
                            i += 1;
                        }
                    }
                    b'~' => {
                        flush_bytes(&mut byte_run, &mut out, cp);
                        if !ignore_group {
                            out.push(' ');
                        }
                        i += 1;
                    }
                    c if c.is_ascii_alphabetic() => {
                        let start = i;
                        while i < rtf.len() && rtf[i].is_ascii_alphabetic() {
                            i += 1;
                        }
                        let word = &rtf[start..i];
                        let neg = if i < rtf.len() && rtf[i] == b'-' {
                            i += 1;
                            true
                        } else {
                            false
                        };
                        let num_start = i;
                        while i < rtf.len() && rtf[i].is_ascii_digit() {
                            i += 1;
                        }
                        let param: Option<i32> = if i > num_start {
                            std::str::from_utf8(&rtf[num_start..i])
                                .ok()
                                .and_then(|s| s.parse::<i32>().ok())
                                .map(|v| if neg { -v } else { v })
                        } else {
                            None
                        };
                        if i < rtf.len() && rtf[i] == b' ' {
                            i += 1;
                        }

                        match word {
                            b"fonttbl" | b"colortbl" | b"stylesheet" | b"info" | b"pict"
                            | b"object" | b" header" | b"fldinst" => {
                                ignore_group = true;
                            }
                            b"uc" => {
                                if let Some(n) = param.filter(|&n| (0..=8).contains(&n)) {
                                    uc_skip = n as usize;
                                }
                            }
                            b"u" => {
                                flush_bytes(&mut byte_run, &mut out, cp);
                                if !ignore_group && let Some(n) = param {
                                    let code = if n < 0 { (n + 65536) as u32 } else { n as u32 };
                                    if let Some(ch) = char::from_u32(code) {
                                        out.push(ch);
                                    }
                                }
                                // Skip `uc_skip` ANSI fallback tokens (`\'xx` or single byte)
                                let mut skipped = 0;
                                while skipped < uc_skip && i < rtf.len() {
                                    if rtf[i] == b'\\' && i + 3 < rtf.len() && rtf[i + 1] == b'\'' {
                                        i += 4;
                                    } else if rtf[i] != b'{' && rtf[i] != b'}' {
                                        i += 1;
                                    } else {
                                        break;
                                    }
                                    skipped += 1;
                                }
                            }
                            b"par" | b"line" | b"row" => {
                                flush_bytes(&mut byte_run, &mut out, cp);
                                if !ignore_group && !out.ends_with("\n\n") {
                                    out.push('\n');
                                }
                            }
                            b"tab" | b"cell" => {
                                flush_bytes(&mut byte_run, &mut out, cp);
                                if !ignore_group {
                                    out.push('\t');
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            ch => {
                if !ignore_group {
                    byte_run.push(ch);
                }
                i += 1;
            }
        }
        if out.len() >= MAX_MESSAGE_BODY_CHARS * 3 {
            break;
        }
    }
    flush_bytes(&mut byte_run, &mut out, cp);
    out.trim().to_string()
}

/// Standalone Outlook `.msg` (`[MS-OXMSG]`, OLE Compound File Binary) extractor.
pub(crate) fn extract_msg(path: &Path) -> Option<ExtractedDoc> {
    let mut comp = cfb::open(path).ok()?;
    let mut streams: Vec<String> = comp
        .walk()
        .filter(|e| e.is_stream())
        .map(|e| e.path().to_string_lossy().into_owned())
        .collect();
    streams.sort();

    let read_str_prop = |comp: &mut cfb::CompoundFile<File>, prop_hex: &str| -> Option<String> {
        let uni_name = format!("/__substg1.0_{prop_hex}001F");
        if let Ok(mut s) = comp.open_stream(&uni_name) {
            let mut buf = Vec::new();
            if s.read_to_end(&mut buf).is_ok() {
                let decoded = decode_utf16le(&buf);
                let t = decoded.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        let ansi_name = format!("/__substg1.0_{prop_hex}001E");
        if let Ok(mut s) = comp.open_stream(&ansi_name) {
            let mut buf = Vec::new();
            if s.read_to_end(&mut buf).is_ok() {
                let decoded = decode_bytes_with_codepage(&buf, 0);
                let t = decoded.trim_matches('\0').trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    };

    let read_bin_prop = |comp: &mut cfb::CompoundFile<File>, prop_hex: &str| -> Option<Vec<u8>> {
        let bin_name = format!("/__substg1.0_{prop_hex}0102");
        let mut s = comp.open_stream(&bin_name).ok()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).ok()?;
        Some(buf)
    };

    let subject = read_str_prop(&mut comp, "0037")
        .or_else(|| read_str_prop(&mut comp, "0E1D"))
        .or_else(|| read_str_prop(&mut comp, "0070"));
    let sender_name = read_str_prop(&mut comp, "0C1A").or_else(|| read_str_prop(&mut comp, "0042"));
    let sender_email = read_str_prop(&mut comp, "5D01")
        .or_else(|| read_str_prop(&mut comp, "0C1F"))
        .or_else(|| read_str_prop(&mut comp, "0065"));
    let sender = match (sender_name, sender_email) {
        (Some(n), Some(a)) if a.contains('@') && !n.eq_ignore_ascii_case(&a) => {
            Some(format!("{n} <{a}>"))
        }
        (Some(n), _) => Some(n),
        (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    let to_line = read_str_prop(&mut comp, "0E04");
    let cc_line = read_str_prop(&mut comp, "0E03");

    // Attachment names inside `/__attach_version1.0_#XXXXXXXX/__substg1.0_3707001F`
    let mut attachments = Vec::new();
    for spath in &streams {
        if spath.starts_with("/__attach_version1.0_")
            && (spath.ends_with("3707001F") || spath.ends_with("3704001F"))
            && let Ok(mut s) = comp.open_stream(spath)
        {
            let mut buf = Vec::new();
            if s.read_to_end(&mut buf).is_ok() {
                let name = decode_utf16le(&buf).trim().to_string();
                if !name.is_empty() && !attachments.contains(&name) {
                    attachments.push(name);
                }
            }
        }
    }

    let body = read_str_prop(&mut comp, "1000")
        .or_else(|| {
            read_bin_prop(&mut comp, "1013")
                .map(|b| clean_html_fragment(&decode_bytes_with_codepage(&b, 0)))
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            read_str_prop(&mut comp, "1013")
                .map(|h| clean_html_fragment(&h))
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            read_bin_prop(&mut comp, "1009")
                .and_then(|b| decompress_lzfu(&b))
                .map(|rtf| strip_rtf_to_text(&rtf, 0))
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();

    let mut out = String::new();
    if let Some(ref subj) = subject {
        out.push_str(&format!("제목: {subj}\n"));
    }
    if let Some(ref snd) = sender {
        out.push_str(&format!("보낸사람: {snd}\n"));
    }
    if let Some(ref to) = to_line {
        out.push_str(&format!("받는사람: {to}\n"));
    }
    if let Some(ref cc) = cc_line {
        out.push_str(&format!("참조: {cc}\n"));
    }
    if !attachments.is_empty() {
        out.push_str(&format!("첨부파일: {}\n", attachments.join(", ")));
    }
    if !body.is_empty() {
        out.push('\n');
        for ch in body.chars().take(MAX_MESSAGE_BODY_CHARS) {
            out.push(ch);
        }
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }

    Some(ExtractedDoc {
        text: trimmed.to_string(),
        props: DocProps {
            title: subject,
            author: sender,
            created: None,
            modified: None,
        },
    })
}

/// RFC 5322 `.eml` MIME email extractor (decodes RFC 2047 headers + text/plain or text/html body).
pub(crate) fn extract_eml(path: &Path) -> Option<ExtractedDoc> {
    let raw = std::fs::read(path).ok()?;
    if raw.is_empty() {
        return None;
    }
    let full = decode_bytes_with_codepage(&raw, 0);
    let (hdr_part, body_part) = full
        .split_once("\r\n\r\n")
        .or_else(|| full.split_once("\n\n"))
        .unwrap_or((full.as_str(), ""));

    let mut unfolded_headers: Vec<String> = Vec::new();
    for line in hdr_part.lines() {
        if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some(last) = unfolded_headers.last_mut()
        {
            last.push(' ');
            last.push_str(line.trim());
        } else {
            unfolded_headers.push(line.trim_end().to_string());
        }
    }

    let mut subject = None;
    let mut from = None;
    let mut to = None;
    let mut date = None;
    for h in &unfolded_headers {
        if let Some((k, v)) = h.split_once(':') {
            let val = decode_rfc2047(v.trim());
            match k.trim().to_ascii_lowercase().as_str() {
                "subject" => subject = Some(val),
                "from" => from = Some(val),
                "to" => to = Some(val),
                "date" => date = Some(val),
                _ => {}
            }
        }
    }

    let cleaned_body = if body_part.contains("<html") || body_part.contains("<HTML") || body_part.contains("<div") {
        clean_html_fragment(body_part)
    } else {
        body_part.chars().take(MAX_MESSAGE_BODY_CHARS).collect()
    };

    let mut out = String::new();
    if let Some(ref s) = subject {
        out.push_str(&format!("제목: {s}\n"));
    }
    if let Some(ref f) = from {
        out.push_str(&format!("보낸사람: {f}\n"));
    }
    if let Some(ref t) = to {
        out.push_str(&format!("받는사람: {t}\n"));
    }
    if let Some(ref d) = date {
        out.push_str(&format!("일시: {d}\n"));
    }
    if !cleaned_body.trim().is_empty() {
        out.push('\n');
        out.push_str(cleaned_body.trim());
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(ExtractedDoc {
        text: trimmed.to_string(),
        props: DocProps {
            title: subject,
            author: from,
            created: None,
            modified: None,
        },
    })
}

fn decode_rfc2047(input: &str) -> String {
    if !input.contains("=?") {
        return input.to_string();
    }
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("=?") {
        let prefix = &rest[..start];
        if !prefix.trim().is_empty() || out.is_empty() {
            out.push_str(prefix);
        }
        let after = &rest[start + 2..];
        let parts: Vec<&str> = after.splitn(4, '?').collect();
        if parts.len() == 4 && let Some(end_eq) = parts[3].starts_with('=').then_some(()) {
            let _ = end_eq;
            let charset = parts[0];
            let enc = parts[1];
            let payload = parts[2];
            let decoded_bytes = if enc.eq_ignore_ascii_case("B") {
                decode_base64_simple(payload)
            } else if enc.eq_ignore_ascii_case("Q") {
                Some(decode_q_encoding(payload))
            } else {
                None
            };
            if let Some(bytes) = decoded_bytes {
                let cp = if charset.eq_ignore_ascii_case("euc-kr") || charset.eq_ignore_ascii_case("ks_c_5601-1987") || charset.eq_ignore_ascii_case("cp949") {
                    949
                } else {
                    65001
                };
                out.push_str(&decode_bytes_with_codepage(&bytes, cp));
                rest = &parts[3][1..];
                continue;
            }
        }
        out.push_str("=?");
        rest = after;
    }
    out.push_str(rest);
    out
}

fn decode_q_encoding(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'_' {
            out.push(b' ');
            i += 1;
        } else if b[i] == b'=' && i + 2 < b.len() {
            if let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
                && let Ok(val) = u8::from_str_radix(hex, 16)
            {
                out.push(val);
                i += 3;
                continue;
            }
            out.push(b'=');
            i += 1;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn decode_base64_simple(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for ch in s.bytes() {
        let val = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\r' | b'\n' | b'\t' => continue,
            _ => return None,
        };
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

fn format_utc_ymd_hm(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    let secs = (ms / 1000) as u64;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    Some(format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lzfu_decompression_and_rtf_stripping_preserves_korean_and_english() {
        // Uncompressed MELA RTF sample containing \ansicpg949 + Unicode \u escapes
        let rtf_body = b"{\\rtf1\\ansi\\ansicpg949{\\fonttbl{\\f0 Malgun Gothic;}}\\pard \\u48176?\\u53552?\\u47532? BMS \\par \\u49849?\\u51064? \\u50756?\\u47308?!}";
        let mut mela = Vec::new();
        mela.extend_from_slice(&((rtf_body.len() + 12) as u32).to_le_bytes());
        mela.extend_from_slice(&(rtf_body.len() as u32).to_le_bytes());
        mela.extend_from_slice(&0x414C_454Du32.to_le_bytes());
        mela.extend_from_slice(&0u32.to_le_bytes());
        mela.extend_from_slice(rtf_body);

        let decomp = decompress_lzfu(&mela).expect("MELA RTF should unpack");
        let text = strip_rtf_to_text(&decomp, 0);
        assert!(text.contains("배터리 BMS"), "Expected Korean + English: {text:?}");
        assert!(text.contains("승인 완료!"), "Expected second line: {text:?}");
    }

    #[test]
    fn rfc2047_subject_decodes_base64_and_q() {
        let encoded = "=?UTF-8?B?7KO86rCE7ZqM7J2Y?= Review";
        assert_eq!(decode_rfc2047(encoded), "주간회의 Review");
    }

    fn utf16le_bytes(s: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity((s.len() + 1) * 2);
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn permute_encrypt(plain: &[u8]) -> Vec<u8> {
        let mut inv = [0u8; 256];
        for (cipher, &pt) in COMP_ENC.iter().enumerate() {
            inv[pt as usize] = cipher as u8;
        }
        plain.iter().map(|&b| inv[b as usize]).collect()
    }

    /// Builds a valid Heap-on-Node (`HN`) + Property Context (`0xBC`) block for testing `[MS-PST]`.
    fn build_pc_block(props: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
        // Item 1 (hid = 0x20): BTHHEADER (8 bytes -> points to Item 2 = hid 0x40)
        // Item 2 (hid = 0x40): BTH leaf records (8 bytes * props.len())
        // Item 3..: variable-length values (hid = (idx << 5))
        let mut items: Vec<Vec<u8>> = Vec::new();
        items.push(vec![0xB5, 2, 6, 0, 0x40, 0, 0, 0]); // Item 1: BTHHEADER -> hidRoot = 0x40

        let mut bth_records = Vec::with_capacity(props.len() * 8);
        let mut value_items: Vec<Vec<u8>> = Vec::new();
        for (prop_id, prop_type, data) in props {
            bth_records.extend_from_slice(&prop_id.to_le_bytes());
            bth_records.extend_from_slice(&prop_type.to_le_bytes());
            if matches!(*prop_type, 0x0003 | 0x0102) {
                assert_eq!(data.len(), 4);
                bth_records.extend_from_slice(data);
                continue;
            }
            let next_item_idx = (3 + value_items.len()) as u32;
            let hnid = next_item_idx << 5;
            bth_records.extend_from_slice(&hnid.to_le_bytes());
            value_items.push(data.clone());
        }
        items.push(bth_records); // Item 2
        items.extend(value_items);

        let mut block = vec![0u8; 12]; // 12-byte HNHDR
        block[2] = 0xEC; // bSig
        block[3] = 0xBC; // Property Context
        block[4..8].copy_from_slice(&0x20u32.to_le_bytes()); // hidUserRoot = 0x20

        let mut offsets: Vec<u16> = Vec::with_capacity(items.len() + 1);
        for item in &items {
            offsets.push(block.len() as u16);
            block.extend_from_slice(item);
        }
        offsets.push(block.len() as u16);

        let ib_hnpm = block.len() as u16;
        block[0..2].copy_from_slice(&ib_hnpm.to_le_bytes());
        block.extend_from_slice(&(items.len() as u16).to_le_bytes()); // cAlloc
        block.extend_from_slice(&0u16.to_le_bytes()); // cFree
        for off in offsets {
            block.extend_from_slice(&off.to_le_bytes());
        }
        block
    }

    #[test]
    fn unicode_pst_archive_indexes_folders_messages_attachments_and_reconciles() {
        let dir = tempfile::tempdir().unwrap();
        let docs_dir = dir.path().join("mail");
        let index_dir = dir.path().join("index");
        std::fs::create_dir_all(&docs_dir).unwrap();
        let pst_path = docs_dir.join("team_archive.pst");

        // Folder, message, and three by-value attachments stored in split XBLOCK streams.
        let folder_pc = permute_encrypt(&build_pc_block(&[(
            0x3001,
            0x001F,
            utf16le_bytes("배터리팀"),
        )]));

        let ft_2026: u64 = UNIX_EPOCH_FT_TICKS + 1_768_000_000u64 * 10_000_000;
        let msg_pc = permute_encrypt(&build_pc_block(&[
            (0x0037, 0x001F, utf16le_bytes("BMS 회로 설계 최종 승인")),
            (0x0C1A, 0x001F, utf16le_bytes("김철수")),
            (0x5D01, 0x001F, utf16le_bytes("chulsoo.kim@company.com")),
            (0x0E06, 0x0040, ft_2026.to_le_bytes().to_vec()),
            (0x3007, 0x0040, (ft_2026 - 10_000_000).to_le_bytes().to_vec()),
            (0x3008, 0x0040, (ft_2026 + 10_000_000).to_le_bytes().to_vec()),
            (
                0x1013,
                0x001F,
                utf16le_bytes("<html><body><p>금주 금요일까지 실행 캐시 검증을 완료합니다.</p></body></html>"),
            ),
        ]));

        use std::io::{Cursor, Write as _};
        let office = |path: &str, xml: &str| {
            let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
            zip.start_file(path, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
            zip.start_file("docProps/core.xml", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(b"<coreProperties><title>officepropertyneedle</title><creator>Office Writer</creator><created>2024-02-03T04:05:00Z</created></coreProperties>").unwrap();
            zip.finish().unwrap().into_inner()
        };
        let pdf = {
            let content = "BT /F1 12 Tf 20 100 Td (pdfneedle) Tj ET";
            let objects = [
                "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_owned(),
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
                format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
                "<< /Title (pdfpropertyneedle) /Author (PDF Writer) /ModDate (D:20240203040500Z) >>".to_owned(),
            ];
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut xref = Vec::new();
            for (i, object) in objects.iter().enumerate() {
                xref.push(pdf.len());
                writeln!(pdf, "{} 0 obj\n{object}\nendobj", i + 1).unwrap();
            }
            let start = pdf.len();
            writeln!(pdf, "xref\n0 7\n0000000000 65535 f ").unwrap();
            for off in xref {
                writeln!(pdf, "{off:010} 00000 n ").unwrap();
            }
            writeln!(pdf, "trailer\n<< /Size 7 /Root 1 0 R /Info 6 0 R >>\nstartxref\n{start}\n%%EOF").unwrap();
            pdf
        };
        let docs = [
            ("셀전압_리포트.xlsx", office("xl/worksheets/sheet1.xml", "<worksheet><sheetData><row><c t=\"inlineStr\"><is><t>excelneedle</t></is></c></row></sheetData></worksheet>")),
            ("slides.pptx", office("ppt/slides/slide1.xml", "<sld><p><t>slidesneedle</t></p></sld>")),
            ("report.pdf", pdf),
        ];

        let mut pst_bytes = vec![0u8; 1024]; // Header (0..564) + padding to 1024
        pst_bytes[0..4].copy_from_slice(b"!BDN");
        pst_bytes[10..12].copy_from_slice(&23u16.to_le_bytes()); // Unicode 64-bit
        pst_bytes[513] = 1; // NDB_CRYPT_PERMUTE

        let push_blob = |buf: &mut Vec<u8>, data: &[u8]| -> (u64, u16) {
            while buf.len() % 64 != 0 {
                buf.push(0);
            }
            let off = buf.len() as u64;
            buf.extend_from_slice(data);
            (off, data.len() as u16)
        };

        let mut bbt_entries = Vec::new();
        let mut push = |bid, data: &[u8]| {
            let (offset, len) = push_blob(&mut pst_bytes, data);
            bbt_entries.push((bid, offset, len));
        };
        let mut subnodes = vec![2, 0, 3, 0, 0, 0, 0, 0];
        push(0x100, &folder_pc);
        push(0x200, &msg_pc);
        for (i, (name, bytes)) in docs.into_iter().enumerate() {
            let bid = 0x300 + i as u64 * 0x100;
            let payload_nid = 0x91u32;
            let pc = build_pc_block(&[
                (ATTACH_DATA_BIN, 0x0102, payload_nid.to_le_bytes().to_vec()),
                (ATTACH_METHOD, 0x0003, 1u32.to_le_bytes().to_vec()),
                (0x3707, 0x001f, utf16le_bytes(name)),
            ]);
            push(bid, &permute_encrypt(&pc));
            subnodes.extend_from_slice(&(0x685 + i as u64 * 32).to_le_bytes());
            subnodes.extend_from_slice(&bid.to_le_bytes());
            subnodes.extend_from_slice(&(bid + 2).to_le_bytes());
            let mut sub = vec![2, 0, 1, 0, 0, 0, 0, 0];
            sub.extend_from_slice(&u64::from(payload_nid).to_le_bytes());
            sub.extend_from_slice(&(bid + 6).to_le_bytes());
            sub.extend_from_slice(&0u64.to_le_bytes());
            push(bid + 2, &sub);
            let mut xblock = vec![1, 1, 2, 0];
            xblock.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            xblock.extend_from_slice(&(bid + 8).to_le_bytes());
            xblock.extend_from_slice(&(bid + 12).to_le_bytes());
            push(bid + 6, &xblock);
            let (first, last) = bytes.split_at(bytes.len() / 2);
            push(bid + 8, &permute_encrypt(first));
            push(bid + 12, &permute_encrypt(last));
        }
        push(0x602, &subnodes);
        bbt_entries.sort_unstable_by_key(|entry| entry.0);

        while pst_bytes.len() % 512 != 0 {
            pst_bytes.push(0);
        }
        let bbt_offset = pst_bytes.len() as u64;
        let mut bbt_page = vec![0u8; 512];
        for (i, &(bid, ib, cb)) in bbt_entries.iter().enumerate() {
            let p = i * 24;
            bbt_page[p..p + 8].copy_from_slice(&bid.to_le_bytes());
            bbt_page[p + 8..p + 16].copy_from_slice(&ib.to_le_bytes());
            bbt_page[p + 16..p + 18].copy_from_slice(&cb.to_le_bytes());
        }
        bbt_page[0x1E8] = bbt_entries.len() as u8;
        bbt_page[0x1EA] = 24;
        bbt_page[0x1EB] = 0;
        bbt_page[0x1F0] = 0x80; // ptypeBBT
        pst_bytes.extend_from_slice(&bbt_page);

        let nbt_offset = pst_bytes.len() as u64;
        let mut nbt_page = vec![0u8; 512];
        let nbt_entries = [
            (0x0122u64, 0x100u64, 0u64, 0u32),       // Folder "배터리팀"
            (0x200024u64, 0x200u64, 0x602u64, 0x122), // Message inside "배터리팀"
        ];
        for (i, &(nid, b_data, b_sub, parent)) in nbt_entries.iter().enumerate() {
            let p = i * 32;
            nbt_page[p..p + 8].copy_from_slice(&nid.to_le_bytes());
            nbt_page[p + 8..p + 16].copy_from_slice(&b_data.to_le_bytes());
            nbt_page[p + 16..p + 24].copy_from_slice(&b_sub.to_le_bytes());
            nbt_page[p + 24..p + 28].copy_from_slice(&parent.to_le_bytes());
        }
        nbt_page[0x1E8] = nbt_entries.len() as u8;
        nbt_page[0x1EA] = 32;
        nbt_page[0x1EB] = 0;
        nbt_page[0x1F0] = 0x81; // ptypeNBT
        pst_bytes.extend_from_slice(&nbt_page);

        pst_bytes[224..232].copy_from_slice(&nbt_offset.to_le_bytes());
        pst_bytes[240..248].copy_from_slice(&bbt_offset.to_le_bytes());
        std::fs::write(&pst_path, &pst_bytes).unwrap();

        crate::rebuild_index(&index_dir, &docs_dir).unwrap();
        let searcher = crate::Searcher::open(&index_dir).unwrap();

        // 1. Search by email HTML body text + verify virtual `::nid` hit
        let hits = searcher.search("실행 캐시 검증", 10).unwrap();
        assert!(hits.iter().any(|h| h.path.contains("::2097188")), "Expected per-email hit: {:?}", hits.iter().map(|h| &h.path).collect::<Vec<_>>());

        // 2. Search by sender (`author:김철수`), title (`title:승인`), PST folder (`path:배터리팀`), attachment filename (`셀전압_리포트`)
        assert!(!searcher.search("author:김철수", 10).unwrap().is_empty(), "author: query must find PST sender");
        assert!(!searcher.search("title:승인", 10).unwrap().is_empty(), "title: query must find PST subject");
        assert!(!searcher.search("path:배터리팀 캐시", 10).unwrap().is_empty(), "path: query must match internal PST folder");
        assert!(!searcher.search("셀전압_리포트", 10).unwrap().is_empty(), "Attachment filename inside PST must be searchable");

        // 3. Preview on virtual `::nid` path returns highlighted body
        let vpath = &hits.iter().find(|h| h.path.contains("::")).unwrap().path;
        for needle in ["excelneedle", "slidesneedle", "pdfneedle"] {
            let query = format!("ext:pst body:{needle}");
            let matches = searcher.search(&query, 10).unwrap();
            assert_eq!(matches.len(), 1, "{needle} should match just the parent email");
            assert_eq!(&matches[0].path, vpath);
            let preview = searcher.preview(vpath, &query).unwrap().unwrap();
            assert!(preview.snippet.contains(needle));
            assert!(!preview.highlighted.is_empty());
        }
        for (needle, expected_metadata) in [
            ("officepropertyneedle", ["첨부 문서 작성자: Office Writer", "첨부 문서 생성 시각: 2024-02-03 04:05 UTC"]),
            ("pdfpropertyneedle", ["첨부 문서 작성자: PDF Writer", "첨부 문서 수정 시각: 2024-02-03 04:05 UTC"]),
        ] {
            let query = format!("ext:pst body:{needle}");
            let found = searcher.search(&query, 10).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(&found[0].path, vpath);
            assert_eq!(found[0].doc_created, Some(1_767_999_999_000));
            assert_eq!(found[0].doc_modified, Some(1_768_000_001_000));
            assert!(found[0].author.as_deref().unwrap().starts_with("김철수"));
            assert!(searcher.search(&format!("title:{needle}"), 10).unwrap().is_empty());
            let preview = searcher.preview(vpath, &query).unwrap().unwrap();
            for expected in expected_metadata {
                assert!(preview.snippet.contains(expected), "{expected}");
            }
        }
        let prev = searcher.preview(vpath, "캐시").unwrap().expect("preview must resolve virtual PST message path");
        assert!(prev.snippet.contains("실행 캐시 검증"));
        assert!(!prev.highlighted.is_empty());

        // 4. Reconcile on unchanged PST does zero work (`added == 0, modified == 0, removed == 0`)
        let mut updater = crate::IndexUpdater::open(&index_dir).unwrap();
        let rec = crate::reconcile(&docs_dir, &mut updater).unwrap();
        assert_eq!(rec, crate::ReconcileStats { added: 0, modified: 0, removed: 0 });

        let output = dir.path().join("markdown");
        std::fs::create_dir(&output).unwrap();
        crate::export::sync(&index_dir, &output).unwrap();
        let files: Vec<_> = std::fs::read_dir(&output)
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect();
        assert_eq!(files.len(), 2, "one summary and one message");
        let mail = files.iter().find(|text| text.contains("message_id:")).unwrap();
        for expected in [
            "source_path: \"team_archive.pst\"",
            "message_id: \"2097188\"",
            "# BMS 회로 설계 최종 승인",
            "실행 캐시 검증",
            "excelneedle",
            "slidesneedle",
            "pdfneedle",
            "첨부 문서 작성자: Office Writer",
        ] {
            assert!(mail.contains(expected), "missing {expected}");
        }
        let summary = files.iter().find(|text| !text.contains("message_id:")).unwrap();
        assert!(summary.contains("총 1통"));
        assert!(!summary.contains("excelneedle"));

        std::fs::remove_file(&pst_path).unwrap();
        crate::reconcile(&docs_dir, &mut updater).unwrap();
        crate::export::sync(&index_dir, &output).unwrap();
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 0);
    }
}
