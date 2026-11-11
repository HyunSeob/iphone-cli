// 설정에서 iOS 버전 읽기

// 설정은 마지막으로 보던 화면을 기억하므로, 껐다 켜서 첫 화면에서 시작한다.
launch("com.apple.Preferences", { restart: true });
// '일반'은 폰에 따라 첫 화면 아래쪽에 있어 스크롤해야 보인다.
scroll("down", { until: "일반" });
tap("일반");
tap("정보");
// 행의 이름표가 "iOS 버전, 27.0.1"처럼 붙어 있다. take: "value"는 쉼표 뒤를 읽는다.
return { ios_version: readField({ selector: { label: "iOS 버전" }, take: "value" }) };
