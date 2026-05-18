/// Markdown table → card-style text conversion.
///
/// Telegram doesn't support HTML tables, so we convert markdown tables
/// into readable card-style key-value blocks.

/// Represents a parsed markdown table.
struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// Try to parse a markdown table from consecutive lines.
///
/// Returns `Some(Table)` if the lines contain a valid table (header row,
/// separator row, data rows), `None` otherwise.
fn try_parse_table(lines: &[&str]) -> Option<Table> {
    if lines.len() < 2 {
        return None;
    }

    let header = lines[0].trim();
    let sep = lines[1].trim();

    if !header.starts_with('|') || !header.ends_with('|') {
        return None;
    }
    if !sep.starts_with('|') || !sep.ends_with('|') {
        return None;
    }

    // Verify separator row contains only |, -, :, and spaces
    let sep_body: String = sep.chars().filter(|c| !c.is_whitespace()).collect();
    if !sep_body.starts_with('|') || !sep_body.ends_with('|') {
        return None;
    }
    let sep_inner = &sep_body[1..sep_body.len().checked_sub(1)?];
    if sep_inner.is_empty() || !sep_inner.chars().all(|c| c == '-' || c == ':' || c == '|') {
        return None;
    }

    let headers: Vec<String> = parse_row(header);
    if headers.is_empty() {
        return None;
    }

    let mut rows = Vec::new();
    for line in lines.iter().skip(2) {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('|') {
            break; // End of table
        }
        let cells = parse_row(trimmed);
        if !cells.is_empty() {
            rows.push(cells);
        }
    }

    if rows.is_empty() {
        return None;
    }

    Some(Table { headers, rows })
}

/// Split a table row into cells (strip outer pipes, split on `|`).
fn parse_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix('|')
        .and_then(|s| s.strip_suffix('|'))
        .unwrap_or(trimmed);
    inner
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// Convert a markdown table to card-style text.
///
/// Each data row becomes a card block prefixed with `📋`.
/// The first header is used as the card title for each row.
fn table_to_cards(table: &Table) -> String {
    let mut output = String::new();

    for (ri, row) in table.rows.iter().enumerate() {
        if ri > 0 {
            output.push('\n');
        }

        // First cell → card title
        let title = row.first().map(|s| s.as_str()).unwrap_or("");
        let title_display = if title.is_empty() { "─" } else { title };
        output.push_str(&format!("📋 {title_display}\n"));

        // Remaining cells → key-value pairs
        for (ci, cell) in row.iter().enumerate().skip(1) {
            let label = table.headers.get(ci).map(|s| s.as_str()).unwrap_or("");
            output.push_str(&format!("   {label}: {cell}\n"));
        }
    }

    output.trim_end().to_string()
}

/// Find and convert markdown tables in text to card-style format.
///
/// Lines that are part of a markdown table are replaced with card-style
/// blocks. Non-table lines are left unchanged.
pub fn convert_tables(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result = String::new();
    let mut i = 0;

    while i < lines.len() {
        // Try to find a table starting at line i
        if let Some(table) = try_parse_table(&lines[i..]) {
            let row_count = 1 + 1 + table.rows.len(); // header + sep + data rows
            let cards = table_to_cards(&table);
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&cards);
            i += row_count;
        } else {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(lines[i]);
            i += 1;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_table() {
        let input = "| Name | Age |\n|------|-----|\n| Alice | 30 |\n| Bob | 25 |\n";
        let result = convert_tables(input);
        assert!(result.contains("📋 Alice"));
        assert!(result.contains("Age: 30"));
        assert!(result.contains("📋 Bob"));
        assert!(result.contains("Age: 25"));
    }

    #[test]
    fn test_table_with_non_table_text() {
        let input = "Here is some data:\n\n| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\nThat's it.\n";
        let result = convert_tables(input);
        assert!(result.contains("Here is some data:"));
        assert!(result.contains("📋 1"));
        assert!(result.contains("B: 2"));
        assert!(result.contains("That's it."));
    }

    #[test]
    fn test_no_table() {
        let input = "Just plain text\nwith multiple lines\n";
        let result = convert_tables(input);
        assert_eq!(result, input.trim());
    }

    #[test]
    fn test_table_with_multiple_headers() {
        let input = "| Command | Description | Example |\n|---|---|---|\n| `git add` | Stage files | `git add .` |\n| `git commit` | Commit | `git commit -m \"msg\"` |\n";
        let result = convert_tables(input);
        assert!(result.contains("📋 `git add`"));
        assert!(result.contains("Description: Stage files"));
        assert!(result.contains("Example: `git add .`"));
        assert!(result.contains("📋 `git commit`"));
        assert!(result.contains("Description: Commit"));
    }

    #[test]
    fn test_table_no_data_rows() {
        let input = "| H1 | H2 |\n|---|---|\n";
        let result = convert_tables(input);
        // Only header + separator, no data rows → not a valid table
        assert_eq!(result, input.trim());
    }

    #[test]
    fn test_table_empty_cells() {
        let input = "| Name | Value |\n|------|-------|\n| X |  |\n|  | Y |\n";
        let result = convert_tables(input);
        assert!(result.contains("📋 X"));
        assert!(result.contains("📋 ─"));
    }

    #[test]
    fn test_table_adjacent_tables() {
        let input = "| A | B |\n|---|---|\n| 1 | 2 |\n| C | D |\n|---|---|\n| 3 | 4 |\n";
        let result = convert_tables(input);
        // Two separate tables should both be converted
        assert!(result.contains("📋 1"));
        assert!(result.contains("📋 3"));
        assert!(result.contains("📋 C"));
    }

    #[test]
    fn test_table_no_surrounding_pipes_preserved() {
        // Lines that don't match table format should be left as-is
        let input = "This is | not a table\nbecause it doesn't start with pipe\n";
        let result = convert_tables(input);
        assert_eq!(result, input.trim());
    }

    #[test]
    fn test_try_parse_table_valid() {
        let lines = vec!["| H1 | H2 |", "|---|---|", "| a | b |"];
        let table = try_parse_table(&lines);
        assert!(table.is_some());
        let t = table.unwrap();
        assert_eq!(t.headers, vec!["H1", "H2"]);
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.rows[0], vec!["a", "b"]);
    }

    #[test]
    fn test_try_parse_table_invalid_separator() {
        let lines = vec!["| H1 | H2 |", "| xxx |", "| a | b |"];
        let table = try_parse_table(&lines);
        assert!(table.is_none());
    }

    #[test]
    fn test_convert_empty_string() {
        let result = convert_tables("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_parse_row_basic() {
        let cells = parse_row("| a | b | c |");
        assert_eq!(cells, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_parse_row_no_outer_pipes() {
        let cells = parse_row("a | b");
        assert_eq!(cells, vec!["a", "b"]);
    }

    #[test]
    fn test_table_with_code_blocks() {
        let input = "| File | Size |\n|------|------|\n| `src/main.rs` | 2 KB |\n| `src/lib.rs` | 1 KB |\n";
        let result = convert_tables(input);
        assert!(result.contains("📋 `src/main.rs`"));
        assert!(result.contains("Size: 1 KB"));
    }
}
