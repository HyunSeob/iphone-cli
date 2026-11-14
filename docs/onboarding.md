# 준비 항목: 무엇이 정말 필수인가

`iph doctor`가 보는 항목마다, 실제로 필수인지 어떻게 가렸는지 적습니다. 기기: iPhone 15 Pro Max, iOS 27.0.1, Wi-Fi 연결 (2026-11-10 ~ 11).

가리는 방법은 둘입니다. **꺼서 확인**(그 항목을 없애고 안 되는 것을 봄)과 **구조상 필수**(그 항목이 주는 것 없이는 다음 단계가 성립하지 않음). 이번 작업은 설정을 바꾸지 않는 조건이었기 때문에 '꺼서 확인'은 하지 못했고, 그 사실을 항목마다 적었습니다.

| 항목 | 판정 | 근거 | 꺼서 확인했나 |
|---|---|---|---|
| 짝짓기 (`pairing`) | 필수 (구조상) | 모든 연결이 맥의 짝짓기 기록으로 lockdown 세션을 여는 데서 시작합니다 | 못 함 (짝짓기를 지우면 사람이 폰에서 '신뢰'를 다시 눌러야 함) |
| 연결 (`connection`) | 필수 | usbmuxd가 폰을 보여 줘야 합니다. Wi-Fi만으로 2시간 동안 1분 간격 120번 확인해 매번 보였습니다 | - |
| 개발자 모드 (`developer_mode`) | 필수 (구조상) | 아래 개발자 디스크 이미지가 개발자 모드에서만 연결됩니다 | 못 함 (설정 변경 · 재시동 필요) |
| 개발자 디스크 이미지 (`developer_disk_image`) | 필수 (구조상) | `axAuditDaemon.remoteAXService`, `displayservice`, `hid.universalhidservice`가 이 이미지에서 나옵니다. RSD 목록에 없으면 읽기도 입력도 불가 | 못 함 (이미 연결돼 있었고, 일부러 떼지 않음) |
| 잠금 해제 (`screen_unlocked`) | 필수 | 잠긴 폰은 앱을 앞으로 가져올 수 없습니다 | 못 함 (잠그면 Face ID · 암호가 필요해 사람 없이 풀 수 없음). lockdown의 `PasswordProtected` 값으로만 판정합니다(설정이 앞에 나오지 않는 것은 `settings_in_front` 또는 `accessibility_tree`로 따로 알림). 잠긴 상태에서는 돌려 보지 못했습니다 |
| 접근성 트리 (`accessibility_tree`) | 필수 | 이게 곧 `iph read` | - |
| 터치 입력 (`touch_input`) | 필수 | 설정 목록을 한 칸 내렸다 올려 실제로 움직였는지 봅니다. 내려도 안 움직이면(목록이 끝에 있거나, 새로 연 입력 세션의 첫 동작이 빠진 경우) 올렸다 내려 한 번 더 봅니다. 한 번만 보던 때는 정상인 폰에서 `fail`이 한 번 나왔습니다 | - |
| **UI 자동화 활성화** (설정 → 개발자) | **모름** | 시험 폰은 이 스위치가 **켜져 있었고** 그 상태에서 터치가 됩니다. 꺼도 되는지는 확인하지 못했습니다. 그래서 `doctor`는 이 스위치를 직접 보지 않고, 터치가 실제로 먹는지로 판정한 뒤 안 될 때만 "켜 보세요"라고 안내합니다 | 못 함 (설정 변경 금지) |
| Wi-Fi (`wifi`) | 권장 | 없어도 USB로 됩니다. 이번 검증은 전부 Wi-Fi로 했습니다 | USB로는 돌려 보지 않음 |
| 자동 잠금 (`auto_lock`) | 권장 | 폰이 잠기면 읽기 · 입력이 멈춥니다. 값을 읽어 오지는 않고 안내만 합니다 | - |
| 방해금지 모드 (`do_not_disturb`) | 알림 | 알림 배너가 화면 위쪽 탭을 가로챕니다 (아래). 오래 맡길 때는 `iph dnd on`으로 켜 두세요. `doctor`는 `iph dnd on`으로 켠 것이 아직 남아 있을 때만 알려 줍니다 | 배너는 실제로 한 번 겪음. 방해금지 모드에서 배너가 안 뜨는지는 따로 확인하지 못함 |

