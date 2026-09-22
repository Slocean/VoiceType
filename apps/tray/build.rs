//! Windows 资源嵌入：应用图标 + 版本信息（资源管理器/任务栏/快捷键显示用）。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/voicetype.ico");
        res.compile().expect("嵌入 Windows 资源失败");
    }
}
