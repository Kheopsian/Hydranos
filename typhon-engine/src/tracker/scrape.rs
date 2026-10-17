//! Tracker scrape (BEP 48 over HTTP, BEP 15 action 2 over UDP).
//!
//! Counts only: seeders and leechers for a batch of torrents, without
//! announcing any of them. Sent from the same interface and through the same
//! proxy as the engine's announces -- a scrape carries the passkey and comes
//! from an address, and a tracker must never learn one the announces hide.

use crate::torrent::meta::InfoHash;

/// Hashes per request. 74 is what fits in one UDP datagram (BEP 15); HTTP
/// trackers commonly cap a multi-scrape lower, so both use 50.
pub const BATCH: usize = 50;

/// What a tracker said about one torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrapeEntry {
    pub info_hash: InfoHash,
    pub complete: u32,
    pub incomplete: u32,
}

/// The scrape URL of an HTTP announce URL (BEP 48): the last path segment
/// must start with `announce`, which becomes `scrape`; anything else has no
/// scrape. The query (a passkey) is kept.
pub fn scrape_url(announce: &str) -> Option<String> {
    let (base, query) = match announce.find('?') {
        Some(i) => (&announce[..i], &announce[i..]),
        None => (announce, ""),
    };
    let after_scheme = base.find("://")? + 3;
    // The last slash of the PATH. Without a path, `rfind` would land in the
    // scheme's `//` and "rewrite" the host: `http://announce-x.com?pk=K`
    // became `http://scrape-x.com?pk=K`, the passkey sent to another domain.
    let slash = after_scheme + base[after_scheme..].rfind('/')?;
    let last = &base[slash + 1..];
    if !last.starts_with("announce") || !(base.starts_with("http://") || base.starts_with("https://")) {
        return None;
    }
    Some(format!("{}/scrape{}{}", &base[..slash], &last["announce".len()..], query))
}

