//! Timestamp parsing and formatting. The **only** module allowed to mention
//! `jiff` (HLD §2) — everything above it speaks [`TimeSettings`].
//!
//! # The two bugs being fixed
//!
//! hulog's `formatTimeFn` (`helpers.go:26`) calls `time.Parse(timeStr, layout)`
//! with the arguments swapped, so the cached fast path never matches and the
//! seven-layout loop runs on every single line. Worse, had it ever matched, it
//! would have printed a time derived from the *layout string*. Here the cache
//! is an index into [`INPUT_LAYOUTS`], it is tried first, and it is only
//! updated after a parse that actually succeeded.
//!
//! # Time zone
//!
//! Default is the machine's local zone, so the clock on screen matches the
//! clock on the wall. This differs from hulog, which kept whatever offset the
//! input carried and therefore printed UTC for a `…Z` timestamp. `time_zone =
//! "utc"` restores the old numbers; the README needs a migration note.
//!
//! # What an input timestamp is taken to mean
//!
//! | input                       | interpreted as                         |
//! |-----------------------------|----------------------------------------|
//! | `…+03:00`                   | that offset — an instant                |
//! | `…Z`                        | UTC — an instant                        |
//! | `2025-06-15 10:32:01`       | already in the display zone             |
//! | `1750000000`                | Unix seconds — an instant               |
//!
//! The third row is the interesting one. A zoneless timestamp carries no zone,
//! so there is nothing to convert *from*; guessing UTC would silently shift
//! every local-time logger by the machine's offset. Printing the clock numbers
//! unchanged is the only reading that cannot be wrong about data we do not
//! have, and it also matches what hulog showed for those shapes.

use jiff::fmt::strtime::{self, BrokenDownTime};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

use crate::error::Error;
use crate::settings::{TimeFormat, TimeSettings, TimeZoneSpec};

/// Input layouts, tried in order, mirroring hulog's `tsFormats`
/// (`helpers.go:11`) in jiff's strptime dialect.
///
/// The first two cover RFC 3339 with and without fractional seconds, which is
/// what almost every JSON logger emits; the rest are the sloppier shapes.
/// `%.f` matches an optional fractional part, and `%:z`/`Z` the offset.
///
/// Entry 2 exists because of a dialect difference: Go's `Z07:00` accepts both
/// `Z` and `+03:00`, while jiff's `%z` family parses offsets **only** and
/// rejects a bare `Z`. The literal `Z` therefore has to live in a layout of its
/// own — see [`TimeFormatter::to_zoned`], which reads that trailing `Z` as
/// "UTC" precisely because jiff leaves no offset in the parsed fields.
pub(crate) const INPUT_LAYOUTS: [&str; 7] = [
    "%Y-%m-%dT%H:%M:%S%.f%:z",
    "%Y-%m-%dT%H:%M:%S%:z",
    "%Y-%m-%dT%H:%M:%S%.fZ",
    "%Y-%m-%dT%H:%M:%S",
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%dT%H:%M:%S%.f",
];

/// Digit counts that make a bare integer an unambiguous Unix timestamp.
///
/// `1e8` seconds and `1e11` milliseconds are both 1973-03-03, and `1e11`
/// seconds and `1e14` milliseconds are both in the year 5138 — so within
/// 9..=11 digits "seconds" and 12..=14 digits "milliseconds" each cover exactly
/// the same, plausible span of history with no overlap between them.
///
/// The lower bound is what keeps this honest: a `ts` field holding `7` is a
/// counter, not 1970-01-01T00:00:07, so short integers stay raw.
const SECOND_DIGITS: std::ops::RangeInclusive<usize> = 9..=11;
const MILLI_DIGITS: std::ops::RangeInclusive<usize> = 12..=14;

