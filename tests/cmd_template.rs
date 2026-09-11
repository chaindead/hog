//! The pure half of command mode, through the library target: substitution,
//! arity and the whitelist (HLD §5).
//!
//! Nothing here spawns a process, opens a file or reads the environment. That
//! is the point of the split HLD §2 draws through `src/command/`: everything
//! that decides **what would run** is a function of the template and the
//! arguments alone, so the security-critical half of the milestone is pinned by
//! tests that need no ssh, no host, no network and no fixture. What a real
//! process adds — the stream, the exit code, the `Drop`-guard — is
//! `tests/cmd_fake.rs`.
//!
//! The three questions this file answers, in the order [`plan`] asks them:
//!
//! 1. **Which brace shapes are placeholders?** Exactly `{N}` with canonical
//!    digits, plus `{{N}}` as its escape. Every other brace in the template is
//!    data, because `find -exec {} \;` and `docker --format '{{.Names}}'` are
//!    real commands that have to survive being written in a config file.
//! 2. **How many arguments does a template need?** Exactly one more than its
//!    highest index — too few *and* too many are refusals, because a surplus
//!    argument is nearly always a typo.
//! 3. **What may an argument contain?** `[A-Za-z0-9._:/@-]`, at most 256 bytes,
//!    checked *before* substitution. The whole ASCII range is swept below, so
//!    the refused set is written down rather than sampled.
//!
//! # Why substitution-after-split is only half-observable from out here
//!
//! HLD §5 requires the arguments to be substituted into the words `shlex::split`
//! produced, never into the template string before it is split — otherwise a
//! value containing a space would silently become two argv entries. Through the
//! public API that failure mode cannot be *reached*, because every character
//! that could split a word is refused by the whitelist one step earlier. So what
//! is checked here is the observable consequence — **the number of argv words is
//! a property of the template alone** — and the direct proof, which needs a
//! `SafeArg` built without the check, stays in the crate's own unit tests where
//! that constructor is reachable.

use hog::command::template::{self, TemplateError};
use hog::command::validate::{self, ArgError, SafeArg};
use hog::command::{CommandError, CommandPlan, plan};
use hog::settings::Settings;

/// The starter config's template, quoted from HLD §5 and the shipped
/// `starter.toml`. Repeated verbatim rather than `include_str!`-ed out of the
/// config, because a test that reads its expectation from the thing under test
/// cannot notice the thing under test changing.
const STARTER: &str =
    "ssh -tt -o ServerAliveInterval=15 {0} 'docker logs -f --since 1h bpam-{1}-1'";

/// Every character the whitelist of HLD §5 accepts, spelled out.
const WHITELIST: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._:/@-";

// ============================================================ small helpers

