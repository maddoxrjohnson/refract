# 1. Nothing runs while the window is hidden

Status: accepted (M3, M4, M6)

**Context.** Refract lives in the tray and is opened with a hotkey, usually for a few seconds. A tool people leave running all day must not cost them frames in a game or battery on a laptop.

**Decision.** When the window is hidden everything that costs anything stops: the canvas frame loop is set to `never` and the ticker is cancelled, the GPU and system sampler thread ends, the interface stops polling usage and refreshing remote details, and the hidden WebView2 is suspended with a low memory target. The helper keeps running, because its inotify watches use no CPU while nothing changes.

**Consequences.** The interface cannot assume it saw every event while hidden, so each show fetches the repo list, settings, remote list and hook state again. The cost is spread over the code (the scene, the sampler, the polls) and tested at each place, not in one switch.
