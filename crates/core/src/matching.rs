// SPDX-License-Identifier: GPL-3.0-or-later
//! Склейка одного и того же трека из разных провайдеров.
//!
//! Правила консервативные: лучше не склеить, чем склеить разные записи.

use crate::Track;
use unicode_normalization::UnicodeNormalization;

/// Допустимая разница длительностей для склейки по метаданным.
const META_DURATION_TOLERANCE_MS: u64 = 2000;

/// Способ, которым определено совпадение двух треков.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Isrc,
    Meta,
}

/// Ключ сравнения: normalize(title) + '\u{1f}' + normalize(первый артист).
/// None, если у трека нет артистов или ключ пуст.
pub fn match_key(track: &Track) -> Option<String> {
    let artist = track.artists.first()?;
    let title = normalize(&track.title);
    let artist = normalize(&artist.name);
    if title.is_empty() || artist.is_empty() {
        return None;
    }
    Some(format!("{title}\u{1f}{artist}"))
}

/// Нормализация для сравнения (не для показа).
pub fn normalize(text: &str) -> String {
    let folded = text
        .nfkc()
        .collect::<String>()
        .to_lowercase()
        .replace('ё', "е");
    let without_groups = strip_groups(&folded, is_removable_group);
    let without_radio = strip_radio_edit_suffix(&without_groups);
    let mut out = String::with_capacity(without_radio.len());
    for c in without_radio.chars() {
        if is_apostrophe(c) {
            continue;
        }
        if c.is_alphanumeric() {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Можно ли считать два трека одним. None — нельзя.
pub fn can_merge(a: &Track, b: &Track) -> Option<MergeMethod> {
    if a.id.provider() == b.id.provider() {
        return None;
    }
    if let (Some(x), Some(y)) = (isrc_key(a), isrc_key(b)) {
        // Different recordings (original vs remaster) carry different ISRCs.
        return (x == y).then_some(MergeMethod::Isrc);
    }
    let key_a = match_key(a)?;
    let key_b = match_key(b)?;
    if key_a == key_b && a.duration_ms.abs_diff(b.duration_ms) <= META_DURATION_TOLERANCE_MS {
        Some(MergeMethod::Meta)
    } else {
        None
    }
}

fn isrc_key(track: &Track) -> Option<String> {
    track
        .isrc
        .as_deref()
        .map(|isrc| isrc.trim().to_uppercase())
        .filter(|isrc| !isrc.is_empty())
}

fn is_apostrophe(c: char) -> bool {
    matches!(c, '\'' | '’' | '`' | 'ʼ')
}

/// Удаляет скобочные группы `(...)` и `[...]`, для которых `remove` вернул true.
/// Группа без закрывающей скобки остаётся как есть.
fn strip_groups(text: &str, remove: impl Fn(&str) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(['(', '[']) {
        let group = &rest[start..];
        let close = if group.starts_with('(') { ')' } else { ']' };
        let Some(end) = group.find(close) else {
            break;
        };
        out.push_str(&rest[..start]);
        if remove(&group[1..end]) {
            out.push(' ');
        } else {
            out.push_str(&group[..=end]);
        }
        rest = &group[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Хвосты, которые не меняют трек: feat/ft/featuring, remaster, radio edit.
fn is_removable_group(inner: &str) -> bool {
    let inner = inner.trim();
    if inner == "radio edit" {
        return true;
    }
    if ["feat", "ft", "featuring"]
        .iter()
        .any(|word| starts_with_word(inner, word))
    {
        return true;
    }
    inner
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == "remaster" || word == "remastered")
}

/// Убирает суффикс ` - radio edit` (с дефисом или тире).
fn strip_radio_edit_suffix(text: &str) -> &str {
    let trimmed = text.trim_end();
    let Some(head) = trimmed.strip_suffix("radio edit") else {
        return text;
    };
    match head.trim_end().strip_suffix(['-', '–', '—']) {
        Some(stripped) => stripped,
        None => text,
    }
}

fn starts_with_word(text: &str, word: &str) -> bool {
    text.strip_prefix(word)
        .is_some_and(|rest| !rest.starts_with(char::is_alphanumeric))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Artist, Availability, TrackId};

    fn track(
        provider_id: &str,
        local: &str,
        title: &str,
        artist: &str,
        duration_ms: u64,
        isrc: Option<&str>,
    ) -> Track {
        let id: TrackId = format!("{provider_id}:{local}")
            .parse()
            .expect("valid track id");
        let artists = if artist.is_empty() {
            Vec::new()
        } else {
            vec![Artist {
                id: format!("{provider_id}:artist")
                    .parse()
                    .expect("valid artist id"),
                name: artist.into(),
                cover: None,
            }]
        };
        Track {
            id,
            title: title.into(),
            artists,
            album: None,
            album_title: None,
            duration_ms,
            cover: None,
            availability: Availability::Available,
            isrc: isrc.map(str::to_owned),
        }
    }

    fn yandex(title: &str, artist: &str, duration_ms: u64) -> Track {
        track("yandex", "1", title, artist, duration_ms, None)
    }

    fn vk(title: &str, artist: &str, duration_ms: u64) -> Track {
        track("vk", "1", title, artist, duration_ms, None)
    }

    #[test]
    fn feat_suffix_is_ignored() {
        let a = yandex("Song (feat. X)", "Artist", 200_000);
        let b = vk("Song", "Artist", 200_000);
        assert_eq!(normalize("Song (feat. X)"), normalize("Song"));
        assert_eq!(normalize("Song [ft. X]"), normalize("Song"));
        assert_eq!(normalize("Song (featuring X)"), normalize("Song"));
        assert_eq!(can_merge(&a, &b), Some(MergeMethod::Meta));
    }

    #[test]
    fn radio_edit_is_ignored() {
        assert_eq!(normalize("Song - Radio Edit"), normalize("Song"));
        assert_eq!(normalize("Song (Radio Edit)"), normalize("Song"));
    }

    #[test]
    fn remaster_is_ignored() {
        assert_eq!(normalize("Song [Remastered 2011]"), normalize("Song"));
        assert_eq!(normalize("Song (2011 Remaster)"), normalize("Song"));
        assert_eq!(normalize("Song (Remastered)"), normalize("Song"));
    }

    #[test]
    fn version_markers_stay_part_of_key() {
        let base = yandex("Song", "Artist", 200_000);
        for title in [
            "Song (Live)",
            "Song (Remix)",
            "Song (Acoustic)",
            "Song (Instrumental)",
        ] {
            let other = vk(title, "Artist", 200_000);
            assert_eq!(can_merge(&base, &other), None, "{title}");
        }
    }

    #[test]
    fn yo_is_folded_to_ye() {
        assert_eq!(normalize("Ёлка"), normalize("Елка"));
        let a = yandex("Ёлка", "Артист", 180_000);
        let b = vk("Елка", "Артист", 180_000);
        assert_eq!(can_merge(&a, &b), Some(MergeMethod::Meta));
    }

    #[test]
    fn case_and_punctuation_are_ignored() {
        assert_eq!(normalize("Don't Stop!"), "dont stop");
        assert_eq!(normalize("Don't Stop!"), normalize("dont stop"));
        assert_eq!(normalize("  Hello,   World  "), "hello world");
    }

    #[test]
    fn fullwidth_is_folded_by_nfkc() {
        assert_eq!(normalize("ＡＢＣ"), normalize("abc"));
    }

    #[test]
    fn same_isrc_merges_despite_different_titles() {
        let a = track(
            "yandex",
            "1",
            "Song",
            "Artist A",
            200_000,
            Some("USRC17607839"),
        );
        let b = track(
            "vk",
            "9",
            "Other Title",
            "Artist B",
            10_000,
            Some("USRC17607839"),
        );
        assert_eq!(can_merge(&a, &b), Some(MergeMethod::Isrc));
    }

    #[test]
    fn isrc_compares_trimmed_and_uppercased() {
        let a = track(
            "yandex",
            "1",
            "Song",
            "Artist",
            200_000,
            Some(" usrc17607839 "),
        );
        let b = track("vk", "9", "Song", "Artist", 200_000, Some("USRC17607839"));
        assert_eq!(can_merge(&a, &b), Some(MergeMethod::Isrc));
        let blank = track("vk", "9", "Song", "Artist", 200_000, Some("   "));
        let other = track("yandex", "1", "Song", "Artist", 200_000, Some(""));
        assert_eq!(can_merge(&other, &blank), Some(MergeMethod::Meta));
    }

    #[test]
    fn duration_tolerance_is_two_seconds() {
        let a = yandex("Song", "Artist", 200_000);
        assert_eq!(
            can_merge(&a, &vk("Song", "Artist", 202_000)),
            Some(MergeMethod::Meta)
        );
        assert_eq!(
            can_merge(&a, &vk("Song", "Artist", 198_000)),
            Some(MergeMethod::Meta)
        );
        assert_eq!(can_merge(&a, &vk("Song", "Artist", 202_001)), None);
    }

    #[test]
    fn same_provider_never_merges() {
        let a = track(
            "yandex",
            "1",
            "Song",
            "Artist",
            200_000,
            Some("USRC17607839"),
        );
        let b = track(
            "yandex",
            "2",
            "Song",
            "Artist",
            200_000,
            Some("USRC17607839"),
        );
        assert_eq!(can_merge(&a, &b), None);
        let c = yandex("Song", "Artist", 200_000);
        let d = track("yandex", "2", "Song", "Artist", 200_000, None);
        assert_eq!(can_merge(&c, &d), None);
    }

    #[test]
    fn no_artists_means_no_key_and_no_meta_merge() {
        let a = yandex("Song", "", 200_000);
        let b = vk("Song", "", 200_000);
        assert_eq!(match_key(&a), None);
        assert_eq!(can_merge(&a, &b), None);
    }

    #[test]
    fn same_title_different_artists_do_not_merge() {
        let a = yandex("Song", "Artist A", 200_000);
        let b = vk("Song", "Artist B", 200_000);
        assert_eq!(can_merge(&a, &b), None);
    }

    #[test]
    fn different_isrcs_block_metadata_merge() {
        let mut a = yandex("Song", "Artist", 200_000);
        let mut b = vk("Song", "Artist", 200_000);
        a.isrc = Some("USAAA1111111".into());
        b.isrc = Some("USBBB2222222".into());
        assert_eq!(can_merge(&a, &b), None);
        b.isrc = None;
        assert_eq!(can_merge(&a, &b), Some(MergeMethod::Meta));
    }
}