fn settings(command: &str) -> Settings {
    Settings {
        command: Some(command.to_owned()),
        ..Settings::default()
    }
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// The public entry point, for a template and the arguments of one invocation.
fn plan_of(template: &str, args: &[&str]) -> Result<CommandPlan, CommandError> {
    plan(&settings(template), &owned(args))
}

/// The assembled argv, program first. Panics with the refusal if there is one,
/// so a failing expectation reads as the message a user would have seen.
fn argv(template: &str, args: &[&str]) -> Vec<String> {
    match plan_of(template, args) {
        Ok(plan) => plan.argv().map(str::to_owned).collect(),
        Err(err) => panic!("{template:?} with {args:?} must plan, got:\n{err}"),
    }
}

/// The refusal for a template that cannot be used with these arguments.
fn refusal(template: &str, args: &[&str]) -> CommandError {
    match plan_of(template, args) {
        Ok(plan) => panic!("{template:?} with {args:?} must be refused, got: {plan}"),
        Err(err) => err,
    }
}

/// The first line of a refusal — the summary, without the guidance under it.
fn summary(err: &CommandError) -> String {
    err.to_string()
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// `template::parse` plus `render`, for the shapes that need no `plan`.
fn render(template: &str, args: &[&str]) -> Result<Vec<String>, TemplateError> {
    let safe: Vec<SafeArg<'_>> = args
        .iter()
        .enumerate()
        .map(|(index, raw)| {
            SafeArg::parse(index, raw)
                .unwrap_or_else(|err| panic!("the fixture argument {raw:?} must pass: {err}"))
        })
        .collect();
    template::parse(template)?.render(&safe)
}

/// The last argv word a template produces, for the brace matrix. Every row
/// there is spelled `x …`, so the program word is never the one under test.
fn last_word(template: &str, args: &[&str]) -> String {
    let mut words = argv(template, args);
    assert!(
        words.len() >= 2,
        "{template:?} must produce a word after `x`"
    );
    words.pop().unwrap_or_default()
}

// ====================================================== the documented case

/// HLD §5's own example, end to end: `hog prod api` against the shipped
/// template. If one line of this file is the milestone, it is this one.
#[test]
fn the_starter_template_assembles_the_argv_hld_5_documents() {
    let plan = plan_of(STARTER, &["prod", "api"]).expect("the shipped template must plan");

    assert_eq!(plan.program(), "ssh");
    assert_eq!(
        plan.args(),
        [
            "-tt",
            "-o",
            "ServerAliveInterval=15",
            "prod",
            // One word: the remote shell gets the whole `docker logs …` line,
            // and `bpam-{1}-1` was filled in without breaking it apart.
            "docker logs -f --since 1h bpam-api-1",
        ]
    );
}

/// The two commands HLD §5 names as the reason a bare `{}` stays literal. Both
/// are real invocations; both must reach the OS exactly as written.
#[test]
fn the_commands_that_must_pass_through_untouched_do() {
    assert_eq!(
        argv("find /var/log -name '*.log' -exec cat {} \\;", &[]),
        [
            "find", "/var/log", "-name", "*.log", "-exec", "cat", "{}", ";"
        ]
    );
    assert_eq!(
        argv("docker ps --format '{{.Names}}\t{{.Status}}'", &[]),
        ["docker", "ps", "--format", "{{.Names}}\t{{.Status}}"]
    );
    assert_eq!(
        argv("docker ps --format '{{range .}}{{.ID}} {{end}}'", &[]),
        ["docker", "ps", "--format", "{{range .}}{{.ID}} {{end}}"]
    );
}

// ==================================================== the placeholder matrix

/// Every brace shape the scanner can meet, and what it becomes. The table is
/// the specification: a shape that is not `{N}` or `{{N}}` is data, and data is
/// copied through byte for byte.
///
/// Each row is `(template, arguments, the last word it produces)`. Every
/// template starts with `x`, so the word under test is never the program.
#[test]
fn the_brace_matrix_is_exactly_two_shapes_and_everything_else_is_data() {
    let rows: &[(&str, &[&str], &str)] = &[
        // ---- the feature: `{N}` is replaced by argument N.
        ("x {0}", &["A"], "A"),
        ("x {0} {1}", &["A", "B"], "B"),
        ("x {0}{1}", &["A", "B"], "AB"),
        ("x {1}{0}", &["A", "B"], "BA"),
        ("x {0} bpam-{1}-1", &["A", "B"], "bpam-B-1"),
        ("x a{0}b{1}c", &["A", "B"], "aAbBc"),
        ("x {0}{0}", &["A"], "AA"),
        ("x '{0}'", &["A"], "A"),
        ("x \"{0}\"", &["A"], "A"),
        ("x 'y {0} z'", &["A"], "y A z"),
        // Ten is two digits, and nothing about the scan is one-digit-only.
        (
            "x {0}{1}{2}{3}{4}{5}{6}{7}{8}{9} {10}",
            &["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "TEN"],
            "TEN",
        ),
        // ---- the escape hatch: `{{N}}` is how a literal `{N}` is written.
        ("x {{0}}", &[], "{0}"),
        ("x {{12}}", &[], "{12}"),
        ("x {{0}}-{0}", &["A"], "{0}-A"),
        ("x {{0}}{0}", &["A"], "{0}A"),
        // ---- everything else: data.
        ("x {}", &[], "{}"),
        ("x {}{}", &[], "{}{}"),
        ("x {abc}", &[], "{abc}"),
        ("x '{ 0 }'", &[], "{ 0 }"),
        ("x {-1}", &[], "{-1}"),
        ("x {+0}", &[], "{+0}"),
        ("x {0.0}", &[], "{0.0}"),
        ("x {0x1}", &[], "{0x1}"),
        ("x {0,1}", &[], "{0,1}"),
        ("x '{0 }'", &[], "{0 }"),
        ("x '{ 0}'", &[], "{ 0}"),
        ("x {0", &[], "{0"),
        ("x 0}", &[], "0}"),
        ("x }{", &[], "}{"),
        ("x {{}}", &[], "{{}}"),
        ("x {{{}}}", &[], "{{{}}}"),
        ("x '{${N}}'", &[], "{${N}}"),
        ("x {{.Names}}", &[], "{{.Names}}"),
        // Leading zeros: one index must have exactly one spelling, or the
        // arity check would be arguing with itself about how many it needs.
        ("x {01}", &[], "{01}"),
        ("x {00}", &[], "{00}"),
        ("x {007}", &[], "{007}"),
        ("x {{01}}", &[], "{{01}}"),
        // ---- half an escape is not an escape, and the scan carries on past
        // the brace that started nothing.
        ("x {{0}", &["A"], "{A"),
        ("x {0}}", &["A"], "A}"),
        ("x {a{0}", &["A"], "{aA"),
        ("x {{{0}}}", &[], "{{0}}"),
    ];

    for (template, args, expected) in rows {
        assert_eq!(
            last_word(template, args),
            *expected,
            "{template:?} with {args:?}"
        );
    }
}

/// The same table read the other way round: which rows consume an argument.
/// A shape that is data must not silently raise the arity, or a user would be
/// asked for an argument that could never be used.
#[test]
fn only_the_two_recognised_shapes_raise_the_arity() {
    let rows: &[(&str, usize)] = &[
        ("x {0}", 1),
        ("x {1} {0}", 2),
        ("x {0} {0}", 1),
        ("x {10} {9} {8} {7} {6} {5} {4} {3} {2} {1} {0}", 11),
        ("x {}", 0),
        ("x {{0}}", 0),
        ("x {{12}}", 0),
        ("x {abc}", 0),
        ("x {01}", 0),
        ("x {00}", 0),
        ("x {{01}}", 0),
        ("x {{.Names}}", 0),
        ("x {-1}", 0),
        ("x {0.0}", 0),
        ("x {{{0}}}", 0),
        // A run of digits too long for a `usize` is not an index; leaving it
        // literal is what keeps "unrecognised shapes are data" whole.
        ("x {99999999999999999999999999999999999999999}", 0),
    ];

    for (template, arity) in rows {
        let parsed = template::parse(template)
            .unwrap_or_else(|err| panic!("{template:?} must parse: {err}"));
        assert_eq!(parsed.required_arity(), *arity, "{template:?}");
        assert_eq!(
            parsed.max_index(),
            arity.checked_sub(1),
            "max_index must be one below the arity: {template:?}"
        );
        assert_eq!(
            parsed.used_indices().len(),
            *arity,
            "the used set is always 0..arity: {template:?}"
        );
    }
}

/// An index too large for a `usize` stays literal instead of overflowing.
#[test]
fn an_absurd_index_is_literal_text() {
    let digits = "9".repeat(40);
    assert_eq!(
        last_word(&format!("x {{{digits}}}"), &[]),
        format!("{{{digits}}}")
    );
}

// ==================================================== where a value can land

/// A placeholder may be the program word: `command = "{0} --version"` is a
/// legitimate template, not a special case.
#[test]
fn a_placeholder_may_be_the_program_itself() {
    let plan = plan_of("{0} logs -f", &["kubectl"]).expect("must plan");
    assert_eq!(plan.program(), "kubectl");
    assert_eq!(plan.args(), ["logs", "-f"]);
}

/// The load-bearing property of HLD §5: substituting into a *word* means a
/// value that sat inside the template's quotes stays inside that argv entry,
/// and the word count is fixed by the template alone.
#[test]
fn the_shape_of_the_argv_is_a_property_of_the_template_not_of_the_arguments() {
    let template = "ssh -tt {0} 'docker logs -f --since 1h bpam-{1}-1 2>&1'";
    let parsed = template::parse(template).expect("must parse");

    for args in [
        ["prod", "api"],
        ["deploy@prod-1.example.com", "a"],
        ["10.0.0.7:2222", "some-very-long-service-name.v2"],
        ["/usr/local/bin/host", "k8s/ns/pod-abc123"],
    ] {
        let plan = plan_of(template, &args).expect("must plan");
        assert_eq!(
            plan.argv().count(),
            parsed.word_count(),
            "the argument set changed the word count: {args:?}"
        );
        assert_eq!(plan.argv().count(), 4, "{args:?}");
        // The quoted section is still one entry, `2>&1` is still inside it, and
        // the value landed in the middle of that same entry.
        assert_eq!(
            plan.args().last().map(String::as_str),
            Some(format!("docker logs -f --since 1h bpam-{}-1 2>&1", args[1]).as_str()),
            "{args:?}"
        );
    }
}

/// Nothing is quoted on the way in. HLD §5 rejects quoting outright — hog
/// cannot know whether a value lands in a word the remote shell will split
/// again — so a checked value appears in the argv byte for byte.
#[test]
fn a_substituted_value_is_copied_verbatim_and_never_quoted() {
    for value in [
        "prod",
        "deploy@prod-1.example.com",
        "10.0.0.7:2222",
        "/var/log/app.log",
        "k8s/namespace/pod-abc123",
        "my_service.v2",
        "--since",
    ] {
        let plan = plan_of("ssh {0}", &[value]).expect("must plan");
        assert_eq!(
            plan.args(),
            [value],
            "{value:?} was rewritten on the way in"
        );
        // And inside a quoted word, where a naive implementation would be
        // tempted to add quotes of its own.
        let inside = plan_of("ssh h 'logs {0} end'", &[value]).expect("must plan");
        assert_eq!(inside.args()[1], format!("logs {value} end"), "{value:?}");
    }
}

// ================================================================== arity

/// The full arity table: for each template, which argument counts are accepted
/// and what the refusal says for the rest. HLD §5 — both too few and too many
/// are errors.
#[test]
fn the_arity_table() {
    let rows: &[(&str, usize)] = &[
        ("kubectl logs -f -l app=api", 0),
        ("ssh {0}", 1),
        ("ssh {0} 'logs {1}'", 2),
        ("ssh {0} 'logs {1} --since {2}'", 3),
        // Repetition and order do not change the count: the highest index does.
        ("ssh {1} {0} {1} {0}", 2),
    ];
    let pool = ["a", "b", "c", "d", "e"];

    for (template, needed) in rows {
        for given in 0..=4 {
            let args = &pool[..given];
            if given == *needed {
                assert_eq!(
                    argv(template, args).len(),
                    template::parse(template).expect("parses").word_count(),
                    "{template:?} must accept exactly {needed} arguments"
                );
                continue;
            }

            let err = refusal(template, args);
            let word = if *needed == 1 {
                "argument"
            } else {
                "arguments"
            };
            assert_eq!(
                summary(&err),
                format!("template needs {needed} {word}, got {given}"),
                "{template:?} with {given} arguments"
            );
            assert!(
                matches!(err, CommandError::Template { .. }),
                "an arity mismatch is a template fault: {err:?}"
            );
        }
    }
}

/// The exact sentence HLD §6 puts in row five of the input-mode table.
#[test]
fn the_arity_message_is_the_one_hld_6_spells_out() {
    assert_eq!(
        summary(&refusal("ssh {0} 'docker logs -f bpam-{1}-1'", &[])),
        "template needs 2 arguments, got 0"
    );
}

/// "1 arguments" would be the giveaway that nobody read the output.
#[test]
fn the_word_argument_is_singular_for_one() {
    assert_eq!(
        summary(&refusal("ssh {0}", &[])),
        "template needs 1 argument, got 0"
    );
    assert_eq!(
        summary(&refusal("ssh {0}", &["a", "b"])),
        "template needs 1 argument, got 2"
    );
    assert_eq!(
        summary(&refusal("kubectl logs -f", &["a"])),
        "template needs 0 arguments, got 1"
    );
}

/// `check_arity` is the same answer as `plan`'s, and it is exact rather than a
/// minimum.
#[test]
fn check_arity_accepts_one_count_and_refuses_every_other() {
    let parsed = template::parse("ssh {0} 'logs {1}'").expect("parses");
    assert_eq!(parsed.required_arity(), 2);
    assert!(parsed.check_arity(2).is_ok());
    for given in [0, 1, 3, 99] {
        assert_eq!(
            parsed.check_arity(given),
            Err(TemplateError::Arity { needed: 2, given })
        );
    }
}

// ======================================================== template defects

/// A template the shell rules cannot split is a defect in the config file, and
/// it is named as one rather than blamed on the arguments.
#[test]
fn a_template_that_will_not_split_is_reported_as_a_template_fault() {
    for broken in [
        "ssh {0} 'docker logs",
        "ssh {0} \"docker logs",
        "ssh host \\",
    ] {
        assert_eq!(
            template::parse(broken),
            Err(TemplateError::Unbalanced),
            "{broken:?}"
        );
        let err = refusal(broken, &["prod"]);
        assert_eq!(
            summary(&err),
            "command template has an unclosed quote or a trailing backslash",
            "{broken:?}"
        );
        // The hint shows the template back, spelled the way the TOML line is,
        // so the reader can see the quote that never closed.
        assert!(
            err.to_string().contains(&format!("{broken:?}")),
            "the refusal must echo the template: {err}"
        );
    }
}

/// A template with no words at all. The `#` case is the surprising one: a `#`
/// that *starts* a word opens a shell comment, so a template that is nothing
/// but a comment has nothing to run.
#[test]
fn a_template_with_no_words_is_reported_rather_than_run() {
    for empty in ["", "   ", "\t\n ", "# ssh {0} 'logs'"] {
        assert_eq!(
            template::parse(empty),
            Err(TemplateError::Empty),
            "{empty:?}"
        );
        assert_eq!(
            summary(&refusal(empty, &[])),
            "command template is empty",
            "{empty:?}"
        );
    }
}

/// A `#` inside a word is ordinary text, which is the other half of the same
/// shell rule and the one that keeps `bpam#1` working.
#[test]
fn a_hash_inside_a_word_is_not_a_comment() {
    assert_eq!(argv("x bpam#1 y", &[]), ["x", "bpam#1", "y"]);
}

/// `{0}` and `{2}` with no `{1}`: the second argument could only ever be typed
/// and thrown away, so the template is wrong — refused at parse time, before
/// anyone counts arguments.
#[test]
fn a_hole_in_the_indices_is_refused() {
    let rows: &[(&str, TemplateError)] = &[
        (
            "ssh {0} 'logs {2}'",
            TemplateError::SkippedIndex { missing: 1, max: 2 },
        ),
        (
            "ssh 'logs {1}'",
            TemplateError::SkippedIndex { missing: 0, max: 1 },
        ),
        (
            "ssh {0} {1} {3}",
            TemplateError::SkippedIndex { missing: 2, max: 3 },
        ),
        (
            "ssh {5}",
            TemplateError::SkippedIndex { missing: 0, max: 5 },
        ),
    ];

    for (template, expected) in rows {
        assert_eq!(
            template::parse(template),
            Err(expected.clone()),
            "{template:?}"
        );
    }

    assert_eq!(
        summary(&refusal("ssh {0} 'logs {2}'", &["a", "b", "c"])),
        "command template uses {2} but never {1}"
    );
}

/// A config file is a place a control byte can be pasted into. The refusal
/// about it must not be the thing that repaints the reader's terminal.
#[test]
fn a_control_byte_in_the_template_is_escaped_in_the_refusal() {
    let message = refusal("ssh \u{1b}[31m {0} 'logs {2}'", &["a"]).to_string();
    assert!(!message.contains('\u{1b}'), "a raw ESC reached the message");
    assert!(message.contains("\\u{1b}"), "{message}");
}

// =========================================================== the whitelist

/// The sweep: every ASCII code point, checked one by one. This is the list of
/// refused characters HLD §5 gestures at, written down instead of sampled.
#[test]
fn every_ascii_character_is_either_on_the_whitelist_or_refused() {
    let mut accepted = String::new();

    for byte in 0u8..=127 {
        let character = char::from(byte);
        let alone = character.to_string();
        let verdict = SafeArg::parse(0, &alone);

        if WHITELIST.contains(character) {
            let arg = verdict.unwrap_or_else(|err| {
                panic!("0x{byte:02x} {character:?} must be accepted, got: {err}")
            });
            assert_eq!(arg.as_str(), alone);
            accepted.push(character);
        } else {
            assert!(
                matches!(verdict, Err(ArgError::Disallowed { .. })),
                "0x{byte:02x} {character:?} must be refused, got {verdict:?}"
            );
        }

        // The same verdict for the character embedded in an otherwise legal
        // argument: the whitelist is per character, not per shape.
        let embedded = format!("api{character}1");
        assert_eq!(
            SafeArg::parse(0, &embedded).is_ok(),
            WHITELIST.contains(character),
            "embedded 0x{byte:02x} {character:?}"
        );
    }

    // `accepted` came out in code-point order, so the expectation is sorted to
    // match: this compares the *sets*, not the spelling of the constant.
    let mut expected: Vec<char> = WHITELIST.chars().collect();
    expected.sort_unstable();
    let expected: String = expected.into_iter().collect();

    assert_eq!(
        accepted, expected,
        "the accepted set drifted from [A-Za-z0-9._:/@-]"
    );
    assert_eq!(
        accepted.chars().count(),
        26 + 26 + 10 + 6,
        "52 letters, 10 digits, and . _ : / @ -"
    );
}

/// The refused half of the sweep, spelled out as text so that a change to the
/// whitelist has to be made here on purpose. Every one of these is a way to
/// break out of a word at one level or another.
#[test]
fn the_refused_printable_characters_are_exactly_these() {
    let refused: String = (0x21u8..=0x7e)
        .map(char::from)
        .filter(|character| !WHITELIST.contains(*character))
        .collect();

    assert_eq!(
        refused,
        // space is 0x20 and is covered by the control/space check below.
        "!\"#$%&'()*+,;<=>?[\\]^`{|}~",
        "the refused printable set changed"
    );

    // And the invisible half: space, every C0 control byte, and DEL. None of
    // them is on the whitelist, and several of them (NUL, newline, ESC) are the
    // reason the whitelist exists at all.
    for byte in (0x00u8..=0x20).chain(std::iter::once(0x7f)) {
        let value = char::from(byte).to_string();
        assert!(
            matches!(SafeArg::parse(0, &value), Err(ArgError::Disallowed { .. })),
            "0x{byte:02x} must be refused"
        );
    }
}

/// Non-ASCII is refused wholesale: hog has no way to know how the far end
/// encodes it, so a non-ASCII host name arrives as punycode or not at all.
#[test]
fn non_ascii_is_refused_however_it_is_spelled() {
    for value in [
        "прод",               // Cyrillic letters are alphanumeric, but not ASCII.
        "café",               // one accented character is enough.
        "ｆｕｌｌｗｉｄｔｈ", // fullwidth forms look like ASCII and are not.
        "a\u{200b}b",         // zero-width space: invisible in a terminal.
        "a\u{2028}b",         // line separator.
        "naïve.example.com",
        "🙂",
    ] {
        assert!(
            matches!(SafeArg::parse(0, value), Err(ArgError::Disallowed { .. })),
            "{value:?} must be refused"
        );
    }
}

/// The shapes the whitelist exists to let through, from HLD §5: docker, k8s and
/// systemd names, `user@host`, `host:port`, paths.
#[test]
fn the_names_command_mode_is_for_are_accepted() {
    for value in [
        "prod",
        "api",
        "bpam-api-1",
        "my_service.v2",
        "deploy@prod-1.example.com",
        "10.0.0.7:2222",
        "::1",
        "/var/log/app.log",
        "k8s/namespace/pod-abc123",
        "nginx.service",
        "UPPER0987",
        "30m",
        "--since",
        "-",
        "a",
    ] {
        let arg = SafeArg::parse(0, value)
            .unwrap_or_else(|err| panic!("{value:?} must be accepted: {err}"));
        assert_eq!(arg.as_str(), value);
    }
}

/// 256 bytes exactly is fine; 257 is not.
#[test]
fn the_length_limit_is_256_bytes_inclusive() {
    let at_limit = "a".repeat(validate::MAX_ARG_BYTES);
    assert!(SafeArg::parse(0, &at_limit).is_ok());

    let over = "a".repeat(validate::MAX_ARG_BYTES + 1);
    assert_eq!(
        SafeArg::parse(0, &over),
        Err(ArgError::TooLong {
            position: 1,
            len: validate::MAX_ARG_BYTES + 1,
            limit: validate::MAX_ARG_BYTES,
        })
    );
}

/// Length is checked before content, so a refusal never echoes a megabyte of
/// attacker-chosen text back at the terminal — and a long non-ASCII value is
/// "too long" rather than "not allowed", which is the honest first answer.
#[test]
fn length_is_checked_before_content() {
    let huge = "; rm -rf /".repeat(1024);
    let message = SafeArg::parse(0, &huge)
        .expect_err("must be refused")
        .to_string();
    assert!(message.starts_with("argument 1 is too long"), "{message}");
    assert!(message.len() < 120, "the value was echoed back: {message}");

    let long_unicode = "é".repeat(200); // 400 bytes, 200 characters.
    assert!(
        matches!(
            SafeArg::parse(0, &long_unicode),
            Err(ArgError::TooLong { .. })
        ),
        "the limit counts bytes, which is what the kernel counts"
    );
}

/// An empty argument passes a whitelist trivially, so it needs its own rule:
/// `bpam-{1}-1` with an empty `{1}` would quietly become `bpam--1`.
#[test]
fn an_empty_argument_is_refused_rather_than_substituted() {
    assert_eq!(SafeArg::parse(0, ""), Err(ArgError::Empty { position: 1 }));
    let err = refusal("ssh {0} 'logs bpam-{1}-1'", &["prod", ""]);
    assert_eq!(summary(&err), "argument 2 is empty");
}

/// HLD §5 numbers arguments the way a human counts them: `{1}` is "argument 2".
#[test]
fn the_refusal_counts_arguments_from_one() {
    for (index, position) in [(0usize, 1usize), (1, 2), (2, 3), (9, 10)] {
        let err = SafeArg::parse(index, "a b").expect_err("a space is refused");
        assert_eq!(err.position(), position);
        assert!(
            err.to_string()
                .starts_with(&format!("argument {position} ")),
            "{err}"
        );
    }
}

// ================================================== refusals through `plan`

/// The security boundary, through the public entry point: not one of these
/// values ever becomes part of an argv.
#[test]
fn injection_attempts_never_reach_an_argv() {
    for hostile in [
        "api; rm -rf /",
        "api;rm",
        "api|tee /tmp/x",
        "api||id",
        "api&",
        "api && id",
        "$(id)",
        "${HOME}",
        "`id`",
        "api\nrm -rf /",
        "api\r\nrm",
        "api\ttab",
        "api rm",
        "'api'",
        "\"api\"",
        "api\\;",
        "api>out",
        "api<in",
        "api*",
        "api?",
        "~/secret",
        "a#b",
        "a!b",
        "a%b",
        "a^b",
        "a+b",
        "a=b",
        "a,b",
        "a[0]",
        "a{0}",
        "-oProxyCommand=id",
        "api\u{1b}[2J",
        "api\u{0}",
        "../../etc/shadow; id",
    ] {
        let err = refusal("ssh {0} 'logs {1}'", &["prod", hostile]);
        assert!(
            matches!(err, CommandError::Argument { .. }),
            "{hostile:?} must be refused as an argument, got: {err:?}"
        );
        assert!(
            summary(&err).starts_with("argument 2 contains characters that are not allowed"),
            "{hostile:?}: {err}"
        );
    }
}

/// The refusal is the one HLD §5 prints: which argument, what is allowed, and
/// how to see what would have run.
#[test]
fn the_refusal_says_which_argument_what_is_allowed_and_what_to_try_next() {
    let err = refusal("ssh {0} 'logs {1}'", &["prod", "api; rm -rf /"]);
    let message = err.to_string();

    assert_eq!(
        summary(&err),
        "argument 2 contains characters that are not allowed: \"api; rm -rf /\""
    );
    assert!(message.contains(validate::ALLOWED_DESCRIPTION), "{message}");
    assert!(
        message.contains("letters, digits and . _ - : / @"),
        "{message}"
    );
    assert!(message.contains("hog --dry-run"), "{message}");
    // The hint quotes the hostile value rather than pasting it in raw: it is
    // on its way to a terminal, and it is attacker-influenced text.
    assert!(message.contains("prod \"api; rm -rf /\""), "{message}");
}

/// A rejected value carrying an ESC must not escape into the terminal of
/// whoever is reading the refusal.
#[test]
fn a_rejected_value_cannot_repaint_the_terminal() {
    let message = refusal("ssh {0}", &["\u{1b}]0;pwned\u{7}"]).to_string();
    assert!(!message.contains('\u{1b}'), "a raw ESC reached the message");
    assert!(!message.contains('\u{7}'), "a raw BEL reached the message");
    assert!(message.contains("\\u{1b}"), "{message}");
}

/// The order the refusals come in, which is the order HLD §5 asks for: a broken
/// template is a config defect and is named first, the count comes next, and
/// only then are the values themselves looked at. Anything else would blame the
/// arguments for a template nobody can use.
#[test]
fn a_template_fault_outranks_an_arity_error_which_outranks_a_bad_argument() {
    // Unsplittable template *and* a hostile argument *and* the wrong count.
    let err = refusal("ssh {0} 'logs", &["a; id", "b; id"]);
    assert_eq!(
        summary(&err),
        "command template has an unclosed quote or a trailing backslash"
    );

    // Good template, wrong count, hostile argument: the count wins, because
    // the whitelist has not run yet.
    let err = refusal("ssh {0}", &["a; id", "b; id"]);
    assert_eq!(summary(&err), "template needs 1 argument, got 2");

    // Right count: now the values are looked at, and the *first* bad one is
    // the one reported.
    let err = refusal("ssh {0} {1}", &["a; id", "b; id"]);
    assert_eq!(
        summary(&err),
        "argument 1 contains characters that are not allowed: \"a; id\""
    );
}

/// Arguments with no template at all. HLD §5 cancelled the old "no template is
/// an error" rule: the built-in `echo {@}` runs instead, so a freshly installed
/// hog answers "where do my arguments go?" rather than refusing to do anything.
#[test]
fn arguments_without_a_template_run_the_built_in_echo() {
    let plan = plan(&Settings::default(), &owned(&["prod", "api"]))
        .expect("the built-in template always runs");
    assert_eq!(plan.program(), "echo");
    assert_eq!(plan.args(), ["prod", "api"]);
}

// ============================================================ the dry run

/// What `--dry-run` prints, as a pure function of the plan: one word per line,
/// because the question the flag exists to answer is *where the word boundaries
/// are*. A space-separated line would hide exactly that.
#[test]
fn the_dry_run_text_is_one_word_per_line() {
    let plan = plan_of(STARTER, &["prod", "api"]).expect("must plan");
    assert_eq!(
        plan.dry_run_text(),
        "ssh\n\
         -tt\n\
         -o\n\
         ServerAliveInterval=15\n\
         prod\n\
         \"docker logs -f --since 1h bpam-api-1\""
    );
}

/// A word that is plain printable ASCII is shown as itself; anything else is
/// quoted, so a space, a tab or an empty entry is visible rather than implied.
#[test]
fn the_dry_run_quotes_only_the_words_that_need_it() {
    assert_eq!(
        plan_of("kubectl logs -f -l app=api", &[])
            .expect("must plan")
            .dry_run_text(),
        "kubectl\nlogs\n-f\n-l\napp=api"
    );
    assert_eq!(
        plan_of("ssh '' host", &[])
            .expect("must plan")
            .dry_run_text(),
        "ssh\n\"\"\nhost"
    );
    assert_eq!(
        plan_of("ssh host 'a\tb'", &[])
            .expect("must plan")
            .dry_run_text(),
        "ssh\nhost\n\"a\\tb\""
    );
}

/// The one-line form used in error messages is the same words with spaces
/// between them — related to the dry run, and deliberately not the same thing.
#[test]
fn the_one_line_form_is_the_same_words_space_separated() {
    let plan = plan_of("ssh -tt {0} 'docker logs {1}'", &["prod", "api"]).expect("must plan");
    assert_eq!(plan.to_string(), "ssh -tt prod \"docker logs api\"");
}

// ============================================== the render step on its own

/// `Template::render` is the same substitution `plan` performs, reachable
/// without a `Settings`. Checked here so the public surface HLD §2 puts in the
/// library target is exercised as a surface, not only as `plan`'s innards.
#[test]
fn render_is_the_same_substitution_plan_performs() {
    assert_eq!(
        render("ssh {0} 'docker logs -f bpam-{1}-1'", &["prod", "api"]),
        Ok(vec![
            "ssh".to_owned(),
            "prod".to_owned(),
            "docker logs -f bpam-api-1".to_owned(),
        ])
    );
    assert_eq!(
        render("ssh {0}", &["a", "b"]),
        Err(TemplateError::Arity {
            needed: 1,
            given: 2
        })
    );
}
