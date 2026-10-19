//! RSS 2.0 and Atom, read into articles.
//!
//! No XML crate is in the tree, and a feed needs very little of XML: elements,
//! attributes, text, CDATA and the five entities plus numeric ones. A small
//! reader that builds a tree is enough, and a feed is capped in size before it
//! gets here (`fetch::MAX_FEED_BYTES`), so the tree is never large. Anything
//! it does not understand -- a DTD, a processing instruction, a namespace
//! declaration -- is skipped, never refused: trackers write odd feeds, and an
//! odd feed that still has items must still give them.

/// One item of a feed, as the store keeps it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Article {
    /// The item's identity in its feed: `<guid>` / `<id>`, else the torrent
    /// link, else the page link, else the title.
    pub guid: String,
    pub title: String,
    /// What to add: the `.torrent` enclosure, a magnet, else the link.
    pub torrent_url: String,
    /// The item's page, when it has one apart from the torrent.
    pub link: String,
    pub description: String,
    pub author: String,
    pub category: String,
    /// Unix seconds; 0 when the feed gave none (the store then uses the time
    /// it first saw the article).
    pub date: i64,
    /// Bytes; 0 when the feed does not say.
    pub size: i64,
}

/// A feed, read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub title: String,
    pub articles: Vec<Article>,
}

/// An element: its name (prefix kept, `torznab:attr`), attributes, the text
/// directly inside it and its children.
#[derive(Debug, Default)]
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    text: String,
    children: Vec<Node>,
}

impl Node {
    /// The name without its namespace prefix.
    fn local(&self) -> &str {
        local_name(&self.name)
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name) || local_name(k).eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn child(&self, local: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.local().eq_ignore_ascii_case(local))
    }

    fn children<'a>(&'a self, local: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.local().eq_ignore_ascii_case(local))
    }

    /// The text of a child, trimmed; empty when there is none.
    fn text_of(&self, local: &str) -> String {
        self.child(local).map(|c| c.text.trim().to_string()).unwrap_or_default()
    }
}

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// Read a feed. An error only when there is no feed at all in the bytes: a
/// feed with zero items is a feed.
pub fn parse(bytes: &[u8]) -> Result<Parsed, String> {
    let text = String::from_utf8_lossy(bytes);
    let root = tree(&text)?;
    let top = root
        .children
        .iter()
        .find(|c| matches!(c.local().to_ascii_lowercase().as_str(), "rss" | "feed" | "rdf"))
        .ok_or_else(|| "not an RSS or Atom feed (no <rss>, <feed> or <rdf:RDF> element)".to_string())?;
    match top.local().to_ascii_lowercase().as_str() {
        "feed" => Ok(atom(top)),
        _ => Ok(rss(top)),
    }
}

/// RSS 2.0, and RSS 1.0 (RDF), whose items sit beside the channel.
fn rss(top: &Node) -> Parsed {
    let channel = top.child("channel");
    let title = channel.map(|c| c.text_of("title")).unwrap_or_default();
    let items = channel
        .into_iter()
        .flat_map(|c| c.children("item"))
        .chain(top.children("item"));
    let articles = items.map(rss_item).filter(|a| !a.guid.is_empty()).collect();
    Parsed { title, articles }
}

fn rss_item(item: &Node) -> Article {
    let mut a = Article {
        title: item.text_of("title"),
        description: item.text_of("description"),
        author: {
            let au = item.text_of("author");
            if au.is_empty() { item.text_of("creator") } else { au }
        },
        category: item.text_of("category"),
        ..Article::default()
    };
    let link = item.text_of("link");
    // qBittorrent's choice: the enclosure that is a torrent, else a magnet in
    // the link, else the link itself.
    let mut enclosure_len = 0i64;
    let mut any_enclosure = String::new();
    for e in item.children("enclosure") {
        let url = e.attr("url").unwrap_or_default().trim().to_string();
        if url.is_empty() {
            continue;
        }
        let len = e.attr("length").and_then(|l| l.trim().parse::<i64>().ok()).unwrap_or(0);
        if e.attr("type").is_some_and(|t| t.trim().eq_ignore_ascii_case("application/x-bittorrent")) {
            a.torrent_url = url;
            enclosure_len = len;
            break;
        }
        if any_enclosure.is_empty() {
            any_enclosure = url;
            enclosure_len = len;
        }
    }
    let magnet = {
        let m = item.text_of("magnetURI");
        if m.is_empty() { item.text_of("magneturi") } else { m }
    };
    if a.torrent_url.is_empty() && link.to_ascii_lowercase().starts_with("magnet:") {
        a.torrent_url = link.clone();
    }
    if a.torrent_url.is_empty() && !any_enclosure.is_empty() {
        a.torrent_url = any_enclosure;
    }
    if a.torrent_url.is_empty() && !magnet.is_empty() {
        a.torrent_url = magnet;
    }
    if a.torrent_url.is_empty() {
        a.torrent_url = link.clone();
    }
    if !link.eq_ignore_ascii_case(&a.torrent_url) {
        a.link = link;
    } else {
        a.link = item.text_of("comments");
    }
    a.size = if enclosure_len > 0 { enclosure_len } else { size_of(item) };
    let date = {
        let d = item.text_of("pubDate");
        if d.is_empty() { item.text_of("date") } else { d }
    };
    a.date = parse_date(&date).unwrap_or(0);
    let guid = item.text_of("guid");
    a.guid = first_non_empty(&[&guid, &a.torrent_url, &a.link, &a.title]);
    a
}

