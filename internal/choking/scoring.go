package choking

import "math"

// ---------------------------------------------------------------------------
// Pure scoring functions for the rarity_captive choking strategy.
//
// These are extracted from choking.go so they can be unit-tested independently
// and potentially reused by other strategies.
// ---------------------------------------------------------------------------

// ComputeRarityScore returns a score in [0, 1] that favours peers with fewer
// pieces (i.e., leechers who still need data). Peers that just connected
// (low duration) get a slight boost to give them a chance to prove themselves.
//
// peerCompletion is in [0, 1] (fraction of pieces the peer has).
// connectionDuration is in seconds.
func ComputeRarityScore(peerCompletion float64, connectionDuration float64) float64 {
	// A peer with 0% completion is maximally rare (score 1.0).
	// A peer with 100% completion is a seed and gets score 0.0.
	baseRarity := 1.0 - clamp(peerCompletion, 0, 1)

	// New connection bonus: peers connected for less than 30 seconds
	// get a small bonus (up to 0.1) to avoid being immediately choked
	// before they can exchange pieces.
	var newPeerBonus float64
	if connectionDuration < 30 {
		newPeerBonus = 0.1 * (1.0 - connectionDuration/30.0)
	}

	return clamp(baseRarity+newPeerBonus, 0, 1)
}

// ComputeSpeedScore returns a score in [0, 1] that rewards peers uploading
// faster data to us (reciprocation incentive).
//
// peerSpeed is the peer's upload rate (bytes/s) toward us.
// maxSpeed is the highest upload rate among all peers in this torrent.
func ComputeSpeedScore(peerSpeed float64, maxSpeed float64) float64 {
	if maxSpeed <= 0 {
		return 0
	}
	return clamp(peerSpeed/maxSpeed, 0, 1)
}

// ComputeFinalScore combines rarity, speed, and intel scores using
// configurable weights. The intel score fills the remaining weight
// (1 - rarityWeight - speedWeight).
//
// All input scores and weights should be in [0, 1].
func ComputeFinalScore(rarity, speed, intel, rarityWeight, speedWeight float64) float64 {
	intelWeight := 1.0 - rarityWeight - speedWeight
	if intelWeight < 0 {
		intelWeight = 0
	}

	score := rarity*rarityWeight + speed*speedWeight + intel*intelWeight
	return clamp(score, 0, 1)
}

// clamp restricts v to the range [lo, hi].
func clamp(v, lo, hi float64) float64 {
	return math.Max(lo, math.Min(hi, v))
}
