package facebook

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"carbuyer/crawler/internal/listing"
)

// Shaped like the actor's basic (no details) results, from a 2026-10-01 run;
// ids, places and prices are made up.
func basicItem(id, title, price, subtitle, city, state string) map[string]any {
	return map[string]any{
		"id":                                     id,
		"marketplace_listing_title":              title,
		"listing_price":                          map[string]any{"amount": price, "formatted_amount": "$" + price},
		"custom_sub_titles_with_rendering_flags": []any{map[string]any{"subtitle": subtitle}},
		"location":                               map[string]any{"reverse_geocode": map[string]any{"city": city, "state": state}},
		"creation_time":                          float64(1790800000),
		"listingUrl":                             "https://www.facebook.com/marketplace/item/" + id + "/",
	}
}

type fakeActor struct {
	monthly float64
	items   []map[string]any
	input   ActorInput
	runs    int
	auth    []string
}

func (f *fakeActor) server(t *testing.T) *httptest.Server {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		f.auth = append(f.auth, r.Header.Get("Authorization"))
		switch {
		case r.URL.Path == "/users/me/limits":
			json.NewEncoder(w).Encode(map[string]any{"data": map[string]any{"current": map[string]any{"monthlyUsageUsd": f.monthly}}})
		case strings.HasPrefix(r.URL.Path, "/acts/") && r.Method == http.MethodPost:
			f.runs++
			json.NewDecoder(r.Body).Decode(&f.input)
			if r.URL.Query().Get("maxTotalChargeUsd") == "" {
				t.Error("run started without a server-side charge cap")
			}
			w.WriteHeader(http.StatusCreated)
			w.Write([]byte(`{"data":{"id":"run1","status":"READY"}}`))
		case strings.HasSuffix(r.URL.Path, "/dataset/items"):
			json.NewEncoder(w).Encode(f.items)
		case strings.HasPrefix(r.URL.Path, "/actor-runs/"):
			w.Write([]byte(`{"data":{"status":"SUCCEEDED","usageTotalUsd":0.012}}`))
		default:
			http.NotFound(w, r)
		}
	}))
	t.Cleanup(srv.Close)
	return srv
}

func TestApifySourceReadsTheNewestTristateCars(t *testing.T) {
	f := &fakeActor{monthly: 1.20, items: []map[string]any{
		basicItem("1", "2018 Nissan Rogue", "9065.00", "100K miles", "Nesconset", "NY"),
		basicItem("2", "2014 Nissan JUKE", "5498.00", "120K miles · Dealership", "Irvington", "NJ"),
		basicItem("3", "2018 Nissan Maxima", "9999", "116K miles", "Eddington", "PA"),
	}}
	srv := f.server(t)
	s := &ApifySource{Token: "tok", BaseURL: srv.URL}
	q := listing.Query{Geo: "tristate", PriceFrom: listing.Num(1000), PriceTo: listing.Num(30000), MaxPages: 1}
	res, err := s.Search(context.Background(), q, nil)
	if err != nil {
		t.Fatal(err)
	}
	if f.runs != 1 || len(res.Listings) != 2 {
		t.Fatalf("runs=%d listings=%d, want 1 run and the two tri-state cars", f.runs, len(res.Listings))
	}
	in := f.input
	if in.GetListingDetails || in.GetAllListingPhotos || in.MaxPagesPerURL != 1 || in.Proxy.ApifyProxyCountry != "US" {
		t.Errorf("input = %+v: want basic results, one page, a US exit", in)
	}
	if u := in.URLs[0]; !strings.Contains(u, "/marketplace/nyc/cars") || !strings.Contains(u, "daysSinceListed=1") ||
		!strings.Contains(u, "minPrice=1000") || !strings.Contains(u, "maxPrice=30000") {
		t.Errorf("url = %s", u)
	}
	rogue, juke := res.Listings[0], res.Listings[1]
	if *rogue.Km != 160934 || *rogue.SellerType != "PrivateSeller" || *rogue.Province != "NY" || rogue.Source != "marketplace" {
		t.Errorf("rogue = km %v %s %s %s", *rogue.Km, *rogue.SellerType, *rogue.Province, rogue.Source)
	}
	if *juke.Km != 193121 || *juke.SellerType != "Dealer" {
		t.Errorf("juke = km %v %s: the Dealership tag and miles", *juke.Km, *juke.SellerType)
	}
	for _, a := range f.auth {
		if a != "Bearer tok" {
			t.Errorf("request without the bearer token: %q", a)
		}
	}
}

func TestApifyInputLeavesOutUnsetLocation(t *testing.T) {
	b, _ := json.Marshal(ActorInput{URLs: []string{"u"}, SortBy: "creation_time_descend"})
	for _, k := range []string{"radiusKm", "latitude", "longitude", "location"} {
		if strings.Contains(string(b), `"`+k+`"`) {
			t.Errorf("%s sent as a zero value (the actor rejects radiusKm 0): %s", k, b)
		}
	}
}

func TestApifySourceStopsAtTheMonthlyBudget(t *testing.T) {
	f := &fakeActor{monthly: 4.47}
	srv := f.server(t)
	s := &ApifySource{Token: "tok", BaseURL: srv.URL}
	res, err := s.Search(context.Background(), listing.Query{Geo: "tristate", MaxPages: 1}, nil)
	if err != nil || f.runs != 0 || len(res.Listings) != 0 {
		t.Errorf("err=%v runs=%d listings=%d: $4.47 + a $0.05 run is over $4.50, nothing may start", err, f.runs, len(res.Listings))
	}
}

func TestApifySourceNeedsAToken(t *testing.T) {
	if _, err := (&ApifySource{}).Search(context.Background(), listing.Query{Geo: "tristate"}, nil); err == nil {
		t.Error("want an error without APIFY_TOKEN")
	}
}
