//! Golden rendering: every file in `tests/corpus/` run through the real binary
//! and compared against a recorded snapshot.
//!
//! # Why the process and not [`hog::render::Renderer`]
//!
//! Three items of the HLD §8 corpus — CRLF endings, a final line without `\n`,
//! and invalid UTF-8 — are decided in `input.rs`, not in the renderer, and
//! `Input` is `pub(crate)` with a `#[cfg(test)]` constructor. Only the assembled
//! process shows the reader and the renderer agreeing, which is also the pairing
//! that broke before (the write path used to swallow non-UTF-8 bytes while every
//! unit test passed). `render::mod` keeps its own unit tests for the shapes that
//! do not need a process.
//!
//! # Reading a snapshot
//!
//! Snapshots are taken with `--color never`, so what is in the file is what a
//! user sees. Two bytes cannot live in a snapshot file legibly, so [`readable`]
//! rewrites them — and the two spellings are the point of several cases:
//!
//! | in the snapshot | means                                                |
//! |-----------------|------------------------------------------------------|
//! | `\x1b`          | hog **escaped** an ESC — four literal characters      |
//! | `\e`            | a **raw** ESC byte reached stdout (pass-through only) |
//! | `\xNN`          | a byte that is not valid UTF-8, passed through        |
//! | `\r`            | a raw CR byte (`\r` in a value is hog's own escape)   |
//!
//! # Time zone
//!
//! `--timezone utc` on every run. The default is the machine's local zone
//! (HLD §10.3), so a snapshot taken any other way would only match in one place.
//!
//! # Regenerating
//!
//! `INSTA_UPDATE=always cargo test --test render_golden`, then **read the diff**:
//! these snapshots are the render contract of HLD §6, and a change to one of
//! them is a change to what `hog` promises.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use assert_cmd::cargo::CommandCargoExt as _;

/// Where the corpus lives. `CARGO_MANIFEST_DIR` rather than a relative path:
/// the working directory of a test binary is not contractual.
const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus");

/// Arguments shared by every golden run. `never` keeps the snapshots readable,
/// `utc` keeps the timestamp column reproducible off this machine.
const BASE: &[&str] = &["--color", "never", "--timezone", "utc"];

/// The byte a rendered line must never contain.
const ESC: u8 = 0x1b;

/// The reference line from HLD §1.
const REFERENCE: &str = concat!(
    r#"{"ts":"2025-06-15T10:32:01Z","level":"info","msg":"server started","#,
    r#""port":8080,"grpc":{"code":"OK","time_ms":1.5}}"#,
);

// ---------------------------------------------------------------- the corpus

/// Every corpus file, in a fixed order so a failure always reports the same way.
fn corpus_files() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(CORPUS)
        .expect("tests/corpus must exist")
        .map(|entry| entry.expect("corpus entry must be readable").path())
        .filter(|path| path.is_file())
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "the corpus must not be empty");
    paths
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .expect("corpus file names are UTF-8")
        .to_owned()
}

fn read_corpus(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(CORPUS).join(name)).expect("corpus file must be readable")
}

// ------------------------------------------------------------- running `hog`

fn command(args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("hog").expect("the binary is built by `cargo test`");
    // None of these may leak in from the developer's shell: the first four
    // decide colour, and the last two decide which config file is read. A
    // golden corpus rendered through somebody's personal `exclude` list is not
    // a golden corpus. `$HOME` is an absolute path with nothing in it, which
    // discovery treats as the ordinary "no config yet" case.
    let no_config_home =
        std::env::temp_dir().join(format!("hog-golden-no-config-{}", std::process::id()));
    command
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("COLORTERM")
        .env_remove("HOG_CONFIG")
        .env("HOME", &no_config_home)
        .args(args);
    command
}

/// Runs a prepared command over `input`, returning its raw stdout.
///
/// Anything on stderr, or any exit code but 0, fails the test: a golden run is
/// supposed to be the boring path, and a warning that appeared unnoticed would
/// be a change nobody recorded.
fn run(mut command: Command, input: &[u8]) -> Vec<u8> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hog must start");

    // Every corpus file is well under a pipe buffer, so one write cannot
    // deadlock against a reader we have not started yet.
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(input)
        .expect("hog must accept the input");

    let out = child.wait_with_output().expect("hog must finish");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
    out.stdout
}

fn hog(args: &[&str], input: &[u8]) -> Vec<u8> {
    run(command(args), input)
}

/// The same, with `COLORTERM` set: it is what decides truecolor against the
/// 256-colour downgrade, so a colour expectation must pin it rather than
/// inherit whatever the developer's terminal claims.
fn hog_truecolor(args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut command = command(args);
    command.env("COLORTERM", "truecolor");
    run(command, input)
}

