fn main() {
    tauri_build::build();

    // Windows 测试目标需要自备 comctl32 v6 的清单依赖。
    //
    // rfd（tauri-plugin-dialog 的依赖）静态导入 comctl32!TaskDialogIndirect。
    // comctl32 是 KnownDLL：进程没有「Common-Controls v6」清单依赖时，加载器只会
    // 绑定 System32 里的 5.82（没有该导出），于是二进制在加载期就
    // 0xC0000139 STATUS_ENTRYPOINT_NOT_FOUND 退出。app 二进制由 tauri-build 注入
    // 清单所以没事，测试目标没有任何清单 —— 不补这一条，`cargo test` 在本机
    // 一条都跑不起来。
    //
    // 只对测试目标生效（rustc-link-arg-tests），app/bin 的链接保持 tauri-build 原样。
    #[cfg(windows)]
    for arg in [
        "/MANIFEST:EMBED",
        "/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'",
    ] {
        println!("cargo:rustc-link-arg-tests={arg}");
    }
}
