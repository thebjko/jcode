use qr2term::render::{QrDark, QrLight};

const QUIET_ZONE_WIDTH: usize = 2;

pub fn render_unicode_qr(data: &str) -> Result<String, qr2term::QrError> {
    let mut matrix = qr2term::qr::Qr::from(data)?.to_matrix();
    matrix.surround(QUIET_ZONE_WIDTH, QrLight);

    let size = matrix.size();
    let pixels = matrix.pixels();
    let mut out = String::new();

    for row in (0..size).step_by(2) {
        for col in 0..size {
            let top = pixels[row * size + col];
            let bottom = if row + 1 < size {
                pixels[(row + 1) * size + col]
            } else {
                QrLight
            };

            let ch = match (top, bottom) {
                (QrDark, QrDark) => '█',
                (QrDark, QrLight) => '▀',
                (QrLight, QrDark) => '▄',
                (QrLight, QrLight) => ' ',
            };
            out.push(ch);
        }
        if row + 2 < size {
            out.push('\n');
        }
    }

    Ok(out)
}

pub fn markdown_section(data: &str, heading: &str) -> Option<String> {
    let qr = render_unicode_qr(data).ok()?;
    Some(format!("{heading}\n\n```text\n{qr}\n```"))
}

pub fn indented_section(data: &str, heading: &str, indent: &str) -> Option<String> {
    let qr = render_unicode_qr(data).ok()?;
    let mut out = String::new();
    out.push_str(heading);
    out.push_str("\n\n");
    for line in qr.lines() {
        out.push_str(indent);
        out.push_str(line);
        out.push('\n');
    }
    Some(out.trim_end_matches('\n').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_unicode_qr_uses_block_glyphs_without_ansi() {
        let qr = render_unicode_qr("https://example.com/login").unwrap();
        assert!(qr.contains('█') || qr.contains('▀') || qr.contains('▄'));
        assert!(qr.contains('\n'));
        assert!(!qr.contains("\u{1b}["));
    }

    #[test]
    fn markdown_section_wraps_qr_in_code_block() {
        let section =
            markdown_section("https://example.com/login", "Scan this on another device:").unwrap();
        assert!(section.starts_with("Scan this on another device:\n\n```text\n"));
        assert!(section.ends_with("\n```"));
    }

    #[test]
    fn indented_section_prefixes_each_line() {
        let section = indented_section("https://example.com/login", "Scan:", "    ").unwrap();
        assert!(section.starts_with("Scan:\n\n    "));
        assert!(section
            .lines()
            .skip(2)
            .all(|line| line.is_empty() || line.starts_with("    ")));
    }
}