/// A golden run: the base arguments plus whatever the case adds.
fn golden(extra: &[&str], input: &[u8]) -> String {
    let mut args = BASE.to_vec();
    args.extend_from_slice(extra);
    readable(&hog(&args, input))
}

// --------------------------------------------------------------- readability

/// Makes raw output safe and legible inside a snapshot file.
///
/// Only three byte classes are rewritten, and each of them is a case a snapshot
/// otherwise could not carry at all — see the table in the module docs. Valid,
/// printable UTF-8 is left exactly as `hog` wrote it.
fn readable(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut rest = bytes;

    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                push_text(&mut out, text);
                break;
            }
            Err(err) => {
                let (valid, invalid) = rest.split_at(err.valid_up_to());
                push_text(
                    &mut out,
                    std::str::from_utf8(valid).expect("valid_up_to ends on a boundary"),
                );
                // `None` means "the rest of the input is an incomplete
                // sequence", so there is nothing left to re-synchronise on.
                let bad = err.error_len().unwrap_or(invalid.len());
                for &byte in &invalid[..bad] {
                    let _ = write!(out, "\\x{byte:02x}");
                }
                rest = &invalid[bad..];
            }
        }
    }

    out
}

fn push_text(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            // A raw ESC: legible as `\e`, and unmistakable next to hog's own
            // four-character `\x1b` escape.
            '\x1b' => out.push_str("\\e"),
            // A raw CR would be invisible in the file and might not survive a
            // checkout on another platform.
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
}

// ================================================================== the tests

/// The whole corpus, one snapshot per file.
///
/// Suffix-per-file mirrors what `insta::glob!` produces (HLD §2 asks for it),
/// so enabling insta's `glob` feature later is a swap of this loop for the
/// macro with no snapshot churn.
#[test]
fn corpus_renders_as_recorded() {
    for path in corpus_files() {
        let input = std::fs::read(&path).expect("corpus file must be readable");
        let output = golden(&[], &input);

        insta::with_settings!({
            snapshot_suffix => file_name(&path),
            input_file => &path,
            description => format!("hog {}", BASE.join(" ")),
            omit_expression => true,
        }, {
            insta::assert_snapshot!(output);
        });
    }
}

/// HLD §6, invariant 3, as a matrix: one corpus file under the three rules of
/// the table, plus a comma-separated list that hides every field of a line.
///
/// The baseline — the same file with no exclusions — is the `exclude.jsonl`
/// snapshot from [`corpus_renders_as_recorded`]; read them side by side.
#[test]
fn exclusions_prune_whole_subtrees() {
    let input = read_corpus("exclude.jsonl");

    for (suffix, args) in [
        ("grpc", ["-e", "grpc"].as_slice()),
        ("grpc.request", &["-e", "grpc.request"]),
        ("grpc.request.deadline", &["-e", "grpc.request.deadline"]),
        // Comma-separated, and enough of them that the third line keeps
        // nothing: a line whose every field is hidden renders **empty**. It
        // must not fall back to echoing the source, which would put the hidden
        // fields back on the screen. The last line is deliberately left intact
        // — insta trims trailing blank lines, so an empty line has to have
        // something after it to be visible in the snapshot at all.
        ("several", &["-e", "grpc,span,trace_id,serviceName"]),
    ] {
        let output = golden(args, &input);

        insta::with_settings!({
            snapshot_suffix => suffix,
            input_file => Path::new(CORPUS).join("exclude.jsonl"),
            description => format!("hog {} {}", BASE.join(" "), args.join(" ")),
            omit_expression => true,
        }, {
            insta::assert_snapshot!(output);
        });
    }
}

/// The one column of the HLD §6 table that a prefix match would get wrong,
/// spelled out where it can be read without opening a snapshot.
#[test]
fn an_exclusion_matches_whole_segments_not_string_prefixes() {
    let line = br#"{"grpc":{"code":"OK","request":{"deadline":"1s"}},"grpcStatus":2,"grpc_id":3}"#;

    assert_eq!(
        golden(&["-e", "grpc"], line),
        "grpcStatus=2 grpc_id=3\n",
        "`-e grpc` must prune the subtree and spare the look-alikes"
    );
    assert_eq!(
        golden(&["-e", "grpc.request"], line),
        "grpc.code=OK grpcStatus=2 grpc_id=3\n"
    );
    assert_eq!(
        golden(&["-e", "grpc.request.deadline"], line),
        "grpc.code=OK grpcStatus=2 grpc_id=3\n"
    );
}

/// A line with nothing left to show renders as an empty line. Falling back to
/// the pass-through path here would print the very field the user hid.
#[test]
fn a_fully_excluded_line_renders_empty_rather_than_echoing_itself() {
    assert_eq!(golden(&["-e", "a"], b"{\"a\":1}\n"), "\n");
    assert_eq!(
        golden(&["-e", "grpc"], b"{\"grpc\":{\"code\":\"OK\"}}\n"),
        "\n"
    );
}