/// Parses timestamps and renders the timestamp column.
///
/// Holds the resolved zone and the layout cache; created once per run.
pub(crate) struct TimeFormatter {
    /// Resolved from [`crate::settings::TimeZoneSpec`] at start-up, so an
    /// unknown IANA name fails before the first line rather than per line.
    zone: jiff::tz::TimeZone,
    format: TimeFormat,
    /// Index into [`INPUT_LAYOUTS`] that last parsed successfully.
    ///
    /// One stream almost always speaks one layout, so this turns a seven-way
    /// probe into one attempt. A miss simply rescans and re-arms the cache.
    cached_layout: Option<usize>,
}

impl TimeFormatter {
    /// Resolves the zone and validates the output format.
    ///
    /// Returns [`Error::UnknownTimeZone`] for a zone the tzdb does not have,
    /// and [`Error::BadTimeFormat`] for a strftime pattern jiff cannot use. Both
    /// are start-up failures on purpose: a pattern that fails per line would
    /// otherwise produce a million identical complaints, or worse, silently
    /// degrade to raw timestamps for the whole run.
    pub(crate) fn new(settings: &TimeSettings) -> Result<Self, Error> {
        let zone = match &settings.zone {
            // `TimeZone::system()` falls back to UTC if it cannot read the
            // system zone; that is jiff's documented behaviour and a better
            // outcome than refusing to print logs.
            TimeZoneSpec::Local => TimeZone::system(),
            TimeZoneSpec::Utc => TimeZone::UTC,
            TimeZoneSpec::Named(name) => {
                TimeZone::get(name).map_err(|_| Error::UnknownTimeZone(name.clone()))?
            }
        };

        if let TimeFormat::Strftime(pattern) = &settings.format {
            validate_pattern(pattern)?;
        }

        Ok(Self {
            zone,
            format: settings.format.clone(),
            cached_layout: None,
        })
    }

    /// `false` when `time_format = "none"` — the caller then skips the column
    /// *and* its trailing space, instead of printing an empty one.
    pub(crate) fn is_enabled(&self) -> bool {
        !matches!(self.format, TimeFormat::Hidden)
    }

    /// Appends the formatted timestamp for `raw` to `out`.
    ///
    /// Writes into a caller-owned buffer rather than returning a `String`:
    /// this runs once per line (`mem-write-over-format`). `out` is **not**
    /// cleared — the caller owns its lifecycle.
    ///
    /// Anything that fails to parse is appended verbatim. A timestamp in a
    /// shape we do not know is still information; replacing it with `?` or
    /// dropping the column would destroy it.
    pub(crate) fn format(&mut self, raw: &str, out: &mut String) {
        match self.format {
            // `is_enabled` already told the renderer to skip the column; being
            // defensive here costs one discriminant test per line.
            TimeFormat::Hidden => return,
            TimeFormat::Raw => {
                out.push_str(raw);
                return;
            }
            TimeFormat::Strftime(_) => {}
        }

        // Parsing needs `&mut self` for the cache, so it has to finish before
        // the pattern is borrowed out of `self.format`.
        let parsed = self.parse(raw);

        let TimeFormat::Strftime(pattern) = &self.format else {
            // Unreachable: the match above returned for the other two variants.
            return;
        };
        let Some(zoned) = parsed else {
            out.push_str(raw);
            return;
        };

        // jiff appends as it goes and does not roll back, so remember where the
        // timestamp starts: a half-written column would corrupt the line.
        let mark = out.len();
        if BrokenDownTime::from(&zoned)
            .format(pattern, &mut *out)
            .is_err()
        {
            out.truncate(mark);
            out.push_str(raw);
        }
    }

    /// Parses `raw` into an instant, cache first.
    ///
    /// The cache is only re-armed after a parse that actually succeeded, which
    /// is the whole difference from hulog: there, a "hit" would have formatted
    /// the layout string instead of the value.
    fn parse(&mut self, raw: &str) -> Option<Zoned> {
        // Cheaper than any strptime attempt, and it bails on the first
        // non-digit — byte 4 of `2025-06-15…`.
        if let Some(zoned) = self.parse_unix(raw) {
            return Some(zoned);
        }

        if let Some(index) = self.cached_layout {
            if let Some(zoned) = self.parse_with(index, raw) {
                return Some(zoned);
            }
        }

        for index in 0..INPUT_LAYOUTS.len() {
            if Some(index) == self.cached_layout {
                // Already tried above, and it missed.
                continue;
            }
            if let Some(zoned) = self.parse_with(index, raw) {
                self.cached_layout = Some(index);
                return Some(zoned);
            }
        }

        None
    }

