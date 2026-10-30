import { useId, useRef, useState, type FormEvent, type ReactNode } from "react";
import type { CostEstimate, PriceRow } from "../backend/types";
import {
  DEFAULT_PRICES,
  MAX_MATCH_LEN,
  MAX_PRICE,
  MAX_PRICE_ROWS,
  PRICES_TAKEN_ON,
  shadowedBy,
} from "../usage/cost";
import "./ui.css";

/** The price columns, in table order, with their short header and full name. */
const COLUMNS: [key: Exclude<keyof PriceRow, "match">, short: string, full: string][] = [
  ["input", "In", "Input"],
  ["output", "Out", "Output"],
  ["cacheWrite5m", "W 5m", "Cache write, 5 minutes"],
  ["cacheWrite1h", "W 1h", "Cache write, 1 hour"],
  ["cacheRead", "Read", "Cache read"],
];

/** A row as typed: every cell is text until it is saved. `id` keys it for React. */
type DraftRow = { id: number } & Record<keyof PriceRow, string>;

/** `rows` as text; ids count down from `firstId`, so they never meet new rows' ids. */
function toDraft(rows: readonly PriceRow[], firstId: number): DraftRow[] {
  return rows.map((r, i) => ({
    id: firstId - i,
    match: r.match,
    input: String(r.input),
    output: String(r.output),
    cacheWrite5m: String(r.cacheWrite5m),
    cacheWrite1h: String(r.cacheWrite1h),
    cacheRead: String(r.cacheRead),
  }));
}

/** What is wrong with the first bad row, and which of its cells are bad. */
type Problem = { message: string; rowId: number; cells: (keyof PriceRow)[] };

/** The rows as prices, or what is wrong with the first bad one. The Windows side checks the same. */
function parseDraft(rows: DraftRow[]): PriceRow[] | Problem {
  const prices: PriceRow[] = [];
  for (const [i, row] of rows.entries()) {
    const match = row.match.trim();
    if (!match) return { message: `Row ${i + 1} needs a model to match.`, rowId: row.id, cells: ["match"] };
    if ([...match].length > MAX_MATCH_LEN) {
      return {
        message: `Row ${i + 1}: a model match can be at most ${MAX_MATCH_LEN} characters.`,
        rowId: row.id,
        cells: ["match"],
      };
    }
    const values = COLUMNS.map(([key]) => (row[key].trim() === "" ? NaN : Number(row[key])));
    const bad = COLUMNS.filter((_, c) => !(values[c] >= 0 && values[c] <= MAX_PRICE)).map(([key]) => key);
    if (bad.length > 0) {
      return {
        message: `Prices for "${match}" must be numbers from 0 to ${MAX_PRICE.toLocaleString("en")}.`,
        rowId: row.id,
        cells: bad,
      };
    }
    const [input, output, cacheWrite5m, cacheWrite1h, cacheRead] = values;
    prices.push({ match, input, output, cacheWrite5m, cacheWrite1h, cacheRead });
  }
  return prices;
}

export interface CostSectionProps {
  cost: CostEstimate;
  /** Saves new cost settings; resolves with whether it saved. */
  onSave(cost: CostEstimate): Promise<boolean>;
  labelId: string;
  /** Shown last, such as why saving failed. */
  children?: ReactNode;
}

/**
 * The usage widget's optional cost estimate (spec §3.5): on or off, and the price table.
 * The toggle saves at once; table edits are kept as a draft until Save.
 */
