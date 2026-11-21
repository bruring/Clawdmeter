#include "swipe.h"

int swipe_step(int dx, int dy, uint32_t ms, int panel_w) {
    const int adx = dx < 0 ? -dx : dx;
    const int ady = dy < 0 ? -dy : dy;
    const int min = panel_w / 4 > 30 ? panel_w / 4 : 30;
    if (ms > SWIPE_MAX_MS || adx < min || 2 * ady > adx) return 0;
    return dx < 0 ? 1 : -1;
}
