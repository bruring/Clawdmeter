#pragma once
#include <stdint.h>
#include "data.h"

// The app's accounts, kept on the device so a swipe shows another one at
// once instead of asking the app and waiting for its write. The app sends
// each account it isn't showing as a `bg:1` payload (with its `an`/`ap`),
// and every shown payload with an `ap` lands here too. Keyed by the `ap`
// index. Pure (no Arduino): runs in the native tests.

#define ACCOUNT_CACHE_SLOTS 8

void account_cache_clear(void);

// Keep d's numbers under its `ap` index. Ignored without `ap`, when !ok, or
// past ACCOUNT_CACHE_SLOTS. A different account count empties the cache
// first: the app's list changed, so the indexes mean other accounts.
void account_cache_store(const UsageData* d, uint32_t now_ms);

// The next kept index from `from`, going `step` (+1 / -1) round the list,
// never `from` itself; -1 when nothing else is kept.
int account_cache_next(int from, int step);

// Put the numbers kept at `idx` over `view` (its clock, activity, alerts
// and settings stay), as a replay, with the reset countdowns run down by
// the time since they came. False when nothing is kept there.
bool account_cache_overlay(UsageData* view, int idx, uint32_t now_ms);
