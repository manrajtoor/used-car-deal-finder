//! Cloudflare D1 implementation of the storage ports. The only module that
//! knows about D1; everything it runs was planned elsewhere as `Stmt`s.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::Value;
use worker::wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement};

use crate::alerts;
use crate::deals::{listing_from_row, DealsQuery};
use crate::ingest::{described_queries, existing_from_row, existing_queries, Existing};
use crate::scores::{self, Group};
use crate::service::{AlertStore, ListingStore, ScoreStore, SnapshotStore};
use crate::snapshot::{assemble, ITEMS_SQL, LATEST_HEADER_SQL};
use crate::sql::{Param, Stmt};

pub struct D1Store {
    db: D1Database,
}

impl D1Store {
    pub fn new(db: D1Database) -> D1Store {
        D1Store { db }
    }

    fn prepare(&self, s: &Stmt) -> Result<D1PreparedStatement, String> {
        let args: Vec<JsValue> = s
            .params
            .iter()
            .map(|p| match p {
                Param::Null => JsValue::null(),
                Param::Int(i) => JsValue::from_f64(*i as f64),
                Param::Real(f) => JsValue::from_f64(*f),
                Param::Text(t) => JsValue::from_str(t),
            })
            .collect();
        self.db.prepare(s.sql.as_str()).bind(&args).map_err(err)
    }

    async fn rows(&self, s: &Stmt) -> Result<Vec<Value>, String> {
        self.prepare(s)?.all().await.map_err(err)?.results::<Value>().map_err(err)
    }

    /// A listing's stored photo links (JSON array in `image_urls`), for the
    /// photo check. Kept out of `LOAD_SQL`: the scorer never needs them.
    pub async fn image_urls(&self, id: &str) -> Result<Vec<String>, String> {
        let q = Stmt::new("SELECT image_urls FROM listings WHERE id = ?1", vec![Param::Text(id.to_string())]);
        let raw = self.rows(&q).await?.into_iter().next().and_then(|r| r.get("image_urls")?.as_str().map(str::to_string));
        Ok(raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or_default())
    }

    async fn batch(&self, stmts: Vec<Stmt>) -> Result<(), String> {
        let prepared = stmts.iter().map(|s| self.prepare(s)).collect::<Result<Vec<_>, _>>()?;
        // A D1 batch runs as one transaction: all of it lands or none does.
        for r in self.db.batch(prepared).await.map_err(err)? {
            if let Some(e) = r.error() {
                return Err(e);
            }
        }
        Ok(())
    }
}

fn err(e: worker::Error) -> String {
    e.to_string()
}

impl ListingStore for D1Store {
    async fn known(&self, ids: &[String]) -> Result<HashMap<String, Existing>, String> {
        let mut out = HashMap::new();
        for q in existing_queries(ids) {
            out.extend(self.rows(&q).await?.iter().filter_map(existing_from_row));
        }
        Ok(out)
    }

    async fn apply(&self, stmts: Vec<Stmt>) -> Result<(), String> {
        self.batch(stmts).await
    }

    async fn described(&self, ids: &[String]) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for q in described_queries(ids) {
            out.extend(self.rows(&q).await?.iter().filter_map(|r| r.get("id")?.as_str().map(str::to_string)));
        }
        Ok(out)
    }

    async fn in_groups(&self, groups: &[Group]) -> Result<(Vec<Value>, BTreeSet<String>), String> {
        let mut out = Vec::new();
        let mut has_score = BTreeSet::new();
        for q in scores::group_queries(groups) {
            for row in self.rows(&q).await? {
                if row.get("has_score").and_then(Value::as_f64) == Some(1.0) {
                    if let Some(id) = row.get("id").and_then(Value::as_str) {
                        has_score.insert(id.to_string());
                    }
                }
                out.push(listing_from_row(&row));
            }
        }
        Ok((out, has_score))
    }
}

impl ScoreStore for D1Store {
    async fn save_scores(&self, stmts: Vec<Stmt>) -> Result<(), String> {
        if stmts.is_empty() {
            return Ok(());
        }
        self.batch(stmts).await
    }

    async fn stored_deals(&self, q: &DealsQuery, min_comps: f64) -> Result<Value, String> {
        let counts = self.rows(&scores::counts_query(q, min_comps)).await?.into_iter().next().unwrap_or(Value::Null);
        let rows = self.rows(&scores::deals_query(q, min_comps)).await?;
        Ok(scores::assemble(&counts, &rows))
    }
}

impl SnapshotStore for D1Store {
    async fn save_snapshot(&self, stmts: Vec<Stmt>) -> Result<(), String> {
        self.batch(stmts).await
    }

    async fn latest_snapshot(&self) -> Result<Option<Value>, String> {
        let Some(header) = self.rows(&Stmt::new(LATEST_HEADER_SQL, vec![])).await?.into_iter().next() else {
            return Ok(None);
        };
        let id = Param::num(header.get("id").and_then(Value::as_f64));
        let items = self.rows(&Stmt::new(ITEMS_SQL, vec![id])).await?;
        Ok(Some(assemble(&header, &items)))
    }
}

impl AlertStore for D1Store {
    async fn alerted(&self, ids: &[String]) -> Result<HashSet<(String, i64)>, String> {
        let mut out = HashSet::new();
        for q in alerts::existing_queries(ids) {
            out.extend(self.rows(&q).await?.iter().filter_map(alerts::key_from_row));
        }
        Ok(out)
    }

    async fn save_alerts(&self, stmts: Vec<Stmt>) -> Result<(), String> {
        self.batch(stmts).await
    }

    async fn recent_alerts(&self, limit: usize) -> Result<Vec<Value>, String> {
        let q = Stmt::new(alerts::LIST_SQL, vec![Param::Int(limit as i64)]);
        Ok(self.rows(&q).await?.iter().map(alerts::from_row).collect())
    }
}
