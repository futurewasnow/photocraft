//! GPano XMP for complete monoscopic equirectangular photos, per Google's Photo Sphere spec.
use crate::{Error, Result, pixel_count};

pub fn gpano(width: u32, height: u32) -> Result<String> {
    pixel_count(width, height)?;
    if u64::from(width) != 2 * u64::from(height) {
        return Err(Error::Invalid("GPano full-sphere dimensions must be 2:1"));
    }
    Ok(format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:GPano="http://ns.google.com/photos/1.0/panorama/" GPano:UsePanoramaViewer="True" GPano:ProjectionType="equirectangular" GPano:FullPanoWidthPixels="{width}" GPano:FullPanoHeightPixels="{height}" GPano:CroppedAreaImageWidthPixels="{width}" GPano:CroppedAreaImageHeightPixels="{height}" GPano:CroppedAreaLeftPixels="0" GPano:CroppedAreaTopPixels="0"/></rdf:RDF></x:xmpmeta>"#
    ))
}

/// Add XMP to a newly encoded JPEG. Caller must not use this to replace existing metadata.
pub fn tag_new_jpeg(jpeg: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    if jpeg.get(..2) != Some(&[0xff, 0xd8]) {
        return Err(Error::Invalid("expected JPEG SOI marker"));
    }
    let xmp = gpano(width, height)?;
    let prefix = b"http://ns.adobe.com/xap/1.0/\0";
    let length = u16::try_from(prefix.len() + xmp.len() + 2).map_err(|_| Error::TooLarge)?;
    let mut out = Vec::new();
    let size = jpeg
        .len()
        .checked_add(usize::from(length) + 2)
        .ok_or(Error::TooLarge)?;
    out.try_reserve_exact(size).map_err(|_| Error::Allocation)?;
    out.extend_from_slice(&[0xff, 0xd8, 0xff, 0xe1]);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(prefix);
    out.extend_from_slice(xmp.as_bytes());
    if let Some(rest) = jpeg.get(2..) {
        out.extend_from_slice(rest);
    }
    Ok(out)
}
