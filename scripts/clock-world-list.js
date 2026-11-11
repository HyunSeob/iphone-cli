// 시계 앱의 세계 시계 목록을 끝까지 읽기 (도시 · 시각)

launch("com.apple.mobiletimer");
tap({ role: "tab", label: "세계 시계" });
waitFor({ role: "header", label: "세계 시계" });
// 행 하나의 이름표: "서울, 오후 11:50, 오늘, +0시간". 화면을 넘기며 겹치는 행은 한 번만 담는다.
const list = readList({
  row: { role: "text", label_regex: ", (오전|오후) ?[0-9]" },
  columns: {
    city: { take: "label", regex: "^([^,]+)" },
    time: { take: "label", regex: "((오전|오후) ?[0-9:]+)" },
  },
  // 읽는 사이에 분이 바뀌면 시각이 달라진 같은 도시를 새 행으로 보게 된다. 도시 이름으로 같은 행인지 가린다.
  key: "city",
  max_scrolls: 6,
});
return { cities: list.rows };
