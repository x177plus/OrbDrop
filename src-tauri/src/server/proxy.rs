//! Beyond Compare 的 WebDAV 入口：**本机回环代理**。
//!
//! ## 为什么要有这一层
//!
//! BC 集成原来的做法是把 profile 的 BaseURL 直接指向对端（`http://<对端 ip:端口>/dav/`）。
//! 传输改成 TLS 之后那样就不行了：BC 是个外部 HTTP 客户端，它不会做证书指纹 pinning，
//! 而"让用户把我们的自签证书装进 Windows 信任库"是明确不要的方案。
//!
//! 所以改成让 BC 连**本机**：profile 的 BaseURL 指向 `http://127.0.0.1:<本机端口>/proxy/<设备id>/`，
//! 由这里把请求原样转发到对端的 `/dav/`（走 TLS + pinning）。BC 那边一个字都不用改 ——
//! 它面对的仍然是一个普通 HTTP 的 WebDAV 根，而且：
//!
//! - 回环流量不出网卡，网管抓不到；
//! - BC 不再需要接触任何证书；
//! - 对端只在**本机**这一跳上是明文，那一跳是回环。
//!
//! ## 认证：跟对端的 DAV 面一模一样
//!
//! BC 5.2 的实测行为是"先裸发一次请求，必须收到 `401 + WWW-Authenticate: Basic` 才会拿
//! profile 里的账号密码重试"（见 `routes_dav` 的文件头）。为了让它看到的东西与原来完全
//! 一致，这里也照做：凭据必须是**该设备的那枚 token**，缺了或不对就回同款挑战。
//! 用户名不参与校验（BC 那条路只支持 Basic，用户名只是发起方标记）。
//!
//! 注意这条路由**只认回环**：它是"本机代 BC 去连对端"，局域网上谁都不该能借它读到对端
//! 的共享项。回环本身不是安全边界（本机任何进程都能读凭据文件），但不能多开一个口子。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path as UrlPath, Request, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{StreamExt, TryStreamExt};
use tracing::{debug, warn};

use crate::state::AppState;

/// 代理入口前缀（profile 的 BaseURL 与这里必须一致）。
pub const PREFIX: &str = "/proxy";

/// 某台设备在**本机**的 WebDAV 根，写进 BC profile 的那个 BaseURL。
///
/// 尾部斜杠不能省：BC 会把 `profile:<名字>?<子路径>` 的子路径直接接到 BaseURL 后面
/// （见 `bc::profile_spec`），少了它就会拼成 `.../proxy/<id><shareId>`。
pub fn base_url(own_port: u16, peer_id: &str) -> String {
    format!("http://127.0.0.1:{own_port}{PREFIX}/{peer_id}/")
}

/// 这条请求该转发到对端的哪个地址（对端 DAV 面的路径命名空间是 `/dav/<shareId>/<相对路径>`）。
fn upstream_url(peer_base: &str, rel: &str) -> String {
    format!("{peer_base}/dav/{}", rel.trim_start_matches('/'))
}

/// BC 的 `COPY`/`MOVE` 目标也在本机代理上，必须改写成对端的地址 —— 否则对端收到一个
/// 指向本机回环的 `Destination`，只会当成越界路径拒掉（或更糟，指到别的东西上）。
fn rewrite_destination(value: &str, own_prefix: &str, upstream_dav: &str) -> Option<String> {
    let at = value.find(own_prefix)?;
    Some(format!("{upstream_dav}{}", &value[at + own_prefix.len()..]))
}

/// 传给对端的头：去掉逐跳头（协议要求）、以及由本次转发重新决定的那些。
fn forward_headers(source: &HeaderMap) -> HeaderMap {
    const DROP: [HeaderName; 8] = [
        header::HOST,
        header::CONNECTION,
        header::CONTENT_LENGTH,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
        header::TE,
        header::TRAILER,
        header::AUTHORIZATION,
    ];
    let mut out = source.clone();
    for name in DROP {
        out.remove(name);
    }
    out
}

/// 回到 BC 的头：同样去掉逐跳头与 `Content-Length`（正文是流式的，长度由 axum 现算）。
fn response_headers(source: &HeaderMap) -> HeaderMap {
    const DROP: [HeaderName; 5] = [
        header::CONNECTION,
        header::CONTENT_LENGTH,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
        header::TRAILER,
    ];
    let mut out = source.clone();
    for name in DROP {
        out.remove(name);
    }
    out
}

