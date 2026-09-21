//! 注入策略演示：倒计时 3 秒（用于手动聚焦目标输入框）后注入测试文本。
//!
//! 用法：
//!   voicetype-demo paste  "你好，世界"   # 剪贴板粘贴
//!   voicetype-demo type   "你好，世界"   # Unicode 逐字键入
//!   voicetype-demo hybrid "完整的一句话"  # 混合流式（模拟中间结果 → 定稿）

use std::env;
use std::thread::sleep;
use std::time::Duration;
use voicetype_inject::{ClipboardInjector, HybridInjector, StreamingInjector, TextInjector, TypingInjector};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: {} <paste|type|hybrid> <文本>", args[0]);
        std::process::exit(2);
    }
    let mode = args[1].as_str();
    let text = args[2].as_str();

    println!("3 秒后开始注入，请点击目标输入框……");
    for i in (1..=3).rev() {
        println!("{i}…");
        sleep(Duration::from_secs(1));
    }

    let result = match mode {
        "paste" => ClipboardInjector::default().inject(text),
        "type" => TypingInjector::default().inject(text),
        "hybrid" => {
            // 模拟流式识别：先上屏前半段作为中间结果，再推全量，最后提交定稿。
            let mut injector = HybridInjector::default();
            let chars: Vec<char> = text.chars().collect();
            let mid = chars.len() / 2;
            let part1: String = chars[..mid].iter().collect();
            injector
                .update_interim(&part1)
                .and_then(|_| injector.update_interim(text))
                .and_then(|_| injector.commit(text))
        }
        _ => {
            eprintln!("未知模式: {mode}（可选 paste|type|hybrid）");
            std::process::exit(2);
        }
    };

    match result {
        Ok(()) => println!("注入完成"),
        Err(e) => {
            eprintln!("注入失败: {e}");
            std::process::exit(1);
        }
    }
}