    /// Tries exactly one layout. Returns `None` on any failure — a timestamp we
    /// cannot read is not an error, it is printed raw.
    fn parse_with(&self, index: usize, raw: &str) -> Option<Zoned> {
        let layout = *INPUT_LAYOUTS.get(index)?;
        // jiff's `parse` insists on consuming the whole input, so a shorter
        // layout cannot silently match a longer timestamp's prefix.
        let parsed = strtime::parse(layout, raw).ok()?;
        self.to_zoned(&parsed, layout)
    }

    /// Turns parsed fields into an instant in the display zone.
    ///
    /// See the module docs for the three readings; this is where they live.
    fn to_zoned(&self, parsed: &BrokenDownTime, layout: &str) -> Option<Zoned> {
        if parsed.offset().is_some() {
            // The input pinned an instant. Showing it in the configured zone is
            // the documented divergence from hulog (HLD §10.3).
            return Some(parsed.to_timestamp().ok()?.to_zoned(self.zone.clone()));
        }

        let civil = parsed.to_datetime().ok()?;
        if ends_with_literal_z(layout) {
            // jiff refuses to let `%z` eat a bare `Z`, so the layout carries the
            // zone instead of the parsed fields. It still means UTC.
            return Some(
                civil
                    .to_zoned(TimeZone::UTC)
                    .ok()?
                    .with_time_zone(self.zone.clone()),
            );
        }

        // No zone anywhere: read the clock numbers as already being in the
        // display zone, so they survive the round trip unchanged.
        civil.to_zoned(self.zone.clone()).ok()
    }

    /// Bare integers as Unix seconds or milliseconds.
    ///
    /// Not part of hulog, and deliberately narrow: only digits, at most one
    /// fractional dot, and a digit count in [`SECOND_DIGITS`] or
    /// [`MILLI_DIGITS`]. Everything else is left for the layout scan or printed
    /// raw. The fraction is accepted for seconds only — `1750000000.123` is a
    /// shape Python's `logging` emits, whereas fractional milliseconds are not
    /// a convention anyone writes.
    fn parse_unix(&self, raw: &str) -> Option<Zoned> {
        let (whole, fraction) = match raw.split_once('.') {
            Some((whole, fraction)) => (whole, Some(fraction)),
            None => (raw, None),
        };
        if !whole.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        if let Some(fraction) = fraction {
            if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
        }

        let timestamp = if SECOND_DIGITS.contains(&whole.len()) {
            let seconds: i64 = whole.parse().ok()?;
            Timestamp::new(seconds, fraction.map_or(0, fraction_to_nanoseconds)).ok()?
        } else if MILLI_DIGITS.contains(&whole.len()) && fraction.is_none() {
            Timestamp::from_millisecond(whole.parse().ok()?).ok()?
        } else {
            return None;
        };

        Some(timestamp.to_zoned(self.zone.clone()))
    }
}

/// Runs the start-up checks of [`TimeFormatter::new`] without keeping the
/// formatter.
///
/// This exists so the config layer can ask the same questions while the parsed
/// document still carries spans: `time_zone = "Mars/Olympus"` and
/// `time_format = "%J"` are both things only jiff can rule on, and reporting
/// them from `Renderer::new` would produce a bare `error: unknown time zone …`
/// that names neither the file nor the line it is written on.
///
/// Delegating to the constructor rather than repeating its two checks is the
/// point: a rule the config accepts and the renderer rejects would be worse
/// than no check at all.
///
/// # Errors
///
/// [`Error::UnknownTimeZone`] or [`Error::BadTimeFormat`], exactly as
/// [`TimeFormatter::new`] returns them.
pub(crate) fn check(settings: &TimeSettings) -> Result<(), Error> {
    TimeFormatter::new(settings).map(|_| ())
}

