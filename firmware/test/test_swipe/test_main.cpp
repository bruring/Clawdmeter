// swipe: which finger paths ask for another account and which are taps.
//   pio test -d firmware -e native -f test_swipe
#include <unity.h>
#include <stdio.h>
#include "swipe.h"

void setUp(void) {}
void tearDown(void) {}

static void test_swipes_and_taps(void) {
    struct { int dx, dy; uint32_t ms; int w; int want; } cases[] = {
        {-200, 10, 250, 480, 1},     // left across the panel: next account
        {200, -30, 250, 480, -1},    // right: previous
        {-120, 0, 250, 480, 1},      // a quarter of the width is enough
        {-100, 0, 250, 480, 0},      // shorter: a tap
        {-200, 120, 250, 480, 0},    // too steep
        {-200, 10, 900, 480, 0},     // too slow: a drag
        {4, 3, 90, 480, 0},          // a plain tap
        {-40, 5, 200, 135, 1},       // a narrow panel: 34 px is a quarter
        {-25, 0, 200, 100, 0},       // never under 30 px
    };
    for (auto& c : cases) {
        char msg[64];
        snprintf(msg, sizeof(msg), "dx %d dy %d ms %u w %d", c.dx, c.dy, (unsigned)c.ms, c.w);
        TEST_ASSERT_EQUAL_INT_MESSAGE(c.want, swipe_step(c.dx, c.dy, c.ms, c.w), msg);
    }
}

int main(int, char**) {
    UNITY_BEGIN();
    RUN_TEST(test_swipes_and_taps);
    return UNITY_END();
}
