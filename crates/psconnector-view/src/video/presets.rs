//! Named picture presets: a picture adjustment plus the source range, saved per game in the settings file
//! (`picture_presets`). Only "Original" is built in, so every game starts untouched.

use psconnector::VideoRange;
use psconnector_protocol::settings;
use serde_json::{Value, json};

use crate::video::picture::Picture;

const KEY: &str = "picture_presets";
pub(crate) const ORIGINAL: &str = "Original";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Preset {
    pub(crate) name: String,
    pub(crate) picture: Picture,
    /// `None` for old presets saved before the range was part of them: they keep the current range
    pub(crate) range: Option<VideoRange>,
}

impl Preset {
    /// Whether choosing this preset would change nothing.
    pub(crate) fn matches(&self, picture: &Picture, range: VideoRange) -> bool {
        self.picture == *picture && self.range.is_none_or(|r| r == range)
    }
}

pub(crate) fn builtin() -> Vec<Preset> {
    vec![Preset { name: ORIGINAL.into(), picture: Picture::default(), range: Some(VideoRange::Auto) }]
}

/// Saved presets from the settings file.
pub(crate) fn saved() -> Vec<Preset> {
    let Some(Value::Object(all)) = settings::get(KEY) else { return Vec::new() };
    all.iter()
        .map(|(name, v)| {
            // Old presets stored picture fields directly. New presets also carry source range.
            let values = v.get("picture").unwrap_or(v);
            let range = v.get("video_range").and_then(Value::as_str).and_then(VideoRange::parse);
            Preset { name: name.clone(), picture: Picture::default().merged(values), range }
        })
        .collect()
}

/// Built-in presets first, then saved ones.
pub(crate) fn all() -> Vec<Preset> {
    builtin().into_iter().chain(saved()).collect()
}

pub(crate) fn save(name: &str, picture: Picture, range: VideoRange) {
    let mut all = settings::get(KEY).filter(Value::is_object).unwrap_or_else(|| json!({}));
    all[name] = json!({"picture": picture.to_json(), "video_range": range.name()});
    settings::set(KEY, all);
}

/// Removes a saved preset; false if there was none by that name.
pub(crate) fn delete(name: &str) -> bool {
    let Some(mut all) = settings::get(KEY).filter(Value::is_object) else { return false };
    let removed = all.as_object_mut().is_some_and(|o| o.remove(name).is_some());
    if removed {
        settings::set(KEY, all);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_original_is_built_in_and_game_profiles_are_user_settings() {
        let presets = builtin();
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0].name, ORIGINAL);
        assert!(presets[0].picture.is_neutral());
        assert_eq!(presets[0].range, Some(VideoRange::Auto));
    }
}
