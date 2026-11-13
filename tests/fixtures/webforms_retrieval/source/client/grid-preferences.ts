// Remember grid column visibility; this does not control map layer access.
export function saveColumnVisibility(column: string, visible: boolean): void {
    localStorage.setItem(`grid-column-${column}`, String(visible));
}
