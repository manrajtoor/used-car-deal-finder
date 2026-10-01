//! Stored scores (`listing_scores`, migration 0004).
//!
//! Scoring every active listing stops fitting the Free plan's ~10 ms of CPU
//! per request (the cron gets the same) once a market holds a few thousand
//! cars. So an ingest scores only the make/model groups its fresh listings
//! belong to, against every active car of those groups, and stores one row
//! per listing. `/api/deals` and the daily snapshot read those rows back and
//! never run the scorer.
//!
//! A group is rescored whenever one of its cars is new, relisted or cheaper,
//! so its stored scores are as fresh as its last change. Cars not seen for a
//! while are expired by the cron (`expire_statements`), which is what retires
//! sold cars: the crawler's own removal check never reaches D1.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::deals::{DealsQuery, LOAD_SELECT};
use crate::sql::{Param, Stmt};

/// A (make, model) pair: everything the scorer compares a car with.
pub type Group = (String, String);

/// Groups per load query: two parameters each, under D1's 100-parameter limit.
const GROUPS_PER_QUERY: usize = 45;

/// One listing (API shape) and its score, `None` when it cannot be priced.
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub listing: Value,
    pub score: Option<Value>,
}

fn text<'a>(l: &'a Value, k: &str) -> Option<&'a str> {
    l.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The distinct groups of the `fresh` ids among `listings` (an ingest's
/// payload, crawler shape). A car without make and model cannot be priced.
pub fn fresh_groups(listings: &[Value], fresh: &[String]) -> Vec<Group> {
    let wanted: BTreeSet<&str> = fresh.iter().map(String::as_str).collect();
    listings
        .iter()
        .filter(|l| text(l, "id").is_some_and(|id| wanted.contains(id)))
        .filter_map(|l| Some((text(l, "make")?.to_string(), text(l, "model")?.to_string())))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Active listings of `groups`, in insertion order within each query, each
/// with `has_score` (1 when `listing_scores` already holds a row for it).
/// Written as `(make = ? AND model = ?) OR ...` with `+removed_at` so SQLite
/// answers each term from idx_listings_model (a MULTI-INDEX OR); otherwise
/// it walked every active row through idx_listings_active, and the earlier
/// `(make, model) IN (VALUES ...)` scanned the table (~2 600 rows per call).
pub fn group_queries(groups: &[Group]) -> Vec<Stmt> {
    groups
        .chunks(GROUPS_PER_QUERY)
        .map(|chunk| {
            let terms: Vec<String> =
                (0..chunk.len()).map(|i| format!("(make = ?{} AND model = ?{})", 2 * i + 1, 2 * i + 2)).collect();
            Stmt::new(
                format!(
                    "{} FROM listings WHERE +removed_at IS NULL AND ({}) ORDER BY rowid",
                    LOAD_SELECT.trim_end_matches(" FROM listings").to_string()
                        + ", EXISTS (SELECT 1 FROM listing_scores s WHERE s.listing_id = listings.id) AS has_score",
                    terms.join(" OR ")
                ),
                chunk.iter().flat_map(|(mk, md)| [Param::Text(mk.clone()), Param::Text(md.clone())]).collect(),
            )
        })
        .collect()
}

/// Scores `listings` (API shape, whole groups) with `minComps = min_comps`.
pub fn score(listings: &[Value], min_comps: f64) -> Result<Vec<Scored>, String> {
    if listings.is_empty() {
        return Ok(Vec::new());
    }
    let out = scorer::score::run(&json!({ "listings": listings, "options": { "model": { "minComps": min_comps } } }))?;
    let scores = out.get("scores").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(listings
        .iter()
        .zip(scores)
        .map(|(l, s)| Scored { listing: l.clone(), score: s.get("score").filter(|v| v.is_object()).cloned() })
        .collect())
}

/// Score writes for one rescored batch of groups. Writing every car of a big
/// group on each ingest costs more CPU than the scoring itself, so only these
/// are written: the `fresh` cars (stored, or dropped when they no longer
/// price), and cars that have no stored score yet (`has_score` false), which
/// backfills a group over time. The rest keep the score they were given.
pub fn upsert_statements(scored: &[Scored], fresh: &[String], has_score: &BTreeSet<String>, now: &str) -> Vec<Stmt> {
    let fresh: BTreeSet<&str> = fresh.iter().map(String::as_str).collect();
    scored
        .iter()
        .filter_map(|s| {
            let id = text(&s.listing, "id")?.to_string();
            let is_fresh = fresh.contains(id.as_str());
            let stored = has_score.contains(&id);
            Some(match &s.score {
                None if is_fresh && stored => {
                    Stmt::new("DELETE FROM listing_scores WHERE listing_id = ?1", vec![Param::Text(id)])
                }
                None => return None,
                Some(_) if !is_fresh && stored => return None,
                Some(score) => {
                    let l = &s.listing;
                    let mut deal = l.clone();
                    deal["score"] = score.clone();
                    let n = |k: &str| score.get(k).and_then(Value::as_f64);
                    Stmt::new(
                        "INSERT OR REPLACE INTO listing_scores \
                         (listing_id, make, model, seller_type, discount_pct, comps, scored_at, deal) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        vec![
                            Param::Text(id),
                            Param::text(text(l, "make")),
                            Param::text(text(l, "model")),
                            Param::text(text(l, "sellerType")),
                            // Not Param::num: a percentage must stay REAL even when whole.
                            Param::Real(n("discountPct").unwrap_or(0.0)),
                            Param::num(n("n")),
                            Param::Text(now.to_string()),
                            Param::Text(deal.to_string()),
                        ],
                    )
                }
            })
        })
        .collect()
}

/// Lowercase letters and digits only, the way `/api/deals` compares names:
/// `rav4` finds "RAV4" and `mercedes benz` finds "Mercedes-Benz".
pub fn name_key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The same folding in SQL, for the separators that occur in make and model names.
fn sql_key(col: &str) -> String {
    format!("lower(replace(replace(replace(replace({col}, '-', ''), ' ', ''), '.', ''), '/', ''))")
}

/// `WHERE` clause and parameters shared by the deals list and its count.
/// `?1` is the minimum comp count.
fn filter(q: &DealsQuery, min_comps: f64) -> (String, Vec<Param>) {
    let mut sql = String::from(
        "FROM listing_scores s JOIN listings l ON l.id = s.listing_id WHERE l.removed_at IS NULL AND s.comps >= ?1",
    );
    let mut params = vec![Param::Real(min_comps)];
    for (want, col) in [(&q.make, "s.make"), (&q.model, "s.model")] {
        if let Some(w) = want {
            params.push(Param::Text(name_key(w)));
            sql.push_str(&format!(" AND {} = ?{}", sql_key(col), params.len()));
        }
    }
    if let Some(st) = &q.seller_type {
        params.push(Param::Text(st.clone()));
        sql.push_str(&format!(" AND s.seller_type = ?{}", params.len()));
    }
    (sql, params)
}

/// The best stored deals for `q`, best discount first.
pub fn deals_query(q: &DealsQuery, min_comps: f64) -> Stmt {
    let (from, mut params) = filter(q, min_comps);
    params.push(Param::Int(q.limit as i64));
    let n = params.len();
    Stmt::new(format!("SELECT s.deal {from} ORDER BY s.discount_pct DESC, s.rowid LIMIT ?{n}"), params)
}

/// `{comps, scored, matched}` for the same query, as `/api/deals` reports them.
pub fn counts_query(q: &DealsQuery, min_comps: f64) -> Stmt {
    let (from, params) = filter(q, min_comps);
    let unfiltered = DealsQuery { limit: q.limit, min_comps: q.min_comps, ..Default::default() };
    let (all, _) = filter(&unfiltered, min_comps);
    Stmt::new(
        format!(
            "SELECT (SELECT COUNT(*) FROM listings WHERE removed_at IS NULL) AS comps, \
             (SELECT COUNT(*) {all}) AS scored, (SELECT COUNT(*) {from}) AS matched"
        ),
        params,
    )
}

/// The `/api/deals` shape from a counts row and deal rows.
pub fn assemble(counts: &Value, deal_rows: &[Value]) -> Value {
    let n = |k: &str| counts.get(k).and_then(Value::as_f64).map_or(Value::from(0), |f| Value::from(f as i64));
    let deals: Vec<Value> = deal_rows
        .iter()
        .filter_map(|r| r.get("deal").and_then(Value::as_str).and_then(|s| serde_json::from_str(s).ok()))
        .collect();
    json!({ "comps": n("comps"), "scored": n("scored"), "matched": n("matched"), "appraiser": Value::Null, "deals": deals })
}

/// Retire cars not seen since `cutoff` (ISO time) and drop their scores. A
/// car that comes back is relisted by the next ingest and rescored.
pub fn expire_statements(now: &str, cutoff: &str) -> Vec<Stmt> {
    vec![
        Stmt::new(
            "UPDATE listings SET removed_at = ?1 WHERE removed_at IS NULL AND last_seen < ?2",
            vec![Param::Text(now.to_string()), Param::Text(cutoff.to_string())],
        ),
        Stmt::new(
            "DELETE FROM listing_scores WHERE listing_id IN (SELECT id FROM listings WHERE removed_at IS NOT NULL)",
            vec![],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn car(id: &str, make: &str, model: &str, price: i64) -> Value {
        json!({"id": id, "source": "craigslist", "make": make, "model": model, "year": 2018, "km": 90000,
               "price": price, "sellerType": "Dealer", "province": "NY", "isDamaged": false})
    }

    #[test]
    fn groups_come_only_from_fresh_priceable_cars() {
        let batch = vec![
            car("a", "Honda", "Civic", 1),
            car("b", "Honda", "Civic", 1),
            car("c", "Toyota", "RAV4", 1),
            json!({"id": "d", "make": "Ford"}),
            car("e", "Kia", "Soul", 1),
        ];
        let fresh = vec!["a".to_string(), "b".into(), "c".into(), "d".into()];
        assert_eq!(fresh_groups(&batch, &fresh), vec![("Honda".into(), "Civic".into()), ("Toyota".into(), "RAV4".into())]);
    }

    #[test]
    fn group_queries_chunk_under_the_parameter_limit() {
        let groups: Vec<Group> = (0..100).map(|i| ("M".to_string(), format!("m{i}"))).collect();
        let qs = group_queries(&groups);
        assert_eq!(qs.len(), 3);
        assert_eq!(qs[0].params.len(), 90);
        assert!(qs[0].sql.contains("WHERE +removed_at IS NULL AND ((make = ?1 AND model = ?2) OR (make = ?3 AND model = ?4) OR"), "{}", qs[0].sql);
        assert!(qs[2].sql.ends_with("(make = ?19 AND model = ?20)) ORDER BY rowid"), "{}", qs[2].sql);
        assert!(qs[0].sql.starts_with("SELECT id, source") && qs[0].sql.contains("AS has_score FROM listings WHERE"), "{}", qs[0].sql);
    }

    #[test]
    fn scores_are_stored_or_dropped() {
        let mut group: Vec<Value> = (0..12).map(|i| car(&format!("d{i}"), "Honda", "Civic", 15000 + (i % 3) * 300)).collect();
        group.push(car("cheap", "Honda", "Civic", 11000));
        group.push(json!({"id": "junk", "make": "Honda", "model": "Civic", "price": 500}));
        let scored = score(&group, 6.0).unwrap();
        assert_eq!(scored.len(), group.len());
        let cheap = scored.iter().find(|s| s.listing["id"] == "cheap").unwrap();
        assert!(cheap.score.as_ref().unwrap()["discountPct"].as_f64().unwrap() > 15.0);

        let fresh = vec!["cheap".to_string(), "junk".into(), "d0".into()];
        let has: BTreeSet<String> = ["junk", "d0", "d1"].iter().map(|s| s.to_string()).collect();
        let st = upsert_statements(&scored, &fresh, &has, "T");
        let ids: Vec<&str> = st.iter().map(|s| s.params[0].as_str().unwrap()).collect();
        assert!(ids.contains(&"d0"), "a fresh car is rewritten");
        assert!(!ids.contains(&"d1"), "a stored, unchanged car keeps its score");
        assert!(ids.contains(&"d2"), "a car without a stored score is backfilled");
        let junk = st.iter().find(|s| s.params[0] == Param::Text("junk".into())).unwrap();
        assert!(junk.sql.starts_with("DELETE"), "a fresh car that no longer prices loses its old score");
        let none: BTreeSet<String> = BTreeSet::new();
        assert!(upsert_statements(&scored, &[], &none, "T").iter().all(|s| !s.sql.starts_with("DELETE")),
                "nothing stored, nothing to delete");
        let row = st.iter().find(|s| s.params[0] == Param::Text("cheap".into())).unwrap();
        assert!(row.sql.starts_with("INSERT OR REPLACE INTO listing_scores"));
        assert!(matches!(row.params[4], Param::Real(p) if p > 15.0));
        let deal: Value = serde_json::from_str(row.params[7].as_str().unwrap()).unwrap();
        assert_eq!(deal["id"], "cheap");
        assert!(deal["score"]["baseline"].as_f64().is_some(), "the stored deal is the /api/deals item");
        assert!(score(&[], 6.0).unwrap().is_empty());
    }

    #[test]
    fn deals_filters_fold_names_and_number_their_parameters() {
        let q = DealsQuery {
            make: Some("mercedes benz".into()),
            model: None,
            seller_type: Some("Dealer".into()),
            limit: 5,
            min_comps: None,
        };
        let s = deals_query(&q, 8.0);
        assert!(s.sql.contains("s.comps >= ?1"));
        assert!(s.sql.contains("'-', '')") && s.sql.contains("= ?2") && s.sql.contains("s.seller_type = ?3"));
        assert!(s.sql.ends_with("LIMIT ?4"));
        assert_eq!(s.params, vec![Param::Real(8.0), Param::Text("mercedesbenz".into()), Param::Text("Dealer".into()), Param::Int(5)]);
        let c = counts_query(&q, 8.0);
        assert_eq!(c.params.len(), 3, "counts share the filter parameters, not the limit");
        assert_eq!(name_key("Mercedes-Benz"), name_key("mercedes benz"));
    }

    #[test]
    fn assemble_and_expire() {
        let v = assemble(&json!({"comps": 30.0, "scored": 12.0, "matched": 2.0}), &[json!({"deal": "{\"id\":\"a\"}"})]);
        assert_eq!((v["comps"].clone(), v["scored"].clone(), v["matched"].clone()), (json!(30), json!(12), json!(2)));
        assert_eq!(v["deals"][0]["id"], "a");
        let e = expire_statements("NOW", "CUT");
        assert_eq!(e[0].params, vec![Param::Text("NOW".into()), Param::Text("CUT".into())]);
        assert!(e[1].sql.starts_with("DELETE FROM listing_scores"));
    }
}
