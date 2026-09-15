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
    // 只对测试目标补清单依赖：rfd（tauri-plugin-dialog 的依赖）静态导入
    // comctl32!TaskDialogIndirect，而 comctl32 是 KnownDLL，没有「Common-Controls v6」
    // 清单依赖时加载器只绑 System32 的 5.82（没有该导出）→ 二进制加载期就
    // 0xC0000139 退出。app 二进制由 tauri-build 注入清单，测试目标没有。
    //
    // 注意：别用不带后缀的 rustc-link-arg —— 它连 app/bin 一起改，
    // /MANIFEST:EMBED 会与 tauri-build 的 .res 清单冲突（LNK1123）。
    #[cfg(windows)]
    {
        // comctl32 延迟加载：任何二进制在**加载期**都不再需要解析
        // `comctl32!TaskDialogIndirect`，测试目标因此不必额外补清单。
        // delayimp.lib 是延迟加载的辅助库（随 Windows SDK 提供）。
        println!("cargo:rustc-link-arg=/DELAYLOAD:comctl32.dll");
        println!("cargo:rustc-link-arg=delayimp.lib");
    }
}
