# RSS feed types and URLs

A short guide to what you can paste into **RSS → Add feed**, and where a
tracker's API key goes. For rules, filters and auto-downloads see the wiki page
[RSS & Automatic Downloads](https://github.com/Kheopsian/Hydranos/wiki/RSS-and-Automatic-Downloads).

## Feed types Hydra accepts

| Type | Recognised by | Notes |
|---|---|---|
| RSS 2.0 | `<rss>` root | The usual tracker "RSS" link. |
| RSS 1.0 (RDF) | `<rdf:RDF>` root | Items beside the channel. |
| Atom | `<feed>` root | |
| Torznab | RSS 2.0 with `torznab:attr` elements | Size and other fields are read from them. |

A Torznab feed is plain RSS, so the Torznab *search* URLs below work as feeds.
Anything else (HTML, JSON, a Torznab `t=caps` answer) is refused as "not an RSS
or Atom feed".

## The key goes in the URL

Hydra sends **no auth headers** when it fetches a feed. If a tracker's docs say
"send the key in the `X-Api-Key` header" (or as a bearer token), that does not
apply here: put the key in the URL, as the tracker's query parameter (usually
`apikey=`, sometimes `passkey=`, `key=` or `torrent_pass=`).

## Building a Torznab URL

Prowlarr, Sonarr and Radarr all do the same thing: take the tracker's base URL,
add `/api`, then the query.

```
https://tracker.example.org/api?t=search&apikey=YOUR_KEY
```

Worked example, from a tracker that gives you the base URL
`https://tracker.example.org` and the key `0123abcd`:

1. Base URL: `https://tracker.example.org`
2. Add `/api`: `https://tracker.example.org/api`
3. Add the function and key: `https://tracker.example.org/api?t=search&apikey=0123abcd`

If the tracker already gave you a full URL with `/torznab/api` or similar, use
that path as given instead of `/api`. A Prowlarr indexer works the same way: its
Torznab URL is `http://prowlarr:9696/<indexer id>/api?t=search&apikey=PROWLARR_KEY`.

### The `t` values

| `t=` | Asks for |
|---|---|
| `caps` | What the tracker supports (an XML description, **not** a feed) |
| `search` | General search; with no `q=` it is the latest releases |
| `tvsearch` | TV |
| `movie` | Movies |
| `music` | Music |
| `book` | Books |

The tracker decides which of these it supports. Open
`https://tracker.example.org/api?t=caps&apikey=KEY` in a browser to see the list
(and its category ids). Use `t=search` unless you have a reason not to; it is
the one every Torznab tracker has.

### Narrowing with `&cat=`

Add `&cat=` with one or more category ids, comma separated:

```
https://tracker.example.org/api?t=search&apikey=0123abcd&cat=2000,2040
```

The ids come from `t=caps` (the Newznab ones: 2000 Movies, 5000 TV, 3000 Audio,
7000 Books; a tracker may add its own, 100000 and up).

## What the key looks like in logs

Hydra never logs or shows a feed URL whole. The query is replaced by `?***`,
credentials are dropped, and a path segment that looks like a key becomes `***`.
You will see `https://tracker.example.org/api?***` in logs, errors and the RSS
tab. The full URL stays in the feed's own settings.

## Troubleshooting

The RSS tab shows the last error of each feed. The tracker's own message is not
shown, so read the Hydra text like this:

| Problem | What the RSS tab shows (usually) |
|---|---|
| Wrong or missing key | `…/api?***: not an RSS or Atom feed (no <rss>, <feed> or <rdf:RDF> element)`. Torznab trackers answer a bad key with a small `<error code="100" …/>` XML and a 200, which is not a feed. Some answer `401` or `403` instead: `… answered 401 Unauthorized`. |
| Wrong path (no `/api`, wrong prefix) | `… answered 404 Not Found`, or `not XML: no element in the response` / `not an RSS or Atom feed` when the site answers with a web page. |
| Unsupported `t` (for example `tvsearch` on a movie-only tracker) | `not an RSS or Atom feed`, from a Torznab `<error code="203" …/>` ("function not available"). Check `t=caps`, or use `t=search`. |
| Tracker unreachable | `…: error sending request`, or a timeout. |

To see the tracker's real message, open the same URL in a browser or with
`curl` from the machine Hydra runs on.

Also remember that a feed is fetched through the route of the engine its
torrents go to (tunnel, interface, proxy), so a tracker that only accepts one
address needs the feed's engine to leave from it.
