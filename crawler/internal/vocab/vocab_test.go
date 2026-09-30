package vocab

import "testing"

func TestUS(t *testing.T) {
	makes, models := US()
	if len(makes) < 30 || len(models) < 1000 {
		t.Fatalf("makes=%d models=%d: us.tsv looks truncated", len(makes), len(models))
	}
	want := map[Model]bool{
		{Make: "Toyota", Model: "RAV4"}:            false,
		{Make: "Honda", Model: "CR-V"}:             false,
		{Make: "Honda", Model: "CR-V", As: "CRV"}:  false,
		{Make: "Ford", Model: "F-150"}:             false,
		{Make: "Ford", Model: "F-150", As: "F150"}: false,
	}
	for _, m := range models {
		if _, ok := want[m]; ok {
			want[m] = true
		}
	}
	for m, found := range want {
		if !found {
			t.Errorf("missing %+v", m)
		}
	}
}
