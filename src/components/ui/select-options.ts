/** One selectable entry. `group` is matched against a `SelectGroupSpec.key`. */
export interface SelectOption {
  value: string;
  label: string;
  group?: string;
}

/** A group heading. Unlike `RegionGroup` this carries an already-translated
 * label, not an i18n key -- the component doesn't do translation. */
export interface SelectGroupSpec {
  key: string;
  label: string;
}

/** One rendered block: a heading plus its options, or `group: null` for the
 * ungrouped block. */
export interface SelectSection {
  group: SelectGroupSpec | null;
  options: SelectOption[];
}

/**
 * Organizes options into rendered sections.
 *
 * Without `groups`, everything lands in a single ungrouped section. With
 * `groups`, sections follow the *groups array's* order (not the options'),
 * empty groups are dropped so no bare heading renders, and any option whose
 * `group` is missing or unknown is kept in a leading ungrouped section --
 * dropping those would silently make dynamically-injected entries (like the
 * region picker's "keep current value" option) unselectable.
 */
export function groupOptions(
  options: SelectOption[],
  groups?: SelectGroupSpec[],
): SelectSection[] {
  if (!groups || groups.length === 0) return [{ group: null, options }];

  const known = new Set(groups.map((g) => g.key));
  const loose = options.filter((o) => !o.group || !known.has(o.group));

  const sections: SelectSection[] = loose.length > 0 ? [{ group: null, options: loose }] : [];
  for (const group of groups) {
    const members = options.filter((o) => o.group === group.key);
    if (members.length > 0) sections.push({ group, options: members });
  }
  return sections;
}
