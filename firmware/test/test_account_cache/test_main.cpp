// account_cache: the other accounts kept for a swipe (account_cache.h).
//   pio test -d firmware -e native -f test_account_cache
#include <unity.h>
#include <string.h>
#include "account_cache.h"

void setUp(void) { account_cache_clear(); }
void tearDown(void) {}

static UsageData acct(int idx, int count, float s, int sr, const char* name) {
    UsageData d = {};
    d.ok = d.valid = true;
    d.account_idx = (int8_t)idx;
    d.account_count = (int8_t)count;
    d.session_pct = s;
    d.session_reset_mins = sr;
    d.weekly_reset_mins = -1;
    strcpy(d.account, name);
    return d;
}

// Swiping goes round the kept accounts in either direction, skipping
// indexes nothing came for; a change in the count forgets the old ones.
static void test_rounds_the_kept_accounts(void) {
    UsageData a = acct(0, 4, 10, 60, "a"), b = acct(1, 4, 20, 60, "b"), d = acct(3, 4, 40, 60, "d");
    account_cache_store(&a, 0);
    account_cache_store(&b, 0);
    account_cache_store(&d, 0);
    struct { int from, step, want; } cases[] = {
        {1, 1, 3},    // 2 never came
        {3, 1, 0},    // wraps
        {0, -1, 3},
        {3, -1, 1},
    };
    for (auto& c : cases) TEST_ASSERT_EQUAL_INT(c.want, account_cache_next(c.from, c.step));

    UsageData no = acct(2, 4, 0, 0, "x");
    no.ok = false;   // no data is never kept (never a 0 %)
    account_cache_store(&no, 0);
    TEST_ASSERT_EQUAL_INT(3, account_cache_next(1, 1));

    UsageData moved = acct(0, 3, 5, 60, "a");   // the app's list changed
    account_cache_store(&moved, 0);
    TEST_ASSERT_EQUAL_INT(-1, account_cache_next(0, 1));
}

// The overlay swaps in the kept account's numbers as a replay, keeps the
// shown payload's own fields, and runs the reset countdowns down.
static void test_overlay(void) {
    UsageData k = acct(1, 2, 42, 90, "other");
    k.weekly_reset_mins = 30;
    account_cache_store(&k, 1000);

    UsageData view = acct(0, 2, 7, 200, "shown");
    view.activity = 2;
    view.clock_epoch = 1234;
    TEST_ASSERT_TRUE(account_cache_overlay(&view, 1, 1000 + 45 * 60000u));
    TEST_ASSERT_EQUAL_FLOAT(42, view.session_pct);
    TEST_ASSERT_EQUAL_INT(45, view.session_reset_mins);
    TEST_ASSERT_EQUAL_INT(0, view.weekly_reset_mins);   // passed: 0, not negative
    TEST_ASSERT_EQUAL_STRING("other", view.account);
    TEST_ASSERT_EQUAL_INT(1, view.account_idx);
    TEST_ASSERT_TRUE(view.replay);
    TEST_ASSERT_EQUAL_INT(2, view.activity);
    TEST_ASSERT_EQUAL_INT(1234, view.clock_epoch);
    TEST_ASSERT_FALSE(account_cache_overlay(&view, 0, 0));   // nothing kept there
}

int main(int, char**) {
    UNITY_BEGIN();
    RUN_TEST(test_rounds_the_kept_accounts);
    RUN_TEST(test_overlay);
    return UNITY_END();
}
