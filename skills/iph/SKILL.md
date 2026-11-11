---
name: iph
description: 사용자의 실제 아이폰(시뮬레이터 아님)에서 앱 화면을 읽고 조작한다. `iph` CLI나 iph_* MCP 도구로 화면 요소(접근성 트리)를 읽고 탭 · 입력 · 스크롤하며, 되풀이하는 순서는 레시피로 굳힌다. "아이폰에서 ○○ 해 줘", "폰에서 확인해 줘", "폰 앱에 있는 내역 · 목록 가져와 줘", "폰 앱 자동화", "iph 레시피 만들어 줘"처럼 사용자의 실제 폰 화면을 보거나 눌러야 하는 요청이면 iph를 말하지 않아도 이 스킬을 쓴다. 실제 아이폰 작업에는 mobile-mcp나 스크린샷을 보고 좌표를 누르는 방식보다 이 스킬을 먼저 쓴다. iOS 시뮬레이터, 안드로이드, 앱 빌드 · 설치에는 쓰지 않는다.
---

# iph: operate the user's real iPhone

`iph` reads the iPhone's screen as a list of elements (label, value, role, position) and taps, types and scrolls. It talks to the phone directly; nothing is installed on it. Every command prints one JSON object. If `iph_*` MCP tools are present they take the same arguments and return the same JSON; otherwise run `iph` in the shell.

This is a real phone with the user's accounts on it. A tap cannot be taken back, so the habits below are about not acting on a guess.

## Before the first command

- Use the phone iph is set up for (`--udid`, `IPH_UDID` or `udid` in `~/.config/iph/config.toml`). If several iPhones are connected and none is chosen, iph says so; ask the user which one rather than picking.
- Start with `iph read`. If it fails with `tunnel_failed`, `ax_unavailable` or `ax_timeout`, run `iph doctor` and pass each failed check's `user_action` on to the user in their words.
- Open only the app the task needs.

## The loop: read, act, check

1. **Read** before you act: `iph read` gives `elements[]` with `id`, `role`, `label`, `value`, `frame` (points), `enabled`. Only what is on screen, unless `--all`.
2. **Act** on an element by what it says, not by coordinates: `iph tap "일반"`, `iph type "hello"`, `iph scroll down --until "개발자"`. Coordinates go stale the moment the screen moves; a label is found again each time.
3. **Check** the result, because the phone never confirms that a touch arrived:
   - `screen_changed: false` after a tap means nothing visible happened. Read the screen before deciding what next; do not tap again on the assumption that it was lost.
   - `app_changed: true` means another app came to the front (a banner was hit, a link opened). Stop and tell the user.

## Outcomes

| `outcome` | What to do |
|---|---|
| `done` | Continue. |
| `not_sent` | Nothing reached the phone. Fix the cause (`error.code`, `error.hint`) and try again. |
| `unknown` | Input may have reached the phone. Do not send it again: a second tap on a button that did work is a double order or a double message. Run `iph read` and look. |
| `blocked` | The safety guard refused. See "Money, messages, orders". |

Frequent `error.code`s: `not_found` (`screen_summary.labels` shows what is on screen; scroll or go to the right screen), `ambiguous_selector` (`candidates` lists the matches; narrow with `role=`, `within=`, `index=`), `covered` (something overlaps the target; scroll), `no_focused_field` (tap the text field first), `app_not_allowed` (the user limited iph to certain apps in `[safety] only_apps`; ask before anything else, and do not edit that list yourself).

## Do Not Disturb

A notification banner drops over the top of the screen, is not in the element list, and takes the tap meant for what is under it. So before your first input iph switches the phone to Do Not Disturb by itself (11 to 36 s; Control Center shows for a moment) and reports it as `dnd` in that command's result.

- `dnd.on: false` means it could not. Go on with care near the top of the screen and tell the user why (`dnd.message`, `dnd.hint`).
- When the work is done, give the user's Focus back: `iph dnd off` (MCP: `iph_dnd` with `"action": "off"`). Left alone, iph does it after five minutes without a command, and until then the user misses notifications.
- `control_center_open`: Do Not Disturb could not be set and Control Center may be lying over the app, so the command was not sent. `iph dnd status` closes it; then read the screen.

## Money, messages, orders

A tap on an element labelled with 결제 · 송금 · 보내기 · 이체 · 주문 · 구매 · 지불 · 충전 · pay · purchase · buy · send money · transfer · place order returns `outcome: "blocked"`, `error.code: "confirm_required"`, and nothing is sent.

- Confirming is the user's decision, not yours. Show them what would be pressed (`element`, `matched_word`) and ask.
- After they say yes, repeat the same tap with `--confirm <op_id>` (MCP: `"confirm": "<op_id>"`), using a new id such as `order-20261111-1`.
- An `op_id` works once. If the result is `unknown`, do not make a new id and try again: run `iph op <op_id>`, read the screen, and report.
- The guard only reads labels. A button with no label or a neutral one ("다음", "확인") passes it, so on any screen that moves money or sends something, stop and ask even when the guard is silent.
- Never answer a Face ID, passcode or payment-authentication prompt. Tell the user the phone is waiting for them.

