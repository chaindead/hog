//! Colours and level tags. The palette and the hash are a compatibility
//! contract with hulog, not a style choice — a user switching binaries must see
//! the same key in the same colour.

use anstyle::{AnsiColor, Color, Effects, RgbColor, Style};

/// How much colour the output stream can carry.
///
/// Resolved once by `crate::output::Output::stdout` and handed to the theme
/// at start-up, so no per-line environment probing happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    /// Colour is off. Styles are still produced; `AutoStream` strips them.
    None,
    /// 256-colour terminal. Truecolor palette entries are downgraded with
    /// [`anstyle_lossy`] — Apple Terminal.app has no 24-bit colour, and
    /// development happens on macOS.
    Ansi256,
    TrueColor,
}

/// Key colours, byte-identical to hulog's `keyColors` (`helpers.go:43`).
///
/// Changing this list re-colours every key at once. That is expected and is the
/// documented consequence of overriding `key_colors` in the config (v0.2).
pub(crate) const KEY_PALETTE: [RgbColor; 10] = [
    RgbColor(255, 51, 102),
    RgbColor(102, 204, 102),
    RgbColor(255, 153, 51),
    RgbColor(102, 204, 255),
    RgbColor(204, 153, 255),
    RgbColor(204, 153, 102),
    RgbColor(102, 153, 153),
    RgbColor(255, 153, 153),
    RgbColor(153, 204, 102),
    RgbColor(153, 153, 204),
];

/// Known level names, their three-letter tags and their styles
/// (hulog `helpers.go:63`). Lookup is case-insensitive on the raw value.
///
/// `warning` and `warn` intentionally share a tag: the tag column must stay
/// three characters wide so messages line up.
pub(crate) const LEVELS: [(&str, &str, Style); 8] = [
    (
        "trace",
        "TRC",
        Style::new().fg_color(Some(Color::Ansi(AnsiColor::Magenta))),
    ),
    (
        "debug",
        "DBG",
        Style::new().fg_color(Some(Color::Ansi(AnsiColor::Blue))),
    ),
    (
        "info",
        "INF",
        Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green))),
    ),
    (
        "warn",
        "WRN",
        Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow))),
    ),
    (
        "warning",
        "WRN",
        Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow))),
    ),
    (
        "error",
        "ERR",
        Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red))),
    ),
    (
        "fatal",
        "FTL",
        Style::new()
            .fg_color(Some(Color::Ansi(AnsiColor::Red)))
            .effects(Effects::BOLD),
    ),
    (
        "panic",
        "PNC",
        Style::new()
            .fg_color(Some(Color::Ansi(AnsiColor::Red)))
            .effects(Effects::BOLD),
    ),
];

/// FNV-1a, 32-bit — the hash hulog uses (`hash/fnv.New32a`).
///
/// Reproduced rather than pulled from a crate so that the colour mapping is
/// pinned in this file next to the palette it indexes. Both are covered by the
/// golden tests.
pub(crate) const fn fnv1a32(bytes: &[u8]) -> u32 {
    const OFFSET_BASIS: u32 = 0x811c_9dc5;
    const PRIME: u32 = 0x0100_0193;

    let mut hash = OFFSET_BASIS;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u32;
        hash = hash.wrapping_mul(PRIME);
        i += 1;
    }
    hash
}

/// How the `level` value should be printed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LevelTag {
    /// `Some("INF")` for a known level. `None` means "print the raw value
    /// uppercased" — the renderer does that in place, without allocating.
    pub(crate) tag: Option<&'static str>,
    /// Style for the whole `[TAG]`, brackets included. Plain for unknown levels.
    pub(crate) style: Style,
}

/// Resolved styles for one run.
///
/// Everything is precomputed in [`Theme::new`]; the per-line work is one hash
/// and one array index. In particular the truecolor -> 256 downgrade happens
/// ten times at start-up, not once per key per line.
///
/// The theme is also **the only thing that turns colour off**. An earlier
/// version left that to `anstream::AutoStream`, which strips escapes on the way
/// out — but its stripping stream runs every byte through a VT parser and
/// silently swallows bytes that are not valid UTF-8, which broke the
/// pass-through guarantee for a non-UTF-8 log line (HLD §8). So the write path
/// is now a plain pass-through and [`ColorLevel::None`] simply yields plain
/// styles, which emit nothing at all.
#[derive(Debug)]
pub(crate) struct Theme {
    palette: [Style; KEY_PALETTE.len()],
    levels: [Style; LEVELS.len()],
    timestamp: Style,
    message: Style,
}

