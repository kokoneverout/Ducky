use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

pub struct Assets;
impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(match path {
            "app-icon.png" => Some(Cow::Borrowed(include_bytes!("../assets/icon.png"))),
            _ => None,
        })
    }
    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(["app-icon.png"]
            .into_iter()
            .filter(|name| name.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
