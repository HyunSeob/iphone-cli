# iphone-cli (`iph`) v0.1 스펙

확정: 2026-11-10, 운영자(이현섭)와 인터뷰로 정함. 구현은 별도 세션.
쓰임: 허드의 AI딸깍 "폰 앱 자동화" 편 (내일 오전 촬영 준비). 영상 기획: `~/code/videos/channels/hud-ai/videos/toss-automation/PLAN.md`

## 1. 한 줄

AI 에이전트가 **실제 아이폰**의 아무 앱이나 화면 요소(접근성 트리)로 읽고 누르게 해 주는 CLI + MCP 서버. **XCUITest · WebDriverAgent · Xcode 서명 없이**, Xcode가 쓰는 애플 개발자 서비스에 직접 붙는다 (Poki의 원리를 따름). 자주 하는 순서는 **스크립트**(자바스크립트)로 적어 AI 왕복 없이 한 번에 실행한다.

## 2. 정한 것

| 항목 | 결정 |
|---|---|
| 언어 · 바탕 | Rust + [`idevice`](https://github.com/jkcoxson/idevice) (MIT) |
| 주 사용자 | AI 에이전트 (Claude Code · Codex). CLI 출력은 JSON 기본, `iph mcp`로 MCP 서버, 에이전트용 스킬 문서 동봉. 사람은 디버깅 때만 |
| 명령 · 저장소 | 명령 `iph`, 저장소 · 크레이트 `iphone-cli` (crates.io · PyPI · npm 모두 비어 있음, 이 맥에 `iph` 없음: 11-10 확인) |
| 첫 버전 범위 | 기본 동작 + 스크립트(자바스크립트, §9. 2026-11-11에 YAML 레시피에서 바꿈). 자동 경로 학습(Poki routes 같은 것)은 다음 버전 |
| 안전장치 | 없음 (§7. 2026-11-12에 사용자 결정으로 모두 없앰) |
| 라이선스 · 공개 | MIT. `HyunSeob/iphone-cli` **비공개**로 시작, 영상 올릴 때 공개. README에 "Poki(https://github.com/JosssphZhou/poki)의 원리를 따라 만들었다" 명시 |
| 스크립트 공개 | 저장소에는 기본 앱(설정 · 계산기 · 메모 · 시계) 예제만. 토스 · 배달 등 개인 스크립트는 `~/.config/iph/scripts/` (저장소 밖) |
| 지원 | macOS 호스트 + 실제 아이폰 iOS 18+ (검증은 iOS 27에서만). 범위 밖: 시뮬레이터, 안드로이드, Windows/Linux 호스트 |
| 온보딩 | `iph setup` (대화형) + `iph doctor --json` (§8) |
| 안내 문구 언어 | 시스템 언어를 따름: 한국어 맥은 한국어, 그 외 영어. JSON 키 · 오류 코드는 항상 영어 |

## 3. 클린룸 규칙 (공개 전제)

- **Poki(AGPL-3.0)와 pymobiledevice3(GPL-3.0)의 코드를 복사하거나 옮겨 쓰지 않는다.** 문서에 적힌 동작 원리, 그리고 아래 §4의 관찰 사실만 참고한다. 의존성으로도 넣지 않는다.
- idevice(MIT)는 의존성으로 쓰고, 필요하면 포크 · 패치 가능 (MIT 고지 유지).
- 시험 고정 데이터(fixture)는 기본 앱 화면에서만 만든다. 토스 등 실제 금융 · 개인 정보가 든 트리는 저장소 · 로그 · fixture 어디에도 남기지 않는다.

## 4. 2026-11-10 시험에서 관찰한 사실 (구현 출발점)

기기: iPhone 15 Pro Max `00008130-001E219222F9001C`, iOS 27.0.1, 개발자 모드 켜짐, Wi-Fi 연결. 도구: pymobiledevice3 11.50.3 (관찰용으로만).

- **맥의 Xcode CoreDevice 터널은 쓸 수 없었다.** `devicectl`이 `tunnelState=connected`, `tunnelIPAddress=fda7:aa22:c7::1`을 보고하지만 맥에 해당 utun · 경로가 없고 접속이 시간 초과. → **자체 터널**(lockdown → CoreDeviceProxy → 사용자 공간 TCP)로 붙어야 한다. pymobiledevice3의 루트 없는 사용자 공간 터널은 1.25초에 열렸다. idevice도 사용자 공간 터널을 지원하는지 먼저 확인할 것 (§11 첫 항목)
- 기기 RSD가 광고한 관련 서비스 (포트는 매번 바뀜, RSD에서 이름으로 찾을 것):
  - `com.apple.accessibility.axAuditDaemon.remoteAXService` (트리 전체)
  - `com.apple.accessibility.axAuditDaemon.remoteserver.shim.remote` (검사기 포커스 이동 방식, 느림)
  - `com.apple.coredevice.displayservice`, `com.apple.coredevice.hid.universalhid`, `com.apple.coredevice.hid.universalhidservice`, `com.apple.coredevice.hid.indigo`, `com.apple.coredevice.pasteboardservice`, `com.apple.coredevice.screencaptureservice`
- **트리 읽기 (`remoteAXService`):** DTX 연결 → 제어 채널에 `clientNeedsAccessibility:`(true) 한 번 → 기기가 `processDataFromRemoteDevice:` 메시지로 bytes 인자를 밀어 줌. 두 개가 옴(약 37KB, 약 196KB). 각각 `bplist00` NSKeyedArchive.
  - 최상위 문자열에 `treeDump`, `treeDumpType`, `TokenType`, `ElementData`, `AXElementTokenSimulator`
  - 클래스: `AXPTranslatorResponse`(약 240), `AXPTranslationObject`(약 37), `NSValue`, `NSDictionary`(키가 **숫자**: AX 속성 번호로 보임), `NSMutableData`(안쪽에 또 다른 아카이브가 있을 수 있음: Poki 문서는 "두 겹"이라 함)
  - 이름표 문자열, 프레임 문자열 `{{x, y}, {w, h}}`(논리 포인트), `AXGestureTapCountKey` 등 제스처 키가 들어 있음
  - 연결부터 큰 덤프 도착까지 2.7~3.3초 (매번 새 연결, 3회). 구독을 유지한 채 다시 받는 시간은 안 잼. Poki 문서: 정지 화면에서 구독 후 약 1.1~1.2초, 키보드 애니메이션 중엔 자주 빈 응답 → 2초 뒤 재구독
  - 할 일: 숫자 속성 키 → 의미(label, value, identifier, frame, traits, role 등) 대응을 기본 앱 화면으로 실험해 찾기
- **검사기 포커스 이동 방식:** 토스 홈 요소 40개를 이름표로 읽음(금액 포함), 11.2초, 위치 없음. 대체 경로로만 남긴다

## 5. 구조

- **상주 프로세스 `iphd`(같은 바이너리의 `iph daemon`):** 기기별로 터널, RSD, AX 구독, 터치 입력 세션(display service 미디어 세션 + HID), 클립보드 연결을 붙잡고 있는다. 첫 `iph` 호출 때 자동으로 뜬다. CLI · MCP는 유닉스 소켓(`~/Library/Application Support/iph/<udid>.sock`)으로 요청한다. 요청은 기기별로 차례대로 처리한다
- 끊김(케이블 · Wi-Fi · 재시동)은 다음 요청 때 다시 연결. **입력이 이미 나갔을 수 있는 요청은 절대 다시 보내지 않는다** (결과 `outcome: "unknown"`)
- 기기 선택: `--udid`, 없으면 연결된 실제 기기가 하나일 때 그것. 여럿이면 오류 + 목록

## 6. 명령 (모두 `--json` 기본, `--human`으로 사람용 출력)

| 명령 | 하는 일 |
|---|---|
| `iph devices` | 연결된 실제 기기, iOS 버전, 연결 방식, 준비 상태 |
| `iph setup` / `iph doctor [--json]` | 온보딩 · 점검 (§8) |
| `iph read [--app] [--all]` | 현재 화면 요소 목록: `id`, `role`, `label`, `value`, `identifier`, `frame`, `traits`, `enabled`, 부모 · 깊이. 기본은 화면 안 요소만 |
| `iph find <selector>` | 셀렉터에 맞는 요소 |
| `iph tap <selector \| x,y>` | 요소 중심(또는 좌표) 탭. `--long <ms>`, `--double` |
| `iph swipe <방향 \| x1,y1,x2,y2>` / `iph scroll <up\|down> [--until <selector>] [--max N]` | 밀기 · 찾을 때까지 스크롤 |
| `iph type <text> [--clear] [--submit]` | 클립보드 붙여넣기로 입력(한글 지원), 사용자 클립보드는 원래대로 되돌림(앱에서 복사한 서식 있는 내용은 예외: 읽을 수 없어 비움), 입력 후 필드 값 확인 |
| `iph key <키>` / `iph button <home\|lock\|volume_up\|volume_down>` | 키 · 버튼 |
| `iph launch <bundle_id \| 앱 이름>` / `iph apps` | 앱 실행 · 목록 |
| `iph screenshot [-o file]` | 화면 캡처 (보조) |
| `iph batch <file \| ->` | JSON 단계 배열을 한 번에 (일단 그대로 둠. 스크립트와 겹치므로 없앨지는 따로 정함) |
| `iph script <file \| -> [--arg k=v]` | 자바스크립트를 바로 실행 (§9) |
| `iph run <이름> [--arg k=v]` / `iph scripts` | 저장된 스크립트 실행 · 목록 |
| `iph mcp` | MCP 서버(stdio) |

- **셀렉터:** `label="정확히"`, `label~="포함"`, `label/정규식/`, `role=button`, `id=...`(identifier), `value~=...`, `within=<셀렉터>`, `index=N`. 문자열 하나만 주면 `label="..."`로 본다. 여러 개가 맞으면 오류 + 후보 목록 (멋대로 첫 번째를 누르지 않는다)
- **모든 입력 뒤 확인:** 탭 · 입력 · 스크롤은 트리를 다시 읽어 `screen_changed: true|false`를 결과에 넣는다. 터치 신호는 기기가 응답하지 않으므로 `sent`와 `verified`를 구분한다. 탭은 화면이 바뀌고(최대 2.5초) 두 번 연속 같게 읽힐 때까지(합쳐 최대 5초) 기다린 뒤 그 화면(`elements`)을 결과에 넣는다. 폰이 화면 전환을 알리지 않아서 브라우저의 페이지 로드 대기를 이렇게 흉내 낸다. `--no-wait`이면 바로 한 번만 읽고 화면은 넣지 않는다. `type`은 붙여 넣은 뒤 칸을 한 번 읽어 보이는 값(`value`)만 돌려주고 맞는지 판정하지 않는다. 앱이 서식 · 가림 · 자동 고침으로 보이는 값을 바꾸기 때문이다 (Playwright `fill`과 같은 분담)
- **결과 공통 형태:** `{ok, outcome: "done"|"not_sent"|"unknown", error?: {code, message, hint}, elapsed_ms, ...}`. 종료 코드는 `ok: true`면 0, 아니면 outcome에 따라 unknown 2 · 그 밖 1 (`scroll --until`이 못 찾은 `ok: false` + `done`도 1). batch · script · run도 같다
- **MCP 도구:** CLI와 1:1 (`iph_read`, `iph_find`, `iph_tap`, `iph_swipe`, `iph_scroll`, `iph_type`, `iph_button`, `iph_launch`, `iph_batch`, `iph_script`, `iph_run`, `iph_scripts`, `iph_doctor`). `iph_script`는 `code` + `args`, `iph_run`은 `name` + `args`. 속도 비교 실험의 A 방식(에이전트가 단계마다 판단)은 이 MCP 기본 도구로, B 방식은 `iph_run`으로 한다

## 7. 안전장치

없음. 2026-11-12에 사용자 결정으로 위험 단어 확인(`--confirm <op_id>`, 스크립트의 `{confirm: true}`, `iph op`), `only_apps`, 입력 전 방해금지 모드 자동 전환을 모두 없앴다. `iph`는 누르는 대상과 앞에 나온 앱을 가리지 않는다. 방해금지 모드는 `iph dnd on|off`로 직접 켜고 끈다.

남은 규칙은 결과를 정확히 알리는 것 하나다: 입력이 나갔는지 모르면 `outcome: "unknown"`으로 알리고, 스크립트는 그런 호출 뒤에 멈춘다 (예외를 잡아도 그 뒤 호출은 폰에 가지 않는다).

## 8. 온보딩 (`iph setup`, `iph doctor`)

순서대로 확인하고, 안 된 항목은 사용자가 폰에서 할 일을 한 줄로 안내한 뒤 다시 확인한다. CLI가 직접 할 수 있는 건 직접 한다.

| 항목 | 확인 · 처리 |
|---|---|
| 맥 신뢰(짝짓기) | 짝짓기 기록 확인. 없으면 "USB로 연결하고 폰에서 '신뢰'를 누르세요" |
| 개발자 모드 | 상태 확인. 꺼져 있으면 메뉴 꺼내기(AMFI)를 직접 하고 "설정 → 개인정보 보호 및 보안 → 개발자 모드 → 켜기 → 재시동 후 비밀번호" 안내 |
| 개발자 디스크 이미지 | 필요한 경우 자동 연결 |
| UI Automation (개발자 설정) | 터치 입력이 실제로 먹는지 시험 탭(빈 곳 아닌 안전한 동작)으로 판단. 안 되면 켜라고 안내 |
| Wi-Fi 연결 | 같은 네트워크에서 무선 연결 가능 여부. 처음엔 USB 필요 |
| 자동 잠금 | 권장 항목: "안 함" 권하기 (실패 아님) |
| 화면 잠김 | 잠겨 있으면 "폰 잠금을 풀어 주세요" |

- 각 항목은 `required` / `recommended`로 표시한다. **어느 항목이 정말 필수인지는 구현 중 실제로 가려 문서에 적는다** (지금은 추정)
- `iph doctor --json`은 에이전트가 사용자를 안내할 수 있게 `{check, status, required, user_action, auto_fixed}` 배열을 낸다

## 9. 스크립트 (JavaScript)

2026-11-11 운영자 결정: YAML 레시피를 없애고 `iph` 안에 넣은 자바스크립트 엔진(QuickJS, `rquickjs`)으로 바꾼다. 날짜 계산 · 조건 · 반복이 필요할 때마다 YAML에 문법을 덧붙이는 대신 언어를 그대로 쓴다. "똑같은 건 두 개 필요 없다"로 YAML은 남기지 않는다. 이름도 '레시피'에서 '스크립트'로 바꿨다 (명령 `iph scripts`, 폴더 `scripts/`).

```js
// 설정에서 iOS 버전 읽기            ← 첫 주석 줄이 `iph scripts`의 설명
launch("com.apple.Preferences", { restart: true });
scroll("down", { until: "일반" });
tap("일반");
tap("정보");
return { ios_version: readField({ selector: { label: "iOS 버전" }, take: "value" }) };
```

- **실행:** `iph script <파일 | ->`(바로 실행, heredoc 가능), `iph run <이름>`(저장된 것), `iph scripts`(목록). MCP는 `iph_script`(`code` + `args`), `iph_run`(`name` + `args`), `iph_scripts`
- **찾는 곳:** `~/.config/iph/scripts/<이름>.js`(개인, 우선) → 저장소 `scripts/`(예제, 바이너리에 포함)
- **인자와 결과:** `--arg k=v`는 `args.k`(문자열). `return`한 객체와 `output(k, v)`가 결과의 `outputs`, `log(...)`가 `logs`. 결과에는 `calls`(호출 수)와 `inputs_sent`(입력을 보낸 호출 수)도 들어간다
- **함수 = 명령:** `read`, `find`, `tap`, `swipe`, `scroll`(`until` 포함), `type`, `key`, `button`, `launch`, `apps`. 인자 이름과 돌려주는 객체는 CLI · MCP와 같다. 셀렉터는 문자열 · 객체 둘 다
- **스크립트에만 있는 것:** `waitFor(selector, {timeout_ms})`, `readField({selector, take, regex})` → 문자열, `readList({row, columns, key, group, stop_below, …})` → `{rows, scrolls, ended}`, `sleep(ms)`, `log(...)`, `output(k, v)`. `tap`은 셀렉터가 나타날 때까지 최대 5초 기다리고 자리가 멈춘 뒤 누르며(`timeout_ms`, `settle`), 배열을 주면 다시 읽지 않고 이어서 누른다. 어느 호출이든 `hint`를 줄 수 있다
- **실패는 예외:** 실패한 호출은 `Error`를 던진다 (`e.code`, `e.outcome`, `e.hint`, `e.result` = 결과 객체 전체). 잡지 않으면 스크립트가 멈추고 `{failed_call: {cmd, line, outcome}, reason: {code, message}, screen_summary, inputs_sent, outputs, logs}`를 돌려준다. 화면이 예상과 다르면 추측해서 진행하지 않는다
- **멈춘 스크립트의 `outcome`:** 입력을 하나라도 보낸 뒤 멈추면 `unknown`(다시 실행하면 그 입력을 또 보낸다), 아무것도 안 보냈으면 `not_sent`
- **칸막이:** 파일 · 네트워크 · 프로세스 접근 없음 (모듈 로더 · 타이머 없음, 폰으로 가는 문은 위 함수들뿐). 실행 시간 `[script] timeout_secs`(기본 300, 최대 840), 메모리 `[script] memory_mb`(기본 64). 넘으면 중단하고, 입력을 이미 보낸 뒤였으면 `outcome: "unknown"`
- **`unknown` 뒤 중단:** §7
- `launch` 뒤 다른 앱이 앞에 나오면 입력과 `readField` · `readList`가 `unexpected_app`으로 멈춘다

## 10. 완료 기준 (오늘 밤, 사람 없이)

검증 기기: 15 Pro Max. **기본 앱(설정 · 계산기 · 메모 · 시계)만** 조작. 설정값은 바꾸지 않는다. 메모는 시험용 메모만 만들고 지운다. 토스 · 금융 · 결제 · 메시지 앱은 열지 않는다.

1. `iph doctor --json`이 이 폰에서 모든 required 통과, 각 항목이 실제로 필수인지 결과가 문서에 적힘
2. `iph read`: 설정 홈 요소가 이름표 · 위치와 함께 나옴. 데몬 유지 상태에서 다시 읽기 시간을 10회 재서 기록(중앙값 · 최대)
3. `iph tap "일반"` → 화면 바뀜 `screen_changed: true`, 뒤로 가기까지 20회 반복 중 놓침 횟수 기록
4. `iph type`: 메모에 한글 · 영문 · 이모지 섞인 글 입력 → 필드 값 일치, 클립보드 원복 확인 → 메모 삭제
5. `iph scroll --until`: 설정에서 화면 밖 항목 찾기
6. 예제 레시피(지금은 스크립트) 2개 이상(설정 iOS 버전 읽기, 계산기 `12×34=` 결과 읽기 등)이 3회 연속 성공
7. 안전장치: 위험 단어 요소는 fixture 트리로 단위 시험(실제 결제 화면 안 씀). `--confirm` 재사용 거부 시험 (2026-11-12에 안전장치를 없애 해당 없음)
8. `iph mcp`를 Claude Code와 Codex 둘 다에 붙여 `iph_read` · `iph_tap` 호출 성공
9. Wi-Fi만으로 동작 (케이블 없이)
10. README(한국어 · 영어), 에이전트 스킬 문서, Poki 원리 명시, 설치 방법

내일 아침 운영자와 함께: 토스 인증 후 `toss-spending` 개인 스크립트(이번 달 지출 목록) 맞추기.

## 11. 작업하며 먼저 확인할 것 (막히면 보고)

1. **idevice가 루트 없이 Wi-Fi로 자체 터널을 여는가** (CoreDeviceProxy + 사용자 공간 TCP). 안 되면: USB 우선으로 가고 Wi-Fi 방법을 보고
2. `remoteAXService` 덤프의 숫자 속성 키 해석, 두 겹 아카이브, 화면 안 요소만 고르기
3. 구독 유지 시 다시 읽기 시간, 애니메이션 중 빈 응답 처리
4. display service 미디어 세션 + HID 탭이 이 iOS에서 먹는지, UI Automation 설정이 필요한지
5. 한글 입력(클립보드 서비스 + Command-V) 동작

## 12. 작업 환경 규칙

- 빌드 산출물 · 툴체인은 외장 디스크: `CARGO_TARGET_DIR=/Volumes/AppsBuilds/AppWork/iphone-cli/target`, rustup · cargo 홈도 `/Volumes/AppsBuilds/AppWork/Toolchains/` 아래. 외장이 없으면 멈추고 보고
- 기기 조작은 §10 범위만. 송금 · 결제 · 메시지 전송 · 설정 변경 금지. 인증(Face ID · 비밀번호)이 필요하면 멈추고 아침에 보고
- 커밋은 이 저장소 안에서만 자유롭게. GitHub 저장소 생성은 `HyunSeob/iphone-cli` **비공개**로. 공개 전환 · crates.io 배포는 하지 않는다
- 이 맥에 실제 아이폰이 두 대 붙어 있다. **15 Pro Max(`00008130-001E219222F9001C`)만** 쓴다. 다른 아이폰(`00008160-…`)은 건드리지 않는다
