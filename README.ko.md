# iphone-cli (`iph`)

AI 에이전트가 **실제 아이폰**의 아무 앱이나 화면 요소(접근성 트리)로 읽고 누르게 해 주는 CLI + MCP 서버입니다. XCUITest · WebDriverAgent · Xcode 서명 없이, Xcode가 쓰는 애플 개발자 서비스에 직접 붙습니다. 자주 하는 순서는 **스크립트**(자바스크립트)로 적어 AI 왕복 없이 한 번에 실행합니다.

[Poki](https://github.com/JosssphZhou/poki)의 원리를 따라 만들었습니다. 코드는 가져오지 않았고, Poki 문서에 적힌 동작 원리와 직접 실험한 결과로 새로 썼습니다. 기기 통신은 [idevice](https://github.com/jkcoxson/idevice)(MIT)에 기대고 있습니다.

English: [README.md](README.md)

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
{"ok":true,"outcome":"done","elapsed_ms":1752,"sent":true,"verified":true,"screen_changed":true,…}

$ iph run settings-version
{"ok":true,"outcome":"done","elapsed_ms":10788,"steps":[…],"outputs":{"ios_version":"27.0.1"},…}
```

## 되는 것 · 안 되는 것

| | |
|---|---|
| 호스트 | macOS, Windows 10 · 11 (x64) |
| 기기 | 실제 아이폰, **iOS 27 이상** (검증은 **iPhone 15 Pro Max · iOS 27.0.1 한 대**에서만 했습니다). 터치 · 입력이 폰의 화면 공유 서비스(`com.apple.coredevice.displayservice`, iOS 27부터)를 거치기 때문에 iOS 26 이하에서는 입력이 안 될 것으로 봅니다 (돌려 보지는 않았습니다) |
| 연결 | USB 또는 같은 Wi-Fi (맥은 Wi-Fi로, 윈도는 USB로만 검증) |
| 범위 밖 | 시뮬레이터, 안드로이드, Linux 호스트, 가로 화면 |

윈도는 iPhone19,4 · iOS 27.2 한 대를 USB로 꽂아 `doctor` · 읽기 · 터치 · 스크립트를 확인했습니다.

## 설치

Rust도 Xcode도 필요 없습니다. 맥에서는 터미널에 한 줄이면 됩니다 (`curl` · `tar` · `shasum` 같은 맥 기본 도구만 씁니다).

```sh
curl -fsSL https://raw.githubusercontent.com/HyunSeob/iphone-cli/main/install.sh | sh
iph setup                     # 폰 준비 상태를 차례로 확인하고, 할 일을 알려 줍니다
```

최신 [릴리스](https://github.com/HyunSeob/iphone-cli/releases)의 유니버설 바이너리(애플 실리콘 · 인텔)를 받아 체크섬을 확인하고 `~/.local/bin/iph`에 넣습니다. 다른 폴더는 `IPH_INSTALL_DIR`, 특정 버전은 `IPH_VERSION=v0.1.0`으로 고릅니다. 윈도는 릴리스에서 `iph-windows-x86_64.zip`을 받아 `iph.exe`를 PATH에 있는 폴더에 두면 됩니다.

- **맥:** 폰을 USB로 꽂고 폰에서 '신뢰'를 누르면 짝짓기가 됩니다.
- **윈도:** 애플의 기기 연결 서비스가 있어야 합니다. Microsoft Store의 **Apple 기기**(Apple Devices) 앱을 깔거나, `winget install Apple.AppleMobileDeviceSupport`로 서비스만 깔면 됩니다. 그 뒤 폰을 USB로 꽂고 '신뢰'를 누르세요.
- **개발자 디스크 이미지:** 폰에 없으면 `iph doctor`가 연결합니다. 맥에 Xcode가 있으면 Xcode의 이미지를 쓰고, 없으면 공개 저장소 [doronz88/DeveloperDiskImage](https://github.com/doronz88/DeveloperDiskImage)에서 처음 한 번 받아 옵니다 (약 18 MB, 커밋과 SHA-256 고정). iph 저장소에는 애플 파일이 들어 있지 않습니다.

소스에서 빌드하려면 Rust 1.88 이상이 필요합니다 (윈도는 Visual Studio Build Tools(C++)도).

```sh
git clone https://github.com/HyunSeob/iphone-cli
cd iphone-cli
cargo install --path .        # ~/.cargo/bin/iph
```

## 처음 준비

`iph setup`은 `iph doctor`를 돌리고, 안 된 항목마다 폰에서 할 일을 한 줄로 알려 준 뒤 다시 확인합니다. 에이전트는 `iph doctor`(JSON)의 `checks[].user_action`을 읽어 사용자를 안내하면 됩니다.

| 항목 | 필수 | 내용 |
|---|---|---|
| `pairing` | 필수 | 맥이 이 폰의 짝짓기 기록을 갖고 있어야 합니다. 없으면 USB로 연결하고 폰에서 '신뢰' |
| `connection` | 필수 | USB 또는 같은 Wi-Fi에서 폰이 보여야 합니다 |
| `developer_mode` | 필수 | 설정 → 개인정보 보호 및 보안 → 개발자 모드. 꺼져 있으면 `iph doctor`가 그 메뉴를 꺼내 줍니다 |
| `developer_disk_image` | 필수 | 접근성 · 화면 · 입력 서비스가 여기서 나옵니다. 없으면 `iph doctor`가 받아서 연결합니다 (위 '설치') |
| `screen_unlocked` | 필수 | 폰 잠금이 풀려 있어야 합니다. 잠김 여부는 lockdown의 값으로만 판정합니다 |
| `settings_in_front` | 필수 | 설정 앱이 앞에 나와야 합니다 (다른 창이 가리면 실패). 화면을 읽을 수 없을 때는 이 대신 `accessibility_tree`가 실패합니다 |
| `accessibility_tree` | 필수 | 화면 요소가 실제로 읽히는지 |
| `touch_input` | 필수 | 설정 앱 목록을 한 칸 내렸다 올려서 터치가 실제로 먹는지 확인합니다 (`--no-touch-test`로 건너뜀) |
| `wifi` | 권장 | 케이블 없이 쓰려면 |
| `auto_lock` | 권장 | 오래 쓸 때는 자동 잠금 '안 함' |
| `do_not_disturb` | 알림 | `iph dnd on`으로 켠 방해금지 모드가 아직 켜져 있을 때만 나옵니다 |

어느 항목이 실제로 필수였는지, 무엇을 확인하지 못했는지는 [docs/onboarding.md](docs/onboarding.md)에 적었습니다.

**알림 배너는 화면 위쪽 탭을 가로챕니다.** 배너는 화면 위쪽을 덮고 접근성 트리에는 나오지 않습니다. 그때 화면 위쪽을 누르면 배너가 눌려 다른 앱이 열립니다 (만드는 동안 실제로 한 번 겪었습니다). 오래 맡길 때는 `iph dnd on`으로 방해금지 모드를 켜 두고, 끝나면 `iph dnd off`로 되돌리세요. 자세한 내용은 아래 [방해금지 모드](#방해금지-모드).

## 명령

모든 명령은 JSON을 냅니다. 사람이 볼 때는 `--human`. 기기는 `--udid`, `IPH_UDID`, `~/.config/iph/config.toml`의 `udid`, 연결된 폰이 하나면 그 폰 순서로 고릅니다. 폰이 여럿인데 고르지 않으면 목록과 함께 오류를 냅니다.

| 명령 | 하는 일 |
|---|---|
| `iph devices` | 연결된 폰, iOS 버전, 연결 방식, 준비 상태 |
| `iph setup` / `iph doctor` | 준비 · 점검 |
| `iph read [--app] [--all] [--full] [--raw] [--max-age ms]` | 화면 요소 목록. `--all` 화면 밖 포함, `--full` 식별자 포함(약 0.5초 더), `--raw` 번호 붙은 속성 전부 |
| `iph find <셀렉터>` | 셀렉터에 맞는 요소 |
| `iph tap <셀렉터 \| x,y> [--long ms] [--double]` | 요소 가운데(또는 좌표) 탭 |
| `iph swipe <up\|down\|left\|right \| x1,y1,x2,y2> [--duration ms] [--drag]` | 밀기. `--drag`는 멈춘 뒤 손을 떼서 관성 없이 옮깁니다 |
| `iph scroll <up\|down> [--until 셀렉터] [--max N]` | 한 칸 스크롤, 또는 요소가 보일 때까지 |
| `iph type <글> [--clear] [--submit]` | 키보드가 올라온 칸에 붙여넣기로 입력 (한글 · 이모지 가능). 클립보드는 되돌리고, 칸의 값을 확인합니다 |
| `iph key <키>` | `return`, `esc`, `tab`, `cmd+a` … |
| `iph button <home\|lock\|volume_up\|volume_down>` | 하드웨어 버튼 |
| `iph launch <bundle id \| 이름> [--restart]` / `iph apps` | 앱 실행 · 목록 |
| `iph screenshot [-o 파일]` | PNG 캡처 |
| `iph batch <파일 \| ->` | JSON 단계 배열을 한 번에 |
| `iph script <파일 \| -> [--arg k=v]` | 자바스크립트를 바로 실행 (`-`는 표준 입력) |
| `iph run <이름> [--arg k=v]` / `iph scripts` | 저장된 스크립트 실행 · 목록 |
| `iph mcp [--tools a,b]` | MCP 서버 (stdio) |
| `iph dnd <on\|off\|status>` | 방해금지 모드를 켜기, 원래 집중 모드로 되돌리기, 지금 상태 보기 |
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

스크립트 · MCP · batch에서는 객체로도 씁니다: `{label, label_contains, label_regex, value, value_contains, value_regex, role, id, id_contains, id_regex, within, index}`.

### 결과

```json
{"ok": true, "outcome": "done", "elapsed_ms": 1752, "sent": true, "verified": true, "screen_changed": true, "element": {…}}
{"ok": false, "outcome": "not_sent", "error": {"code": "not_found", "message": "…", "hint": "…"}, "elapsed_ms": 1210, "screen_summary": {…}}
```

| `outcome` | 뜻 | 종료 코드 |
|---|---|---|
| `done` | 끝까지 실행됨. `ok: false`와 같이 오면 할 일은 다 했지만 찾던 것이 없었다는 뜻입니다 (아래) | 0, `ok: false`면 1 |
| `not_sent` | 폰에 아무것도 가지 않음. 다시 해도 안전 | 1 |
| `unknown` | 입력이 나갔을 수 있음. **다시 보내지 말고** 화면을 읽어 확인 | 2 |

종료 코드는 `ok: true`일 때만 0입니다. `batch` · `script` · `run`도 같습니다 (아무것도 안 갔으면 1). `ok: false`인데 `outcome: "done"`인 것은 `scroll --until`이 못 찾았을 때 하나입니다: 스크롤은 실제로 다 나갔으므로 `not_sent`(아무것도 안 감)가 아니고, 결과는 `error.code: "not_found"` · `found: false` · 종료 코드 1입니다. 다시 해도 위험하지 않지만 화면은 그만큼 움직여 있습니다.

터치 신호는 폰이 받았다고 답하지 않습니다. 그래서 `sent`(보냄)와 `verified`(보낸 뒤 화면을 다시 읽음)를 나누고, 탭 · 입력 · 스크롤 뒤에는 화면을 다시 읽어 `screen_changed`를 넣습니다. 다른 앱이 앞으로 나왔으면 `app_changed: true`가 붙습니다.

## 설정

```toml
# ~/.config/iph/config.toml
udid = "00008130-…"

[dnd]
# focus = "업무"          # iph dnd on이 방해금지 모드 대신 켤 집중 모드 (폰에 보이는 이름 그대로)
# control = "Focus"      # 폰 언어가 한국어 · 영어가 아닐 때, 제어 센터의 집중 모드 항목 이름

[daemon]
input_idle_secs = 120    # 입력 세션을 이만큼 안 쓰면 닫음. 0이면 명령마다 닫음
idle_exit_secs = 1800    # 요청이 이만큼 없으면 상주 프로세스 종료
screen_video = false     # 입력 중 화면 영상도 받을지. 터치에는 필요 없고, 무선에서는 터널을 막아 느려집니다
restart_accessibility = true  # 폰의 접근성 서비스가 답을 멈추거나 빈 답만 보내면 끝내서 폰이 새로 띄우게 함 (약 20초 걸림)

[script]
timeout_secs = 300       # 스크립트 하나가 돌 수 있는 시간 (최대 840)
memory_mb = 64           # 스크립트가 쓸 수 있는 메모리
```

입력을 보내려면 폰이 '화면 공유 중' 상태여야 합니다 (폰 상단에 표시가 뜹니다). 이 상태는 소리 연결만으로 만들고, 화면 영상은 받지 않습니다 (`screen_video`). 화면 공유 중에 내용을 가리거나 경고하는 앱이라면 `input_idle_secs = 0`으로 두세요. 입력을 보내는 동안만 공유 상태가 되고, 대신 입력마다 3~4초쯤 더 걸립니다. 읽기는 화면 공유와 상관없습니다.

`iph`는 누르는 대상을 가리지 않습니다. 결제 · 송금 · 주문 버튼도 다른 버튼과 똑같이 누르고, 앞에 어떤 앱이 있든 입력을 보냅니다. 무엇을 누를지는 부르는 쪽이 정합니다.

### 방해금지 모드

- **켜기 · 끄기:** `iph dnd on`은 방해금지 모드를 켜고, `iph dnd off`는 켜기 전의 집중 모드로 되돌립니다. `iph`가 스스로 켜거나 끄지는 않습니다.
- **되돌리는 곳:** `on` 때 켜져 있던 집중 모드(없었으면 '없음')를 `~/Library/Application Support/iph/<udid>.dnd.json`에 적어 두고, `off` 때 그대로 되돌립니다. 원래 방해금지 모드였으면 손대지 않고, 그 사이에 사람이 다른 집중 모드로 바꿨으면 그것도 그대로 둡니다. 상주 프로세스가 끝나도 기록은 남습니다.
- **방법:** 폰에 이걸 바꿔 주는 서비스가 없어서 사람과 같은 방법을 씁니다. 제어 센터를 내리고, '집중 모드'를 길게 눌러 목록에서 고르고, 홈을 두 번 눌러 닫습니다. 그래서 화면에 제어 센터가 잠깐 보이고, 한 번에 보통 11~15초, 상주 프로세스가 막 떴을 때는 19~36초가 걸립니다.
- **필요한 것:** 제어 센터 첫 페이지에 '집중 모드' 항목이 있고, 앱을 쓰는 중에도 제어 센터가 열려야 합니다. 폰 언어가 한국어 · 영어가 아니면 `[dnd]`의 `control`, `focus`에 폰에 보이는 이름을 적으세요.
- **막지 못하는 것:** 방해금지 모드에서도 뜨는 것(알람 · 타이머, 긴급 재난 문자, 방해금지 모드 설정에서 허용해 둔 사람 · 앱)과 시스템 팝업. 방해금지 모드가 켜진 동안 실제로 배너가 안 뜨는지는 따로 확인하지 않았습니다 (iOS의 동작에 기댑니다).

## 스크립트

자주 하는 순서는 자바스크립트로 적어 한 번에 실행합니다. `read`, `tap`, `scroll` 같은 명령이 그대로 함수이고, 반복 · 조건 · 변수는 언어의 것을 씁니다. 엔진(QuickJS)이 `iph` 안에 들어 있어 따로 설치할 것은 없습니다.

```js
// 설정에서 iOS 버전 읽기
launch("com.apple.Preferences", { restart: true });
scroll("down", { until: "일반" });
tap("일반");
tap("정보");
return { ios_version: readField({ selector: { label: "iOS 버전" }, take: "value" }) };
```

```sh
iph script flow.js --arg city=서울     # 파일을 바로 실행
iph script - <<'EOF'                   # 표준 입력으로
return { front: read({ app: true }).apps[0].name };
EOF
iph run settings-version               # 저장된 스크립트를 이름으로
iph scripts                            # 저장된 스크립트 목록
```

```json
{"ok": true, "outcome": "done", "elapsed_ms": 10712, "outputs": {"ios_version": "27.0.1"}, "logs": [], "calls": 5, "inputs_sent": 4, "script": "settings-version", "source": "builtin:settings-version"}
```

- **저장하는 곳:** `~/.config/iph/scripts/<이름>.js`(개인, 우선) → 저장소 `scripts/`(예제, 바이너리에 들어 있음). 파일의 첫 주석 줄이 `iph scripts`에 설명으로 나옵니다.
- **인자와 결과:** `--arg k=v`는 `args.k`로 들어옵니다 (글자이므로 숫자는 `Number(args.n)`). `return`한 객체와 `output(k, v)`로 넣은 값이 결과의 `outputs`가 되고, `log(…)`로 남긴 줄은 `logs`에 담깁니다.

| 함수 | 내용 |
|---|---|
| `read(opts)` · `find(selector, opts)` · `apps()` | 같은 이름의 명령과 인자 · 결과가 같습니다 |
| `tap(target, {long_ms, double, timeout_ms, settle})` | 셀렉터면 나타날 때까지 최대 5초 기다리고, 두 번 연속 같은 자리에 있을 때 누릅니다 (막 뜬 화면은 줄이 밀릴 수 있습니다). 끄려면 `settle: false` |
| `tap([a, b, …])` | 지금 화면에서 여러 개를 이어서 탭. 중간에 다시 읽지 않습니다 (키패드용) |
| `swipe(방향 \| {from, to}, {duration_ms, drag})` · `scroll(방향, {until, max})` | 밀기 · 스크롤 |
| `type(글, {clear, submit})` · `key(이름)` · `button(이름)` | 입력 · 키 · 버튼 |
| `launch(앱, {restart})` | 실행하고 앞에 나왔는지 확인. `restart`는 껐다 켜서 첫 화면에서 시작 |
| `waitFor(selector, {timeout_ms})` | 요소가 나올 때까지 (기본 8초). 시스템 창(Face ID 등)이 앱을 잠깐 덮어도 기다립니다 |
| `readField({selector, take, regex})` | 값 하나를 글자로. `take`: `value`(기본), `label`, `identifier`, `text` |
| `readList({row, columns, key, group, stop_below, until, max_scrolls, limit})` | 목록을 스크롤하며 겹치지 않게 모아 `{rows, scrolls, ended}`로 돌려줍니다 |
| `sleep(ms)` · `log(…)` · `output(k, v)` | 대기 · 기록 · 결과에 값 넣기 |

- 함수가 돌려주는 것은 같은 이름의 CLI 명령이 내는 JSON 객체 그대로입니다 (`tap("일반").screen_changed`, `read().elements`). 셀렉터는 문자열과 객체 둘 다 됩니다.
- **실패는 예외입니다.** 실패한 호출은 `Error`를 던지고, `e.code`, `e.outcome`, `e.hint`, `e.result`(결과 객체 전체)가 붙어 있습니다. 잡지 않으면 스크립트가 거기서 멈추고 결과에 `failed_call: {cmd, line, outcome}`, `reason: {code, message}`, `screen_summary`가 담깁니다. 추측해서 진행하지 않습니다. 없어도 되는 것을 눌러 볼 때만 `try { … } catch (e) { … }`로 잡으세요.
- **멈춘 스크립트의 `outcome`:** 멈추기 전에 입력을 하나라도 보냈으면 `unknown`(처음부터 다시 돌리면 그 입력을 또 보내게 됩니다), 아무것도 보내지 않았으면 `not_sent`입니다. `inputs_sent`에 입력을 보낸 호출 수가 있습니다.
- **`unknown` 뒤에는 이어 가지 못합니다.** 입력이 나갔는지 모르는 호출(`outcome: "unknown"`)이 나오면 스크립트를 그 자리에서 끝냅니다. `try`로 잡아도 다음 호출은 폰에 가지 않습니다.
- 어느 호출이든 옵션에 `hint: "…"`를 주면, 그 호출에서 멈췄을 때 사람에게 보여 줄 안내가 됩니다 (Face ID를 기다리는 `waitFor`에 "폰을 봐 주세요").
- `launch` 뒤에는 그 앱이 계속 앞에 있어야 합니다. 다른 앱이 나오면 입력과 `readField` · `readList`가 `unexpected_app`으로 멈춥니다.
- **`readList`:** `columns`는 `{이름: {take, regex, selector}}` 또는 모든 행에 같은 값을 넣는 `{이름: {value: "…"}}`입니다. `group: {selector, regex, as}`는 행 사이의 머리글(날짜 줄 등)을 그 아래 행들의 칸으로 붙이고, `stop_below: N`은 머리글의 숫자가 N보다 작아지면 멈춥니다. `key: "칸 이름"`은 그 칸이 같으면 같은 행으로 봅니다. 읽는 사이에 글자가 바뀌는 행(시각, 남은 시간)이 있는 목록에 씁니다. 없으면 행 전체가 같아야 같은 행입니다.
- `readList`는 화면마다 한 번 읽고, 그 화면의 마지막 두 줄이 다음 화면 맨 위에 오도록 스크롤 거리를 스스로 정합니다 (폰 크기와 앱에 따라 달라서 읽은 위치로 계산합니다). 겹치는 두 줄로 앞뒤 화면을 이어 붙이므로 사이에 빠진 줄이 없는지 알 수 있습니다. 목록 끝이라고 판단하기 전에는 위아래로 한 번 움직여 확인합니다.
- **칸막이:** 스크립트는 파일 · 네트워크 · 다른 프로세스에 닿지 못합니다 (모듈 불러오기와 타이머도 없습니다). `Date`, `JSON`, `RegExp`, `Map` 같은 언어 기능은 그대로 있습니다. `[script] timeout_secs`(기본 300초)나 `memory_mb`(기본 64MB)를 넘으면 중단합니다.

예제: [`settings-version`](scripts/settings-version.js), [`calculator-multiply`](scripts/calculator-multiply.js), [`clock-world-list`](scripts/clock-world-list.js), 반복 · 조건을 쓰는 [`settings-list`](scripts/settings-list.js). 금융 · 배달 같은 개인 스크립트는 저장소에 넣지 말고 `~/.config/iph/scripts/`에 두세요.

### batch

`iph batch`는 JSON 단계 배열을 순서대로 실행하고 첫 실패에서 멈춥니다. 반복이나 조건이 필요 없는 짧은 순서용입니다.

| 단계 | 내용 |
|---|---|
| `launch: <앱>` 또는 `{app, restart}` | 실행하고 앞에 나왔는지 확인 |
| `wait_for: <셀렉터>` 또는 `{selector, timeout_ms}` | 요소가 나올 때까지 (기본 8초) |
| `tap: <셀렉터>` 또는 `{selector, long_ms, double, timeout_ms}` · `tap: [셀렉터, …]` | 탭 · 이어서 탭 |
| `type: <글>` 또는 `{text, clear, submit}` · `swipe: <방향>` 또는 `{from, to, duration_ms, drag}` | 입력 · 밀기 |
| `scroll_until: <셀렉터>` 또는 `{selector, direction, max}` | 보일 때까지 스크롤 |
| `read_field: {name, selector, take, regex}` · `read_list: {name, row, columns, …}` | 값 하나 · 목록 읽기 (`readField` · `readList`와 같음) |
| `assert: <셀렉터>` · `button: home` · `key: return` · `sleep: 500` | 있어야 진행 · 버튼 · 키 · 대기(ms) |
| (어느 단계든) `when: "a == b"` · `hint: "…"` | 조건 · 멈췄을 때 안내 |

단계가 실패하면 `{failed_step, reason, screen_summary, inputs_sent}`를 돌려주고, 단계별 `elapsed_ms`가 `steps`에 들어 있습니다.

## MCP

```sh
claude mcp add iph -- iph mcp                                   # Claude Code
codex mcp add iph -- iph mcp                                    # Codex
iph mcp --tools read,find,script                                # 일부 도구만 내놓기
```

에이전트가 `iph`를 알아서 쓰게 하려면 스킬을 걸어 둡니다 (MCP 등록 없이 셸에서 `iph`를 부르는 것만으로도 됩니다).

```sh
ln -s "$PWD/skills/iph" ~/.claude/skills/iph                    # Claude Code
ln -s "$PWD/skills/iph" ~/.codex/skills/iph                     # Codex
```

도구: `iph_read`, `iph_find`, `iph_tap`, `iph_swipe`, `iph_scroll`, `iph_type`, `iph_key`, `iph_button`, `iph_launch`, `iph_apps`, `iph_screenshot`, `iph_batch`, `iph_script`, `iph_run`, `iph_scripts`, `iph_doctor`, `iph_dnd`. 인자와 결과는 CLI와 같습니다. `iph_script`는 `code`(자바스크립트)와 `args`를, `iph_run`은 `name`과 `args`를 받습니다. 에이전트용 안내는 [skills/iph/SKILL.md](skills/iph/SKILL.md)에 있습니다.

## 속도

iPhone 15 Pro Max · iOS 27.0.1 · Wi-Fi, 상주 프로세스가 붙어 있는 상태에서 잰 값입니다 (2026-11-11).

| 동작 | 걸린 시간 |
|---|---|
| `iph read` (설정 첫 화면, 10회) | 중앙값 1.35초, 최대 2.10초 |
| `iph read --full` (10회) | 중앙값 1.45초, 최대 1.79초 |
| `iph tap` + 화면 확인 (20회, 놓침 0) | 중앙값 1.75초, 최대 4.30초 |
| `iph scroll` 한 칸 + 확인 | 2.1~2.6초 |
| `iph type` (메모, 확인 · 클립보드 복원 포함) | 4~7초 |
| 스크립트 `settings-version` (호출 5번, USB로 연결) | 9.9~13.3초 (8회) |
| 상주 프로세스가 없을 때 첫 `iph read` (터널 + 구독 포함) | 약 2~3초 |
| 방해금지 모드 켜기 또는 되돌리기 | 연결이 붙어 있을 때 중앙값 12.5초 (11회, 10.9~14.2초). 상주 프로세스를 막 띄웠을 때 19~36초 |

한 번 읽는 데 1초 넘게 걸리는 것은 폰 쪽 사정입니다. 폰은 요청을 받고 약 1.1초 조용히 기다린 뒤에야 화면을 내보냅니다. 화면이 스스로 자주 바뀌는 앱에서는 옛 화면을 읽지 않으려고 두 번 읽어 맞춰 보기 때문에 단계마다 1~2초가 더 듭니다. 자세한 관찰은 [docs/how-it-works.md](docs/how-it-works.md).

## 알려진 한계

- 검증한 기기는 한 대입니다. 다른 iOS 버전에서는 접근성 속성 번호가 다를 수 있습니다 (`iph read --raw`로 확인).
- 화면이 계속 움직이는 앱(애니메이션, 동영상)은 읽기가 더 오래 걸립니다.
- 알림 배너 · 시스템 팝업은 앱의 트리에 나오지 않습니다. 배너는 `iph dnd on`으로 줄일 수 있지만, 방해금지 모드에서도 뜨는 것은 막지 못합니다.
- 새 입력 세션이 터치를 하나도 받지 않은 일이 한 번 있었습니다 (원인 모름, 세션 십여 개 중 하나). `iph dnd`는 제어 센터 내리기가 먹지 않으면 세션을 새로 열어 다시 시도합니다.
- **폰의 응답이 느려지는 구간이 있습니다.** 2026-11-11 오전 두 시간 동안 네 번(짧게는 몇 분, 길게는 40분) 읽기가 2~30초씩 걸리거나 시간 초과됐고, 입력 연결이 끊기기도 했습니다. 한 번은 다른 작업이 맥의 CPU를 다 쓰고 있을 때였고, 나머지는 원인을 모릅니다 (상주 프로세스를 연달아 껐다 켠 직후가 많았습니다). 이런 구간에서는 늦게 도착한 옛 화면을 보고 움직일 수 있어서, 방해금지 모드 전환이 실패하거나 제어 센터가 열린 채 남거나 앱에서 홈 화면으로 나간 일이 있었습니다. `iph stop` 뒤 1분쯤 기다렸다 다시 하면 돌아왔습니다. 자세한 기록은 [docs/how-it-works.md](docs/how-it-works.md) 7절.
- 세로 화면만 다룹니다.
- `iph apps`의 이름은 폰이 알려 주는 이름(대개 영어)입니다. 실행은 bundle id가 확실합니다.
- `iph type`은 클립보드의 글자(또는 그림 한 장)만 되돌립니다. **앱에서 복사한 내용(메모의 글처럼 서식이 함께 있는 것)은 되돌리지 못합니다.** 그 내용은 컴퓨터에서 읽으려 하면 폰의 클립보드가 1분쯤 멈추기 때문에 읽지 않고, 입력이 끝나면 클립보드를 비웁니다. 결과에 `clipboard_restored: false`와 `clipboard_note`가 나옵니다.
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
cargo test          # 폰 없이 도는 시험: 파서(실제 계산기 화면 덤프), 셀렉터, 스크립트
IPH_TRACE=1 iph …   # 상주 프로세스 로그에 요청 · 덤프 시각을 남김 (~/Library/Application Support/iph/<udid>.log)
IPH_PCAP=<폴더> iph … # 터널의 패킷을 Wireshark용 파일로 남김. 화면 내용이 들어 있으니 다 보면 지울 것
```

## 라이선스

MIT. [LICENSE](LICENSE). 미디어 세션 시작 부분은 idevice의 예제 도구를 고쳐 썼습니다 ([LICENSE.idevice](LICENSE.idevice)). `src/tcp`는 터널의 TCP 스택으로, jktcp 0.1.8(MIT)을 가져와 연결을 포기하기까지의 시간 상수 두 개를 늘리고 패킷 기록의 마이크로초 값을 고친 것입니다 ([LICENSE.jktcp](LICENSE.jktcp)).
