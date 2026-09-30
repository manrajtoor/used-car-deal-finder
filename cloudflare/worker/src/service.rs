//! Use cases (ingest + scores + alerts, deals, snapshot, alert list) written
//! against small storage ports and a notifier port. `d1.rs` implements the
//! ports on Cloudflare D1; the tests below use an in-memory fake. The HTTP and
//! cron handlers in `entry.rs` only translate requests into these calls.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::{json, Value};

use crate::alerts::{self, Alert, AlertRule, AlertsQuery, Notifier};
use crate::deals::DealsQuery;
use crate::ingest::{listing_id, parse_payload, Existing, Planner, SaveStats};
use crate::scores::{self, Group};
use crate::snapshot;
use crate::sql::Stmt;

/// The scorer's default `minComps`: `/api/deals` shows scores backed by at
/// least this many comparable cars unless the request or DEFAULT_MIN_COMPS
/// says otherwise.
pub const DEFAULT_DEALS_MIN_COMPS: f64 = 8.0;

/// An error with the HTTP status it should map to.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(m: impl Into<String>) -> Self {
        ApiError { status: 400, message: m.into() }
    }
    pub fn internal(m: impl Into<String>) -> Self {
        ApiError { status: 500, message: m.into() }
    }
}

/// Where listings live.
#[allow(async_fn_in_trait)] // single-threaded wasm: no Send bound needed
pub trait ListingStore {
    /// Stored state for those of `ids` that exist.
    async fn known(&self, ids: &[String]) -> Result<HashMap<String, Existing>, String>;
    /// Runs the planned writes atomically.
    async fn apply(&self, stmts: Vec<Stmt>) -> Result<(), String>;
    /// Active listings of these make/model groups, in the crawler's camelCase
    /// shape, and the ids among them that already have a stored score.
    async fn in_groups(&self, groups: &[Group]) -> Result<(Vec<Value>, BTreeSet<String>), String>;
}

/// Where stored scores live (`listing_scores`).
#[allow(async_fn_in_trait)]
pub trait ScoreStore {
    /// Runs score writes (and the expiry sweep) atomically.
    async fn save_scores(&self, stmts: Vec<Stmt>) -> Result<(), String>;
    /// `{comps, scored, matched, appraiser, deals}` for `q`, from stored scores.
    async fn stored_deals(&self, q: &DealsQuery, min_comps: f64) -> Result<Value, String>;
}

/// Where deal snapshots live.
#[allow(async_fn_in_trait)]
pub trait SnapshotStore {
    async fn save_snapshot(&self, stmts: Vec<Stmt>) -> Result<(), String>;
    async fn latest_snapshot(&self) -> Result<Option<Value>, String>;
}

/// Where snipe alerts live.
#[allow(async_fn_in_trait)]
pub trait AlertStore {
    /// The (listing id, price) keys of `ids` that were already alerted.
    async fn alerted(&self, ids: &[String]) -> Result<HashSet<(String, i64)>, String>;
    /// Runs alert writes (inserts, notified marks) atomically.
    async fn save_alerts(&self, stmts: Vec<Stmt>) -> Result<(), String>;
    /// Newest alerts first, in the API shape (`alerts::from_row`).
    async fn recent_alerts(&self, limit: usize) -> Result<Vec<Value>, String>;
}

/// POST /api/listings (without scores or alerts).
pub async fn ingest(store: &impl ListingStore, body: &Value, now: &str) -> Result<SaveStats, ApiError> {
    Ok(ingest_planned(store, body, now).await?.0.stats)
}

