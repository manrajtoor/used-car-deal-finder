-- carbuyer D1 schema, part 4: stored scores (Worker only, not in the Go store).
--
-- One row per priced active listing, written by POST /api/listings for the
-- make/model groups a batch touched (cloudflare/worker/src/scores.rs), so
-- /api/deals and the daily snapshot read scores instead of re-scoring every
-- car, which does not fit the Free plan's CPU time per request.

CREATE TABLE IF NOT EXISTS listing_scores (
  listing_id   TEXT PRIMARY KEY,
  make         TEXT,
  model        TEXT,
  seller_type  TEXT,
  discount_pct REAL NOT NULL,
  comps        INTEGER,
  scored_at    TEXT NOT NULL,
  -- the /api/deals item: the listing plus its "score"
  deal         TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS listing_scores_discount ON listing_scores (discount_pct DESC);

-- Loading a make/model group uses idx_listings_model, the expiry sweep
-- idx_listings_active (both in 0001); listings itself stays as the Go store has it.
