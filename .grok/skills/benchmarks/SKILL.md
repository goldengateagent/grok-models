---
name: benchmarks
description: Query Artificial Analysis model benchmarks by a user-provided substring in the model slug and print all results. 
Use for benchmark lookups or when the user runs `/benchmarks`.
---

# Artificial Analysis model benchmarks

When the user runs `/benchmarks <search string>`, use the text after `/benchmarks` as `SEARCH_STRING`. If it is missing, ask for it. 
Run the query with the search string passed to `jq` as an argument:

```sh
curl 'https://artificialanalysis.ai/api/v2/data/llms/models' \
  -H "x-api-key: $ARTIFICIAL_ANALYSIS_API_KEY" \
| jq --arg search "$SEARCH_STRING" '
  .data
  | map(
      select(.slug | contains($search))
      | pick(
          .name,
          .slug,
          .evaluations.artificial_analysis_intelligence_index,
          .evaluations.artificial_analysis_coding_index
        )
    )
'
```
