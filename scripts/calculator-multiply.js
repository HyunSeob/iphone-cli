// 계산기로 두 수를 곱해 결과 읽기 (--arg a=12 --arg b=34)

const a = String(args.a ?? "12");
const b = String(args.b ?? "34");
if (!/^[0-9]+$/.test(a) || !/^[0-9]+$/.test(b)) {
  throw new Error("a와 b는 0 이상의 정수여야 합니다");
}

launch("com.apple.calculator");
waitFor("등호");
// 지우기 키는 입력 중이면 '지우기', 아니면 '모두 지우기'라서 둘 다 받고, 두 번 눌러 남은 계산까지 비운다.
const clear = { label_regex: "^(모두 )?지우기$" };
// 목록으로 주면 화면을 다시 읽지 않고 이어서 누른다 (키패드처럼 누르는 동안 움직이지 않는 화면용).
tap([clear, clear, ...a, "곱하기", ...b, "등호"]);
// 계산기 표시 창은 이름표가 없고 식별자에 값이 들어 있다: StandardInputView;value:408
const shown = readField({ selector: { id_regex: "^StandardInputView" }, take: "identifier", regex: "value:(.*)$" });
return { result: shown };