## 직접 고쳐 주는 것

- **개발자 모드 메뉴 꺼내기:** 개발자 모드가 꺼져 있으면 `doctor`가 설정에 그 스위치가 보이게 요청합니다 (`auto_fixed: true`). 켜고 재시동하는 것은 사람이 해야 합니다. 시험 폰은 켜져 있어서 이 경로는 돌려 보지 못했습니다. iOS 27.0.1에서 Wi-Fi로 AMFI 서비스에 상태를 물으면 연결이 끊겨서, 상태는 이미지 마운터 서비스(안 되면 lockdown 값)에서 읽습니다.
- **개발자 디스크 이미지 연결:** 서비스가 안 보이면 `cryptexd`로 Cryptex 이미지를 설치합니다 (`src/ddi.rs`). 이미지는 맥의 Xcode 폴더(`/Library/Developer/DeveloperDiskImages/iOS_DDI/Restore`)에 있으면 그것을, 없으면 공개 저장소 doronz88/DeveloperDiskImage의 고정 커밋에서 받아 SHA-256을 확인해 씁니다. 그래도 안 되면 iOS 17.4 이전용 옛 방식(image mounter, Xcode 이미지만)을 시도합니다. 받아 오기 · 확인 · 불러오기는 맥과 윈도에서 시험했지만, 두 시험 폰 모두 이미지가 이미 연결돼 있어서(윈도 폰은 재시동 뒤에도) **폰에 설치하는 단계는 한 번도 실행되지 않았습니다.**

## 후속 확인: 같은 Wi-Fi인데 무선 기기가 보이지 않음

2026-11-14 사용자 제공 실기기 확인: 맥과 iPhone이 같은 Wi-Fi 대역에 있어도 `com.apple.mobile.wireless_lockdown`의 `EnableWifiConnections` / `EnableWifiDebugging`이 비활성화되어 있으면 Bonjour `_apple-mobdev2._tcp.local.` 광고와 usbmuxd `Network` 엔트리가 없었습니다. USB에서 두 값을 `SetValue(..., true)`로 설정하자 광고와 `Network` 엔트리가 나타났고, doctor의 `wifi` 항목도 통과했습니다. 위 표의 11월 10~11일 검증과 별개의 후속 사례입니다.

현재 `doctor`는 USB 연결에서 `SupportsWifi`, `SupportsWifiSyncing`과 두 활성화 플래그를 읽어, 꺼진 플래그와 네트워크 발견 실패를 구분합니다. `setup`과 `doctor`는 기본 실행에서 무선 플래그를 변경하지 않습니다. `setup --fix-wifi` 또는 `doctor --fix-wifi`로 명시적으로 요청하면, 지원 플래그 중 하나가 `true`인 경우 명시적으로 `false`인 활성화 플래그만 켜고 값을 다시 읽습니다. 알 수 없는 값은 바꾸지 않습니다. 설정 후 같은 UDID의 `Network` 엔트리를 최대 4초 확인하며, `auto_fixed: true`만으로 무선 연결 성공을 표시하지 않습니다. 실패 키와 기기 오류·시간 초과는 `detail`에 남습니다.

직접 설정할 때는 Finder/Apple 기기 앱의 'Wi-Fi에 있을 때 이 iPhone 보기'와 Xcode의 'Connect via network'를 확인하세요. 두 플래그가 모두 켜져 있는데 엔트리가 없으면 같은 Wi-Fi 대역·AP/클라이언트 격리·mDNS 차단을 확인합니다. Wi-Fi는 계속 권장 항목으로, 경고가 있어도 USB는 사용할 수 있습니다.

