// Package vocab is the built-in list of US makes and models that the
// title-only sources (Craigslist, Marketplace) match ad titles against.
//
// The CLI otherwise learns that list from what the structured sources
// (AutoHebdo, Kijiji) stored. A tri-state crawl has no structured source and
// the scheduled run starts from an empty database, so without this list no
// title would ever get a make and model, and nothing could be priced.
//
// us.tsv is generated from NHTSA vPIC: node tools/us-vocabulary/generate.mjs.
package vocab

import (
	_ "embed"
	"strings"
)

//go:embed us.tsv
var usTSV string

// Model is one make/model pair and a spelling to search for (As; empty means
// the model name itself). The matcher answers with Model either way.
type Model struct{ Make, Model, As string }

// US returns the makes and the models, plus a hyphen-free spelling of every
// hyphenated model, since sellers write "CRV" and "F150" as often as "CR-V".
func US() (makes []string, models []Model) {
	seen := map[string]bool{}
	for _, line := range strings.Split(usTSV, "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		mk, md, ok := strings.Cut(line, "\t")
		if !ok || mk == "" || md == "" {
			continue
		}
		if !seen[mk] {
			seen[mk] = true
			makes = append(makes, mk)
		}
		models = append(models, Model{Make: mk, Model: md})
		if joined := strings.ReplaceAll(md, "-", ""); joined != md {
			models = append(models, Model{Make: mk, Model: md, As: joined})
		}
	}
	return makes, models
}
