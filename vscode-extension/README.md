# Refract

Companion extension for [Refract](https://github.com/maddoxrjohnson/refract). When Refract launches an agent for a folder, this extension opens a terminal in the editor area of that folder's VS Code window and runs the agent command in it.

It runs in the workspace (WSL) extension host and does nothing unless Refract has left a launch request for the window's folder. In an ordinary window it is inert.

## How it works

Refract's helper writes a request file to `$XDG_STATE_HOME/refract/launch/` (default `~/.local/state/refract/launch/`). The extension claims a matching request by renaming it, opens a terminal named after the request, and types a short command line into it (see below). A window opened for a request also gets a bare layout: sidebar, panel and secondary sidebar closed, only the terminal left open; the layout closes other clean tabs by identity and never touches terminals or unsaved files. Requests that arrive later just add a terminal tab.

Arguments that are not plain tokens, such as a multi-line prompt, are not typed. They are passed in the terminal's environment as `REFRACT_ARG_<n>` and the typed line refers to them as `"$REFRACT_ARG_<n>"`, so the line stays short and free of characters the terminal would interpret. A request whose program (`argv[0]`) is not a plain token is ignored.

The extension only acts when the window has exactly one workspace folder. See `docs/launch.md` in the repository for the full protocol.

## License

MIT