impl Theme {
    /// Precomputes every style this run can emit, at the stream's colour depth.
    ///
    /// The truecolor -> 256 downgrade happens **here**, ten times per run, and
    /// never again: `key_style` must stay one hash plus one array read.
    ///
    /// [`ColorLevel::None`] produces plain styles throughout, so not one escape
    /// byte is written and nothing downstream has to remove any.
    pub(crate) fn new(color: ColorLevel) -> Self {
        // `Style` is `Copy`, so the arrays start plain and are filled in place —
        // no `Vec`, no allocation.
        let mut palette = [Style::new(); KEY_PALETTE.len()];
        for (slot, rgb) in palette.iter_mut().zip(KEY_PALETTE) {
            let resolved = match color {
                ColorLevel::None => continue,
                // Apple Terminal.app has no 24-bit colour, and development
                // happens on macOS: without this the palette collapses to
                // whatever the terminal guesses from an unsupported sequence.
                ColorLevel::Ansi256 => Color::Ansi256(anstyle_lossy::rgb_to_xterm(rgb)),
                ColorLevel::TrueColor => Color::Rgb(rgb),
            };
            *slot = Style::new().fg_color(Some(resolved));
        }

        // Level styles are named ANSI colours, so there is nothing to downgrade
        // — only to suppress.
        let mut levels = [Style::new(); LEVELS.len()];
        if color != ColorLevel::None {
            for (slot, (_, _, style)) in levels.iter_mut().zip(LEVELS) {
                *slot = style;
            }
        }

        let (timestamp, message) = match color {
            ColorLevel::None => (Style::new(), Style::new()),
            _ => (
                Style::new().effects(Effects::DIMMED),
                Style::new().effects(Effects::BOLD),
            ),
        };

        Self {
            palette,
            levels,
            timestamp,
            message,
        }
    }

    /// Style for a tail key: `FNV-1a(dotted path) % palette.len()`.
    ///
    /// The argument is the **full dotted path**, so `grpc.code` and `code` are
    /// different keys with different colours. Verified against hulog: `port`,
    /// `code` and `trace_id` land on entry 0, `grpc.code` and `user_id` on 8,
    /// `grpc.time_ms` on 9 (see the tests).
    pub(crate) fn key_style(&self, dotted_path: &str) -> Style {
        let index = fnv1a32(dotted_path.as_bytes()) as usize % self.palette.len();
        // `%` bounds the index by construction, so this cannot fail; using
        // `get`/`unwrap_or_default` here would only hide a future off-by-one.
        self.palette[index]
    }

    /// Tag and style for a level value. Matching is case-insensitive.
    ///
    /// ASCII-insensitive rather than Unicode-lowercasing: every entry in
    /// [`LEVELS`] is ASCII, and the Unicode path would allocate once per line
    /// to discover the same answer.
    ///
    /// v0.2 adds the `[output.levels]` remapping (pino/bunyan send `30`/`50`)
    /// in front of this lookup.
    pub(crate) fn level(&self, raw: &str) -> LevelTag {
        for (index, (name, tag, _)) in LEVELS.into_iter().enumerate() {
            // `eq_ignore_ascii_case` compares lengths first, so the eight
            // candidates cost eight length checks for a value like `30`.
            if raw.eq_ignore_ascii_case(name) {
                return LevelTag {
                    tag: Some(tag),
                    // `enumerate` over the same array bounds the index.
                    style: self.levels[index],
                };
            }
        }
        // Unknown level: the renderer prints the raw value uppercased and
        // unstyled. Dropping it or colouring it like `info` would be a lie.
        LevelTag {
            tag: None,
            style: Style::new(),
        }
    }

    /// Style of the timestamp column: faint, so it recedes.
    pub(crate) fn timestamp_style(&self) -> Style {
        self.timestamp
    }