/// BC 的凭据必须是这台设备的那枚 token（用户名不参与校验，同 `server::auth`）。
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    use base64::Engine as _;
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    if let Some(bearer) = value.strip_prefix("Bearer ") {
        return bearer.trim() == token;
    }
    let Some(basic) = value.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(basic) else {
        return false;
    };
    let Ok(text) = String::from_utf8(decoded) else {
        return false;
    };
    // token 取第一个冒号之后的部分：用户名是我们自己写进 profile 的，万一它含冒号，
    // 取第二段就会拿错（同 `server::auth::split_basic`）。
    text.split_once(':')
        .map(|(_, pass)| pass == token)
        .unwrap_or(false)
}

/// BC 只支持 Basic，挑战头必须声明它（缺了挑战头 BC 直接判定连接失败）。
fn unauthorized(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            format!("Basic realm=\"{}\"", crate::bc::DAV_USER),
        )],
        msg.to_string(),
    )
        .into_response()
}

/// `PROXY` 入口（根）：`/proxy/{peer_id}` 与 `/proxy/{peer_id}/`。
///
/// 单独一个入口是因为通配段 `{*rest}` 匹配不了空串（axum 0.8 的语义），而 BC 会真的去
/// 请求这个根（它先 GET/OPTIONS 探一下这个位置能不能加载）。
pub async fn dav_proxy_root(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    UrlPath(peer_id): UrlPath<String>,
    req: Request<Body>,
) -> Response {
    forward(state, addr, peer_id, String::new(), req).await
}

/// `PROXY` 入口：`/proxy/{peer_id}/{*rest}`。
pub async fn dav_proxy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    UrlPath((peer_id, rest)): UrlPath<(String, String)>,
    req: Request<Body>,
) -> Response {
    forward(state, addr, peer_id, rest, req).await
}