/// Runs the upsert and returns the planner (stats + fresh ids) and the batch.
async fn ingest_planned(store: &impl ListingStore, body: &Value, now: &str) -> Result<(Planner, Vec<Value>), ApiError> {
    let payload = parse_payload(body, now).map_err(ApiError::bad_request)?;
    let mut ids: Vec<String> = payload.listings.iter().filter_map(listing_id).map(str::to_string).collect();
    ids.sort();
    ids.dedup();
    let known = store.known(&ids).await.map_err(ApiError::internal)?;
    let mut planner = Planner::new(known);
    let stmts: Vec<Stmt> = payload.listings.iter().flat_map(|l| planner.plan(l, &payload.scope)).collect();
    if !stmts.is_empty() {
        store.apply(stmts).await.map_err(ApiError::internal)?;
    }
    Ok((planner, payload.listings))
}

/// What POST /api/listings answers: the save stats (the Go client reads
/// those) plus how many cars were rescored and the alerts this batch raised.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestOutcome {
    #[serde(flatten)]
    pub stats: SaveStats,
    pub rescored: usize,
    pub new_alerts: usize,
    /// Set when scoring or alerting failed. The listings are stored either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert_error: Option<String>,
}

/// POST /api/listings: store the batch, rescore the make/model groups of its
/// fresh listings (new, relisted or cheaper), store those scores, and alert
/// on the fresh ones that score as strong deals. Scoring and alerting never
/// fail the ingest: the listings are already committed.
pub async fn ingest_and_alert<S: ListingStore + ScoreStore + AlertStore>(
    store: &S,
    notifier: &impl Notifier,
    rule: &AlertRule,
    body: &Value,
    now: &str,
) -> Result<IngestOutcome, ApiError> {
    let (planner, batch) = ingest_planned(store, body, now).await?;
    let mut out = IngestOutcome { stats: planner.stats.clone(), rescored: 0, new_alerts: 0, alert_error: None };
    match rescore(store, rule, &batch, &planner.fresh, now).await {
        Ok(scored) => {
            out.rescored = scored.len();
            match raise_alerts(store, notifier, rule, &scored, &planner.fresh, now).await {
                Ok(n) => out.new_alerts = n,
                Err(e) => out.alert_error = Some(e),
            }
        }
        Err(e) => out.alert_error = Some(format!("rescoring failed: {e}")),
    }
    Ok(out)
}

/// Scores and stores the groups of the fresh listings; nothing fresh, no scoring.
async fn rescore<S: ListingStore + ScoreStore>(
    store: &S,
    rule: &AlertRule,
    batch: &[Value],
    fresh: &[String],
    now: &str,
) -> Result<Vec<scores::Scored>, String> {
    let groups = scores::fresh_groups(batch, fresh);
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let (rows, has_score) = store.in_groups(&groups).await?;
    // Scored with the alert rule's minComps, so every car an alert could
    // fire on is priced; /api/deals filters on the stored comp count.
    let scored = scores::score(&rows, rule.min_comps)?;
    store.save_scores(scores::upsert_statements(&scored, fresh, &has_score, now)).await?;
    Ok(scored)
}

/// Returns how many new alerts were stored.
async fn raise_alerts<S: AlertStore>(
    store: &S,
    notifier: &impl Notifier,
    rule: &AlertRule,
    scored: &[scores::Scored],
    fresh: &[String],
    now: &str,
) -> Result<usize, String> {
    let found = alerts::find(scored, fresh, rule);
    if found.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = found.iter().map(|a| a.listing_id.clone()).collect();
    let done = store.alerted(&ids).await?;
    let new: Vec<Alert> = found.into_iter().filter(|a| !done.contains(&a.key())).collect();
    if new.is_empty() {
        return Ok(0);
    }
    store.save_alerts(alerts::insert_statements(&new, now)).await?;
    // A notifier failure leaves the alerts stored and unmarked.
    match notifier.notify(&new).await {
        Ok(0) => {}
        Ok(_) => store.save_alerts(alerts::notified_statements(&new, now)).await?,
        Err(e) => return Err(format!("{} alert(s) stored, but notifying failed: {e}", new.len())),
    }
    Ok(new.len())
}

