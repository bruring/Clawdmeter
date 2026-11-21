#pragma once
#include <stdint.h>

// A finger's path from press to release on a touch panel. A quick, mostly
// horizontal one asks the app for another of its accounts (a peek): a swipe
// to the left the next one, to the right the previous one. Anything else is
// a tap. Pure: no Arduino, so it runs in the native unit tests.

// Longer than this from press to release is a slow drag, not a swipe.
#define SWIPE_MAX_MS 800

// +1 (swipe left: next account), -1 (swipe right: previous) or 0 (a tap).
// `dx`/`dy` are release minus press in pixels, `panel_w` the panel's width:
// a swipe covers at least a quarter of it (30 px at least) and runs at least
// twice as far across as up or down.
int swipe_step(int dx, int dy, uint32_t ms, int panel_w);
