#include "account_cache.h"
#include <string.h>

struct Slot {
    bool used;
    uint32_t at;   // when it came
    UsageData d;
};

static Slot slots[ACCOUNT_CACHE_SLOTS];
static int count = -1;   // the `ap` count the slots belong to

void account_cache_clear(void) {
    memset(slots, 0, sizeof(slots));
    count = -1;
}

void account_cache_store(const UsageData* d, uint32_t now_ms) {
    if (!d->ok || d->account_idx < 0 || d->account_count < 2) return;
    if (d->account_count != count) {
        account_cache_clear();
        count = d->account_count;
    }
    if (d->account_idx >= ACCOUNT_CACHE_SLOTS) return;
    Slot& s = slots[d->account_idx];
    s.used = true;
    s.at = now_ms;
    s.d = *d;
}

int account_cache_next(int from, int step) {
    if (count < 2 || from < 0) return -1;
    const int dir = step < 0 ? -1 : 1;
    for (int k = 1; k < count; k++) {
        const int i = ((from + dir * k) % count + count) % count;
        if (i < ACCOUNT_CACHE_SLOTS && slots[i].used) return i;
    }
    return -1;
}

static int run_down(int mins, int gone) {
    if (mins < 0) return mins;   // unknown or not started stays so
    return mins > gone ? mins - gone : 0;
}

bool account_cache_overlay(UsageData* view, int idx, uint32_t now_ms) {
    if (idx < 0 || idx >= ACCOUNT_CACHE_SLOTS || !slots[idx].used) return false;
    const UsageData& k = slots[idx].d;
    const int gone = (int)((uint32_t)(now_ms - slots[idx].at) / 60000u);
    view->session_pct = k.session_pct;
    view->session_reset_mins = run_down(k.session_reset_mins, gone);
    view->weekly_pct = k.weekly_pct;
    view->weekly_reset_mins = run_down(k.weekly_reset_mins, gone);
    view->fable_pct = k.fable_pct;
    memcpy(view->status, k.status, sizeof(view->status));
    view->enterprise = k.enterprise;
    view->weekly_only = k.weekly_only;
    view->codex = k.codex;
    view->resets_held = k.resets_held;
    view->resets_expiry_n = k.resets_expiry_n;
    memcpy(view->resets_expiry, k.resets_expiry, sizeof(view->resets_expiry));
    memcpy(view->resets_kind, k.resets_kind, sizeof(view->resets_kind));
    memcpy(view->account, k.account, sizeof(view->account));
    view->account_idx = k.account_idx;
    view->account_count = k.account_count;
    view->time_pct = k.time_pct;
    view->period_days = k.period_days;
    memcpy(view->reset_date, k.reset_date, sizeof(view->reset_date));
    view->ok = true;
    view->replay = true;
    return true;
}