/// Scrape an HTTP tracker for up to `BATCH` torrents.
pub async fn http_scrape_on(
    scrape_url: &str,
    hashes: &[InfoHash],
    user_agent: &str,
    device: &str,
    proxy: &str,
) -> Result<Vec<ScrapeEntry>, String> {
    let mut url = scrape_url.to_string();
    for (i, h) in hashes.iter().take(BATCH).enumerate() {
        let sep = if i == 0 && !url.contains('?') { '?' } else { '&' };
        url.push(sep);
        url.push_str("info_hash=");
        url.push_str(&super::http::url_encode_binary(h));
    }
    let mut builder = super::http::pin_builder(
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            // Same as announce: the scrape URL carries the passkey, so a forged
            // redirect must not replay it to another host.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(user_agent),
        device,
    )?;
    if let Some(px) = super::http::proxy_for(proxy)? {
        builder = builder.proxy(px);
    }
    let client = builder.build().map_err(|e| format!("http client: {e}"))?;
    // The error names the host, never the URL: it carries the passkey.
    let resp = client.get(&url).send().await.map_err(|e| format!("scrape request: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!("scrape: HTTP {}", resp.status().as_u16()));
    }
    let body = super::http::read_capped(resp, super::http::MAX_TRACKER_BODY)
        .await
        .map_err(|e| format!("scrape body: {e}"))?;
    parse_http_scrape(&body)
}

/// `d5:filesd<20-byte hash>d8:completei..e10:incompletei..e...e...ee`.
///
/// Its own walker: the keys of `files` are raw hashes, and the general
/// decoder turns dict keys into (lossy) strings.
pub fn parse_http_scrape(data: &[u8]) -> Result<Vec<ScrapeEntry>, String> {
    let mut p = Parser { d: data, i: 0 };
    p.expect(b'd')?;
    let mut out = Vec::new();
    while p.peek()? != b'e' {
        let key = p.bytes()?.to_vec();
        match key.as_slice() {
            b"files" => {
                p.expect(b'd')?;
                while p.peek()? != b'e' {
                    let hash = p.bytes()?;
                    let ih: Option<InfoHash> = hash.try_into().ok();
                    let (mut complete, mut incomplete) = (0i64, 0i64);
                    p.expect(b'd')?;
                    while p.peek()? != b'e' {
                        match p.bytes()? {
                            b"complete" => complete = p.int()?,
                            b"incomplete" => incomplete = p.int()?,
                            _ => p.skip()?,
                        }
                    }
                    p.i += 1;
                    if let Some(info_hash) = ih {
                        out.push(ScrapeEntry {
                            info_hash,
                            complete: complete.clamp(0, u32::MAX as i64) as u32,
                            incomplete: incomplete.clamp(0, u32::MAX as i64) as u32,
                        });
                    }
                }
                p.i += 1;
            }
            b"failure reason" => {
                let why = p.bytes()?;
                return Err(format!("tracker: {}", String::from_utf8_lossy(why)));
            }
            _ => p.skip()?,
        }
    }
    Ok(out)
}

struct Parser<'a> {
    d: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Result<u8, String> {
        self.d.get(self.i).copied().ok_or_else(|| "scrape: truncated".to_string())
    }
    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek()? != c {
            return Err(format!("scrape: expected {:?} at {}", c as char, self.i));
        }
        self.i += 1;
        Ok(())
    }
    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let colon = self.d[self.i..].iter().position(|&b| b == b':').ok_or("scrape: bad string")?;
        let len: usize = std::str::from_utf8(&self.d[self.i..self.i + colon])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or("scrape: bad string length")?;
        let start = self.i + colon + 1;
        let end = start.checked_add(len).filter(|e| *e <= self.d.len()).ok_or("scrape: truncated")?;
        self.i = end;
        Ok(&self.d[start..end])
    }
    fn int(&mut self) -> Result<i64, String> {
        self.expect(b'i')?;
        let end = self.d[self.i..].iter().position(|&b| b == b'e').ok_or("scrape: bad int")?;
        let v = std::str::from_utf8(&self.d[self.i..self.i + end]).ok().and_then(|s| s.parse().ok()).ok_or("scrape: bad int")?;
        self.i += end + 1;
        Ok(v)
    }
    fn skip(&mut self) -> Result<(), String> {
        self.skip_depth(0)
    }
    // A hostile tracker can answer a scrape with nothing but opening tokens
    // (`llll...`); skipping it recursively would overflow the stack and abort
    // the whole process. Honest scrape replies nest only a couple deep.
    fn skip_depth(&mut self, depth: usize) -> Result<(), String> {
        if depth >= 100 {
            return Err("scrape: nested too deep".into());
        }
        match self.peek()? {
            b'i' => self.int().map(|_| ()),
            b'l' | b'd' => {
                self.i += 1;
                while self.peek()? != b'e' {
                    self.skip_depth(depth + 1)?;
                }
                self.i += 1;
                Ok(())
            }
            _ => self.bytes().map(|_| ()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deeply_nested_scrape_is_rejected_not_overflow() {
        // A hostile tracker can answer a scrape with nothing but `l`; skipping
        // it must error out before overflowing the stack and aborting.
        let bomb = vec![b'l'; 200_000];
        assert!(parse_http_scrape(&bomb).is_err());
    }

    /// A scrape answer that keeps streaming is refused at the cap, not read
    /// into memory to the end of the timeout.
    #[tokio::test]
    async fn a_scrape_answer_past_the_cap_is_refused_unread() {
        let (base, _) = crate::tracker::http::endless::server("200 OK", b"d5:filesd".to_vec(), b'l').await;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            http_scrape_on(&format!("{base}/scrape"), &[[0x41u8; 20]], "t", "", ""),
        )
        .await
        .expect("the client gave up on its own, before its 20 s timeout");
        let e = out.expect_err("refused");
        assert!(e.contains("larger than"), "refused for its size: {e}");
    }

    #[test]
    fn the_scrape_url_is_bep_48s() {
        assert_eq!(scrape_url("http://t.example/announce").as_deref(), Some("http://t.example/scrape"));
        assert_eq!(scrape_url("https://t.example/x/announce.php?passkey=k").as_deref(), Some("https://t.example/x/scrape.php?passkey=k"));
        assert_eq!(scrape_url("https://t.example/abc/announce").as_deref(), Some("https://t.example/abc/scrape"));
        assert_eq!(scrape_url("https://t.example/a"), None, "no announce segment, no scrape");
        assert_eq!(scrape_url("https://t.example/announce/x"), None, "announce must be the last segment");
        assert_eq!(scrape_url("udp://t.example:80/announce"), None, "UDP has its own scrape");
        // No path: the host is never rewritten, whatever it is called.
        assert_eq!(scrape_url("http://announce-x.example?pk=K"), None);
        assert_eq!(scrape_url("https://announce.t.example"), None);
    }

    #[test]
    fn a_multi_scrape_answer_is_read_with_binary_hashes() {
        let a = [0xFFu8; 20];
        let b = [0x41u8; 20];
        let mut body = b"d5:filesd20:".to_vec();
        body.extend_from_slice(&a);
        body.extend_from_slice(b"d8:completei12e10:downloadedi99e10:incompletei3e4:name3:fooe20:");
        body.extend_from_slice(&b);
        body.extend_from_slice(b"d8:completei0e10:incompletei7eee5:flagsd20:min_request_intervali1800eee");
        let got = parse_http_scrape(&body).unwrap();
        assert_eq!(got, vec![
            ScrapeEntry { info_hash: a, complete: 12, incomplete: 3 },
            ScrapeEntry { info_hash: b, complete: 0, incomplete: 7 },
        ]);
    }

    #[test]
    fn a_refusal_or_garbage_is_an_error_not_zeroes() {
        assert!(parse_http_scrape(b"d14:failure reason9:forbiddene").unwrap_err().contains("forbidden"));
        assert!(parse_http_scrape(b"<html>").is_err());
        assert!(parse_http_scrape(b"d5:filesd20:short").is_err());
        assert_eq!(parse_http_scrape(b"d5:filesdee").unwrap(), vec![], "nothing known is an empty answer");
    }

    /// ⭐ The scrape URL carries the passkey like the announce one: a redirect
    /// is an error, and the listener it points at is never contacted.
    #[tokio::test]
    async fn a_scrape_does_not_follow_a_redirect() {
        use crate::tracker::http::redirect_trap;
        use std::sync::atomic::Ordering;
        for status in ["301 Moved Permanently", "302 Found", "307 Temporary Redirect", "308 Permanent Redirect"] {
            let trap = redirect_trap::start(status, b"d5:filesdee").await;
            let url = format!("{}/scrape?passkey=SECRET", trap.base);
            let out = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                http_scrape_on(&url, &[[0xABu8; 20]], "t", "", ""),
            )
            .await
            .expect("the client gave up on its own");
            assert_eq!(trap.reached.load(Ordering::SeqCst), 0, "{status}: the redirect target was contacted");
            assert!(out.is_err(), "{status}: a redirect is not an answer: {:?}", out.map(|v| v.len()));
            assert_eq!(trap.asked.load(Ordering::SeqCst), 1, "{status}: the tracker was asked once");
        }
    }
}
