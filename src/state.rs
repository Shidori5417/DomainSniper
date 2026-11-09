//! Domain state enum, transition rules and check interval calculation.
//!
//! `decide` is a pure function: it turns the previous DB snapshot plus one observation into the
//! new state, the values to persist and the events to notify about. Nothing here does I/O.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::check::parking::ParkingEvidence;
use crate::check::{CheckError, Lookup, Registration};
use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Aktif,
    ParkSatilik,
    BitisYakin,
    SuresiDoldu,
    Redemption,
    PendingDelete,
    Bosta,
    Askida,
    Bilinmiyor,
}

impl State {
    pub const ALL: [State; 9] = [
        State::Aktif,
        State::ParkSatilik,
        State::BitisYakin,
        State::SuresiDoldu,
        State::Redemption,
        State::PendingDelete,
        State::Bosta,
        State::Askida,
        State::Bilinmiyor,
    ];

    /// The name stored in the database `state` column.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Aktif => "Aktif",
            State::ParkSatilik => "ParkSatilik",
            State::BitisYakin => "BitisYakin",
            State::SuresiDoldu => "SuresiDoldu",
            State::Redemption => "Redemption",
            State::PendingDelete => "PendingDelete",
            State::Bosta => "Bosta",
            State::Askida => "Askida",
            State::Bilinmiyor => "Bilinmiyor",
        }
    }
}

impl Serialize for State {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for State {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseStateError {
    given: String,
}

impl fmt::Display for ParseStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let valid: Vec<&str> = State::ALL.iter().map(|s| s.as_str()).collect();
        write!(
            f,
            "geçersiz durum {:?}; geçerli değerler: {}",
            self.given,
            valid.join(", ")
        )
    }
}

impl std::error::Error for ParseStateError {}

impl FromStr for State {
    type Err = ParseStateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        State::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| ParseStateError {
                given: s.to_string(),
            })
    }
}

/// Check interval for a state, from `[zamanlama]` (no jitter).
pub fn interval(state: State, cfg: &Config) -> Duration {
    let s = &cfg.schedule;
    let minutes = match state {
        State::Aktif => s.active,
        State::ParkSatilik => s.parked_for_sale,
        State::BitisYakin => s.expiring_soon,
        State::SuresiDoldu => s.expired,
        State::Redemption => s.redemption,
        State::PendingDelete => s.pending_delete,
        State::Bosta => s.available,
        State::Askida => s.suspended,
        State::Bilinmiyor => s.unknown,
    };
    Duration::minutes(i64::from(minutes))
}

/// What the decision needs from the stored domain row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Previous {
    pub state: State,
    pub expires_at: Option<DateTime<Utc>>,
    pub pending_delete_since: Option<DateTime<Utc>>,
    pub notified_thresholds: Vec<u32>,
    pub error_count: u32,
    pub pending_free_confirm: bool,
}

impl Default for Previous {
    /// A domain that was never checked.
    fn default() -> Self {
        Self {
            state: State::Bilinmiyor,
            expires_at: None,
            pending_delete_since: None,
            notified_thresholds: Vec::new(),
            error_count: 0,
            pending_free_confirm: false,
        }
    }
}

/// The raw result of one check run.
#[derive(Debug)]
pub enum Observation {
    Err(CheckError),
    Ok {
        /// Where the registry answer came from (RDAP URL / WHOIS server).
        source: String,
        lookup: Lookup,
        dns_ns: Vec<String>,
        parking: Option<ParkingEvidence>,
    },
}

/// Domain-independent facts worth notifying about; Phase 6 turns them into messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    StateChanged {
        from: State,
        to: State,
        detail: String,
    },
    /// A free domain was registered by someone again.
    Retaken {
        registrar: Option<String>,
    },
    PendingDelete {
        estimated_drop: DateTime<Utc>,
    },
    BecameFree,
    /// Expiry is `days_left` days away (rounded up) and a configured threshold was crossed.
    ExpiryWarning {
        days_left: i64,
    },
    Renewed {
        new_expires: DateTime<Utc>,
    },
    Unreachable {
        error_count: u32,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub from: State,
    pub to: State,
    pub detail: String,
}

