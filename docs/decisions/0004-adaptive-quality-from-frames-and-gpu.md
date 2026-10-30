# 4. Pick the graphics level from frame intervals and GPU counters

Status: accepted (M4, polish)

**Context.** The spec calls for three levels chosen automatically. In a WebView the CPU time of a frame says nothing about the GPU, because GL calls queue in another process. Other programs, such as a game, can take the GPU without Refract's frames getting any later until it is too late.

**Decision.** The controller (`quality/controller.ts`) uses two signals. The scene reports the time between consecutive drawn frames of continuous animation, and the Windows side reports system GPU use every 2 seconds. Other programs above 60% send it straight to Low. Late frames (median over 1.25 times the budget for 3 seconds) step down one level and block that level for 30 seconds. Stepping up needs 10 seconds of on-time frames (within 1.1 times the budget) and GPU headroom. Low is lifted on GPU readings alone, after others stay under 45%, with a wait that doubles (to two minutes) when the GPU turns busy again soon after.

**Consequences.** The gaps between thresholds stop the level flipping, and every flip rebuilds the effects, so it matters. The controller is a pure object that is given the time, so its tests need no clock. A fixed choice in the graphics control bypasses it. The thresholds are tested with simulated samples; how they behave on real hardware under a game is still to be checked by hand.