async fn forward(
    state: Arc<AppState>,
    addr: SocketAddr,
    peer_id: String,
    rest: String,
    req: Request<Body>,
) -> Response {
    if !addr.ip().is_loopback() {
        warn!(%addr, "非回环地址访问本机 WebDAV 代理，已拒绝");
        return (StatusCode::FORBIDDEN, "Loopback only").into_response();
    }

    let Some(peer) = state.get_peer(&peer_id) else {
        return (StatusCode::NOT_FOUND, "设备不在列表中").into_response();
    };
    if peer.ip.is_empty() || peer.port == 0 {
        // 地址还没学到（mDNS 还没解析出来 / 是跨网段手动添加之前）：BC 现在连不上。
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("「{}」的地址未知", peer.name),
        )
            .into_response();
    }
    let token = match crate::credential::get_peer_token(&peer_id) {
        Ok(Some(token)) => token,
        _ => return (StatusCode::UNAUTHORIZED, "未与该设备配对").into_response(),
    };
    if !authorized(req.headers(), &token) {
        return unauthorized("Invalid credentials");
    }

    let peer_base = crate::net::peer_base(peer.dial().0, peer.dial().1);
    let url = upstream_url(&peer_base, &rest);
    let own_prefix = format!("{PREFIX}/{peer_id}");

    let mut headers = forward_headers(req.headers());
    // `COPY`/`MOVE` 的 `Destination` 指向的是本机代理的地址，必须改写成对端的 ——
    // 否则对端会收到一个指向 `127.0.0.1` 的目标（它会按越界路径拒掉）。
    // 注意 `Destination` 不在 http crate 的标准头里（它是 WebDAV 的），只能按名字写。
    let destination = req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(destination) = destination {
        // 目标不经过本机代理（BC 自己拼的别的地址）：原样带过去，由对端按自己的越界规则
        // 拒绝 —— 这里不替它猜。
        if let Some(rewritten) =
            rewrite_destination(&destination, &own_prefix, &format!("{peer_base}/dav"))
        {
            if let Ok(value) = axum::http::HeaderValue::from_str(&rewritten) {
                headers.insert("destination", value);
            }
        }
    }
    // 对端那一跳用**它认的那枚 token**（BC 给本机代理的凭据已经核过了）。
    if let Ok(value) = format!("Bearer {token}").parse() {
        headers.insert(header::AUTHORIZATION, value);
    }

    let method = req.method().clone();
    let body = reqwest::Body::wrap_stream(req.into_body().into_data_stream());
    let upstream = match crate::net::dial_client(peer.dial().0, &peer.dial_pin())
        .request(method.clone(), &url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            // 指纹不一致要单独说：它是"对方换过机器/有人在中间"，不是"它没开机"。
            let why = if crate::net::is_identity_rejected(&e) {
                "对端证书指纹与钉住的不一致（需重新配对）"
            } else {
                "联系不上对端"
            };
            debug!(%peer_id, method = %method, %e, "WebDAV 代理转发失败");
            return (StatusCode::BAD_GATEWAY, why.to_string()).into_response();
        }
    };

    let status = upstream.status();
    let headers = response_headers(upstream.headers());
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    // WebDAV 的正文里带着**对端命名空间**里的绝对路径（`<D:href>/dav/<共享项id>/…`），
    // 原样交给 BC 就会被它按"响应所在的主机"解析成 `127.0.0.1:<本机端口>/dav/…` ——
    // 那是本机共享项那一面，上面根本没有对端的共享项。表现正是"对端一个条目都看不到、
    // 往里点也进不去"。所以正文要跟 `Destination` 一样改写回本机代理的前缀。
    if content_type.contains("xml") {
        match read_xml_body(upstream, MAX_REWRITABLE_BODY).await {
            Ok(XmlBody::Rewrite(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                let rewritten = rewrite_hrefs(&text, &peer_id);
                let mut response = Response::new(Body::from(rewritten));
                *response.status_mut() = status;
                *response.headers_mut() = headers;
                return response;
            }
            // 这个尺寸的列表 BC 本来也打不开：不改前缀，但**绝不**把整个响应缓冲进内存。
            Ok(XmlBody::Stream(body)) => {
                warn!(peer_id = %peer_id, "WebDAV 响应过大，href 未改写");
                let mut response = Response::new(body);
                *response.status_mut() = status;
                *response.headers_mut() = headers;
                return response;
            }
            Err(e) => {
                debug!(%peer_id, %e, "读取对端 WebDAV 响应失败");
                return (StatusCode::BAD_GATEWAY, "读取对端响应失败").into_response();
            }
        }
    }

    let stream = upstream.bytes_stream().map_err(std::io::Error::other);
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    // WebDAV 的能力声明由对端给出（`DAV: 1,2`），原样透传即可 —— 见 `routes_dav` 里
    // "BC 用 OPTIONS 协商能力"那段。这里不再自己加，避免两处声明打架。
    response
}

/// 缓冲改写只做 XML 正文（PROPFIND 的列表 / 错误 multistatus），这个上限远高于任何
/// 能用的目录列表。
const MAX_REWRITABLE_BODY: usize = 8 * 1024 * 1024;

/// [`read_xml_body`] 的结果：能改写就整份给，超限就交一条**不缓冲**的流。
enum XmlBody {
    Rewrite(Vec<u8>),
    Stream(Body),
}

/// 按 `max` 读一个 XML 响应体。
///
/// 旧写法 `upstream.bytes().await` 是**先读完再比上限**：对端只要一直写，缓冲区就会一直长
/// （这个尺寸的检查等于没有）。这里分三档：
/// 1. `Content-Length` 明确超限 → 一个字节都不缓冲，直接流式转发；
/// 2. 声明值缺失 / 撒谎 → 按块累计，越过 `max` 立刻停手；
/// 3. 已读部分 + 触发越界的那一块 + 剩余流按原顺序拼回去（BC 拿到的字节不变）。
async fn read_xml_body(
    mut upstream: reqwest::Response,
    max: usize,
) -> Result<XmlBody, reqwest::Error> {
    if upstream
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        let stream = upstream.bytes_stream().map_err(std::io::Error::other);
        return Ok(XmlBody::Stream(Body::from_stream(stream)));
    }

    let mut head: Vec<u8> = Vec::new();
    let mut over: Option<axum::body::Bytes> = None;
    while let Some(chunk) = upstream.chunk().await? {
        if head.len().saturating_add(chunk.len()) > max {
            over = Some(chunk);
            break;
        }
        head.extend_from_slice(&chunk);
    }
    let Some(chunk) = over else {
        return Ok(XmlBody::Rewrite(head));
    };

    let prefix = futures_util::stream::once(async move {
        Ok::<_, std::io::Error>(axum::body::Bytes::from(head))
    });
    let first = futures_util::stream::once(async move { Ok::<_, std::io::Error>(chunk) });
    let rest = upstream.bytes_stream().map_err(std::io::Error::other);
    Ok(XmlBody::Stream(Body::from_stream(
        prefix.chain(first).chain(rest),
    )))
}

