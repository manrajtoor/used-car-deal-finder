//! GET /api/deals: its query parameters, and the listing shape the scorer
//! reads. The deals themselves are the scores ingest stored (`scores.rs`):
//! re-scoring the whole market per request does not fit the Free plan.

use serde_json::{Map, Value};

use crate::sql::json_num;

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 200;

/// Columns read for scoring and display. Descriptions and image lists are
/// left out: the scorer does not need them and they are the bulk of a row.
/// Callers add the `WHERE` (`scores::group_queries`).
pub const LOAD_SELECT: &str = "SELECT id, source, url, make, model, year, km, price, first_price, trim_text, \
     seller_type, seller_name, city, province, is_damaged, is_parts, is_conditional_price, \
     price_changes, first_seen, last_seen \
     FROM listings";

#[derive(Debug, Clone, PartialEq)]
pub struct DealsQuery {
    pub make: Option<String>,
    pub model: Option<String>,
    /// "Dealer" or "PrivateSeller".
    pub seller_type: Option<String>,
    pub limit: usize,
    /// Scorer `model.minComps` override (the CLI's `--min-comps`).
    pub min_comps: Option<f64>,
}

impl Default for DealsQuery {
    fn default() -> Self {
        DealsQuery { make: None, model: None, seller_type: None, limit: DEFAULT_LIMIT, min_comps: None }
    }
}

impl DealsQuery {
    /// From URL query pairs: `make`, `model`, `seller` (dealer|private), `limit`, `minComps`.
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<DealsQuery, String> {
        let mut q = DealsQuery::default();
        for (k, v) in pairs {
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            match k {
                "make" => q.make = Some(v.to_string()),
                "model" => q.model = Some(v.to_string()),
                "seller" => {
                    q.seller_type = Some(match v.to_ascii_lowercase().as_str() {
                        "dealer" => "Dealer".to_string(),
                        "private" => "PrivateSeller".to_string(),
                        _ => return Err(format!("seller must be dealer or private, not {v:?}")),
                    })
                }
                "limit" => {
                    let n: usize = v.parse().map_err(|_| format!("limit: {v:?} is not a whole number"))?;
                    q.limit = n.clamp(1, MAX_LIMIT);
                }
                "minComps" | "min_comps" => {
                    let n: f64 = v.parse().map_err(|_| format!("minComps: {v:?} is not a number"))?;
                    if !n.is_finite() || n < 1.0 {
                        return Err("minComps must be a number >= 1".into());
                    }
                    q.min_comps = Some(n);
                }
                _ => {}
            }
        }
        Ok(q)
    }
}

/// A D1 row (snake_case columns) back into the crawler's camelCase listing
/// shape, like the Go `LoadListings`. 0/1 flags become real booleans: the
/// scorer tests `isDamaged === true` strictly.
pub fn listing_from_row(row: &Value) -> Value {
    let mut m = Map::new();
    let text = |k: &str| row.get(k).filter(|v| v.is_string()).cloned().unwrap_or(Value::Null);
    let num = |k: &str| row.get(k).and_then(Value::as_f64).map_or(Value::Null, json_num);
    let flag = |k: &str| Value::Bool(row.get(k).and_then(Value::as_f64) == Some(1.0));
    for (out, col) in [("id", "id"), ("source", "source"), ("url", "url"), ("make", "make"), ("model", "model")] {
        m.insert(out.into(), text(col));
    }
    m.insert("year".into(), num("year"));
    m.insert("km".into(), num("km"));
    m.insert("price".into(), num("price"));
    m.insert("trimText".into(), text("trim_text"));
    m.insert("sellerType".into(), text("seller_type"));
    m.insert("sellerName".into(), text("seller_name"));
    m.insert("city".into(), text("city"));
    m.insert("province".into(), text("province"));
    m.insert("isDamaged".into(), flag("is_damaged"));
    m.insert("isParts".into(), flag("is_parts"));
    m.insert("isConditionalPrice".into(), flag("is_conditional_price"));
    m.insert("firstPrice".into(), num("first_price"));
    m.insert("priceChanges".into(), num("price_changes"));
    m.insert("firstSeen".into(), text("first_seen"));
    m.insert("lastSeen".into(), text("last_seen"));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn query_params() {
        let q = DealsQuery::from_pairs([("make", "toyota"), ("limit", "500"), ("minComps", "3"), ("seller", "Private"), ("x", "y")])
            .unwrap();
        assert_eq!(q.make.as_deref(), Some("toyota"));
        assert_eq!(q.limit, MAX_LIMIT);
        assert_eq!(q.min_comps, Some(3.0));
        assert_eq!(q.seller_type.as_deref(), Some("PrivateSeller"));
        assert!(DealsQuery::from_pairs([("limit", "ten")]).is_err());
        assert!(DealsQuery::from_pairs([("minComps", "0")]).is_err());
        assert!(DealsQuery::from_pairs([("seller", "robot")]).is_err());
        assert_eq!(DealsQuery::from_pairs([("make", " ")]).unwrap(), DealsQuery::default());
    }

    #[test]
    fn rows_become_listings() {
        let row = json!({"id": "a", "source": "autohebdo", "year": 2019.0, "price": 24000.0, "km": null,
                         "is_damaged": 1.0, "is_parts": 0.0, "trim_text": "XLE", "seller_type": "Dealer"});
        let l = listing_from_row(&row);
        assert_eq!(l["year"].to_string(), "2019");
        assert_eq!(l["isDamaged"], true);
        assert_eq!(l["isParts"], false);
        assert_eq!(l["isConditionalPrice"], false);
        assert_eq!(l["trimText"], "XLE");
        assert_eq!(l["km"], Value::Null);
    }
}
