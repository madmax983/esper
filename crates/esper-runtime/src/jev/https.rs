//! The blocking HTTPS client behind [`LiveTransport`][super::LiveTransport].
//!
//! Host-only (`host` feature): `std` sockets plus `rustls`. One
//! `POST`, one response, no connection reuse, no redirects, no
//! cookies — just enough HTTP/1.1 to carry a System One call. Every
//! malformed, truncated, or oversized response fails closed to
//! [`JevError`][super::JevError]; the bearer key is written into the
//! request head and never logged.
//!
//! Proxying follows the ambient environment like a conventional
//! client: when `HTTPS_PROXY` (or `https_proxy`) is set and the host
//! is not bypassed by `NO_PROXY`, the client tunnels with
//! `CONNECT`, authenticating to the proxy from the proxy URL's own
//! userinfo. The Jev bearer key itself still comes only from the
//! caller — proxy handling never touches it.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use super::JevError;

/// How long a TCP connect may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long any single socket read or write may take. The live API
/// answers in about a second (SPEC §19.9); sixty seconds is the
/// fail-closed backstop, matching the Python reference client.
const IO_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest response head the client will parse: 64 KiB.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// The largest response body the client will keep: 1 MiB. A System
/// One answer is about a kilobyte; anything past this is not one.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// A parsed `https://host[:port]/path` endpoint.
pub(super) struct Endpoint {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) path: String,
}

/// Split an `https://` URL into host, port, and path.
///
/// # Errors
///
/// Returns [`JevError::Transport`] when the URL is not an `https://`
/// URL with a host. Plain `http://` is rejected: the bearer key must
/// never travel in the clear.
pub(super) fn parse_endpoint(url: &str) -> Result<Endpoint, JevError> {
    const TRANSPORT_NOT_HTTPS: JevError = JevError::Transport("the endpoint is not an https URL");
    let rest = url.strip_prefix("https://").ok_or(TRANSPORT_NOT_HTTPS)?;
    let (authority, path) = rest
        .find('/')
        .map_or((rest, "/"), |index| rest.split_at(index));
    if authority.is_empty() {
        return Err(TRANSPORT_NOT_HTTPS);
    }
    let (host, port) = match authority.rfind(':') {
        Some(index) => {
            let port = authority[index + 1..]
                .parse::<u16>()
                .map_err(|_| TRANSPORT_NOT_HTTPS)?;
            (&authority[..index], port)
        }
        None => (authority, 443),
    };
    if host.is_empty() {
        return Err(TRANSPORT_NOT_HTTPS);
    }
    Ok(Endpoint {
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// A parsed HTTP response: the status and the body bytes.
pub(super) struct HttpResponse {
    pub(super) status: u16,
    pub(super) body: Vec<u8>,
}

/// POST `body` to `endpoint` over HTTPS with
/// `Authorization: Bearer <api_key>`.
///
/// # Errors
///
/// Returns [`JevError::Transport`] when the connection, the proxy
/// tunnel, or the TLS handshake fails; [`JevError::BadResponse`]
/// when the response is malformed, truncated, or oversized; and
/// [`JevError::HttpStatus`] is left to the caller — this function
/// returns every status, 200 or not, and the caller decides.
pub(super) fn post(
    endpoint: &Endpoint,
    api_key: &str,
    body: &[u8],
) -> Result<HttpResponse, JevError> {
    let mut tls = connect_tls(endpoint)?;
    let head = request_head(endpoint, api_key, body.len());
    tls.write_all(&head)
        .map_err(|_| JevError::Transport("the HTTPS request write failed"))?;
    tls.write_all(body)
        .map_err(|_| JevError::Transport("the HTTPS request write failed"))?;
    tls.flush()
        .map_err(|_| JevError::Transport("the HTTPS request flush failed"))?;
    read_response(&mut tls)
}

/// Build the request head: `POST`, the verified headers, and the
/// `Content-Length`. The body follows verbatim.
fn request_head(endpoint: &Endpoint, api_key: &str, body_len: usize) -> Vec<u8> {
    let mut head = Vec::new();
    head.extend_from_slice(b"POST ");
    head.extend_from_slice(endpoint.path.as_bytes());
    head.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    head.extend_from_slice(endpoint.host.as_bytes());
    if endpoint.port != 443 {
        head.extend_from_slice(b":");
        head.extend_from_slice(endpoint.port.to_string().as_bytes());
    }
    head.extend_from_slice(b"\r\nAuthorization: Bearer ");
    head.extend_from_slice(api_key.as_bytes());
    head.extend_from_slice(
        b"\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: ",
    );
    head.extend_from_slice(body_len.to_string().as_bytes());
    head.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    head
}

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

/// Open the TLS session: TCP (through a `CONNECT` tunnel when the
/// environment names a proxy), then the `rustls` handshake with the
/// Mozilla root set.
fn connect_tls(endpoint: &Endpoint) -> Result<TlsStream, JevError> {
    let stream = match proxy_for(&endpoint.host) {
        None => connect_host(&endpoint.host, endpoint.port)?,
        Some(proxy) => {
            let mut stream = connect_host(&proxy.host, proxy.port)?;
            connect_tunnel(&mut stream, endpoint, proxy.auth.as_deref())?;
            stream
        }
    };
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|_| JevError::Transport("the HTTPS socket rejected its read timeout"))?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|_| JevError::Transport("the HTTPS socket rejected its write timeout"))?;
    let server_name = rustls::pki_types::ServerName::try_from(endpoint.host.clone())
        .map_err(|_| JevError::Transport("the endpoint host is not a valid DNS name"))?;
    let connection = rustls::ClientConnection::new(tls_config()?, server_name)
        .map_err(|_| JevError::Transport("the TLS session failed to start"))?;
    Ok(rustls::StreamOwned::new(connection, stream))
}

/// The `rustls` client config: TLS 1.2/1.3 with the Mozilla roots.
/// An empty root set fails closed — there is nothing to verify
/// against.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, JevError> {
    let mut roots = rustls::RootCertStore::empty();
    let (added, _) =
        roots.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
    if added == 0 {
        return Err(JevError::Transport(
            "the TLS root set is empty: no certificate to verify",
        ));
    }
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// TCP-connect to one host, trying every resolved address.
fn connect_host(host: &str, port: u16) -> Result<TcpStream, JevError> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|_| JevError::Transport("the HTTPS endpoint did not resolve"))?;
    for addr in addrs {
        if let Ok(stream) = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            return Ok(stream);
        }
    }
    Err(JevError::Transport("the HTTPS connection failed"))
}

