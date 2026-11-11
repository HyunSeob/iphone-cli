# iphone-cli (`iph`)

A CLI and MCP server that lets AI agents read and operate any app on a **real iPhone** through its accessibility tree. No XCUITest, no WebDriverAgent, no Xcode signing: it talks directly to the Apple developer services Xcode itself uses. Sequences you run often become **scripts** (JavaScript) that execute in one call, without a round trip to the model between steps.

Built on the principle of [Poki](https://github.com/JosssphZhou/poki). No Poki code is used; this is a new implementation from the behaviour Poki's documentation describes and from experiments on a device. Device communication relies on [idevice](https://github.com/jkcoxson/idevice) (MIT).

한국어: [README.md](README.md)

```console
$ iph read --human
Settings  430x932  15 elements
application   "Settings"  @0,0 430x932
  header        "Settings"  @20,119.3 53.7x37
  switch        "Airplane Mode" = "0"  @20,489.3 390x49.3
  button        "Wi-Fi, HomeNet"  @20,538.7 390x49.3
  button        "General"  @20,700 390x49.3
  …

$ iph tap "General"
{"ok":true,"outcome":"done","elapsed_ms":1752,"sent":true,"verified":true,"screen_changed":true,…}
```

## Scope

| | |
|---|---|
| Host | macOS |
| Device | A real iPhone, iOS 18 or later (verified on **one iPhone 15 Pro Max with iOS 27.0.1** only, Korean interface) |
| Connection | USB or the same Wi-Fi (verified over Wi-Fi only) |
| Out of scope | Simulators, Android, Windows/Linux hosts, landscape |

## Install

Rust 1.88 or later, and Xcode once (to pair the phone and for its developer disk image).

```sh
git clone https://github.com/HyunSeob/iphone-cli
cd iphone-cli
cargo install --path .        # ~/.cargo/bin/iph
iph setup                     # walks through what the phone needs
```

## First-time setup

`iph setup` runs `iph doctor`, says in one line what to do on the phone for each failed check, and checks again. An agent can read `checks[].user_action` from `iph doctor` (JSON) and guide the user.

| Check | Required | What it is |
|---|---|---|
| `pairing` | yes | The Mac holds a pairing record for the phone. If not: connect by USB and tap Trust |
| `connection` | yes | The phone is visible over USB or the same Wi-Fi |
| `developer_mode` | yes | Settings → Privacy & Security → Developer Mode. If it is off, `iph doctor` makes that switch appear |
| `developer_disk_image` | yes | Source of the accessibility, display and input services. Selecting the phone once in Xcode's Devices and Simulators mounts it |
| `screen_unlocked` | yes | The phone is unlocked |
| `accessibility_tree` | yes | Screen elements can really be read |
| `touch_input` | yes | Scrolls the Settings list down one step and back to see that touch lands (`--no-touch-test` skips it) |
| `wifi` | recommended | For working without the cable |
| `auto_lock` | recommended | Auto-Lock → Never for long sessions |
| `do_not_disturb` | recommended | Whether iph is set to turn Do Not Disturb on before its input and put the earlier Focus back afterwards (it is by default) |

Which checks proved truly required, and what could not be verified, is in [docs/onboarding.md](docs/onboarding.md) (Korean).

**Because of notification banners, iph keeps the phone in Do Not Disturb while it works.** Banners cover the top of the screen and are not part of the accessibility tree. A tap near the top then lands on the banner and opens another app (this happened once during development). So before its first input iph turns Do Not Disturb on, and afterwards it puts back the Focus that was on. Details and how to turn this off: [Do Not Disturb](#do-not-disturb) below.

## Commands

Every command prints JSON; add `--human` for people. The device is chosen by `--udid`, then `IPH_UDID`, then `udid` in `~/.config/iph/config.toml`, then the only connected phone. With several phones and none chosen, you get an error with the list.

| Command | Does |
|---|---|
| `iph devices` | Connected phones, iOS version, connection, readiness |
| `iph setup` / `iph doctor` | Preparation and checks |
| `iph read [--app] [--all] [--full] [--raw] [--max-age ms]` | Elements on screen. `--all` includes off-screen ones, `--full` adds identifiers (about 0.5 s more), `--raw` attaches every numbered attribute |
| `iph find <selector>` | Elements matching a selector |
| `iph tap <selector \| x,y> [--long ms] [--double] [--confirm op_id]` | Tap the middle of an element, or a point |
| `iph swipe <up\|down\|left\|right \| x1,y1,x2,y2> [--duration ms] [--drag]` | Swipe. `--drag` slows to a stop before lifting, so nothing coasts |
| `iph scroll <up\|down> [--until selector] [--max N]` | One step, or until an element shows |
| `iph type <text> [--clear] [--submit]` | Paste text into the field that has the keyboard (any language, emoji). The clipboard is put back and the field's value checked |
| `iph key <key>` | `return`, `esc`, `tab`, `cmd+a` … |
| `iph button <home\|lock\|volume_up\|volume_down>` | Hardware buttons |
| `iph launch <bundle id \| name> [--restart]` / `iph apps` | Launch an app, list apps |
| `iph screenshot [-o file]` | PNG capture |
| `iph batch <file \| ->` | A JSON array of steps in one call |
| `iph script <file \| -> [--arg k=v] [--confirm op_id]` | Run JavaScript as given (`-` reads stdin) |
| `iph run <name> [--arg k=v] [--confirm op_id]` / `iph scripts` | Run a saved script, list saved scripts |
| `iph op <op_id>` | Look up a confirmed operation |
| `iph mcp [--tools a,b]` | MCP server on stdio |
| `iph dnd <on\|off\|status>` | Turn Do Not Disturb on, put the earlier Focus back, or show the current state. Normally iph does this by itself |
| `iph stop` | Put the earlier Focus back, then stop the resident process for this phone |

### Selectors

- A plain string is an exact label: `iph tap "General"`
- `label="exact"`, `label~="contains"`, `label/regex/`
- `role=button` (application, button, switch, text_field, search_field, key, tab, header, text, image, link, adjustable, other)
- `id=…` matches the accessibility identifier or the element `id` from `iph read`; `id~=` and `id/regex/` work too
- `value="…"`, `value~=…`
- `within="<selector>"` only inside that element, `index=N` the Nth match (from 0)
- Several matches are an `ambiguous_selector` error with the candidates; nothing is picked for you

Rows labelled `"name, value"` (as in Settings: `"Wi-Fi, HomeNet"`) also match `label="Wi-Fi"`.

Scripts, MCP and batch also take an object: `{label, label_contains, label_regex, value, value_contains, value_regex, role, id, id_contains, id_regex, within, index}`.

### Results

```json
{"ok": true, "outcome": "done", "elapsed_ms": 1752, "sent": true, "verified": true, "screen_changed": true, "element": {…}}
{"ok": false, "outcome": "not_sent", "error": {"code": "not_found", "message": "…", "hint": "…"}, "elapsed_ms": 1210, "screen_summary": {…}}
```

| `outcome` | Meaning | Exit code |
|---|---|---|
| `done` | Ran to the end | 0 |
| `not_sent` | Nothing reached the phone. Safe to try again | 1 |
| `unknown` | Input may have reached the phone. **Do not resend**; read the screen | 2 |
| `blocked` | Refused by the safety guard | 3 |

The phone does not acknowledge touch reports. So results separate `sent` from `verified` (the screen was read again afterwards), and taps, typing and scrolls report `screen_changed`. When another app came to the front, `app_changed: true` is added. When Do Not Disturb was turned on ahead of the command, `dnd: {on, changed, previous, elapsed_ms}` is added.

## Safety guard

- **Guarded words:** a tap whose target label contains 결제, 결제하기, 송금, 보내기, 이체, 주문, 주문하기, 구매, 구매하기, 결제 완료, 지불, 충전, pay, purchase, buy, send money, transfer or place order is not sent; the result is `outcome: "blocked"`, `error.code: "confirm_required"`. Korean words match anywhere in the label, English ones as whole words ("Payment methods" passes).
- **`--confirm <op_id>`:** after a person approved, a new id lets the tap through. The same `op_id` is never pressed twice: it is written to `~/Library/Application Support/iph/ops.jsonl` (0600) before the tap goes out, so it stays spent even if the process dies halfway. When the outcome is uncertain, look it up with `iph op <op_id>` instead of retrying with a new id.
- **`{confirm: true}` in a script:** a script can only mark a call `confirm: true`; the op_id counts only when a person passes it with `iph run/script … --confirm <op_id>`. A script that puts an op_id into `confirm` itself (read from a blocked result or made up) is refused with `confirm_not_allowed`. One op_id presses one marked call, so two presses need two `--confirm`. A tap on a guarded label that the script did not mark stays blocked even when `--confirm` was given.
- **Swipes:** a swipe that begins and ends on one element can press it like a tap, so it is refused when that element's label is a guarded word.
- **`only_apps`:** when set in `~/.config/iph/config.toml`, only those apps are launched, and input is refused while anything else is in front.
- **Do Not Disturb:** turned on before input and put back afterwards (below).

```toml
# ~/.config/iph/config.toml
udid = "00008130-…"

[safety]
add = ["donate"]
remove = ["보내기"]
only_apps = ["com.apple.Preferences", "com.apple.calculator"]
dnd = true               # Do Not Disturb on before input, earlier Focus back afterwards (default true)
dnd_idle_secs = 300      # put it back after this long without a command; 0 = only at iph stop or exit
# dnd_focus = "Work"     # a Focus to switch to instead of Do Not Disturb, by the name the phone shows
# dnd_control = "Focus"  # label of the Focus control in Control Center, for a phone not in Korean or English

[daemon]
input_idle_secs = 120    # close the input session after this long unused; 0 closes it after every command
idle_exit_secs = 1800    # the resident process exits after this long without requests

[script]
timeout_secs = 300       # how long one script may run (840 at most)
memory_mb = 64           # how much memory a script may hold
```

Sending input needs the phone to be in "screen sharing" state (an indicator shows at the top of the phone). For an app that hides its content or warns while the screen is shared, set `input_idle_secs = 0`: the phone is then in that state only while input is being sent, at the price of 3 to 4 s more per input. Reading does not depend on screen sharing.

**Limit:** the guard reads labels and nothing else. A button without a label, or labelled differently (an icon-only pay button, "Next", "OK"), is not caught. In any flow where money or messages are at stake, mark that call `{confirm: true}` in the script yourself.

### Do Not Disturb

- **Turned on:** when a command that sends input (`tap`, `swipe`, `scroll`, `type`, `key`, `button`, `launch`, `batch`) first runs in the resident process. A script turns it on before its first call that sends input; a script that only reads leaves it alone. If Do Not Disturb is already on, nothing changes.
- **Put back:** at `iph stop`, after `dnd_idle_secs` (5 minutes by default) without a command, when the resident process exits on its own, or with `iph dnd off`. Later input turns it on again.
- **Put back to what:** the Focus that was on before (or none) is written to `~/Library/Application Support/iph/<udid>.dnd.json` and restored as it was. If the phone was already in Do Not Disturb, iph leaves it alone; if a person switched to another Focus meanwhile, that one stays.
- **How:** no service on the phone offers this, so iph does what a person does: pull Control Center down, hold the Focus control, pick the mode in the list, press Home twice. Control Center is therefore visible for a moment, and one switch usually takes 11 to 15 s, or 19 to 36 s when the resident process has just started. This errand is exempt from `only_apps` and the guarded words.
- **Needs:** the Focus control on Control Center's first page, and Control Center opening while an app is in front. For a phone in a language other than Korean or English, put the names the phone shows in `dnd_control` and `dnd_focus`.
- **On failure:** the command still runs and its result carries `dnd: {on: false, code, message}`. After two failures the resident process stops trying. Only when Control Center may have been left open is the command held back, with `control_center_open`.
- **If the resident process dies:** Do Not Disturb stays on. The record file lets the next run take over and put it back; `iph dnd off` does it at once.
- **Not covered:** whatever comes through Do Not Disturb (alarms and timers, emergency alerts, people and apps allowed in the Do Not Disturb settings) and system pop-ups. That is why `only_apps` and `app_changed` stay. Whether banners really stay away while Do Not Disturb is on was not tested separately (it relies on how iOS behaves).
- **Turning it off:** `[safety] dnd = false`. Then turn a Focus on yourself while iph works.

## Scripts

A sequence you run often is written in JavaScript and runs in one call. The commands (`read`, `tap`, `scroll` …) are functions; loops, conditions and variables are the language's own. The engine (QuickJS) is inside `iph`, so there is nothing else to install.

```js
// Read the iOS version from Settings
launch("com.apple.Preferences", { restart: true });
scroll("down", { until: "일반" });
tap("일반");
tap("정보");
return { ios_version: readField({ selector: { label: "iOS 버전" }, take: "value" }) };
```

```sh
iph script flow.js --arg city=Seoul    # run a file
iph script - <<'EOF'                   # from stdin
return { front: read({ app: true }).apps[0].name };
EOF
iph run settings-version               # a saved script, by name
iph scripts                            # list saved scripts
```

```json
{"ok": true, "outcome": "done", "elapsed_ms": 10712, "outputs": {"ios_version": "27.0.1"}, "logs": [], "calls": 5, "inputs_sent": 4, "script": "settings-version", "source": "builtin:settings-version"}
```

- **Where they live:** `~/.config/iph/scripts/<name>.js` (personal, wins), then the repository's `scripts/` (examples, built into the binary). A file's first comment line is its description in `iph scripts`.
- **Arguments and results:** `--arg k=v` arrives as `args.k` (text, so a number is `Number(args.n)`). The object the script returns, plus anything set with `output(k, v)`, becomes `outputs`; lines written with `log(…)` come back in `logs`.

| Function | What it does |
|---|---|
| `read(opts)` · `find(selector, opts)` · `apps()` | Same arguments and result as the command of that name |
| `tap(target, {long_ms, double, timeout_ms, settle, confirm})` | For a selector: waits up to 5 s for it to appear and taps once it has been in the same place on two readings in a row (a screen that has just come up may still shift its rows). `settle: false` turns that off |
| `tap([a, b, …])` | Tap several targets in a row on the screen as it is, without reading in between (keypads) |
| `swipe(direction \| {from, to}, {duration_ms, drag})` · `scroll(direction, {until, max})` | Swipe, scroll |
| `type(text, {clear, submit})` · `key(name)` · `button(name)` | Type, key, button |
| `launch(app, {restart})` | Launch and check it is in front. `restart` quits it first so it starts on its first screen |
| `waitFor(selector, {timeout_ms})` | Until the element appears (8 s by default). It sits out a system panel (Face ID) that covers the app for a moment |
| `readField({selector, take, regex})` | One value, as text. `take`: `value` (default), `label`, `identifier`, `text` |
| `readList({row, columns, key, group, stop_below, until, max_scrolls, limit})` | Collect a list while scrolling, each row once; returns `{rows, scrolls, ended}` |
| `sleep(ms)` · `log(…)` · `output(k, v)` | Wait, note, add a value to the result |

- A function returns the JSON object the CLI command of the same name prints (`tap("일반").screen_changed`, `read().elements`). Selectors can be strings or objects.
- **A failure is an exception.** A call that fails throws an `Error` carrying `e.code`, `e.outcome`, `e.hint` and `e.result` (the whole result object). Uncaught, it ends the script, and the result has `failed_call: {cmd, line, outcome}`, `reason: {code, message}` and `screen_summary`. Nothing is guessed. Use `try { … } catch (e) { … }` only around something that may legitimately be missing.
- **The `outcome` of a script that stopped:** `blocked` when the safety guard stopped it. Otherwise `unknown` if any input had gone out before it stopped (running it again would send that input again), and `not_sent` if none had. `inputs_sent` counts the calls that sent input.
- **Nothing follows `unknown`.** After a call whose input may or may not have reached the phone (`outcome: "unknown"`) the script is ended there. Catching the error does not help: no further call reaches the phone.
- Any call takes `hint: "…"` among its options: what to tell the person if the script stops at that call ("look at the phone" on a `waitFor` that sits out Face ID).
- After `launch`, that app must stay in front. If another app comes up, input calls and `readField` / `readList` stop with `unexpected_app`.
- **`readList`:** `columns` is `{name: {take, regex, selector}}`, or `{name: {value: "…"}}` for the same value on every row. `group: {selector, regex, as}` turns headings between rows (a date line) into a column of the rows under them, and `stop_below: N` stops once a heading's number falls below N. `key: "column"` says two rows are the same row when that column agrees; use it for lists whose rows change while they are read (a clock, time left). Without it the whole row has to agree.
- `readList` reads each screen once and works out by itself how far to scroll so that the last two rows of one screen land at the top of the next (that depends on the phone and the app, so it is computed from where rows were read). The two shared rows stitch the screens together, which shows whether a row was skipped. Before it calls the end of a list it moves the list up and down once to check.
- **Walls:** a script cannot reach files, the network or other processes (no module loading, no timers). `Date`, `JSON`, `RegExp`, `Map` and the rest of the language are there. A script that runs past `[script] timeout_secs` (300 s by default) or holds more than `memory_mb` (64 MB) is stopped.

Examples: [`settings-version`](scripts/settings-version.js), [`calculator-multiply`](scripts/calculator-multiply.js), [`clock-world-list`](scripts/clock-world-list.js), and [`settings-list`](scripts/settings-list.js) with a loop and a condition (all written for a Korean-language phone). Keep personal scripts (banking, delivery) out of the repository, in `~/.config/iph/scripts/`.

### batch

`iph batch` runs a JSON array of steps in order and stops at the first failure. It is for short sequences that need no loop or condition.

| Step | What it does |
|---|---|
| `launch: <app>` or `{app, restart}` | Launch and check it is in front |
| `wait_for: <selector>` or `{selector, timeout_ms}` | Until the element appears (8 s by default) |
| `tap: <selector>` or `{selector, long_ms, double, timeout_ms}` · `tap: [selector, …]` | Tap, tap several in a row |
| `type: <text>` or `{text, clear, submit}` · `swipe: <direction>` or `{from, to, duration_ms, drag}` | Type, swipe |
| `scroll_until: <selector>` or `{selector, direction, max}` | Scroll until it shows |
| `read_field: {name, selector, take, regex}` · `read_list: {name, row, columns, …}` | Read one value, read a list (as `readField` and `readList`) |
| `assert: <selector>` · `button: home` · `key: return` · `sleep: 500` | Continue only if it is there, button, key, wait (ms) |
| (on any step) `confirm: "<op_id>"` · `when: "a == b"` · `hint: "…"` | Press after confirmation, condition, what to say if it stops |

A failing step returns `{failed_step, reason, screen_summary, inputs_sent}`; each step's `elapsed_ms` is in `steps`.

## MCP

```sh
claude mcp add iph -- iph mcp          # Claude Code
codex mcp add iph -- iph mcp           # Codex
iph mcp --tools read,find,script       # offer only some tools
```

To have an agent reach for `iph` on its own, link the skill (it works with `iph` in the shell alone, without the MCP registration).

```sh
ln -s "$PWD/skills/iph" ~/.claude/skills/iph                    # Claude Code
ln -s "$PWD/skills/iph" ~/.codex/skills/iph                     # Codex
```

Tools: `iph_read`, `iph_find`, `iph_tap`, `iph_swipe`, `iph_scroll`, `iph_type`, `iph_key`, `iph_button`, `iph_launch`, `iph_apps`, `iph_screenshot`, `iph_batch`, `iph_script`, `iph_run`, `iph_scripts`, `iph_doctor`, `iph_dnd`. Arguments and results are the same as the CLI's. `iph_script` takes `code` (JavaScript) and `args`; `iph_run` takes `name` and `args`. Guidance for agents: [skills/iph/SKILL.md](skills/iph/SKILL.md).

## Speed

iPhone 15 Pro Max, iOS 27.0.1, Wi-Fi, with the resident process connected (2026-11-11).

| Action | Time |
|---|---|
| `iph read` (Settings home, 10 runs) | median 1.35 s, max 2.10 s |
| `iph read --full` (10 runs) | median 1.45 s, max 1.79 s |
| `iph tap` including the check (20 runs, 0 misses) | median 1.75 s, max 4.30 s |
| `iph scroll`, one step with check | 2.1 to 2.6 s |
| `iph type` (Notes, with check and clipboard restore) | 4 to 7 s |
| Script `settings-version` (5 calls, over USB) | 9.9 to 13.3 s (8 runs) |
| First `iph read` without a resident process (tunnel and subscription included) | about 2 to 3 s |
| Turning Do Not Disturb on, or putting the earlier Focus back | median 12.5 s with the connection up (11 runs, 10.9 to 14.2 s); 19 to 36 s right after the resident process starts |

A reading takes more than a second because of the phone: after a request it waits about 1.1 s in silence before it sends the screen. In apps whose screen often changes on its own, readings are taken twice and compared so that an out-of-date screen is not used; that adds 1 to 2 s per step. Details: [docs/how-it-works.md](docs/how-it-works.md) (Korean).

## Known limits

- Verified on one device. Other iOS versions may number the accessibility attributes differently (check with `iph read --raw`).
- Apps whose screen keeps moving (animations, video) take longer to read.
- Notification banners and system pop-ups are not in the app's tree. Do Not Disturb keeps most banners away, but not what comes through it.
- Turning Do Not Disturb on adds 11 to 36 s ahead of the first input. If you are recording the phone's screen, Control Center opening and closing will be in the recording; run `iph dnd on` beforehand, or turn it on by hand and set `[safety] dnd = false`.
- Once, a new input session took no touch at all (cause unknown, one session in about a dozen). The Do Not Disturb step notices this and tries again on a new session. With that feature off, this check is gone too.
- **There are stretches when the phone answers slowly.** On the morning of 2026-11-11 this happened four times in two hours (from a few minutes to 40): readings took 2 to 30 s or timed out, and input connections dropped. Once the Mac's CPU was fully used by other work; the cause of the others is unknown (often right after the resident process had been stopped and started several times in a row). In such a stretch a late reading of an earlier screen can be acted on: switching Do Not Disturb failed, Control Center was left open, or the app was left for the home screen. `iph stop`, a minute's wait and a new start brought it back. The record is in [docs/how-it-works.md](docs/how-it-works.md), section 7 (Korean).
- Portrait only.
- `iph apps` shows the names the phone reports (usually English). Launch by bundle id to be sure.
- `iph type` puts back the clipboard's text (or one image). Rich content comes back as plain text.
- Nothing works on a locked phone. Face ID and passcode prompts are never answered for you.
- Mounting the developer disk image automatically is written but untested (the test phone already had it mounted).

## How it is put together

One `iph` call is one request over a Unix socket (`~/Library/Application Support/iph/<udid>.sock`) to a resident process (`iph daemon`, started by the first call). It keeps the tunnel, the accessibility subscription and the touch-input session open per phone and handles requests one at a time. It exits after 30 minutes without requests.

```
iph (CLI) ─┐
iph mcp  ──┼─ unix socket ─ iph daemon ─ lockdown → CoreDeviceProxy → userspace TCP tunnel → RSD
           ┘                              ├ axAuditDaemon.remoteAXService   (screen elements)
                                          ├ displayservice + universalhid   (touch, keyboard)
                                          ├ pasteboardservice               (clipboard)
                                          ├ appservice                      (launch, app list)
                                          └ screencaptureservice            (capture)
```

## Development

```sh
cargo test          # runs without a phone: parser (real Calculator dumps), selectors, safety guard, scripts
IPH_TRACE=1 iph …   # logs request and dump times to ~/Library/Application Support/iph/<udid>.log
```

## License

MIT, see [LICENSE](LICENSE). The media-session bring-up is adapted from an idevice example tool ([LICENSE.idevice](LICENSE.idevice)). `vendor/jktcp` is the tunnel's TCP stack, jktcp 0.1.8 (MIT), with two constants changed so that it waits longer before giving a connection up ([vendor/jktcp/LICENSE](vendor/jktcp/LICENSE)).
