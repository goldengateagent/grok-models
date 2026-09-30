---
name: benchmarks-update
description: Query Artificial Analysis model benchmarks by one or more user-provided substrings in slugs from the BENCHMARKS map and print all results. Use for benchmark lookups or when the user runs `/benchmarks-update`.
---

# Artificial Analysis model benchmarks

When the user runs `/benchmarks-update <search string> [<search string> ...]`, split the text after `/benchmarks-update` into whitespace-separated `SEARCH_STRINGS`. If none are provided, ask for them.

Read `rust/grok-models.rs/src/benchmarks.rs` and store in `MATCHING_SLUGS` each unique slug key in the `BENCHMARKS` map that contains at least one string in `SEARCH_STRINGS`.

For each matching slug, run a separate request, passing the slug to `jq` as an argument for an exact match. Print all results.

```sh
for slug in "${MATCHING_SLUGS[@]}"; do
  curl 'https://artificialanalysis.ai/api/v2/data/llms/models' \
    -H "x-api-key: $ARTIFICIAL_ANALYSIS_API_KEY" \
  | jq --arg slug "$slug" -c '
      .data
      | map(
          select(.slug == $slug)
          | pick(
              .name,
              .slug,
              .evaluations.artificial_analysis_intelligence_index,
              .evaluations.artificial_analysis_coding_index
            )
        )
    '
done | jq -s 'add // []'
```
