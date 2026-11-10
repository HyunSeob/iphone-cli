# iphone-cli (`iph`)

A CLI and MCP server that lets AI agents read and operate any app on a **real iPhone** through its accessibility tree. No XCUITest, no WebDriverAgent, no Xcode signing: it talks directly to the Apple developer services Xcode itself uses. Sequences you run often become **recipes** that execute in one call, without a round trip to the model between steps.

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

Which checks proved truly required, and what could not be verified, is in [docs/onboarding.md](docs/onboarding.md) (Korean).

**Recommended: turn on a Focus (Do Not Disturb) while iph works.** Notification banners cover the top of the screen and are not part of the accessibility tree. A tap near the top then lands on the banner and opens another app (this happened once during development). `iph` reports `app_changed` and stops a recipe when another app comes to the front, but it cannot stop the tap itself.

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
| `iph run <recipe> [--arg k=v] [--confirm op_id]` / `iph recipes` | Run a recipe, list recipes |
| `iph op <op_id>` | Look up a confirmed operation |
| `iph mcp [--tools a,b]` | MCP server on stdio |
| `iph stop` | Stop the resident process for this phone |

### Selectors

- A plain string is an exact label: `iph tap "General"`
- `label="exact"`, `label~="contains"`, `label/regex/`
- `role=button` (application, button, switch, text_field, search_field, key, tab, header, text, image, link, adjustable, other)
- `id=…` matches the accessibility identifier or the element `id` from `iph read`; `id~=` and `id/regex/` work too
- `value="…"`, `value~=…`
- `within="<selector>"` only inside that element, `index=N` the Nth match (from 0)
- Several matches are an `ambiguous_selector` error with the candidates; nothing is picked for you

Rows labelled `"name, value"` (as in Settings: `"Wi-Fi, HomeNet"`) also match `label="Wi-Fi"`.

Recipes, MCP and batch also take an object: `{label, label_contains, label_regex, value, value_contains, value_regex, role, id, id_contains, id_regex, within, index}`.

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

The phone does not acknowledge touch reports. So results separate `sent` from `verified` (the screen was read again afterwards), and taps, typing and scrolls report `screen_changed`. When another app came to the front, `app_changed: true` is added.

## Safety guard

- **Guarded words:** a tap whose target label contains 결제, 결제하기, 송금, 보내기, 이체, 주문, 주문하기, 구매, 구매하기, 결제 완료, 지불, 충전, pay, purchase, buy, send money, transfer or place order is not sent; the result is `outcome: "blocked"`, `error.code: "confirm_required"`. Korean words match anywhere in the label, English ones as whole words ("Payment methods" passes).
- **`--confirm <op_id>`:** after a person approved, a new id lets the tap through. The same `op_id` is never pressed twice: it is written to `~/Library/Application Support/iph/ops.jsonl` (0600) before the tap goes out, so it stays spent even if the process dies halfway. When the outcome is uncertain, look it up with `iph op <op_id>` instead of retrying with a new id.
- **`confirm: true` on a recipe step:** the step stops unless the run has `--confirm <op_id>`.
- **`only_apps`:** when set in `~/.config/iph/config.toml`, only those apps are launched, and input is refused while anything else is in front.

```toml
# ~/.config/iph/config.toml
udid = "00008130-…"

[safety]
add = ["donate"]
remove = ["보내기"]
only_apps = ["com.apple.Preferences", "com.apple.calculator"]

[daemon]
input_idle_secs = 120    # close the input session after this long unused; 0 closes it after every command
idle_exit_secs = 1800    # the resident process exits after this long without requests
```

Sending input needs the phone to be in "screen sharing" state (an indicator shows at the top of the phone). For an app that hides its content or warns while the screen is shared, set `input_idle_secs = 0`: the phone is then in that state only while input is being sent, at the price of 3 to 4 s more per input. Reading does not depend on screen sharing.

**Limit:** the guard reads labels and nothing else. A button without a label, or labelled differently (an icon-only pay button, "Next", "OK"), is not caught. In any flow where money or messages are at stake, mark the last step `confirm: true` yourself.

## Recipes

```yaml
name: settings-version
description: Read the iOS version from Settings
app: com.apple.Preferences
args: {}
steps:
  - launch: {app: com.apple.Preferences, restart: true}
  - scroll_until: {label: "일반"}
  - tap: {label: "일반"}
  - tap: {label: "정보"}
  - read_field: {name: ios_version, selector: {label: "iOS 버전"}, take: value}
output: [ios_version]
```

