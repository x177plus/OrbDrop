<div align="center">

<img src="./src-tauri/icons/icon.png" width="120" height="120" alt="OrbDrop">

<h1>OrbDrop</h1>

<p><b>LAN file relay</b> — drag into the orb and it is visible, push and it lands</p>

<p>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg" alt="License"></a>
  <a href="https://github.com/x177plus/OrbDrop/releases/latest"><img src="https://img.shields.io/github/v/release/x177plus/OrbDrop?sort=semver&color=green" alt="Release"></a>
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Android-0078d4.svg" alt="Platform">
  <img src="https://img.shields.io/badge/Tauri-2-24C8DB.svg?logo=tauri&logoColor=white" alt="Tauri">
  <img src="https://img.shields.io/badge/SvelteKit-2-FF3E00.svg?logo=svelte&logoColor=white" alt="SvelteKit">
</p>

<p><b>English</b> | <a href="./README.zh-CN.md">简体中文</a></p>

</div>

## What it is

OrbDrop is a LAN file tool whose whole UI is an always-on-top floating orb. Two devices that know each other get **two independent file paths**:

- **Drag-to-share (zero copy)** — drop a folder or file onto the orb and it is registered in place, then made visible to the peers you are connected to. They browse your real disk, download, compare in Beyond Compare and edit/create/delete — writes land back on the source files. Nothing is copied, the data never leaves the machine that owns it, and a recipient who goes offline loses access.
- **Push (real transfer)** — pick files and folders in a conversation and push them. Devices you have paired with receive without asking; anyone else gets a consent bubble. This is the path that actually writes files to the other side's disk.

LAN only: no NAT traversal, no relay servers, no cloud, no accounts.

## Download

