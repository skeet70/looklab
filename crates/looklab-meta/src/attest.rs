// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::exif::ParsedMetadata;
use crate::types::{CaptureTime, ImageNumber};

pub fn has_makernotes(metadata: &ParsedMetadata) -> bool {
    metadata.exif.has_maker_note || metadata.fuji.is_some()
}

pub fn software_matches_firmware(software: Option<&str>, firmware: Option<&str>) -> bool {
    let (Some(software), Some(firmware)) = (software, firmware) else {
        return false;
    };
    if software.trim() == firmware.trim() {
        return true;
    }
    let Some(firmware_version) = parse_version(firmware) else {
        return false;
    };
    software
        .split_whitespace()
        .any(|token| parse_version(token) == Some(firmware_version))
}

/// Which pipeline rendered a JPEG ([docs/07 §2](../../../docs/07-contribution.md)).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Renderer {
    /// The camera rendered it: a camera JPEG, an in-camera RAW conversion, or the
    /// maker's RAW converter running on the camera's image processor.
    Camera,
    /// An external editor saved it, so its colours show that edit.
    Editor { app: EditorApp },
    /// The camera rendered it, but its maker notes were stripped afterwards.
    Resaved { stripped_makernotes: bool },
}

/// An external editor whose `Software` tag identifies its exports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorApp {
    Lightroom,
    CaptureOne,
    Darktable,
    RapidRaw,
}

impl EditorApp {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Lightroom => "Adobe Lightroom",
            Self::CaptureOne => "Capture One",
            Self::Darktable => "darktable",
            Self::RapidRaw => "RapidRAW",
        }
    }
}

/// What [`classify_renderer`] reads from a JPEG and, when known, its paired RAW.
#[derive(Clone, Copy, Debug)]
pub struct RendererEvidence<'a> {
    /// The JPEG's EXIF `Software`.
    pub software: Option<&'a str>,
    /// Whether the JPEG carries maker notes.
    pub makernotes: bool,
    /// Whether the paired RAW carries maker notes.
    pub raw_makernotes: bool,
    /// The paired RAW's camera firmware, never a version derived from the JPEG's own `Software`.
    pub raw_firmware: Option<&'a str>,
}

/// Classifies a JPEG's renderer from its `Software` tag and maker notes.
///
/// Recognized editor strings:
/// - `Adobe Photoshop Lightroom 4.0 (Macintosh)` and
///   `Adobe Photoshop Lightroom Classic 13.2 (Windows)`.
/// - `Capture One 11`, `Capture One 16 for Macintosh` and `Capture One 22`.
/// - `darktable 4.6.1` (darktable sets `Exif.Image.Software` to its package string).
/// - `RapidRAW` (RapidRAW sets `ExifTag::Software` to that literal).
///
/// A camera render is any non-editor JPEG that still carries maker notes. Camera
/// firmware writes `Digital Camera X-M1 Ver1.00`, and an in-camera RAW conversion
/// writes the same string; FUJIFILM X RAW STUDIO converts on the camera's image
/// processor. Those strings are evidence, but maker notes decide the classification.
///
/// A JPEG with no maker notes lost the camera's settings; when its RAW has them,
/// the maker notes were stripped after rendering, as a phone-app transfer does.
pub fn classify_renderer(evidence: RendererEvidence<'_>) -> Renderer {
    let software = evidence
        .software
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if let Some(app) = software.and_then(editor_app) {
        return Renderer::Editor { app };
    }
    if !evidence.makernotes {
        return Renderer::Resaved {
            stripped_makernotes: evidence.raw_makernotes,
        };
    }
    Renderer::Camera
}

impl ParsedMetadata {
    /// Classifies this JPEG's renderer; pass its paired RAW when one is known.
    pub fn renderer(&self, raw: Option<&ParsedMetadata>) -> Renderer {
        classify_renderer(RendererEvidence {
            software: self.exif.software.as_deref(),
            makernotes: has_makernotes(self),
            raw_makernotes: raw.is_some_and(has_makernotes),
            raw_firmware: raw.and_then(|raw| raw.firmware.as_deref()),
        })
    }
}

fn editor_app(software: &str) -> Option<EditorApp> {
    let lower = software.to_ascii_lowercase();
    if lower.starts_with("adobe photoshop lightroom")
        || lower.starts_with("adobe lightroom")
        || lower.starts_with("lightroom")
    {
        return Some(EditorApp::Lightroom);
    }
    if lower.starts_with("capture one") {
        return Some(EditorApp::CaptureOne);
    }
    if lower.starts_with("darktable") {
        return Some(EditorApp::Darktable);
    }
    if lower.starts_with("rapidraw") {
        return Some(EditorApp::RapidRaw);
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingData {
    pub capture_time: Option<CaptureTime>,
    pub image_number: Option<ImageNumber>,
}

pub fn pairing_data(metadata: &ParsedMetadata) -> PairingData {
    PairingData {
        capture_time: metadata.exif.capture_time.clone(),
        image_number: metadata.fuji.as_ref().and_then(|fuji| fuji.image_count),
    }
}

fn parse_version(text: &str) -> Option<(u32, u32)> {
    let text = text.trim().trim_matches('\0').trim();
    let text = text
        .strip_prefix("Ver")
        .or_else(|| text.strip_prefix("ver"))
        .unwrap_or(text);
    let text = text
        .strip_prefix('V')
        .or_else(|| text.strip_prefix('v'))
        .unwrap_or(text)
        .trim();
    if let Some((major, minor)) = text.split_once('.') {
        return Some((component(major)?, component(minor)?));
    }
    if text.len() == 4 && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some((component(&text[..2])?, component(&text[2..])?));
    }
    if !text.is_empty() && text.len() <= 3 && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some((text.parse().ok()?, 0));
    }
    None
}

