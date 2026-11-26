//! Section and text filtering shared by every platform shell.

use crate::store::{Clip, ClipKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Text,
    Images,
    Commands,
}

/// Indices of the clips visible in `section` for `query`, newest first.
pub fn filter_indices(clips: &[Clip], section: Section, query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    clips
        .iter()
        .enumerate()
        .filter(|(_, clip)| match section {
            Section::Images => clip.kind == ClipKind::Image,
            Section::Commands => clip.kind == ClipKind::Command,
            Section::Text => clip.kind != ClipKind::Image && clip.kind != ClipKind::Command,
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
            title: None,
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
        assert!(filter_indices(&clips, Section::Commands, "").is_empty());
    }

    #[test]
    fn commands_have_their_own_section() {
        let clips = vec![
            clip(ClipKind::Command, "cargo test"),
            clip(ClipKind::Text, "hello"),
            clip(ClipKind::Command, "git status"),
        ];

        assert_eq!(filter_indices(&clips, Section::Text, ""), vec![1]);
        assert_eq!(filter_indices(&clips, Section::Commands, ""), vec![0, 2]);
        assert_eq!(filter_indices(&clips, Section::Commands, "git"), vec![2]);
    }

    #[test]
    fn query_matches_preview_text_and_source() {
        let clips = vec![clip(ClipKind::Text, "hello world")];
        assert_eq!(filter_indices(&clips, Section::Text, "HELLO"), vec![0]);
        assert_eq!(filter_indices(&clips, Section::Text, "world"), vec![0]);
        assert_eq!(filter_indices(&clips, Section::Text, "safari"), vec![0]);
        assert!(filter_indices(&clips, Section::Text, "absent").is_empty());
    }

    #[test]
    fn query_matches_a_user_title_case_insensitively() {
        let mut named = clip(ClipKind::Image, "Image");
        named.title = Some("Release checklist".to_string());
        let clips = vec![named];

        assert_eq!(
            filter_indices(&clips, Section::Images, "release CHECK"),
            vec![0]
        );
        assert_eq!(filter_indices(&clips, Section::Images, "image"), vec![0]);
        assert!(filter_indices(&clips, Section::Images, "absent").is_empty());
    }

    #[test]
    fn query_matches_title_and_content_after_rename() {
        let mut named = clip(ClipKind::Text, "hello world");
        named.title = Some("Greeting".to_string());
        let clips = vec![named];

        assert_eq!(filter_indices(&clips, Section::Text, "greeting"), vec![0]);
        assert_eq!(filter_indices(&clips, Section::Text, "hello"), vec![0]);
        assert_eq!(filter_indices(&clips, Section::Text, "world"), vec![0]);
    }
}
