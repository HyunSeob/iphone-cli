//! `iph setup`: run the doctor, explain what is missing, wait for the user to
//! fix it on the phone, and check again.

use std::io::{BufRead, IsTerminal, Write};

use serde_json::json;

use crate::{client, human, i18n::t};

pub async fn run(udid: &str) -> i32 {
    let interactive = std::io::stdin().is_terminal();
    println!(
        "{}",
        t(
            "iph 준비 상태를 확인합니다. 터치 확인을 위해 폰에서 설정 앱이 열리고 목록이 한 번 내려갔다 올라옵니다.",
            "Checking that the phone is ready for iph. To test touch, Settings opens on the phone and its list scrolls down and back once.",
        )
    );
    loop {
        let v = client::request(udid, "doctor", json!({})).await;
        println!("{}", human::render("doctor", &v));
        if v["ready"] == true {
            println!(
                "{}",
                t(
                    "준비됐습니다. `iph read`로 화면을 읽어 보세요.",
                    "Ready. Try `iph read`."
                )
            );
            return 0;
        }
        if !interactive {
            return 1;
        }
        print!(
            "{}",
            t(
                "위 안내대로 한 뒤 Enter를 누르면 다시 확인합니다 (그만두려면 q): ",
                "Do the steps above, then press Enter to check again (q to quit): ",
            )
        );
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() || line.trim() == "q" {
            return 1;
        }
    }
}