fn component(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(software: Option<&str>) -> RendererEvidence<'_> {
        RendererEvidence {
            software,
            makernotes: true,
            raw_makernotes: true,
            raw_firmware: Some("1.00"),
        }
    }

    #[test]
    fn lightroom_software_is_an_editor_export() {
        // A real Lightroom 4 export (ianare/exif-samples, jpg/tests/46_UnicodeEncodeError.jpg)
        // and the current Lightroom Classic string (metadata-editor.com/glossary/software).
        for software in [
            "Adobe Photoshop Lightroom 4.0 (Macintosh)",
            "Adobe Photoshop Lightroom Classic 13.2 (Windows)",
        ] {
            assert_eq!(
                classify_renderer(jpeg(Some(software))),
                Renderer::Editor {
                    app: EditorApp::Lightroom
                },
                "{software}"
            );
        }
    }

    #[test]
    fn capture_one_software_is_an_editor_export() {
        // Capture One writes the product name without a consistent version suffix
        // (Capture One support community post 7831714247837).
        for software in [
            "Capture One 11",
            "Capture One 16 for Macintosh",
            "Capture One 22",
        ] {
            assert_eq!(
                classify_renderer(jpeg(Some(software))),
                Renderer::Editor {
                    app: EditorApp::CaptureOne
                },
                "{software}"
            );
        }
    }

    #[test]
    fn darktable_and_rapidraw_software_are_editor_exports() {
        // darktable sets Exif.Image.Software to its package string in src/common/exif.cc;
        // tools/create_version_c.sh formats that as "darktable <version>".
        assert_eq!(
            classify_renderer(jpeg(Some("darktable 4.6.1"))),
            Renderer::Editor {
                app: EditorApp::Darktable
            }
        );
        // RapidRAW sets ExifTag::Software("RapidRAW") in
        // src-tauri/src/metadata/export_metadata.rs.
        assert_eq!(
            classify_renderer(jpeg(Some("RapidRAW"))),
            Renderer::Editor {
                app: EditorApp::RapidRaw
            }
        );
    }

    #[test]
    fn fujifilm_firmware_software_is_a_camera_render() {
        // A real X-M1 JPEG writes "Digital Camera X-M1 Ver1.00"; an in-camera RAW
        // conversion writes the same firmware string.
        for firmware in [Some("1.00"), Some("2.00"), None] {
            let mut evidence = jpeg(Some("Digital Camera X-M1 Ver1.00"));
            evidence.raw_firmware = firmware;
            assert_eq!(
                classify_renderer(evidence),
                Renderer::Camera,
                "{firmware:?}"
            );
        }
        let mut stripped = jpeg(Some("Digital Camera X-M1 Ver1.00"));
        stripped.makernotes = false;
        assert_eq!(
            classify_renderer(stripped),
            Renderer::Resaved {
                stripped_makernotes: true
            }
        );
    }

    #[test]
    fn x_raw_studio_is_a_camera_render() {
        // FUJIFILM's X RAW STUDIO features guide says the conversion runs on the
        // camera's image processor. Its exact Software string is unconfirmed, so the
        // app-name prefix is accepted defensively alongside the camera firmware string.
        assert_eq!(
            classify_renderer(jpeg(Some("FUJIFILM X RAW STUDIO 1.28.0"))),
            Renderer::Camera
        );
        let mut stripped = jpeg(Some("FUJIFILM X RAW STUDIO 1.28.0"));
        stripped.makernotes = false;
        assert_eq!(
            classify_renderer(stripped),
            Renderer::Resaved {
                stripped_makernotes: true
            }
        );
    }

    #[test]
    fn a_makernote_less_jpeg_is_re_saved() {
        let evidence = RendererEvidence {
            software: Some("iOS 17.5.1"),
            makernotes: false,
            raw_makernotes: true,
            raw_firmware: Some("1.00"),
        };
        assert_eq!(
            classify_renderer(evidence),
            Renderer::Resaved {
                stripped_makernotes: true
            }
        );
        let unknown_raw = RendererEvidence {
            software: None,
            makernotes: false,
            raw_makernotes: false,
            raw_firmware: None,
        };
        assert_eq!(
            classify_renderer(unknown_raw),
            Renderer::Resaved {
                stripped_makernotes: false
            }
        );
    }

    #[test]
    fn unlisted_software_with_makernotes_stays_a_camera_render() {
        assert_eq!(
            classify_renderer(jpeg(Some("NIKON D750 Ver1.01"))),
            Renderer::Camera
        );
        assert_eq!(classify_renderer(jpeg(None)), Renderer::Camera);
    }
}