/// The size a tracker feed says in one of its own ways: torznab / newznab
/// attributes, ezRSS `contentLength`, or a plain `<size>`.
fn size_of(item: &Node) -> i64 {
    for c in &item.children {
        if c.local().eq_ignore_ascii_case("attr") && c.attr("name").is_some_and(|n| n.eq_ignore_ascii_case("size")) {
            if let Some(v) = c.attr("value").and_then(|v| v.trim().parse::<i64>().ok()) {
                return v;
            }
        }
    }
    for name in ["contentLength", "size"] {
        if let Ok(v) = item.text_of(name).parse::<i64>() {
            return v;
        }
    }
    0
}

fn atom(top: &Node) -> Parsed {
    let title = top.text_of("title");
    let articles = top.children("entry").map(atom_entry).filter(|a| !a.guid.is_empty()).collect();
    Parsed { title, articles }
}

fn atom_entry(e: &Node) -> Article {
    let mut a = Article {
        title: e.text_of("title"),
        description: {
            let s = e.text_of("summary");
            if s.is_empty() { e.text_of("content") } else { s }
        },
        author: e.child("author").map(|au| au.text_of("name")).unwrap_or_default(),
        category: e.child("category").and_then(|c| c.attr("term")).unwrap_or_default().to_string(),
        ..Article::default()
    };
    for l in e.children("link") {
        let href = l.attr("href").unwrap_or_default().trim().to_string();
        if href.is_empty() {
            continue;
        }
        let rel = l.attr("rel").unwrap_or("alternate").trim().to_ascii_lowercase();
        let torrent = l.attr("type").is_some_and(|t| t.trim().eq_ignore_ascii_case("application/x-bittorrent"));
        if rel == "enclosure" && (torrent || a.torrent_url.is_empty()) {
            a.torrent_url = href;
            a.size = l.attr("length").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        } else if rel == "alternate" && a.link.is_empty() {
            a.link = href;
        }
    }
    if a.torrent_url.is_empty() {
        a.torrent_url = std::mem::take(&mut a.link);
    }
    if a.size == 0 {
        a.size = size_of(e);
    }
    let date = {
        let p = e.text_of("published");
        if p.is_empty() { e.text_of("updated") } else { p }
    };
    a.date = parse_date(&date).unwrap_or(0);
    let id = e.text_of("id");
    a.guid = first_non_empty(&[&id, &a.torrent_url, &a.link, &a.title]);
    a
}

