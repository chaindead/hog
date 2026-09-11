//! The Go benchmark, re-run against the Rust renderer.
//!
//! `hulog`'s `main_test.go` measures `processLine` on five lines; those five
//! lines and that configuration are reproduced here verbatim, because "the Go
//! version costs 804–2056 ns/line" is the number this rewrite is judged against
//! (HLD §1). Anything else measured here would not be comparable.
//!
//! # What is inside the timed region
//!
//! One call to [`Renderer::render`] plus the `clear()` of the output buffer —
//! exactly the work `pipeline.rs` does per line, minus the `read_until` that
//! produced the line. Parsing, flattening, exclusion, sorting, timestamp
//! formatting and quoting are all in there.
//!
//! The buffer is reused across iterations, which is not quite what Go does: Go
//! returns a fresh `string` from a `strings.Builder` and pays an allocation per
//! line. Reuse is the honest thing to measure, though, because reuse is what
//! the real pipeline does (`mem-reuse-collections`) — it writes into one
//! `BufWriter` for the life of the process and never builds a `String` per
//! line. The comparison is therefore "cost per line in each program's own hot
//! path", not "same code, two languages".
//!
//! # Why colour is off and the zone is UTC
//!
//! * `ColorLevel::None` matches the Go run: `fatih/color` disables itself when
//!   stdout is not a terminal, and `go test` pipes it. Styles are still looked
//!   up per key here — the FNV-1a hash and the palette index are computed
//!   either way — only the escape bytes are not emitted.
//! * `TimeZoneSpec::Utc`, not the shipped default of `Local`, so the numbers do
//!   not depend on the machine's `TZ` and so the conversion matches what Go
//!   did with these `…Z` inputs (it kept the input offset — HLD §10.3).
//!   Measured: `Local` costs nothing extra — 441 ns against 438 ns, inside the
//!   noise — because jiff resolves the zone once at start-up.
//!
//! # Measured, macOS arm64 (M-series), `cargo bench`
//!
//! Go numbers are the ones recorded in `main_test.go` on the same class of
//! machine. Run-to-run spread here is a couple of percent, so read the ratios,
//! not the last digit.
//!
//! | line       |     Go |   hog | ratio |
//! |------------|-------:|------:|------:|
//! | Simple     |  805ns | 438ns | 1.8× |
//! | Exclude    |  920ns | 547ns | 1.7× |
//! | Nested     | 1399ns | 822ns | 1.7× |
//! | ManyFields | 2056ns |1084ns | 1.9× |
//! | NonJSON    |    3ns |   6ns | 0.5× |
//!
//! `NonJSON` is the one row where Go looks better, and it is not a like-for-
//! like row: Go's `processLine` *returns its own argument* for a non-JSON line
//! and the copy happens later, in the caller. Here the 6 ns is the copy — 47
//! bytes into the output buffer at ~6.8 GiB/s. Nothing is being done slowly;
//! the two functions simply draw the boundary in different places, and a
//! pass-through line costs neither program anything that matters.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use hog::render::{ColorLevel, Renderer};
use hog::settings::{ExcludeSet, FieldNames, Settings, TimeFormat, TimeSettings, TimeZoneSpec};

/// The five lines from `hulog/main_test.go`, byte for byte.
///
/// A slice rather than a map, because Go's `map` iteration order made its own
/// report come out shuffled and there is no reason to inherit that.
const LINES: [(&str, &str); 5] = [
    (
        "Simple",
        r#"{"ts":"2025-06-15T10:32:01Z","level":"info","msg":"server started","port":8080}"#,
    ),
    (
        "Exclude",
        r#"{"ts":"2025-06-15T10:32:01Z","level":"info","msg":"server started","port":8080,"ex":1,"ex2":1,"ex3":1}"#,
    ),
    (
        "Nested",
        r#"{"ts":"2025-06-15T10:32:01Z","level":"error","msg":"request failed","http":{"method":"POST","path":"/api/v1/users","status":500},"latency":0.235}"#,
    ),
    (
        "ManyFields",
        r#"{"ts":"2025-06-15T10:32:01Z","level":"warn","msg":"slow query","db":"postgres","query":"SELECT *","duration":1.23,"rows":1000,"user":"admin","host":"db-1","region":"us-east-1","trace_id":"abc123"}"#,
    ),
    ("NonJSON", "this is just a plain text log line with no JSON"),
];

/// `benchCfg` from the Go test: the three column fields by their plain names,
/// three excluded keys, and `15:04:05` in strftime spelling.
fn bench_settings() -> Settings {
    Settings {
        fields: FieldNames {
            ts: vec!["ts".to_owned()],
            level: vec!["level".to_owned()],
            msg: vec!["msg".to_owned()],
        },
        time: TimeSettings {
            format: TimeFormat::Strftime("%H:%M:%S".to_owned()),
            zone: TimeZoneSpec::Utc,
        },
        exclude: ExcludeSet::new(["ex", "ex2", "ex3"].map(str::to_owned)),
        sort_keys: true,
        ..Settings::default()
    }
}

fn renderer() -> Renderer {
    Renderer::new(bench_settings(), ColorLevel::None).expect("bench settings are valid")
}

/// Renders every line once and checks the result.
///
/// Without this, a renderer that silently turned into a no-op — an exclusion
/// that ate everything, a parse that started failing — would still benchmark,
/// and would benchmark beautifully. Run as part of `cargo bench` rather than as
/// a `#[test]`, because the thing worth pinning is that *these* settings on
/// *these* lines still do the work.
fn assert_the_bench_is_not_measuring_nothing() {
    let expected = [
        "10:32:01 [INF] server started port=8080\n",
        "10:32:01 [INF] server started port=8080\n",
        "10:32:01 [ERR] request failed http.method=POST http.path=/api/v1/users http.status=500 latency=0.235\n",
        "10:32:01 [WRN] slow query db=postgres duration=1.23 host=db-1 query=\"SELECT *\" region=us-east-1 rows=1000 trace_id=abc123 user=admin\n",
        "this is just a plain text log line with no JSON\n",
    ];

    let mut renderer = renderer();
    for ((name, line), want) in LINES.iter().zip(expected) {
        let mut out = Vec::new();
        renderer.render(line, &mut out).expect("a Vec never fails");
        let got = String::from_utf8(out).expect("output stays UTF-8");
        assert_eq!(got, want, "{name} does not render what the bench assumes");
    }
}

fn render(criterion: &mut Criterion) {
    assert_the_bench_is_not_measuring_nothing();

    let mut group = criterion.benchmark_group("render");
    for (name, line) in LINES {
        // Bytes, so the report carries throughput as well: HLD §1 quotes the
        // prototype at 403 MiB/s, and ns/line alone cannot be checked against
        // that.
        group.throughput(Throughput::Bytes(line.len() as u64));
        group.bench_function(name, |bencher| {
            // One renderer per benchmark, built outside the timed region: its
            // buffers and its timestamp layout cache are meant to live for the
            // whole run, and rebuilding them per iteration would measure
            // start-up instead. The cache warms on the first iteration, which
            // is exactly what happens on line 2 of a real stream.
            let mut renderer = renderer();
            let mut out = Vec::with_capacity(256);
            bencher.iter(|| {
                out.clear();
                renderer
                    .render(black_box(line), &mut out)
                    .expect("a Vec never fails");
                // Keeps the optimiser from deciding the bytes are unread and
                // deleting the writes that produced them.
                black_box(out.len())
            });
        });
    }
    group.finish();
}

criterion_group!(benches, render);
criterion_main!(benches);