/// Everything to persist after a check, plus the events to emit.
#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    pub state: State,
    pub next_check_in: Duration,
    pub pending_free_confirm: bool,
    pub pending_delete_since: Option<DateTime<Utc>>,
    pub notified_thresholds: Vec<u32>,
    pub error_count: u32,
    /// Registry data to store; `None` keeps what the row already has.
    pub registration: Option<Registration>,
    pub events: Vec<Event>,
    pub history: Option<HistoryEntry>,
}

impl Decision {
    /// Starting point: nothing changes and the next check follows the current state's interval.
    fn unchanged(prev: &Previous, cfg: &Config) -> Self {
        Self {
            state: prev.state,
            next_check_in: interval(prev.state, cfg),
            pending_free_confirm: prev.pending_free_confirm,
            pending_delete_since: prev.pending_delete_since,
            notified_thresholds: prev.notified_thresholds.clone(),
            error_count: prev.error_count,
            registration: None,
            events: Vec::new(),
            history: None,
        }
    }
}

/// The first four rows of the priority table; they need no parking evidence or expiry thresholds.
fn status_state(reg: &Registration, now: DateTime<Utc>) -> Option<(State, String)> {
    let has = |needle: &str| reg.statuses.iter().any(|s| s.contains(needle));
    if has("pending delete") {
        return Some((State::PendingDelete, "RDAP durumu: pending delete".into()));
    }
    if has("redemption period") {
        return Some((State::Redemption, "RDAP durumu: redemption period".into()));
    }
    if has("client hold") || has("server hold") {
        let hold = if has("server hold") {
            "server hold"
        } else {
            "client hold"
        };
        return Some((State::Askida, format!("RDAP durumu: {hold}")));
    }
    reg.expires.filter(|e| *e <= now).map(|expires| {
        (
            State::SuresiDoldu,
            format!("bitiş tarihi geçti ({})", expires.format("%Y-%m-%d")),
        )
    })
}

/// Parking detection is only worth its HTTP request when no status already decides the state.
pub fn needs_parking_check(reg: &Registration, now: DateTime<Utc>) -> bool {
    status_state(reg, now).is_none()
}

/// Whole days until `expires`, rounded up (23 h left counts as 1 day). Needs `expires > now`.
fn days_left(expires: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    const DAY_SECS: i64 = 86_400;
    ((expires - now).num_seconds() + DAY_SECS - 1).div_euclid(DAY_SECS)
}

fn classify(
    reg: &Registration,
    parking: Option<&ParkingEvidence>,
    now: DateTime<Utc>,
    cfg: &Config,
) -> (State, String) {
    if let Some(found) = status_state(reg, now) {
        return found;
    }
    let near_expiry = reg
        .expires
        .zip(cfg.alerts.expiry_thresholds_days.iter().max())
        .map(|(expires, max)| (days_left(expires, now), *max))
        .filter(|(left, max)| *left <= i64::from(*max));
    if let Some((left, _)) = near_expiry {
        let mut detail = format!("bitişe {left} gün kaldı");
        if let Some(evidence) = parking {
            detail.push_str(&format!("; ayrıca park/satılık izi: {}", evidence.reason));
        }
        return (State::BitisYakin, detail);
    }
    match parking {
        Some(evidence) => (State::ParkSatilik, evidence.reason.clone()),
        None => (State::Aktif, "kayıtlı".into()),
    }
}

fn pending_delete_days(domain: &str, cfg: &Config) -> i64 {
    let tld = domain.rsplit('.').next().unwrap_or(domain);
    let days = cfg
        .alerts
        .extension_pending_delete_days
        .get(tld)
        .copied()
        .unwrap_or(cfg.alerts.pending_delete_days);
    i64::from(days)
}