// ------------------------------------------------------------- line fidelity

/// One input line in, exactly one output line out — for every corpus file.
///
/// This is the invariant `hog | grep` and `hog | head` rest on (HLD §8), and it
/// is the one a snapshot cannot state, only illustrate.
#[test]
fn every_input_line_produces_exactly_one_output_line() {
    for path in corpus_files() {
        let input = std::fs::read(&path).expect("corpus file must be readable");
        let output = hog(BASE, &input);
        let name = file_name(&path);

        assert_eq!(
            count_lines(&output),
            count_lines(&input),
            "line count changed for {name}"
        );
        assert!(
            output.is_empty() || output.ends_with(b"\n"),
            "{name}: output must end with a newline"
        );
    }
}

/// Lines as the reader counts them: a final fragment without a `\n` is a line.
fn count_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    // `split` yields one more piece than there are newlines, so a trailing
    // newline has to give its empty last piece back.
    let pieces = bytes.split(|byte| *byte == b'\n').count();
    if bytes.ends_with(b"\n") {
        pieces - 1
    } else {
        pieces
    }
}

/// CRLF is the pty's doing (`ssh -tt` adds ONLCR in v0.3), not the log's, so it
/// must not change one byte of the output.
#[test]
fn crlf_input_renders_exactly_like_lf_input() {
    let crlf = read_corpus("crlf.txt");

    let mut lf = Vec::with_capacity(crlf.len());
    let mut bytes = crlf.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'\r' && bytes.peek() == Some(&b'\n') {
            continue;
        }
        lf.push(byte);
    }
    assert!(lf.len() < crlf.len(), "the fixture must really be CRLF");

    assert_eq!(hog(BASE, &crlf), hog(BASE, &lf));
}

/// Exactly **one** trailing CR is the pty's; a second one is data and survives.
#[test]
fn a_second_carriage_return_is_data() {
    assert_eq!(hog(BASE, b"plain\r\r\n"), b"plain\r\n");
}

/// A producer killed mid-line still wrote a line, and it must be rendered.
#[test]
fn a_final_line_without_a_newline_is_rendered_and_gains_one() {
    let input = read_corpus("no_final_newline.txt");
    assert!(!input.ends_with(b"\n"), "the fixture must really lack it");

    let output = hog(BASE, &input);
    assert!(output.ends_with(b"\n"), "hog terminates the line it prints");
    assert_eq!(
        String::from_utf8(output).expect("this fixture is UTF-8"),
        "[INF] first line\n[ERR] the last line has no trailing newline a=1\n"
    );
}

/// A log line is not required to be UTF-8, and the bytes must reach the fd
/// exactly as they came — snapshots go through [`readable`], so the byte-level
/// promise is asserted here instead.
#[test]
fn invalid_utf8_passes_through_byte_for_byte() {
    let output = hog(BASE, &read_corpus("invalid_utf8.bin"));

    assert_eq!(
        output,
        b"[INF] before the bad bytes\n\
          \xff\xfe raw bytes survive\n\
          [INF] after the bad bytes\n\
          {\"msg\":\"\xff inside an otherwise fine json line\"}\n\
          caf\xe9 latin-1 never reaches the parser\n"
    );
}

// -------------------------------------------------------- escape containment

