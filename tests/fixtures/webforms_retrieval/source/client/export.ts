// The downloaded CSV uses a semicolon separator and preserves empty fields.
export function exportRows(rows: string[][]): string {
    const separator = ";";
    return rows.map(row => row.map(cell => `"${cell.replace(/"/g, '""')}"`).join(separator)).join("\r\n");
}
