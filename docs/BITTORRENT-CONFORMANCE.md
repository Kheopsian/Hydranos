# Hydranos — BitTorrent protocol conformance

This document describes what Hydranos puts on the wire -- to a tracker and to
another peer -- which specification each part follows, and where it still
deviates. It
is written for tracker operators evaluating whether to allow the client, and it
is meant to be checked rather than believed: every claim below names the test
that asserts it, and the suite runs offline in a few seconds.

Hydranos is a BitTorrent client written in Rust (engine name: Typhon). It is
built for one unusual workload — a single instance seeding several hundred
thousand torrents — and that shape explains most of the design decisions here.

## Contents

- [Running the suite](#running-the-suite)
- [Specifications implemented](#specifications-implemented)
- [What a tracker receives](#what-a-tracker-receives)
- [The peer protocol](#the-peer-protocol)
- [Private torrents](#private-torrents)
- [Conformance by rule](#conformance-by-rule)
- [Known deviations](#known-deviations)
- [What this audit changed](#what-this-audit-changed)
- [Announce behaviour at scale](#announce-behaviour-at-scale)
- [Identity: peer id and User-Agent](#identity-peer-id-and-user-agent)

## Running the suite

The suite has two halves, because the client is built as two binaries: a
library (`typhon_engine`) holding the wire code, and a `hydra` binary holding
the announce policy.

```sh
cd typhon-engine

# Wire conformance: starts a real HTTP tracker on loopback, drives the real
# announce path against it, asserts on the query that arrived and on what the
# client made of the answer.
cargo test --test bep_conformance

# Request construction and event selection, one test per rule.
cargo test --bin hydra bep_rules
cargo test --bin hydra event_rules

# The peer half: handshake, message set, framing, private torrents.
cargo test --test bep_peer_conformance
```

Nothing in the suite reaches the network. The tracker is a `TcpListener` bound
to `127.0.0.1:0`, started per test, recording the request it received so the
assertions run against the bytes that arrived rather than against our own
builder's output. A test asserting on a string our own code produced would only
prove that the code is consistent with itself, which is precisely the objection
an operator should raise.

## Specifications implemented

| BEP | What | Status |
|---|---|---|
| 3 | Core protocol: metainfo, HTTP tracker, peer wire | Implemented |
| 5 | DHT | Implemented — never for a private torrent |
| 6 | Fast Extension | Implemented |
| 7 | IPv6 tracker extension (`peers6`, `ip=`) | Implemented |
| 9 | Metadata exchange (`ut_metadata`) | Implemented |
| 10 | Extension protocol | Implemented |
| 11 | Peer exchange (`ut_pex`) | Implemented — never for a private torrent |
| 12 | Multitracker metadata (tiers) | Implemented |
| 15 | UDP tracker protocol | **Not implemented** |
| 19 | WebSeed (`url-list`) | Implemented |
| 20 | Peer id conventions | Implemented |
| 23 | Compact peer lists | Implemented |
| 27 | Private torrents | Implemented |
| 48 | Scrape | **Not implemented** |
| 55 | Hole punching (`ut_holepunch`) | Implemented, both as client and rendezvous |
| — | MSE/PE encrypted connections | Implemented |

## What a tracker receives

A complete announce from Hydranos, for a seeding torrent on a tracker carrying
its passkey in the query string:

```
GET /announce?passkey=<PASSKEY>
  &info_hash=%AB%CD...            20 raw bytes, percent-encoded   (BEP 3)
  &peer_id=-HY4R00-xxxxxxxxxxxx   20 bytes, Azureus style         (BEP 20)
  &port=16171
  &uploaded=<cumulative bytes>
  &downloaded=<cumulative bytes>
  &left=0                         0 means seeding                 (BEP 3)
  &compact=1                      compact peer list requested     (BEP 23)
  &numwant=0                      a seeder has nothing to dial
  &key=1f4a9c02                   stable across address changes
  [&event=started|completed|stopped]                              (BEP 3)
  [&ip=<public address>]          only when the source address is wrong (BEP 7)
```

Parameter order is fixed, and matches what the 3.x Go implementation emitted so
that a tracker-side log diff across the Rust port shows nothing moved.

### Response keys understood

| Key | Handling |
|---|---|
| `failure reason` | Becomes an error; the rest of the dictionary is ignored and the tracker's own wording is surfaced to the operator verbatim. |
| `interval` | Honoured as the delay before the next announce for that torrent. |
| `min interval` | Honoured as a floor; it wins over `interval` when the two disagree. |
| `complete` / `incomplete` | Recorded and displayed as the swarm counts. |
| `peers` (byte string) | Compact, 6 bytes per peer (BEP 23). |
| `peers` (list of dicts) | Non-compact form, also accepted. |
| `peers6` | Compact IPv6, 18 bytes per peer (BEP 7). |

A malformed answer — HTML from a captive portal or a CDN error page — produces
an error, not a panic, and a truncated trailing entry in a compact list does
not discard the peers that arrived whole.

## The peer protocol

The handshake is the 68 bytes BEP 3 specifies, and the length is not
negotiable: the far side reads exactly that many, so a short peer id makes a
65-byte handshake and both ends wait on each other forever. The types enforce
it — `[u8; 20]`, not a slice.

```
<19><"BitTorrent protocol"><8 reserved><info_hash: 20><peer_id: 20>
                            ^
                            reserved[5] |= 0x10   BEP 10 extension protocol
                            reserved[7] |= 0x04   BEP 6 fast extension
```

No other reserved bit is set. Claiming an extension we do not implement leaves
a peer waiting for messages that never come.

Three things end a handshake before it starts: a protocol string that is not
BEP 3's, an info hash we do not hold (incoming) or did not ask for (outgoing),
and a peer id equal to our own — which means the tracker or the DHT handed us
our own listener, and dialling on would loop us onto ourselves.

Messages are the BEP 3 set (0–8), the BEP 6 set (13–17) and BEP 10's id 20,
each framed as a 4-byte big-endian length prefix. A length of zero is a
keepalive. An id we do not recognise is carried and ignored rather than being
treated as fatal, because BEP 3 says to ignore what you do not understand and a
client that disconnects instead breaks against every peer that gains a message
before we do.

The length prefix is four bytes a stranger controls, so it is bounded before
anything is allocated: a frame larger than 256 KiB — a 16 KiB block plus
headers, with room to spare — is refused on its header.

## Private torrents

BEP 27 is the rule a private tracker will ban an account over, so it is worth
being explicit: **a torrent whose info dict carries `private = 1` looks for
peers nowhere but its own trackers.** No DHT, no peer exchange, no local
discovery.

Both places that could leak — the DHT registration and the peer session's BEP
10 handshake — ask the same question, `TorrentMeta::allows_peer_discovery()`.
It used to be written out twice in two distant files, which is one copy too
many for a rule of this weight; there is now one predicate and a test that
holds it.

The flag is read as BEP 27 defines it: `private = 1` and nothing else. An
absent key is public, and so is `private = 0`.

## Conformance by rule

Every row is one test. `cargo test` runs all of them.

### The announce request

| Rule | Spec | Test |
|---|---|---|
| All mandatory parameters are emitted | BEP 3 | `bep3_every_mandatory_parameter_is_emitted` |
| …and all of them arrive at the tracker | BEP 3 | `bep3_the_mandatory_parameters_are_all_present` |
| `info_hash` is 20 raw bytes, not 40 hex characters | BEP 3 | `bep3_the_info_hash_is_twenty_bytes_not_forty_characters` |
| …verified on the wire | BEP 3 | `bep3_the_info_hash_decodes_to_exactly_twenty_bytes` |
| `peer_id` is exactly 20 bytes | BEP 20 | `bep20_the_peer_id_is_twenty_bytes` |
| …verified on the wire | BEP 20 | `bep3_the_peer_id_decodes_to_exactly_twenty_bytes` |
| `peer_id` follows the Azureus convention | BEP 20 | `bep20_the_peer_id_follows_the_azureus_convention` |
| A complete torrent announces `left=0` | BEP 3 | `bep3_a_complete_torrent_announces_left_zero` |
| Counters are non-negative decimals | BEP 3 | `bep3_the_counters_are_non_negative_decimals` |
| `compact=1` is requested | BEP 23 | `bep23_the_compact_peer_list_is_requested` |
| `ip=` appears only when there is one to declare | BEP 7 | `bep7_the_ip_parameter_appears_only_when_we_have_one_to_declare` |
| A seeder asks for no peers, a leecher does | — | `a_seeding_torrent_asks_for_no_peers_and_a_leeching_one_does` |
| A passkey in the tracker URL is never dropped | — | `a_passkey_in_the_tracker_url_is_never_dropped` |
| A malformed info hash builds no URL at all | — | `a_malformed_info_hash_builds_no_url` |
| `key` is sent | convention | `convention_a_key_is_sent_so_the_tracker_can_re_identify_us` |
| `key` does not change between announces | convention | `convention_the_key_does_not_change_between_two_announces` |

### Events

| Rule | Spec | Test |
|---|---|---|
| A periodic announce carries no `event` key at all | BEP 3 | `bep3_a_periodic_announce_carries_no_event_key` |
| Only the three defined events are emitted | BEP 3 | `bep3_only_the_three_defined_events_are_emitted` |
| The first announce, and only it, says `started` | BEP 3 | `the_first_announce_is_the_only_started_one` |
| A finished download reports `completed` | BEP 3 | `a_finished_download_reports_completed` |
| A stopped torrent reports `stopped` | BEP 3 | `a_stopped_torrent_reports_stopped` |
| An owed event outranks `started` | BEP 3 | `an_owed_event_outranks_the_first_announce` |

### The announce response

| Rule | Spec | Test |
|---|---|---|
| `failure reason` is an error, not a peer list | BEP 3 | `bep3_a_failure_reason_is_an_error_and_not_a_peer_list` |
| The tracker's `interval` is the one we use | BEP 3 | `bep3_the_interval_the_tracker_asks_for_is_the_one_we_report` |
| `min interval` is read as a floor | BEP 3 | `bep3_the_min_interval_floor_is_read` |
| An absent `min interval` is not a floor of zero | BEP 3 | `bep3_an_absent_min_interval_is_not_a_floor_of_zero` |
| Swarm counts survive the parse | BEP 3 | `bep3_the_swarm_counts_survive_the_parse` |
| A compact peer list is 6 bytes per peer | BEP 23 | `bep23_a_compact_peer_list_is_six_bytes_per_peer` |
| A truncated last entry does not lose the others | BEP 23 | `bep23_a_truncated_last_entry_does_not_lose_the_others` |
| The dictionary peer list is understood too | BEP 3 | `bep3_the_dictionary_peer_list_is_understood_too` |
| `peers6` is 18 bytes per peer | BEP 7 | `bep7_peers6_is_eighteen_bytes_per_peer` |
| Both families in one answer are both kept | BEP 7 | `bep7_both_families_in_one_answer_are_both_kept` |
| A non-bencode answer is an error, not a panic | — | `a_non_bencode_answer_is_an_error_not_a_panic` |
| The User-Agent asked for is the one sent | — | `the_user_agent_asked_for_is_the_one_sent` |

### The peer handshake

| Rule | Spec | Test |
|---|---|---|
| The handshake is 68 bytes | BEP 3 | `bep3_the_handshake_is_sixty_eight_bytes` |
| pstrlen is 19 and pstr is "BitTorrent protocol" | BEP 3 | `bep3_the_protocol_string_is_the_one_the_spec_names` |
| info hash at offset 28, peer id at 48 | BEP 3 | `bep3_the_info_hash_and_peer_id_sit_where_the_spec_puts_them` |
| Reserved claims fast and extended, nothing else | BEP 6, 10 | `the_reserved_bytes_claim_fast_and_extended_and_nothing_more` |
| A handshake round trips | BEP 3 | `bep3_a_handshake_round_trips` |
| A foreign protocol string is refused | BEP 3 | `bep3_a_foreign_protocol_string_is_refused` |
| A peer claiming no extension is read as claiming none | BEP 10 | `a_peer_claiming_no_extension_is_read_as_claiming_none` |

### The peer message set

| Rule | Spec | Test |
|---|---|---|
| The core ids are 0-8 as numbered | BEP 3 | `bep3_the_core_message_ids_are_the_numbers_the_spec_gives` |
| The core messages round trip | BEP 3 | `bep3_the_core_messages_round_trip_through_the_wire` |
| A piece index is big-endian | BEP 3 | `bep3_a_piece_index_is_big_endian` |
| A piece carries index, begin and block | BEP 3 | `bep3_a_piece_carries_its_index_begin_and_block` |
| A bitfield is passed through untouched | BEP 3 | `bep3_a_bitfield_is_passed_through_untouched` |
| The fast extension ids are 13-17 | BEP 6 | `bep6_the_fast_extension_ids_are_the_numbers_the_spec_gives` |
| A reject echoes the request it refuses | BEP 6 | `bep6_a_reject_echoes_the_request_it_refuses` |
| Have all and have none carry no payload | BEP 6 | `bep6_have_all_and_have_none_carry_no_payload` |
| An extended message is id 20 then the sub id | BEP 10 | `bep10_an_extended_message_is_id_twenty_then_the_sub_id` |

### Framing

| Rule | Spec | Test |
|---|---|---|
| A 4-byte big-endian length prefix | BEP 3 | `bep3_the_length_prefix_is_four_bytes_big_endian` |
| A keepalive is a length of zero | BEP 3 | `bep3_a_keepalive_is_a_length_of_zero_and_no_payload` |
| An oversized frame is refused on its header | — | `an_oversized_frame_is_refused_on_its_header` |
| A partial frame waits instead of guessing | — | `a_partial_frame_waits_instead_of_guessing` |
| Two messages in one read decode separately | — | `two_messages_in_one_read_decode_separately` |
| An unknown id is ignored, not fatal | BEP 3 | `an_unknown_message_id_is_ignored_not_fatal` |

### Private torrents

| Rule | Spec | Test |
|---|---|---|
| `private = 1` is parsed as private | BEP 27 | `bep27_a_private_torrent_is_parsed_as_private` |
| An absent key is public | BEP 27 | `bep27_a_torrent_without_the_key_is_public` |
| `private = 0` is not private | BEP 27 | `bep27_private_zero_is_not_private` |
| A private torrent allows no peer discovery | BEP 27 | `bep27_a_private_torrent_allows_no_peer_discovery` |
| A public torrent does allow it | BEP 27 | `bep27_a_public_torrent_allows_peer_discovery` |

## Known deviations

Stated first rather than buried, because an operator will find them anyway and
a client that hides them is not worth allowing.

### Scrape is not implemented

Hydranos never sends a scrape request. Swarm counts come from the `complete`
and `incomplete` fields of the announce response instead. This is a missing
feature rather than a violation — BEP 48 is an extension, and nothing in the
protocol requires a client to scrape — but it is worth stating plainly: there
are no scrapes to examine, because there are none.

### UDP trackers are not supported, and `udp://` is not refused

BEP 15 is not implemented: announces go over HTTP only. A `udp://` tracker
inside a `.torrent` is therefore simply never reached, which is the expected
outcome for an unimplemented transport.

What is not expected: the tracker editor **accepts** a `udp://` URL typed by
hand, and the announce path then builds a request for it and hands it to an
HTTP client that cannot speak it. The tracker fails on every attempt until the
circuit breaker gives up on the host. Either the editor should refuse the
scheme or the transport should exist; today it does neither.

### Hole punching introduces both sides, and only public addresses

BEP 55 is implemented in both directions: we ask, we act on a `connect` by
dialling immediately -- the other end is opening its hole at that instant and it
closes within seconds -- and we serve as the rendezvous for two peers we hold.

Serving one means telling **both** sides, since the asker dials into a closed
NAT unless the other punches at the same moment. The register is the per-torrent
peer table that already exists; a session that cannot be introduced is answered
with BEP 55's `NotConnected` or `NoSupport` rather than with silence, so the
asker goes and asks somebody else.

Two limits are deliberate, because a rendezvous is a request to make a stranger
open a connection to an address of the asker's choosing:

- only public unicast addresses can be named. Loopback, private, link-local and
  documentation ranges are refused, or a swarm becomes an amplifier aimed at
  whatever sits on the rendezvous peer's own network
- at most sixteen introductions may be queued for one peer at a time, and a
  repeat of one already waiting is not queued again

### The peer id carries the version, and the version moves

Our peer id encodes the client version in its four Azureus characters, so it
changes when the version does. `key` now covers this — a tracker can recognise
us across the change — but a tracker that keys on peer id alone will still see
a new peer after an upgrade. Reflecting only `major.minor` in the peer id is
under consideration.

### Delivery of `stopped` is best-effort

The event is recorded on the torrent and sent by the announce runner on its
next pass, normally within seconds. If the process is killed between the two,
the event is lost and the tracker times the entry out as it would for any
client that crashed. No client can guarantee otherwise.

## What this audit changed

The suite was written first and run against the code as it stood. Four rules
failed; all four are fixed, and the tests that caught them are the ones listed
above.

| Defect | Consequence for a tracker | Status |
|---|---|---|
| `key` was never sent | No way to recognise a peer across an address change; combined with a version-derived peer id, an upgrade looked like a new peer | Fixed |
| `min interval` was never read | The floor a tracker imposes did not constrain a forced re-announce | Fixed |
| `event=completed` was never sent | Snatches were never recorded | Fixed |
| `event=stopped` was never sent | A stop was silent; the tracker kept us in the swarm until the entry went stale | Fixed |

The peer half of the protocol was audited the same way and needed no change:
all 27 of its rules passed on the first run. The defects were all on the
tracker side.

`stopped` is not new behaviour: the 3.x Go implementation sent it, from a
single call site, once per user-initiated stop. It was lost in the port to Rust
rather than removed deliberately, and this restores the same contract — one
announce per user action, never a bulk flush at shutdown.

## Announce behaviour at scale

The constraints a 300,000-torrent catalogue imposes, and what they mean for a
tracker:

- **One announce per torrent per interval**, at the cadence the tracker asked
  for. There is no independent timer and no client-chosen cadence.
- **Tier order is respected** (BEP 12): the first tracker in a tier that answers
  ends the attempt for that torrent. Walking every tier would announce the same
  torrent repeatedly and double-count upload on trackers sharing a swarm.
- **Admission is rate-limited on join**: at most 500 torrents enter the schedule
  per 10-second cycle, i.e. 50/s. Admitting a whole catalogue at once would make
  every torrent due at the same instant and produce a burst on first contact.
- **A circuit breaker per tracker host** stops announcing to a tracker that has
  stopped answering, instead of retrying into a wall.
- **A paused torrent announces to nobody** — including when a forced
  re-announce targets it directly. The one exception is the single `stopped`
  announce it owes on being paused, after which it leaves the schedule.
- **A seeding torrent sends `numwant=0`.** It is directly reachable and has
  nothing to dial, so asking for 200 peers per announce across a seeding
  catalogue would open thousands of idle connections to the same few large
  seedboxes. On a sampled basis a seeding torrent does ask for a small list;
  that is the only way to verify the tracker is handing out our address
  correctly.

## Identity: peer id and User-Agent

Hydranos presents one identity, to every tracker and to every peer.

The peer id is Azureus style: `-HY####-` followed by 12 random bytes. The four
version characters are base 36, because the minor version has passed 9 -- so
4.27.0 is `-HY4R00-` (`R` = 27). No client table in libtorrent or Transmission
carries an `-HY` entry, so other clients render it as "HY 4.R.0.0"; this is
cosmetic and affects no interoperability.

**Client spoofing was removed.** A per-tracker override used to replace the peer
id prefix and the User-Agent with another client's -- qBittorrent 5.2.2 in
practice -- on the announce and, since 4.26.0, on the peer handshake as well. It
is gone: the mechanism, the `[announce_clients]` configuration table, the API
routes that edited it, and the interface that exposed it.

The reason is not that it was hard to defend one tracker at a time. It is that a
client asking an operator to trust what it reports cannot at the same time
misreport the one thing the operator can check directly. Everything else in this
document -- the counters, the events, the private-torrent rule -- is a claim that
rests on our word. A spoofed peer id is that word being demonstrably false.

What replaces it is nothing. A tracker that does not want unrecognised clients
will see `-HY4R00-` and decide accordingly, which is the tracker's call to make.

For operators who need it: `TorrentMeta::allows_peer_discovery()`, the peer id
derivation and the announce URL builder are all covered by the test suite above,
so the identity on the wire can be checked rather than taken on trust.
