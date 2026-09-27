import { useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';

export type SearchableOption = { id: number; name: string };

/** Options rendered at once; typing narrows larger lists (#420). */
const MAX_VISIBLE_OPTIONS = 50;

/**
 * Accessible, searchable single select (#420): an ARIA 1.2 combobox
 * (`role="combobox"` input + `role="listbox"` popup, `aria-activedescendant`
 * for the highlighted option). Shows names, reports the numeric id.
 *
 * Keyboard: typing filters, ArrowDown/ArrowUp move (and open the list),
 * Enter picks the highlighted option, Escape closes (a second Escape clears
 * the search). `value` is the selected id, or `null` for "none".
 */
export function SearchableSelect({
  label,
  options,
  value,
  onChange,
  noneLabel,
  noMatchesLabel,
  moreLabel,
  placeholder,
  disabled
}: {
  label: string;
  options: SearchableOption[];
  value: number | null;
  onChange: (value: number | null) => void;
  /** Label of the "no value" option. */
  noneLabel: string;
  noMatchesLabel: string;
  /** Hint shown when more matches exist than are rendered; receives the hidden count. */
  moreLabel: (hidden: number) => string;
  placeholder?: string;
  disabled?: boolean;
}) {
  const baseId = useId();
  const listId = `${baseId}-list`;
  const inputRef = useRef<HTMLInputElement>(null);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState<string | null>(null);
  const [highlight, setHighlight] = useState(0);

  const selected = value === null ? null : options.find((option) => option.id === value) ?? null;
  // An id that is not in the synced mirror (not yet synced / deleted) stays
  // visible as "#id" instead of silently disappearing.
  const selectedText = value === null ? '' : selected ? selected.name : `#${value}`;

  const matches = useMemo(() => {
    const needle = (query ?? '').trim().toLocaleLowerCase();
    if (!needle) return options;
    return options.filter(
      (option) => option.name.toLocaleLowerCase().includes(needle) || String(option.id) === needle
    );
  }, [options, query]);
  const visible = matches.slice(0, MAX_VISIBLE_OPTIONS);
  // Entry 0 is always the "none" option.
  const entries: Array<SearchableOption | null> = [null, ...visible];
  const activeIndex = Math.min(highlight, entries.length - 1);

  const optionId = (index: number) => `${baseId}-opt-${index}`;

  const openList = () => {
    if (disabled) return;
    setOpen(true);
    const currentIndex = entries.findIndex((entry) => (entry?.id ?? null) === value);
    setHighlight(currentIndex >= 0 ? currentIndex : 0);
  };

  const commit = (entry: SearchableOption | null) => {
    onChange(entry ? entry.id : null);
    setQuery(null);
    setOpen(false);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    switch (event.key) {
      case 'ArrowDown':
        event.preventDefault();
        if (!open) openList();
        else setHighlight((index) => Math.min(index + 1, entries.length - 1));
        break;
      case 'ArrowUp':
        event.preventDefault();
        if (!open) openList();
        else setHighlight((index) => Math.max(index - 1, 0));
        break;
      case 'Enter':
        if (open) {
          event.preventDefault();
          commit(entries[activeIndex] ?? null);
        }
        break;
      case 'Escape':
        if (open) {
          event.preventDefault();
          event.stopPropagation();
          setOpen(false);
        } else if (query !== null) {
          event.preventDefault();
          setQuery(null);
        }
        break;
      default:
        break;
    }
  };

  return (
    <div className="searchable-select">
      <label htmlFor={`${baseId}-input`}>{label}</label>
      <input
        id={`${baseId}-input`}
        ref={inputRef}
        type="text"
        role="combobox"
        autoComplete="off"
        aria-autocomplete="list"
        aria-expanded={open}
        aria-controls={listId}
        aria-activedescendant={open ? optionId(activeIndex) : undefined}
        value={query ?? selectedText}
        placeholder={placeholder}
        disabled={disabled}
        onChange={(event) => {
          setQuery(event.target.value);
          setHighlight(event.target.value.trim() ? 1 : 0);
          setOpen(true);
        }}
        onFocus={(event) => event.target.select()}
        onClick={() => (open ? setOpen(false) : openList())}
        onKeyDown={onKeyDown}
        onBlur={() => {
          setOpen(false);
          setQuery(null);
        }}
      />
      <ul id={listId} role="listbox" aria-label={label} className="searchable-select-list" hidden={!open}>
        {entries.map((entry, index) => (
          <li
            key={entry ? entry.id : 'none'}
            id={optionId(index)}
            role="option"
            aria-selected={(entry?.id ?? null) === value}
            className={index === activeIndex ? 'active' : undefined}
            // mousedown (not click) so the pick lands before the input blurs.
            onMouseDown={(event) => {
              event.preventDefault();
              commit(entry);
            }}
          >
            {entry ? entry.name : noneLabel}
          </li>
        ))}
        {visible.length === 0 && (
          <li role="presentation" className="searchable-select-empty">
            {noMatchesLabel}
          </li>
        )}
        {matches.length > visible.length && (
          <li role="presentation" className="searchable-select-empty">
            {moreLabel(matches.length - visible.length)}
          </li>
        )}
      </ul>
    </div>
  );
}
