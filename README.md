# hog

Reads JSON logs and prints them for humans.

One log line in, one readable line out: a timestamp, a level tag, the message,
and everything else as `key=value`. Nested objects are flattened to dotted keys,
noisy fields can be hidden, and anything that is not JSON is passed through
untouched. `hog` can read a pipe, or run the command that produces the logs for
you — `ssh`, `kubectl logs`, `docker logs`, whatever your config says.

It is a Rust rewrite of [`hulog`](https://github.com/chaindead/hulog) and keeps
its output format and its colours, minus the ways that one lost data. See
[Migrating from hulog](#migrating-from-hulog).

## Before and after

Your service writes this:

```console
$ tail -2 app.log
{"level":"info","ts":"2026-09-11T09:14:02.418Z","logger":"api","msg":"request completed","trace_id":"7f3c1a9e4b2d","grpc":{"service":"shop.v1.Orders","method":"Create","code":"OK","time_ms":18.4,"request":{"deadline":"2026-09-11T09:14:12Z","user_id":90210}},"http":{"status":200}}
{"level":"error","ts":"2026-09-11T09:14:03.771Z","logger":"api","msg":"upstream failed after retries","upstream":"payments","error":"context deadline exceeded","attempts":3}
```

`hog` prints this:

```console
$ tail -2 app.log | hog
09:14:02 [INF] request completed grpc.code=OK grpc.method=Create grpc.request.deadline=2026-09-11T09:14:12Z grpc.request.user_id=90210 grpc.service=shop.v1.Orders grpc.time_ms=18.4 http.status=200 logger=api trace_id=7f3c1a9e4b2d
09:14:03 [ERR] upstream failed after retries attempts=3 error="context deadline exceeded" logger=api upstream=payments
```

(The time column is printed in your machine's time zone; the output above
assumes UTC. If you are coming from `hulog`, read
[Migrating from hulog](#migrating-from-hulog) — this is the one thing that
changed.)

And with the fields you never read hidden:

```console
$ tail -2 app.log | hog -e grpc,logger
09:14:02 [INF] request completed http.status=200 trace_id=7f3c1a9e4b2d
09:14:03 [ERR] upstream failed after retries attempts=3 error="context deadline exceeded" upstream=payments
```

Three properties of that output are contract, not coincidence, because tooling
downstream depends on them:

* **One input line is always one output line.** `hog | grep`, `hog | head` and
  `hog | less` all behave. Values containing a space, a quote or an `=` are
  logfmt-quoted (`error="context deadline exceeded"`); newlines, tabs and ESC
  are escaped, so a log line can never repaint your terminal.
* **The tail is sorted by the full dotted key**, so two lines can be compared
  column by column and a diff of two runs is stable. Set `sort_keys = false` to
  keep the producer's order instead.
* **A key's colour depends only on its name** — `FNV-1a(name) % palette` — so
  `trace_id` is the same colour in this run, tomorrow, and on your colleague's
  machine. Your eye finds it without reading the text.

Anything that does not start with `{` is printed byte for byte, which is what
keeps stack traces, build banners and ssh's own chatter readable in the middle
of a stream. A line longer than 1 MiB is printed as well and the stream carries
on afterwards; in `hulog` one such line silently ended the log, discarding
everything after it and still exiting 0. That bug is what started this rewrite.
Invalid UTF-8 is passed through byte for byte for the same reason: a bad byte is
not a reason to stop reading.

## Install

Prebuilt binaries are attached to each [release][releases]; they are static
(musl) on Linux and native on macOS and Windows.

```console
$ curl -fsSL https://github.com/chaindead/hog/releases/latest/download/hog_Darwin_arm64.tar.gz | tar xz
$ install -m755 hog /usr/local/bin/hog
```

Archives are named `hog_{Darwin,Linux}_{arm64,x86_64}.tar.gz` and
`hog_Windows_{arm64,x86_64}.zip`, each with a `.sha256` beside it.

From source, with a Rust toolchain (1.87 or newer):

```console
$ cargo install --git https://github.com/chaindead/hog --locked
```

Optional, and worth the thirty seconds:

```console
$ hog completions zsh > "${fpath[1]}/_hog"      # bash, elvish, fish, powershell, zsh
```

`hog --version` prints the release tag the binary was built from, or `dev` with
the commit hash for a build out of a working copy.

[releases]: https://github.com/chaindead/hog/releases

## Two ways to feed it

`hog` decides which one you meant from whether you gave it positional
arguments. There is no flag for it and no other heuristic.

### A pipe or a file

```console
$ tail -f app.log | hog
$ hog < app.log
$ kubectl logs -f deploy/api | hog -e trace_id
```

Output is flushed as soon as the input goes quiet, so follow mode is live; when
you redirect to a file it batches instead. No flag, no tty guessing — `tail -f x
| hog > out.txt` is both a pipe and live, and it works.

Typing a bare `hog` at a prompt with nothing configured prints the help and
exits 2 rather than sitting there waiting for you to type JSON at it.

### A command it runs for you

The usual case is logs on another machine, and the usual shape of that is a
one-line shell wrapper everyone on the team has their own copy of. Put the line
in `hog`'s config instead:

```toml
command = "ssh -tt -o ServerAliveInterval=15 {0} 'docker logs -f --since 1h myapp-{1}-1'"
```

```console
$ hog prod api
09:14:02 [INF] request completed grpc.code=OK http.status=200 trace_id=7f3c1a9e4b2d
```

`hog` assigns no meaning to a position: `{0}` is simply your first argument,
`{1}` the second, and what they mean is whatever the template does with them.
`ssh` is nothing special here either — the same mechanism runs `kubectl logs`,
`docker logs`, `aws logs tail` or a script of your own.

The template is split into argv by shell quoting rules and executed directly.
**There is no local shell.** Exactly one shell is involved, the remote one
inside `'docker logs …'`, which is why a `2>&1` written inside the quotes works
and one written outside becomes a literal argument.

#### Placeholders

| In the template | Means |
| --- | --- |
| `{0}`, `{1}`, … | The first, second, … positional argument. |
| `{@}` | Every argument no index took. |
| `{}` | Literal. `find -exec {} \;` passes through untouched. |
| `{{0}}`, `{{@}}` | Literal `{0}` and `{@}`. `docker --format '{{.Names}}'` is safe. |

`{@}` is the variable tail. With `N` the highest index the template uses, `{@}`
is arguments `N+1` onwards; with no indexed placeholder at all it is all of
them. It expands to separate argv words where it stands, and to nothing when
there are none — exactly like `"$@"` in a shell:

```toml
command = "ssh -tt {0} 'docker logs -f {@}'"
```

```console
$ hog prod -- --since 30m --tail 200
```

That is also why there is no `--since` and no `--tail` flag on `hog` itself: the
tail goes straight to the program that already has those flags.

The `--` is load-bearing. `hog`'s own flags keep working after the positional
arguments (`hog prod api -e trace_id` is valid), so an unknown `--since` is a
usage error, not a value — `--` is what says "the rest is for the template".
Arguments with no leading dash need nothing: `hog prod api 30m` is fine as it
is.

Two template rules are enforced when the config is read, rather than at the
moment they would confuse you:

* **Indices run from `{0}` with no gaps.** `"ssh {0} {2}"` would ask you for
  three arguments and use two, which is a typo every time.
* **`{@}` is a whole word, and there is only one.** `myapp-{@}-1` has no sensible
  meaning — gluing several arguments into one word could only surprise you.

Arity is checked against what you typed: `{0}` and `{1}` want exactly two
arguments, `{@}` accepts any number, and both together set a floor with no
ceiling. `hog --help` prints your own template, its arity and an example:

```
Configured command (~/.hog.toml):
  ssh -tt -o ServerAliveInterval=15 {0} 'docker logs -f --since 1h myapp-{1}-1'

Takes 2 arguments:
  hog <ARG0> <ARG1>

Example:
  hog prod api
```

#### The default template

With no `command` key in the config — or with no config file at all — `hog` runs
a built-in `echo {@}`:

```console
$ hog prod api
prod api
```

That is not a useful command, it is a legible one. A brand-new install answers
"where do my arguments go?" on the first run instead of erroring, and `--help`
has something to show before you have written anything.

#### Seeing what would run

```console
$ hog --dry-run prod api
ssh
-tt
-o
ServerAliveInterval=15
prod
"docker logs -f --since 1h myapp-api-1"
```

One argv word per line, quoted where a word contains spaces, and nothing is
executed. This is the answer to "did my quoting do what I think", and it never
touches the network.

#### Arguments are checked, not quoted

Substituted arguments must match `[A-Za-z0-9._:/@-]` and be at most 256 bytes:

```console
$ hog prod 'api; rm -rf /'
error: argument 2 contains characters that are not allowed: "api; rm -rf /"

  Arguments are substituted into a shell command, so hog only accepts
  letters, digits and . _ - : / @

  Check what would run:  hog --dry-run prod "api; rm -rf /"
```

Quoting the value instead would be the obvious move and it does not work. `hog`
cannot know whether your argument lands in a word the *remote* shell will split
a second time (`'docker logs myapp-{1}-1'` — it will) or in a plain argv entry
(`-l app={1}` — it will not). The first needs quotes, the second is broken by
them, and nothing in the template says which one you wrote. So `hog` refuses to
carry the characters that would matter rather than pretending to neutralise
them. The whitelist still covers docker, Kubernetes and systemd names,
`user@host` and `host:port`, which is everything these arguments are in
practice.

## Configuration

There is nothing to set up: the first time `hog` runs and finds no config file,
it writes one and says so.

```console
$ cat app.log | hog
hog: created /home/you/.hog.toml
10:32:01 [INF] server started port=8080
$ hog config path
/home/you/.hog.toml
$ hog config edit
```

The note goes to stderr, once, so `cat app.log | hog | grep …` never sees it.
The file `hog` writes is the commented starter, and every value in it is the
built-in default with `command` left commented out — so the run that creates it
renders exactly what the run before it would have. If the file cannot be written (a
read-only `$HOME`, no `$HOME` at all), `hog` says so in one line and carries on
with the built-in defaults; it will not refuse to show you logs over a config
file.

The first of these that exists wins, and files are never merged:

1. `--config PATH` — a missing file here is an error, not a fallback.
2. `$HOG_CONFIG` — the same, from the environment.
3. `~/.hog.toml` — written from the starter if it is not there yet.
4. Nothing — the built-in defaults, silently.

Only line 3 is ever created. A file you named yourself and misspelled stays an
error, because a config quietly written to a path you did not expect is a worse
answer than being told.

Values then layer: **built-in defaults < config file < command-line flags.**

One dotfile in your home directory, on macOS as everywhere else: `hog` is a
developer CLI that belongs in your dotfiles, and the same file has to work
unchanged on the Linux hosts you ssh into. `~/.hog.toml` is looked for in
`$HOME` **only** — there is deliberately no implicit `./hog.toml` or
`./.hog.toml` beside the working directory and no walk up the directory tree.
The config names a command that `hog` executes, so picking one up from the
current directory would turn `git clone && cd && hog prod api` into a way to run
a stranger's command. Pass `--config ./hog.toml` when a per-project config is
what you want.

An unknown key is a warning with a line number, never an error:

```
warning: /home/you/.hog.toml:4: unknown key `output.command`
```

That is on purpose in both directions. A typo gets named instead of silently
ignored, and a config written for a newer `hog` still runs on an older one.

The file `hog` writes for you is fully commented, and that commentary is the
reference for the format. In outline:

```toml
exclude = []                                  # dotted paths to hide
command = "ssh -tt {0} 'docker logs -f {@}'"  # the template

[fields]                                      # first candidate present wins,
ts    = ["ts", "time", "timestamp", "@timestamp"]   # decided per line
level = ["level", "severity", "lvl"]
msg   = ["msg", "message"]

[output]
time_format = "%H:%M:%S"   # jiff strftime, or "raw", or "none"
time_zone   = "local"      # "local" | "utc" | IANA, e.g. "Europe/Moscow"
color       = "auto"       # "auto" | "always" | "never"; NO_COLOR is honoured
sort_keys   = true         # false keeps the JSON's own key order
# key_colors = ["#ff3366", "#66cc66", …]   # palette for key names

[output.levels]            # pino and bunyan send numbers
"30" = "info"
"50" = "error"
```

`command` and `exclude` are top-level keys, so they have to stay **above** the
first `[table]` header — a bare key written after `[output]` is `output.command`
as far as TOML is concerned. (`hog` diagnoses exactly that, with the line
number. It was a real bug in the design document this tool was written from.)

### `hog config`, verb by verb

| Command | Does |
| --- | --- |
| `hog config` | The resolved configuration, and which file it came from. |
| `hog config path` | Just the path, one line — safe inside `$(…)`. |
| `hog config edit` | Open it in `$VISUAL` / `$EDITOR`, and check it on the way out. |
| `hog config exclude` | The persistent exclude list, one path per line. |
| `hog config exclude add trace_id,log_id` | Append, keeping every comment in the file. |
| `hog config exclude rm trace_id` | Remove. |
| `hog config command` | The current template. |
| `hog config command set "<template>"` | Replace it. |

Edits are made with `toml_edit`, so your comments and formatting survive, and
the file is replaced atomically. `command set` validates the template **before**
writing:

```console
$ hog config command set 'ssh {0} {2} logs'
error: command template uses {2} but never {1}
```

A template that can only fail at the next run never reaches the file.

## Hiding fields

Most of the value of a log pretty-printer is in what it leaves out. An exclusion
is a dotted path, and it prunes the whole subtree under it:

| Rule | `grpc.code` | `grpc.request.deadline` | `grpcStatus` |
| --- | --- | --- | --- |
| `exclude = ["grpc"]` | hidden | hidden | **visible** |
| `exclude = ["grpc.request"]` | visible | hidden | visible |
| `exclude = ["grpc.request.deadline"]` | visible | hidden | visible |

The last column is the point. `grpcStatus` starts with the same six letters but
it is a different key, and matching happens on segment boundaries, not on string
prefixes. That is also the answer to "why not globs": a glob like `grpc.*` reads
as if it should hide `grpcStatus` in half the implementations you have used, and
subtree pruning already covers the case anyone actually wanted. Paths that are
just paths cannot surprise you.

`-e` is additive and `-E` clears:

| Invocation | Effective exclusions |
| --- | --- |
| `hog` | the config's list |
| `hog -e foo` | the config's list **plus** `foo` |
| `hog -E` | nothing hidden |
| `hog -E -e foo` | exactly `["foo"]` |

`-e` is repeatable and comma-separated, so `-e a,b -e c` gives all three. `-E`
deliberately does **not** conflict with `-e`: `-E -e foo` is how you override
the file for one run, and it is the reason the two flags exist as a pair rather
than as one flag with modes.

For a change you want to keep, `hog config exclude add` writes to the file.

## Migrating from hulog

The output format is the same, the level tags are the same, and the key colours
are the same algorithm over the same palette, so a stream you know still looks
like itself. Two things differ, and only one of them will surprise you.

**Timestamps move.** `hog` defaults to `time_zone = "local"` and prints the
timestamp in your machine's zone. `hulog` kept whatever offset was written in
the input string, so a log stamped `2026-09-11T09:14:02.418Z` printed as
`09:14:02` no matter where you read it. On a machine in UTC+3 the same line now
prints as `12:14:02`. Nothing is wrong; a log written at 09:14 UTC did happen at
12:14 where you are sitting, which is usually what you want when you are
correlating it with something that just happened on your screen.

If you want the old digits back — comparing against an archived `hulog` capture,
or a screenshot in an incident write-up — put this in `[output]`:

```toml
time_zone = "utc"
```

or pass `--timezone utc` for a single run. Any IANA name works too, so
`time_zone = "Europe/Moscow"` pins a team to one zone regardless of laptops.

**Your exclude list is not migrated automatically.** The config `hog` writes on
its first run carries the fourteen fields from the original `hulog` list
*commented out*, for you to uncomment. They are also written out one by one
rather than collapsed: the original list excludes the leaf
`grpc.request.deadline` while leaving `grpc.request` alone, which means the
other `grpc.request.*` fields were kept on purpose. Rewriting those fourteen
entries as a single `grpc` would have thrown away data somebody chose to keep.

Out of the box `hog` hides nothing at all.

## Two flags in the starter template that look like noise

The template in the starter config is this:

```toml
command = "ssh -tt -o ServerAliveInterval=15 {0} 'docker logs -f --since 1h myapp-{1}-1'"
```

`-tt` and `-o ServerAliveInterval=15` are both load-bearing, and both look like
clutter you could tidy away. In the previous design `hog` added them for you;
now that the template is yours, they are editable, which means they are
deletable. So:

**`-tt` forces a pty on the remote side.** Without it, `ssh` learns that you
have gone away only when it next tries to write to a closed pipe. On a **quiet**
container there is no next write: you press Ctrl-C, `hog` and `ssh` exit, and
`docker logs -f` keeps running on the remote host, one orphan per invocation.
With a pty the far end gets a hangup instead and the follower dies with it. The
containers this bites on are precisely the ones you leave a log open on for ten
minutes waiting for something to happen.

**`-o ServerAliveInterval=15` replaces ssh's default of `0`.** Zero means "never
check whether the connection is alive", so when a VPN drops — as opposed to
being closed — the TCP connection stays open forever from your side. `hog` sits
there with no output, no error and exit code nothing. Fifteen seconds of
keepalive turns that into a connection error within a minute. Check it yourself
with `ssh -G somehost | grep serveraliveinterval`.

Neither is magic and neither is mandatory; if your template runs `kubectl logs`
they are meaningless. But if it runs `ssh` and you delete them, those are the
two failures you have signed up for.

## Arguments that begin with a dash

`-` is on the argument whitelist, because host names, image tags and container
names are full of it. A consequence worth knowing: an argument like `--tail`
passes validation, and it reaches the spawned program's argv as **a flag of that
program**.

```console
$ hog prod -- --tail        # with command = "ssh {0} 'docker logs -f {@}'"
```

runs `docker logs -f --tail` on the far end. The `--` is for `hog`'s own parser,
which would otherwise read `--tail` as a `hog` flag.

This is not a privilege escalation and it is not an injection. No shell is
involved, the argument stays exactly one argv word, it cannot become two, and
the program it reaches is the one your own config file named. But it is worth
saying out loud, because the whitelist looks like it is about safety and this is
the edge it does not cover: whoever can pass arguments to your `hog` invocation
can pass flags to the program in your template. Keep that in mind before
pointing a template at a program whose flags do destructive things, and before
wiring `hog` into something that builds its arguments from untrusted input.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success; the input stream ended. |
| 1 | Runtime error — the config, the template, or a rejected argument. |
| 2 | Usage error, including a bare `hog` on a terminal with nothing configured. |
| 127 | The template's first word is not an executable in `PATH`. |
| 141 | `hog`'s own stdout was closed — you piped it into `head`. |
| *n* | Otherwise, the exit status of the command `hog` ran. A command killed by signal *s* reports 128 + *s*, exactly as a shell would. |

`hog` does not try to tell "could not connect" (ssh exits 255) apart from "the
remote command failed", because it does not know which program the template
runs. What it does do is say so loudly, with the count:

```
error: command exited with status 255 after 1423 lines
```

A dropped VPN after twenty minutes of output must not read like the log ending.

The child process is killed and reaped on every way out — Ctrl-C, a closed pipe,
a `SIGTERM` to `hog` itself. `SIGTERM` is handled explicitly for one reason: a
signal without a handler skips unwinding, so the guard that kills the child
never runs, and a quiet `docker logs -f` would be inherited by pid 1 and live
forever.

## Environment

| Variable | Effect |
| --- | --- |
| `HOG_CONFIG` | Config file to read, unless `--config` is given. |
| `HOME` | Where the default config `~/.hog.toml` is looked for. |
| `NO_COLOR` | Set to anything to turn colour off. |
| `CLICOLOR_FORCE` | Set to anything to force colour on, even into a pipe. |
| `TERM` | How much colour the terminal takes; true colour is downgraded to 256 where needed. |
| `VISUAL`, `EDITOR` | The editor `hog config edit` launches, `VISUAL` first. |

## Documentation

`hog --help` is dynamic — it prints your own template and its arity, so it
doubles as "I installed this, now what". `hog config --help` and
`hog config exclude --help` cover the subcommands the same way.

The design document this implementation follows — the reasoning behind every
decision summarised above, and the ones that were rejected — is
[`docs/HLD.md`](docs/HLD.md) (in Russian).
