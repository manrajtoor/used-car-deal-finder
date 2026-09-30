package apistore

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"

	"carbuyer/crawler/internal/listing"
	"carbuyer/crawler/internal/store"
)

const token = "t0ken-for-tests"

// fakeWorker mimics POST /api/listings: checks the bearer token and answers
// with stats computed from the body.
type fakeWorker struct {
	mu     sync.Mutex
	bodies []wireBody
	status int // non-zero forces this status
}

func (f *fakeWorker) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost || r.URL.Path != IngestPath {
		http.Error(w, `{"error":"not found"}`, http.StatusNotFound)
		return
	}
	if r.Header.Get("Authorization") != "Bearer "+token {
		w.WriteHeader(http.StatusUnauthorized)
		io.WriteString(w, `{"error":"missing or wrong bearer token"}`)
		return
	}
	if f.status != 0 {
		w.WriteHeader(f.status)
		io.WriteString(w, "<!DOCTYPE html><html>boom</html>")
		return
	}
	if ct := r.Header.Get("Content-Type"); ct != "application/json" {
		http.Error(w, "content-type "+ct, http.StatusBadRequest)
		return
	}
	var b wireBody
	if err := json.NewDecoder(r.Body).Decode(&b); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	f.mu.Lock()
	f.bodies = append(f.bodies, b)
	f.mu.Unlock()
	json.NewEncoder(w).Encode(Stats{Seen: len(b.Listings), Added: len(b.Listings), PriceDrops: 1, NewAlerts: 1})
}

func listings(n int) []listing.Listing {
	out := make([]listing.Listing, n)
	for i := range out {
		out[i] = listing.Listing{ID: listing.Str(string(rune('a' + i))), Source: "autohebdo", Price: listing.Num(20000 + float64(i))}
	}
	return out
}

func TestPushesInBatchesWithTokenAndScope(t *testing.T) {
	fw := &fakeWorker{}
	srv := httptest.NewServer(fw)
	defer srv.Close()

	c, err := New(srv.URL, token)
	if err != nil {
		t.Fatal(err)
	}
	c.BatchSize = 2
	scope := store.Scope{MakeSlug: listing.Str("toyota"), ModelSlug: listing.Str("rav4"), GeoSlug: listing.Str("reg_qc"), SeenAt: "2026-09-23T00:00:00.000Z"}
	stats, err := c.Save(context.Background(), listings(5), scope)
	if err != nil {
		t.Fatal(err)
	}
	if stats.Seen != 5 || stats.Added != 5 || stats.PriceDrops != 3 {
		t.Errorf("stats = %+v", stats)
	}
	if len(fw.bodies) != 3 || len(fw.bodies[2].Listings) != 1 {
		t.Fatalf("batches = %d", len(fw.bodies))
	}
	b := fw.bodies[0]
	if *b.Scope.MakeSlug != "toyota" || *b.Scope.GeoSlug != "reg_qc" || b.Scope.SeenAt != scope.SeenAt {
		t.Errorf("scope = %+v", b.Scope)
	}
	if *b.Listings[1].ID != "b" || *b.Listings[1].Price != 20001 {
		t.Errorf("listing = %+v", b.Listings[1])
	}
	// Totals accumulate across Save calls.
	c.Save(context.Background(), listings(1), scope)
	if c.Totals.Seen != 6 || c.Totals.NewAlerts != c.Totals.PriceDrops || c.Totals.NewAlerts == 0 {
		t.Errorf("totals = %+v", c.Totals)
	}
}

func TestErrorsNeverLeakTheToken(t *testing.T) {
	fw := &fakeWorker{}
	srv := httptest.NewServer(fw)
	defer srv.Close()

	wrong, _ := New(srv.URL, "not-the-token")
	_, err := wrong.Save(context.Background(), listings(1), store.Scope{})
	if err == nil || !strings.Contains(err.Error(), "HTTP 401") || !strings.Contains(err.Error(), "wrong bearer token") {
		t.Fatalf("err = %v", err)
	}
	if strings.Contains(err.Error(), "not-the-token") {
		t.Errorf("error leaks the token: %v", err)
	}

	fw.status = http.StatusForbidden
	c, _ := New(srv.URL, token)
	c.BatchSize = 1
	stats, err := c.Save(context.Background(), listings(3), store.Scope{})
	if err == nil || !strings.Contains(err.Error(), "Cloudflare Access") || !strings.Contains(err.Error(), "listings 1-1 of 3") {
		t.Errorf("err = %v", err)
	}
	if stats.Seen != 0 {
		t.Errorf("stats = %+v", stats)
	}
}

