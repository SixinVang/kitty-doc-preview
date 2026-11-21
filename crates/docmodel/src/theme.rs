//! Colors everything derives from. The default source is the terminal's own
//! palette (queried via OSC 10/11/4 at startup), with `dark`/`light` presets
//! as built-in overrides (`--theme` / `DOCP_THEME`).

use super::Rgb;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Theme {
    pub name: &'static str,
    pub fg: Rgb,
    pub bg: Rgb,
    /// ANSI colors 0 to 15.
    pub palette: [Rgb; 16],
}

impl Theme {
    pub fn dark_default() -> Theme {
        let p = [
            (0x1d, 0x1f, 0x21),
            (0xcc, 0x66, 0x66),
            (0xb5, 0xbd, 0x68),
            (0xf0, 0xc6, 0x74),
            (0x81, 0xa2, 0xbe),
            (0xb2, 0x94, 0xbb),
            (0x8a, 0xbe, 0xb7),
            (0xc5, 0xc8, 0xc6),
            (0x66, 0x66, 0x66),
            (0xd5, 0x4e, 0x53),
            (0xb9, 0xca, 0x4a),
            (0xe7, 0xc5, 0x47),
            (0x7a, 0xa6, 0xda),
            (0xc3, 0x97, 0xd8),
            (0x70, 0xc0, 0xb1),
            (0xea, 0xea, 0xea),
        ];
        Theme {
            name: "dark",
            fg: Rgb(0xe0, 0xe0, 0xe0),
            bg: Rgb(0x1d, 0x1f, 0x21),
            palette: p.map(|(r, g, b)| Rgb(r, g, b)),
        }
    }

    pub fn light_default() -> Theme {
        let p = [
            (0xf6, 0xf8, 0xfa),
            (0xcf, 0x22, 0x2e),
            (0x1a, 0x7f, 0x37),
            (0x9a, 0x67, 0x00),
            (0x05, 0x50, 0xf0),
            (0x82, 0x3f, 0xb8),
            (0x1b, 0x7c, 0x83),
            (0x24, 0x29, 0x2e),
            (0x8c, 0x95, 0x9e),
            (0xa4, 0x0e, 0x26),
            (0x11, 0x6d, 0x29),
            (0x74, 0x4c, 0x00),
            (0x09, 0x69, 0xda),
            (0x62, 0x2c, 0xa7),
            (0x1b, 0x7c, 0x83),
            (0x45, 0x4c, 0x52),
        ];
        Theme {
            name: "light",
            fg: Rgb(0x24, 0x29, 0x2e),
            bg: Rgb(0xf6, 0xf8, 0xfa),
            palette: p.map(|(r, g, b)| Rgb(r, g, b)),
        }
    }

    pub fn accent(&self) -> Rgb {
        self.palette[12]
    }
    pub fn red(&self) -> Rgb {
        self.palette[1]
    }
    pub fn green(&self) -> Rgb {
        self.palette[2]
    }
    pub fn yellow(&self) -> Rgb {
        self.palette[3]
    }
    pub fn purple(&self) -> Rgb {
        self.palette[13]
    }
    pub fn cyan(&self) -> Rgb {
        self.palette[6]
    }
    pub fn muted(&self) -> Rgb {
        Self::mix(self.fg, self.bg, 0.45)
    }
    pub fn faint(&self) -> Rgb {
        Self::mix(self.fg, self.bg, 0.7)
    }
    pub fn code_bg(&self) -> Rgb {
        Self::mix(self.bg, self.fg, 0.07)
    }

    /// `t` = 0 gives `a`, 1 gives `b`.
    pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
        a.mix(b, t)
    }

    /// Parse `#rgb`, `#rrggbb` and xterm's `rgb:rrrr/gggg/bbbb`.
    pub fn parse_rgb(s: &str) -> Option<Rgb> {
        Rgb::parse(s)
    }

    /// Patch a theme from OSC 10/11/4 replies found in a probe response
    /// stream. Replies end with ST (`ESC \`) or BEL.
    pub fn from_osc(buf: &[u8], base: Theme) -> Theme {
        let mut theme = base;
        let s = String::from_utf8_lossy(buf);
        let mut fg = None;
        let mut bg = None;
        for part in s.split(['\x07']).flat_map(|p| p.split("\x1b\\")) {
            let Some(osc) = part.rfind("\x1b]").map(|i| &part[i + 2..]) else {
                continue;
            };
            if let Some(v) = osc.strip_prefix("10;") {
                fg = Self::parse_rgb(v);
            } else if let Some(v) = osc.strip_prefix("11;") {
                bg = Self::parse_rgb(v);
            } else if let Some(v) = osc.strip_prefix("4;") {
                if let Some((idx, color)) = v.split_once(';') {
                    if let (Ok(i), Some(c)) = (idx.parse::<usize>(), Self::parse_rgb(color)) {
                        if i < 16 {
                            theme.palette[i] = c;
                        }
                    }
                }
            }
        }
        if let Some(fg) = fg {
            theme.fg = fg;
        }
        if let Some(bg) = bg {
            theme.bg = bg;
        }
        theme.name = "terminal";
        theme
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xterm_and_hex() {
        assert_eq!(
            Theme::parse_rgb("rgb:0f0f/1b1b/1d1d"),
            Some(Rgb(0x0f, 0x1b, 0x1d))
        );
        assert_eq!(Theme::parse_rgb("rgb:ff/80/00"), Some(Rgb(255, 128, 0)));
        assert_eq!(Theme::parse_rgb("#fff"), Some(Rgb(255, 255, 255)));
        assert_eq!(Theme::parse_rgb("#5b7bd5"), Some(Rgb(0x5b, 0x7b, 0xd5)));
    }

    #[test]
    fn mixes() {
        assert_eq!(
            Theme::mix(Rgb(0, 0, 0), Rgb(200, 100, 50), 0.5),
            Rgb(100, 50, 25)
        );
    }

    #[test]
    fn patches_from_osc_replies() {
        let reply = b"\x1b]10;rgb:e0e0/f0f0/efef\x1b\\\x1b]11;rgb:0f0f/1b1b/1d1d\x1b\\\x1b]4;4;rgb:6a6a/7474/8a8a\x1b\\\x1b_Gi=31;OK\x1b\\\x1b[?62;22c";
        let t = Theme::from_osc(reply, Theme::dark_default());
        assert_eq!(t.name, "terminal");
        assert_eq!(t.bg, Rgb(0x0f, 0x1b, 0x1d));
        assert_eq!(t.fg, Rgb(0xe0, 0xf0, 0xef));
        assert_eq!(t.palette[4], Rgb(0x6a, 0x74, 0x8a));
    }
}
