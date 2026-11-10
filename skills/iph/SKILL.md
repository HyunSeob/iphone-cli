---
name: iph
description: Read and operate apps on the user's real iPhone with the `iph` CLI or its MCP tools (iph_read, iph_tap, iph_type, iph_scroll, iph_run …) through the accessibility tree. Use when the user asks to do something on their iPhone, check or collect something from a phone app, automate a phone flow, or write an iph recipe. Covers the read-act-verify loop, selectors, outcomes that must not be retried, the pay/send safety guard, and recipes.
---

# iph: operate a real iPhone

`iph` reads the iPhone's screen as a list of elements (label, value, role, position) and taps, types and scrolls. Every command prints one JSON object. The MCP tools (`iph_read`, `iph_tap`, …) take the same arguments and return the same JSON as text.

## The loop

1. **Read** before you act: `iph read`. You get `elements[]` with `id`, `role`, `label`, `value`, `frame` (points), `enabled`. Only what is on screen, unless `--all`.
2. **Act** on an element by what it says, not by coordinates: `iph tap "일반"`, `iph type "hello"`, `iph scroll down --until "개발자"`.
3. **Check the result**, every time:
   - `screen_changed: true` means the screen reacted. `false` after a tap means the tap did nothing visible; read the screen before deciding what next.
   - `app_changed: true` means another app came to the front (a notification banner was hit, a link opened). Stop and tell the user.
4. A fresh read costs about 1.2 s and a verified tap about 1.9 s; that is the phone's pace. For a sequence you already know, use **one** `iph batch` or `iph run` call instead of many single ones.

## Outcomes

| `outcome` | What to do |
|---|---|
| `done` | Continue. |
| `not_sent` | Nothing reached the phone. Fix the cause (`error.code`, `error.hint`) and try again. |
| `unknown` | Input may have reached the phone. **Never send it again.** Run `iph read` and look. |
| `blocked` | The safety guard refused. See below. |

Common `error.code`s: `not_found` (the label is not on screen; `screen_summary.labels` shows what is; scroll or go to the right screen), `ambiguous_selector` (several match; `candidates` lists them; narrow with `role=`, `within=`, `index=`), `covered` (something overlaps the target; scroll), `no_focused_field` (tap the text field first), `tunnel_failed` / `ax_timeout` (phone asleep, locked or off the network; ask the user, or `iph doctor`), `app_not_allowed` (the user limited iph to certain apps).

## Selectors

- A plain string is an exact label: `"일반"`. A row labelled `"Wi-Fi, HomeNet"` also matches `"Wi-Fi"`.
- `label~="contains"`, `label/regex/`, `role=button`, `value~="…"`, `id=<identifier or element id>`, `within="<selector>"`, `index=N`, combined with spaces: `role=button label~="삭제"`.
- In MCP, batch and recipes an object works too: `{"label": "일반", "role": "button"}`, with `label_contains`, `label_regex`, `value`, `value_contains`, `id`, `id_contains`, `id_regex`, `within`, `index`.
- Several matches are an error, never a guess. Read the `candidates` and narrow.
- `id=` needs identifiers, which arrive about 0.5 s after the labels (`iph read --full`).

## Safety guard: paying, sending, ordering

A tap on an element labelled with 결제 · 송금 · 보내기 · 이체 · 주문 · 구매 · 지불 · 충전 · pay · purchase · buy · send money · transfer · place order returns `outcome: "blocked"`, `error.code: "confirm_required"`, and nothing is sent.

- **Do not confirm on your own.** Show the user what would be pressed (`element`, `matched_word`) and ask.
- After the user says yes, repeat the same tap with `--confirm <op_id>` (MCP: `"confirm": "<op_id>"`), where the id is new, e.g. `toss-20261111-1`.
- An `op_id` works once. If the result is `unknown`, do not make a new id and try again: run `iph op <op_id>` and read the screen, then report to the user.
- The guard only reads labels. A button with no label or a neutral one ("다음", "확인") is not caught, so on any screen that moves money or sends a message, stop and ask the user even when the guard is silent.
- Never answer a Face ID, passcode or payment-authentication prompt. Tell the user it is waiting.

## Typing

`iph type "글"` pastes into the field that has the keyboard, so tap the field first. `--clear` replaces the content, `--submit` presses Return. The result has `value` (what the field holds now) and `clipboard_restored`. The on-screen keyboard disappears for a moment while typing; that is expected.

## One call for many steps

`iph batch -` (or `iph_batch`) takes a JSON array of steps and stops at the first failure:

```json
[{"launch": "com.apple.Preferences"},
 {"scroll_until": {"label": "일반"}},
 {"tap": {"label": "일반"}},
 {"tap": {"label": "정보"}},
 {"read_field": {"name": "ios_version", "selector": {"label": "iOS 버전"}, "take": "value"}}]
```

The result has `steps[]` (each with `elapsed_ms`) and `outputs`. On failure: `failed_step`, `reason`, `screen_summary`. The `outcome` of a failed run is the failing step's; `inputs_sent` says how many earlier steps had already sent input, so think before running the whole thing again.

Steps: `launch`, `wait_for`, `tap` (a selector, or a list of selectors tapped in a row without re-reading: for keypads), `type`, `swipe`, `scroll_until`, `read_field` (`take`: `value` | `label` | `identifier` | `text`, optional `regex` with one capture group), `read_list` (`row` selector, `columns: {name: {take, regex}}`, scrolls and collects each row once), `assert`, `button`, `key`, `sleep` (ms).

## Recipes

`iph recipes` lists them; `iph run <name> [--arg k=v]` runs one. A recipe is the same steps in a YAML file, with a name and arguments:

```yaml
name: my-flow
description: what it does
app: com.example.app
args: {query: null}          # null = required, anything else is the default
steps:
  - launch: com.example.app
  - tap: {label: "검색"}
  - type: {text: "{{query}}", submit: true}
  - read_list:
      name: results
      row: {role: button, label_regex: "원$"}
      columns:
        title: {take: label, regex: "^([^,]+)"}
        price: {take: label, regex: "([0-9,]+)원"}
      max_scrolls: 10
output: [results]
```

Write a recipe by doing the flow once with single commands, noting the labels from `iph read`, then turning those commands into steps. Put personal recipes in `~/.config/iph/recipes/<name>.yaml`; never save a recipe or its output with the user's financial or personal data into a repository. Mark a step that pays, sends or orders with `confirm: true`; it then runs only with `iph run … --confirm <op_id>`, which the user must approve.

## When something is wrong

- `iph doctor` checks pairing, Developer Mode, the developer disk image, lock state, reading and touch, and gives a `user_action` for each failure. Pass those on to the user in their words.
- Banners and system pop-ups are not in the element list. If a tap near the top of the screen opened another app, that is what happened. Suggest a Focus (Do Not Disturb).
- A locked phone cannot be operated. Ask the user to unlock it.
- If the element list looks wrong for an app, `iph screenshot -o /tmp/x.png` shows what is really on screen.