export function CostSection({ cost, onSave, labelId, children }: CostSectionProps) {
  // Added rows count up from 1; rows from a table count down from -1 (see `toDraft`).
  const lastId = useRef(0);
  const problemId = useId();
  const [draft, setDraft] = useState<DraftRow[] | null>(null);
  const [problem, setProblem] = useState<Problem | null>(null);
  const rows = draft ?? toDraft(cost.prices, -1);
  const shadows = shadowedBy(rows.map((r) => r.match));
  /** Marks a cell named in the problem as invalid and points it at the message. */
  const invalid = (row: DraftRow, key: keyof PriceRow) =>
    problem?.rowId === row.id && problem.cells.includes(key)
      ? { "aria-invalid": true, "aria-describedby": problemId }
      : {};

  const edit = (next: DraftRow[]) => {
    setDraft(next);
    setProblem(null);
  };
  const editCell = (id: number, key: keyof PriceRow, value: string) =>
    edit(rows.map((r) => (r.id === id ? { ...r, [key]: value } : r)));

  const save = (ev: FormEvent) => {
    ev.preventDefault();
    if (!draft) return;
    const prices = parseDraft(draft);
    if (!Array.isArray(prices)) {
      setProblem(prices);
      return;
    }
    // Edits made while saving start a new draft, which the save must not throw away.
    const submitted = draft;
    void onSave({ ...cost, prices }).then((saved) => {
      if (saved) setDraft((d) => (d === submitted ? null : d));
    });
  };

  return (
    <section className="rf-section" aria-labelledby={labelId}>
      <h3 id={labelId} className="rf-section__label">
        Cost estimate
      </h3>
      <label className="rf-check">
        <input
          type="checkbox"
          checked={cost.enabled}
          onChange={(ev) => void onSave({ ...cost, enabled: ev.target.checked })}
        />
        <span>Estimate cost at API prices</span>
      </label>
      {cost.enabled && (
        <form className="rf-prices" onSubmit={save}>
          <p className="rf-muted rf-prices__help">
            US dollars per million tokens. The first row whose text is in the model id is used. Defaults
            from Anthropic's price list, {PRICES_TAKEN_ON}. Plan limits are not shown.
          </p>
          <table className="rf-prices__table">
            <thead>
              <tr>
                <th scope="col" title="Text found in the model id, such as opus-5-5">
                  Model
                </th>
                {COLUMNS.map(([key, short, full]) => (
                  <th key={key} scope="col" title={`${full}, $ per million tokens`}>
                    {short}
                  </th>
                ))}
                <th scope="col">
                  <span className="rf-visually-hidden">Remove</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row, i) => (
                <tr key={row.id}>
                  <td>
                    <input
                      className="rf-input rf-mono rf-prices__input"
                      aria-label={`Row ${i + 1} model match`}
                      spellCheck={false}
                      value={row.match}
                      onChange={(ev) => editCell(row.id, "match", ev.target.value)}
                      {...invalid(row, "match")}
                    />
                  </td>
                  {COLUMNS.map(([key, , full]) => (
                    <td key={key}>
                      <input
                        className="rf-input rf-mono rf-prices__input"
                        aria-label={`Row ${i + 1} ${full.toLowerCase()} price`}
                        inputMode="decimal"
                        spellCheck={false}
                        value={row[key]}
                        onChange={(ev) => editCell(row.id, key, ev.target.value)}
                        {...invalid(row, key)}
                      />
                    </td>
                  ))}
                  <td>
                    <button
                      type="button"
                      className="rf-button rf-button--ghost rf-prices__remove"
                      aria-label={`Remove row ${i + 1}${row.match ? ` (${row.match})` : ""}`}
                      onClick={() => edit(rows.filter((r) => r.id !== row.id))}
                    >
                      ×
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {shadows.map(
            (j, i) =>
              j !== null && (
                <p key={rows[i].id} className="rf-muted rf-prices__warning">
                  Row {i + 1} is never used: row {j + 1} ("{rows[j].match.trim()}") matches first.
                </p>
              ),
          )}
          {problem && (
            <p id={problemId} className="rf-error" role="alert">
              {problem.message}
            </p>
          )}
          <div className="rf-prices__actions">
            <button
              type="button"
              className="rf-button"
              disabled={rows.length >= MAX_PRICE_ROWS}
              onClick={() =>
                edit([
                  ...rows,
                  {
                    id: ++lastId.current,
                    match: "",
                    input: "0",
                    output: "0",
                    cacheWrite5m: "0",
                    cacheWrite1h: "0",
                    cacheRead: "0",
                  },
                ])
              }
            >
              Add row
            </button>
            <button type="button" className="rf-button" onClick={() => edit(toDraft(DEFAULT_PRICES, -1_000_000))}>
              Reset to defaults
            </button>
            {draft && (
              <>
                <button type="submit" className="rf-button">
                  Save prices
                </button>
                <button
                  type="button"
                  className="rf-link"
                  onClick={() => {
                    setDraft(null);
                    setProblem(null);
                  }}
                >
                  Discard
                </button>
              </>
            )}
          </div>
        </form>
      )}
      {children}
    </section>
  );
}