/// GET /api/alerts.
pub async fn recent_alerts(
    store: &impl AlertStore,
    q: &AlertsQuery,
    rule: &AlertRule,
    now: &str,
) -> Result<Value, ApiError> {
    let list = store.recent_alerts(q.limit).await.map_err(ApiError::internal)?;
    Ok(json!({ "generatedAt": now, "rule": rule.to_json(), "alerts": list }))
}

/// GET /api/deals, from stored scores.
pub async fn deals(store: &impl ScoreStore, q: &DealsQuery, now: &str) -> Result<Value, ApiError> {
    let min_comps = q.min_comps.unwrap_or(DEFAULT_DEALS_MIN_COMPS);
    let mut out = store.stored_deals(q, min_comps).await.map_err(ApiError::internal)?;
    out["generatedAt"] = Value::from(now);
    Ok(out)
}

/// The cron job: expire cars not seen since `cutoff`, then store the best
/// stored deals as the day's snapshot.
pub async fn take_snapshot<S: ScoreStore + SnapshotStore>(
    store: &S,
    min_comps: Option<f64>,
    now: &str,
    cutoff: &str,
    cron: Option<&str>,
) -> Result<Value, ApiError> {
    store.save_scores(scores::expire_statements(now, cutoff)).await.map_err(ApiError::internal)?;
    let q = DealsQuery { limit: snapshot::SNAPSHOT_DEALS, min_comps, ..Default::default() };
    let out = deals(store, &q, now).await?;
    store
        .save_snapshot(snapshot::statements(&out, now, cron, min_comps))
        .await
        .map_err(ApiError::internal)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::NoNotifier;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use serde_json::json;

    /// The fakes never await anything real, so one poll finishes the future.
    fn block_on<F: Future>(f: F) -> F::Output {
        let mut cx = Context::from_waker(Waker::noop());
        match pin!(f).as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("fake store future did not complete"),
        }
    }

    #[derive(Default)]
    struct Fake {
        known: HashMap<String, Existing>,
        rows: Vec<Value>,
        applied: RefCell<Vec<Stmt>>,
        snapshots: RefCell<Vec<Vec<Stmt>>>,
        fail: bool,
        /// Alert table: (listing id, price, deal JSON, notified).
        alert_rows: RefCell<Vec<(String, i64, String, bool)>>,
        active_calls: RefCell<usize>,
        /// Groups asked for by each in_groups call.
        group_calls: RefCell<Vec<Vec<Group>>>,
        /// listing_scores: id -> (discount, comps, deal JSON).
        score_rows: RefCell<HashMap<String, (f64, f64, Value)>>,
        expiries: RefCell<usize>,
    }

    impl ListingStore for Fake {
        async fn known(&self, ids: &[String]) -> Result<HashMap<String, Existing>, String> {
            if self.fail {
                return Err("d1 down".into());
            }
            Ok(self.known.iter().filter(|(k, _)| ids.contains(k)).map(|(k, v)| (k.clone(), v.clone())).collect())
        }
        async fn apply(&self, stmts: Vec<Stmt>) -> Result<(), String> {
            self.applied.borrow_mut().extend(stmts);
            Ok(())
        }
        async fn in_groups(&self, groups: &[Group]) -> Result<(Vec<Value>, BTreeSet<String>), String> {
            *self.active_calls.borrow_mut() += 1;
            self.group_calls.borrow_mut().push(groups.to_vec());
            let key = |l: &Value| (l["make"].as_str().unwrap_or("").to_string(), l["model"].as_str().unwrap_or("").to_string());
            let rows: Vec<Value> = self.rows.iter().filter(|l| groups.contains(&key(l))).cloned().collect();
            let has = self.score_rows.borrow().keys().cloned().collect();
            Ok((rows, has))
        }
    }

    /// Interprets the score statements the way SQLite would.
    impl ScoreStore for Fake {
        async fn save_scores(&self, stmts: Vec<Stmt>) -> Result<(), String> {
            let mut rows = self.score_rows.borrow_mut();
            for st in stmts {
                if st.sql.starts_with("INSERT OR REPLACE INTO listing_scores") {
                    let deal: Value = serde_json::from_str(st.params[7].as_str().unwrap()).unwrap();
                    rows.insert(st.params[0].as_str().unwrap().into(), (st.params[4].as_f64().unwrap(), st.params[5].as_f64().unwrap_or(0.0), deal));
                } else if st.sql.starts_with("DELETE FROM listing_scores WHERE listing_id = ?1") {
                    rows.remove(st.params[0].as_str().unwrap());
                } else if st.sql.starts_with("UPDATE listings SET removed_at") {
                    *self.expiries.borrow_mut() += 1;
                }
            }
            Ok(())
        }
        async fn stored_deals(&self, q: &DealsQuery, min_comps: f64) -> Result<Value, String> {
            let rows = self.score_rows.borrow();
            let key = |v: &Value, k: &str| scores::name_key(v[k].as_str().unwrap_or(""));
            let mut hits: Vec<&(f64, f64, Value)> = rows
                .values()
                .filter(|r| r.1 >= min_comps)
                .filter(|r| q.make.as_ref().is_none_or(|m| key(&r.2, "make") == scores::name_key(m)))
                .filter(|r| q.model.as_ref().is_none_or(|m| key(&r.2, "model") == scores::name_key(m)))
                .filter(|r| q.seller_type.as_ref().is_none_or(|t| r.2["sellerType"].as_str() == Some(t)))
                .collect();
            hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
            let matched = hits.len();
            let deals: Vec<Value> = hits.iter().take(q.limit).map(|r| r.2.clone()).collect();
            let scored = rows.values().filter(|r| r.1 >= min_comps).count();
            Ok(json!({"comps": self.rows.len(), "scored": scored, "matched": matched, "appraiser": null, "deals": deals}))
        }
    }

    /// Interprets the two alert statements the way SQLite would.
    impl AlertStore for Fake {
        async fn alerted(&self, ids: &[String]) -> Result<HashSet<(String, i64)>, String> {
            Ok(self.alert_rows.borrow().iter().filter(|r| ids.contains(&r.0)).map(|r| (r.0.clone(), r.1)).collect())
        }
        async fn save_alerts(&self, stmts: Vec<Stmt>) -> Result<(), String> {
            let mut rows = self.alert_rows.borrow_mut();
            for st in stmts {
                if st.sql.starts_with("INSERT OR IGNORE") {
                    let id = st.params[0].as_str().unwrap().to_string();
                    let price = st.params[1].as_f64().unwrap() as i64;
                    if !rows.iter().any(|r| r.0 == id && r.1 == price) {
                        rows.push((id, price, st.params[10].as_str().unwrap().to_string(), false));
                    }
                } else if st.sql.starts_with("UPDATE new_deal_alerts SET notified_at") {
                    let id = st.params[1].as_str().unwrap();
                    let price = st.params[2].as_f64().unwrap() as i64;
                    rows.iter_mut().filter(|r| r.0 == id && r.1 == price).for_each(|r| r.3 = true);
                }
            }
            Ok(())
        }
        async fn recent_alerts(&self, limit: usize) -> Result<Vec<Value>, String> {
            let rows = self.alert_rows.borrow();
            Ok(rows
                .iter()
                .rev()
                .take(limit)
                .map(|r| {
                    let notified = if r.3 { json!("T") } else { Value::Null };
                    alerts::from_row(&json!({"created_at": "T", "notified_at": notified, "deal": r.2}))
                })
                .collect())
        }
    }

    /// Records what it was asked to send; can pretend to deliver or to fail.
    #[derive(Default)]
    struct Outbox {
        sent: RefCell<Vec<String>>,
        deliver: bool,
        fail: bool,
    }

    impl Notifier for Outbox {
        async fn notify(&self, alerts: &[Alert]) -> Result<usize, String> {
            if self.fail {
                return Err("smtp down".into());
            }
            self.sent.borrow_mut().extend(alerts.iter().map(|a| a.listing_id.clone()));
            Ok(if self.deliver { alerts.len() } else { 0 })
        }
    }

    /// Ten normal dealer RAV4s already stored, as `active()` returns them, plus `extra`.
    fn stored_market(extra: &Value) -> Vec<Value> {
        let mut v: Vec<Value> = (0..10)
            .map(|i| {
                json!({"id": format!("d{i}"), "source": "autohebdo", "make": "Toyota", "model": "RAV4",
                       "year": 2019, "km": 60000 + i * 1000, "price": 24800 + (i % 3) * 200,
                       "sellerType": "Dealer", "province": "QC", "isDamaged": false})
            })
            .collect();
        v.push(extra.clone());
        v
    }

    fn car(id: &str, price: i64) -> Value {
        json!({"id": id, "source": "autohebdo", "make": "Toyota", "model": "RAV4", "year": 2019, "km": 61000,
               "price": price, "sellerType": "Dealer", "province": "QC", "isDamaged": false})
    }

    fn known(id: &str, price: f64, removed: bool) -> HashMap<String, Existing> {
        HashMap::from([(id.to_string(), Existing { price: Some(price), price_changes: 0, removed, ..Default::default() })])
    }

    #[test]
    fn a_new_cheap_car_is_alerted_once() {
        let cheap = car("cheap", 20000);
        let fake = Fake { rows: stored_market(&cheap), ..Default::default() };
        let out = Outbox::default();
        let rule = AlertRule::default();

        let r = block_on(ingest_and_alert(&fake, &out, &rule, &json!([cheap]), "T1")).unwrap();
        assert_eq!((r.stats.added, r.new_alerts, r.alert_error.clone()), (1, 1, None));
        assert_eq!(*out.sent.borrow(), vec!["cheap"]);
        assert!(!fake.alert_rows.borrow()[0].3, "nothing delivered: not marked notified");
        let wire = serde_json::to_value(&r).unwrap();
        assert_eq!(wire["added"], 1, "stats stay flat on the wire for the Go client");
        assert_eq!(wire["newAlerts"], 1);
        assert!(wire.get("alertError").is_none());

        // The crawler sees the same car again at the same price: not fresh,
        // so no scoring and no alert.
        let again = Fake {
            known: known("cheap", 20000.0, false),
            rows: fake.rows.clone(),
            alert_rows: RefCell::new(fake.alert_rows.borrow().clone()),
            ..Default::default()
        };
        let r = block_on(ingest_and_alert(&again, &out, &rule, &json!([cheap]), "T2")).unwrap();
        assert_eq!(r.new_alerts, 0);
        assert_eq!(*again.active_calls.borrow(), 0, "nothing fresh: the scorer is not run");
        assert_eq!(again.alert_rows.borrow().len(), 1);
    }

    #[test]
    fn same_key_never_twice_but_a_further_drop_alerts_again() {
        let rule = AlertRule::default();
        let out = Outbox { deliver: true, ..Default::default() };
        // Relisted at 20000 (fresh), but that key was alerted before.
        let fake = Fake {
            known: known("cheap", 20000.0, true),
            rows: stored_market(&car("cheap", 20000)),
            alert_rows: RefCell::new(vec![("cheap".into(), 20000, "{}".into(), true)]),
            ..Default::default()
        };
        let r = block_on(ingest_and_alert(&fake, &out, &rule, &json!([car("cheap", 20000)]), "T")).unwrap();
        assert_eq!((r.stats.relisted, r.new_alerts), (1, 0));
        assert!(out.sent.borrow().is_empty());

        // Then it drops to 19000: a new key, alerted, delivered and marked.
        let drop = Fake {
            known: known("cheap", 20000.0, false),
            rows: stored_market(&car("cheap", 19000)),
            alert_rows: RefCell::new(fake.alert_rows.borrow().clone()),
            ..Default::default()
        };
        let r = block_on(ingest_and_alert(&drop, &out, &rule, &json!([car("cheap", 19000)]), "T")).unwrap();
        assert_eq!((r.stats.price_drops, r.new_alerts), (1, 1));
        let rows = drop.alert_rows.borrow();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[1].1, rows[1].3), (19000, true));
    }

    #[test]
    fn normal_prices_and_a_disabled_rule_raise_nothing() {
        let out = Outbox::default();
        let fair = car("fair", 24900);
        let fake = Fake { rows: stored_market(&fair), ..Default::default() };
        let r = block_on(ingest_and_alert(&fake, &out, &AlertRule::default(), &json!([fair]), "T")).unwrap();
        assert_eq!((r.stats.added, r.new_alerts), (1, 0));

        let cheap = car("cheap", 20000);
        let fake = Fake { rows: stored_market(&cheap), ..Default::default() };
        let off = AlertRule { enabled: false, ..Default::default() };
        let r = block_on(ingest_and_alert(&fake, &out, &off, &json!([cheap]), "T")).unwrap();
        assert_eq!(r.new_alerts, 0);
        assert_eq!(r.rescored, 11, "alerts off still keeps the dashboard's scores current");
        assert!(out.sent.borrow().is_empty());
    }

    #[test]
    fn a_failing_notifier_keeps_the_ingest_and_the_alert() {
        let cheap = car("cheap", 20000);
        let fake = Fake { rows: stored_market(&cheap), ..Default::default() };
        let out = Outbox { fail: true, ..Default::default() };
        let r = block_on(ingest_and_alert(&fake, &out, &AlertRule::default(), &json!([cheap]), "T")).unwrap();
        assert_eq!(r.stats.added, 1);
        assert_eq!(r.new_alerts, 0);
        assert!(r.alert_error.as_deref().unwrap().contains("smtp down"));
        assert_eq!(fake.alert_rows.borrow().len(), 1, "stored, so it still shows on the dashboard");
        assert!(!fake.applied.borrow().is_empty(), "the listing itself was saved");
    }

    #[test]
    fn alert_list_endpoint_shape() {
        let fake = Fake::default();
        fake.alert_rows.borrow_mut().push(("a".into(), 1, json!({"id": "a", "price": 1}).to_string(), false));
        fake.alert_rows.borrow_mut().push(("b".into(), 2, json!({"id": "b", "price": 2}).to_string(), true));
        let q = AlertsQuery::from_pairs([("limit", "1")]).unwrap();
        let v = block_on(recent_alerts(&fake, &q, &AlertRule::default(), "NOW")).unwrap();
        assert_eq!(v["generatedAt"], "NOW");
        assert_eq!(v["rule"]["minDiscountPct"], 15);
        let list = v["alerts"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!((list[0]["id"].as_str(), list[0]["notifiedAt"].as_str()), (Some("b"), Some("T")), "newest first");
    }

    impl SnapshotStore for Fake {
        async fn save_snapshot(&self, stmts: Vec<Stmt>) -> Result<(), String> {
            self.snapshots.borrow_mut().push(stmts);
            Ok(())
        }
        async fn latest_snapshot(&self) -> Result<Option<Value>, String> {
            Ok(None)
        }
    }

    #[test]
    fn ingest_plans_against_known_rows() {
        let fake = Fake {
            known: HashMap::from([("old".to_string(), Existing { price: Some(100.0), price_changes: 0, removed: true, ..Default::default() })]),
            ..Default::default()
        };
        let body = json!({"listings": [{"id": "new", "price": 5}, {"id": "old", "price": 90}, {"nope": 1}],
                          "scope": {"seenAt": "T"}});
        let stats = block_on(ingest(&fake, &body, "NOW")).unwrap();
        assert_eq!(stats, SaveStats { seen: 2, added: 1, relisted: 1, price_drops: 1, price_rises: 0, skipped: 1, unchanged: 0 });
        let applied = fake.applied.borrow();
        assert_eq!(applied.len(), 4, "insert+history, update+history");
        assert!(applied[2].sql.starts_with("UPDATE"));
    }

    #[test]
    fn ingest_errors_map_to_statuses() {
        let fake = Fake::default();
        assert_eq!(block_on(ingest(&fake, &json!(3), "N")).unwrap_err().status, 400);
        let down = Fake { fail: true, ..Default::default() };
        assert_eq!(block_on(ingest(&down, &json!([{"id": "a"}]), "N")).unwrap_err().status, 500);
        // Nothing to write: no call to apply.
        block_on(ingest(&fake, &json!([]), "N")).unwrap();
        assert!(fake.applied.borrow().is_empty());
    }

    #[test]
    fn a_batch_loads_and_rescores_only_its_own_groups() {
        let mut rows = stored_market(&car("cheap", 20000));
        rows.push(json!({"id": "civic", "source": "autohebdo", "make": "Honda", "model": "Civic", "year": 2018,
                         "km": 50000, "price": 15000, "sellerType": "Dealer", "province": "QC", "isDamaged": false}));
        let fake = Fake { rows, ..Default::default() };
        let r = block_on(ingest_and_alert(&fake, &NoNotifier, &AlertRule::default(), &json!([car("cheap", 20000)]), "T")).unwrap();
        assert_eq!(*fake.group_calls.borrow(), vec![vec![("Toyota".to_string(), "RAV4".to_string())]]);
        assert_eq!(r.rescored, 11, "the ten stored RAV4s and the new one, not the Civic");
        assert_eq!(fake.score_rows.borrow().len(), 11);
        assert!(!fake.score_rows.borrow().contains_key("civic"));
    }

    #[test]
    fn deals_read_stored_scores_filtered_and_ranked() {
        let fake = Fake { rows: stored_market(&car("cheap", 20000)), ..Default::default() };
        block_on(ingest_and_alert(&fake, &NoNotifier, &AlertRule::default(), &json!([car("cheap", 20000)]), "T")).unwrap();
        let calls = *fake.active_calls.borrow();

        let q = DealsQuery { min_comps: Some(6.0), ..Default::default() };
        let v = block_on(deals(&fake, &q, "NOW")).unwrap();
        assert_eq!(*fake.active_calls.borrow(), calls, "reading deals never loads or scores listings");
        assert_eq!(v["generatedAt"], "NOW");
        let list = v["deals"].as_array().unwrap();
        assert_eq!(list[0]["id"], "cheap", "biggest discount first");
        assert!(list[0]["score"]["discountPct"].as_f64().unwrap() >= 15.0);
        assert_eq!(v["matched"], v["scored"]);

        let honda = DealsQuery { make: Some("honda".into()), min_comps: Some(6.0), ..Default::default() };
        assert_eq!(block_on(deals(&fake, &honda, "NOW")).unwrap()["matched"], 0);
        let strict = DealsQuery { min_comps: Some(50.0), ..Default::default() };
        assert_eq!(block_on(deals(&fake, &strict, "NOW")).unwrap()["scored"], 0, "too few comps behind every score");
    }

    #[test]
    fn snapshot_expires_then_stores_the_stored_deals() {
        let fake = Fake { rows: stored_market(&car("cheap", 20000)), ..Default::default() };
        block_on(ingest_and_alert(&fake, &NoNotifier, &AlertRule::default(), &json!([car("cheap", 20000)]), "T")).unwrap();
        let calls = *fake.active_calls.borrow();
        let out = block_on(take_snapshot(&fake, Some(6.0), "T", "CUT", Some("0 11 * * *"))).unwrap();
        assert_eq!(*fake.expiries.borrow(), 1);
        assert_eq!(*fake.active_calls.borrow(), calls, "the cron does not rescore");
        assert_eq!(out["generatedAt"], "T");
        assert!(out["scored"].as_u64().unwrap() > 0);
        let saved = fake.snapshots.borrow();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].len(), 1 + out["deals"].as_array().unwrap().len() + 2);
    }
}
