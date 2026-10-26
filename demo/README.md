# Demo: data quality before and after

`dq_scenes.py` creates six externally measured data quality rule occurrences on the
output fields of the demo MCP Server's tools. It then switches their scores between
scenes, so the gate can be shown before and after a data fix without running a Cloud
Data Quality job.

| Tool | Field | Dimension | Threshold | before | degraded | after |
|---|---|---|---|---|---|---|
| `search_sales_orders` | `orders[].customer_email` | Validity | 95 | 61.5 | 93.2 | 98.7 |
| | `orders[].shipping_country` | Completeness | 98 | 88 | 99.1 | 100 |
| | `orders[].order_total` | Accuracy | 99 | 97 | 99.4 | 99.6 |
| | `orders[].customer_id` | Uniqueness | 100 | 100 | 100 | 100 |
| `check_inventory` | `stock_items[].available_qty` | Validity | 99 | 100 | 100 | 100 |
| | `stock_items[].last_counted_at` | Completeness | 95 | 97.5 | 97.5 | 97.5 |

With the defaults (minimum, warn below 90, block below 70):

| Scene | `search_sales_orders` | `check_inventory` | Other tools |
|---|---|---|---|
| before | **blocked** at 61.5, the upstream is not called | pass, 97.5 | unscored, allowed |
| degraded | **warn** at 93.2: e-mail validity is below its threshold of 95 | pass | unscored |
| after | **pass** at 98.7, the trace shows 61.5 → 98.7 | pass | unscored |

## Setup

1. `cp config.json.example config.json` and fill it in with the same values as the
   gateway policy. The service account needs write access to the catalog. `config.json`
   is gitignored.
2. `pip install requests`
3. `python3 dq_scenes.py provision` creates the rules (in the *after* scene). It is
   idempotent.

## Run

```sh
python3 dq_scenes.py before     # broken data
python3 dq_scenes.py degraded   # one rule below its own threshold
python3 dq_scenes.py after      # fixed data
python3 dq_scenes.py status     # the scores and the verdict the gate will reach
python3 dq_scenes.py teardown   # delete the demo rules
```

Each scene waits until CDGC search shows the new scores (usually about 15 s). The gateway
picks them up within `scoreRefreshSeconds` of its last refresh, so set it to 30 for the
demo. Switch scenes about a minute before you show them.

Show *before* first, then *after*. The gateway records the previous score only when it
sees the score change, so the before → after trend appears after it has served the
*before* scene at least once.

Nothing secret is printed: HTTP errors show the status only.
