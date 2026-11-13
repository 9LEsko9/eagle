// SPDX-License-Identifier: GPL-3.0-or-later
//! Read trustworthy AAC trimming metadata; do not infer encoder delay.
use crate::source::Reader;
use std::io::{self, SeekFrom};
const MAX_MOOV: u64 = 4 * 1024 * 1024;
#[derive(Clone, Copy, Debug)]
pub(crate) struct Trim {
    pub start: u64,
    pub length: u64,
    pub rate: u32,
}
#[derive(Clone, Copy)]
struct Atom<'a> {
    kind: &'a [u8],
    data: &'a [u8],
}
fn atoms(mut data: &[u8]) -> Option<Vec<Atom<'_>>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        if out.len() >= 8192 || data.len() < 8 {
            return None;
        }
        let size = be32(data, 0)? as usize;
        let (size, header) = match size {
            0 => (data.len(), 8),
            1 => (usize::try_from(be64(data, 8)?).ok()?, 16),
            n => (n, 8),
        };
        if size < header || size > data.len() {
            return None;
        }
        out.push(Atom {
            kind: &data[4..8],
            data: &data[header..size],
        });
        data = &data[size..];
    }
    Some(out)
}
fn child<'a>(list: &[Atom<'a>], kind: &[u8]) -> Option<&'a [u8]> {
    list.iter().find(|a| a.kind == kind).map(|a| a.data)
}
fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}
fn be64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}
fn scale(data: &[u8]) -> Option<u32> {
    be32(data, if *data.first()? == 1 { 20 } else { 12 }).filter(|s| *s > 0)
}
fn itunes_trim(list: &[Atom<'_>]) -> Option<Trim> {
    for atom in list {
        if atom.kind == b"udta" {
            if let Some(children) = atoms(atom.data)
                && let Some(trim) = itunes_trim(&children)
            {
                return Some(trim);
            }
        } else if atom.kind == b"meta" {
            let Some(metadata) = atom.data.get(4..).and_then(atoms) else {
                continue;
            };
            let Some(items) = child(&metadata, b"ilst").and_then(atoms) else {
                continue;
            };
            for item in items.iter().filter(|a| a.kind == b"----") {
                let Some(fields) = atoms(item.data) else {
                    continue;
                };
                if child(&fields, b"mean").and_then(|s| s.get(4..)) != Some(b"com.apple.iTunes")
                    || child(&fields, b"name").and_then(|s| s.get(4..)) != Some(b"iTunSMPB")
                {
                    continue;
                }
                let parsed = (|| {
                    let text = std::str::from_utf8(child(&fields, b"data")?.get(8..)?).ok()?;
                    let mut values = text
                        .split_whitespace()
                        .map(|s| u64::from_str_radix(s, 16).ok());
                    let _ = values.next()??;
                    Some((values.next()??, values.next()??, values.next()??))
                })();
                let Some((start, padding, length)) = parsed else {
                    continue;
                };
                if length > 0
                    && start.checked_add(length).is_some()
                    && start < 1_920_000
                    && padding < 1_920_000
                {
                    return Some(Trim {
                        start,
                        length,
                        rate: 0,
                    });
                }
            }
        }
    }
    None
}
fn parse(moov: &[u8], rate: u32) -> Option<Trim> {
    let list = atoms(moov)?;
    // Prefer explicit sample counts, never combine them with edit-list trim.
    if let Some(trim) = itunes_trim(&list) {
        return Some(trim);
    }
    let movie_scale = scale(child(&list, b"mvhd")?)? as u64;
    for trak in list.iter().filter(|a| a.kind == b"trak") {
        let track = atoms(trak.data)?;
        let media = atoms(child(&track, b"mdia")?)?;
        let handler = child(&media, b"hdlr")?;
        if handler.get(8..12)? != b"soun" {
            continue;
        }
        let media_scale = scale(child(&media, b"mdhd")?)? as u64;
        let rate = if rate == 0 { media_scale as u32 } else { rate };
        let edits = atoms(child(&track, b"edts")?)?;
        let edit = child(&edits, b"elst")?;
        if be32(edit, 4)? != 1 {
            return None;
        }
        let (duration, start, at) = match edit.first()? {
            0 => (be32(edit, 8)? as u64, be32(edit, 12)? as i32 as i64, 16),
            1 => (be64(edit, 8)?, be64(edit, 16)? as i64, 24),
            _ => return None,
        };
        if start < 0 || edit.get(at..at + 4)? != [0, 1, 0, 0] {
            return None;
        }
        let start = (start as u64).checked_mul(rate as u64)? / media_scale;
        let length = duration
            .checked_mul(rate as u64)?
            .checked_add(movie_scale / 2)?
            / movie_scale;
        return (length > 0).then_some(Trim {
            start,
            length,
            rate,
        });
    }
    None
}
/// The movie atom is bounded independently of the audio payload. Range-less files
/// may omit inaccessible tail metadata; playback never guesses missing trim.
pub(crate) fn read(source: &mut Reader, rate: u32) -> io::Result<Option<Trim>> {
    let result = (|| {
        let mut offset = 0u64;
        for _ in 0..8192 {
            let mut header = [0; 8];
            match source.read_exact(&mut header) {
                Ok(()) => (),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::UnexpectedEof | io::ErrorKind::Unsupported
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
            let mut size = u32::from_be_bytes(
                header[..4]
                    .try_into()
                    .map_err(|_| io::Error::other("MP4 header"))?,
            ) as u64;
            let mut prefix = 8;
            if size == 1 {
                let mut wide = [0; 8];
                source.read_exact(&mut wide)?;
                size = u64::from_be_bytes(wide);
                prefix = 16;
            }
            if size < prefix {
                return Ok(None);
            }
            if &header[4..8] == b"moov" {
                if size > MAX_MOOV {
                    return Ok(None);
                }
                let mut moov = vec![0; (size - prefix) as usize];
                source.read_exact(&mut moov)?;
                return Ok(parse(&moov, rate));
            }
            offset = offset
                .checked_add(size)
                .ok_or_else(|| io::Error::other("MP4 atom overflow"))?;
            if source.byte_len().is_some_and(|len| offset >= len) {
                return Ok(None);
            }
            match source.seek(SeekFrom::Start(offset)) {
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::Unsupported => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    })();
    source.seek(SeekFrom::Start(0))?;
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    fn atom(kind: &[u8], data: &[u8]) -> Vec<u8> {
        [(data.len() as u32 + 8).to_be_bytes().as_slice(), kind, data].concat()
    }
    #[test]
    fn itunes_sample_counts_are_read_only_from_the_named_freeform_tag() {
        let fields = [
            atom(
                b"mean",
                &[vec![0; 4], b"com.apple.iTunes".to_vec()].concat(),
            ),
            atom(b"name", &[vec![0; 4], b"iTunSMPB".to_vec()].concat()),
            atom(
                b"data",
                &[
                    vec![0; 8],
                    b" 00000000 00000400 00000038 0000000000002EE0".to_vec(),
                ]
                .concat(),
            ),
        ]
        .concat();
        let moov = atom(
            b"udta",
            &atom(
                b"meta",
                &[vec![0; 4], atom(b"ilst", &atom(b"----", &fields))].concat(),
            ),
        );
        let trim = parse(&moov, 48000).unwrap();
        assert_eq!(trim.start, 1024);
        assert_eq!(trim.length, 12000);
        assert!(
            parse(
                &atom(
                    b"free",
                    b"iTunSMPB data 00000000 00000400 00000038 0000000000002EE0"
                ),
                48000
            )
            .is_none()
        );
    }
    fn valid_itunes_item() -> Vec<u8> {
        atom(
            b"----",
            &[
                atom(
                    b"mean",
                    &[vec![0; 4], b"com.apple.iTunes".to_vec()].concat(),
                ),
                atom(b"name", &[vec![0; 4], b"iTunSMPB".to_vec()].concat()),
                atom(
                    b"data",
                    &[
                        vec![0; 8],
                        b" 00000000 00000400 00000038 0000000000002EE0".to_vec(),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        )
    }

    #[test]
    fn meta_without_ilst_does_not_hide_a_later_trim() {
        let meta = |contents: Vec<u8>| atom(b"meta", &[vec![0; 4], contents].concat());
        let moov = atom(
            b"udta",
            &[
                meta(atom(b"free", &[])),
                meta(atom(b"ilst", &valid_itunes_item())),
            ]
            .concat(),
        );
        let trim = parse(&moov, 48000).unwrap();
        assert_eq!((trim.start, trim.length), (1024, 12000));
    }

    #[test]
    fn one_malformed_freeform_item_does_not_hide_the_next() {
        let items = [atom(b"----", &[0, 0, 0]), valid_itunes_item()].concat();
        let moov = atom(b"meta", &[vec![0; 4], atom(b"ilst", &items)].concat());
        let trim = parse(&moov, 48000).unwrap();
        assert_eq!((trim.start, trim.length), (1024, 12000));
    }

    #[test]
    fn invalid_itunes_data_does_not_hide_the_next_item() {
        let invalid_data = [
            vec![0; 7],
            [vec![0; 8], vec![0xff]].concat(),
            [vec![0; 8], b"00000000 invalid 00000038 00002EE0".to_vec()].concat(),
        ];
        for data in invalid_data {
            let fields = [
                atom(
                    b"mean",
                    &[vec![0; 4], b"com.apple.iTunes".to_vec()].concat(),
                ),
                atom(b"name", &[vec![0; 4], b"iTunSMPB".to_vec()].concat()),
                atom(b"data", &data),
            ]
            .concat();
            let items = [atom(b"----", &fields), valid_itunes_item()].concat();
            let moov = atom(b"meta", &[vec![0; 4], atom(b"ilst", &items)].concat());
            let trim = parse(&moov, 48000).unwrap();
            assert_eq!((trim.start, trim.length), (1024, 12000));
        }
    }

    #[test]
    fn simple_audio_edit_list_is_scaled_and_not_double_trimmed() {
        let mut mvhd = vec![0; 16];
        mvhd[12..].copy_from_slice(&1000u32.to_be_bytes());
        let mut mdhd = vec![0; 16];
        mdhd[12..].copy_from_slice(&48000u32.to_be_bytes());
        let mut handler = vec![0; 12];
        handler[8..].copy_from_slice(b"soun");
        let mut edit = vec![0; 20];
        edit[4..8].copy_from_slice(&1u32.to_be_bytes());
        edit[8..12].copy_from_slice(&250u32.to_be_bytes());
        edit[12..16].copy_from_slice(&1024u32.to_be_bytes());
        edit[16..20].copy_from_slice(&[0, 1, 0, 0]);
        let media = atom(
            b"mdia",
            &[atom(b"mdhd", &mdhd), atom(b"hdlr", &handler)].concat(),
        );
        let track = atom(
            b"trak",
            &[media, atom(b"edts", &atom(b"elst", &edit))].concat(),
        );
        let trim = parse(&[atom(b"mvhd", &mvhd), track].concat(), 48000).unwrap();
        assert_eq!(trim.start, 1024);
        assert_eq!(trim.length, 12000);
        assert!(atoms(&[0, 0, 0, 4, b'm', b'o', b'o', b'v']).is_none());
    }
}
