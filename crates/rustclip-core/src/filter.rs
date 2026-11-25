//! Section and text filtering shared by every platform shell.

use crate::store::{Clip, ClipKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Text,
    Images,
}

/// Indices of the clips visible in `section` for `query`, newest first.
pub fn filter_indices(clips: &[Clip], section: Section, query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    clips
        .iter()
        .enumerate()
        .filter(|(_, clip)| match section {
            Section::Images => clip.kind == ClipKind::Image,
            Section::Text => clip.kind != ClipKind::Image,
        })
        .filter(|(_, clip)| {
            if query.is_empty() || clip.preview.to_lowercase().contains(&query) {
                return true;
            }
            if let Some(text) = &clip.text
                && text.to_lowercase().contains(&query)
            {
                return true;
            }
            if let Some(source) = &clip.source_name
                && source.to_lowercase().contains(&query)
            {
                return true;
            }
            clip.display_title().to_lowercase().contains(&query)
        })
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(kind: ClipKind, preview: &str) -> Clip {
        Clip {
            id: "id".to_string(),
            kind,
            preview: preview.to_string(),
            text: Some(preview.to_string()),
            data_type: None,
            blob: None,
            byte_len: preview.len(),
            width: None,
            height: None,
            source_name: Some("Safari".to_string()),
            source_bundle: Some("com.apple.Safari".to_string()),
            date_ms: 0,
            hash: 1,
            pinned: false,
        }
    }

    #[test]
    fn sections_keep_text_and_images_separate() {
        let clips = vec![
            clip(ClipKind::Text, "hello"),
            clip(ClipKind::Image, "Image"),
            clip(ClipKind::File, "notes.txt"),
        ];

        assert_eq!(filter_indices(&clips, Section::Text, ""), vec![0, 2]);
        assert_eq!(filter_indices(&clips, Section::Images, ""), vec![1]);
    }

    #[test]
    fn query_matches_preview_text_and_source() {
        let clips = vec![clip(ClipKind::Text, "hello world")];
        assert_eq!(filter_indices(&clips, Section::Text, "HELLO"), vec![0]);
        assert_eq!(filter_indices(&clips, Section::Text, "world"), vec![0]);
        assert_eq!(filter_indices(&clips, Section::Text, "safari"), vec![0]);
        assert!(filter_indices(&clips, Section::Text, "absent").is_empty());
    }
}
