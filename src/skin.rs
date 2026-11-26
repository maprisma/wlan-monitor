//! Optional app palettes. Native follows the desktop theme.

use cosmic::iced::Color;

pub const SKIN_KEY: &str = "skin";
pub const SKINS: [&str; 6] = ["native", "synthwave", "cozy", "terminal", "disco", "zen"];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkinStyle {
    pub accent: Color,
    pub accent_rgb: [u8; 3],
    pub border_tint: Color,
    pub bg_tint: Color,
    pub window_bg: [u8; 3],
    pub foreground: [u8; 3],
    pub border_width: f32,
}

impl SkinStyle {
    pub fn palette(&self) -> maprisma_kit::look::Palette {
        maprisma_kit::look::Palette {
            accent: Some(self.accent_rgb),
            background: Some(self.window_bg),
            foreground: Some(self.foreground),
            ..Default::default()
        }
    }
}

pub fn normalize_skin(raw: &str) -> &'static str {
    SKINS
        .iter()
        .find(|skin| **skin == raw)
        .copied()
        .unwrap_or("native")
}

pub fn skin_style(skin: &str) -> Option<SkinStyle> {
    let (accent, border, tint, window_bg, foreground, border_width) = match normalize_skin(skin) {
        "synthwave" => (
            [0xff, 0x2e, 0x93],
            [0x00, 0xe5, 0xff],
            [0x3a, 0x12, 0x59],
            [0x13, 0x09, 0x24],
            [0xf4, 0xea, 0xff],
            1.5,
        ),
        "cozy" => (
            [0xf0, 0x9a, 0x36],
            [0xe0, 0x82, 0x2a],
            [0x59, 0x30, 0x12],
            [0x21, 0x15, 0x0e],
            [0xf5, 0xe8, 0xd6],
            1.25,
        ),
        "terminal" => (
            [0x2c, 0xe8, 0x68],
            [0x22, 0xc5, 0x5e],
            [0x0d, 0x38, 0x1a],
            [0x09, 0x13, 0x0b],
            [0xd2, 0xfc, 0xe0],
            1.25,
        ),
        "disco" => (
            [0xd9, 0x46, 0xef],
            [0xfa, 0xcc, 0x15],
            [0x4c, 0x16, 0x66],
            [0x1c, 0x0c, 0x2b],
            [0xf8, 0xec, 0xff],
            1.5,
        ),
        "zen" => (
            [0x52, 0xb7, 0x98],
            [0x64, 0xc2, 0xa4],
            [0x1e, 0x38, 0x32],
            [0x14, 0x1d, 0x1c],
            [0xe2, 0xf0, 0xec],
            1.0,
        ),
        _ => return None,
    };
    Some(SkinStyle {
        accent: Color::from_rgb8(accent[0], accent[1], accent[2]),
        accent_rgb: accent,
        border_tint: Color::from_rgb8(border[0], border[1], border[2]),
        bg_tint: Color::from_rgb8(tint[0], tint[1], tint[2]),
        window_bg,
        foreground,
        border_width,
    })
}

pub fn skin_label(skin: &str) -> &'static str {
    crate::i18n::tr(match normalize_skin(skin) {
        "synthwave" => "Synthwave",
        "cozy" => "Cozy",
        "terminal" => "Terminal",
        "disco" => "Disco",
        "zen" => "Zen",
        _ => "Native",
    })
}

pub fn active_theme(skin: &str, corners_choice: &str) -> Option<cosmic::Theme> {
    if let Some(style) = skin_style(skin) {
        let radius = maprisma_kit::corners::radius(
            corners_choice,
            maprisma_kit::corners::desktop_radius(),
        );
        Some(maprisma_kit::look::theme_from(&style.palette(), Some(radius)))
    } else {
        maprisma_kit::corners::theme(corners_choice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_and_order() {
        assert_eq!(
            SKINS,
            ["native", "synthwave", "cozy", "terminal", "disco", "zen"]
        );
        for skin in SKINS {
            assert_eq!(normalize_skin(skin), skin);
        }
        for raw in ["", "Native", "unknown"] {
            assert_eq!(normalize_skin(raw), "native");
        }
    }

    #[test]
    fn palettes() {
        assert!(skin_style("native").is_none());
        let expected = [
            (
                "synthwave",
                [0xff, 0x2e, 0x93],
                [0x00, 0xe5, 0xff],
                [0x3a, 0x12, 0x59],
                [0x13, 0x09, 0x24],
                [0xf4, 0xea, 0xff],
                1.5,
            ),
            (
                "cozy",
                [0xf0, 0x9a, 0x36],
                [0xe0, 0x82, 0x2a],
                [0x59, 0x30, 0x12],
                [0x21, 0x15, 0x0e],
                [0xf5, 0xe8, 0xd6],
                1.25,
            ),
            (
                "terminal",
                [0x2c, 0xe8, 0x68],
                [0x22, 0xc5, 0x5e],
                [0x0d, 0x38, 0x1a],
                [0x09, 0x13, 0x0b],
                [0xd2, 0xfc, 0xe0],
                1.25,
            ),
            (
                "disco",
                [0xd9, 0x46, 0xef],
                [0xfa, 0xcc, 0x15],
                [0x4c, 0x16, 0x66],
                [0x1c, 0x0c, 0x2b],
                [0xf8, 0xec, 0xff],
                1.5,
            ),
            (
                "zen",
                [0x52, 0xb7, 0x98],
                [0x64, 0xc2, 0xa4],
                [0x1e, 0x38, 0x32],
                [0x14, 0x1d, 0x1c],
                [0xe2, 0xf0, 0xec],
                1.0,
            ),
        ];
        for (skin, accent, border, tint, bg, fg, width) in expected {
            let style = skin_style(skin).unwrap();
            assert_eq!(style.accent_rgb, accent);
            assert_eq!(
                style.accent,
                Color::from_rgb8(accent[0], accent[1], accent[2])
            );
            assert_eq!(
                style.border_tint,
                Color::from_rgb8(border[0], border[1], border[2])
            );
            assert_eq!(style.bg_tint, Color::from_rgb8(tint[0], tint[1], tint[2]));
            assert_eq!(
                (style.window_bg, style.foreground, style.border_width),
                (bg, fg, width)
            );
            assert_eq!(
                style.palette(),
                maprisma_kit::look::Palette {
                    accent: Some(accent),
                    background: Some(bg),
                    foreground: Some(fg),
                    ..Default::default()
                }
            );
        }
    }

    #[test]
    fn labels_and_themes() {
        for (skin, label) in [
            ("native", "Native"),
            ("synthwave", "Synthwave"),
            ("cozy", "Cozy"),
            ("terminal", "Terminal"),
            ("disco", "Disco"),
            ("zen", "Zen"),
        ] {
            assert_eq!(skin_label(skin), crate::i18n::tr(label));
        }
        for skin in SKINS {
            let theme = active_theme(skin, "square").unwrap();
            assert_eq!(theme.cosmic().corner_radii.radius_m, [0.0; 4]);
            if let Some(style) = skin_style(skin) {
                assert_eq!(theme.cosmic().is_dark, style.palette().is_dark());
            }
        }
        assert_eq!(
            active_theme("unknown", "rounded")
                .unwrap()
                .cosmic()
                .corner_radii
                .radius_m,
            [12.0; 4]
        );
    }
}