/// A proxy from the environment: host, port, and optional
/// `user:password` from the proxy URL's userinfo.
struct Proxy {
    host: String,
    port: u16,
    auth: Option<String>,
}

/// Parse an `http://[user:pass@]host[:port]` proxy URL.
fn parse_proxy(url: &str) -> Result<Proxy, JevError> {
    const TRANSPORT_BAD_PROXY: JevError = JevError::Transport("the proxy URL is not usable");
    let rest = url.strip_prefix("http://").ok_or(TRANSPORT_BAD_PROXY)?;
    let (userinfo, authority) = rest.rfind('@').map_or((None, rest), |index| {
        (Some(&rest[..index]), &rest[index + 1..])
    });
    let (host, port) = match authority.rfind(':') {
        Some(index) => {
            let port = authority[index + 1..]
                .parse::<u16>()
                .map_err(|_| TRANSPORT_BAD_PROXY)?;
            (&authority[..index], port)
        }
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err(TRANSPORT_BAD_PROXY);
    }
    Ok(Proxy {
        host: host.to_string(),
        port,
        auth: userinfo.map(str::to_string),
    })
}

/// Whether `host` is bypassed by a `NO_PROXY` value: an exact match
/// or a domain-suffix match.
fn host_bypassed(host: &str, no_proxy: &str) -> bool {
    no_proxy.split(',').any(|entry| {
        let entry = entry.trim().trim_start_matches('.');
        !entry.is_empty() && (host == entry || host.ends_with(&format!(".{entry}")))
    })
}

