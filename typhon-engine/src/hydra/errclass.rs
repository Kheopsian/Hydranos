//! What KIND of tracker error a torrent is in, so a library can be triaged by
//! the gesture each error calls for rather than by the string a tracker wrote.
//!
//! Measured on the production library: 1931 torrents in tracker_error carried
//! only 13 distinct messages, and those 13 collapse to the four classes below.
//! Grouping by raw message would split the one class that matters -- the dead
//! torrents -- across five spellings in two languages ("torrent introuvable",
//! "Unregistered torrent", "Torrent not registered on this tracker", "...with
//! this tracker", "not found"), which is precisely the split the operator is
//! trying to undo.
//!
//! The keys are stable machine names. The UI translates them; nothing here is
//! shown to anyone.

/// The classes, in order of how ACTIONABLE they are. The order is the tie-break
/// below, not decoration.
pub const CLASSES: [&str; 5] = ["auth", "dead", "throttled", "unreachable", "other"];

fn rank(class: &str) -> usize {
    CLASSES.iter().position(|c| *c == class).unwrap_or(CLASSES.len())
}

/// Classify ONE half of a tracker error message.
fn class_of_part(part: &str) -> &'static str {
    let s = part.to_ascii_lowercase();

    // Credentials. Checked first because a tracker that rejects the passkey
    // often says so in the same breath as "unregistered", and the passkey is
    // the fixable half: one edit clears every torrent on that tracker.
    if s.contains("passkey")
        || s.contains("unauthorized")
        || s.contains("authentication")
        || s.contains("invalid user")
        || s.contains(" 401")
        || s.contains(" 403")
    {
        return "auth";
    }

    // Gone from the tracker's side. This is the class worth acting on: the
    // torrent will never announce again, whatever we do.
    if s.contains("not registered")
        || s.contains("unregistered")
        || s.contains("introuvable")
        || s.contains("not found")
        || s.contains("has been deleted")
        || s.contains("inactif")
    {
        return "dead";
    }

    // Transient by construction: a quota, a rate limit, a per-torrent peer cap.
    // Nothing to do but wait, which is exactly why it deserves its own chip --
    // so it stops padding the list of things that look broken.
    if s.contains("429")
        || s.contains("too many requests")
        || s.contains("peers on this torrent")
        || s.contains("unsatisfied")
        || s.contains("rate limit")
        || s.contains("slow down")
    {
        return "throttled";
    }

    // The tracker, not us: DNS, routing, TCP, timeouts.
    if s.contains("timed out")
        || s.contains("timeout")
        || s.contains("unreachable")
        || s.contains("dns error")
        || s.contains("connect error")
        || s.contains("connection refused")
        || s.contains("no route")
        || s.contains("client error (connect)")
    {
        return "unreachable";
    }

    "other"
}

/// The class of a whole tracker error message. `""` for no error at all, so a
/// caller can use the empty string as "not in error" without a second check.
///
/// A tracker error carries BOTH stacks, as `v4: ... | v6: ...`, and the two
/// halves disagree often enough to matter: a torrent can be unregistered over
/// v4 while v6 merely failed to connect. Each half is classified and the most
/// actionable wins. The operator asks "what do I have to do about this", and
/// "fix the passkey" or "delete it" is an answer, while "the tracker was
/// unreachable over one of two stacks" is not.
pub fn classify(msg: &str) -> &'static str {
    if msg.trim().is_empty() {
        return "";
    }
    msg.split(" | ")
        .map(class_of_part)
        .min_by_key(|c| rank(c))
        .unwrap_or("other")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_error_is_not_a_class() {
        assert_eq!(classify(""), "");
        assert_eq!(classify("   "), "");
    }

    /// The messages the production library actually carries, verbatim.
    #[test]
    fn the_real_messages() {
        let cases = [
            ("v4: <url>): operation timed out | v6: <url>): client error (Connect): tcp connect error: Network unreachable", "unreachable"),
            ("v4: tracker: torrent introuvable ou inactif | v6: tracker: torrent introuvable ou inactif", "dead"),
            ("v4: <url> 429 Too Many Requests:  | v6: <url> 429 Too Many Requests: ", "throttled"),
            ("tracker: You already have 3 peers on this torrent. Ignoring.", "throttled"),
            ("v4: tracker: invalid passkey | v6: tracker: invalid passkey", "auth"),
            ("v4: tracker: Unregistered torrent: supprime ou introuvable sur V3X | v6: tracker: Unregistered torrent: supprime ou introuvable sur V3X", "dead"),
            ("v4: tracker: torrent introuvable | v6: tracker: torrent introuvable", "dead"),
            ("v4: tracker: Torrent not registered on this tracker | v6: tracker: Torrent not registered on this tracker", "dead"),
            ("v4: tracker: Torrent not registered with this tracker | v6: tracker: Torrent not registered with this tracker", "dead"),
            ("v4: <url>): client error (Connect): dns error: failed to lookup address information: Name or service not known", "unreachable"),
            ("v4: tracker: User currently at unsatisfied limit, you have 150 unsatisfied torrents. | v6: <url>): client error (Connect): tcp connect error", "throttled"),
            ("tracker: Torrent has been deleted.", "dead"),
        ];
        for (msg, want) in cases {
            assert_eq!(classify(msg), want, "misclassified: {msg}");
        }
    }

    /// The half that can be acted on decides, whichever stack it came from.
    #[test]
    fn the_actionable_half_wins() {
        assert_eq!(
            classify("v4: tracker: torrent not registered with this tracker | v6: <url>): client error (Connect): tcp connect error"),
            "dead",
        );
        assert_eq!(
            classify("v4: <url>): operation timed out | v6: tracker: invalid passkey"),
            "auth",
        );
    }

    #[test]
    fn anything_else_is_other() {
        assert_eq!(classify("tracker: the sky is falling"), "other");
    }
}
