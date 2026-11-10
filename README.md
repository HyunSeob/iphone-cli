# iphone-cli (`iph`)

AI 에이전트가 **실제 아이폰**의 아무 앱이나 화면 요소(접근성 트리)로 읽고 누르게 해 주는 CLI + MCP 서버입니다. XCUITest · WebDriverAgent · Xcode 서명 없이, Xcode가 쓰는 애플 개발자 서비스에 직접 붙습니다. 자주 하는 순서는 **레시피**로 묶어 AI 왕복 없이 한 번에 실행합니다.

[Poki](https://github.com/JosssphZhou/poki)의 원리를 따라 만들었습니다. 코드는 가져오지 않았고, Poki 문서에 적힌 동작 원리와 직접 실험한 결과로 새로 썼습니다. 기기 통신은 [idevice](https://github.com/jkcoxson/idevice)(MIT)에 기대고 있습니다.

English: [README.en.md](README.en.md)

```console
$ iph read --human
설정  430x932  15 elements
application   "설정"  @0,0 430x932
  header        "설정"  @20,119.3 53.7x37
  switch        "에어플레인 모드" = "0"  @20,489.3 390x49.3
  button        "Wi-Fi, HomeNet"  @20,538.7 390x49.3
  button        "일반"  @20,700 390x49.3
  …

$ iph tap "일반"
{"ok":true,"outcome":"done","elapsed_ms":1863,"sent":true,"verified":true,"screen_changed":true,…}

$ iph run settings-version
{"ok":true,"outcome":"done","elapsed_ms":8252,"steps":[…],"outputs":{"ios_version":"27.0.1"},…}
```

## 되는 것 · 안 되는 것

| | |
|---|---|
| 호스트 | macOS |
| 기기 | 실제 아이폰, iOS 18 이상 (검증은 **iPhone 15 Pro Max · iOS 27.0.1 한 대**에서만 했습니다) |
| 연결 | USB 또는 같은 Wi-Fi (검증은 Wi-Fi로만) |
| 범위 밖 | 시뮬레이터, 안드로이드, Windows · Linux 호스트, 가로 화면 |

## 설치

Rust(1.85 이상)와, 처음 한 번 Xcode가 필요합니다 (폰 짝짓기와 개발자 디스크 이미지).

```sh
git clone https://github.com/HyunSeob/iphone-cli
cd iphone-cli
cargo install --path .        # ~/.cargo/bin/iph
iph setup                     # 폰 준비 상태를 차례로 확인하고, 할 일을 알려 줍니다
```

## 처음 준비

`iph setup`은 `iph doctor`를 돌리고, 안 된 항목마다 폰에서 할 일을 한 줄로 알려 준 뒤 다시 확인합니다. 에이전트는 `iph doctor`(JSON)의 `checks[].user_action`을 읽어 사용자를 안내하면 됩니다.

| 항목 | 필수 | 내용 |
|---|---|---|
| `pairing` | 필수 | 맥이 이 폰의 짝짓기 기록을 갖고 있어야 합니다. 없으면 USB로 연결하고 폰에서 '신뢰' |
| `connection` | 필수 | USB 또는 같은 Wi-Fi에서 폰이 보여야 합니다 |
| `developer_mode` | 필수 | 설정 → 개인정보 보호 및 보안 → 개발자 모드. 꺼져 있으면 `iph doctor`가 그 메뉴를 꺼내 줍니다 |
| `developer_disk_image` | 필수 | 접근성 · 화면 · 입력 서비스가 여기서 나옵니다. Xcode의 '기기 및 시뮬레이터'에서 폰을 한 번 고르면 연결됩니다 |
| `screen_unlocked` | 필수 | 폰 잠금이 풀려 있어야 합니다 |
| `accessibility_tree` | 필수 | 화면 요소가 실제로 읽히는지 |
| `touch_input` | 필수 | 설정 앱 목록을 한 칸 내렸다 올려서 터치가 실제로 먹는지 확인합니다 (`--no-touch-test`로 건너뜀) |
| `wifi` | 권장 | 케이블 없이 쓰려면 |
| `auto_lock` | 권장 | 오래 쓸 때는 자동 잠금 '안 함' |

어느 항목이 실제로 필수였는지, 무엇을 확인하지 못했는지는 [docs/onboarding.md](docs/onboarding.md)에 적었습니다.

**권장: 쓰는 동안 집중 모드(방해금지)를 켜 두세요.** 알림 배너는 화면 위쪽을 덮고 접근성 트리에는 나오지 않습니다. 그때 화면 위쪽을 누르면 배너가 눌려 다른 앱이 열립니다 (만드는 동안 실제로 한 번 겪었습니다). `iph`는 다른 앱이 앞에 나오면 결과에 `app_changed`를 넣고 레시피를 멈추지만, 누르는 것 자체를 막지는 못합니다.

## 명령

모든 명령은 JSON을 냅니다. 사람이 볼 때는 `--human`. 기기는 `--udid`, `IPH_UDID`, `~/.config/iph/config.toml`의 `udid`, 연결된 폰이 하나면 그 폰 순서로 고릅니다. 폰이 여럿인데 고르지 않으면 목록과 함께 오류를 냅니다.

| 명령 | 하는 일 |
|---|---|
| `iph devices` | 연결된 폰, iOS 버전, 연결 방식, 준비 상태 |
| `iph setup` / `iph doctor` | 준비 · 점검 |
| `iph read [--app] [--all] [--full] [--raw] [--max-age ms]` | 화면 요소 목록. `--all` 화면 밖 포함, `--full` 식별자 포함(약 0.5초 더), `--raw` 번호 붙은 속성 전부 |
| `iph find <셀렉터>` | 셀렉터에 맞는 요소 |
| `iph tap <셀렉터 \| x,y> [--long ms] [--double] [--confirm op_id]` | 요소 가운데(또는 좌표) 탭 |
| `iph swipe <up\|down\|left\|right \| x1,y1,x2,y2> [--duration ms] [--drag]` | 밀기. `--drag`는 멈춘 뒤 손을 떼서 관성 없이 옮깁니다 |
| `iph scroll <up\|down> [--until 셀렉터] [--max N]` | 한 칸 스크롤, 또는 요소가 보일 때까지 |
| `iph type <글> [--clear] [--submit]` | 키보드가 올라온 칸에 붙여넣기로 입력 (한글 · 이모지 가능). 클립보드는 되돌리고, 칸의 값을 확인합니다 |
| `iph key <키>` | `return`, `esc`, `tab`, `cmd+a` … |
| `iph button <home\|lock\|volume_up\|volume_down>` | 하드웨어 버튼 |
| `iph launch <bundle id \| 이름> [--restart]` / `iph apps` | 앱 실행 · 목록 |
| `iph screenshot [-o 파일]` | PNG 캡처 |
| `iph batch <파일 \| ->` | JSON 단계 배열을 한 번에 |
| `iph run <레시피> [--arg k=v] [--confirm op_id]` / `iph recipes` | 레시피 실행 · 목록 |
| `iph op <op_id>` | 확인하고 누른 동작의 기록 조회 |
| `iph mcp [--tools a,b]` | MCP 서버 (stdio) |
| `iph stop` | 이 폰의 상주 프로세스 종료 |

### 셀렉터

- 문자열 하나: 정확한 이름표. `iph tap "일반"`
- `label="정확히"`, `label~="포함"`, `label/정규식/`
- `role=button` (application, button, switch, text_field, search_field, key, tab, header, text, image, link, adjustable, other)
- `id=…` 접근성 식별자, 또는 `iph read`가 준 요소 `id`. `id~=`, `id/정규식/`도 됩니다
- `value="…"`, `value~=…`
- `within="<셀렉터>"` 그 요소 안쪽만, `index=N` N번째(0부터)
- 여러 개가 맞으면 멋대로 고르지 않고 `ambiguous_selector` 오류와 후보를 냅니다

설정 앱의 행처럼 이름표가 `"Wi-Fi, HomeNet"`(이름, 값)인 경우, `label="Wi-Fi"`는 쉼표 앞 이름과도 맞습니다.

레시피 · MCP · batch에서는 객체로도 씁니다: `{label, label_contains, label_regex, value, value_contains, value_regex, role, id, id_contains, id_regex, within, index}`.

### 결과

```json
{"ok": true, "outcome": "done", "elapsed_ms": 1863, "sent": true, "verified": true, "screen_changed": true, "element": {…}}
{"ok": false, "outcome": "not_sent", "error": {"code": "not_found", "message": "…", "hint": "…"}, "elapsed_ms": 1210, "screen_summary": {…}}
```

| `outcome` | 뜻 | 종료 코드 |
|---|---|---|
| `done` | 끝까지 실행됨 | 0 |
| `not_sent` | 폰에 아무것도 가지 않음. 다시 해도 안전 | 1 |
| `unknown` | 입력이 나갔을 수 있음. **다시 보내지 말고** 화면을 읽어 확인 | 2 |
| `blocked` | 안전장치가 막음 | 3 |

터치 신호는 폰이 받았다고 답하지 않습니다. 그래서 `sent`(보냄)와 `verified`(보낸 뒤 화면을 다시 읽음)를 나누고, 탭 · 입력 · 스크롤 뒤에는 화면을 다시 읽어 `screen_changed`를 넣습니다. 다른 앱이 앞으로 나왔으면 `app_changed: true`가 붙습니다.

## 안전장치

- **위험 단어:** 누르려는 요소의 이름표에 결제, 결제하기, 송금, 보내기, 이체, 주문, 주문하기, 구매, 구매하기, 결제 완료, 지불, 충전, pay, purchase, buy, send money, transfer, place order가 있으면 누르지 않고 `outcome: "blocked"`, `error.code: "confirm_required"`를 돌려줍니다. 한글은 이름표 어디에 있어도 걸리고, 영어는 단어 단위로 봅니다 ("Payment methods"는 통과).
- **`--confirm <op_id>`:** 사람이 확인했을 때 새 ID를 붙이면 누릅니다. 같은 `op_id`는 다시 누르지 않습니다. 누르기 전에 `~/Library/Application Support/iph/ops.jsonl`(0600)에 먼저 적으므로, 중간에 죽어도 그 ID는 쓴 것으로 남습니다. 결과가 불확실하면 새 ID로 재시도하지 말고 `iph op <op_id>`로 조회만 하세요.
- **레시피의 `confirm: true`:** 그 단계는 `iph run … --confirm <op_id>` 없이는 멈춥니다.
- **`only_apps`:** `~/.config/iph/config.toml`에 적으면 그 앱들만 실행하고, 다른 앱이 앞에 있으면 입력을 막습니다.

```toml
# ~/.config/iph/config.toml
udid = "00008130-…"

[safety]
add = ["기부"]            # 위험 단어 추가
remove = ["보내기"]        # 기본 단어 제외
only_apps = ["com.apple.Preferences", "com.apple.calculator"]
```

**한계:** 이름표만 봅니다. 이름표가 없거나 다른 말로 붙은 버튼(아이콘만 있는 결제 버튼, "다음", "확인")은 막지 못합니다. 돈이나 메시지가 걸린 흐름은 레시피에서 마지막 단계에 `confirm: true`를 직접 달아 두세요.

## 레시피

```yaml
name: settings-version
description: 설정에서 iOS 버전 읽기
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

| 단계 | 내용 |
|---|---|
| `launch: <앱>` 또는 `{app, restart}` | 실행하고 앞에 나왔는지 확인. `restart`는 껐다 켜서 첫 화면에서 시작 |
| `wait_for: <셀렉터>` 또는 `{selector, timeout_ms}` | 요소가 나올 때까지 (기본 8초) |
| `tap: <셀렉터>` 또는 `{selector, long_ms, double, timeout_ms}` | 탭. 대상이 나올 때까지 잠깐 기다립니다 |
| `tap: [셀렉터, …]` | 지금 화면에서 여러 개를 이어서 탭. 중간에 다시 읽지 않습니다 (키패드용) |
| `type: <글>` 또는 `{text, clear, submit}` | 입력 |
| `swipe: <방향>` 또는 `{from, to, duration_ms, drag}` | 밀기 |
| `scroll_until: <셀렉터>` 또는 `{selector, direction, max}` | 보일 때까지 스크롤 |
| `read_field: {name, selector, take, regex}` | 값 하나 읽기. `take`: `value`(기본), `label`, `identifier`, `text` |
| `read_list: {name, row, columns, until, max_scrolls, limit}` | 목록을 스크롤하며 겹치지 않게 모으기. `columns`는 `{이름: {take, regex, selector}}` |
| `assert: <셀렉터>` | 이 요소가 있어야 진행 |
| `button: home` · `key: return` · `sleep: 500` | 버튼 · 키 · 대기(ms) |
| (어느 단계든) `confirm: true` | `--confirm <op_id>`가 있어야 실행 |

- `{{이름}}`은 `--arg 이름=값`으로 바뀝니다. `args`에 기본값을 적고, `null`이면 필수입니다.
- 단계가 실패하면 즉시 멈추고 `{failed_step, reason, screen_summary}`를 돌려줍니다. 추측해서 진행하지 않습니다.
- `launch` 뒤에는 그 앱이 계속 앞에 있어야 합니다. 다른 앱이 나오면 `unexpected_app`으로 멈춥니다.
- 단계별 `elapsed_ms`가 결과에 들어 있습니다.
- 찾는 곳: `~/.config/iph/recipes/*.yaml`(개인, 우선) → 저장소 `recipes/`(예제, 바이너리에 들어 있음). 파일 경로를 직접 줘도 됩니다.

예제: [`settings-version`](recipes/settings-version.yaml), [`calculator-multiply`](recipes/calculator-multiply.yaml), [`clock-world-list`](recipes/clock-world-list.yaml). 금융 · 배달 같은 개인 레시피는 저장소에 넣지 말고 `~/.config/iph/recipes/`에 두세요.

## MCP

```sh
claude mcp add iph -- iph mcp                                   # Claude Code
codex mcp add iph -- iph mcp                                    # Codex
iph mcp --tools read,find,run                                   # 일부 도구만 내놓기
```

도구: `iph_read`, `iph_find`, `iph_tap`, `iph_swipe`, `iph_scroll`, `iph_type`, `iph_key`, `iph_button`, `iph_launch`, `iph_apps`, `iph_screenshot`, `iph_batch`, `iph_run`, `iph_recipes`, `iph_doctor`. 인자와 결과는 CLI와 같습니다. 에이전트용 안내는 [skills/iph/SKILL.md](skills/iph/SKILL.md)에 있습니다.

## 속도

iPhone 15 Pro Max · iOS 27.0.1 · Wi-Fi, 상주 프로세스가 붙어 있는 상태에서 잰 값입니다 (2026-11-11).

| 동작 | 걸린 시간 |
|---|---|
| `iph read` (설정 첫 화면, 10회) | 중앙값 1.20초, 최대 1.37초 |
| `iph read --full` (10회) | 중앙값 1.45초, 최대 1.79초 |
| `iph tap` + 화면 확인 (20회, 놓침 0) | 중앙값 1.86초, 최대 2.58초 |
| `iph scroll` 한 칸 + 확인 | 2.1~2.6초 |
| `iph type` (메모, 확인 · 클립보드 복원 포함) | 2.5~5초 |
| 레시피 `settings-version` (5단계) | 8.2~13.2초 |
| 상주 프로세스가 없을 때 첫 `iph read` (터널 + 구독 포함) | 약 2~3초 |

한 번 읽는 데 1초 넘게 걸리는 것은 폰 쪽 사정입니다. 폰은 요청을 받고 약 1.1초 조용히 기다린 뒤에야 화면을 내보냅니다. 자세한 관찰은 [docs/how-it-works.md](docs/how-it-works.md).

## 알려진 한계

- 검증한 기기는 한 대입니다. 다른 iOS 버전에서는 접근성 속성 번호가 다를 수 있습니다 (`iph read --raw`로 확인).
- 화면이 계속 움직이는 앱(애니메이션, 동영상)은 읽기가 더 오래 걸립니다.
- 알림 배너 · 시스템 팝업은 앱의 트리에 나오지 않습니다.
- 세로 화면만 다룹니다.
- `iph apps`의 이름은 폰이 알려 주는 이름(대개 영어)입니다. 실행은 bundle id가 확실합니다.
- `iph type`은 클립보드의 글자(또는 그림 한 장)만 되돌립니다. 서식 있는 복사 내용은 글자만 남습니다.
- 잠긴 폰에서는 동작하지 않습니다. Face ID · 암호 입력은 대신하지 않습니다.
- 개발자 디스크 이미지 자동 연결은 코드만 있고 실제로 돌려 보지 못했습니다 (시험 폰은 이미 연결돼 있었습니다).

## 구조

`iph` 한 번 실행 = 상주 프로세스(`iph daemon`, 첫 호출 때 자동으로 뜸)에 유닉스 소켓(`~/Library/Application Support/iph/<udid>.sock`)으로 요청 하나. 상주 프로세스가 폰별로 터널, 접근성 구독, 터치 입력 세션을 붙잡고 요청을 차례대로 처리합니다. 30분 동안 요청이 없으면 스스로 끝납니다.

```
iph (CLI) ─┐
iph mcp  ──┼─ unix socket ─ iph daemon ─ lockdown → CoreDeviceProxy → 사용자 공간 TCP 터널 → RSD
           ┘                              ├ axAuditDaemon.remoteAXService   (화면 요소)
                                          ├ displayservice + universalhid   (터치 · 키보드)
                                          ├ pasteboardservice               (클립보드)
                                          ├ appservice                      (앱 실행 · 목록)
                                          └ screencaptureservice            (캡처)
```

## 개발

```sh
cargo test          # 폰 없이 도는 시험: 파서(실제 계산기 화면 덤프), 셀렉터, 안전장치, 레시피
IPH_TRACE=1 iph …   # 상주 프로세스 로그에 요청 · 덤프 시각을 남김 (~/Library/Application Support/iph/<udid>.log)
```

## 라이선스

MIT. [LICENSE](LICENSE). 미디어 세션 시작 부분은 idevice의 예제 도구를 고쳐 썼습니다 ([LICENSE.idevice](LICENSE.idevice)).