/// Rejects a strftime pattern jiff cannot format, e.g. a trailing lone `%`.
///
/// The probe is a [`Zoned`] rather than a civil datetime so that zone-dependent
/// specifiers (`%z`, `%Z`, `%Q`) validate as the real calls will use them.
fn validate_pattern(pattern: &str) -> Result<(), Error> {
    let probe = Timestamp::UNIX_EPOCH.to_zoned(TimeZone::UTC);
    let mut sink = String::new();
    BrokenDownTime::from(&probe)
        .format(pattern, &mut sink)
        .map_err(|_| Error::BadTimeFormat(pattern.to_owned()))
}

/// Does the layout end in a literal `Z` (not the `%Z` specifier)?
fn ends_with_literal_z(layout: &str) -> bool {
    let bytes = layout.as_bytes();
    match bytes.split_last() {
        Some((b'Z', rest)) => rest.last() != Some(&b'%'),
        _ => false,
    }
}

/// Fractional digits to nanoseconds: pads short fractions, truncates long ones.
fn fraction_to_nanoseconds(fraction: &str) -> i32 {
    let mut digits = fraction.bytes();
    let mut nanoseconds = 0_i32;
    // Exactly nine steps, so the result is always in `0..1_000_000_000`.
    for _ in 0..9 {
        let digit = digits.next().map_or(0, |byte| i32::from(byte - b'0'));
        nanoseconds = nanoseconds * 10 + digit;
    }
    nanoseconds
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moscow is UTC+3 year-round (no DST since 2014), so an expected string in
    /// these tests never depends on the date it is run.
    const MOSCOW: &str = "Europe/Moscow";

    fn formatter(zone: TimeZoneSpec, format: &str) -> TimeFormatter {
        let settings = TimeSettings {
            format: TimeFormat::parse(format).expect("test pattern is valid"),
            zone,
        };
        TimeFormatter::new(&settings).expect("test settings resolve")
    }

    fn rendered(formatter: &mut TimeFormatter, raw: &str) -> String {
        let mut out = String::new();
        formatter.format(raw, &mut out);
        out
    }

    // ---------------------------------------------------------------- zones

    /// The headline divergence from hulog, spelled out so it cannot drift.
    ///
    /// `hulog-bin` prints `10:32:01` for this line — it keeps the offset the
    /// input carried. We print the configured zone (HLD §10.3), so a UTC+3 user
    /// sees `13:32:01`. Asserting against Moscow rather than the machine's own
    /// zone is what stops this from passing by coincidence on a UTC box.
    #[test]
    fn utc_input_is_shown_in_the_configured_zone_not_the_input_offset() {
        let mut moscow = formatter(TimeZoneSpec::Named(MOSCOW.to_owned()), "%H:%M:%S");
        assert_eq!(rendered(&mut moscow, "2025-06-15T10:32:01Z"), "13:32:01");

        // …and `time_zone = "utc"` gives the old numbers back, which is the
        // migration advice the README will carry.
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");
        assert_eq!(rendered(&mut utc, "2025-06-15T10:32:01Z"), "10:32:01");
    }

    #[test]
    fn an_explicit_offset_is_converted_too() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");
        assert_eq!(rendered(&mut utc, "2025-06-15T10:32:01+03:00"), "07:32:01");

        let mut moscow = formatter(TimeZoneSpec::Named(MOSCOW.to_owned()), "%H:%M:%S");
        assert_eq!(
            rendered(&mut moscow, "2025-06-15T10:32:01-05:00"),
            "18:32:01"
        );
    }

    /// A date-crossing conversion, because `%H:%M:%S` alone would hide it.
    #[test]
    fn conversion_can_move_the_date() {
        let mut moscow = formatter(TimeZoneSpec::Named(MOSCOW.to_owned()), "%Y-%m-%d %H:%M:%S");
        assert_eq!(
            rendered(&mut moscow, "2025-06-15T23:30:00Z"),
            "2025-06-16 02:30:00"
        );
    }

    /// A timestamp with no zone information is not silently shifted: there is
    /// nothing to convert from, so the clock numbers come out unchanged.
    #[test]
    fn zoneless_input_keeps_its_clock_numbers() {
        for zone in [
            TimeZoneSpec::Utc,
            TimeZoneSpec::Named(MOSCOW.to_owned()),
            TimeZoneSpec::Local,
        ] {
            let mut formatter = formatter(zone, "%Y-%m-%d %H:%M:%S");
            assert_eq!(
                rendered(&mut formatter, "2025-06-15 10:32:01"),
                "2025-06-15 10:32:01"
            );
            assert_eq!(
                rendered(&mut formatter, "2025-06-15T10:32:01"),
                "2025-06-15 10:32:01"
            );
        }
    }

    #[test]
    fn local_resolves_to_the_system_zone() {
        let local = formatter(TimeZoneSpec::Local, "%H:%M:%S");
        assert_eq!(local.zone.iana_name(), TimeZone::system().iana_name());
    }

    #[test]
    fn an_unknown_zone_fails_at_start_up() {
        let settings = TimeSettings {
            format: TimeFormat::Strftime("%H:%M:%S".to_owned()),
            zone: TimeZoneSpec::Named("Nowhere/Nothing".to_owned()),
        };
        assert!(matches!(
            TimeFormatter::new(&settings),
            Err(Error::UnknownTimeZone(name)) if name == "Nowhere/Nothing"
        ));
    }

    // --------------------------------------------------------------- layouts

    /// Every layout in the table parses the shape it exists for. A layout that
    /// silently stopped matching would cost only formatting, so nothing else
    /// would notice.
    #[test]
    fn every_layout_parses_its_shape() {
        let samples = [
            ("2025-06-15T10:32:01.123456789+03:00", "07:32:01"),
            ("2025-06-15T10:32:01+03:00", "07:32:01"),
            ("2025-06-15T10:32:01.250Z", "10:32:01"),
            ("2025-06-15T10:32:01", "10:32:01"),
            ("2025-06-15 10:32:01.250", "10:32:01"),
            ("2025-06-15 10:32:01", "10:32:01"),
            ("2025-06-15T10:32:01.250", "10:32:01"),
        ];
        assert_eq!(samples.len(), INPUT_LAYOUTS.len());
        for (raw, expected) in samples {
            // A fresh formatter per sample, so the cache cannot carry an answer
            // from the previous one.
            let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");
            assert_eq!(rendered(&mut utc, raw), expected, "input {raw:?}");
        }
    }

    #[test]
    fn sub_second_precision_survives() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S%.f");
        assert_eq!(
            rendered(&mut utc, "2025-06-15T10:32:01.123456789Z"),
            "10:32:01.123456789"
        );
    }

    /// The `Z` must be read as UTC even though jiff leaves no offset behind for
    /// the literal-`Z` layout. Getting this wrong is invisible under
    /// `time_zone = "utc"` and wrong by hours everywhere else.
    #[test]
    fn trailing_z_means_utc_not_zoneless() {
        let mut moscow = formatter(TimeZoneSpec::Named(MOSCOW.to_owned()), "%H:%M:%S");
        assert_eq!(rendered(&mut moscow, "2025-06-15T10:32:01Z"), "13:32:01");
        // The same civil time without the `Z` is *not* shifted.
        let mut moscow = formatter(TimeZoneSpec::Named(MOSCOW.to_owned()), "%H:%M:%S");
        assert_eq!(rendered(&mut moscow, "2025-06-15T10:32:01"), "10:32:01");
    }

    #[test]
    fn literal_z_detection_ignores_the_percent_z_specifier() {
        assert!(ends_with_literal_z("%Y-%m-%dT%H:%M:%S%.fZ"));
        assert!(!ends_with_literal_z("%Y-%m-%dT%H:%M:%S%Z"));
        assert!(!ends_with_literal_z("%Y-%m-%dT%H:%M:%S%:z"));
        assert!(!ends_with_literal_z(""));
    }

    // ----------------------------------------------------------------- cache

    #[test]
    fn the_cache_starts_empty_and_arms_on_the_first_hit() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");
        assert_eq!(utc.cached_layout, None);
        assert_eq!(rendered(&mut utc, "2025-06-15T10:32:01Z"), "10:32:01");
        assert_eq!(utc.cached_layout, Some(2));
    }

    /// Proof that the cache is consulted **before** the scan, not merely kept.
    ///
    /// `2025-06-15T10:32:01` is matched by layout 3 and by layout 6, and a scan
    /// always finds 3 first. Arming the cache with 6 and seeing it survive a
    /// successful parse means the scan never ran.
    #[test]
    fn a_primed_cache_wins_over_the_scan_order() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");
        utc.cached_layout = Some(6);
        assert_eq!(rendered(&mut utc, "2025-06-15T10:32:01"), "10:32:01");
        assert_eq!(utc.cached_layout, Some(6), "the scan re-armed the cache");

        // Same shape, the other overlapping pair: 4 and 5 both match, scan
        // order finds 4.
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");
        utc.cached_layout = Some(5);
        assert_eq!(rendered(&mut utc, "2025-06-15 10:32:01"), "10:32:01");
        assert_eq!(utc.cached_layout, Some(5));
    }

    /// hulog's actual bug: with the arguments swapped, a cache hit would have
    /// formatted a time derived from the *layout string*, so every line after
    /// the first would print the same wrong value. Two different timestamps of
    /// one shape, through one formatter, must give two different answers.
    #[test]
    fn a_cache_hit_formats_the_value_not_the_layout() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%Y-%m-%d %H:%M:%S");
        assert_eq!(
            rendered(&mut utc, "2025-06-15T10:32:01Z"),
            "2025-06-15 10:32:01"
        );
        let armed = utc.cached_layout;
        assert_eq!(armed, Some(2));

        for (raw, expected) in [
            ("2025-06-15T23:59:58Z", "2025-06-15 23:59:58"),
            ("1999-01-02T03:04:05Z", "1999-01-02 03:04:05"),
            ("2038-01-19T03:14:07Z", "2038-01-19 03:14:07"),
        ] {
            assert_eq!(rendered(&mut utc, raw), expected, "input {raw:?}");
            assert_eq!(
                utc.cached_layout, armed,
                "input {raw:?} disturbed the cache"
            );
        }
    }

    #[test]
    fn the_cache_re_arms_when_the_shape_changes() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");

        assert_eq!(rendered(&mut utc, "2025-06-15T10:32:01Z"), "10:32:01");
        assert_eq!(utc.cached_layout, Some(2));

        assert_eq!(rendered(&mut utc, "2025-06-15 11:00:00"), "11:00:00");
        assert_eq!(utc.cached_layout, Some(4));

        // …and back, so a stream interleaving two producers stays correct.
        assert_eq!(rendered(&mut utc, "2025-06-15T12:00:00Z"), "12:00:00");
        assert_eq!(utc.cached_layout, Some(2));
    }

    /// An armed cache must not turn an unparseable timestamp into a parsed one,
    /// and must not be cleared by it either.
    #[test]
    fn an_armed_cache_survives_an_unparseable_line() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");
        assert_eq!(rendered(&mut utc, "2025-06-15T10:32:01Z"), "10:32:01");
        assert_eq!(rendered(&mut utc, "yesterday"), "yesterday");
        assert_eq!(utc.cached_layout, Some(2));
        assert_eq!(rendered(&mut utc, "2025-06-15T10:32:02Z"), "10:32:02");
    }

    // ----------------------------------------------------------- unix epochs

    #[test]
    fn unix_seconds_and_milliseconds_are_recognised() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%Y-%m-%d %H:%M:%S");
        assert_eq!(rendered(&mut utc, "1750000000"), "2025-06-15 15:06:40");
        assert_eq!(rendered(&mut utc, "1750000000123"), "2025-06-15 15:06:40");
    }

    #[test]
    fn fractional_unix_seconds_keep_their_precision() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S%.f");
        assert_eq!(rendered(&mut utc, "1750000000.5"), "15:06:40.5");
        assert_eq!(rendered(&mut utc, "1750000000.123456"), "15:06:40.123456");
        // More than nanosecond precision is truncated, not rejected.
        assert_eq!(
            rendered(&mut utc, "1750000000.1234567891"),
            "15:06:40.123456789"
        );
    }

    /// Numbers that are not unambiguously timestamps stay raw. A `ts` field
    /// holding a counter must not become 1970.
    #[test]
    fn ambiguous_numbers_are_printed_raw() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");
        for raw in [
            "7",
            "42",
            "20250615",           // a date, not an epoch
            "12345678",           // 8 digits: below the seconds floor
            "175000000000000000", // 18 digits: past the millisecond ceiling
            "1750000000123.5",    // fractional milliseconds are not a convention
            "-1750000000",        // signed epochs are not worth guessing at
            "1750000000.",
            "",
        ] {
            assert_eq!(rendered(&mut utc, raw), raw, "input {raw:?}");
        }
    }

    #[test]
    fn unix_timestamps_are_converted_like_any_other_instant() {
        let mut moscow = formatter(TimeZoneSpec::Named(MOSCOW.to_owned()), "%H:%M:%S");
        assert_eq!(rendered(&mut moscow, "1750000000"), "18:06:40");
    }

    // ------------------------------------------------------- output policies

    #[test]
    fn unparsed_timestamps_are_passed_through_verbatim() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");
        for raw in [
            "yesterday",
            "15/06/2025 10:32:01",
            "2025-06-15T10:32:01+0300", // no-colon offsets are not in the table
            "2025-13-45T99:99:99Z",     // well shaped, impossible values
            "",
        ] {
            assert_eq!(rendered(&mut utc, raw), raw, "input {raw:?}");
        }
    }

    #[test]
    fn raw_format_never_parses_anything() {
        let mut utc = formatter(TimeZoneSpec::Utc, "raw");
        assert!(utc.is_enabled());
        assert_eq!(
            rendered(&mut utc, "2025-06-15T10:32:01Z"),
            "2025-06-15T10:32:01Z"
        );
        assert_eq!(utc.cached_layout, None);
    }

    #[test]
    fn none_format_writes_nothing() {
        let mut utc = formatter(TimeZoneSpec::Utc, "none");
        assert!(!utc.is_enabled());
        assert_eq!(rendered(&mut utc, "2025-06-15T10:32:01Z"), "");
    }

    /// `format` appends. The renderer reuses one scratch buffer for the whole
    /// line, so clearing it here would eat whatever was written before.
    #[test]
    fn format_appends_and_does_not_clear() {
        let mut utc = formatter(TimeZoneSpec::Utc, "%H:%M:%S");
        let mut out = String::from("before ");
        utc.format("2025-06-15T10:32:01Z", &mut out);
        utc.format(" / unparseable", &mut out);
        assert_eq!(out, "before 10:32:01 / unparseable");
    }

    #[test]
    fn a_broken_pattern_fails_at_start_up() {
        let settings = TimeSettings {
            format: TimeFormat::Strftime("%H:%M:%".to_owned()),
            zone: TimeZoneSpec::Utc,
        };
        assert!(matches!(
            TimeFormatter::new(&settings),
            Err(Error::BadTimeFormat(pattern)) if pattern == "%H:%M:%"
        ));
    }

    #[test]
    fn zone_aware_patterns_are_accepted() {
        let mut moscow = formatter(TimeZoneSpec::Named(MOSCOW.to_owned()), "%H:%M:%S %:z");
        assert_eq!(
            rendered(&mut moscow, "2025-06-15T10:32:01Z"),
            "13:32:01 +03:00"
        );
    }

    #[test]
    fn the_default_pattern_is_the_hulog_one() {
        let mut utc = formatter(TimeZoneSpec::Utc, crate::settings::DEFAULT_TIME_FORMAT);
        assert_eq!(rendered(&mut utc, "2025-06-15T10:32:01Z"), "10:32:01");
    }
}
