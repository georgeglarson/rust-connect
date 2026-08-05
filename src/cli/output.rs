//! Human-readable rendering for client-mode output.

/// One row of the `devices` table.
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub state: String,
    pub paired_at: Option<String>,
}

/// Device IDs are 32+ chars of wire-safe ASCII; the first 8 are plenty for
/// telling devices apart in a terminal. `chars().take()` keeps this safe
/// even for off-spec input.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// "2026-07-30T12:34:56.789Z" -> "2026-07-30 12:34"; anything that does not
/// look like an RFC3339 timestamp passes through unchanged.
pub fn format_timestamp(rfc3339: &str) -> String {
    let bytes = rfc3339.as_bytes();
    let looks_iso = bytes.len() >= 16
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && (bytes[10] == b'T' || bytes[10] == b' ')
        && bytes[13] == b':';
    if looks_iso {
        // Char-wise, not byte-wise: a crafted string with a multi-byte
        // character straddling byte 16 would panic a `..16` byte slice.
        let mut chars: Vec<char> = rfc3339.chars().take(16).collect();
        if chars.len() == 16 {
            chars[10] = ' ';
        }
        chars.into_iter().collect()
    } else {
        rfc3339.to_string()
    }
}

/// Seconds -> compact uptime ("2d 3h", "3h 4m", "5m 6s", "42s").
pub fn format_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {}s", secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Maximum width of the NAME column before truncation.
const MAX_NAME_WIDTH: usize = 30;

/// Aligned table: ID (short), NAME, TYPE, STATE, PAIRED AT.
pub fn render_devices_table(rows: &[DeviceRow]) -> String {
    if rows.is_empty() {
        return "No devices known.\n".to_string();
    }

    let headers = ["ID", "NAME", "TYPE", "STATE", "PAIRED AT"];
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|r| {
            [
                short_id(&r.id),
                truncate(&r.name, MAX_NAME_WIDTH),
                r.device_type.clone(),
                r.state.clone(),
                r.paired_at
                    .as_deref()
                    .map(format_timestamp)
                    .unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();

    let mut widths = [0usize; 5];
    for (i, header) in headers.iter().enumerate() {
        widths[i] = header
            .len()
            .max(cells.iter().map(|c| c[i].len()).max().unwrap_or(0));
    }

    let mut out = String::new();
    render_row(&mut out, &headers.map(str::to_string), &widths);
    for row in &cells {
        render_row(&mut out, row, &widths);
    }
    out
}

fn render_row(out: &mut String, cells: &[String; 5], widths: &[usize; 5]) {
    out.push_str(&format!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}\n",
        cells[0],
        cells[1],
        cells[2],
        cells[3],
        cells[4],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
    ));
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut cut = max.saturating_sub(1);
    while cut > 0 && !value.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &value[..cut])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_short_id() {
        assert_eq!(short_id("0123456789abcdef0123456789abcdef"), "01234567");
        assert_eq!(short_id("short"), "short");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn test_format_timestamp() {
        assert_eq!(
            format_timestamp("2026-07-30T12:34:56.789Z"),
            "2026-07-30 12:34"
        );
        assert_eq!(
            format_timestamp("2026-07-30T12:34:56+00:00"),
            "2026-07-30 12:34"
        );
        assert_eq!(format_timestamp("not-a-date"), "not-a-date");
        assert_eq!(format_timestamp(""), "");
    }

    #[test]
    fn test_format_uptime() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(42), "42s");
        assert_eq!(format_uptime(90), "1m 30s");
        assert_eq!(format_uptime(3_900), "1h 5m");
        assert_eq!(format_uptime(200_000), "2d 7h");
    }

    #[test]
    fn test_render_devices_table_empty() {
        assert_eq!(render_devices_table(&[]), "No devices known.\n");
    }

    #[test]
    fn test_render_devices_table_alignment() {
        let rows = vec![
            DeviceRow {
                id: "0123456789abcdef0123456789abcdef".to_string(),
                name: "Pixel 8".to_string(),
                device_type: "phone".to_string(),
                state: "connected".to_string(),
                paired_at: Some("2026-07-30T12:34:56Z".to_string()),
            },
            DeviceRow {
                id: "ab12cd34ef56".to_string(),
                name: "Work Laptop With A Very Long Name Indeed".to_string(),
                device_type: "laptop".to_string(),
                state: "disconnected".to_string(),
                paired_at: None,
            },
        ];

        let table = render_devices_table(&rows);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "header + 2 rows:\n{table}");

        let header = lines[0];
        assert!(header.starts_with("ID"));
        assert!(header.contains("NAME"));
        assert!(header.contains("TYPE"));
        assert!(header.contains("STATE"));
        assert!(header.contains("PAIRED AT"));

        // Columns line up: both rows' NAME cells start at the header offset.
        let name_col = header.find("NAME").expect("header column");
        assert!(lines[1][name_col..].starts_with("Pixel 8"));
        assert!(lines[2][name_col..].starts_with("Work Laptop"));
        assert!(lines[1].contains("01234567"));
        assert!(lines[1].contains("2026-07-30 12:34"));
        assert_eq!(
            lines[2].len(),
            lines[2].trim_end().len(),
            "last column unpadded"
        );
        assert!(lines[2].contains("ab12cd34"));
        assert!(lines[2].ends_with('-'), "unpaired shows '-': {}", lines[2]);
        assert!(lines[2].contains('…'), "long name truncated: {}", lines[2]);
    }

    #[test]
    fn test_truncate_multibyte_safe() {
        let s = "éééééééééééééééé"; // 16 chars, 32 bytes
        let t = truncate(s, 10);
        assert!(t.ends_with('…'));
        assert!(t.chars().count() <= 10);
    }
}
