// 설정 > 일반의 항목 이름을 위에서부터 찾는 항목까지 모으기 (--arg until="VPN 및 기기 관리")

const until = args.until || "VPN 및 기기 관리";
const maxScrolls = Number(args.max || 10);

launch("com.apple.Preferences", { restart: true });
scroll("down", { until: "일반" });
tap("일반");
waitFor("정보");

const names = [];
let found = false;
let scrolls = 0;
while (true) {
  // 한 화면을 읽어 위에서부터 처음 보는 항목만 담는다. 화면이 겹치게 넘어가므로 같은 이름이 다시 보인다.
  // 행은 버튼이고, 왼쪽 위 '설정'(뒤로 가기)도 버튼이라 뺀다.
  const rows = read({ app: true })
    .elements.filter((el) => el.role === "button" && el.label && !el.traits.includes("back_button"))
    .sort((a, b) => a.frame.y - b.frame.y);
  for (const el of rows) {
    const name = el.label.split(",")[0].trim();
    if (!names.includes(name)) names.push(name);
    if (name === until) {
      found = true;
      break;
    }
  }
  if (found || scrolls >= maxScrolls) break;
  // 더 내려가지 않으면 목록의 끝이다.
  if (!scroll("down").screen_changed) break;
  scrolls += 1;
}
log(`${names.length}개, 스크롤 ${scrolls}번, '${until}' ${found ? "찾음" : "못 찾음"}`);
return { names, found, scrolls };
