//! 回归：LocalSend 的启动**不能**依赖调用方是否在 Tokio reactor 里。
//!
//! 用户侧症状（安卓）：设置页打开「支持 LocalSend 协议」→ App 当场闪退；重开 App，
//! 因为开关已经落盘，启动链路（在运行时里）把它起起来了，状态行显示"运行中"。
//!
//! 根因：`set_localsend_enabled` / 托盘「在线」都是**同步**入口，跑在主线程、没有
//! reactor；`localsend::start` → `discovery::start` 在同步路径上就 `UdpSocket::from_std`
//! → tokio 的 `scheduler::Handle::current()` panic，而 release 档 `panic = "abort"`。
//!
//! 这里在**普通线程**（无 reactor）上调用 `localsend::start`：只要它 panic，这个用例就红。

use std::sync::atomic::Ordering;

use orbdrop_lib::config::Config;
use orbdrop_lib::state::AppState;

#[test]
fn localsend_start_does_not_require_a_reactor() {
    let dir = std::env::temp_dir().join(format!(
        "orbdrop_ls_no_reactor_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("ORBDROP_DATA_DIR", &dir);

    let mut config = Config::default();
    config.online = true;
    config.localsend_enabled = true;
    let state = std::sync::Arc::new(AppState::new(config, dir.join("config.toml")));
    // `localsend::start` 在端口还没绑上时会提前退出（那就不走 socket 那段了）。
    state.server_port.store(3890, Ordering::SeqCst);

    let started = state.clone();
    let outcome = std::thread::spawn(move || orbdrop_lib::localsend::start(&started)).join();
    assert!(
        outcome.is_ok(),
        "LocalSend 启动在无 reactor 的线程上 panic 了（用户侧 = 点开关闪退）"
    );

    // 让后台那几拍真的跑起来（首轮公告在 100ms 之后）：转换已经挪进 `discovery::run`，
    // 这条路径要在运行时里走通；顺带确认 `start` 把成功状态交回给了界面。
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        state.localsend.discovery_ok(),
        "发现服务应当已经起来：{:?}",
        state
            .localsend
            .status(true, true)
            .discovery_error
    );

    orbdrop_lib::localsend::stop(&state);
    let _ = std::fs::remove_dir_all(&dir);
}