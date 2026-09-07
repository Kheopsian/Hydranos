//! Measuring the tunnel with iperf3.
//!
//! Started from a request and outliving it: a run takes tens of seconds, and
//! the HTTP call that asks for one returns immediately. The result lands in
//! the bench database, which is what `/api/vpn-speedtest/latest` reads.

use std::sync::Arc;
use std::time::Duration;

/// One measurement, both directions.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Measurement {
    pub ts: i64,
    /// Megabits per second.
    pub upload_mbps: f64,
    pub download_mbps: f64,
    pub server: String,
    pub error: String,
}

/// A run in flight, so two cannot overlap.
///
/// iperf3 saturates the link by design. Two at once measure each other, and
/// the number they produce is meaningless -- worse than no number, because it
/// looks like one.
#[derive(Default)]
pub struct Runner {
    running: std::sync::atomic::AtomicBool,
    last: std::sync::RwLock<Option<Measurement>>,
}

impl Runner {
    pub fn last(&self) -> Option<Measurement> {
        self.last.read().unwrap().clone()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Start a run unless one is already going. Returns false when refused.
    pub fn start(self: &Arc<Self>, server: String, port: u16) -> bool {
        if self
            .running
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            return false;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let result = measure(&server, port).await;
            *me.last.write().unwrap() = Some(result);
            me.running.store(false, std::sync::atomic::Ordering::Release);
        });
        true
    }
}

/// Upload then download, one after the other.
///
/// Never together: iperf3 in both directions at once splits the link between
/// them and reports half of each.
async fn measure(server: &str, port: u16) -> Measurement {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let upload = run_iperf3(server, port, false).await;
    let download = run_iperf3(server, port, true).await;

    match (upload, download) {
        (Ok(u), Ok(d)) => Measurement {
            ts,
            upload_mbps: u,
            download_mbps: d,
            server: server.to_string(),
            error: String::new(),
        },
        (u, d) => {
            // Report which direction failed. "iperf3 failed" alone sends an
            // operator to look at the whole tunnel when only one leg is shut.
            let mut why = Vec::new();
            if let Err(e) = &u {
                why.push(format!("upload: {e}"));
            }
            if let Err(e) = &d {
                why.push(format!("download: {e}"));
            }
            Measurement {
                ts,
                upload_mbps: u.unwrap_or(0.0),
                download_mbps: d.unwrap_or(0.0),
                server: server.to_string(),
                error: why.join(", "),
            }
        }
    }
}

/// One direction. Returns megabits per second.
async fn run_iperf3(server: &str, port: u16, reverse: bool) -> Result<f64, String> {
    let mut cmd = tokio::process::Command::new("iperf3");
    cmd.arg("-c")
        .arg(server)
        .arg("-p")
        .arg(port.to_string())
        .arg("-J")
        .arg("-t")
        .arg("10");
    if reverse {
        cmd.arg("-R");
    }
    let out = tokio::time::timeout(Duration::from_secs(40), cmd.output())
        .await
        .map_err(|_| "iperf3 did not finish in 40s".to_string())?
        .map_err(|e| format!("cannot run iperf3: {e}"))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.trim().chars().take(200).collect());
    }
    parse_bits_per_second(&out.stdout).map(|bps| bps / 1_000_000.0)
}

/// Pull the summary throughput out of iperf3's JSON.
///
/// `sum_received` and not `sum_sent`: what arrived is the throughput, and the
/// two differ by exactly the packets that were lost.
pub fn parse_bits_per_second(stdout: &[u8]) -> Result<f64, String> {
    let v: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|e| format!("iperf3 json: {e}"))?;
    v.get("end")
        .and_then(|e| e.get("sum_received"))
        .and_then(|s| s.get("bits_per_second"))
        .and_then(|b| b.as_f64())
        .ok_or_else(|| "iperf3 json has no end.sum_received.bits_per_second".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_received_rate_is_the_one_that_counts() {
        let json = br#"{"end":{"sum_sent":{"bits_per_second":900000000.0},
                                "sum_received":{"bits_per_second":850000000.0}}}"#;
        // Sent is what we pushed, received is what arrived. The gap is loss,
        // and reporting the larger number would hide it.
        assert_eq!(parse_bits_per_second(json).unwrap(), 850_000_000.0);
    }

    #[test]
    fn a_reply_without_a_rate_is_an_error_not_a_zero() {
        // Zero would show as "the tunnel is dead" on a panel; an error says
        // "the measurement did not happen", which is a different thing.
        assert!(parse_bits_per_second(b"{}").is_err());
        assert!(parse_bits_per_second(b"not json").is_err());
    }

    #[tokio::test]
    async fn two_runs_cannot_overlap() {
        let r = Arc::new(Runner::default());
        // A host that cannot resolve: the run fails fast but still occupies
        // the slot while it does.
        assert!(r.start("iperf-nonexistent.invalid".into(), 5201));
        assert!(
            !r.start("iperf-nonexistent.invalid".into(), 5201),
            "two iperf3 runs at once measure each other, not the link"
        );
    }
}
