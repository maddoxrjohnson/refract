import { Component, type ReactNode } from "react";

/**
 * Keeps a scene that fails to load (its chunk on a flaky network) or to render from taking
 * the whole app down: the overlays stay, with the same message as when WebGL cannot start.
 */
export class SceneBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  componentDidCatch(error: unknown) {
    console.error("refract: the scene failed", error);
  }

  render() {
    if (!this.state.failed) return this.props.children;
    return (
      <div className="scene">
        <div className="scene-fail">The scene could not be loaded. Reload the app to try again.</div>
      </div>
    );
  }
}