/// The ambient proxy for `host`, if the environment names one and
/// `NO_PROXY` does not bypass it.
fn proxy_for(host: &str) -> Option<Proxy> {
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if host_bypassed(host, &no_proxy) {
        return None;
    }
    let url = std::env::var("HTTPS_PROXY")
        .or_else(|_| std::env::var("https_proxy"))
        .ok()?;
    if url.trim().is_empty() {
        return None;
    }
    parse_proxy(&url).ok()
}

/// Tunnel through the proxy with `CONNECT`, then hand the raw stream
/// back for the TLS handshake.
fn connect_tunnel(
    stream: &mut TcpStream,
    endpoint: &Endpoint,
    auth: Option<&str>,
) -> Result<(), JevError> {
    let mut request = Vec::new();
    request.extend_from_slice(b"CONNECT ");
    request.extend_from_slice(endpoint.host.as_bytes());
    request.extend_from_slice(b":");
    request.extend_from_slice(endpoint.port.to_string().as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(endpoint.host.as_bytes());
    request.extend_from_slice(b":");
    request.extend_from_slice(endpoint.port.to_string().as_bytes());
    if let Some(credentials) = auth {
        request.extend_from_slice(b"\r\nProxy-Authorization: Basic ");
        request.extend_from_slice(base64_encode(credentials.as_bytes()).as_bytes());
    }
    request.extend_from_slice(b"\r\n\r\n");
    stream
        .write_all(&request)
        .map_err(|_| JevError::Transport("the proxy CONNECT write failed"))?;
    let mut head = Vec::new();
    loop {
        if head.len() > MAX_HEAD_BYTES {
            return Err(JevError::BadResponse);
        }
        let mut chunk = [0u8; 1024];
        let read = stream
            .read(&mut chunk)
            .map_err(|_| JevError::Transport("the proxy CONNECT read failed"))?;
        if read == 0 {
            return Err(JevError::Transport("the proxy closed the tunnel"));
        }
        head.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_head_end(&head) {
            let (status, _, _) = parse_head(&head[..end])?;
            if status != 200 {
                return Err(JevError::HttpStatus(status));
            }
            return Ok(());
        }
    }
}

/// Standard base64 with padding, for `Proxy-Authorization`.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let group = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((group >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((group >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((group >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(group & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The offset just past the `\r\n\r\n` that ends the head, if
/// present.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

/// The parsed head: status, optional `Content-Length`, and whether
/// the body is chunked (which this client rejects).
fn parse_head(head: &[u8]) -> Result<(u16, Option<u64>, bool), JevError> {
    let mut lines = head.split(|byte| *byte == b'\n');
    let status_line = lines.next().ok_or(JevError::BadResponse)?;
    let status_line = status_line.strip_suffix(b"\r").unwrap_or(status_line);
    let status = parse_status_line(status_line)?;
    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    for line in lines {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(JevError::BadResponse)?;
        let name = line[..colon].trim_ascii();
        let value = line[colon + 1..].trim_ascii();
        if name.eq_ignore_ascii_case(b"content-length") {
            let length = parse_decimal(value)?;
            if let Some(first) = content_length
                && first != length
            {
                return Err(JevError::BadResponse);
            }
            content_length = Some(length);
        } else if name.eq_ignore_ascii_case(b"transfer-encoding")
            && value
                .split(|byte| *byte == b',')
                .any(|token| token.trim_ascii().eq_ignore_ascii_case(b"chunked"))
        {
            chunked = true;
        }
    }
    Ok((status, content_length, chunked))
}

/// Parse `HTTP/1.x NNN ...`: the version must be 1.0 or 1.1 and the
/// status three digits in 100..=599.
fn parse_status_line(line: &[u8]) -> Result<u16, JevError> {
    let version = line.strip_prefix(b"HTTP/").ok_or(JevError::BadResponse)?;
    if version.len() < 4 || (&version[..3] != b"1.0" && &version[..3] != b"1.1") {
        return Err(JevError::BadResponse);
    }
    let rest = version[3..]
        .strip_prefix(b" ")
        .ok_or(JevError::BadResponse)?;
    if rest.len() < 3
        || !rest[..3].iter().all(u8::is_ascii_digit)
        || (rest.len() > 3 && rest[3] != b' ')
    {
        return Err(JevError::BadResponse);
    }
    let status = u16::from(rest[0] - b'0') * 100
        + u16::from(rest[1] - b'0') * 10
        + u16::from(rest[2] - b'0');
    if !(100..600).contains(&status) {
        return Err(JevError::BadResponse);
    }
    Ok(status)
}

/// Parse ASCII decimal digits as `u64`, rejecting empty input,
/// non-digits, and overflow.
fn parse_decimal(digits: &[u8]) -> Result<u64, JevError> {
    if digits.is_empty() {
        return Err(JevError::BadResponse);
    }
    let mut value: u64 = 0;
    for &digit in digits {
        if !digit.is_ascii_digit() {
            return Err(JevError::BadResponse);
        }
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(digit - b'0')))
            .ok_or(JevError::BadResponse)?;
    }
    Ok(value)
}