func TestEmptySaveSendsNothing(t *testing.T) {
	fw := &fakeWorker{}
	srv := httptest.NewServer(fw)
	defer srv.Close()
	c, _ := New(srv.URL, token)
	if _, err := c.Save(context.Background(), nil, store.Scope{}); err != nil || len(fw.bodies) != 0 {
		t.Errorf("err=%v bodies=%d", err, len(fw.bodies))
	}
}

func TestCancelledContext(t *testing.T) {
	srv := httptest.NewServer(&fakeWorker{})
	defer srv.Close()
	c, _ := New(srv.URL, token)
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := c.Save(ctx, listings(1), store.Scope{}); err == nil {
		t.Error("want an error for a cancelled context")
	}
}

func TestNewValidates(t *testing.T) {
	cases := []struct {
		url, token, endpoint string
		ok                   bool
	}{
		{"https://carbuyer-api.me.workers.dev", "t", "https://carbuyer-api.me.workers.dev/api/listings", true},
		{"https://carbuyer-api.me.workers.dev/", "t", "https://carbuyer-api.me.workers.dev/api/listings", true},
		{"https://x.dev/api/listings?a=1", "t", "https://x.dev/api/listings", true},
		{"http://127.0.0.1:8787", "t", "http://127.0.0.1:8787/api/listings", true},
		{"http://localhost:8787", "t", "http://localhost:8787/api/listings", true},
		{"http://example.com", "t", "", false}, // token in clear text
		{"ftp://example.com", "t", "", false},
		{"not a url", "t", "", false},
		{"https://x.dev", "  ", "", false},
	}
	for _, c := range cases {
		cl, err := New(c.url, c.token)
		if (err == nil) != c.ok {
			t.Errorf("New(%q) err = %v", c.url, err)
			continue
		}
		if c.ok && cl.Endpoint != c.endpoint {
			t.Errorf("New(%q).Endpoint = %q, want %q", c.url, cl.Endpoint, c.endpoint)
		}
	}
}

func TestDescribedAsksTheWorkerInChunks(t *testing.T) {
	var calls []int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/api/listings/described" || r.Method != http.MethodPost || r.Header.Get("Authorization") != "Bearer tok" {
			http.Error(w, "wrong request", http.StatusBadRequest)
			return
		}
		var body struct{ IDs []string }
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		calls = append(calls, len(body.IDs))
		// Everything ending in 7 has a description.
		var got []string
		for _, id := range body.IDs {
			if strings.HasSuffix(id, "7") {
				got = append(got, id)
			}
		}
		json.NewEncoder(w).Encode(map[string]any{"described": got})
	}))
	defer srv.Close()
	c, err := New(srv.URL, "tok")
	if err != nil {
		t.Fatal(err)
	}
	ids := make([]string, 1500)
	for i := range ids {
		ids[i] = fmt.Sprintf("craigslist:%d", i)
	}
	got, err := c.Described(context.Background(), ids)
	if err != nil {
		t.Fatal(err)
	}
	if len(calls) != 2 || calls[0] != MaxDescribedIDs || calls[1] != 500 {
		t.Errorf("calls = %v, want [1000 500]", calls)
	}
	if len(got) != 150 || !got["craigslist:7"] || got["craigslist:8"] {
		t.Errorf("got %d ids", len(got))
	}
}

func TestDescribedErrorNeverLeaksTheToken(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		http.Error(w, "boom", http.StatusInternalServerError)
	}))
	defer srv.Close()
	c, _ := New(srv.URL, "s3cret-token")
	_, err := c.Described(context.Background(), []string{"a"})
	if err == nil || strings.Contains(err.Error(), "s3cret-token") || !strings.Contains(err.Error(), "500") {
		t.Errorf("err = %v", err)
	}
}