fn first_non_empty(v: &[&str]) -> String {
    v.iter().find(|s| !s.trim().is_empty()).map(|s| s.trim().to_string()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The XML reader
// ---------------------------------------------------------------------------

/// Open elements allowed at once. The tree is walked and dropped recursively,
/// and no feed goes anywhere near this deep.
const MAX_DEPTH: usize = 100;

/// The document as a tree under an unnamed root. Unclosed elements are closed
/// at the end and a stray end tag closes up to its match, if any: a broken
/// feed gives what it has.
fn tree(src: &str) -> Result<Node, String> {
    let mut stack: Vec<Node> = vec![Node::default()];
    let b = src.as_bytes();
    let mut i = 0;
    let mut saw_element = false;
    while i < b.len() {
        if b[i] != b'<' {
            let end = src[i..].find('<').map(|n| i + n).unwrap_or(b.len());
            let text = decode_entities(&src[i..end]);
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&text);
            }
            i = end;
            continue;
        }
        let rest = &src[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->").map(|n| n + 3).unwrap_or(rest.len());
        } else if rest.starts_with("<![CDATA[") {
            let end = rest.find("]]>").unwrap_or(rest.len());
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&rest[9..end.max(9)]);
            }
            i += (end + 3).min(rest.len());
        } else if rest.starts_with("<?") {
            i += rest.find("?>").map(|n| n + 2).unwrap_or(rest.len());
        } else if rest.starts_with("<!") {
            i += skip_declaration(rest);
        } else if rest.starts_with("</") {
            let end = rest.find('>').unwrap_or(rest.len());
            let name = rest[2..end].trim();
            if stack[1..].iter().any(|n| n.name == name) {
                while stack.len() > 1 {
                    let done = stack.pop().unwrap();
                    let closes = done.name == name;
                    stack.last_mut().unwrap().children.push(done);
                    if closes {
                        break;
                    }
                }
            }
            i += (end + 1).min(rest.len());
        } else {
            let end = tag_end(rest).ok_or_else(|| "the feed ends inside a tag".to_string())?;
            let inner = &rest[1..end];
            let self_closing = inner.ends_with('/');
            let inner = inner.trim_end_matches('/');
            let (name, attrs) = read_tag(inner);
            i += end + 1;
            if name.is_empty() {
                continue;
            }
            saw_element = true;
            let node = Node { name, attrs, ..Node::default() };
            if self_closing {
                stack.last_mut().unwrap().children.push(node);
            } else {
                if stack.len() > MAX_DEPTH {
                    return Err("the feed is nested too deep".into());
                }
                stack.push(node);
            }
        }
    }
    while stack.len() > 1 {
        let done = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(done);
    }
    if !saw_element {
        return Err("not XML: no element in the response".into());
    }
    Ok(stack.pop().unwrap())
}

/// `<!DOCTYPE ... [ ... ]>`: the internal subset has its own `>`s.
fn skip_declaration(rest: &str) -> usize {
    let mut depth = 0i32;
    for (n, c) in rest.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            '>' if depth <= 0 => return n + 1,
            _ => {}
        }
    }
    rest.len()
}

/// The `>` that ends a start tag, ignoring any inside quoted attributes.
fn tag_end(rest: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (n, c) in rest.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, '>') => return Some(n),
            _ => {}
        }
    }
    None
}

fn read_tag(inner: &str) -> (String, Vec<(String, String)>) {
    let inner = inner.trim();
    let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
    let name = inner[..name_end].to_string();
    let mut attrs = Vec::new();
    let mut s = inner[name_end..].trim_start();
    while !s.is_empty() {
        let Some(eq) = s.find('=') else { break };
        let key = s[..eq].trim().to_string();
        let after = s[eq + 1..].trim_start();
        let (value, consumed) = match after.chars().next() {
            Some(q @ ('"' | '\'')) => match after[1..].find(q) {
                Some(close) => (&after[1..1 + close], close + 2),
                None => (&after[1..], after.len()),
            },
            _ => {
                let e = after.find(char::is_whitespace).unwrap_or(after.len());
                (&after[..e], e)
            }
        };
        if !key.is_empty() {
            attrs.push((key, decode_entities(value)));
        }
        s = after[consumed..].trim_start();
    }
    (name, attrs)
}

/// The predefined entities and character references. An unknown entity is
/// kept as written: `&nbsp;` in a title is better shown than dropped.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail[..tail.len().min(12)].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// RFC 822/2822 (RSS) or RFC 3339 (Atom), to Unix seconds.
pub fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.as_bytes().first().is_some_and(u8::is_ascii_digit) && s.len() >= 10 && s.as_bytes()[4] == b'-' {
        return parse_rfc3339(s);
    }
    parse_rfc2822(s)
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    let rest = s.get(10..).unwrap_or("");
    if rest.is_empty() {
        return Some(days_from_civil(y, mo, d) * 86400);
    }
    let rest = &rest[1..]; // 'T' or ' '
    let h: i64 = rest.get(0..2)?.parse().ok()?;
    let mi: i64 = rest.get(3..5)?.parse().ok()?;
    let sec: i64 = rest.get(6..8).and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut tz = rest.get(8..).unwrap_or("");
    if tz.starts_with('.') {
        tz = tz.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    }
    let offset = zone_offset(tz).unwrap_or(0);
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec - offset)
}