/// Decides the new state, persisted values and events for one observation of `domain`.
pub fn decide(
    domain: &str,
    prev: &Previous,
    obs: &Observation,
    now: DateTime<Utc>,
    cfg: &Config,
) -> Decision {
    match obs {
        Observation::Err(err) => decide_error(prev, err, cfg),
        Observation::Ok {
            lookup: Lookup::NotFound,
            dns_ns,
            ..
        } => decide_not_found(prev, dns_ns, cfg),
        Observation::Ok {
            lookup: Lookup::Registered(reg),
            parking,
            ..
        } => decide_registered(domain, prev, reg, parking.as_ref(), now, cfg),
    }
}

fn decide_error(prev: &Previous, err: &CheckError, cfg: &Config) -> Decision {
    let mut d = Decision::unchanged(prev, cfg);
    d.error_count = prev.error_count.saturating_add(1);
    d.next_check_in = d.next_check_in.min(interval(State::Bilinmiyor, cfg));
    if d.error_count == cfg.alerts.consecutive_error_alert {
        d.events.push(Event::Unreachable {
            error_count: d.error_count,
            reason: err.to_string(),
        });
    }
    d
}

fn decide_not_found(prev: &Previous, dns_ns: &[String], cfg: &Config) -> Decision {
    let mut d = Decision::unchanged(prev, cfg);
    d.error_count = 0;
    if prev.state == State::Bosta {
        d.pending_free_confirm = false;
    } else if !prev.pending_free_confirm {
        d.pending_free_confirm = true;
        d.next_check_in = Duration::minutes(i64::from(cfg.schedule.available_confirm_minutes));
    } else if dns_ns.is_empty() {
        d.pending_free_confirm = false;
        d.state = State::Bosta;
        d.next_check_in = interval(State::Bosta, cfg);
        d.pending_delete_since = None;
        d.notified_thresholds.clear();
        d.events.push(Event::BecameFree);
        d.history = Some(HistoryEntry {
            from: prev.state,
            to: State::Bosta,
            detail: "kayıt bulunamadı, NS yok (2 kez doğrulandı)".into(),
        });
    } else {
        d.pending_free_confirm = false;
    }
    d
}

