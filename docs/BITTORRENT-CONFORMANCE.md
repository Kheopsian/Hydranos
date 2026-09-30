# Hydranos — BitTorrent protocol conformance

This document describes what Hydranos puts on the wire, to a tracker and to
another peer: which specification each part follows, where it deviates, and
how to check both yourself. It is written for tracker operators deciding
whether to allow the client, and it is meant to be checked rather than
believed. Every claim below names the test that asserts it. The suite runs
offline in seconds, and a second suite runs the client against software
somebody else wrote: opentracker, Torrust in private mode, and
qBittorrent/libtorrent.

Hydranos is a BitTorrent client written in Rust (engine name: Typhon). It is
built for one unusual workload, a single instance seeding hundreds of thousands
of torrents, and that shape explains most of the design decisions here.

## Contents

- [Checking it yourself](#checking-it-yourself)
- [Specifications implemented](#specifications-implemented)
- [What a tracker receives](#what-a-tracker-receives)
- [Events and counters](#events-and-counters)
- [How often we announce](#how-often-we-announce)
- [The peer protocol](#the-peer-protocol)
- [Private torrents](#private-torrents)
- [Identity: peer id, key and User-Agent](#identity-peer-id-key-and-user-agent)
- [Interoperability](#interoperability)
- [Conformance by rule](#conformance-by-rule)
- [Known deviations](#known-deviations)
- [What the audits changed](#what-the-audits-changed)

## Checking it yourself

Everything runs from a checkout. There is no network access and no setup; the
trackers the unit suite talks to are HTTP servers it starts on `127.0.0.1:0`,
and it asserts on the requests that *arrived* rather than on what our own
builder meant to send.

```sh
cd typhon-engine

# The announcer: what goes in the query, the event sequence per tracker, the
# floors, and whole sessions played against a recording tracker.
cargo test --bin hydranos announce::

# The transport and the answer parser, and the per-torrent session counters.
cargo test --lib tracker::http torrent::

# The wire, against a real HTTP tracker on loopback.
cargo test --test bep_conformance

# The peer half: handshake, messages, framing, private torrents, live sessions.
cargo test --test bep_peer_conformance --test peer_session
```

And one guard over this document itself: every test named in a table below
must exist, or `cargo test --test conformance_doc` fails.

The interoperability suite needs Docker and nothing else. It starts
opentracker, Torrust in private mode and qBittorrent on a throwaway network,
runs against them, and removes everything, pass or fail:

```sh
tools/interop/run.sh
```

Both suites run on every push in CI (`.github/workflows/ci.yml`, jobs `rust`
and `interop`).

## Specifications implemented

| BEP | What | Status |
|---|---|---|
| 3 | Core protocol: metainfo, HTTP tracker, peer wire | Implemented |
| 5 | DHT | Implemented. Never for a private torrent |
| 6 | Fast Extension | Implemented |
| 7 | IPv6 tracker extension (`peers6`, `ip=`) | Implemented |
| 9 | Metadata exchange (`ut_metadata`) | Implemented |
| 10 | Extension protocol | Implemented |
| 11 | Peer exchange (`ut_pex`) | Implemented. Never for a private torrent, sent or received |
| 12 | Multitracker metadata (tiers) | Implemented |
| 15 | UDP tracker protocol | **Not implemented** (see [deviations](#known-deviations)) |
| 19 | WebSeed (`url-list`) | Implemented |
| 20 | Peer id conventions | Implemented |
| 23 | Compact peer lists | Implemented |
| 27 | Private torrents | Implemented, on what we send *and* on what we receive |
| 31 | Tracker returns `retry in` | Implemented |
| 48 | Scrape | **Not implemented**: we never scrape |
| 55 | Hole punching (`ut_holepunch`) | Implemented. Never for a private torrent |
| — | MSE/PE encrypted connections | Implemented, both directions |

## What a tracker receives

A complete announce from Hydranos 4.2.4, for a seeding torrent on a tracker
carrying its passkey in the query string:

```
GET /announce?passkey=<PASSKEY>
  &info_hash=%AB%CD...            20 raw bytes, every byte percent-encoded (BEP 3)
  &peer_id=-HY4240-xxxxxxxxxxxx   20 bytes, Azureus style                 (BEP 20)
  &port=16171
  &uploaded=<bytes this session>  since `started`, never the lifetime total
  &downloaded=<bytes this session> verified payload only
  &left=<bytes still needed>      from the pieces held; 0 means seeding   (BEP 3)
  &compact=1                                                             (BEP 23)
  &numwant=0                      a seeder has nothing to dial
  &key=1f4a9c02                   stable for the process, secret          (convention)
  [&event=started|completed|stopped]                                     (BEP 3)
  [&ip=<public address>]          only when the source address is wrong   (BEP 7)
  [&trackerid=<id>]               only if this tracker handed us one      (BEP 3)
User-Agent: Hydranos/4.2.4
```

A periodic announce carries **no `event` key at all**, not an empty one.

### Response keys understood

| Key | Handling |
|---|---|
| `failure reason` | The announce failed, whatever else the dictionary holds and whatever type the value is. The tracker's own wording reaches the operator verbatim, with the passkey redacted out of any URL. |
| `retry in` (BEP 31) | Read from a refusal: that many minutes before this tracker is asked again, or `never` for the rest of the session. |
| `warning message` | The announce counted; the warning is logged for the operator. |
| `interval` | Honoured as the delay before the next announce. A value of zero or below falls back to 1800 s. |
| `min interval` | A floor per tracker. The periodic schedule, races and internal re-announces never cross it. Two things do, as in qBittorrent: a `completed` or `stopped` event, each sent once, and a re-announce a person forces (the button or the API), at most once a minute per torrent. |
| `tracker id` | Kept for the session and echoed back as `trackerid=`. |
| `complete` / `incomplete` | Recorded and displayed as the swarm counts. A negative count reads as zero. |
| `peers` (byte string) | Compact, 6 bytes per peer (BEP 23). A truncated last entry is dropped, the others kept; port 0 is dropped. |
| `peers` (list of dicts) | Non-compact form, also accepted. A port outside 1–65535 is dropped rather than wrapped. |
| `peers6` | Compact IPv6, 18 bytes per peer (BEP 7). |

An HTTP error status is an error, not an empty swarm. A `Retry-After` header
in seconds is honoured like `retry in`. A body that is not bencode (a captive
portal, a CDN error page) is an error, not a panic.

## Events and counters

BEP 3 events belong to a (torrent, tracker) pair, not to a torrent. Hydranos
keeps, per torrent and per tracker, what that tracker has been told this
session (`TrackerSlot`), and derives every event from it:

- **`started`** opens a session with one tracker: its first announce after
  the torrent is loaded, resumed after a stop, or back from an error. A
  fail-over tracker, or one added while the torrent runs, gets its own
  `started` before anything else.
- **`completed`** is sent once to every tracker that saw us leeching, when the
  last piece verifies. It is retried if the tracker is down, and never sent
  for a torrent that was already complete when its session began: a cross-seed
  or a re-added torrent is a seed, not a snatch.
- **`stopped`** is sent to every tracker that heard `started`, when the torrent
  is paused or its data disappears. It is attempted once. A torrent that
  finished and was stopped before the announcer ran sends `completed`, then
  `stopped`.
- Events go out when they happen: the torrent is queued for an immediate
  announce, at most once a minute per torrent. A request inside that minute
  waits for the torrent's next scheduled announce.
- **Counters are the session's.** `uploaded` and `downloaded` count from zero
  at `started`, as every mainstream client does. The lifetime totals are
  persisted across restarts for the operator's own statistics, and they never
  reach a tracker. A `started` claiming hundreds of gigabytes is what tracker
  anti-cheat flags, and a tracker that credits a new peer's first report would
  count it twice.
- `uploaded` counts piece payload actually handed to the socket. `downloaded`
  counts payload that passed its SHA-1 check. `left` is computed from the
  pieces held, so data already on disk is not reported as missing.

## How often we announce

- **One announce per torrent per interval**, at the cadence the tracker sets.
  The scheduler never re-announces a torrent less than 60 seconds after the
  previous time: a shorter wait is replaced by the 30-minute default. The one
  exception is a race's registration retry, below. A manual re-announce is
  refused within 60 seconds of the previous one.
- **`min interval` is enforced per tracker on top of that**, races included. A
  floored tracker costs no request. A re-announce a person forces crosses it,
  exactly as qBittorrent's "Force reannounce" does: libtorrent's
  `ignore_min_interval`, which is also what autobrr and every tool driving
  qBittorrent's API get. A tracker's own `retry in` or `Retry-After` is never
  crossed, forced or not.
- **A race retries a tracker that has not registered it yet.** The `.torrent`
  often reaches the client before the tracker has finished taking the upload,
  and the tracker answers with a `failure reason` ("unregistered torrent").
  While a race still downloading gets only such refusals, it re-announces every
  7 seconds, 50 times at most, and stops at the first tracker that registers
  it. This is autobrr's reannounce action with its defaults, which trackers
  already see from every racing qBittorrent. No `min interval` is crossed,
  because a tracker that refuses a torrent has set none for it. A 429, a
  timeout or a tracker that does not answer at all is never retried this way.
- **Tier order is respected** (BEP 12). The first tracker in the tier list that
  answers ends the attempt; the next tier is only for failure. The exception is
  a race, a torrent being downloaded now, which announces to every tracker it
  carries, as libtorrent's `announce_to_all_trackers` does.
- **The catalogue joins gradually.** At startup, torrents enter the schedule
  spread over one default interval, so that every torrent does not fall due at
  the same instant.
- **A circuit breaker per tracker host** stops announcing to a tracker that has
  stopped answering: 5 failures open it for 10 minutes. An HTTP 429 or a
  `failure reason` counts as an answer, because the tracker is up. On a 429
  the scheduler slows down for that tracker instead.
- **A seeding torrent sends `numwant=0`.** It is reachable and has nothing to
  dial. It asks for 50 on its first announce to a tracker and on one announce
  in 64 afterwards, to check that the tracker hands out our address; a seed
  that finds it is not reachable asks for 50, to dial them itself.
- Nothing is announced while a torrent's data is being checked.

## The peer protocol

The handshake is the 68 bytes BEP 3 specifies, and the types enforce the
length (`[u8; 20]`, not a slice): a short peer id makes a 65-byte handshake,
and both ends then wait on each other forever.

```
<19><"BitTorrent protocol"><8 reserved><info_hash: 20><peer_id: 20>
                            reserved[5] |= 0x10   BEP 10 extension protocol
                            reserved[7] |= 0x04   BEP 6 fast extension
```

No other reserved bit is set. Three things end a handshake before it starts:
a protocol string that is not BEP 3's, an info hash we do not hold (incoming)
or did not ask for (outgoing), and a peer id equal to our own.

Messages are the BEP 3 set (0–8), the BEP 6 set (13–17) and BEP 10's id 20,
each framed by a 4-byte big-endian length. A length of zero is a keepalive. An
unknown id is ignored, not fatal. A frame larger than 256 KiB is refused on its
header, before anything is allocated.

Outgoing connections try plaintext first and fall back to MSE for peers that
require encryption; incoming MSE is accepted. Both directions are exercised
against libtorrent with encryption *required* (see
[Interoperability](#interoperability)).

## Private torrents

BEP 27 is the rule a private tracker will ban an account over: **a torrent
whose info dict carries `private = 1` gets its peers from its trackers and from
nowhere else.** For such a torrent:

- no DHT registration or lookup;
- no BEP 10 extension handshake, so no `ut_pex`, `ut_holepunch` or
  `ut_metadata` is advertised;
- **and nothing a peer sends unasked is acted on.** A PEX message or a
  hole-punch `connect` naming a peer is ignored. Not advertising an extension
  does not stop a peer from sending its messages anyway, so the guard sits on
  what we receive (`peer_sources_allowed`);
- no local discovery (LSD is not implemented at all).

The flag is read as BEP 27 defines it: `private = 1` and nothing else. An
absent key is public, and so is `private = 0`.

## Identity: peer id, key and User-Agent

Hydranos presents **one identity**, the same to every tracker and every peer:

- **Peer id** `-HY####-` followed by 12 random alphanumerics, drawn once per
  process. The id a tracker lists is the id that connects to you. The four
  version characters are base 36, one per component (4.2.4 → `-HY4240-`), so a
  minor version past 9 still fits the field.
- **User-Agent** `Hydranos/<version>`, on announces and on every other HTTP
  request the client makes.
- **`key`**: 8 hex characters, stable for the life of the process, derived from
  the peer id and a secret drawn at startup. It cannot be computed from the
  peer id alone, which every peer reads in the handshake.

**Client spoofing does not exist.** An earlier version could present another
client's peer id and User-Agent to chosen trackers. It was removed along with
its configuration and API: a client that asks an operator to trust what it
reports cannot misreport the one thing the operator can check directly.

## Interoperability

`tools/interop/run.sh` runs the client against software somebody else wrote,
pinned by image digest. Each test reads its verdict from the *other side's*
state:

| Counterpart | Version | What is checked | Read back from |
|---|---|---|---|
| opentracker | `lednerb/opentracker-docker` (digest in the script) | `started` as leecher, `completed` → one snatch, `stopped` → gone, resume → back as a seed with no second snatch; a cross-seed never counts as a snatch; `min interval` holds | its scrape |
| Torrust Tracker, private mode | `torrust/tracker` (digest in the script) | our exact peer id; `started` with zero counters despite a 900 GB lifetime total; the session's upload; nothing sent inside `min interval`, and a forced re-announce heard; `stopped` removes us; the snatch counted once; no key → refused, and the refusal reaches the operator | its REST API peer table |
| qBittorrent / libtorrent | 5.2.3 / 2.0.14 | libtorrent downloads a torrent from us, and we download one from it; MSE both ways with libtorrent *requiring* encryption; a private torrent transfers | libtorrent's own piece check; our hash check and our bytes on disk |

Test sources: `typhon-engine/src/hydra/announce/interop.rs` (trackers) and
`typhon-engine/tests/interop_libtorrent.rs` (peers).

## Conformance by rule

Every row is one test. `cargo test` runs all of them except the
interoperability suite, which needs Docker.

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
| `left` counts the pieces not held, not the traffic | BEP 3 | `left_counts_the_pieces_we_do_not_hold` |
| Counters are non-negative decimals | BEP 3 | `bep3_the_counters_are_non_negative_decimals`, `the_session_counters_never_go_negative` |
| `compact=1` is requested | BEP 23 | `bep23_the_compact_peer_list_is_requested` |
| `ip=` appears only when there is one to declare | BEP 7 | `bep7_the_ip_parameter_appears_only_when_we_have_one_to_declare` |
| A tracker id is echoed back as `trackerid` | BEP 3 | `bep3_a_tracker_id_is_echoed_back`, `a_tracker_id_is_echoed_on_the_next_announce` |
| A seeder asks for no peers, a leecher does | — | `a_seeding_torrent_asks_for_no_peers_and_a_leeching_one_does` |
| A passkey in the tracker URL is never dropped | — | `a_passkey_in_the_tracker_url_is_never_dropped` |
| A malformed info hash builds no URL at all | — | `a_malformed_info_hash_builds_no_url` |
| `key` is sent, stable, and not derivable from the peer id | convention | `convention_a_key_is_sent_so_the_tracker_can_re_identify_us`, `convention_the_key_does_not_change_between_two_announces`, `convention_the_key_is_not_a_function_of_the_public_peer_id_alone` |
| The User-Agent asked for is the one sent | — | `the_user_agent_asked_for_is_the_one_sent` |
| Trackers see the product's one User-Agent | — | `trackers_see_the_same_user_agent_as_everything_else` |
| The edited tracker list is the one announced to | — | `an_edited_tracker_list_is_the_one_announced_to` |

### Events and sessions

| Rule | Spec | Test |
|---|---|---|
| A session opens with `started`; later announces carry no event | BEP 3 | `a_session_opens_with_started_and_continues_without_an_event`, `bep3_a_periodic_announce_carries_no_event_key` |
| Only the three defined events exist | BEP 3 | `bep3_only_the_three_defined_events_are_emitted` |
| An unanswered `started` is sent again | BEP 3 | `an_unanswered_started_is_sent_again` |
| The whole life of a torrent, as the tracker receives it | BEP 3 | `the_wire_sequence_of_a_whole_session` |
| `completed` reaches every tracker that saw us leeching | BEP 3 | `completed_is_owed_to_every_tracker_that_heard_started`, `completed_reaches_every_tracker_that_saw_us_leeching` |
| A tracker that never heard `started` is never told `completed` | BEP 3 | `a_tracker_that_never_heard_started_is_not_told_completed` |
| A `completed` that fails is retried | BEP 3 | `a_completed_that_fails_is_retried` |
| `stopped` goes to the trackers that had us, and only them | BEP 3 | `stopped_is_owed_to_the_trackers_that_had_us`, `a_stop_owed_to_nobody_sends_nothing` |
| Finished and stopped together: `completed`, then `stopped` | BEP 3 | `completed_goes_out_before_stopped`, `a_stop_does_not_erase_an_owed_completion` |
| A failed `stopped` is not retried | BEP 3 | `a_failed_stopped_is_not_retried` |
| After a stop, the next session starts with `started` | BEP 3 | `after_a_stop_the_next_announce_is_started`, `a_resume_opens_a_new_announce_session` |
| A stop undone before it was sent owes no departure | BEP 3 | `a_stop_already_undone_owes_no_departure` |
| A fail-over tracker hears `started` first | BEP 3, 12 | `the_fail_over_tracker_hears_started_first` |
| Counters report the session, never the lifetime total | BEP 3 | `a_new_session_never_reports_the_lifetime_total`, `a_reload_keeps_the_lifetime_totals_and_reports_a_fresh_session` |
| Starting a running torrent keeps its session | BEP 3 | `starting_a_running_torrent_keeps_its_session` |
| A paused torrent announces to nobody | — | `a_paused_torrent_announces_to_nobody_however_it_was_asked` |
| A torrent whose data is gone leaves the swarm | — | `a_torrent_whose_data_is_gone_leaves_the_swarm` |
| Nothing is said while data is checked | — | `a_torrent_being_checked_says_nothing` |
| Events go out now, not at the next scheduled announce | — | `a_stop_and_a_resume_each_ask_for_an_announce_now` |

### Floors and refusals

| Rule | Spec | Test |
|---|---|---|
| `min interval` is read | BEP 3 | `bep3_the_min_interval_floor_is_read`, `the_min_interval_the_tracker_states_is_read` |
| An absent `min interval` is not a floor of zero | BEP 3 | `bep3_an_absent_min_interval_is_not_a_floor_of_zero` |
| `min interval` is a floor for the schedule, races and internal re-announces | BEP 3 | `min_interval_is_a_hard_floor`, `min_interval_holds_back_bumps_and_races_alike` |
| A re-announce a person forces crosses it, like qBittorrent's; a tracker's `retry in`/`Retry-After` is never crossed | — | `a_forced_reannounce_crosses_min_interval_but_not_a_retry_hint`, `a_forced_reannounce_crosses_min_interval_like_qbittorrent`, `a_person_s_bump_is_forced_and_an_internal_one_is_not` |
| A race the tracker has not registered is retried every 7 s, 50 times at most | — | `an_unregistered_race_is_retried_in_seconds`, `registration_retries_are_bounded_and_end_on_registration`, `only_a_registration_retry_may_wait_under_a_minute` |
| A tracker that does not answer is not retried in seconds | — | `a_tracker_that_does_not_answer_is_not_retried_in_seconds` |
| Events are not held by the floor | BEP 3 | `events_are_not_held_by_the_floor` |
| `retry in` minutes, and `never` | BEP 31 | `bep31_retry_in_is_read_from_a_refusal`, `bep31_retry_in_is_obeyed`, `bep31_never_means_never` |
| HTTP `Retry-After` is obeyed | RFC 9110 | `retry_after_is_obeyed` |
| A hint cannot park a torrent for more than a day | — | `a_retry_hint_is_capped_at_a_day` |
| `failure reason` is an error, whatever its type | BEP 3 | `bep3_a_failure_reason_is_an_error_and_not_a_peer_list`, `a_failure_reason_of_any_type_is_a_refusal` |
| `warning message` is carried; the answer still counts | — | `a_warning_is_carried_and_the_answer_still_counts` |
| Impossible values are not wrapped into huge ones | — | `impossible_values_are_not_wrapped_into_huge_ones` |
| A 429 slows us down rather than tripping the breaker | — | `a_tracker_asking_to_slow_down_is_not_set_aside` |
| A host the breaker refuses is not announced to | — | `a_host_the_breaker_refuses_is_not_announced_to` |
| A passkey never reaches the logs | — | `an_error_message_never_carries_the_url` |
| A `udp://` tracker typed by hand is refused, with the reason | BEP 15 | `a_udp_tracker_is_refused_with_the_reason` |

### The announce response

| Rule | Spec | Test |
|---|---|---|
| The tracker's `interval` is the one we use | BEP 3 | `bep3_the_interval_the_tracker_asks_for_is_the_one_we_report` |
| Swarm counts survive the parse | BEP 3 | `bep3_the_swarm_counts_survive_the_parse` |
| A compact peer list is 6 bytes per peer | BEP 23 | `bep23_a_compact_peer_list_is_six_bytes_per_peer` |
| A truncated last entry does not lose the others | BEP 23 | `bep23_a_truncated_last_entry_does_not_lose_the_others` |
| A peer on an impossible port is dropped | — | `a_peer_on_an_impossible_port_is_dropped` |
| The dictionary peer list is understood too | BEP 3 | `bep3_the_dictionary_peer_list_is_understood_too` |
| `peers6` is 18 bytes per peer | BEP 7 | `bep7_peers6_is_eighteen_bytes_per_peer` |
| Both families in one answer are both kept | BEP 7 | `bep7_both_families_in_one_answer_are_both_kept` |
| Two family answers: the stricter floor and the tracker id survive | BEP 7 | `the_merge_keeps_the_stricter_floor_and_the_tracker_id` |
| A non-bencode answer is an error, not a panic | — | `a_non_bencode_answer_is_an_error_not_a_panic` |

### Tiers

| Rule | Spec | Test |
|---|---|---|
| A hoard stops at the first tier that answers | BEP 12 | `a_hoard_stops_at_the_first_tier_that_answers` |
| A tracker that has us and asked for quiet still holds its tier | BEP 12 | `a_quiet_tracker_holds_its_tier_and_a_dead_one_does_not` |

### Identity

| Rule | Spec | Test |
|---|---|---|
| Every listener and the announcer present the same peer id | BEP 20 | `every_caller_gets_the_same_peer_id` |
| Two engines in one process are two peers | BEP 20 | `two_engines_have_two_identities` |
| The fingerprint, then twelve alphanumerics | BEP 20 | `the_peer_id_is_the_fingerprint_then_twelve_alphanumerics` |
| The fingerprint carries the real version | BEP 20 | `the_fingerprint_carries_the_real_version` |

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

### The peer message set and framing

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
| A complete seed opens with `have all` when fast is on | BEP 6 | `a_complete_seed_opens_with_have_all_when_fast_is_on` |
| …and with a full bitfield when it is off | BEP 3 | `the_same_seed_opens_with_a_bitfield_when_fast_is_off` |
| An extended message is id 20 then the sub id | BEP 10 | `bep10_an_extended_message_is_id_twenty_then_the_sub_id` |
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
| A private torrent learns no peer from PEX, even unasked | BEP 27 | `bep27_a_private_torrent_learns_no_peer_from_pex_even_unasked` |
| A private torrent dials no peer a hole punch names | BEP 27, 55 | `bep27_a_private_torrent_dials_no_peer_a_hole_punch_names` |

The two "unasked" tests each run a public control first. It proves the probe
does see a learned or dialled peer, so the zero measured on the private torrent
is not a probe that always reads zero.

## Known deviations

Stated first rather than buried: an operator will find them anyway.

### UDP trackers are not supported

BEP 15 is not implemented; announces go over HTTP(S) only. A `udp://` tracker
inside a `.torrent` is never contacted, and one typed into the tracker editor
is refused with the reason (`a_udp_tracker_is_refused_with_the_reason`).

### Scrape is not implemented

Hydranos never sends a scrape request. Swarm counts come from the `complete`
and `incomplete` fields of the announce response. BEP 48 is an extension and
nothing requires a client to scrape. You will see no scrapes from this client
because it sends none.

### Two address families, one announce each

By default a tracker is announced to from IPv4 and from IPv6, with the same
peer id, the same `key` and the same event, as libtorrent 1.2+ does (one
announce per listen socket). BEP 7 describes this as one peer with two
addresses. A tracker that keys peers by id and overwrites instead of merging
can be pinned to one family per host in the configuration
(`[announce_ip_modes]`). On a host without IPv6, only one announce leaves.

### The peer id carries the version

The four version characters change when the version does. `key` covers this:
a tracker can recognise us across an upgrade within a process. A restart is a
new session everywhere anyway (`started`, a fresh peer id, counters from zero),
as with any client.

### `stopped` is attempted once, `completed` survives failures but not a restart

A departure that meets a tracker that is down is not retried; the tracker times
the entry out, as it would for a client that crashed. A `completed` owed to a
tracker that is down is retried at every announce until it lands, but it lives
in memory: a restart before it lands loses it. The snatch then shows as a seed
that never downloaded.

### Magnet links are not resolved

The daemon adds `.torrent` files only; a magnet link is refused at the API.
Every announce Hydranos sends therefore comes from the announcer described
here, for a torrent whose metadata it holds.

## What the audits changed

The suites were written first and run against the code as it stood; every rule
that failed was fixed, and the tests that caught them are the ones above.

### September 2026, round one

| Defect | Consequence for a tracker | Status |
|---|---|---|
| `key` was never sent | No way to recognise a peer across an address change | Fixed |
| `min interval` was never read | The tracker's floor was not honoured | Fixed |
| `event=completed` was never sent | Snatches were never recorded | Fixed |
| `event=stopped` was never sent | A stop was silent; the tracker kept us until the entry went stale | Fixed |

### 30 September 2026, round two

| Defect | Consequence for a tracker | Status |
|---|---|---|
| `uploaded`/`downloaded` were **lifetime totals**, persisted across restarts | Every restart sent `started` claiming the whole history, the anti-cheat signature; a tracker crediting a new peer's first report counted it twice | Fixed: session counters |
| The tracker was told one peer id and every peer handshake carried another | The peer a tracker listed never matched the peer that connected | Fixed: one id per process |
| Events were per torrent and spent on the first tracker | A fail-over tracker heard a periodic announce without `started`; other trackers never heard `completed` | Fixed: per-tracker book |
| An event was consumed before it was sent | A tracker that was down lost the `completed`, and with it the snatch | Fixed: retried |
| A stop overwrote an owed `completed` | Finished-then-stopped lost the snatch | Fixed |
| A resume sent no `started` | The tracker had dropped us at `stopped` and saw a periodic announce from an unknown peer | Fixed |
| `min interval` did not hold back races | A floor the tracker stated could be crossed on our own initiative | Fixed: enforced per tracker. A re-announce a person forces crosses it, as qBittorrent's does |
| A race refused as "unregistered" was retried 30 minutes later, and five refusals opened the breaker | Races lost to clients running autobrr's 7-second reannounce | Fixed: autobrr's cadence (7 s, 50 times), and a refusal counts as the answer it is |
| `left` was `size − downloaded` | Data already on disk was reported as missing; a 90 % torrent announced as 0 % | Fixed: from the piece map |
| `tracker id` was ignored | BEP 3 asks for it to be echoed back | Fixed |
| BEP 31 `retry in` and HTTP `Retry-After` were ignored | A tracker asking for quiet was asked again on schedule | Fixed |
| A `failure reason` that was not a string was read as success | A refusal counted as an announce | Fixed |
| Negative `interval`/counts wrapped into u32 | A negative interval became a 136-year wait | Fixed |
| PEX and hole-punch messages were processed on private torrents | A peer could make a private torrent dial an address it chose: a peer from outside the tracker | Fixed: guarded on receipt |
| The announce User-Agent said `Hydra/<v>` | It disagreed with every other request, and "Hydra" is also the name of a password brute-forcer | Fixed: `Hydranos/<v>` |
| `key` was an unsalted hash of the peer id | Anyone who read our peer id in a handshake could compute it | Fixed: salted per process |
| The announcer read the tracker list from the `.torrent`, not the edited one | A tracker removed by the operator was still announced to | Fixed |
| A torrent whose data vanished kept announcing as a seed | Leechers were sent to a peer that refuses every request | Fixed: `stopped`, then silence |
| Nothing checked the client against other implementations | Everything above was our reading of the BEPs against our own fixtures | Added: `tools/interop/run.sh`, in CI |

The peer half of the protocol was audited in round one and needed no change.
Round two found the BEP 27 hole on the receive side described above. The
interoperability suite found nothing else to fix: libtorrent accepts
our handshake, our framing and our MSE in both directions.
