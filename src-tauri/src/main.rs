// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 自替换的 helper 档：**必须在 Tauri 起来之前返回**。带这个参数启动的进程是"刚从
    // `%TEMP%` 拉起来的新 exe"，它的活是等旧进程退出、把自己覆盖到目标路径再拉起来 ——
    // 走到 Tauri 那一步就会多一个托盘图标、还会和单实例锁打架。见 `update::apply`。
    #[cfg(desktop)]
    if orbdrop_lib::update::run_helper_from_args() {
        return;
    }
    orbdrop_lib::run()
}
