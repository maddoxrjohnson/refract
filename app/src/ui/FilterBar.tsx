import "./ui.css";

export interface FilterBarProps {
  /** The typed filter; the bar hides while it is empty. */
  filter: string;
  /** Gems that match it. */
  matches: number;
  total: number;
  /** Dispatch `{ type: "clearFilter" }`. */
  onClear(): void;
}

/** Shows what has been typed and how many gems match (spec §3.3). */
export function FilterBar({ filter, matches, total, onClear }: FilterBarProps) {
  if (!filter) return null;
  return (
    <div className="rf-glass rf-filter" role="search" aria-label="Filter">
      <span className="rf-filter__text" aria-label={`Filter: ${filter}`}>
        {filter}
      </span>
      <span
        className={`rf-filter__count${matches === 0 ? " rf-filter__count--none" : ""}`}
        role="status"
        aria-live="polite"
      >
        {matches === 0 ? "No matches" : `${matches} of ${total}`}
      </span>
      <button type="button" className="rf-button rf-button--ghost" onClick={onClear}>
        Clear <kbd className="rf-kbd">Esc</kbd>
      </button>
    </div>
  );
}
