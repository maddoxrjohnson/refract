import { useId } from "react";
import type { QualityLevel, QualitySetting } from "../quality/tiers";
import "./ui.css";

export const QUALITY_STORAGE_KEY = "refract.quality";

const SETTINGS: { value: QualitySetting; label: string }[] = [
  { value: "auto", label: "Auto" },
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
];

const LEVEL_LABELS: Record<QualityLevel, string> = { low: "Low", medium: "Medium", high: "High" };

const isSetting = (v: unknown): v is QualitySetting => SETTINGS.some((s) => s.value === v);

/** The saved setting, or "auto" when there is none or storage is unavailable. */
export function loadQualitySetting(): QualitySetting {
  try {
    const v = localStorage.getItem(QUALITY_STORAGE_KEY);
    return isSetting(v) ? v : "auto";
  } catch {
    return "auto";
  }
}

export function saveQualitySetting(setting: QualitySetting): void {
  try {
    localStorage.setItem(QUALITY_STORAGE_KEY, setting);
  } catch {
    // Storage blocked: the choice lasts until the window reloads.
  }
}

export interface QualityControlProps {
  setting: QualitySetting;
  /** The level drawing now; shown next to Auto. */
  level: QualityLevel;
  /** Called after the choice is saved. */
  onChange(setting: QualitySetting): void;
}

/** Auto, Low, Medium or High (spec §3.6). Saves the choice in localStorage. */
export function QualityControl({ setting, level, onChange }: QualityControlProps) {
  const name = useId();
  const choose = (next: QualitySetting) => {
    saveQualitySetting(next);
    onChange(next);
  };
  return (
    <fieldset className="rf-glass rf-quality">
      <legend>Graphics</legend>
      <div className="rf-quality__options">
        {SETTINGS.map((s) => (
          <label key={s.value} className="rf-quality__option">
            <input
              type="radio"
              name={name}
              value={s.value}
              checked={setting === s.value}
              onChange={() => choose(s.value)}
            />
            <span>{s.label}</span>
          </label>
        ))}
      </div>
      <p className="rf-quality__active" role="status">
        {setting === "auto" && (
          <>
            Drawing at <span className="rf-mono">{LEVEL_LABELS[level]}</span>
          </>
        )}
      </p>
    </fieldset>
  );
}