## What is on the screen belongs to the user

Answer with what was asked for. Do not copy readings of banking, messaging or account screens into a repository, a recipe file or notes you keep; a recipe holds the steps, never the data it read.

## Fewer, bigger calls

A fresh reading takes 1 to 2 s and a checked tap about 2 s; that is the phone's pace, not something to retry around. For a sequence you already know, make one `iph batch` or `iph run` call instead of one per step.

`iph batch -` (MCP: `iph_batch`) takes a JSON array of steps and stops at the first failure:

```json
[{"launch": "com.apple.Preferences"},
 {"scroll_until": {"label": "일반"}},
 {"tap": {"label": "일반"}},
 {"tap": {"label": "정보"}},
 {"read_field": {"name": "ios_version", "selector": {"label": "iOS 버전"}, "take": "value"}}]
```

The result has `steps[]` (each with `elapsed_ms`) and `outputs`. On failure: `failed_step`, `reason`, `screen_summary`, and `inputs_sent` (how many earlier steps had already sent input, so think before running the whole thing again).

Steps: `launch`, `wait_for`, `tap` (a selector, or a list of selectors tapped in a row without re-reading, for keypads), `type`, `swipe`, `scroll_until`, `read_field` (`take`: `value` | `label` | `identifier` | `text`, optional `regex` with one capture group), `read_list` (`row` selector, `columns: {name: {take, regex}}`; scrolls and collects each row once), `assert`, `button`, `key`, `sleep` (ms).

## Turning a flow into a recipe

When the user will want the same thing again, freeze it: `iph recipes` lists what exists, `iph run <name> [--arg k=v]` runs one without a model in the loop.

1. Do the flow once with single commands and note the labels `iph read` shows.
2. Write the same steps as YAML in `~/.config/iph/recipes/<name>.yaml` (personal recipes live there, not in a repository).
3. Run it three times. A recipe that passes once can still break on a slow screen; add `wait_for` where it did.

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

More for recipes: `{{today.day}}`, `{{ago.6.month}}` and the like fill in dates; any step may add `when: "a == b"` (or `!=`) to run only then and `hint: "…"` to tell the person what to do if it stops there (a screen that waits for Face ID: follow the tap with a `wait_for` that has a long `timeout_ms` and such a hint). `read_list` takes `group: {selector, regex, as}` to give each row the heading it stands under (a date line), `stop_below: N` to stop once a heading's number falls below N, and `{value: "…"}` columns for a constant.

Mark a step that pays, sends or orders with `confirm: true`; it then runs only with `iph run … --confirm <op_id>`, which the user must approve.

## Selectors

- A plain string is an exact label: `"일반"`. A row labelled `"Wi-Fi, HomeNet"` also matches `"Wi-Fi"`.
- `label~="contains"`, `label/regex/`, `role=button`, `value~="…"`, `id=<identifier or element id>`, `within="<selector>"`, `index=N`, combined with spaces: `role=button label~="삭제"`.
- In MCP, batch and recipes an object works too: `{"label": "일반", "role": "button"}`, with `label_contains`, `label_regex`, `value`, `value_contains`, `id`, `id_contains`, `id_regex`, `within`, `index`.
- Several matches are an error, never a guess. Read the `candidates` and narrow.
- `id=` needs identifiers, which arrive about 0.5 s after the labels (`iph read --full`).

## Typing

`iph type "글"` pastes into the field that has the keyboard, so tap the field first. `--clear` replaces the content, `--submit` presses Return. The result has `value` (what the field holds now) and `clipboard_restored`. The on-screen keyboard disappears for a moment while typing; that is expected.

## Commands at a glance

`read`, `find`, `tap`, `swipe`, `scroll`, `type`, `key`, `button`, `launch`, `apps` (bundle ids), `screenshot`, `batch`, `run`, `recipes`, `op`, `dnd`, `doctor`, `devices`, `stop`. `iph <command> --help` has the flags; add `--human` to read a result yourself.

## When something is off

- **Readings take many seconds, or `ax_timeout`, `input_failed`, `input_unavailable` come back:** the phone is in a slow stretch. Stop sending input, because a late reading can show an earlier screen and a tap would then go to the wrong place. Run `iph stop` once, wait a minute, read again, and tell the user if it does not clear. Slow stretches have tended to follow several stops and starts in a row, so do not loop on that.
- **The element list looks wrong for what the user describes:** `iph screenshot -o <file>` shows what is really on screen. Banners and system pop-ups are never in the list.
- **The phone is locked:** nothing works until the user unlocks it.
- **iph was killed and the phone stayed in Do Not Disturb:** `iph dnd status` shows whether iph set it; `iph dnd off` puts the earlier Focus back.