/// Read one full response: head first, then the body — `Content-Length`
/// bytes, or everything to close when the length is absent. Chunked
/// transfer coding is rejected: the System One API answers with
/// `Content-Length`, and anything else is not its response.
fn read_response(tls: &mut TlsStream) -> Result<HttpResponse, JevError> {
    let mut buf: Vec<u8> = Vec::new();
    let head_end = loop {
        if buf.len() > MAX_HEAD_BYTES {
            return Err(JevError::BadResponse);
        }
        let mut chunk = [0u8; 4096];
        let read = tls
            .read(&mut chunk)
            .map_err(|_| JevError::Transport("the HTTPS response read failed"))?;
        if read == 0 {
            return Err(JevError::BadResponse);
        }
        buf.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_head_end(&buf) {
            break end;
        }
    };
    let (status, content_length, chunked) = parse_head(&buf[..head_end])?;
    if chunked {
        return Err(JevError::BadResponse);
    }
    let mut body = buf[head_end..].to_vec();
    match content_length {
        Some(length) => {
            let length = usize::try_from(length).map_err(|_| JevError::BadResponse)?;
            if length > MAX_BODY_BYTES {
                return Err(JevError::BadResponse);
            }
            while body.len() < length {
                let mut chunk = [0u8; 8192];
                let read = tls
                    .read(&mut chunk)
                    .map_err(|_| JevError::Transport("the HTTPS response read failed"))?;
                if read == 0 {
                    return Err(JevError::BadResponse);
                }
                body.extend_from_slice(&chunk[..read]);
            }
            body.truncate(length);
        }
        None => loop {
            if body.len() > MAX_BODY_BYTES {
                return Err(JevError::BadResponse);
            }
            let mut chunk = [0u8; 8192];
            let read = tls
                .read(&mut chunk)
                .map_err(|_| JevError::Transport("the HTTPS response read failed"))?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        },
    }
    Ok(HttpResponse { status, body })
}