Pick your platform, then take the file in the “Download this” column; `<version>` is the release
version (for example `0.1.1`). Release pages:
[GitHub latest](https://github.com/x177plus/OrbDrop/releases/latest) ·
[Gitee latest](https://gitee.com/x177plus/OrbDrop/releases).

| Platform | Download this | Notes |
| --- | --- | --- |
| Windows desktop (x86_64) | `OrbDrop_<version>_windows_x64.exe` | Single portable exe, no installer |
| macOS 11+ (universal) | `OrbDrop_<version>_macos_universal.dmg` | **Use this for first install**; unsigned builds need one right-click → **Open** |
| Android (arm64) | `OrbDrop_<version>_android_arm64.apk` | Most phones; smaller and lighter |
| Android (universal) | `OrbDrop_<version>_android_universal.apk` | Older devices/emulators; pick this if unsure |

`OrbDrop_<version>_macos_universal.app.zip` and `update.json` are for **in-app auto-update** — you do
not need to download them by hand; the app fetches them when an update is available. The Windows
build is a single portable exe (no installer). The macOS build is a universal `.app` (Apple Silicon +
Intel); self-update requires the app to live in a writable location (normally `/Applications`). See
[`docs/macos.md`](./docs/macos.md) for the platform notes and the real-Mac verification checklist.

If an APK refuses to install, pick by device: almost every phone is arm64
(`OrbDrop_<version>_android_arm64.apk`, smaller and lighter); if unsure, use the universal build.

### Update manifest and verification

The manifest URL is a compile-time constant in the client, so this repository serves two fixed locations:

- GitHub: the `update.json` release asset — `https://github.com/x177plus/OrbDrop/releases/latest/download/update.json`
- Gitee: `update.json` at the repository root — `https://gitee.com/x177plus/OrbDrop/raw/master/update.json`

Every artifact URL inside the manifest is pinned to a tag (`releases/download/vX.Y.Z/...`), so an old client
never fetches the wrong build after a new release. Each artifact carries `size` and `sha256` (Windows builds
additionally carry a Tauri update signature in `sig`), so a download can be checked by hand:
`sha256sum OrbDrop_<version>_windows_x64.exe` or `shasum -a 256 OrbDrop_<version>_macos_universal.app.zip`.

## Highlights

### 1. Floating orb: drag in, drag out

- Always-on-top, frameless, in one of two shapes — a round orb or a rounded-rect card with per-device pages. It snaps to a screen edge (only a sliver stays visible and slides back on hover), highlights while you drag files over it, and the tray icon flashes with a red dot while something needs you. A second launch raises the running instance instead of starting a second copy.
- Drop a folder onto the orb → it becomes a share (idempotent, deduped by path) and is instantly visible to every peer currently holding a long-lived connection. Each share has its own broadcast switch, and a recursive watcher pushes change notifications so the remote tree refreshes itself.
- Drag any file or folder **out of the tree and into Explorer / Finder — or into any other application**. On Windows, remote items become a Windows Cloud Files placeholder locally (measured **0.22 ms and 0 bytes of disk** per item) and the bytes are pulled from the peer only when the target process actually reads them; macOS has no public equivalent, so remote items are staged to the mirror directory before the drag starts (see [`docs/macos.md`](./docs/macos.md)). Applications that only accept real paths (WeChat and friends) receive one on both platforms. On Windows there is no pre-download and nothing to wait for at drag start; on macOS only the staging wait applies.
- Drop files onto an open conversation overlay and they are sent to that conversation immediately.

### 2. Beyond Compare, straight from the tree

- Right-click a local or remote share → open it in Beyond Compare (Windows; `bcomp` on macOS is wired up but unverified).
- OrbDrop provisions Beyond Compare's own `BCProfiles.xml` itself: a WebDAV profile with base URL pointing at a **loopback proxy**, username `OrbDrop`, password = that device's token, encrypted exactly the way BC encrypts it (AES-256-GCM inside the `BCmp|1|` envelope). No manual profile setup, no password prompt to fill in.
- Beyond Compare talks to `http://127.0.0.1:<port>/proxy/<device-id>/`; OrbDrop forwards that to the peer's `/dav/` endpoint over TLS with fingerprint pinning, so BC never has to trust a certificate and never touches the network directly.
- Everything BC saves — `PUT`, `DELETE`, `MKCOL`, `MOVE`, `COPY` — is written back to the **sender's source file in real time**. There is no staging directory and no temporary copy.
- Profiles are pre-written at startup and after each pairing (Beyond Compare only picks up a `BCProfiles.xml` that existed when it started), and pruned when a pairing is removed — only entries carrying our username are ever touched.

### 3. Conversations, broadcast and group chat

Messaging is built in, and it is the host of the push feature rather than an add-on.

- **Three kinds of conversation**: 1:1, one-way broadcast lists (each recipient sees a normal private chat, members never see each other) and shared group chats.
- **Push lives in the conversation.** The “+” beside the input opens an in-app picker where files *and* folders are selected in the same dialog and selections accumulate across directories, with a cart to review or drop items. One manifest is built once and sent to every recipient: each target gets its own invite, consent, progress and cancel, and a single summary card reports per-device status. One unreachable device does not affect the others.
- **Offline peers queue messages** and flush them in order as soon as the device comes back. Read receipts, unread and pending badges on device rows, and a notification when a message arrives. **A conversation stays in the list for as long as it holds messages** — unpaired and offline included: the record is user data, not a property of the current link.
- **Group chats are peer-to-peer about the facts that matter**: a shared roster with last-writer-wins convergence (any member may rename the group or add/remove people), members do not need to be paired, receivers pull the messages they missed, and members relay each other's group messages for up to 3 hops so a group keeps working across subnets.
- **Paste an image into the composer** (`Ctrl+V` on desktop, the keyboard clipboard on Android) and the text/image segments are sent in the order you typed them; file messages can be opened right from the conversation.

### 4. Works when the Wi-Fi isolates clients, and across subnets

- **AP isolation** (same subnet, devices visible via mDNS, unicast blocked): when a direct dial fails, OrbDrop asks a reachable device that advertises relay (**on by default** since first launch — turn it off in Settings; a config written before this switch existed also reads as on) for an endpoint. The relay copies bytes blindly — end-to-end TLS is untouched, so it cannot read anything — single hop, at most 8 concurrent relays. Device rows show “via X” while the relay is in use.
- **Relay memory**: once a target is really reached through a relay, OrbDrop remembers *“that segment is reached through this segment”* in the address book. The memory persists across restarts, generalizes to the rest of the target segment (one decision covers that entire subnet), fails over to another relay in the pool when the anchor goes offline, and is propagated back to the relayed side over the same link. Direct connections always win, and the relay is dropped the moment they recover.
- **Cross-subnet discovery**: mDNS cannot cross a router, so paired devices exchange their **entire** mDNS view over unicast every 5 seconds. Add and pair **one anchor** per subnet and everything the anchor can see appears — multi-NIC hosts report what each interface sees. Discovery itself stays single-hop; group messages have their own member-to-member relay.
- **Manual add** (IP, or `IP:port`; a blank port scans 3890–3899), multi-NIC address reporting with subnet-aware dialing, and a read-only counter of how many bytes you have relayed for others.

## Everything else

| Area | Behaviour |
|---|---|
| Trust model | Pairing is the trust boundary: paired devices receive pushes without asking, anyone unpaired gets a consent bubble that stays until handled. Pair with a 6-digit code (2-minute TTL, rate limited) or with a no-code invite the peer accepts from a notification. |
| Long-lived connection | Opt-in and consent-based: clicking Connect sends a request and the peer accepts before the WebSocket is dialed. “Disconnect” drops the link and pauses locally — the pause survives restarts and rejects the peer's automatic redial until someone clicks Connect again. |
| Invisible mode | Go offline to deregister mDNS, drop every long-lived connection and answer all inbound requests with 503. Pairing, tokens and shares are untouched; going back online restores everything by itself. |
| Transport security | A self-signed identity generated once, plus peer certificate fingerprint pinning (TOFU — the fingerprint is remembered, not the hostname). Plaintext exists only on loopback. Tokens and fingerprints share one credentials file in the app data directory; “wipe all credentials” also removes the Beyond Compare profiles. |
| Transfer engine | Block-level resume keyed by file identity (a failed push resumes instead of restarting, and a receiver keeps receiving on its own even after it restarts), per-block SHA-256 verification, `<name>.orbdrop-part` followed by an atomic rename, configurable concurrency (1–8) and block size (1–32 MB). Measured on loopback TLS with a hot cache: 3,000 × 4 KB files in 1.72 s, a single 128 MB file at 101 MB/s. |
| Devices | Per-device remarks stored **only locally** (never sent to the peer), search by name or IP, pending badges, unpair at any time — the peer does not need to be online. |
| Settings | Download folder and mode, per-sender subdirectory for pushes, device name, port, autostart, relay switches, transfer concurrency and block size, and **interface language** (the system language is picked on first launch, falling back to English; switch between English and Simplified Chinese at any time). The choice takes effect immediately and also switches the tray menu and Android notifications. Logs rotate daily in the app data directory. |
| Android | A foreground service keeps the app online in the background, pushes arrive with a notification (on a channel separate from the keepalive notification, so you can mute one without losing the other), files can be sent from a conversation, received files open with a tap (`.apk` goes straight to the system installer), and the phone can act as a relay anchor too. |
| Self-update | The manifest's artifacts carry both `sha256` and a minisign signature; the desktop single-file exe replaces itself using the freshly downloaded exe as a helper (and rolls back on failure); downloads try GitHub then Gitee with speed-based failover and a 24-hour preferred-mirror memory. |

## What it does not do

No internet traversal or relay servers — relaying happens strictly inside the LAN, blind, between devices that can be verified. No cross-VLAN auto-discovery, no virtual drive mounting, no cloud storage or accounts, no directory mirroring or sync. Sending a *file* to an offline device fails immediately (messages are queued instead).

## Tech

Rust core, shared verbatim by the desktop and Android builds: an axum HTTP/WebSocket server on a single port (the listener tells TLS from plaintext by the first byte), self-signed identities and fingerprint pinning via rustls/rcgen, mDNS via `mdns-sd`, WebSocket signalling via tokio-tungstenite, plus hand-written WebDAV, a Windows Cloud Filters (CFAPI) provider for drag-out, and a macOS `NSDraggingSession` / staging path. The shell is Tauri 2; the UI is Svelte 5 + Vite.

## Documentation

Deep documentation lives in [`docs/`](./docs/README.md) (Simplified Chinese): the requirements
document, the orb-form design proposal, the main implementation record (38 sections), and topic
notes on the CFAPI drag-out mirror, the Beyond Compare integration, LAN relay, and the
self-update/release chain. [`docs/README.md`](./docs/README.md) is the map — what each file is,
what state it is in, and which code it belongs to.

## Build from source

Requirements: Node.js, a Rust toolchain, and — for Android — the Android SDK/NDK.

```bash
npm install          # install dependencies
npm run tauri dev    # desktop dev (vite + tauri)
npm run mac:build    # macOS universal .app + .dmg (on macOS)
npm run mac:artifact # macOS .app + DMG + updater zip (on macOS)
npm run android:dev  # Android dev
npm run check        # svelte-check type check
npm run android:build:arm64  # Android release APK for arm64 only
```

`npm run android:build` compiles the Rust crate for all four ABIs sequentially, so it is the slow one — use `android:build:arm64` while iterating (one ABI, roughly a quarter of the work) and save the full build for release day. Note that `--target aarch64` only limits which ABIs Rust builds: the Gradle flavor stays `universal`, so the arm64-only APK lands in (and overwrites) `apk/universal/release/`.

Packaging: `npm run tauri build` (Windows desktop), `npm run mac:build` (macOS .app/.dmg), `npm run android:release` (arm64 APK, installed to the device), `npm run release` (bump version, build, publish to both release remotes; add `--macos-zip` to include a macOS artifact). `npm run mirror` runs the local mirror harness used to test the updater.

Running several instances on one machine (useful for testing against yourself) needs a distinct data directory and port per instance: set `ORBDROP_ALLOW_MULTI=1` and `ORBDROP_DATA_DIR` accordingly. In this mode mDNS is loopback-only by default, so the extra instances are invisible to other machines on the LAN (the release app on the same machine won't see them either). Set `ORBDROP_MDNS_SCOPE=lan` on an instance to expose it deliberately, or `off` to disable mDNS entirely (manual add still works).

Release artifacts and the update manifest live in the [releases](https://github.com/x177plus/OrbDrop/releases/latest) of this repository.

## Community & Acknowledgments

- [LINUX DO](https://linux.do) — a new kind of ideal community. Where possible begins.

## License

**GNU General Public License v3.0 or later** (`GPL-3.0-or-later`) — see [LICENSE](./LICENSE).

Copyright (C) 2026 x177plus

This program is free software: you can redistribute it and/or modify it under the terms of the
GNU General Public License as published by the Free Software Foundation, either version 3 of the
License, or (at your option) any later version. This program is distributed in the hope that it
will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=x177plus/OrbDrop&type=Date)](https://star-history.com/#x177plus/OrbDrop&Date)