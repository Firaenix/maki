use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use maki_agent::{ImageMediaType, ImageSource};
use mlua::Table;

/// Reads a `{ media_type, data }` table. {what} names the entry in errors.
pub(crate) fn image_from_table(entry: &Table, what: &str) -> Result<ImageSource, String> {
    let media_type = entry
        .get::<String>("media_type")
        .map_err(|_| format!("{what} is missing 'media_type'"))?;
    let media_type = ImageMediaType::from_mime(&media_type).ok_or_else(|| {
        let supported: Vec<&str> = ImageMediaType::ALL.iter().map(|m| m.mime()).collect();
        format!(
            "unsupported {what} media_type '{media_type}' ({})",
            supported.join(", ")
        )
    })?;
    let data = entry
        .get::<String>("data")
        .map_err(|_| format!("{what} is missing base64 'data'"))?;
    // Bad base64 would land in history and fail every later request;
    // validate once at the boundary.
    if data.is_empty() {
        return Err(format!("{what} 'data' is empty"));
    }
    BASE64
        .decode(data.as_bytes())
        .map_err(|e| format!("{what} 'data' is not valid base64: {e}"))?;
    Ok(ImageSource::new(media_type, Arc::from(data)))
}
