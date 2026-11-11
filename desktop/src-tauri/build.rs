fn main() {
    // 和核心 crate 一样：winfsp-x64.dll 延迟加载，找不到时由
    // ftpmount_rs::preload_winfsp() 从官方安装目录兜底 LoadLibrary。
    winfsp::build::winfsp_link_delayload();
    tauri_build::build();
}