/// Parse a complete raw response (head plus body) the same way
/// [`read_response`] does — the unit-test entry point for the
/// parser, without a socket.
#[cfg(test)]
pub(super) fn parse_response(raw: &[u8]) -> Result<HttpResponse, JevError> {
    let head_end = find_head_end(raw).ok_or(JevError::BadResponse)?;
    if head_end > MAX_HEAD_BYTES {
        return Err(JevError::BadResponse);
    }
    let (status, content_length, chunked) = parse_head(&raw[..head_end])?;
    if chunked {
        return Err(JevError::BadResponse);
    }
    let rest = &raw[head_end..];
    if let Some(length) = content_length {
        let length = usize::try_from(length).map_err(|_| JevError::BadResponse)?;
        if length > MAX_BODY_BYTES || rest.len() < length {
            return Err(JevError::BadResponse);
        }
        Ok(HttpResponse {
            status,
            body: rest[..length].to_vec(),
        })
    } else {
        if rest.len() > MAX_BODY_BYTES {
            return Err(JevError::BadResponse);
        }
        Ok(HttpResponse {
            status,
            body: rest.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_splits_host_port_and_path() {
        let endpoint =
            parse_endpoint("https://api.typesafe.ai/v1/systemone").expect("valid endpoint");
        assert_eq!(endpoint.host, "api.typesafe.ai");
        assert_eq!(endpoint.port, 443);
        assert_eq!(endpoint.path, "/v1/systemone");
    }

    #[test]
    fn endpoint_accepts_an_explicit_port_and_defaults_the_path() {
        let endpoint = parse_endpoint("https://example.invalid:8443").expect("valid endpoint");
        assert_eq!(endpoint.host, "example.invalid");
        assert_eq!(endpoint.port, 8443);
        assert_eq!(endpoint.path, "/");
    }

    #[test]
    fn endpoint_rejects_plain_http() {
        // The bearer key must never travel in the clear.
        assert!(parse_endpoint("http://api.typesafe.ai/v1/systemone").is_err());
    }

    #[test]
    fn endpoint_rejects_a_missing_host() {
        assert!(parse_endpoint("https:///v1/systemone").is_err());
        assert!(parse_endpoint("https://").is_err());
        assert!(parse_endpoint("not a url").is_err());
    }

    #[test]
    fn request_head_carries_bearer_auth_and_lengths() {
        let endpoint =
            parse_endpoint("https://api.typesafe.ai/v1/systemone").expect("valid endpoint");
        let head = request_head(&endpoint, "secret-key", 12);
        let text = core::str::from_utf8(&head).expect("the head is ASCII");
        assert!(text.starts_with("POST /v1/systemone HTTP/1.1\r\n"));
        assert!(text.contains("\r\nHost: api.typesafe.ai\r\n"));
        assert!(text.contains("\r\nAuthorization: Bearer secret-key\r\n"));
        assert!(text.contains("\r\nContent-Type: application/json\r\n"));
        assert!(text.contains("\r\nAccept: application/json\r\n"));
        assert!(text.contains("\r\nContent-Length: 12\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn response_parses_status_and_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let response = parse_response(raw).expect("valid response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"{\"a\":1}");
    }

    #[test]
    fn response_reports_a_non_200_status() {
        let raw = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
        let response = parse_response(raw).expect("the status still parses");
        assert_eq!(response.status, 400);
        assert_eq!(response.body, b"");
    }

    #[test]
    fn response_rejects_chunked_transfer_coding() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn response_rejects_a_garbage_status_line() {
        assert!(parse_response(b"not http at all\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/2 200 OK\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/1.1 99 X\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/1.1 2000 X\r\n\r\n").is_err());
    }

    #[test]
    fn response_rejects_an_oversized_body() {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert!(parse_response(raw.as_bytes()).is_err());
    }

    #[test]
    fn response_rejects_conflicting_content_lengths() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\nabc";
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn response_reads_to_close_without_a_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\n\r\nabc";
        let response = parse_response(raw).expect("valid response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"abc");
    }

    #[test]
    fn response_rejects_a_truncated_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc";
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn proxy_parses_userinfo_host_and_port() {
        let proxy = parse_proxy("http://user:pass@proxy.example:3128").expect("valid proxy");
        assert_eq!(proxy.host, "proxy.example");
        assert_eq!(proxy.port, 3128);
        assert_eq!(proxy.auth.as_deref(), Some("user:pass"));
    }

    #[test]
    fn proxy_defaults_the_port_and_skips_auth() {
        let proxy = parse_proxy("http://proxy.example").expect("valid proxy");
        assert_eq!(proxy.port, 80);
        assert!(proxy.auth.is_none());
    }

    #[test]
    fn proxy_rejects_a_non_http_scheme() {
        assert!(parse_proxy("socks5://proxy.example:1080").is_err());
        assert!(parse_proxy("https://proxy.example:443").is_err());
    }

    #[test]
    fn no_proxy_bypasses_exact_and_suffix_matches() {
        assert!(host_bypassed("localhost", "localhost,127.0.0.1"));
        assert!(host_bypassed("api.internal.example", ".internal.example"));
        assert!(host_bypassed("api.internal.example", "internal.example"));
        assert!(!host_bypassed(
            "api.typesafe.ai",
            "localhost,internal.example"
        ));
        assert!(!host_bypassed("api.typesafe.ai", ""));
        assert!(!host_bypassed("notinternal.example", "internal.example"));
    }
}