fn parse_rfc2822(s: &str) -> Option<i64> {
    // "Thu, 08 Oct 2026 12:00:00 +0000"; the weekday is optional.
    let s = match s.find(',') {
        Some(c) => &s[c + 1..],
        None => s,
    };
    let mut parts = s.split_whitespace();
    let d: u32 = parts.next()?.parse().ok()?;
    let mon = parts.next()?;
    let mo = month(mon)?;
    let mut y: i64 = parts.next()?.parse().ok()?;
    if y < 100 {
        y += if y < 50 { 2000 } else { 1900 };
    }
    let time = parts.next().unwrap_or("00:00:00");
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next().unwrap_or("0").parse().ok()?;
    let sec: i64 = t.next().unwrap_or("0").parse().ok()?;
    let offset = parts.next().and_then(zone_offset).unwrap_or(0);
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec - offset)
}

fn month(m: &str) -> Option<u32> {
    const M: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let m = m.get(0..3)?.to_ascii_lowercase();
    M.iter().position(|x| *x == m).map(|p| p as u32 + 1)
}

/// `+0200`, `-05:00`, `Z`, `GMT`, `UT` and the US zones RFC 822 names.
fn zone_offset(z: &str) -> Option<i64> {
    let z = z.trim();
    match z.to_ascii_uppercase().as_str() {
        "" | "Z" | "GMT" | "UT" | "UTC" => return Some(0),
        "EST" => return Some(-5 * 3600),
        "EDT" => return Some(-4 * 3600),
        "CST" => return Some(-6 * 3600),
        "CDT" => return Some(-5 * 3600),
        "MST" => return Some(-7 * 3600),
        "MDT" => return Some(-6 * 3600),
        "PST" => return Some(-8 * 3600),
        "PDT" => return Some(-7 * 3600),
        _ => {}
    }
    let sign = match z.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = z[1..].chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 4 {
        return None;
    }
    let h: i64 = digits[0..2].parse().ok()?;
    let m: i64 = digits[2..4].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Unix seconds as RFC 2822, UTC: what qBittorrent's API gives for an
/// article's `date` and a rule's `lastMatch`.
pub fn rfc2822(ts: i64) -> String {
    const D: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    // civil_from_days
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} +0000",
        D[days.rem_euclid(7) as usize],
        d,
        M[(m - 1) as usize],
        y,
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS2: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE rss [ <!ENTITY foo "bar"> ]>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel>
  <title>Example &amp; Tracker</title>
  <!-- a comment with <item> inside -->
  <item>
    <title><![CDATA[Show.Name.S01E05.1080p.WEB <x264>]]></title>
    <link>https://tracker.example/details/42</link>
    <guid isPermaLink="false">tracker-42</guid>
    <pubDate>Thu, 08 Oct 2026 12:00:00 +0200</pubDate>
    <enclosure url="https://tracker.example/dl/42.torrent?passkey=SECRET&amp;x=1" length="1234567" type="application/x-bittorrent"/>
    <torznab:attr name="size" value="999"/>
    <category>TV</category>
  </item>
  <item>
    <title>Magnet only</title>
    <link>magnet:?xt=urn:btih:c9e15763f722f23e98a29decdfae341b98d53056&amp;dn=x</link>
    <pubDate>Tue, 7 Oct 26 01:02:03 GMT</pubDate>
    <torznab:attr name="size" value="2048" />
  </item>
  <item>
    <title>No link at all &#233;t&#xE9;</title>
  </item>
</channel>
</rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title type="text">Atom tracker</title>
  <entry>
    <title>Book One (EPUB)</title>
    <id>urn:uuid:1225c695-cfb8-4ebb-aaaa-80da344efa6a</id>
    <updated>2026-10-08T10:30:00Z</updated>
    <link rel="alternate" href="https://atom.example/book/1"/>
    <link rel="enclosure" type="application/x-bittorrent" length="4096" href="https://atom.example/book/1.torrent"/>
    <summary>A book</summary>
    <author><name>someone</name></author>
  </entry>
  <entry>
    <title>Book Two</title>
    <id>book-2</id>
    <published>2026-10-07T23:00:00.123+02:00</published>
    <link href="https://atom.example/book/2.torrent"/>
  </entry>
</feed>"#;

    #[test]
    fn rss2_items_give_their_torrent_guid_date_and_size() {
        let p = parse(RSS2.as_bytes()).unwrap();
        assert_eq!(p.title, "Example & Tracker");
        assert_eq!(p.articles.len(), 3, "{:?}", p.articles);
        let a = &p.articles[0];
        assert_eq!(a.title, "Show.Name.S01E05.1080p.WEB <x264>");
        assert_eq!(a.guid, "tracker-42");
        assert_eq!(a.torrent_url, "https://tracker.example/dl/42.torrent?passkey=SECRET&x=1");
        assert_eq!(a.link, "https://tracker.example/details/42");
        assert_eq!(a.size, 1_234_567, "the enclosure's length wins");
        assert_eq!(a.date, 1_791_453_600, "12:00 at +0200 is 10:00 UTC");
        assert_eq!(a.category, "TV");

        let m = &p.articles[1];
        assert!(m.torrent_url.starts_with("magnet:?xt=urn:btih:c9e1"), "{}", m.torrent_url);
        assert!(m.torrent_url.ends_with("&dn=x"));
        assert_eq!(m.guid, m.torrent_url, "no guid: the torrent link is the identity");
        assert_eq!(m.size, 2048, "torznab size");
        assert_eq!(m.date, parse_date("2026-10-07T01:02:03Z").unwrap(), "two-digit year, GMT");

        let n = &p.articles[2];
        assert_eq!(n.title, "No link at all été");
        assert_eq!(n.guid, n.title);
        assert!(n.torrent_url.is_empty());
    }

    #[test]
    fn atom_entries_give_their_enclosure_or_link() {
        let p = parse(ATOM.as_bytes()).unwrap();
        assert_eq!(p.title, "Atom tracker");
        assert_eq!(p.articles.len(), 2);
        let a = &p.articles[0];
        assert_eq!(a.guid, "urn:uuid:1225c695-cfb8-4ebb-aaaa-80da344efa6a");
        assert_eq!(a.torrent_url, "https://atom.example/book/1.torrent");
        assert_eq!(a.link, "https://atom.example/book/1");
        assert_eq!(a.size, 4096);
        assert_eq!(a.date, 1_791_455_400);
        assert_eq!(a.description, "A book");
        assert_eq!(a.author, "someone");
        let b = &p.articles[1];
        assert_eq!(b.torrent_url, "https://atom.example/book/2.torrent", "a plain link is the torrent");
        assert_eq!(b.date, 1_791_406_800, "23:00 at +02:00, fractions dropped");
    }

    #[test]
    fn something_that_is_not_a_feed_is_refused() {
        assert!(parse(b"<html><body>login required</body></html>").is_err());
        assert!(parse(b"not xml at all").is_err());
        assert!(parse(b"").is_err());
        // An empty channel is a feed with nothing in it.
        assert_eq!(parse(b"<rss><channel><title>t</title></channel></rss>").unwrap().articles.len(), 0);
    }

    /// Dropping the tree recurses once per level, so a feed nested a million
    /// deep must be an error, not a stack to run out of.
    #[test]
    fn a_feed_nested_absurdly_deep_is_refused() {
        let deep = "<a>".repeat(1_000_000);
        assert!(parse(deep.as_bytes()).is_err());
        let deep_then_closed = format!("{}{}", "<a>".repeat(1_000_000), "</a>".repeat(1_000_000));
        assert!(parse(deep_then_closed.as_bytes()).is_err());
        // Real feeds are a handful of levels deep; a generous nesting still reads.
        let ok = format!("<rss><channel>{}{}</channel></rss>", "<x>".repeat(50), "</x>".repeat(50));
        assert!(parse(ok.as_bytes()).is_ok());
    }

    #[test]
    fn dates_round_trip_through_rfc2822() {
        let t = parse_date("Thu, 08 Oct 2026 10:00:00 +0000").unwrap();
        assert_eq!(rfc2822(t), "Thu, 08 Oct 2026 10:00:00 +0000");
        assert_eq!(parse_date("08 Oct 2026 10:00:00 EST").unwrap(), t + 5 * 3600, "no weekday, named zone");
        assert_eq!(parse_date("2026-10-08").unwrap(), t - 10 * 3600);
        assert_eq!(rfc2822(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert!(parse_date("yesterday").is_none());
    }
}