이번 변경의 실기기 검증(macOS, USB 연결 iPhone 15 Pro Max · iOS 27.0.1)에서는 두 지원 플래그와 `EnableWifiConnections`가 `true`, `EnableWifiDebugging`이 `false`였습니다. 수정한 doctor가 꺼진 키를 안내했고, `doctor --fix-wifi --no-touch-test`가 해당 값을 `true`로 설정·재조회해 `auto_fixed: true`를 반환했습니다. 이 기기의 `Network` 엔트리는 4초 안에 나타나지 않아 무선 성공으로 표시하지 않았습니다. 이어 실행한 릴리스 바이너리의 `setup`은 같은 경고를 유지하면서 터치 확인까지 통과해 USB 준비 완료(종료 코드 0)를 반환했습니다. 두 플래그를 모두 켜는 경로, 쓰기 거부·부분 성공·알 수 없는 값·시간 초과·지연된 Network 등록은 가짜 lockdown 응답과 가상 시간으로 회귀 검증합니다.

## 겪은 일: 알림 배너

2026-11-10 23:00, 설정 앱에서 '뒤로' 버튼(화면 왼쪽 위)을 누르는 순간 일기 앱의 알림 배너가 떴고, 탭이 배너에 들어가 일기 앱이 열렸습니다. 배너는 앱의 접근성 트리에 없어서 미리 알 수 없었습니다. 그 뒤로 추가한 것:

- 탭 결과의 `app_changed` (앞에 나온 앱이 바뀌었을 때)
- 스크립트 · batch의 `unexpected_app` (실행한 앱이 아닌 앱이 앞에 있으면 즉시 멈춤)

배너 자체를 감지하지는 못합니다. 2026-11-11부터 `iph`가 입력 전에 방해금지 모드를 스스로 켰고, `[safety] only_apps`로 목록 밖의 앱에서는 입력을 막았습니다. 2026-11-12에 사용자 결정으로 둘 다 없앴습니다. 지금은 `iph dnd on`으로 직접 켜고 `iph dnd off`로 되돌립니다. 방법과 확인한 범위는 `docs/how-it-works.md` 7절에 있습니다.

참고로 사고 당시 이 폰에는 'Default'라는 집중 모드가 켜져 있었을 가능성이 있습니다 (다음 날 아침에 켜져 있었습니다). 집중 모드가 켜져 있다는 것만으로는 배너가 막히지 않습니다. 그 모드가 허용한 앱의 알림은 그대로 뜨기 때문입니다. 그래서 `iph dnd on`은 '아무 집중 모드'가 아니라 방해금지 모드로 바꿉니다 (시험 폰의 방해금지 모드는 허용된 사람 · 앱이 없었습니다).

## `iph doctor` 출력

```json
{
  "ok": true, "outcome": "done", "ready": true, "connection": "wifi", "failing": [],
  "checks": [
    {"check": "pairing", "status": "ok", "required": true, "auto_fixed": false},
    {"check": "developer_mode", "status": "ok", "required": true, "auto_fixed": false},
    {"check": "touch_input", "status": "fail", "required": true, "auto_fixed": false,
     "detail": "the drag was sent but the screen did not move",
     "user_action": "설정 → 개발자 → 'UI 자동화 활성화'를 켜고 다시 시도하세요. 그래도 안 되면 폰을 재시동하세요."}
  ]
}
```

`status`: `ok`, `fail`, `warn`(판정 못 함), `skipped`(앞 단계가 안 돼 건너뜀), `info`(안내만). `ready`는 필수 항목이 모두 `ok`일 때 `true`입니다. 터치 확인은 설정 앱을 앞으로 가져와 목록을 움직이므로, 그게 곤란하면 `--no-touch-test`를 쓰세요 (그러면 `touch_input`이 `skipped`가 되어 `ready`는 `false`입니다).
