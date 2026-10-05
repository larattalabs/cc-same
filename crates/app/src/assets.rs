//! What the window draws from: GPUI Kit's default icons, plus the few Lucide icons it needs beyond
//! them, so the whole catalog need not ship.

use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

/// Icons outside GPUI Kit's default set, by asset path.
const EXTRA: &[(&str, &[u8])] = &[("icons/pencil.svg", include_bytes!("../assets/icons/pencil.svg"))];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match EXTRA.iter().find(|(extra, _)| *extra == path) {
            Some((_, data)) => Ok(Some(Cow::Borrowed(data))),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names = gpui_kit::assets::Assets.list(path)?;
        names.extend(EXTRA.iter().filter(|(extra, _)| extra.starts_with(path)).map(|(extra, _)| (*extra).into()));
        Ok(names)
    }
}