    /// Style of the message column: bold, so it leads.
    pub(crate) fn message_style(&self) -> Style {
        self.message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published FNV-1a/32 test vectors. If these move, every key in the
    /// output changes colour at once.
    #[test]
    fn fnv1a32_matches_the_reference_vectors() {
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
        assert_eq!(fnv1a32(b"foobar"), 0xbf9c_f968);
    }

    /// Pinned key -> palette-entry pairs.
    ///
    /// The first eight were read off the real hulog binary running under a pty
    /// (`38;2;R;G;B` sequences), so this is a compatibility assertion, not a
    /// restatement of the implementation: a user switching binaries must keep
    /// seeing `trace_id` in the same colour.
    const PINNED: [(&str, usize); 10] = [
        ("port", 0),
        ("code", 0),
        ("trace_id", 0),
        ("serviceName", 1),
        ("span.kind", 1),
        ("grpc.code", 8),
        ("user_id", 8),
        ("grpc.time_ms", 9),
        ("msg", 4),
        ("level", 5),
    ];

    #[test]
    fn key_colours_match_hulog() {
        let theme = Theme::new(ColorLevel::TrueColor);
        for (key, index) in PINNED {
            let expected = Style::new().fg_color(Some(Color::Rgb(KEY_PALETTE[index])));
            assert_eq!(theme.key_style(key), expected, "key {key:?}");
        }
    }

    /// The dotted path is hashed whole, so a leaf and a nested leaf with the
    /// same name are different keys. This is invariant 2 in HLD §6.
    #[test]
    fn nesting_changes_the_colour() {
        let theme = Theme::new(ColorLevel::TrueColor);
        assert_ne!(theme.key_style("code"), theme.key_style("grpc.code"));
    }

    /// Segment-boundary sanity: `grpcStatus` shares a prefix with `grpc` but is
    /// a different key, and nothing about the colouring treats it as related.
    #[test]
    fn prefix_sharing_keys_are_unrelated() {
        let theme = Theme::new(ColorLevel::TrueColor);
        assert_ne!(theme.key_style("grpc"), theme.key_style("grpcStatus"));
    }

    #[test]
    fn ansi256_downgrade_is_applied_once_and_stays_stable() {
        let theme = Theme::new(ColorLevel::Ansi256);
        for (index, rgb) in KEY_PALETTE.into_iter().enumerate() {
            let expected =
                Style::new().fg_color(Some(Color::Ansi256(anstyle_lossy::rgb_to_xterm(rgb))));
            assert_eq!(theme.palette[index], expected);
        }
        // A downgraded palette is still ten distinct entries, otherwise the
        // whole point — telling keys apart at a glance — is lost.
        let mut seen: Vec<_> = theme.palette.iter().collect();
        seen.sort_unstable_by_key(|style| format!("{style:?}"));
        seen.dedup_by_key(|style| format!("{style:?}"));
        assert_eq!(seen.len(), KEY_PALETTE.len());
    }

    /// `ColorLevel::None` must emit **nothing**, not styles for someone else to
    /// strip. The stripping stream this used to rely on runs every byte through
    /// a VT parser and eats invalid UTF-8, so the write path is a plain
    /// pass-through now and suppression has to happen here.
    #[test]
    fn no_colour_level_produces_plain_styles_everywhere() {
        let theme = Theme::new(ColorLevel::None);
        for style in theme.palette {
            assert_eq!(style, Style::new());
        }
        for (name, _, _) in LEVELS {
            assert_eq!(theme.level(name).style, Style::new(), "level {name:?}");
        }
        assert_eq!(theme.timestamp_style(), Style::new());
        assert_eq!(theme.message_style(), Style::new());
    }

    /// …and a style that emits nothing really does write zero bytes, which is
    /// what makes dropping the stripping stream safe.
    #[test]
    fn a_plain_style_writes_no_escape_bytes() {
        let theme = Theme::new(ColorLevel::None);
        let mut out = Vec::new();
        let style = theme.key_style("port");
        style.write_to(&mut out).expect("a Vec never fails");
        style.write_reset_to(&mut out).expect("a Vec never fails");
        assert!(out.is_empty(), "got {out:?}");
    }

    #[test]
    fn level_tags_are_case_insensitive() {
        let theme = Theme::new(ColorLevel::TrueColor);
        for (raw, tag) in [
            ("trace", "TRC"),
            ("DEBUG", "DBG"),
            ("Info", "INF"),
            ("warn", "WRN"),
            ("WARNING", "WRN"),
            ("eRrOr", "ERR"),
            ("fatal", "FTL"),
            ("panic", "PNC"),
        ] {
            assert_eq!(theme.level(raw).tag, Some(tag), "level {raw:?}");
        }
    }

    #[test]
    fn warn_and_warning_share_a_three_character_tag() {
        let theme = Theme::new(ColorLevel::TrueColor);
        assert_eq!(theme.level("warn").tag, theme.level("warning").tag);
        for (_, tag, _) in LEVELS {
            assert_eq!(tag.len(), 3, "tag {tag:?} would break column alignment");
        }
    }

    #[test]
    fn unknown_level_has_no_tag_and_no_style() {
        let theme = Theme::new(ColorLevel::TrueColor);
        for raw in ["30", "notice", "", "info "] {
            let level = theme.level(raw);
            assert_eq!(level.tag, None, "level {raw:?}");
            assert_eq!(level.style, Style::new(), "level {raw:?}");
        }
    }

    #[test]
    fn fatal_and_panic_are_bold_red() {
        let theme = Theme::new(ColorLevel::TrueColor);
        let bold_red = Style::new()
            .fg_color(Some(Color::Ansi(AnsiColor::Red)))
            .effects(Effects::BOLD);
        assert_eq!(theme.level("fatal").style, bold_red);
        assert_eq!(theme.level("panic").style, bold_red);
        // `error` is red but *not* bold, so the three still read apart.
        assert_eq!(
            theme.level("error").style,
            Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)))
        );
    }

    /// hulog renders the timestamp dim and the message bold (confirmed from the
    /// captured escape sequences: `\e[2m` and `\e[1m`).
    #[test]
    fn timestamp_is_dim_and_message_is_bold() {
        let theme = Theme::new(ColorLevel::TrueColor);
        assert_eq!(
            theme.timestamp_style(),
            Style::new().effects(Effects::DIMMED)
        );
        assert_eq!(theme.message_style(), Style::new().effects(Effects::BOLD));
    }
}