/// A rendered line can never carry a raw ESC, whatever the producer put in it:
/// an unescaped one repaints everything the reader sees from there on.
///
/// The pass-through path is deliberately exempt — a line hog does not parse is
/// echoed byte for byte (HLD §8) — so `passthrough.txt` is excluded here, and
/// its raw ESC is visible as `\e` in that file's snapshot.
#[test]
fn no_rendered_line_can_smuggle_an_escape_to_the_terminal() {
    for path in corpus_files() {
        let name = file_name(&path);
        if name == "passthrough.txt" {
            continue;
        }
        let output = hog(
            BASE,
            &std::fs::read(&path).expect("corpus file must be readable"),
        );
        assert!(
            !output.contains(&ESC),
            "{name}: a raw ESC reached stdout: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

/// …and the escaping is visible rather than lossy: the bytes are still there,
/// spelled out.
#[test]
fn an_escape_sequence_is_spelled_out_not_dropped() {
    assert_eq!(
        golden(&[], br#"{"msg":"boom","evil":"\u001b[31mRED"}"#),
        "boom evil=\"\\x1b[31mRED\"\n"
    );
}

/// A `\n` inside a value must not be able to forge a second log line.
#[test]
fn a_newline_in_a_value_cannot_forge_a_line() {
    let output = golden(
        &[],
        br#"{"msg":"real","fake":"\nlevel=error msg=injected"}"#,
    );
    assert_eq!(output, "real fake=\"\\nlevel=error msg=injected\"\n");
}

// ------------------------------------------------------------------- columns

/// A part costs a separator only when it prints something.
///
/// "Absent" is about the rendered characters, not about the field: an empty
/// timestamp and an empty message are both in the line and both render nothing,
/// so neither may leave a space behind. Leaving one is the same defect as
/// hulog's `10:32:01 [INF]  port=8080`, only at the other end of the line.
#[test]
fn a_column_that_prints_nothing_costs_no_separator() {
    assert_eq!(golden(&[], br#"{"level":"info","msg":""}"#), "[INF]\n");
    assert_eq!(golden(&[], br#"{"ts":"","msg":"hi"}"#), "hi\n");
    assert_eq!(
        golden(&[], br#"{"ts":"","msg":"","port":8080}"#),
        "port=8080\n"
    );
    // Genuinely absent parts cost nothing either.
    assert_eq!(
        golden(&[], br#"{"level":"info","port":8080}"#),
        "[INF] port=8080\n"
    );
    // A message of one space is data, not an empty column.
    assert_eq!(golden(&[], br#"{"level":"info","msg":" "}"#), "[INF]  \n");
    // An empty *tail* value keeps its field: the key makes it visible.
    assert_eq!(
        golden(&[], br#"{"level":"info","note":""}"#),
        "[INF] note=\"\"\n"
    );
}

// -------------------------------------------------------------------- colour

/// `--color always` really does emit ANSI, and the exact sequences are pinned:
/// the palette and the FNV-1a mapping are a compatibility contract with hulog
/// (HLD §6, invariant 2), not a style choice.
///
/// `COLORTERM` decides truecolor against the 256-colour downgrade, so the test
/// sets it instead of inheriting whatever the developer's terminal says.
#[test]
fn color_always_emits_ansi() {
    let raw = hog_truecolor(
        &["--color", "always", "--timezone", "utc"],
        REFERENCE.as_bytes(),
    );
    assert!(raw.contains(&ESC), "no escapes at all in {raw:?}");

    insta::with_settings!({
        description => "hog --color always --timezone utc, with COLORTERM=truecolor",
        omit_expression => true,
    }, {
        insta::assert_snapshot!(readable(&raw));
    });
}

/// The same input twice, in two processes, must be byte-identical: a key's
/// colour comes from the key alone — not the position, the neighbours, the
/// order of appearance, or the run.
#[test]
fn key_colour_is_stable_between_runs() {
    let coloured = |input: &str| {
        let out = hog_truecolor(
            &["--color", "always", "--timezone", "utc"],
            input.as_bytes(),
        );
        String::from_utf8(out).expect("output stays UTF-8")
    };

    let first = coloured(REFERENCE);
    assert_eq!(
        first,
        coloured(REFERENCE),
        "two runs must agree byte for byte"
    );

    // …and the same key keeps its colour in a line with different neighbours,
    // in a different position, in a different process.
    let second = coloured(r#"{"a":1,"port":9090,"zz":{"deep":true},"trace_id":"x"}"#);

    let port = sgr_before(&first, "port").expect("the reference line has a `port` key");
    assert_eq!(
        Some(port.as_str()),
        sgr_before(&second, "port").as_deref(),
        "`port` changed colour between lines"
    );
    assert_ne!(
        port,
        sgr_before(&first, "grpc.code").expect("the reference line has `grpc.code`"),
        "a nested key hashes differently and must not share the colour"
    );
}

/// The SGR sequence immediately before `key` in a coloured line.
fn sgr_before(text: &str, key: &str) -> Option<String> {
    let at = text.find(key)?;
    let start = text[..at].rfind('\x1b')?;
    Some(text[start..at].to_owned())
}

/// Without `COLORTERM` the truecolor palette is downgraded to 256 colours —
/// Apple Terminal.app has no 24-bit colour — and `--color never` emits nothing
/// for anyone to strip.
#[test]
fn color_depth_follows_the_terminal() {
    let always = String::from_utf8(hog(
        &["--color", "always", "--timezone", "utc"],
        REFERENCE.as_bytes(),
    ))
    .expect("output stays UTF-8");
    assert!(
        always.contains("\x1b[38;5;"),
        "expected ansi256: {always:?}"
    );
    assert!(!always.contains("\x1b[38;2;"), "no truecolor: {always:?}");

    let never = hog(BASE, REFERENCE.as_bytes());
    assert!(!never.contains(&ESC), "never must emit no escapes");
    // Stripping the colour must leave exactly the golden output.
    assert_eq!(
        strip(&always),
        String::from_utf8(never).expect("output stays UTF-8")
    );
}

/// Removes SGR sequences. Small enough to spell out, and it keeps the test
/// honest about what it is comparing.
fn strip(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        match after.find('m') {
            Some(end) => rest = &after[end + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}