| Step | What it does |
|---|---|
| `launch: <app>` or `{app, restart}` | Launch and check it is in front. `restart` quits it first so it starts on its first screen |
| `wait_for: <selector>` or `{selector, timeout_ms}` | Until the element appears (8 s by default) |
| `tap: <selector>` or `{selector, long_ms, double, timeout_ms}` | Tap; waits briefly for the target to appear |
| `tap: [selector, …]` | Tap several targets in a row on the screen as it is, without reading in between (keypads) |
| `type: <text>` or `{text, clear, submit}` | Type |
| `swipe: <direction>` or `{from, to, duration_ms, drag}` | Swipe |
| `scroll_until: <selector>` or `{selector, direction, max}` | Scroll until it shows |
| `read_field: {name, selector, take, regex}` | Read one value. `take`: `value` (default), `label`, `identifier`, `text` |
| `read_list: {name, row, columns, until, max_scrolls, limit}` | Collect a list while scrolling, each row once. `columns` is `{name: {take, regex, selector}}` |
| `assert: <selector>` | Continue only if the element is there |
| `button: home` · `key: return` · `sleep: 500` | Button, key, wait (ms) |
| `confirm: true` (on any step) | Runs only with `--confirm <op_id>` |

- `{{name}}` is replaced by `--arg name=value`. `args` holds defaults; `null` makes an argument required.
- A failing step stops the run with `{failed_step, reason, screen_summary}`. Nothing is guessed. The `outcome` is that of the failing step. `inputs_sent` counts the earlier steps that already sent input, so check it before deciding whether the whole run can simply be started again.
- After `launch`, that app must stay in front; another app stops the run with `unexpected_app`.
- Each step reports its `elapsed_ms`.
- Looked up in `~/.config/iph/recipes/*.yaml` (personal, wins) and then the repository's `recipes/` (examples, built into the binary). A file path works too.

Examples: [`settings-version`](recipes/settings-version.yaml), [`calculator-multiply`](recipes/calculator-multiply.yaml), [`clock-world-list`](recipes/clock-world-list.yaml) (written for a Korean-language phone). Keep personal recipes (banking, delivery) out of the repository, in `~/.config/iph/recipes/`.

## MCP

```sh
claude mcp add iph -- iph mcp          # Claude Code
codex mcp add iph -- iph mcp           # Codex
iph mcp --tools read,find,run          # offer only some tools
```

Tools: `iph_read`, `iph_find`, `iph_tap`, `iph_swipe`, `iph_scroll`, `iph_type`, `iph_key`, `iph_button`, `iph_launch`, `iph_apps`, `iph_screenshot`, `iph_batch`, `iph_run`, `iph_recipes`, `iph_doctor`. Arguments and results are the same as the CLI's. Guidance for agents: [skills/iph/SKILL.md](skills/iph/SKILL.md).

## Speed

iPhone 15 Pro Max, iOS 27.0.1, Wi-Fi, with the resident process connected (2026-11-11).

| Action | Time |
|---|---|
| `iph read` (Settings home, 10 runs) | median 1.35 s, max 2.10 s |
| `iph read --full` (10 runs) | median 1.45 s, max 1.79 s |
| `iph tap` including the check (20 runs, 0 misses) | median 1.75 s, max 4.30 s |
| `iph scroll`, one step with check | 2.1 to 2.6 s |
| `iph type` (Notes, with check and clipboard restore) | 4 to 7 s |
| Recipe `settings-version` (5 steps) | 10.8 to 22.4 s |
| First `iph read` without a resident process (tunnel and subscription included) | about 2 to 3 s |

A reading takes more than a second because of the phone: after a request it waits about 1.1 s in silence before it sends the screen. In apps whose screen often changes on its own, readings are taken twice and compared so that an out-of-date screen is not used; that adds 1 to 2 s per step. Details: [docs/how-it-works.md](docs/how-it-works.md) (Korean).

## Known limits

- Verified on one device. Other iOS versions may number the accessibility attributes differently (check with `iph read --raw`).
- Apps whose screen keeps moving (animations, video) take longer to read.
- Notification banners and system pop-ups are not in the app's tree.
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
cargo test          # runs without a phone: parser (real Calculator dumps), selectors, safety guard, recipes
IPH_TRACE=1 iph …   # logs request and dump times to ~/Library/Application Support/iph/<udid>.log
```

## License

MIT, see [LICENSE](LICENSE). The media-session bring-up is adapted from an idevice example tool ([LICENSE.idevice](LICENSE.idevice)).
