// SPDX-License-Identifier: AGPL-3.0-or-later
//! JSON names for the schema's closed sets, which derive `Deserialize` only.

macro_rules! serde_names {
    ($module:ident, $ty:ty, [$($variant:ident => $name:literal),+ $(,)?]; field) => {
        pub mod $module {
            use serde::de::Error as _;
            use serde::{Deserialize, Deserializer, Serializer};

            const NAMES: &[&str] = &[$($name),+];

            pub fn name(value: $ty) -> &'static str {
                match value {
                    $(<$ty>::$variant => $name),+
                }
            }

            pub fn parse(text: &str) -> Option<$ty> {
                match text {
                    $($name => Some(<$ty>::$variant),)+
                    _ => None,
                }
            }

            pub fn serialize<S: Serializer>(
                value: &$ty,
                serializer: S,
            ) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(name(*value))
            }

            pub fn deserialize<'de, D: Deserializer<'de>>(
                deserializer: D,
            ) -> Result<$ty, D::Error> {
                let text = String::deserialize(deserializer)?;
                parse(&text).ok_or_else(|| D::Error::unknown_variant(&text, NAMES))
            }
        }
    };
    ($module:ident, $ty:ty, [$($variant:ident => $name:literal),+ $(,)?]) => {
        pub mod $module {
            pub fn name(value: $ty) -> &'static str {
                match value {
                    $(<$ty>::$variant => $name),+
                }
            }

            pub fn parse(text: &str) -> Option<$ty> {
                match text {
                    $($name => Some(<$ty>::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

serde_names!(
    point,
    looklab_schema::InsertionPoint,
    [
        CameraRgbWbLinear => "camera_rgb_wb_linear",
        SceneLinearRec2020 => "scene_linear_rec2020",
        SceneLinearXyzD65 => "scene_linear_xyz_d65",
        DisplayEncodedPostTone => "display_encoded_post_tone",
    ];
    field
);

serde_names!(
    transfer,
    looklab_schema::TransferFunction,
    [
        Linear => "linear",
        Log2Shaper => "log2_shaper",
        Srgb => "srgb",
        Custom1d => "custom_1d",
    ];
    field
);

serde_names!(
    primaries,
    looklab_schema::Primaries,
    [
        CameraNative => "camera_native",
        Rec2020 => "rec2020",
        XyzD65 => "xyz_d65",
        Srgb => "srgb",
    ];
    field
);

serde_names!(
    precision,
    looklab_schema::Precision,
    [F32 => "f32", F16 => "f16", U16 => "u16", U12 => "u12"]
);

serde_names!(
    interpolation,
    looklab_schema::Interpolation,
    [Trilinear => "trilinear", Tetrahedral => "tetrahedral"]
);