/// 把正文里所有 `…:href>/dav/…` 的前缀换成本机代理的前缀。
///
/// 只看 `href>` 后面紧跟的那一段：`/dav/` 这三个字符完全可能出现在**路径中间**
/// （某个目录就叫 `dav`），无差别替换会把它们一起改坏。href 里其余部分（中文、空格都
/// 已经百分号编码）一个字节都不动 —— 只换前缀，BC 拿到的路径才和它请求的那套一致。
fn rewrite_hrefs(body: &str, peer_id: &str) -> String {
    const MARKER: &str = "href>";
    let mut out = String::with_capacity(body.len() + 32);
    let mut rest = body;
    while let Some(at) = rest.find(MARKER) {
        let (head, tail) = rest.split_at(at + MARKER.len());
        out.push_str(head);
        match tail.strip_prefix("/dav/") {
            Some(remainder) => {
                out.push_str(PREFIX);
                out.push('/');
                out.push_str(peer_id);
                out.push('/');
                rest = remainder;
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, Method};
    use std::sync::Arc;

    use crate::config::ShareItem;

    #[test]
    fn base_url_is_loopback_and_trailing_slash_survives() {
        assert_eq!(
            base_url(3890, "peer-a"),
            "http://127.0.0.1:3890/proxy/peer-a/"
        );
    }

    #[test]
    fn upstream_url_maps_onto_the_peer_dav_namespace() {
        let base = "https://10.0.0.5:3890";
        assert_eq!(
            upstream_url(base, "share-id/sub/a.txt"),
            "https://10.0.0.5:3890/dav/share-id/sub/a.txt"
        );
        // 根（BC 会请求它）落在 `/dav/` 上，不能多一个斜杠
        assert_eq!(upstream_url(base, ""), "https://10.0.0.5:3890/dav/");
        assert_eq!(upstream_url(base, "/x"), "https://10.0.0.5:3890/dav/x");
    }

    /// BC 的 `COPY`/`MOVE` 目标也在本机代理上：不改写的话，对端会收到一个指向
    /// `127.0.0.1` 的目标（它按越界路径拒掉，用户看到的是"复制失败"）。
    #[test]
    fn destination_is_rewritten_from_the_proxy_to_the_peer() {
        let rewritten = rewrite_destination(
            "http://127.0.0.1:3890/proxy/peer-a/share-1/b.txt",
            "/proxy/peer-a",
            "https://10.0.0.5:3890/dav",
        );
        assert_eq!(
            rewritten.as_deref(),
            Some("https://10.0.0.5:3890/dav/share-1/b.txt")
        );
        // 不经过本机代理的目标：原样留着，由对端按自己的规则拒绝
        assert_eq!(
            rewrite_destination("http://elsewhere/dav/x", "/proxy/peer-a", "https://x/dav"),
            None
        );
    }

    /// 对端的 PROPFIND 回的是**它自己命名空间**里的绝对路径。不改写的话，BC 会拿
    /// "响应所在的主机"去解析它 —— 落到本机自己的 `/dav/`（本机共享项那一面），
    /// 于是一个条目都看不到、往里点也进不去。
    #[test]
    fn hrefs_are_rewritten_back_into_the_proxy_namespace() {
        let share = "f68512a6-023f-4d2d-8b2f-6784496b4afb";
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">\
             <D:response><D:href>/dav/{share}/</D:href><D:propstat><D:prop>\
             <D:resourcetype><D:collection/></D:resourcetype></D:prop>\
             <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\
             <D:response><D:href>/dav/{share}/sub/</D:href></D:response>\
             <D:response><D:href>/dav/{share}/%E6%96%B0%E5%BB%BA%20%E6%96%87%E6%9C%AC.txt</D:href></D:response>\
             </D:multistatus>"
        );
        let rewritten = rewrite_hrefs(&body, "peer-a");
        assert!(rewritten.contains(&format!("<D:href>/proxy/peer-a/{share}/</D:href>")));
        assert!(rewritten.contains(&format!("<D:href>/proxy/peer-a/{share}/sub/</D:href>")));
        // 百分号编码过的中文/空格原样保留：改的只有前缀
        assert!(rewritten.contains(&format!(
            "<D:href>/proxy/peer-a/{share}/%E6%96%B0%E5%BB%BA%20%E6%96%87%E6%9C%AC.txt</D:href>"
        )));
        assert!(!rewritten.contains(":href>/dav/"), "{rewritten}");
    }

    /// `/dav/` 也可能出现在**路径中间**（某个目录就叫 `dav`）：只认 `href>` 后面那一段。
    #[test]
    fn href_rewrite_only_touches_the_prefix() {
        assert_eq!(
            rewrite_hrefs("<D:href>/dav/s1/dav/dav.txt</D:href>", "p"),
            "<D:href>/proxy/p/s1/dav/dav.txt</D:href>"
        );
        // 不是 `/dav/` 开头的 href：不动（别的服务器形状不该被我们改坏）
        assert_eq!(
            rewrite_hrefs("<D:href>/other/x</D:href>", "p"),
            "<D:href>/other/x</D:href>"
        );
    }

    /// 一个只答一次的裸 HTTP 服务端；`parts` 之间停顿一下，让客户端分块收到。
    async fn one_shot_http_parts(parts: Vec<Vec<u8>>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            for part in parts {
                socket.write_all(&part).await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}/dav/x")
    }

    /// 声明值超限：一个字节都不缓冲，整条身体照样按原顺序转发。
    #[tokio::test]
    async fn oversized_xml_with_a_length_is_streamed() {
        crate::net::install_crypto_provider();
        let payload = "x".repeat(4096);
        let url = one_shot_http_parts(vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        )
        .into_bytes()])
        .await;
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();
        let body = match read_xml_body(resp, 1024).await.unwrap() {
            XmlBody::Rewrite(_) => panic!("超限的响应不该被整份读进内存"),
            XmlBody::Stream(body) => body,
        };
        let collected = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        assert_eq!(collected.as_ref(), payload.as_bytes());
    }

    /// 声明值撒谎/缺失（chunked）：读到越过上限就停，已读前缀 + 后续流必须原样拼回去。
    #[tokio::test]
    async fn oversized_xml_without_a_length_keeps_the_prefix_and_rest() {
        crate::net::install_crypto_provider();
        let chunk = "y".repeat(800);
        let headers = "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nTransfer-Encoding: chunked\r\n\r\n";
        let framed = format!("{:x}\r\n{chunk}\r\n", chunk.len());
        let url = one_shot_http_parts(vec![
            format!("{headers}{framed}").into_bytes(),
            framed.clone().into_bytes(),
            format!("{framed}0\r\n\r\n").into_bytes(),
        ])
        .await;
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();
        let body = match read_xml_body(resp, 1024).await.unwrap() {
            XmlBody::Rewrite(_) => panic!("超过上限的响应不该被整份读进内存"),
            XmlBody::Stream(body) => body,
        };
        let collected = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        assert_eq!(
            collected.len(),
            2400,
            "前缀 + 越界块 + 剩余流一个字节都不能丢"
        );
        assert!(collected.iter().all(|b| *b == b'y'));
    }

    /// 上限之内：照常整份读出，交给 href 改写。
    #[tokio::test]
    async fn small_xml_is_read_whole() {
        crate::net::install_crypto_provider();
        let payload = "<D:multistatus/>";
        let url = one_shot_http_parts(vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        )
        .into_bytes()])
        .await;
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();
        match read_xml_body(resp, 1024).await.unwrap() {
            XmlBody::Rewrite(bytes) => assert_eq!(bytes, payload.as_bytes()),
            XmlBody::Stream(_) => panic!("上限内的响应必须能改写"),
        }
    }

    /// 端到端：BC 从前到后真正会走的动作，走一遍。
    ///
    /// 两个实例跑在一个进程里：**B 是"对端"**（有共享项、把调用方认成长连接），**A 是本机**
    /// （地址簿里指着 B）。然后只打 A 的**回环代理**：
    ///
    /// 1. 裸探一次 → 必须拿到 `401 + WWW-Authenticate: Basic`（BC 的第一次探测就长这样）；
    /// 2. 拿 profile 里的账号密码 `PROPFIND Depth: 1` → 207，而且 **href 必须是本机代理的
    ///    前缀**（这条就是"对端一个条目都看不到"那个 bug 的回归测试）；
    /// 3. 按列表里的 href `GET` 一次 → 真的拿到对端那个文件的内容。
    #[tokio::test]
    async fn bc_reaches_the_peer_through_the_loopback_proxy() {
        let peer_root =
            std::env::temp_dir().join(format!("orbdrop_proxy_peer_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(peer_root.join("sub")).unwrap();
        std::fs::write(peer_root.join("hello.txt"), b"hello from the peer").unwrap();
        // 用户实际踩到的那个形状：文件名带空格与中文（BC 会用列表里那条 href 回去取它）
        std::fs::write(peer_root.join("新建 文本文档.txt"), b"spaced").unwrap();

        // ---- B：对端 ----
        let mut peer_config = crate::config::test_config();
        let share = ShareItem::new(peer_root.clone());
        let share_id = share.id.clone();
        peer_config.shares.push(share);
        let peer_config_path =
            std::env::temp_dir().join(format!("orbdrop_proxy_cfg_{}.toml", uuid::Uuid::new_v4()));
        let peer_state = Arc::new(AppState::new(peer_config, peer_config_path));
        peer_state.config.lock().shares[0].broadcast_enabled = true;
        let peer_port = crate::server::start_server_loopback(peer_state.clone(), 4151)
            .await
            .expect("peer server");

        // B 认这枚 token 属于"调用方"，而且调用方**此刻处于长连接**（可见性的两个条件）。
        let caller_id = format!("device-caller-{}", uuid::Uuid::new_v4());
        let token = format!("tok-{}", uuid::Uuid::new_v4());
        crate::credential::set_peer(&caller_id, &token).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(crate::ws::EVENT_QUEUE_CAPACITY);
        peer_state.ws.register(&caller_id, tx);

        // ---- A：本机（代理这一侧） ----
        let own_state = Arc::new(AppState::new(
            crate::config::test_config(),
            std::env::temp_dir().join(format!("orbdrop_proxy_own_{}.toml", uuid::Uuid::new_v4())),
        ));
        let own_port = crate::server::start_server_loopback(own_state.clone(), 4150)
            .await
            .expect("own server");
        own_state.add_peer_book(crate::config::PeerDevice {
            id: caller_id.clone(),
            name: "对端".into(),
            ip: "127.0.0.1".into(),
            port: peer_port,
            online: true,
            paired: true,
            auto_connect: false,
            paused: false,
            relay: None,
        });

        let client = crate::net::client();
        let base = base_url(own_port, &caller_id);
        let share_url = format!("{base}{share_id}/");

        // 1. 裸探：BC 的第一次请求不带凭据
        let bare = client.get(&share_url).send().await.unwrap();
        assert_eq!(bare.status(), StatusCode::UNAUTHORIZED);
        assert!(bare
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("Basic"));

        // 2. 带 profile 里的凭据列一层
        let creds =
            |req: reqwest::RequestBuilder| req.basic_auth(crate::bc::DAV_USER, Some(&token));
        let listed = creds(client.request(Method::from_bytes(b"PROPFIND").unwrap(), &share_url))
            .header("Depth", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::MULTI_STATUS);
        let body = listed.text().await.unwrap();
        assert!(body.contains("hello.txt"), "列表里该有对端的文件: {body}");
        // href 是**路径形式**（标准形状，按响应所在的主机解析）—— 所以它必须落在本机代理的
        // 前缀上；写成 `http://…/dav/…`（对端命名空间）时代它就会指回本机自己的 /dav/。
        let expect_href = format!("<D:href>{PREFIX}/{caller_id}/{share_id}/hello.txt</D:href>");
        assert!(
            body.contains(&expect_href),
            "href 必须是本机代理的前缀，否则 BC 会去请求本机自己的 /dav/：{body}"
        );
        assert!(!body.contains("<D:href>/dav/"), "{body}");

        // 3. 按列表里的 href 取内容
        let file = creds(client.get(format!("{base}{share_id}/hello.txt")))
            .send()
            .await
            .unwrap();
        assert_eq!(file.status(), StatusCode::OK);
        assert_eq!(file.text().await.unwrap(), "hello from the peer");

        // 4. 带空格与中文的名字：列表里是**百分号编码**的（`href_of` 的 `percent_encode_path`），
        //    BC 就按那条 href 回来取。
        let encoded = "%E6%96%B0%E5%BB%BA%20%E6%96%87%E6%9C%AC%E6%96%87%E6%A1%A3.txt";
        assert!(
            body.contains(encoded),
            "列表里该有这条（百分号编码）: {body}"
        );
        let spaced = creds(client.get(format!("{base}{share_id}/{encoded}")))
            .send()
            .await
            .unwrap();
        assert_eq!(spaced.status(), StatusCode::OK, "带空格的名字也要取得回来");
        assert_eq!(spaced.text().await.unwrap(), "spaced");

        // 5. 子目录：BC 钻进去时要的就是这一层（用户报的第二个症状）
        let sub = creds(client.request(
            Method::from_bytes(b"PROPFIND").unwrap(),
            format!("{base}{share_id}/sub/"),
        ))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
        assert_eq!(sub.status(), StatusCode::MULTI_STATUS);
        let sub_body = sub.text().await.unwrap();
        assert!(
            sub_body.contains(&format!(
                "<D:href>{PREFIX}/{caller_id}/{share_id}/sub/</D:href>"
            )),
            "子目录自己的 href 也要落在代理前缀上: {sub_body}"
        );

        // 6. **能力协商**：BC 对 profile 的根先发 `OPTIONS`，要看到 `DAV: 1,2` 才认为这是
        //    WebDAV 位置（看不到就直接加载不了远端目录）。这一条曾经被本机自己的 CORS 层
        //    抢答（返回 200 + `Access-Control-Allow-Methods: *`，一个 `DAV:` 头都没有），
        //    所以它必须**经过**代理一路打到对端的 `/dav` 上。
        let options = creds(client.request(
            Method::from_bytes(b"OPTIONS").unwrap(),
            format!("{base}{share_id}/"),
        ))
        .send()
        .await
        .unwrap();
        assert_eq!(
            options.status(),
            StatusCode::OK,
            "OPTIONS 应当由对端的 DAV 面回答"
        );
        assert!(
            options
                .headers()
                .get("dav")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .contains("1,2"),
            "对端的能力协商必须原样穿透代理（被本机 CORS 抢答时会只剩 Access-Control-*）：{:?}",
            options.headers()
        );
        assert!(
            options
                .headers()
                .get("access-control-allow-methods")
                .is_none(),
            "这条路上不该再出现 CORS 的 preflight 抢答"
        );

        crate::server::stop_server(&peer_state).await;
        crate::server::stop_server(&own_state).await;
        crate::credential::remove_peer(&caller_id).ok();
        let _ = std::fs::remove_dir_all(peer_root);
    }

    /// 凭据必须是**这台设备**的那枚 token；用户名不参与（BC 只支持 Basic）。
    #[test]
    fn credentials_must_carry_the_peer_token() {
        let token = "tok-a";
        let basic = |text: &str| {
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(text);
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {encoded}")).unwrap(),
            );
            headers
        };

        assert!(authorized(&basic("OrbDrop:tok-a"), token));
        assert!(authorized(&basic("anyone:tok-a"), token), "用户名只是标记");
        assert!(!authorized(&basic("OrbDrop:wrong"), token));
        assert!(!authorized(&basic("OrbDrop:"), token));
        assert!(
            !authorized(&HeaderMap::new(), token),
            "裸请求必须被拒（BC 会先裸探一次）"
        );

        let mut bearer = HeaderMap::new();
        bearer.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer tok-a"),
        );
        assert!(authorized(&bearer, token));
    }
}