fn decide_registered(
    domain: &str,
    prev: &Previous,
    reg: &Registration,
    parking: Option<&ParkingEvidence>,
    now: DateTime<Utc>,
    cfg: &Config,
) -> Decision {
    let mut d = Decision::unchanged(prev, cfg);
    d.error_count = 0;
    d.pending_free_confirm = false;
    d.registration = Some(reg.clone());

    let (state, detail) = classify(reg, parking, now, cfg);
    d.state = state;
    d.next_check_in = interval(state, cfg);

    d.pending_delete_since = match (state, prev.state, prev.pending_delete_since) {
        (State::PendingDelete, State::PendingDelete, Some(since)) => Some(since),
        (State::PendingDelete, _, _) => Some(now),
        _ => None,
    };

    if state != prev.state {
        d.history = Some(HistoryEntry {
            from: prev.state,
            to: state,
            detail: detail.clone(),
        });
    }
    let entering_pending_delete =
        state == State::PendingDelete && d.pending_delete_since == Some(now);
    if entering_pending_delete {
        let drop = now + Duration::days(pending_delete_days(domain, cfg));
        d.events.push(Event::PendingDelete {
            estimated_drop: drop,
        });
    } else if state != prev.state {
        if prev.state == State::Bosta {
            d.events.push(Event::Retaken {
                registrar: reg.registrar.clone(),
            });
        } else if prev.state != State::Bilinmiyor {
            d.events.push(Event::StateChanged {
                from: prev.state,
                to: state,
                detail,
            });
        }
    }

    // A domain coming back from Bosta is a fresh registration, not a renewal.
    let fresh = prev.state == State::Bosta;
    if fresh {
        d.notified_thresholds.clear();
    }
    if let (false, Some(old), Some(new)) = (fresh, prev.expires_at, reg.expires) {
        if new - old > Duration::days(1) {
            d.notified_thresholds.clear();
            d.events.push(Event::Renewed { new_expires: new });
        }
    }

    if let Some(expires) = reg.expires.filter(|e| *e > now) {
        let left = days_left(expires, now);
        let crossed: Vec<u32> = cfg
            .alerts
            .expiry_thresholds_days
            .iter()
            .copied()
            .filter(|t| left <= i64::from(*t) && !d.notified_thresholds.contains(t))
            .collect();
        if !crossed.is_empty() {
            d.events.push(Event::ExpiryWarning { days_left: left });
            d.notified_thresholds.extend(crossed);
            d.notified_thresholds.sort_unstable();
            d.notified_thresholds.dedup();
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    const DOMAIN: &str = "example.com";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    fn days(n: i64) -> DateTime<Utc> {
        now() + Duration::days(n)
    }

    fn reg(statuses: &[&str], expires: Option<DateTime<Utc>>) -> Registration {
        Registration {
            statuses: statuses.iter().map(|s| s.to_string()).collect(),
            expires,
            registrar: Some("Reg Inc".into()),
            ..Registration::default()
        }
    }

    fn registered(r: Registration, parking: Option<&str>) -> Observation {
        Observation::Ok {
            source: "test".into(),
            lookup: Lookup::Registered(r),
            dns_ns: vec!["ns1.example.net".into()],
            parking: parking.map(|reason| ParkingEvidence {
                reason: reason.into(),
            }),
        }
    }

    fn not_found(ns: &[&str]) -> Observation {
        Observation::Ok {
            source: "test".into(),
            lookup: Lookup::NotFound,
            dns_ns: ns.iter().map(|s| s.to_string()).collect(),
            parking: None,
        }
    }

    fn transient() -> Observation {
        Observation::Err(CheckError::Transient("timeout".into()))
    }

    fn prev(state: State) -> Previous {
        Previous {
            state,
            ..Previous::default()
        }
    }

    fn run(p: &Previous, obs: &Observation) -> Decision {
        decide(DOMAIN, p, obs, now(), &Config::default())
    }

    /// Applies a decision to the snapshot like the scheduler will persist it.
    fn apply(p: &Previous, d: &Decision) -> Previous {
        Previous {
            state: d.state,
            expires_at: d.registration.as_ref().map_or(p.expires_at, |r| r.expires),
            pending_delete_since: d.pending_delete_since,
            notified_thresholds: d.notified_thresholds.clone(),
            error_count: d.error_count,
            pending_free_confirm: d.pending_free_confirm,
        }
    }

    #[test]
    fn state_round_trips_through_text() {
        for state in State::ALL {
            assert_eq!(state.as_str().parse::<State>(), Ok(state));
            assert_eq!(state.to_string(), state.as_str());
        }
        let err = "aktif".parse::<State>().unwrap_err().to_string();
        assert!(
            err.contains("\"aktif\"") && err.contains("ParkSatilik"),
            "{err}"
        );
    }

    #[test]
    fn intervals_follow_config() {
        let cfg = Config::default();
        assert_eq!(interval(State::Aktif, &cfg), Duration::hours(24));
        assert_eq!(interval(State::PendingDelete, &cfg), Duration::minutes(10));
        assert_eq!(interval(State::Bilinmiyor, &cfg), Duration::minutes(30));
    }

    #[test]
    fn priority_mapping() {
        let far = Some(days(400));
        let cases: Vec<(&str, Registration, Option<&str>, State)> = vec![
            (
                "pending delete wins over all",
                reg(
                    &["pending delete", "redemption period", "client hold"],
                    Some(days(-5)),
                ),
                Some("p"),
                State::PendingDelete,
            ),
            (
                "redemption over hold",
                reg(&["redemption period", "server hold"], far),
                None,
                State::Redemption,
            ),
            (
                "client hold",
                reg(&["client hold"], far),
                None,
                State::Askida,
            ),
            (
                "server hold",
                reg(&["ok", "server hold"], Some(days(-1))),
                None,
                State::Askida,
            ),
            (
                "expired",
                reg(&["ok"], Some(days(-1))),
                Some("p"),
                State::SuresiDoldu,
            ),
            (
                "auto renew period expired",
                reg(&["auto renew period"], Some(days(-3))),
                None,
                State::SuresiDoldu,
            ),
            (
                "near expiry",
                reg(&["ok"], Some(days(20))),
                None,
                State::BitisYakin,
            ),
            (
                "near expiry and parked",
                reg(&["ok"], Some(days(20))),
                Some("NS: ns1.sedoparking.com"),
                State::BitisYakin,
            ),
            (
                "parked",
                reg(&["ok"], far),
                Some("NS: ns1.sedoparking.com"),
                State::ParkSatilik,
            ),
            (
                "plain active",
                reg(&["client transfer prohibited"], far),
                None,
                State::Aktif,
            ),
            ("no expiry known", reg(&["ok"], None), None, State::Aktif),
        ];
        for (name, r, parking, want) in cases {
            let d = run(&prev(State::Aktif), &registered(r, parking));
            assert_eq!(d.state, want, "{name}");
            assert_eq!(
                d.next_check_in,
                interval(want, &Config::default()),
                "{name}"
            );
        }
    }

    #[test]
    fn near_expiry_detail_mentions_parking() {
        let d = run(
            &prev(State::Aktif),
            &registered(
                reg(&["ok"], Some(days(20))),
                Some("NS: ns1.sedoparking.com"),
            ),
        );
        let detail = d.history.unwrap().detail;
        assert!(
            detail.contains("20 gün") && detail.contains("sedoparking"),
            "{detail}"
        );
    }

    #[test]
    fn parking_precheck_skips_decided_states() {
        assert!(needs_parking_check(&reg(&["ok"], Some(days(10))), now()));
        for statuses in [
            &["pending delete"][..],
            &["redemption period"],
            &["client hold"],
            &["server hold"],
        ] {
            assert!(!needs_parking_check(&reg(statuses, None), now()));
        }
        assert!(!needs_parking_check(&reg(&["ok"], Some(days(-1))), now()));
    }

    #[test]
    fn transient_error_keeps_state_and_alerts_once() {
        let cfg = Config::default();
        assert_eq!(cfg.alerts.consecutive_error_alert, 5);
        let mut p = prev(State::Aktif);
        p.expires_at = Some(days(100));
        let mut unreachable = 0;
        for n in 1..=8u32 {
            let d = run(&p, &transient());
            assert_eq!(d.state, State::Aktif);
            assert_eq!(d.error_count, n);
            assert_eq!(d.next_check_in, Duration::minutes(30));
            assert!(d.history.is_none() && d.registration.is_none());
            unreachable += d
                .events
                .iter()
                .filter(|e| matches!(e, Event::Unreachable { .. }))
                .count();
            assert_eq!(d.events.len(), usize::from(n == 5), "n={n}");
            p = apply(&p, &d);
        }
        assert_eq!(unreachable, 1);

        let ok = run(&p, &registered(reg(&["ok"], Some(days(100))), None));
        assert_eq!(ok.error_count, 0);
        p = apply(&p, &ok);
        let again = run(&p, &transient());
        assert_eq!(again.error_count, 1);
    }

    #[test]
    fn error_backoff_uses_shorter_interval() {
        let d = run(&prev(State::PendingDelete), &transient());
        assert_eq!(d.next_check_in, Duration::minutes(10));
        let d = run(&prev(State::Bilinmiyor), &transient());
        assert_eq!(d.next_check_in, Duration::minutes(30));
    }

    #[test]
    fn double_confirmation_sequence() {
        let mut p = prev(State::Aktif);
        p.expires_at = Some(days(100));

        let first = run(&p, &not_found(&[]));
        assert_eq!(first.state, State::Aktif);
        assert!(first.pending_free_confirm);
        assert_eq!(first.next_check_in, Duration::minutes(2));
        assert!(first.events.is_empty() && first.history.is_none());
        p = apply(&p, &first);

        let second = run(&p, &not_found(&[]));
        assert_eq!(second.state, State::Bosta);
        assert!(!second.pending_free_confirm);
        assert_eq!(second.events, [Event::BecameFree]);
        let h = second.history.as_ref().unwrap();
        assert_eq!((h.from, h.to), (State::Aktif, State::Bosta));
        assert_eq!(second.next_check_in, Duration::hours(6));
    }

    #[test]
    fn second_not_found_with_ns_resets_flag() {
        let mut p = prev(State::Aktif);
        p.pending_free_confirm = true;
        let d = run(&p, &not_found(&["ns1.example.net"]));
        assert_eq!(d.state, State::Aktif);
        assert!(!d.pending_free_confirm);
        assert_eq!(d.next_check_in, Duration::hours(24));
        assert!(d.events.is_empty() && d.history.is_none());
    }

    #[test]
    fn registered_after_flag_resets_it() {
        let mut p = prev(State::Aktif);
        p.pending_free_confirm = true;
        let d = run(&p, &registered(reg(&["ok"], Some(days(400))), None));
        assert!(!d.pending_free_confirm);
        assert!(d.events.is_empty());
    }

    #[test]
    fn already_free_stays_silent() {
        let mut p = prev(State::Bosta);
        p.pending_free_confirm = true;
        for obs in [not_found(&[]), not_found(&["ns1.x.net"])] {
            let d = run(&p, &obs);
            assert_eq!(d.state, State::Bosta);
            assert!(d.events.is_empty() && d.history.is_none());
            assert!(!d.pending_free_confirm);
            assert_eq!(d.next_check_in, Duration::hours(6));
        }
    }

    #[test]
    fn free_from_unknown_first_check_still_notifies() {
        let mut p = prev(State::Bilinmiyor);
        p.pending_free_confirm = true;
        let d = run(&p, &not_found(&[]));
        assert_eq!(d.events, [Event::BecameFree]);
        assert!(d.history.is_some());
    }

    #[test]
    fn free_to_registered_is_retaken() {
        let d = run(
            &prev(State::Bosta),
            &registered(reg(&["ok"], Some(days(365))), None),
        );
        assert_eq!(d.state, State::Aktif);
        assert_eq!(
            d.events,
            [Event::Retaken {
                registrar: Some("Reg Inc".into())
            }]
        );
        assert_eq!(d.history.unwrap().from, State::Bosta);
    }

    #[test]
    fn retaken_with_later_expiry_is_not_a_renewal() {
        let mut p = prev(State::Bosta);
        p.expires_at = Some(days(-100));
        p.notified_thresholds = vec![30, 7, 1];
        let d = run(&p, &registered(reg(&["ok"], Some(days(365))), None));
        assert_eq!(d.events.len(), 1);
        assert!(matches!(d.events[0], Event::Retaken { .. }));
        assert!(d.notified_thresholds.is_empty());
    }

    #[test]
    fn pending_delete_entry_estimates_drop() {
        let obs = registered(reg(&["pending delete"], Some(days(-40))), None);
        let d = run(&prev(State::Redemption), &obs);
        assert_eq!(d.state, State::PendingDelete);
        assert_eq!(d.pending_delete_since, Some(now()));
        assert_eq!(
            d.events,
            [Event::PendingDelete {
                estimated_drop: days(5)
            }]
        );
        assert_eq!(d.next_check_in, Duration::minutes(10));
    }

    #[test]
    fn pending_delete_uses_tld_override() {
        let mut cfg = Config::default();
        cfg.alerts
            .extension_pending_delete_days
            .insert("org".into(), 8);
        let obs = registered(reg(&["pending delete"], None), None);
        let org = decide("example.org", &prev(State::Aktif), &obs, now(), &cfg);
        assert_eq!(
            org.events,
            [Event::PendingDelete {
                estimated_drop: days(8)
            }]
        );
        let com = decide("example.com", &prev(State::Aktif), &obs, now(), &cfg);
        assert_eq!(
            com.events,
            [Event::PendingDelete {
                estimated_drop: days(5)
            }]
        );
        let nested = decide("a.example.co.org", &prev(State::Aktif), &obs, now(), &cfg);
        assert_eq!(
            nested.events,
            [Event::PendingDelete {
                estimated_drop: days(8)
            }]
        );
    }

    #[test]
    fn pending_delete_stay_keeps_original_since() {
        let mut p = prev(State::PendingDelete);
        p.pending_delete_since = Some(days(-2));
        let d = run(&p, &registered(reg(&["pending delete"], None), None));
        assert_eq!(d.pending_delete_since, Some(days(-2)));
        assert!(d.events.is_empty() && d.history.is_none());
    }

    #[test]
    fn leaving_pending_delete_clears_since() {
        let mut p = prev(State::PendingDelete);
        p.pending_delete_since = Some(days(-2));
        let d = run(&p, &registered(reg(&["ok"], Some(days(365))), None));
        assert_eq!(d.state, State::Aktif);
        assert_eq!(d.pending_delete_since, None);
        assert!(matches!(
            d.events[0],
            Event::StateChanged {
                from: State::PendingDelete,
                to: State::Aktif,
                ..
            }
        ));
    }

    #[test]
    fn active_and_parked_changes_emit_state_changed() {
        let far = Some(days(400));
        let to_parked = run(
            &prev(State::Aktif),
            &registered(reg(&["ok"], far), Some("NS: ns1.dan.com")),
        );
        assert_eq!(to_parked.state, State::ParkSatilik);
        match &to_parked.events[..] {
            [Event::StateChanged {
                from: State::Aktif,
                to: State::ParkSatilik,
                detail,
            }] => {
                assert!(detail.contains("ns1.dan.com"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
        let back = run(
            &prev(State::ParkSatilik),
            &registered(reg(&["ok"], far), None),
        );
        assert!(matches!(
            back.events[..],
            [Event::StateChanged {
                from: State::ParkSatilik,
                to: State::Aktif,
                ..
            }]
        ));
        let same = run(
            &prev(State::ParkSatilik),
            &registered(reg(&["ok"], far), Some("NS: x.dan.com")),
        );
        assert!(same.events.is_empty() && same.history.is_none());
    }

    #[test]
    fn first_check_writes_history_without_state_changed() {
        let d = run(
            &prev(State::Bilinmiyor),
            &registered(reg(&["ok"], Some(days(400))), None),
        );
        assert_eq!(d.state, State::Aktif);
        assert!(d.events.is_empty());
        let h = d.history.unwrap();
        assert_eq!((h.from, h.to), (State::Bilinmiyor, State::Aktif));
        assert!(d.registration.is_some());
    }

    #[test]
    fn first_check_into_pending_delete_still_emits_special_event() {
        let d = run(
            &prev(State::Bilinmiyor),
            &registered(reg(&["pending delete"], None), None),
        );
        assert_eq!(
            d.events,
            [Event::PendingDelete {
                estimated_drop: days(5)
            }]
        );
        assert!(d.history.is_some());
    }

    #[test]
    fn days_left_rounds_up() {
        assert_eq!(days_left(now() + Duration::hours(23), now()), 1);
        assert_eq!(days_left(now() + Duration::hours(24), now()), 1);
        assert_eq!(days_left(now() + Duration::hours(25), now()), 2);
        assert_eq!(days_left(now() + Duration::seconds(1), now()), 1);
    }

    #[test]
    fn expiry_boundary_day() {
        // 30 days and 1 minute left rounds up to 31: no warning yet.
        let just_outside = Some(days(30) + Duration::minutes(1));
        let d = run(
            &prev(State::Aktif),
            &registered(reg(&["ok"], just_outside), None),
        );
        assert!(d.events.is_empty());
        assert_eq!(d.state, State::Aktif);
        // Exactly 30 days left warns.
        let d = run(
            &prev(State::BitisYakin),
            &registered(reg(&["ok"], Some(days(30))), None),
        );
        assert_eq!(d.events, [Event::ExpiryWarning { days_left: 30 }]);
        assert_eq!(d.notified_thresholds, [30]);
        assert_eq!(d.state, State::BitisYakin);
    }

    #[test]
    fn expiry_warns_once_per_threshold() {
        let mut p = prev(State::BitisYakin);
        p.expires_at = Some(days(30));
        p.notified_thresholds = vec![30];
        let obs = registered(reg(&["ok"], Some(days(30))), None);
        assert!(run(&p, &obs).events.is_empty());

        let d = run(&p, &registered(reg(&["ok"], Some(days(7))), None));
        // expiry moved earlier, not a renewal
        assert_eq!(d.events, [Event::ExpiryWarning { days_left: 7 }]);
        assert_eq!(d.notified_thresholds, [7, 30]);
        let p2 = apply(&p, &d);
        assert!(run(&p2, &registered(reg(&["ok"], Some(days(6))), None))
            .events
            .is_empty());
    }

    #[test]
    fn expiry_crossing_several_thresholds_emits_one_event() {
        let d = run(
            &prev(State::Bilinmiyor),
            &registered(reg(&["ok"], Some(days(5))), None),
        );
        assert_eq!(d.events, [Event::ExpiryWarning { days_left: 5 }]);
        assert_eq!(d.notified_thresholds, [7, 30]);
        let d = run(
            &prev(State::BitisYakin),
            &registered(reg(&["ok"], Some(now() + Duration::hours(10))), None),
        );
        assert_eq!(d.events, [Event::ExpiryWarning { days_left: 1 }]);
        assert_eq!(d.notified_thresholds, [1, 7, 30]);
    }

    #[test]
    fn no_warning_for_past_expiry() {
        let d = run(
            &prev(State::Aktif),
            &registered(reg(&["ok"], Some(days(-2))), None),
        );
        assert_eq!(d.state, State::SuresiDoldu);
        assert!(d
            .events
            .iter()
            .all(|e| !matches!(e, Event::ExpiryWarning { .. })));
    }

    #[test]
    fn renewal_resets_thresholds_and_reevaluates() {
        let mut p = prev(State::BitisYakin);
        p.expires_at = Some(days(5));
        p.notified_thresholds = vec![7, 30];
        let d = run(&p, &registered(reg(&["ok"], Some(days(370))), None));
        assert_eq!(d.state, State::Aktif);
        assert!(d.notified_thresholds.is_empty());
        assert!(matches!(
            d.events[..],
            [Event::StateChanged { .. }, Event::Renewed { .. }]
        ));
        assert_eq!(
            d.events[1],
            Event::Renewed {
                new_expires: days(370)
            }
        );

        // Renewed into the warning window again: thresholds are re-evaluated against the new date.
        let d = run(&p, &registered(reg(&["ok"], Some(days(25))), None));
        assert!(d.events.contains(&Event::Renewed {
            new_expires: days(25)
        }));
        assert!(d.events.contains(&Event::ExpiryWarning { days_left: 25 }));
        assert_eq!(d.notified_thresholds, [30]);
    }

    #[test]
    fn small_expiry_drift_is_not_a_renewal() {
        let mut p = prev(State::Aktif);
        p.expires_at = Some(days(100));
        let d = run(
            &p,
            &registered(reg(&["ok"], Some(days(100) + Duration::hours(12))), None),
        );
        assert!(d.events.is_empty());
    }
}
