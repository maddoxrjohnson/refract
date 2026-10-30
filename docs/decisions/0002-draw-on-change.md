# 2. Draw on change, paced by a ticker

Status: accepted (M4)

**Context.** A scene that redraws 60 times a second all the time uses the GPU even when nothing moves, and the spec asks for negligible cost.

**Decision.** The canvas uses R3F's `demand` frame loop. A frame is drawn when props change, the window resizes or the pointer moves, and after a frame the scene asks for another only if something is still moving (ambient motion, an easing camera or gem, pointer parallax). `scene/ticker.ts` turns that request into an invalidation `1000 / fps` ms after the last frame started, a few milliseconds early so the next animation frame lands on the right refresh. Low has no ambient motion, so it is idle whenever nothing changes.

**Consequences.** The frame cap is held by the ticker, not by the display. Pointer movement goes through the ticker too, so waving the mouse does not exceed the cap. The frame loop allocates nothing per frame, since it can run for long stretches at the cap. Frame times are only meaningful while the scene animates continuously, which the quality controller accounts for ([0004](0004-adaptive-quality-from-frames-and-gpu.md)).
