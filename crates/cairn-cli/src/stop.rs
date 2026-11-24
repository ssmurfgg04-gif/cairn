//! `cairn stop` (review #58 "Quit Cairn"): ask the running daemon to exit
//! through its own loopback console API, the same path the dashboard's
//! Quit button uses. Plain std HTTP/1.1 over loopback, so the tray (which
//! only ever shells out to `cairn.exe`) can quit cleanly with no new deps.
//!
//! The per-launch dashboard token (P0 gate) is read from the served page,
//! exactly as a same-user browser would; a different OS user cannot reach
//! the loopback console's page through this any more than through Chrome.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

fn request(addr: &str, raw: &str) -> std::io::Result<String> {
    let sock = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("bad address"))?;
    let mut s = TcpStream::connect_timeout(&sock, Duration::from_secs(3))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.write_all(raw.as_bytes())?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    Ok(out)
}

/// Extract `window.CAIRN_TOKEN = "<32 hex>"` from the served page.
pub(crate) fn token_from_page(html: &str) -> Option<String> {
    let start = html.find("window.CAIRN_TOKEN = \"")? + "window.CAIRN_TOKEN = \"".len();
    let tok: String = html[start..].chars().take_while(|c| *c != '"').collect();
    (tok.len() == 32 && tok.bytes().all(|b| b.is_ascii_hexdigit())).then_some(tok)
}

/// `Ok(true)` = the daemon acknowledged and is exiting; `Ok(false)` =
/// nothing is listening (already stopped).
pub fn stop_daemon(ui_addr: &str) -> anyhow::Result<bool> {
    let page = match request(
        ui_addr,
        &format!("GET / HTTP/1.1\r\nHost: {ui_addr}\r\nConnection: close\r\n\r\n"),
    ) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Ok(false),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "cannot reach the cairn daemon at {ui_addr}: {e}"
            ))
        }
    };
    let token = token_from_page(&page)
        .ok_or_else(|| anyhow::anyhow!("{ui_addr} answered, but it is not a cairn console"))?;
    let resp = request(
        ui_addr,
        &format!(
            "POST /api/v1/quit HTTP/1.1\r\nHost: {ui_addr}\r\nx-cairn-token: {token}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        ),
    )?;
    if !resp.contains("\"quitting\":true") {
        anyhow::bail!(
            "the daemon refused to quit: {}",
            resp.lines().next().unwrap_or("")
        );
    }
    // wait (bounded) until the port is released, so callers can rely on it
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        let gone = ui_addr
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .map(|a| TcpStream::connect_timeout(&a, Duration::from_millis(200)).is_err())
            .unwrap_or(true);
        if gone {
            return Ok(true);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_is_read_only_from_the_real_marker() {
        let page = r#"<script>window.CAIRN_TOKEN = "0123456789abcdef0123456789abcdef";</script>"#;
        assert_eq!(
            super::token_from_page(page).as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            super::token_from_page(r#"window.CAIRN_TOKEN = "%%CAIRN_TOKEN%%";"#),
            None
        );
        assert_eq!(super::token_from_page("<html>other app</html>"), None);
    }

    #[test]
    fn nothing_listening_means_already_stopped() {
        // a port in the reserved range nobody binds in tests
        assert!(!super::stop_daemon("127.0.0.1:1").unwrap());
    }
}
