package facebook

// Marketplace through Apify, as a source.Source. Marketplace refuses cloud
// IPs (GitHub's runners are blocked, Cloudflare's get a login wall), so the
// scheduled crawl reaches it through the curious_coder actor, which fetches
// from residential exits.
//
// Built for the Free plan's $5 a month: one run reads the newest page(s) of
// cars listed in the last day, without listing details ($0.50 per 1 000
// results), and every run first checks the month's Apify spend and does
// nothing once it reaches the budget.

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"

	"carbuyer/crawler/internal/listing"
	"carbuyer/crawler/internal/source"
)

// ApifySourceName is the name searches.yml uses. Listings are still stored as
// SourceName ("marketplace"): the same site, priced as the same sellers.
const ApifySourceName = "marketplace-apify"

// ResultsPerPage is how many cards one Marketplace search page carries.
const ResultsPerPage = 24

// ApifySource runs the actor for one search.
type ApifySource struct {
	Token string
	Match Matcher
	// MaxCost caps one run, in dollars (Apify's maxTotalChargeUsd). Default 0.05.
	MaxCost float64
	// MonthlyBudget: no run starts once the month's Apify usage reaches it.
	// Default 4.50, under the Free plan's $5.
	MonthlyBudget float64
	Client        *http.Client
	BaseURL       string // ApifyBase
	Log           func(string)
}

var _ source.Source = (*ApifySource)(nil)

// Name implements source.Source.
func (s *ApifySource) Name() string { return ApifySourceName }

func (s *ApifySource) logf(format string, a ...any) {
	if s.Log != nil {
		s.Log(fmt.Sprintf(format, a...))
	}
}

func (s *ApifySource) base() string {
	if s.BaseURL != "" {
		return s.BaseURL
	}
	return ApifyBase
}

func (s *ApifySource) client() *http.Client {
	if s.Client != nil {
		return s.Client
	}
	return http.DefaultClient
}

// MonthlyUsage is the dollars Apify has billed this account in the current
// usage cycle (GET /users/me/limits, current.monthlyUsageUsd).
func (s *ApifySource) MonthlyUsage(ctx context.Context) (float64, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, s.base()+"/users/me/limits", nil)
	if err != nil {
		return 0, err
	}
	req.Header.Set("Authorization", "Bearer "+s.Token)
	resp, err := s.client().Do(req)
	if err != nil {
		return 0, err
	}
	defer resp.Body.Close()
	b, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return 0, err
	}
	if resp.StatusCode != http.StatusOK {
		return 0, fmt.Errorf("Apify usage check: HTTP %d", resp.StatusCode)
	}
	var v struct {
		Data struct {
			Current struct {
				MonthlyUsageUsd *float64 `json:"monthlyUsageUsd"`
			} `json:"current"`
		} `json:"data"`
	}
	if err := json.Unmarshal(b, &v); err != nil {
		return 0, err
	}
	if v.Data.Current.MonthlyUsageUsd == nil {
		return 0, fmt.Errorf("Apify usage check: no current.monthlyUsageUsd in the answer")
	}
	return *v.Data.Current.MonthlyUsageUsd, nil
}

// ApifySearchURL is the Marketplace search the actor opens: cars listed in
// the last day, newest first, within the query's price range.
func ApifySearchURL(token string, q listing.Query) string {
	params := [][2]string{{"daysSinceListed", "1"}, {"sortBy", "creation_time_descend"}}
	if q.PriceFrom != nil {
		params = append(params, [2]string{"minPrice", strconv.Itoa(int(*q.PriceFrom))})
	}
	if q.PriceTo != nil {
		params = append(params, [2]string{"maxPrice", strconv.Itoa(int(*q.PriceTo))})
	}
	return Origin + "/marketplace/" + token + "/cars" + formEncode(params)
}

// proxyCountry is where the actor's exit should be: an exit elsewhere serves
// that country's cars.
func proxyCountry(province string) string {
	switch province {
	case "QC", "ON", "NB", "NS":
		return "CA"
	}
	return "US"
}

// Search implements source.Source: one actor run per city, q.MaxPages pages
// each (at least 1). Over the monthly budget it returns nothing, not an
// error, so the rest of the crawl goes on.
func (s *ApifySource) Search(ctx context.Context, q listing.Query, onPage func(source.PageEvent)) (source.Result, error) {
	cities, err := Cities(q.Geo)
	if err != nil {
		return source.Result{}, err
	}
	if s.Token == "" {
		return source.Result{}, fmt.Errorf("APIFY_TOKEN is not set")
	}
	maxCost, budget := s.MaxCost, s.MonthlyBudget
	if maxCost <= 0 {
		maxCost = 0.05
	}
	if budget <= 0 {
		budget = 4.50
	}
	pages := max(q.MaxPages, 1)
	res := source.Result{Listings: []listing.Listing{}, Truncated: true}

	for i, city := range cities {
		spent, err := s.MonthlyUsage(ctx)
		if err != nil {
			return res, err
		}
		if spent+maxCost > budget {
			s.logf("  Apify: $%.2f used this month, budget $%.2f: skipping %s", spent, budget, city.Key)
			continue
		}
		url := ApifySearchURL(city.Token, q)
		if res.URL == "" {
			res.URL = url
		}
		in := ActorInput{
			URLs: []string{url}, SortBy: "creation_time_descend",
			Proxy:          Proxy{UseApifyProxy: true, ApifyProxyCountry: proxyCountry(city.Province)},
			MaxPagesPerURL: pages,
			MaxItems:       pages * ResultsPerPage,
			ResultsLimit:   pages * ResultsPerPage,
			// Details and the full gallery are the expensive events; the
			// card already has title, price, mileage, place and dealer tag.
			GetListingDetails: false, GetAllListingPhotos: false, StrictFiltering: false,
		}
		run, err := RunActor(ctx, ActorRun{Input: &in, MaxItems: in.MaxItems, MaxCost: maxCost, Token: s.Token,
			Client: s.Client, BaseURL: s.BaseURL})
		if err != nil {
			return res, err
		}
		items, skipped := NormalizeMarketplaceItems(run.Items, s.Match)
		kept, foreign := 0, 0
		for _, it := range items {
			if it.Province != nil && *it.Province != "" && !sameRegion(*it.Province, city.Province) {
				foreign++
				continue
			}
			res.Listings = append(res.Listings, it.Listing)
			kept++
		}
		res.PagesWalked += pages
		s.logf("  Apify %s: %d results, %d kept, %d outside the region, %d parts or unusable (run %s, ~$%.3f)",
			city.Key, len(run.Items), kept, foreign, skipped, run.RunID, EstimateBasicCost(len(run.Items)))
		if onPage != nil {
			onPage(source.PageEvent{Page: i + 1, Of: len(cities), Found: kept})
		}
	}
	res.Pages = listing.Num(float64(res.PagesWalked))
	return res, nil
}
